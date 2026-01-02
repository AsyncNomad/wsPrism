//! WebSocket handler (Sprint 3+).
//!
//! Sprint 4-ish hardening included:
//! - Multi-session support (1:1 / 1:N) per tenant policy
//! - Session-based presence (room->sessions), correct cleanup on disconnect
//! - RAII cleanup guard (no leak on any error path)
//! - Connection-level rate limiting (optional) + tenant-level (optional)
//! - Hot lane error surface configurable (sys.error vs silent)
//! - Roomless hot lane configurable

use axum::{
    extract::{ws:: CloseFrame, ws::Message, ws::WebSocket, ws::WebSocketUpgrade, Query, State},
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use wsprism_core::error::{Result, WsPrismError};

use crate::app_state::AppState;
use crate::dispatch::Dispatcher;
use crate::policy::engine::{ConnRateLimiter, HotErrorMode, OnExceed, PolicyDecision};
use crate::realtime::core::Connection;
use crate::realtime::{RealtimeCore, RealtimeCtx};
use crate::transport::codec::{decode, Inbound};

static NEXT_SID: AtomicU64 = AtomicU64::new(1);

fn gen_sid() -> String {
    let n = NEXT_SID.fetch_add(1, Ordering::Relaxed);
    // short deterministic sid
    format!("{:x}", n)
}

// --------------------
// Query parsing
// --------------------
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub tenant: String,
    pub ticket: String,

    /// Optional client-provided session id (e.g. tab id). If absent, server generates.
    #[serde(default)]
    pub sid: Option<String>,
}

// --------------------
// Session local state
// --------------------
#[derive(Debug)]
struct SessionState {
    active_room: Option<String>,
    last_activity: Instant,
    conn_limiter: Option<ConnRateLimiter>,
}

// --------------------
// Safe JSON builders
// --------------------
fn sys_authed_json(tenant: &str, user: &str, sid: &str) -> String {
    json!({
        "v": 1,
        "svc": "sys",
        "type": "authed",
        "flags": 0,
        "data": { "tenant": tenant, "user": user, "sid": sid }
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

fn sys_kicked_json(reason: &str) -> String {
    json!({
        "v": 1,
        "svc": "sys",
        "type": "kicked",
        "flags": 0,
        "data": { "reason": reason }
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
// RAII cleanup
// --------------------
struct SessionCleanup {
    core: Arc<RealtimeCore>,
    user_key: String,
    session_key: String,
}

impl Drop for SessionCleanup {
    fn drop(&mut self) {
        let _ = self.core.sessions.remove_session(&self.user_key, &self.session_key);
        self.core.presence.cleanup_session(&self.session_key);
        tracing::debug!(user_key=%self.user_key, session_key=%self.session_key, "session cleaned up");
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

    // ---- sid
    let sid = q
        .sid
        .filter(|s| !s.trim().is_empty())
        .filter(|s| s.len() <= 64)
        .unwrap_or_else(gen_sid);

    // ---- shared core/dispatcher
    let core: Arc<RealtimeCore> = app.realtime();
    let dispatcher: Arc<Dispatcher> = app.dispatcher();

    // ---- derived keys (tenant scoped)
    let user_key = format!("{}::{}", q.tenant, user_id);
    let session_key = format!("{}::{}::{}", q.tenant, user_id, sid);

    // ---- per-session span
    let span = tracing::info_span!(
        "ws_session",
        tenant = %q.tenant,
        user = %user_id,
        sid = %sid
    );
    let _enter = span.enter();

    // ---- outbound channel
    let (out_tx, mut out_rx) = mpsc::channel::<Message>(1024);

    // ---- split socket
    let (mut ws_tx, mut ws_rx) = socket.split();

    // ---- Enforce session policy BEFORE insert
    let sp = policy.session_policy();
    let max_sessions = sp.max_sessions_per_user as usize;

    let current = core.sessions.count_user_sessions(&user_key);
    if current >= max_sessions {
        match sp.on_exceed {
            OnExceed::Deny => {
                tracing::debug!(current, max_sessions, "too many sessions: deny");
                let _ = out_tx
                    .send(Message::Text(sys_error_json("TOO_MANY_SESSIONS", "too many sessions")))
                    .await;
                return Ok(());
            }
            OnExceed::KickOldest => {
                tracing::debug!(current, max_sessions, "too many sessions: evict oldest");
                if let Some((victim_session_key, victim_conn)) = core.sessions.evict_oldest(&user_key) {
                    core.presence.cleanup_session(&victim_session_key);

                    if victim_conn.tx.try_send(Message::Text(sys_kicked_json("max_sessions_exceeded"))).is_err() {
                        tracing::warn!(victim_session_key=%victim_session_key, "failed to notify victim (sys.kicked)");
                    }

                    let frame = CloseFrame {
                        code: 1008, // Policy Violation (RFC6455)
                        reason: Cow::from("kicked_by_policy"),
                    };
                    if victim_conn.tx.try_send(Message::Close(Some(frame))).is_err() {
                        tracing::warn!(victim_session_key=%victim_session_key, "failed to close victim session");
                    }
                } else {
                    let _ = out_tx
                        .send(Message::Text(sys_error_json("TOO_MANY_SESSIONS", "too many sessions")))
                        .await;
                    return Ok(());
                }
            }
        }
    }

    // ---- register session
    core.sessions.insert(
        user_key.clone(),
        session_key.clone(),
        Connection { tx: out_tx.clone() },
    );

    // ---- cleanup guard (covers ALL early returns)
    let _cleanup = SessionCleanup {
        core: core.clone(),
        user_key: user_key.clone(),
        session_key: session_key.clone(),
    };

    // ---- send authed
    out_tx
        .send(Message::Text(sys_authed_json(&q.tenant, &user_id, &sid)))
        .await
        .map_err(|_| WsPrismError::Internal("outbound channel closed".into()))?;

    // ---- timers
    let gw = &app.cfg().gateway;
    let ping_every = Duration::from_millis(gw.ping_interval_ms);
    let idle_timeout = Duration::from_millis(gw.idle_timeout_ms);

    let mut ping_tick = tokio::time::interval(ping_every);
    ping_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut idle_tick = tokio::time::interval(Duration::from_millis(250));
    idle_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    let mut sess = SessionState {
        active_room: None,
        last_activity: Instant::now(),
        conn_limiter: policy.new_connection_limiter(),
    };

    // ---- main loop
    loop {
        tokio::select! {
            // outbound writer
            maybe_out = out_rx.recv() => {
                match maybe_out {
                    Some(m) => {
                        if ws_tx.send(m).await.is_err() {
                            tracing::debug!("ws_tx send failed; closing session");
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

                // ---- decode once (no '?': ensure RAII cleanup always works + uniform behavior)
                let decoded = match decode(msg) {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::debug!(err=%e, "decode failed");
                        // Ext-style error surface (client-visible) for decode errors
                        let _ = out_tx.send(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string()))).await;
                        break;
                    }
                };

                match decoded {
                    Inbound::Ping(payload) => {
                        let _ = out_tx.send(Message::Pong(payload)).await;
                    }
                    Inbound::Pong(_) => {}
                    Inbound::Close => break,

                    Inbound::Text { env, bytes_len } => {
                        // ---- per-connection limiter (optional)
                        if let Some(lim) = sess.conn_limiter.as_mut() {
                            if !lim.allow() {
                                // ext: drop (cheap)
                                continue;
                            }
                        }

                        // ---- tenant policy (ext lane)
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

                        // ---- room join/leave (minimal transport responsibility)
                        if env.svc == "room" && env.msg_type == "join" {
                            let room = env.room.clone().unwrap_or_else(|| "default".to_string());
                            sess.active_room = Some(room.clone());

                            let ctx = RealtimeCtx::new(
                                q.tenant.clone(),
                                user_id.clone(),
                                sid.clone(),
                                sess.active_room.clone(),
                                core.clone(),
                            );
                            ctx.join_room(&room);

                            let _ = out_tx.send(Message::Text(sys_joined_json(&room))).await;
                            continue;
                        }

                        if env.svc == "room" && env.msg_type == "leave" {
                            if let Some(room) = sess.active_room.take() {
                                let ctx = RealtimeCtx::new(
                                    q.tenant.clone(),
                                    user_id.clone(),
                                    sid.clone(),
                                    None,
                                    core.clone(),
                                );
                                ctx.leave_room(&room);
                            }
                            let _ = out_tx.send(Message::Text(sys_left_json())).await;
                            continue;
                        }

                        // ---- dispatch to TextService
                        let ctx = RealtimeCtx::new(
                            q.tenant.clone(),
                            user_id.clone(),
                            sid.clone(),
                            sess.active_room.clone(),
                            core.clone(),
                        );

                        if let Err(e) = dispatcher.dispatch_text(ctx, env).await {
                            tracing::debug!(code=%e.client_code().as_str(), err=%e, "text service error");
                            let _ = out_tx.send(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string()))).await;
                        }
                    }

                    Inbound::Hot { frame, bytes_len } => {
                        // ---- per-connection limiter (optional)
                        if let Some(lim) = sess.conn_limiter.as_mut() {
                            if !lim.allow() {
                                // hot: silent drop
                                continue;
                            }
                        }

                        // ---- tenant policy (hot lane)
                        match policy.check_hot(bytes_len, frame.svc_id, frame.opcode) {
                            PolicyDecision::Pass => {}
                            PolicyDecision::Drop => continue,
                            PolicyDecision::Reject { code, msg } => {
                                // hot error surface configurable
                                match policy.hot_error_mode() {
                                    HotErrorMode::Silent => continue,
                                    HotErrorMode::SysError => {
                                        let _ = out_tx.send(Message::Text(sys_error_json(code.as_str(), msg))).await;
                                        continue;
                                    }
                                }
                            }
                            PolicyDecision::Close { code, msg } => {
                                match policy.hot_error_mode() {
                                    HotErrorMode::Silent => break,
                                    HotErrorMode::SysError => {
                                        let _ = out_tx.send(Message::Text(sys_error_json(code.as_str(), msg))).await;
                                        break;
                                    }
                                }
                            }
                        }

                        // ---- Hot Lane room requirement configurable
                        if policy.hot_requires_active_room() && sess.active_room.is_none() {
                            match policy.hot_error_mode() {
                                HotErrorMode::Silent => continue,
                                HotErrorMode::SysError => {
                                    let _ = out_tx.send(Message::Text(sys_error_json("BAD_REQUEST", "no active_room"))).await;
                                    continue;
                                }
                            }
                        }

                        let ctx = RealtimeCtx::new(
                            q.tenant.clone(),
                            user_id.clone(),
                            sid.clone(),
                            sess.active_room.clone(),
                            core.clone(),
                        );

                        if let Err(e) = dispatcher.dispatch_hot(ctx, frame).await {
                            tracing::debug!(code=%e.client_code().as_str(), err=%e, "hot service error");
                            match policy.hot_error_mode() {
                                HotErrorMode::Silent => {}
                                HotErrorMode::SysError => {
                                    let _ = out_tx.send(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string()))).await;
                                }
                            }
                        }
                    }
                }
            }

            // ping tick
            _ = ping_tick.tick() => {
                let _ = out_tx.send(Message::Ping(Vec::new())).await;
            }

            // idle timeout poll (interval, cheaper than sleep-in-select)
            _ = idle_tick.tick() => {
                if sess.last_activity.elapsed() >= idle_timeout {
                    tracing::debug!(elapsed_ms=%sess.last_activity.elapsed().as_millis(), "idle timeout");
                    let _ = out_tx.send(Message::Text(sys_error_json("TIMEOUT", "idle timeout"))).await;
                    break;
                }
            }
        }
    }

    Ok(())
}
