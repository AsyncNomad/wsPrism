use std::sync::Arc;

use wsprism_core::error::{Result, WsPrismError};

use crate::{app_state::AppState, policy};

/// Immutable metadata for a connected session.
/// Immutable metadata for a connected session (tenant/user/sid).
#[derive(Debug, Clone)]
pub struct SessionMeta {
    /// Tenant identifier.
    pub tenant_id: String,
    /// User identifier within the tenant.
    pub user_id: String,
    /// Session identifier (per-connection).
    pub session_id: String,
}

/// Resolved tenant runtime (compiled policy + limits) for a given session.
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
