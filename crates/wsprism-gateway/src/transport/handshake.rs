//! Handshake Defender (pre-upgrade DoS guard).
//!
//! Purpose:
//! - Stop abuse *before* WebSocket upgrade.
//! - Per-IP + global leaky-bucket limiter.
//! - Returns HTTP 429 with Retry-After header hint.
//! - Note: cleanup is probabilistic and inline; under extreme IP churn it can
//!   briefly block the caller. A background cleaner is preferable for very high churn.

use std::net::IpAddr;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use dashmap::DashMap;
use tokio::sync::Mutex;
use crate::config::schema::HandshakeConfig;

/// Simple leaky bucket (capacity/refill, best-effort).
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
}

/// A lightweight in-memory handshake rate limiter.
///
/// Concurrency note: `check` may invoke a probabilistic cleanup via `retain`
/// when `per_ip` grows large. That cleanup can briefly lock shards. For strict
/// latency guarantees, move cleanup to a background task instead of running
/// inline with request handling.
#[derive(Debug)]
pub struct HandshakeDefender {
    cfg: HandshakeConfig,
    global: Mutex<LeakyBucket>,
    per_ip: DashMap<IpAddr, Mutex<LeakyBucket>>,
}

impl HandshakeDefender {
    pub fn new(cfg: HandshakeConfig) -> Self {
        Self {
            global: Mutex::new(LeakyBucket::new(cfg.global_burst, cfg.global_rps)),
            per_ip: DashMap::new(),
            cfg,
        }
    }

    pub fn enabled(&self) -> bool {
        self.cfg.enabled
    }

    /// Check handshake allowance. Returns Ok if allowed.
    /// On reject, returns retry-after seconds (min 1).
    pub async fn check(&self, ip: IpAddr) -> Result<(), u64> {
        if !self.cfg.enabled {
            return Ok(());
        }

        // 1) Global
        {
            let mut g = self.global.lock().await;
            if let Err(ra) = g.try_take(1) {
                return Err(ra);
            }
        }

        // 2) Per-IP
        let entry = self.per_ip.entry(ip).or_insert_with(|| {
            Mutex::new(LeakyBucket::new(self.cfg.per_ip_burst, self.cfg.per_ip_rps))
        });
        {
            let mut b = entry.value().lock().await;
            if let Err(ra) = b.try_take(1) {
                return Err(ra);
            }
        }

        // Best-effort size control (Lazy Cleanup)
        if self.per_ip.len() > self.cfg.max_ip_entries {
            // "Pseudo-random" eviction without external crate dependency.
            // Use nanoseconds from system time as a seed.
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .subsec_nanos();
            
            // ~10% chance to run cleanup when over limit
            if nanos % 100 < 10 {
                // Clear roughly 10% of entries (arbitrary batch)
                // DashMap doesn't support safe iteration during retain easily without locking shards,
                // so we just remove some keys if we can find them, or clear all if desperate.
                // For simplicity/safety here: retain only recently accessed? No timestamp stored.
                // Fallback: Remove every 10th item (conceptually).
                // Or just:
                self.per_ip.retain(|_, _| {
                    let n = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().subsec_nanos();
                    n % 10 != 0 // Drop ~10%
                });
                tracing::warn!(len = self.per_ip.len(), "handshake defender ip map trimmed");
            }
        }

        Ok(())
    }
}

/// Helper: format Retry-After duration.
pub fn retry_after_header_secs(secs: u64) -> (String, u64) {
    let s = secs.max(1);
    (s.to_string(), s)
}
