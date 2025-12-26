//! WebSocket handler (Sprint 1).
//!
//! Responsibilities:
//! - Upgrade HTTP -> WS
//! - Extract tenant/ticket from query string
//! - Create a per-session tracing span
//! - Heartbeat ping + idle timeout
//! - Decode-once and handle minimal auth
//!
//! Non-goals in Sprint 1:
//! - Policy engine
//! - Presence / active_room
//! - Dispatcher services (chat/game)

use axum::{
    extract::{ws::WebSocket, ws::WebSocketUpgrade, Query, State},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};
use tracing::Instrument;

use crate::{app_state::AppState, transport::codec};

#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub tenant: String,
    pub ticket: String,
}

pub async fn ws_upgrade(
    State(state): State<AppState>,
    Query(q): Query<WsQuery>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    // NOTE: we do NOT resolve tenant here yet (Sprint 2: TenantContext)
    ws.on_upgrade(move |socket| handle_socket(state, q, socket))
}

async fn handle_socket(state: AppState, q: WsQuery, socket: WebSocket) {
    // We create a per-session span immediately.
    // user is unknown until ticket resolves.
    let session_id = uuid_like();
    let span = tracing::info_span!(
        "ws_session",
        tenant = %q.tenant,
        session = %session_id,
        user = "-"
    );

    async move {
        // Resolve ticket -> user_id (Sprint 1: stub)
        let user_id = match state.resolve_ticket(&q.ticket) {
            Ok(u) => u,
            Err(e) => {
                tracing::warn!(code = %e.client_code().as_str(), "auth failed");
                // best-effort close
                let mut ws = socket;
                let _ = ws.send(axum::extract::ws::Message::Close(None)).await;
                return;
            }
        };

        // Re-open span with user filled (simple approach).
        let span2 = tracing::info_span!(
            "ws_session",
            tenant = %q.tenant,
            session = %session_id,
            user = %user_id
        );
        run_session(state, q.tenant, user_id, socket).instrument(span2).await;
    }
    .instrument(span)
    .await;
}

async fn run_session(state: AppState, tenant: String, user_id: String, socket: WebSocket) {
    let (mut ws_tx, mut ws_rx) = socket.split();

    // Outbound queue (writer task). Size can be tuned later.
    let (out_tx, mut out_rx) = mpsc::channel::<axum::extract::ws::Message>(1024);

    // Writer task
    let writer = tokio::spawn(async move {
        while let Some(msg) = out_rx.recv().await {
            if ws_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    // Heartbeat + idle timeout configuration from config
    let ping_interval = Duration::from_millis(state.cfg().gateway.ping_interval_ms);
    let idle_timeout = Duration::from_millis(state.cfg().gateway.idle_timeout_ms);

    let mut last_rx = Instant::now();
    let mut ping_tick = tokio::time::interval(ping_interval);

    // Send initial "authed" message (Sprint 1)
    let authed = format!(
        r#"{{\"v\":1,\"svc\":\"sys\",\"type\":\"authed\",\"flags\":0,\"data\":{{\"tenant\":\"{}\",\"user\":\"{}\"}}}}"#,
        tenant, user_id
    );
    let _ = out_tx.send(axum::extract::ws::Message::Text(authed)).await;

    loop {
        tokio::select! {
            _ = ping_tick.tick() => {
                // Proactive ping; if client is dead, writer will fail eventually.
                let _ = out_tx.try_send(axum::extract::ws::Message::Ping(Vec::new()));
            }

            incoming = ws_rx.next() => {
                match incoming {
                    Some(Ok(msg)) => {
                        last_rx = Instant::now();

                        // Decode once
                        match codec::decode(msg) {
                            Ok(codec::Inbound::Ping(v)) => {
                                // Reply pong quickly
                                let _ = out_tx.try_send(axum::extract::ws::Message::Pong(v));
                            }
                            Ok(codec::Inbound::Pong(_)) => {
                                // keep-alive acknowledgement
                            }
                            Ok(codec::Inbound::Close) => break,
                            Ok(codec::Inbound::Text(env)) => {
                                // Sprint 1: just log. (Policy/Plugin/Service in later sprints)
                                tracing::debug!(svc=%env.svc, msg_type=%env.msg_type, flags=env.flags, "text inbound");
                            }
                            Ok(codec::Inbound::Hot(frame)) => {
                                tracing::debug!(svc_id=frame.svc_id, opcode=frame.opcode, flags=frame.flags, seq=?frame.seq, payload_len=frame.payload.len(), "hot inbound");
                            }
                            Ok(codec::Inbound::Other) => {}
                            Err(e) => {
                                // Bad request etc. In Sprint 1 we just log and continue.
                                tracing::warn!(code=%e.client_code().as_str(), err=%e.to_string(), "decode failed");
                            }
                        }
                    }
                    Some(Err(_e)) => break,
                    None => break,
                }
            }

            _ = tokio::time::sleep_until((last_rx + idle_timeout).into()) => {
                tracing::info!("idle timeout; closing");
                break;
            }
        }
    }

    // Best-effort close
    let _ = out_tx.send(axum::extract::ws::Message::Close(None)).await;
    writer.abort();
}

/// Small session id generator without extra dependencies.
/// (Replace with uuid crate later if desired.)
fn uuid_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
    format!("{:x}", nanos)
}
