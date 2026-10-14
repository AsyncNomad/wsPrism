//! WebSocket upgrade (admission before the protocol switch).
//!
//! - Handshake limiter (429), draining (503), unknown tenant (400),
//!   authentication (401), tenant capacity (503)
//! - Tenant frame limit enforced in the WS codec (S2)
//! - Trace id / sid generation and the per-session tracing span (C6)
//!
//! Everything after the upgrade lives in `transport::session`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{connect_info::ConnectInfo, ws::WebSocketUpgrade, Query, State},
    http::{header::RETRY_AFTER, HeaderMap, StatusCode},
    response::IntoResponse,
};
use serde::Deserialize;
use tracing::Instrument;

use crate::app_state::AppState;
use crate::transport::handshake::retry_after_header;
use crate::transport::session::{self, SessionIdentity};

static NEXT_SID: AtomicU64 = AtomicU64::new(1);
static NEXT_TRACE: AtomicU64 = AtomicU64::new(1);

fn gen_sid() -> String {
    format!("{:x}", NEXT_SID.fetch_add(1, Ordering::Relaxed))
}
fn gen_trace() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let seq = NEXT_TRACE.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}", now, seq)
}

/// Fixed `tenant` label for requests naming a tenant that isn't configured.
const UNKNOWN_TENANT_LABEL: &str = "_unknown";

/// WebSocket upgrade query parameters.
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub tenant: String,
    pub ticket: String,
    /// Optional client-provided session id (tab/browser id). Generated if absent.
    #[serde(default)]
    pub sid: Option<String>,
}

pub async fn ws_upgrade(
    State(app): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
) -> impl IntoResponse {
    let t_cfg = app.tenant(&q.tenant);
    // S5: metric labels must come from config, never from the raw query.
    let tenant_label = t_cfg.map_or(UNKNOWN_TENANT_LABEL, |t| t.id.as_str());

    if let Err(wait_secs) = app.handshake().check(addr.ip()) {
        app.metrics()
            .handshake_rejections
            .inc(&[("tenant", tenant_label), ("reason", "rate_limit")]);
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, retry_after_header(wait_secs));
        return (StatusCode::TOO_MANY_REQUESTS, headers, "Too Many Requests").into_response();
    }
    if app.is_draining() {
        return (StatusCode::SERVICE_UNAVAILABLE, "draining").into_response();
    }
    let Some(t_cfg) = t_cfg else {
        app.metrics()
            .handshake_rejections
            .inc(&[("tenant", tenant_label), ("reason", "unknown_tenant")]);
        return (StatusCode::BAD_REQUEST, "Unknown Tenant").into_response();
    };
    // C3: authenticate before upgrading, so unauthenticated clients get a
    // proper HTTP 401 and never consume a socket task.
    let user_id = match app.resolve_ticket(&q.ticket) {
        Ok(u) => u,
        Err(_) => {
            app.metrics()
                .handshake_rejections
                .inc(&[("tenant", tenant_label), ("reason", "auth_failed")]);
            return (StatusCode::UNAUTHORIZED, "Unauthorized").into_response();
        }
    };
    // S2: enforce the tenant frame limit in the WS codec itself, so oversized
    // payloads are rejected before they are buffered in full or decoded.
    let max_frame_bytes = t_cfg.limits.max_frame_bytes;
    {
        let limit = t_cfg.limits.max_sessions_total;
        if limit > 0 {
            let current = app.realtime().sessions.count_tenant_sessions(&q.tenant);
            if current >= limit {
                app.metrics()
                    .handshake_rejections
                    .inc(&[("tenant", &q.tenant), ("reason", "tenant_capacity")]);
                let mut headers = HeaderMap::new();
                headers.insert(RETRY_AFTER, retry_after_header(1));
                return (
                    StatusCode::SERVICE_UNAVAILABLE,
                    headers,
                    "Tenant Capacity Exceeded",
                )
                    .into_response();
            }
        }
    }

    app.metrics()
        .ws_upgrades
        .inc(&[("tenant", &q.tenant), ("status", "ok")]);
    ws.max_message_size(max_frame_bytes)
        .max_frame_size(max_frame_bytes)
        .on_upgrade(move |socket| {
            let sid = q.sid.clone().unwrap_or_else(gen_sid);
            let trace_id = gen_trace();
            // C6: instrument the whole session future instead of entering the
            // span across awaits (clippy.toml bans holding span guards there).
            let span = tracing::info_span!("ws", %trace_id, t=%q.tenant, u=%user_id, s=%sid);
            let id = SessionIdentity {
                tenant: q.tenant,
                user_id,
                sid,
                trace_id,
            };
            async move {
                if let Err(e) = session::run(app, id, socket).await {
                    tracing::error!("session error: {}", e);
                }
            }
            .instrument(span)
        })
}
