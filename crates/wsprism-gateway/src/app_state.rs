//! Shared application state for wsPrism Gateway.
//!
//! Sprint 2:
//! - Build per-tenant compiled policy runtimes at startup for fast lookup.
//! - Keep auth stub (ticket -> user_id).

use std::collections::HashMap;
use std::sync::Arc;

use wsprism_core::error::{Result, WsPrismError};

use crate::{config::GatewayConfig, policy};

#[derive(Clone)]
pub struct AppState {
    inner: Arc<AppStateInner>,
}

struct AppStateInner {
    cfg: GatewayConfig,
    tenant_policy: HashMap<String, Arc<policy::TenantPolicyRuntime>>,
}

impl AppState {
    pub fn new(cfg: GatewayConfig) -> Self {
        let mut tenant_policy = HashMap::new();
        for t in &cfg.tenants {
            // Policy defaults are strict; config must provide allowlists.
            let runtime = policy::TenantPolicyRuntime::new(
                t.id.clone(),
                t.limits.max_frame_bytes,
                &t.policy,
            )
            .expect("tenant policy compile failed");

            tenant_policy.insert(t.id.clone(), Arc::new(runtime));
        }

        Self {
            inner: Arc::new(AppStateInner { cfg, tenant_policy }),
        }
    }

    pub fn cfg(&self) -> &GatewayConfig {
        &self.inner.cfg
    }

    pub fn tenant_policy(&self, tenant_id: &str) -> Option<Arc<policy::TenantPolicyRuntime>> {
        self.inner.tenant_policy.get(tenant_id).cloned()
    }

    /// Sprint 1/2 auth: a minimal deterministic ticket resolver.
    pub fn resolve_ticket(&self, ticket: &str) -> Result<String> {
        match ticket {
            "dev" => Ok("user:dev".to_string()),
            _ => Err(WsPrismError::AuthFailed),
        }
    }
}
