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
    body::Body,
    extract::{
        connect_info::ConnectInfo, ws::WebSocketUpgrade, FromRequestParts, Query, Request, State,
    },
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use hyper_util::rt::TokioIo;
use tokio::net::TcpStream;

use crate::config::schema::TransportKind;
use serde::Deserialize;
use tracing::Instrument;

use crate::app_state::AppState;
use crate::transport::admission::{self, HandshakeRequest};
use crate::transport::session::{self, SessionIdentity};
use crate::transport::subprotocol;

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
    Query(q): Query<WsQuery>,
    req: Request,
) -> Response {
    let (mut parts, _body) = req.into_parts();
    let admitted = {
        let hreq = HandshakeRequest {
            tenant: &q.tenant,
            query_ticket: q.ticket.as_deref(),
            peer,
            headers: &parts.headers,
        };
        match admission::admit(&app, &hreq) {
            Ok(admitted) => admitted,
            Err(rejection) => return rejection.into_response(),
        }
    };

    app.metrics()
        .ws_upgrades
        .inc(&[("tenant", &q.tenant), ("status", "ok")]);
    match app.cfg().gateway.transport {
        TransportKind::Tungstenite => {
            let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &app).await {
                Ok(ws) => ws,
                Err(rejection) => return rejection.into_response(),
            };
            let ws = match admitted.subprotocol {
                Some(version) => ws.protocols([version]),
                None => ws,
            };
            // S2: enforce the tenant frame limit in the WS codec itself, so
            // oversized payloads are rejected before they are buffered in full.
            ws.max_message_size(admitted.max_frame_bytes)
                .max_frame_size(admitted.max_frame_bytes)
                .on_upgrade(move |socket| {
                    let (id, span) = session_start(
                        q,
                        admitted.user_id,
                        admitted.client_ip,
                        admitted.subprotocol,
                    );
                    let protocol = id.protocol;
                    async move {
                        if let Err(e) =
                            session::run(app, id, session::split_websocket(socket, protocol)).await
                        {
                            tracing::error!("session error: {}", e);
                        }
                    }
                    .instrument(span)
                })
        }
        TransportKind::Raw => raw_upgrade(app, parts, q, admitted),
    }
}

/// Identity and tracing span for a session about to start.
fn session_start(
    q: WsQuery,
    user_id: String,
    ip: std::net::IpAddr,
    subprotocol: Option<&'static str>,
) -> (SessionIdentity, tracing::Span) {
    let sid = q.sid.unwrap_or_else(gen_sid);
    let trace_id = gen_trace();
    // C6: instrument the whole session future instead of entering the span
    // across awaits (clippy.toml bans holding span guards there).
    let span = tracing::info_span!("ws", %trace_id, t=%q.tenant, u=%user_id, s=%sid, %ip);
    let id = SessionIdentity {
        protocol: subprotocol::version_of(subprotocol),
        tenant: q.tenant,
        user_id,
        sid,
        trace_id,
    };
    (id, span)
}

/// Experimental raw transport (ADR 0007 stage 1): answer the upgrade
/// ourselves and hand the session the upgraded `TcpStream`.
fn raw_upgrade(
    app: AppState,
    mut parts: axum::http::request::Parts,
    q: WsQuery,
    admitted: admission::Admitted,
) -> Response {
    let h = &parts.headers;
    let has = |name: header::HeaderName, token: &str| {
        h.get_all(name)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .any(|v| v.split(',').any(|t| t.trim().eq_ignore_ascii_case(token)))
    };
    let key = h
        .get(header::SEC_WEBSOCKET_KEY)
        .map(|k| k.as_bytes().to_vec());
    let version_ok = h
        .get(header::SEC_WEBSOCKET_VERSION)
        .is_some_and(|v| v.as_bytes() == b"13");
    let (Some(key), true, true, true) = (
        key,
        version_ok,
        has(header::UPGRADE, "websocket"),
        has(header::CONNECTION, "upgrade"),
    ) else {
        return (StatusCode::BAD_REQUEST, "invalid websocket upgrade").into_response();
    };
    let Some(on_upgrade) = parts.extensions.remove::<hyper::upgrade::OnUpgrade>() else {
        return (StatusCode::BAD_REQUEST, "connection is not upgradable").into_response();
    };
    let accept = tungstenite::handshake::derive_accept_key(&key);
    let max = admitted.max_frame_bytes;
    let subprotocol = admitted.subprotocol;
    let (id, span) = session_start(q, admitted.user_id, admitted.client_ip, subprotocol);
    let protocol = id.protocol;
    tokio::spawn(
        async move {
            let upgraded = match on_upgrade.await {
                Ok(u) => u,
                Err(e) => return tracing::debug!("upgrade failed: {e}"),
            };
            // A plain TcpStream splits without a lock; behind hyper-util's
            // protocol detection the IO is wrapped in a private type, so fall
            // back to the generic (lightly locked) split of the upgraded IO.
            let result = match upgraded.downcast::<TokioIo<TcpStream>>() {
                Ok(parts) => {
                    let (r, w) = parts.io.into_inner().into_split();
                    let halves =
                        session::raw_from_halves(r, w, parts.read_buf, protocol, max).await;
                    session::run(app, id, halves).await
                }
                Err(upgraded) => {
                    let (r, w) = tokio::io::split(TokioIo::new(upgraded));
                    let halves = session::raw_from_halves(r, w, Bytes::new(), protocol, max).await;
                    session::run(app, id, halves).await
                }
            };
            if let Err(e) = result {
                tracing::error!("session error: {}", e);
            }
        }
        .instrument(span),
    );
    let mut resp = Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::UPGRADE, "websocket")
        .header(header::CONNECTION, "upgrade")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept);
    if let Some(p) = subprotocol {
        resp = resp.header(header::SEC_WEBSOCKET_PROTOCOL, p);
    }
    resp.body(Body::empty())
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}
