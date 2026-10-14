//! WebSocket upgrade handler.
//!
//! Runs the admission checks (`transport::admission`), then switches
//! protocols with the tenant frame limit in the codec (S2) and starts the
//! session under its own tracing span (C6). Everything after the upgrade
//! lives in `transport::session`.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::{
    extract::{connect_info::ConnectInfo, ws::WebSocketUpgrade, Query, State},
    http::HeaderMap,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use tracing::Instrument;

use crate::app_state::AppState;
use crate::transport::admission::{self, HandshakeRequest};
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

/// WebSocket upgrade query parameters.
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub tenant: String,
    /// Legacy ticket carrier; honoured only if the tenant allows `query`.
    #[serde(default)]
    pub ticket: Option<String>,
    /// Optional client-provided session id (tab/browser id). Generated if absent.
    #[serde(default)]
    pub sid: Option<String>,
}

pub async fn ws_upgrade(
    State(app): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
) -> Response {
    let req = HandshakeRequest {
        tenant: &q.tenant,
        query_ticket: q.ticket.as_deref(),
        peer,
        headers: &headers,
    };
    let admitted = match admission::admit(&app, &req) {
        Ok(admitted) => admitted,
        Err(rejection) => return rejection.into_response(),
    };

    app.metrics()
        .ws_upgrades
        .inc(&[("tenant", &q.tenant), ("status", "ok")]);
    let ws = match admitted.subprotocol {
        Some(version) => ws.protocols([version]),
        None => ws,
    };
    // S2: enforce the tenant frame limit in the WS codec itself, so oversized
    // payloads are rejected before they are buffered in full or decoded.
    ws.max_message_size(admitted.max_frame_bytes)
        .max_frame_size(admitted.max_frame_bytes)
        .on_upgrade(move |socket| {
            let sid = q.sid.clone().unwrap_or_else(gen_sid);
            let trace_id = gen_trace();
            let user_id = admitted.user_id;
            let ip = admitted.client_ip;
            // C6: instrument the whole session future instead of entering the
            // span across awaits (clippy.toml bans holding span guards there).
            let span = tracing::info_span!("ws", %trace_id, t=%q.tenant, u=%user_id, s=%sid, %ip);
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
