use std::sync::Arc;

use wsprism_core::error::{Result, WsPrismError};

use crate::{app_state::AppState, policy};

/// Immutable metadata for a connected session.
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub tenant_id: String,
    pub user_id: String,
    pub session_id: String,
}

/// Resolved tenant runtime (compiled policy + limits).
#[derive(Clone)]
pub struct TenantContext {
    pub meta: SessionMeta,
    pub policy: Arc<policy::TenantPolicyRuntime>,
}

impl TenantContext {
    pub fn tenant_id(&self) -> &str {
        &self.meta.tenant_id
    }
    pub fn user_id(&self) -> &str {
        &self.meta.user_id
    }
    pub fn session_id(&self) -> &str {
        &self.meta.session_id
    }
}

/// Resolve tenant runtime or return a client-visible error.
pub fn resolve_tenant(state: &AppState, tenant_id: &str) -> Result<Arc<policy::TenantPolicyRuntime>> {
    state
        .tenant_policy(tenant_id)
        .ok_or_else(|| WsPrismError::BadRequest(format!("unknown tenant: {tenant_id}")))
}
