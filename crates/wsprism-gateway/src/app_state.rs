//! Shared application state for wsPrism Gateway.
//!
//! Sprint 1: keep this intentionally small.
//! - Config is loaded in main and stored here
//! - Ticket validation is a tiny stub (replace with infra store in Sprint 2/3)
//!
//! Everything is `Arc`-friendly and cloneable.

use std::sync::Arc;

use crate::config::GatewayConfig;

#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    pub cfg: GatewayConfig,
}

impl AppState {
    pub fn new(cfg: GatewayConfig) -> Self {
        Self {
            inner: Arc::new(AppStateInner { cfg }),
        }
    }

    pub fn cfg(&self) -> &GatewayConfig {
        &self.inner.cfg
    }

    /// Sprint 1 auth: a minimal, deterministic ticket resolver.
    ///
    /// - ticket=="dev" => user_id "user:dev"
    /// - otherwise => AuthFailed
    ///
    /// Replace with a real TicketStore (DashMap/Redis) later.
    pub fn resolve_ticket(&self, ticket: &str) -> wsprism_core::Result<String> {
        use wsprism_core::error::WsPrismError;

        match ticket {
            "dev" => Ok("user:dev".to_string()),
            _ => Err(WsPrismError::AuthFailed),
        }
    }
}
