//! Gateway config loader (strict parsing).

pub mod schema;

use std::fs;

use wsprism_core::error::{Result, WsPrismError};

pub use schema::{GatewayConfig, TenantConfig, TenantLimits, TenantPolicy};

pub fn load_from_file(path: &str) -> Result<GatewayConfig> {
    let s = fs::read_to_string(path)
        .map_err(|e| WsPrismError::Internal(format!("read config failed: {e}")))?;
    load_from_str(&s)
}

pub fn load_from_str(s: &str) -> Result<GatewayConfig> {
    let cfg: GatewayConfig = serde_yaml::from_str(s)
        .map_err(|e| WsPrismError::BadRequest(format!("invalid yaml: {e}")))?;
    cfg.validate()?;
    Ok(cfg)
}
