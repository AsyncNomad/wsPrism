//! Compiled policy runtime for a tenant.
//!
//! Parses allowlists, enforces size/rate limits, and exposes connection-level
//! limiters as needed by the transport layer.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wsprism_core::error::ClientCode;

pub use crate::config::schema::{HotErrorMode, OnExceed, SessionMode};
use crate::config::schema::{RateLimitScope, SessionPolicy, TenantPolicy, TicketSource};

use super::allowlist::{
    compile_ext_rules, compile_hot_rules, is_ext_allowed, is_hot_allowed, ExtRule, HotRule,
};
use super::origin::OriginPolicy;

/// Decision from policy evaluation.
#[derive(Debug, Clone)]
pub enum PolicyDecision {
    Pass,
    Drop,
    Reject { code: ClientCode, msg: &'static str },
    Close { code: ClientCode, msg: &'static str },
}

/// Tenant-scoped policy runtime.
/// Construct once at startup, then share via Arc.
pub struct TenantPolicyRuntime {
    pub tenant_id: String,

    max_frame_bytes: usize,
    ext_rules: Vec<ExtRule>,
    hot_rules: Vec<HotRule>,

    // Rate limit configuration
    rate_limit_scope: RateLimitScope,
    conn_rps: u32,
    conn_burst: u32,
    tenant_limiter: Option<RateLimiter>,

    // Session policy
    sessions: SessionPolicy,

    // Hot lane behavior
    hot_error_mode: HotErrorMode,
    hot_requires_active_room: bool,
    hot_tick: Option<std::time::Duration>,

    origins: OriginPolicy,
    ticket_sources: Vec<TicketSource>,
}

impl TenantPolicyRuntime {
    pub fn new(
        tenant_id: String,
        max_frame_bytes: usize,
        policy: &TenantPolicy,
    ) -> wsprism_core::Result<Self> {
        let ext_rules = compile_ext_rules(&policy.ext_allowlist)?;
        let hot_rules = compile_hot_rules(&policy.hot_allowlist)?;

        let tenant_limiter = match policy.rate_limit_scope {
            RateLimitScope::Tenant | RateLimitScope::Both => Some(RateLimiter::new(
                policy.rate_limit_rps,
                policy.rate_limit_burst,
            )),
            RateLimitScope::Connection => None,
        };

        Ok(Self {
            tenant_id,
            max_frame_bytes,
            ext_rules,
            hot_rules,
            rate_limit_scope: policy.rate_limit_scope,
            conn_rps: policy.rate_limit_rps,
            conn_burst: policy.rate_limit_burst,
            tenant_limiter,
            sessions: policy.sessions.clone(),
            hot_error_mode: policy.hot_error_mode,
            hot_requires_active_room: policy.hot_requires_active_room,
            hot_tick: (policy.hot_tick_hz > 0)
                .then(|| std::time::Duration::from_secs_f64(1.0 / f64::from(policy.hot_tick_hz))),
            origins: OriginPolicy::compile(&policy.allowed_origins)?,
            ticket_sources: policy.ticket_sources.clone(),
        })
    }

    pub fn session_policy(&self) -> &SessionPolicy {
        &self.sessions
    }
    pub fn hot_error_mode(&self) -> HotErrorMode {
        self.hot_error_mode
    }
    pub fn hot_requires_active_room(&self) -> bool {
        self.hot_requires_active_room
    }
    /// Pacing period for sending a session's Hot slot (`None` = immediate).
    pub fn hot_tick(&self) -> Option<std::time::Duration> {
        self.hot_tick
    }

    /// Browser origin policy for upgrades (CSWSH protection).
    pub fn origins(&self) -> &OriginPolicy {
        &self.origins
    }

    /// Where clients may present their ticket, in priority order.
    pub fn ticket_sources(&self) -> &[TicketSource] {
        &self.ticket_sources
    }

    /// Create per-connection limiter if enabled (Connection/Both).
    pub fn new_connection_limiter(&self) -> Option<ConnRateLimiter> {
        match self.rate_limit_scope {
            RateLimitScope::Connection | RateLimitScope::Both => {
                Some(ConnRateLimiter::new(self.conn_rps, self.conn_burst))
            }
            RateLimitScope::Tenant => None,
        }
    }

    /// Cheap global checks for any inbound payload.
    pub fn check_len(&self, bytes_len: usize) -> PolicyDecision {
        if bytes_len > self.max_frame_bytes {
            return PolicyDecision::Close {
                code: ClientCode::BadRequest,
                msg: "frame too large",
            };
        }
        PolicyDecision::Pass
    }

    /// Ext Lane policy: svc/type allowlist + (optional) tenant-level rate limit.
    pub fn check_text(&self, bytes_len: usize, svc: &str, msg_type: &str) -> PolicyDecision {
        match self.check_len(bytes_len) {
            PolicyDecision::Pass => {}
            other => return other,
        }

        if let Some(lim) = &self.tenant_limiter {
            if !lim.allow() {
                return PolicyDecision::Drop;
            }
        }

        if self.ext_rules.is_empty() {
            return PolicyDecision::Reject {
                code: ClientCode::NotAllowed,
                msg: "ext_allowlist empty (strict deny)",
            };
        }

        if !is_ext_allowed(&self.ext_rules, svc, msg_type) {
            return PolicyDecision::Reject {
                code: ClientCode::NotAllowed,
                msg: "svc/type not allowed",
            };
        }

        PolicyDecision::Pass
    }

    /// Hot Lane policy: svc_id/opcode allowlist + (optional) tenant-level rate limit.
    pub fn check_hot(&self, bytes_len: usize, svc_id: u8, opcode: u8) -> PolicyDecision {
        match self.check_len(bytes_len) {
            PolicyDecision::Pass => {}
            other => return other,
        }

        if let Some(lim) = &self.tenant_limiter {
            if !lim.allow() {
                return PolicyDecision::Drop;
            }
        }

        if self.hot_rules.is_empty() {
            return PolicyDecision::Drop; // strict deny
        }

        if !is_hot_allowed(&self.hot_rules, svc_id, opcode) {
            return PolicyDecision::Drop;
        }

        PolicyDecision::Pass
    }
}

/// Per-connection token bucket (no mutex).
#[derive(Debug)]
pub struct ConnRateLimiter {
    bucket: TokenBucket,
}

impl ConnRateLimiter {
    pub fn new(rps: u32, burst: u32) -> Self {
        Self {
            bucket: TokenBucket::new(rps, burst),
        }
    }

    pub fn allow(&mut self) -> bool {
        self.bucket.allow()
    }
}

/// Minimal token-bucket limiter (tenant-level, shared).
struct RateLimiter {
    inner: Arc<Mutex<TokenBucket>>,
}

impl RateLimiter {
    fn new(rps: u32, burst: u32) -> Self {
        Self {
            inner: Arc::new(Mutex::new(TokenBucket::new(rps, burst))),
        }
    }

    fn allow(&self) -> bool {
        // Poisoned mutex means logic bug; treat as "deny" instead of panic.
        // (enterprise: never bring down gateway)
        if let Ok(mut g) = self.inner.lock() {
            g.allow()
        } else {
            false
        }
    }
}

#[derive(Debug)]
struct TokenBucket {
    rps: u32,
    capacity: u32,
    tokens: u32,
    last: Instant,
}

impl TokenBucket {
    fn new(rps: u32, burst: u32) -> Self {
        let rps = rps.max(1);
        let capacity = burst.max(1);
        Self {
            rps,
            capacity,
            tokens: capacity,
            last: Instant::now(),
        }
    }

    fn allow(&mut self) -> bool {
        self.allow_at(Instant::now())
    }

    fn allow_at(&mut self, now: Instant) -> bool {
        self.refill_at(now);
        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }

    /// Add the tokens earned since `last`. Only the time actually converted
    /// into tokens is consumed, so fractional progress carries over (C6: the
    /// old version reset `last = now` and lost up to one token interval per
    /// refill). A full bucket banks no extra time.
    fn refill_at(&mut self, now: Instant) {
        const NANOS_PER_SEC: u128 = 1_000_000_000;
        let rps = u128::from(self.rps);
        let elapsed = now.saturating_duration_since(self.last).as_nanos();
        let add = elapsed * rps / NANOS_PER_SEC;
        if add == 0 {
            return;
        }
        let total = u128::from(self.tokens) + add;
        if total >= u128::from(self.capacity) {
            self.tokens = self.capacity;
            self.last = now;
        } else {
            self.tokens = total as u32;
            let used = add * NANOS_PER_SEC / rps;
            self.last += Duration::from_nanos(used as u64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drained(rps: u32, burst: u32) -> (TokenBucket, Instant) {
        let mut b = TokenBucket::new(rps, burst);
        let t0 = b.last;
        while b.allow_at(t0) {}
        (b, t0)
    }

    #[test]
    fn fractional_refill_time_carries_over() {
        let (mut b, t0) = drained(10, 5); // one token per 100ms
        assert!(b.allow_at(t0 + Duration::from_millis(199))); // 1 token, 99ms credit
        assert!(b.allow_at(t0 + Duration::from_millis(200))); // credit completes token 2
        assert!(!b.allow_at(t0 + Duration::from_millis(200)));
    }

    #[test]
    fn full_bucket_does_not_bank_extra_time() {
        let (mut b, t0) = drained(10, 2);
        let later = t0 + Duration::from_secs(10); // would be 100 tokens uncapped
        assert!(b.allow_at(later));
        assert!(b.allow_at(later));
        assert!(!b.allow_at(later));
        assert!(!b.allow_at(later + Duration::from_millis(99)));
        assert!(b.allow_at(later + Duration::from_millis(100)));
    }

    #[test]
    fn sustained_rate_matches_rps() {
        // burst 2 so the bucket never hits its cap between polls (with burst
        // 1, time spent full is correctly not banked, giving ~95).
        let (mut b, t0) = drained(10, 2);
        // Poll every 15ms for 10s: should admit 100 (10/s), not fewer.
        let admitted = (1..=667)
            .filter(|i| b.allow_at(t0 + Duration::from_millis(15 * i)))
            .count();
        assert_eq!(admitted, 100);
    }
}
