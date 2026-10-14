//! Handshake admission: every check a WebSocket upgrade must pass, in order.
//!
//! Each check is a small function returning `Result<_, Rejection>`; `admit`
//! runs them in sequence and counts rejections. New admission rules (origin
//! policy, trusted proxies, pluggable authenticators) are added here as
//! another step, without touching the upgrade handler.

use std::net::SocketAddr;

use axum::http::{header::RETRY_AFTER, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};

use crate::app_state::AppState;
use crate::config::TenantConfig;
use crate::transport::handshake::retry_after_header;

/// Fixed `tenant` label for requests naming a tenant that isn't configured.
const UNKNOWN_TENANT_LABEL: &str = "_unknown";

/// What the admission checks get to see of an upgrade request.
pub struct HandshakeRequest<'a> {
    /// Tenant id as requested by the client (unvalidated).
    pub tenant: &'a str,
    pub ticket: &'a str,
    /// TCP peer address.
    pub peer: SocketAddr,
    pub headers: &'a HeaderMap,
}

/// A request that passed every check.
pub struct Admitted {
    pub user_id: String,
    /// Tenant frame limit, enforced in the WS codec (S2).
    pub max_frame_bytes: usize,
}

/// Why a request was turned away, rendered as a plain HTTP response.
#[derive(Debug)]
pub struct Rejection {
    pub status: StatusCode,
    /// Metric label for `handshake_rejections{reason}` (static, bounded set).
    pub reason: &'static str,
    pub body: &'static str,
    /// Seconds for a `Retry-After` header, if the client may retry.
    pub retry_after: Option<u64>,
}

impl Rejection {
    fn new(status: StatusCode, reason: &'static str, body: &'static str) -> Self {
        Self {
            status,
            reason,
            body,
            retry_after: None,
        }
    }

    fn retry_after(mut self, secs: u64) -> Self {
        self.retry_after = Some(secs);
        self
    }
}

impl IntoResponse for Rejection {
    fn into_response(self) -> Response {
        let mut headers = HeaderMap::new();
        if let Some(secs) = self.retry_after {
            headers.insert(RETRY_AFTER, retry_after_header(secs));
        }
        (self.status, headers, self.body).into_response()
    }
}

/// Run every admission check; count and return the first rejection.
pub fn admit(app: &AppState, req: &HandshakeRequest<'_>) -> Result<Admitted, Rejection> {
    let tenant = app.tenant(req.tenant);
    let result = check_all(app, req, tenant);
    if let Err(rejection) = &result {
        // S5: metric labels must come from config, never from the raw query.
        let label = tenant.map_or(UNKNOWN_TENANT_LABEL, |t| t.id.as_str());
        app.metrics()
            .handshake_rejections
            .inc(&[("tenant", label), ("reason", rejection.reason)]);
    }
    result
}

fn check_all(
    app: &AppState,
    req: &HandshakeRequest<'_>,
    tenant: Option<&TenantConfig>,
) -> Result<Admitted, Rejection> {
    rate_limit(app, req)?;
    not_draining(app)?;
    let tenant = tenant.ok_or_else(|| {
        Rejection::new(StatusCode::BAD_REQUEST, "unknown_tenant", "Unknown Tenant")
    })?;
    let user_id = authenticate(app, req)?;
    tenant_capacity(app, tenant)?;
    Ok(Admitted {
        user_id,
        max_frame_bytes: tenant.limits.max_frame_bytes,
    })
}

/// Handshake defender: per-IP and global token buckets (S6).
fn rate_limit(app: &AppState, req: &HandshakeRequest<'_>) -> Result<(), Rejection> {
    app.handshake().check(req.peer.ip()).map_err(|wait_secs| {
        Rejection::new(
            StatusCode::TOO_MANY_REQUESTS,
            "rate_limit",
            "Too Many Requests",
        )
        .retry_after(wait_secs)
    })
}

fn not_draining(app: &AppState) -> Result<(), Rejection> {
    if app.is_draining() {
        return Err(Rejection::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "draining",
            "draining",
        ));
    }
    Ok(())
}

/// C3: authenticate before upgrading, so unauthenticated clients get a proper
/// HTTP 401 and never consume a socket task.
fn authenticate(app: &AppState, req: &HandshakeRequest<'_>) -> Result<String, Rejection> {
    app.resolve_ticket(req.ticket)
        .map_err(|_| Rejection::new(StatusCode::UNAUTHORIZED, "auth_failed", "Unauthorized"))
}

/// Tenant-wide session cap (best-effort; the registry re-checks on insert).
fn tenant_capacity(app: &AppState, tenant: &TenantConfig) -> Result<(), Rejection> {
    let limit = tenant.limits.max_sessions_total;
    if limit > 0 && app.realtime().sessions.count_tenant_sessions(&tenant.id) >= limit {
        return Err(Rejection::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "tenant_capacity",
            "Tenant Capacity Exceeded",
        )
        .retry_after(1));
    }
    Ok(())
}
