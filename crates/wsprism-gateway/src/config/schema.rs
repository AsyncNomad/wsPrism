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

        self.gateway.validate()?;   // Verify the scope of value

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TenantConfig {
    pub id: String,
    #[serde(default)]
    pub limits: TenantLimits,
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct TenantLimits {
    #[serde(default = "default_max_frame_bytes")]
    pub max_frame_bytes: usize,
}

fn default_max_frame_bytes() -> usize {
    4096
}
