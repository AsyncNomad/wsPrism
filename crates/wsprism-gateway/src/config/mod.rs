//! Gateway config loader (strict parsing).
//!
//! This module exposes the typed configuration (parsed from `wsprism.yaml`)
//! and helpers to load/validate it before wiring the gateway runtime.

pub mod env;
pub mod schema;

use std::fs;

use wsprism_protocol::error::{Result, WsPrismError};

pub use schema::{GatewayConfig, TenantConfig, TenantLimits, TenantPolicy};

pub fn load_from_file(path: &str) -> Result<GatewayConfig> {
    let s = fs::read_to_string(path)
        .map_err(|e| WsPrismError::Internal(format!("read config failed: {e}")))?;
    load_from_str(&s)
}

/// Parse and validate. `${VAR}` placeholders in values come from the
/// process environment (see [`env`]).
pub fn load_from_str(s: &str) -> Result<GatewayConfig> {
    load_with_env(s, &|name| std::env::var(name).ok())
}

/// [`load_from_str`] with the variables from `lookup` (tests, embedders).
pub fn load_with_env(s: &str, lookup: &dyn Fn(&str) -> Option<String>) -> Result<GatewayConfig> {
    let cfg: GatewayConfig = if env::uses_placeholders(s) {
        // Rewritten after parsing; errors then name the setting
        // (`tenants[0].limits.max_rooms`) instead of a line.
        let mut value: serde_norway::Value = serde_norway::from_str(s)
            .map_err(|e| WsPrismError::BadRequest(format!("invalid yaml: {e}")))?;
        env::substitute(&mut value, lookup)?;
        serde_path_to_error::deserialize(value).map_err(|e| {
            WsPrismError::BadRequest(format!("invalid config at {}: {}", e.path(), e.inner()))
        })?
    } else {
        serde_norway::from_str(s)
            .map_err(|e| WsPrismError::BadRequest(format!("invalid yaml: {e}")))?
    };
    cfg.validate()?;
    Ok(cfg)
}
