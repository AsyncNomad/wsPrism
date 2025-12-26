//! WebSocket handler (Sprint 2).
//!
//! Responsibilities:
//! - Upgrade HTTP -> WS
//! - Extract tenant/ticket from query string
//! - Resolve TenantContext (tenant policy runtime + session meta)
//! - Lifecycle: ping/pong + idle timeout
//! - Decode-once then apply policy (cheap) then update session-local active_room
//!
//! Sprint 2 scope:
//! - PolicyEngine: len limit, allowlist, tenant rate limit
//! - Active Room: join/leave in Ext Lane, Hot Lane routes to active_room only (no room in binary frame)

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

// --------------------
// Query parsing
// --------------------
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub tenant: String,
    pub ticket: String,
}

// --------------------
// Session local state (Sprint 2)
// --------------------
#[derive(Debug)]
struct SessionState {
    active_room: Option<String>,
    last_activity: Instant,
}

// --------------------
// Safe JSON builders (PRODUCTION FIX)
// --------------------
fn sys_authed_json(tenant: &str, user: &str) -> String {
    json!({
        "v": 1,
        "svc": "sys",
        "type": "authed",
        "flags": 0,
        "data": {
            "tenant": tenant,
            "user": user
        }
    })
    .to_string()
}

fn sys_error_json(code: &str, msg: &str) -> String {
    json!({
        "v": 1,
        "svc": "sys",
        "type": "error",
        "flags": 0,
        "data": {
            "code": code,
            "msg": msg
        }
    })
    .to_string()
}

fn sys_joined_json(room: &str) -> String {
    json!({
        "v": 1,
        "svc": "sys",
        "type": "joined",
        "flags": 0,
        "room": room
    })
    .to_string()
}

fn sys_left_json() -> String {
    json!({
        "v": 1,
        "svc": "sys",
        "type": "left",
        "flags": 0
    })
    .to_string()
}

// --------------------
// Cheap frame length helper (policy before decode)
// --------------------
fn frame_len(msg: &Message) -> usize {
    match msg {
        Message::Text(s) => s.as_bytes().len(),
        Message::Binary(b) => b.len(),
        Message::Ping(v) => v.len(),
        Message::Pong(v) => v.len(),
        Message::Close(_) => 0,
    }
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
        if let Err(e) = run_session(app, q, socket).await {
            let _ = e;
        }
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

    // ---- outbound channel
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(1024);

    // ---- split socket
    let (mut ws_tx, mut ws_rx) = socket.split();

    // ---- send authed (SAFE JSON)
    out_tx
        .send(Message::Text(sys_authed_json(&q.tenant, &user_id)))
        .await
        .map_err(|_| WsPrismError::Internal("outbound channel closed".into()))?;

    // ---- timers (Sprint 1)
    let gw = &app.cfg().gateway;
    let ping_every = Duration::from_millis(gw.ping_interval_ms);
    let idle_timeout = Duration::from_millis(gw.idle_timeout_ms);

    let mut ping_tick = tokio::time::interval(ping_every);
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut sess = SessionState {
        active_room: None,
        last_activity: Instant::now(),
    };

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

                // ✅ cheap-first: bytes_len 먼저 계산 (decode보다 앞)
                let bytes_len = frame_len(&msg);

                // ✅ 메시지 타입별로 "decode 이전" 정책 적용
                match &msg {
                    Message::Text(s) => {
                        // Text는 svc/type를 알아야 allowlist 체크 가능 → 헤더만 싸게 파싱
                        // 너의 text::Envelope가 RawValue 기반이라면 여기서 body 파싱은 발생하지 않음.
                        let env: wsprism_core::protocol::text::Envelope = serde_json::from_str(s)
                            .map_err(|e| WsPrismError::BadRequest(format!("decode failed: {e}")))?;

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

                        // ✅ policy 통과했으니 이제 decode (Decode-once 철학 유지)
                        // (decode가 다시 JSON을 파싱할 수 있으니, 앞으로 Sprint3에서는 codec가 "Envelope만" 반환하도록 개선 추천)
                        let decoded = decode(Message::Text(s.clone()))?;
                        match decoded {
                            Inbound::Text { env, .. } => {
                                if env.svc == "room" && env.msg_type == "join" {
                                    let room = env.room.clone().unwrap_or_else(|| "default".to_string());
                                    sess.active_room = Some(room.clone());
                                    let _ = out_tx.send(Message::Text(sys_joined_json(&room))).await;
                                    continue;
                                }
                                if env.svc == "room" && env.msg_type == "leave" {
                                    sess.active_room = None;
                                    let _ = out_tx.send(Message::Text(sys_left_json())).await;
                                    continue;
                                }
                                let _ = env;
                            }
                            // Text로 들어왔는데 다른 타입이면 무시
                            _ => {}
                        }
                    }

                    Message::Binary(_) => {
                        // Hot은 svc_id/opcode를 알아야 allowlist → header만 싸게 파싱해야 함.
                        // 가장 이상적: core hot parser로 header만 빠르게 파싱 (panic-free).
                        // 현재는 decode가 HotFrame까지 만들어주므로, policy를 먼저 적용하려면 header-only peek가 필요.
                        //
                        // ✅ 최소 침습: "bytes_len max_frame" / rate limit 같은 것만 먼저 막고,
                        // svc_id/opcode allowlist는 decode 이후에 체크(완전한 cheap-first는 Sprint3에서 codec 개선으로 해결).
                        //
                        // 그래도 가장 큰 병목인 "크기 폭탄"은 decode 전에 차단됨.
                        match policy.check_hot(bytes_len, 0, 0) {
                            // (주의) check_hot이 svc_id/opcode를 강제하면 여기서 0,0이 문제.
                            // 네 로컬 디버깅에서 이 부분을 이미 맞췄다 했으니,
                            // check_hot이 "len/rate"만 먼저 검사할 수 있도록 구현되어 있다는 전제.
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

                        // 이제 decode
                        let decoded = decode(msg)?;
                        match decoded {
                            Inbound::Hot { frame, bytes_len } => {
                                // ✅ 여기서 svc_id/opcode allowlist를 확실히 적용 (현 상태의 현실적 최선)
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

                                if sess.active_room.is_none() {
                                    let _ = out_tx.send(Message::Text(sys_error_json("BAD_REQUEST", "no active_room"))).await;
                                    continue;
                                }

                                let _ = frame;
                            }
                            _ => {}
                        }
                    }

                    Message::Ping(payload) => {
                        let _ = out_tx.send(Message::Pong(payload.clone())).await;
                    }
                    Message::Pong(_) => {}
                    Message::Close(_) => break,
                }
            }

            // ping
            _ = ping_tick.tick() => {
                let _ = out_tx.send(Message::Ping(Vec::new())).await;
            }

            // idle timeout
            _ = tokio::time::sleep(Duration::from_millis(250)) => {
                if sess.last_activity.elapsed() >= idle_timeout {
                    let _ = out_tx.send(Message::Text(sys_error_json("TIMEOUT", "idle timeout"))).await;
                    break;
                }
            }
        }
    }

    Ok(())
}
