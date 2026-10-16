//! Connect proxy (ADR 0008 §6): the tenant's backend decides who connects.
//!
//! `POST url {tenant, token, client_ip}` -> `200 {user, permissions?,
//! expires_at?}` admits, `401`/`403` rejects, anything else (other status,
//! timeout, unreachable, malformed reply) is "unavailable" (503 +
//! Retry-After), so a backend outage is not reported as bad credentials.

use std::time::{Duration, UNIX_EPOCH};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::Semaphore;

use super::{AuthError, AuthRequest, Authenticator, Identity, Permissions};
use crate::backend::{Backend, Endpoint};
use crate::config::schema::ProxyAuthConfig;
use wsprism_protocol::error::Result;

pub struct ProxyAuthenticator {
    backend: Backend,
    endpoint: Endpoint,
    /// Bounds concurrent calls, so a connection storm cannot pile up
    /// unbounded requests on a struggling backend.
    in_flight: Semaphore,
}

#[derive(Deserialize)]
struct Reply {
    user: String,
    #[serde(default)]
    permissions: Option<PermissionLists>,
    /// Unix seconds; the session is closed then.
    #[serde(default)]
    expires_at: Option<u64>,
}

#[derive(Deserialize)]
struct PermissionLists {
    #[serde(default)]
    subscribe: Option<Vec<String>>,
    #[serde(default)]
    publish: Option<Vec<String>>,
}

impl ProxyAuthenticator {
    pub fn from_config(cfg: &ProxyAuthConfig, backend: &Backend) -> Result<Self> {
        cfg.validate()?;
        Ok(Self {
            backend: backend.clone(),
            endpoint: Endpoint::new(
                "auth proxy",
                &cfg.url,
                cfg.timeout_ms,
                cfg.secret_env.as_deref(),
            )?,
            in_flight: Semaphore::new(cfg.max_in_flight as usize),
        })
    }
}

#[async_trait]
impl Authenticator for ProxyAuthenticator {
    fn kind(&self) -> &'static str {
        "proxy"
    }

    async fn authenticate(
        &self,
        req: &AuthRequest<'_>,
    ) -> std::result::Result<Identity, AuthError> {
        let _permit = self
            .in_flight
            .try_acquire()
            .map_err(|_| AuthError::Unavailable("connect proxy saturated"))?;
        let body = json!({"tenant": req.tenant, "token": req.token, "client_ip": req.client_ip.to_string()});
        let (status, body) = self
            .backend
            .post_json(&self.endpoint, &body)
            .await
            .map_err(|_| AuthError::Unavailable("connect proxy unreachable"))?;
        match status {
            200 => {}
            401 | 403 => return Err(AuthError::Rejected("connect proxy rejected")),
            _ => return Err(AuthError::Unavailable("connect proxy error status")),
        }
        let reply: Reply = serde_json::from_slice(&body)
            .map_err(|_| AuthError::Unavailable("connect proxy malformed reply"))?;
        if !crate::ident::is_safe_id(&reply.user) {
            return Err(AuthError::Rejected("connect proxy user is not a valid id"));
        }
        let permissions = reply
            .permissions
            .map(|p| Permissions::from_lists(p.subscribe, p.publish))
            .unwrap_or_default();
        Ok(Identity {
            user_id: reply.user,
            permissions,
            expires_at: reply
                .expires_at
                .map(|s| UNIX_EPOCH + Duration::from_secs(s)),
        })
    }
}
