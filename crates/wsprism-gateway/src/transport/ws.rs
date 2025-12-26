//! WebSocket handler (Sprint 3).
//!
//! Responsibilities:
//! - Upgrade HTTP -> WS
//! - Extract tenant/ticket from query string
//! - Resolve tenant policy + auth(ticket -> user_id)
//! - Lifecycle: ping/pong + idle timeout
//! - Decode-once -> Policy(cheap) -> session-local active_room update
//! - Sprint 3 wiring:
//!   - Register outbound sender into RealtimeCore SessionRegistry
//!   - Update Presence on join/leave
//!   - Dispatch Text/Hot messages to services via Dispatcher
//!
//! Notes:
//! - Hot Lane(binary)는 active_room 1개만 라우팅 (room id 없음).
//! - Transport는 "room join/leave"만 최소한으로 처리(세션 로컬 active_room + presence).
//!   나머지는 Dispatcher/Service로 넘김 (Sprint 4~에서 room service로 이동 가능)

use axum::{
    extract::{ws::Message, ws::WebSocket, ws::WebSocketUpgrade, Query, State},
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use wsprism_core::error::{Result, WsPrismError};

use crate::app_state::AppState;
use crate::policy::engine::PolicyDecision;
use crate::transport::codec::{decode, Inbound};

// Sprint 3
use crate::dispatch::Dispatcher;
use crate::realtime::core::Connection;
use crate::realtime::{RealtimeCore, RealtimeCtx};

// --------------------
// Query parsing
// --------------------
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub tenant: String,
    pub ticket: String,
}

// --------------------
// Session local state
// --------------------
#[derive(Debug)]
struct SessionState {
    active_room: Option<String>,
    last_activity: Instant,
}

// --------------------
// Safe JSON builders
// --------------------
fn sys_authed_json(tenant: &str, user: &str) -> String {
    json!({
        "v": 1,
        "svc": "sys",
        "type": "authed",
        "flags": 0,
        "data": { "tenant": tenant, "user": user }
    })
    .to_string()
}

fn sys_error_json(code: &str, msg: &str) -> String {
    json!({
        "v": 1,
        "svc": "sys",
        "type": "error",
        "flags": 0,
        "data": { "code": code, "msg": msg }
    })
    .to_string()
}

fn sys_joined_json(room: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "joined", "flags": 0, "room": room }).to_string()
}

fn sys_left_json() -> String {
    json!({ "v": 1, "svc": "sys", "type": "left", "flags": 0 }).to_string()
}

// --------------------
// Entry
// --------------------
pub async fn ws_upgrade(
    State(app): State<AppState>,
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
) -> Response {
    ws.on_upgrade(move |socket| async move {
        // 로깅 정책은 네가 원복했다고 했으니 조용히 처리
        let _ = run_session(app, q, socket).await;
    })
}

// --------------------
// Core session loop
// --------------------
async fn run_session(app: AppState, q: WsQuery, socket: WebSocket) -> Result<()> {
    // ---- resolve tenant policy runtime
    let policy = app
        .tenant_policy(&q.tenant)
        .ok_or_else(|| WsPrismError::BadRequest("unknown tenant".into()))?;

    // ---- auth ticket -> user_id
    let user_id = app.resolve_ticket(&q.ticket)?;

    // ---- Sprint3: shared core/dispatcher (AppState에 반드시 있어야 함)
    let core: std::sync::Arc<RealtimeCore> = app.realtime();
    let dispatcher: std::sync::Arc<Dispatcher> = app.dispatcher();

    // ---- outbound channel
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(1024);

    // ---- split socket
    let (mut ws_tx, mut ws_rx) = socket.split();

    // ---- Sprint3: register session into registry
    core.sessions.insert(
        user_id.clone(),
        Connection {
            tx: out_tx.clone(),
        },
    );

    // ---- send authed (SAFE JSON)
    out_tx
        .send(Message::Text(sys_authed_json(&q.tenant, &user_id)))
        .await
        .map_err(|_| WsPrismError::Internal("outbound channel closed".into()))?;

    // ---- timers (Sprint1)
    let gw = &app.cfg().gateway;
    let ping_every = Duration::from_millis(gw.ping_interval_ms);
    let idle_timeout = Duration::from_millis(gw.idle_timeout_ms);

    let mut ping_tick = tokio::time::interval(ping_every);
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut sess = SessionState {
        active_room: None,
        last_activity: Instant::now(),
    };

    // ---- main loop
    loop {
        tokio::select! {
            // outbound writer
            maybe_out = out_rx.recv() => {
                match maybe_out {
                    Some(m) => {
                        if ws_tx.send(m).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }

            // inbound reader
            incoming = ws_rx.next() => {
                let Some(incoming) = incoming else { break; };
                let Ok(msg) = incoming else { break; };

                sess.last_activity = Instant::now();

                // ---- decode once
                let decoded = decode(msg)?;
                match decoded {
                    Inbound::Ping(payload) => {
                        // 명시적 pong
                        let _ = out_tx.send(Message::Pong(payload)).await;
                    }
                    Inbound::Pong(_) => {}
                    Inbound::Close => break,

                    Inbound::Text { env, bytes_len } => {
                        // ---- policy (ext lane)
                        match policy.check_text(bytes_len, &env.svc, &env.msg_type) {
                            PolicyDecision::Pass => {}
                            PolicyDecision::Drop => continue,
                            PolicyDecision::Reject { code, msg } => {
                                let _ = out_tx.send(Message::Text(sys_error_json(code.as_str(), msg))).await;
                                continue;
                            }
                            PolicyDecision::Close { code, msg } => {
                                let _ = out_tx.send(Message::Text(sys_error_json(code.as_str(), msg))).await;
                                break;
                            }
                        }

                        // ---- Sprint2+3: room join/leave는 session-local active_room + presence 업데이트
                        if env.svc == "room" && env.msg_type == "join" {
                            let room = env.room.clone().unwrap_or_else(|| "default".to_string());
                            sess.active_room = Some(room.clone());
                            core.presence.join(&room, &user_id);
                            let _ = out_tx.send(Message::Text(sys_joined_json(&room))).await;
                            continue;
                        }

                        if env.svc == "room" && env.msg_type == "leave" {
                            if let Some(room) = sess.active_room.take() {
                                core.presence.leave(&room, &user_id);
                            }
                            let _ = out_tx.send(Message::Text(sys_left_json())).await;
                            continue;
                        }

                        // ---- Sprint3: dispatch to TextService
                        let ctx = RealtimeCtx::new(
                            q.tenant.clone(),
                            user_id.clone(),
                            sess.active_room.clone(),
                            core.clone(),
                        );

                        if let Err(e) = dispatcher.dispatch_text(ctx, env).await {
                            // 서비스 에러는 sys:error로 전달 (연결을 바로 끊진 않음)
                            let _ = out_tx.send(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string()))).await;
                        }
                    }

                    Inbound::Hot { frame, bytes_len } => {
                        // ---- policy (hot lane)
                        match policy.check_hot(bytes_len, frame.svc_id, frame.opcode) {
                            PolicyDecision::Pass => {}
                            PolicyDecision::Drop => continue,
                            PolicyDecision::Reject { code, msg } => {
                                let _ = out_tx.send(Message::Text(sys_error_json(code.as_str(), msg))).await;
                                continue;
                            }
                            PolicyDecision::Close { code, msg } => {
                                let _ = out_tx.send(Message::Text(sys_error_json(code.as_str(), msg))).await;
                                break;
                            }
                        }

                        // ---- Hot Lane routes to active_room only
                        if sess.active_room.is_none() {
                            let _ = out_tx.send(Message::Text(sys_error_json("BAD_REQUEST", "no active_room"))).await;
                            continue;
                        }

                        let ctx = RealtimeCtx::new(
                            q.tenant.clone(),
                            user_id.clone(),
                            sess.active_room.clone(),
                            core.clone(),
                        );

                        if let Err(e) = dispatcher.dispatch_hot(ctx, frame).await {
                            let _ = out_tx.send(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string()))).await;
                        }
                    }
                }
            }

            // ping tick
            _ = ping_tick.tick() => {
                let _ = out_tx.send(Message::Ping(Vec::new())).await;
            }

            // idle timeout poll
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if sess.last_activity.elapsed() >= idle_timeout {
                    let _ = out_tx.send(Message::Text(sys_error_json("TIMEOUT", "idle timeout"))).await;
                    break;
                }
            }
        }
    }

    // ---- cleanup
    core.sessions.remove(&user_id);
    core.presence.cleanup_user(&user_id);

    Ok(())
}
