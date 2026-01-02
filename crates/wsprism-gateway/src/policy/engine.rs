use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use wsprism_core::error::ClientCode;

pub use crate::config::schema::{HotErrorMode, OnExceed, SessionMode};
use crate::config::schema::{RateLimitScope, SessionPolicy, TenantPolicy};

use super::allowlist::{
    compile_ext_rules, compile_hot_rules, is_ext_allowed, is_hot_allowed, ExtRule, HotRule,
};

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
            RateLimitScope::Tenant | RateLimitScope::Both => {
                Some(RateLimiter::new(policy.rate_limit_rps, policy.rate_limit_burst))
            }
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

        let add = (elapsed.as_millis() as u64 * self.rps as u64 / 1000) as u32;
        if add > 0 {
            self.tokens = (self.tokens + add).min(self.capacity);
            self.last = now;
        }
    }
}
