//! Handshake Defender (pre-upgrade DoS guard).
//!
//! Purpose:
//! - Stop abuse *before* WebSocket upgrade.
//! - Per-IP + global token buckets; rejected callers get HTTP 429 with a
//!   `Retry-After` hint.
//!
//! Design notes (see docs/HARDENING.md, S6):
//! - `check` is synchronous and never holds a DashMap guard beyond a single
//!   expression, so it cannot deadlock against other map operations.
//! - Per-IP is checked *before* global, so one flooding IP is stopped by its
//!   own bucket and cannot drain the shared global budget.
//! - The per-IP map is hard-capped at `max_ip_entries`. At the cap, a lossless
//!   sweep evicts entries whose bucket has fully refilled (indistinguishable
//!   from fresh ones). IPs that still don't fit are not tracked and are
//!   limited by the global bucket only.

use std::net::IpAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use crate::config::schema::HandshakeConfig;
use dashmap::DashMap;

/// Minimum interval between two sweeps of the per-IP map.
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// Token bucket (capacity/refill, fractional tokens).
#[derive(Debug)]
pub struct LeakyBucket {
    capacity: u32,
    tokens: f64,
    refill_per_sec: f64,
    last: Instant,
}

impl LeakyBucket {
    pub fn new(capacity: u32, refill_per_sec: u32) -> Self {
        let cap = capacity.max(1);
        Self {
            capacity: cap,
            tokens: cap as f64,
            refill_per_sec: refill_per_sec.max(1) as f64,
            last: Instant::now(),
        }
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.last = now;
        self.tokens = (self.tokens + elapsed * self.refill_per_sec).min(self.capacity as f64);
    }

    /// Consume `cost` tokens. Returns Ok if allowed, Err with retry_after seconds (ceil).
    pub fn try_take(&mut self, cost: u32) -> Result<(), u64> {
        self.refill();
        let c = cost.max(1) as f64;
        if self.tokens >= c {
            self.tokens -= c;
            Ok(())
        } else {
            let missing = c - self.tokens;
            let wait = (missing / self.refill_per_sec).ceil();
            Err(wait.max(1.0) as u64) // Retry-After min 1
        }
    }

    /// True if, at `now`, this bucket would be full again. Such a bucket
    /// behaves exactly like a freshly created one, so dropping it is lossless.
    fn is_refilled(&self, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.last).as_secs_f64();
        self.tokens + elapsed * self.refill_per_sec >= self.capacity as f64
    }
}

/// A lightweight in-memory handshake rate limiter.
#[derive(Debug)]
pub struct HandshakeDefender {
    cfg: HandshakeConfig,
    global: Mutex<LeakyBucket>,
    per_ip: DashMap<IpAddr, LeakyBucket>,
    created: Instant,
    last_sweep_ms: AtomicU64,
}

impl HandshakeDefender {
    pub fn new(cfg: HandshakeConfig) -> Self {
        Self {
            global: Mutex::new(LeakyBucket::new(cfg.global_burst, cfg.global_rps)),
            per_ip: DashMap::new(),
            created: Instant::now(),
            last_sweep_ms: AtomicU64::new(0),
            cfg,
        }
    }

    /// Check handshake allowance. Returns Ok if allowed.
    /// On reject, returns retry-after seconds (min 1).
    pub fn check(&self, ip: IpAddr) -> Result<(), u64> {
        if !self.cfg.enabled {
            return Ok(());
        }

        // 1) Per-IP. DashMap guards are confined to a single expression so no
        //    shard lock is ever held while touching the map again. (Do not move
        //    them into an `if let` scrutinee: in edition 2021 its temporaries
        //    live through the `else` branch.)
        let ip = self.bucket_key(ip);
        let tracked = self.per_ip.get_mut(&ip).map(|mut b| b.try_take(1));
        let per_ip = match tracked {
            Some(r) => r,
            None if self.per_ip.len() < self.cfg.max_ip_entries => self
                .per_ip
                .entry(ip)
                .or_insert_with(|| LeakyBucket::new(self.cfg.per_ip_burst, self.cfg.per_ip_rps))
                .try_take(1),
            None => {
                // At capacity: make room for later arrivals, but don't track
                // this IP now. It is still subject to the global bucket.
                self.maybe_sweep();
                Ok(())
            }
        };
        per_ip?;

        // 2) Global. A poisoned lock only means a panic elsewhere mid-update;
        //    the bucket is plain data, so keep using it rather than failing.
        self.global
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .try_take(1)?;

        Ok(())
    }

    /// The per-IP bucket an address counts against: IPv4 as is, IPv6 by
    /// its `ipv6_prefix_len` prefix.
    fn bucket_key(&self, ip: IpAddr) -> IpAddr {
        match ip {
            IpAddr::V4(_) => ip,
            IpAddr::V6(v6) => {
                let bits = u32::from(self.cfg.ipv6_prefix_len.clamp(32, 128));
                let mask = u128::MAX.checked_shl(128 - bits).unwrap_or(0);
                IpAddr::V6((u128::from(v6) & mask).into())
            }
        }
    }

    /// Sweep at most once per `SWEEP_INTERVAL` across all callers.
    fn maybe_sweep(&self) {
        let now_ms = self.created.elapsed().as_millis() as u64;
        let last = self.last_sweep_ms.load(Ordering::Relaxed);
        if now_ms.saturating_sub(last) < SWEEP_INTERVAL.as_millis() as u64 {
            return;
        }
        if self
            .last_sweep_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return; // another caller is sweeping
        }
        let before = self.per_ip.len();
        self.sweep_refilled(Instant::now());
        tracing::warn!(
            before,
            after = self.per_ip.len(),
            "handshake defender swept refilled ip entries"
        );
    }

    /// Evict every per-IP entry that is fully refilled at `now` (an entry
    /// becomes evictable `per_ip_burst / per_ip_rps` seconds after last use).
    fn sweep_refilled(&self, now: Instant) {
        self.per_ip.retain(|_, b| !b.is_refilled(now));
    }
}

/// Helper: build a `Retry-After` header value (seconds, min 1).
pub fn retry_after_header(secs: u64) -> axum::http::HeaderValue {
    axum::http::HeaderValue::from(secs.max(1))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn cfg(global_burst: u32, per_ip_burst: u32, max_ip_entries: usize) -> HandshakeConfig {
        HandshakeConfig {
            enabled: true,
            global_burst,
            global_rps: 1,
            per_ip_burst,
            per_ip_rps: 1,
            max_ip_entries,
            ipv6_prefix_len: 64,
        }
    }

    fn ip(n: u8) -> IpAddr {
        IpAddr::V4(Ipv4Addr::new(10, 0, 0, n))
    }

    #[test]
    fn single_ip_flood_does_not_drain_global_budget() {
        let d = HandshakeDefender::new(cfg(10, 2, 1000));

        let admitted_a = (0..100).filter(|_| d.check(ip(1)).is_ok()).count();
        assert_eq!(admitted_a, 2, "per-IP burst caps the flooding IP");

        // The other 8 global tokens are still available to other clients.
        let admitted_others = (2..12).filter(|&n| d.check(ip(n)).is_ok()).count();
        assert_eq!(admitted_others, 8);
    }

    #[test]
    fn sweep_evicts_only_refilled_entries() {
        let d = HandshakeDefender::new(cfg(100, 3, 1000));
        d.check(ip(1)).unwrap(); // partially used
        for _ in 0..3 {
            let _ = d.check(ip(2)); // exhausted
        }

        // Right now neither bucket is full again: nothing is evicted.
        d.sweep_refilled(Instant::now());
        assert_eq!(d.per_ip.len(), 2);

        // Far enough in the future both have refilled: both are evicted.
        d.sweep_refilled(Instant::now() + Duration::from_secs(60));
        assert_eq!(d.per_ip.len(), 0);
    }

    #[test]
    fn map_never_exceeds_cap_and_overflow_ips_fall_back_to_global() {
        let d = HandshakeDefender::new(cfg(5, 1, 2));
        assert!(d.check(ip(1)).is_ok());
        assert!(d.check(ip(2)).is_ok());

        // ip(3) doesn't fit: untracked, so its per-IP burst of 1 isn't applied,
        // but the global bucket (5 total, 2 used) still is.
        let admitted = (0..10).filter(|_| d.check(ip(3)).is_ok()).count();
        assert_eq!(admitted, 3);
        assert_eq!(d.per_ip.len(), 2);
    }

    #[test]
    fn ipv6_addresses_share_their_64_prefix_bucket() {
        let d = HandshakeDefender::new(cfg(100, 2, 1000));
        let v6 = |s: &str| s.parse::<IpAddr>().unwrap();
        assert!(d.check(v6("2001:db8:1:2::1")).is_ok());
        assert!(d.check(v6("2001:db8:1:2::ffff")).is_ok());
        assert!(
            d.check(v6("2001:db8:1:2:aaaa::9")).is_err(),
            "same /64: bucket exhausted"
        );
        assert!(d.check(v6("2001:db8:1:3::1")).is_ok(), "another /64");
    }

    #[test]
    fn disabled_defender_admits_everything() {
        let mut c = cfg(1, 1, 1);
        c.enabled = false;
        let d = HandshakeDefender::new(c);
        assert!((0..100).all(|_| d.check(ip(1)).is_ok()));
    }
}
