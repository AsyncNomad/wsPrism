//! Handshake admission: every check a WebSocket upgrade must pass, in order.
//!
//! Each check is a small function returning `Result<_, Rejection>`; `admit`
//! runs them in sequence and counts rejections. New admission rules (origin
//! policy, trusted proxies, pluggable authenticators) are added here as
//! another step, without touching the upgrade handler.

use std::net::{IpAddr, SocketAddr};

use axum::http::{
    header::{ORIGIN, RETRY_AFTER},
    HeaderMap, StatusCode,
};
use axum::response::{IntoResponse, Response};

use crate::app_state::AppState;
use crate::config::TenantConfig;
use crate::policy::TenantPolicyRuntime;
use crate::transport::credentials;
use crate::transport::handshake::retry_after_header;
use crate::transport::subprotocol::{Negotiated, Offered};

/// Fixed `tenant` label for requests naming a tenant that isn't configured.
const UNKNOWN_TENANT_LABEL: &str = "_unknown";

/// What the admission checks get to see of an upgrade request.
pub struct HandshakeRequest<'a> {
    /// Tenant id as requested by the client (unvalidated).
    pub tenant: &'a str,
    /// `?ticket=` (only honoured if the tenant lists `query` in ticket_sources).
    pub query_ticket: Option<&'a str>,
    /// TCP peer address.
    pub peer: SocketAddr,
    pub headers: &'a HeaderMap,
}

/// A request that passed every check.
pub struct Admitted {
    pub user_id: String,
    /// Subprotocol to echo in the upgrade response, if one was negotiated.
    pub subprotocol: Option<&'static str>,
    /// Client address (resolved through trusted proxies).
    pub client_ip: IpAddr,
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
    let client_ip = app.trusted_proxies().client_ip(req.peer.ip(), req.headers);
    rate_limit(app, client_ip)?;
    not_draining(app)?;
    let tenant = tenant.ok_or_else(|| {
        Rejection::new(StatusCode::BAD_REQUEST, "unknown_tenant", "Unknown Tenant")
    })?;
    let policy = app.tenant_policy(&tenant.id).ok_or_else(|| {
        Rejection::new(StatusCode::BAD_REQUEST, "unknown_tenant", "Unknown Tenant")
    })?;
    let offered = Offered::parse(req.headers);
    let subprotocol = negotiate_protocol(&offered)?;
    origin_allowed(&policy, req)?;
    let user_id = authenticate(app, &policy, req, &offered)?;
    tenant_capacity(app, tenant)?;
    Ok(Admitted {
        user_id,
        subprotocol,
        client_ip,
        max_frame_bytes: tenant.limits.max_frame_bytes,
    })
}

/// Handshake defender: per-client-IP and global token buckets (S6).
fn rate_limit(app: &AppState, client_ip: IpAddr) -> Result<(), Rejection> {
    app.handshake().check(client_ip).map_err(|wait_secs| {
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

/// Protocol version via `Sec-WebSocket-Protocol` (see `transport::subprotocol`).
fn negotiate_protocol(offered: &Offered<'_>) -> Result<Option<&'static str>, Rejection> {
    match offered.negotiate() {
        Negotiated::Implicit => Ok(None),
        Negotiated::Selected(version) => Ok(Some(version)),
        Negotiated::Unsupported => Err(Rejection::new(
            StatusCode::BAD_REQUEST,
            "unsupported_protocol",
            "Unsupported Sec-WebSocket-Protocol (offer wsprism.v1)",
        )),
    }
}

/// Cross-site WebSocket hijacking protection (see `policy::origin`).
fn origin_allowed(
    policy: &TenantPolicyRuntime,
    req: &HandshakeRequest<'_>,
) -> Result<(), Rejection> {
    let origin = req.headers.get(ORIGIN).and_then(|v| v.to_str().ok());
    if !policy.origins().allows(origin) {
        return Err(Rejection::new(
            StatusCode::FORBIDDEN,
            "origin_not_allowed",
            "Origin Not Allowed",
        ));
    }
    Ok(())
}

/// C3: authenticate before upgrading, so unauthenticated clients get a proper
/// HTTP 401 and never consume a socket task. The ticket is read from the
/// tenant's `ticket_sources` (see `transport::credentials`).
fn authenticate(
    app: &AppState,
    policy: &TenantPolicyRuntime,
    req: &HandshakeRequest<'_>,
    offered: &Offered<'_>,
) -> Result<String, Rejection> {
    let ticket = credentials::find(policy.ticket_sources(), req, offered)
        .ok_or_else(|| Rejection::new(StatusCode::UNAUTHORIZED, "auth_missing", "Unauthorized"))?;
    app.resolve_ticket(ticket)
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
