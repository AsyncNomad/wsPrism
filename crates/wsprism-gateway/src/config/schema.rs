//! Config schema with strict parsing.
//!
//! `deny_unknown_fields` prevents silent misconfiguration.

use serde::Deserialize;

use wsprism_core::error::{Result, WsPrismError};

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GatewayConfig {
    pub version: u32,
    #[serde(default)]
    pub gateway: GatewaySection,
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
        Ok(())
    }
}

#[derive(Debug, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GatewaySection {
    #[serde(default = "default_listen")]
    pub listen: String,
}

fn default_listen() -> String {
    "0.0.0.0:8080".into()
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
