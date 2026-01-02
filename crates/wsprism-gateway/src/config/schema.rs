use serde::Deserialize;
use wsprism_core::error::{Result, WsPrismError};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub version: u32,

    #[serde(default)]
    pub gateway: GatewaySection,

    #[serde(default)]
    pub tenants: Vec<TenantConfig>,
}

impl GatewayConfig {
    pub fn validate(&self) -> Result<()> {
        if self.version != 1 {
            return Err(WsPrismError::UnsupportedVersion);
        }
        if self.tenants.is_empty() {
            return Err(WsPrismError::BadRequest("tenants must not be empty".into()));
        }

        // Unique tenant ids
        {
            use std::collections::HashSet;
            let mut seen = HashSet::new();
            for t in &self.tenants {
                if t.id.trim().is_empty() {
                    return Err(WsPrismError::BadRequest("tenant.id must not be empty".into()));
                }
                if !seen.insert(t.id.clone()) {
                    return Err(WsPrismError::BadRequest(format!("duplicate tenant id: {}", t.id)));
                }
                t.validate()?;
            }
        }

        self.gateway.validate()?;
        Ok(())
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewaySection {
    #[serde(default = "default_listen")]
    pub listen: String,

    #[serde(default = "default_ping_interval_ms")]
    pub ping_interval_ms: u64,

    #[serde(default = "default_idle_timeout_ms")]
    pub idle_timeout_ms: u64,
}

impl Default for GatewaySection {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            ping_interval_ms: default_ping_interval_ms(),
            idle_timeout_ms: default_idle_timeout_ms(),
        }
    }
}

impl GatewaySection {
    pub fn validate(&self) -> Result<()> {
        if !(5000..=120000).contains(&self.ping_interval_ms) {
            return Err(WsPrismError::BadRequest(
                "gateway.ping_interval_ms must be between 5000 and 120000".into(),
            ));
        }
        if !(10000..=600000).contains(&self.idle_timeout_ms) {
            return Err(WsPrismError::BadRequest(
                "gateway.idle_timeout_ms must be between 10000 and 600000".into(),
            ));
        }
        if self.idle_timeout_ms <= self.ping_interval_ms {
            return Err(WsPrismError::BadRequest(
                "gateway.idle_timeout_ms must be greater than ping_interval_ms".into(),
            ));
        }
        Ok(())
    }
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
}
fn default_ping_interval_ms() -> u64 {
    20000
}
fn default_idle_timeout_ms() -> u64 {
    60000
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    pub id: String,

    #[serde(default)]
    pub limits: TenantLimits,

    /// Sprint 2+: policy controls (strict by default).
    #[serde(default)]
    pub policy: TenantPolicy,
}

impl TenantConfig {
    pub fn validate(&self) -> Result<()> {
        if self.limits.max_frame_bytes == 0 {
            return Err(WsPrismError::BadRequest("limits.max_frame_bytes must be > 0".into()));
        }
        self.policy.validate()?;
        Ok(())
    }
}

#[derive(Debug, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct TenantLimits {
    #[serde(default = "default_max_frame_bytes")]
    pub max_frame_bytes: usize,
}

fn default_max_frame_bytes() -> usize {
    4096
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum RateLimitScope {
    Tenant,
    Connection,
    Both,
}

fn default_rate_limit_scope() -> RateLimitScope {
    // YAML 코멘트(연결당)에 맞춰 기본은 connection으로.
    RateLimitScope::Connection
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum HotErrorMode {
    SysError,
    Silent,
}

fn default_hot_error_mode() -> HotErrorMode {
    HotErrorMode::SysError
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    Single,
    Multi,
}

#[derive(Debug, Deserialize, Clone, Copy)]
#[serde(rename_all = "snake_case")]
pub enum OnExceed {
    Deny,
    KickOldest,
}

#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct SessionPolicy {
    #[serde(default = "default_session_mode")]
    pub mode: SessionMode,

    #[serde(default = "default_max_sessions_per_user")]
    pub max_sessions_per_user: u32,

    #[serde(default = "default_on_exceed")]
    pub on_exceed: OnExceed,
}

fn default_session_mode() -> SessionMode {
    SessionMode::Multi
}
fn default_max_sessions_per_user() -> u32 {
    4
}
fn default_on_exceed() -> OnExceed {
    OnExceed::Deny
}

impl Default for SessionPolicy {
    fn default() -> Self {
        Self {
            mode: default_session_mode(),
            max_sessions_per_user: default_max_sessions_per_user(),
            on_exceed: default_on_exceed(),
        }
    }
}

/// Tenant policy knobs.
/// Defaults are STRICT (deny-by-default) but include minimal room join/leave for Sprint 2.
#[derive(Debug, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TenantPolicy {
    /// Tenant-level or connection-level inbound rate limit in requests per second.
    #[serde(default = "default_rate_limit_rps")]
    pub rate_limit_rps: u32,

    /// Burst capacity for token bucket.
    #[serde(default = "default_rate_limit_burst")]
    pub rate_limit_burst: u32,

    /// Where to apply rate limiting.
    #[serde(default = "default_rate_limit_scope")]
    pub rate_limit_scope: RateLimitScope,

    /// Ext lane allowlist entries, like:
    /// - "room:join"
    /// - "room:leave"
    /// - "chat:*" (wildcard type)
    #[serde(default = "default_ext_allowlist")]
    pub ext_allowlist: Vec<String>,

    /// Hot lane allowlist entries, like:
    /// - "1:*" (svc_id=1, any opcode)
    /// - "1:1" (svc_id=1, opcode=1)
    #[serde(default)]
    pub hot_allowlist: Vec<String>,

    /// Session policy (1:1 / 1:N)
    #[serde(default)]
    pub sessions: SessionPolicy,

    /// Hot lane error surface (sys.error vs silent)
    #[serde(default = "default_hot_error_mode")]
    pub hot_error_mode: HotErrorMode,

    /// If true, hot lane requires active_room; if false, roomless hot services are allowed.
    #[serde(default = "default_hot_requires_active_room")]
    pub hot_requires_active_room: bool,
}

fn default_hot_requires_active_room() -> bool {
    true
}

impl Default for TenantPolicy {
    fn default() -> Self {
        Self {
            rate_limit_rps: default_rate_limit_rps(),
            rate_limit_burst: default_rate_limit_burst(),
            rate_limit_scope: default_rate_limit_scope(),
            ext_allowlist: default_ext_allowlist(),
            hot_allowlist: Vec::new(),
            sessions: SessionPolicy::default(),
            hot_error_mode: default_hot_error_mode(),
            hot_requires_active_room: default_hot_requires_active_room(),
        }
    }
}

impl TenantPolicy {
    pub fn validate(&self) -> Result<()> {
        if self.rate_limit_rps == 0 || self.rate_limit_burst == 0 {
            return Err(WsPrismError::BadRequest(
                "policy.rate_limit_rps and rate_limit_burst must be > 0".into(),
            ));
        }

        // sessions policy sanity
        match self.sessions.mode {
            SessionMode::Single => {
                if self.sessions.max_sessions_per_user != 1 {
                    return Err(WsPrismError::BadRequest(
                        "policy.sessions.mode=single requires max_sessions_per_user=1".into(),
                    ));
                }
            }
            SessionMode::Multi => {
                if self.sessions.max_sessions_per_user == 0 {
                    return Err(WsPrismError::BadRequest(
                        "policy.sessions.max_sessions_per_user must be > 0".into(),
                    ));
                }
            }
        }

        Ok(())
    }
}

fn default_rate_limit_rps() -> u32 {
    200
}
fn default_rate_limit_burst() -> u32 {
    400
}
fn default_ext_allowlist() -> Vec<String> {
    vec!["room:join".into(), "room:leave".into()]
}
