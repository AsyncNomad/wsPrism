//! wsPrism gateway binary (Sprint 0).
//!
//! Sprint 0 focuses on: strict config parsing, tracing bootstrap,
//! and shared error plumbing.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

use tracing_subscriber::{fmt, EnvFilter};

fn main() -> Result<(), wsprism_core::WsPrismError> {
    fmt().with_env_filter(EnvFilter::from_default_env()).init();

    let cfg = wsprism_gateway::config::load_from_file("wsprism.yaml")?;
    tracing::info!(version = cfg.version, "config loaded (strict parsing ok)");
    Ok(())
}
