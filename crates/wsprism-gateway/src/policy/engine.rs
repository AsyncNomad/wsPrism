use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wsprism_core::error::ClientCode;

use crate::config::schema::TenantPolicy;

use super::allowlist::{compile_ext_rules, compile_hot_rules, is_ext_allowed, is_hot_allowed, ExtRule, HotRule};

/// Decision from policy evaluation.
///
/// - Pass: continue to next stage
/// - Drop: silently ignore (recommended for Hot Lane abuse)
/// - Reject: send an error to client but keep the connection
/// - Close: send an error (best-effort) then close the connection
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

    // Simple token bucket rate limiter (tenant-level).
    limiter: RateLimiter,
}

impl TenantPolicyRuntime {
    pub fn new(tenant_id: String, max_frame_bytes: usize, policy: &TenantPolicy) -> wsprism_core::Result<Self> {
        let ext_rules = compile_ext_rules(&policy.ext_allowlist)?;
        let hot_rules = compile_hot_rules(&policy.hot_allowlist)?;

        Ok(Self {
            tenant_id,
            max_frame_bytes,
            ext_rules,
            hot_rules,
            limiter: RateLimiter::new(policy.rate_limit_rps, policy.rate_limit_burst),
        })
    }

    /// Cheap global checks for any inbound payload.
    pub fn check_len(&self, bytes_len: usize) -> PolicyDecision {
        if bytes_len > self.max_frame_bytes {
            // For extreme abuse you may prefer Close, but Reject is friendlier for Ext.
            return PolicyDecision::Close {
                code: ClientCode::BadRequest,
                msg: "frame too large",
            };
        }
        PolicyDecision::Pass
    }

    /// Ext Lane policy: svc/type allowlist + rate limit.
    pub fn check_text(&self, bytes_len: usize, svc: &str, msg_type: &str) -> PolicyDecision {
        match self.check_len(bytes_len) {
            PolicyDecision::Pass => {}
            other => return other,
        }

        if !self.limiter.allow() {
            return PolicyDecision::Drop;
        }

        // If allowlist is empty, we default-deny (strict).
        if self.ext_rules.is_empty() {
            return PolicyDecision::Reject {
                code: ClientCode::BadRequest,
                msg: "ext_allowlist empty (strict deny)",
            };
        }

        if !is_ext_allowed(&self.ext_rules, svc, msg_type) {
            return PolicyDecision::Reject {
                code: ClientCode::BadRequest,
                msg: "svc/type not allowed",
            };
        }

        PolicyDecision::Pass
    }

    /// Hot Lane policy: svc_id/opcode allowlist + rate limit.
    pub fn check_hot(&self, bytes_len: usize, svc_id: u8, opcode: u8) -> PolicyDecision {
        match self.check_len(bytes_len) {
            PolicyDecision::Pass => {}
            other => return other,
        }

        // Hot Lane: rate limit violations should be silent drops.
        if !self.limiter.allow() {
            return PolicyDecision::Drop;
        }

        if self.hot_rules.is_empty() {
            return PolicyDecision::Drop; // strict deny for hot lane
        }

        if !is_hot_allowed(&self.hot_rules, svc_id, opcode) {
            return PolicyDecision::Drop;
        }

        PolicyDecision::Pass
    }
}

/// Minimal token-bucket limiter.
///
/// NOTE: This is intentionally small and dependency-free for Sprint 2.
/// In production, you may wrap a battle-tested crate (e.g., governor).
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
        let mut g = self.inner.lock().expect("rate limiter poisoned");
        g.allow()
    }
}

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
        self.refill();

        if self.tokens == 0 {
            return false;
        }
        self.tokens -= 1;
        true
    }

    fn refill(&mut self) {
        let now = Instant::now();
        let elapsed = now.duration_since(self.last);
        if elapsed < Duration::from_millis(50) {
            return;
        }

        // Refill in ms granularity (cheap, deterministic).
        let add = (elapsed.as_millis() as u64 * self.rps as u64 / 1000) as u32;
        if add > 0 {
            self.tokens = (self.tokens + add).min(self.capacity);
            self.last = now;
        }
    }
}
