//! WebSocket handler (transport + lifecycle).
//!
//! - Pre-upgrade defenses: IP handshake limiter (429) and tenant capacity (503)
//! - Trace ID generation/propagation into spans and sys.* messages
//! - Session/room governance and policy enforcement
//! - Labeled metrics for policy decisions/errors + sampled Hot Lane latency

use axum::{
    extract::{connect_info::ConnectInfo, ws::CloseFrame, ws::Message, ws::WebSocket, ws::WebSocketUpgrade, Query, State},
    http::{HeaderMap, StatusCode, header::RETRY_AFTER},
    response::{IntoResponse},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration, Instant};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use wsprism_core::error::{Result, WsPrismError};
use crate::app_state::AppState;
use crate::policy::engine::{ConnRateLimiter, HotErrorMode, OnExceed, PolicyDecision};
use crate::realtime::core::Connection;
use crate::realtime::RealtimeCore;
use crate::realtime::RealtimeCtx;
use crate::transport::codec::{decode, Inbound};
use crate::transport::handshake::retry_after_header_secs;
use crate::obs::metrics::GatewayMetrics;

static NEXT_SID: AtomicU64 = AtomicU64::new(1);
static NEXT_TRACE: AtomicU64 = AtomicU64::new(1);

fn gen_sid() -> String { format!("{:x}", NEXT_SID.fetch_add(1, Ordering::Relaxed)) }
fn gen_trace() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let seq = NEXT_TRACE.fetch_add(1, Ordering::Relaxed);
    format!("{:x}-{:x}", now, seq)
}

/// WebSocket upgrade query parameters.
#[derive(Debug, Deserialize)]
pub struct WsQuery {
    pub tenant: String,
    pub ticket: String,
    /// Optional client-provided session id (tab/browser id). Generated if absent.
    #[serde(default)]
    pub sid: Option<String>,
}

/// Per-connection mutable state used inside the WS loop.
struct SessionState {
    active_room: Option<String>,
    last_activity: Instant,
    conn_limiter: Option<ConnRateLimiter>,
}

fn sys_authed_json(tenant: &str, user: &str, sid: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "authed", "data": { "tenant": tenant, "user": user, "sid": sid }, "trace_id": trace_id }).to_string()
}
fn sys_error_json(code: &str, msg: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "error", "data": { "code": code, "msg": msg }, "trace_id": trace_id }).to_string()
}
fn sys_kicked_json(reason: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "kicked", "data": { "reason": reason }, "trace_id": trace_id }).to_string()
}
fn sys_joined_json(room: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "joined", "room": room, "trace_id": trace_id }).to_string()
}
fn sys_left_json(trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "left", "trace_id": trace_id }).to_string()
}

/// RAII guard that tears down session and presence entries on exit.
struct SessionCleanup {
    core: Arc<RealtimeCore>, tenant_id: String, user_key: String, session_key: String, metrics: Arc<GatewayMetrics>,
}
impl Drop for SessionCleanup {
    fn drop(&mut self) {
        let _ = self.core.sessions.remove_session(&self.user_key, &self.session_key);
        self.core.presence.cleanup_session(&self.tenant_id, &self.user_key, &self.session_key);
        self.metrics.ws_active_sessions.dec(&[("tenant", &self.tenant_id)]);
        tracing::debug!(s=%self.session_key, "session raii cleanup done");
    }
}

pub async fn ws_upgrade(
    State(app): State<AppState>, ConnectInfo(addr): ConnectInfo<SocketAddr>, ws: WebSocketUpgrade, Query(q): Query<WsQuery>,
) -> impl IntoResponse {
    if let Err(wait_secs) = app.handshake().check(addr.ip()).await {
        app.metrics().handshake_rejections.inc(&[("tenant", &q.tenant), ("reason", "rate_limit")]);
        let (val, _) = retry_after_header_secs(wait_secs);
        let mut headers = HeaderMap::new();
        headers.insert(RETRY_AFTER, val.parse().unwrap());
        return (StatusCode::TOO_MANY_REQUESTS, headers, "Too Many Requests").into_response();
    }
    if app.is_draining() { return (StatusCode::SERVICE_UNAVAILABLE, "draining").into_response(); }
    if let Some(t_cfg) = app.cfg().tenants.iter().find(|t| t.id == q.tenant) {
        let limit = t_cfg.limits.max_sessions_total;
        if limit > 0 {
            let current = app.realtime().sessions.count_tenant_sessions(&q.tenant);
            if current >= limit {
                app.metrics().handshake_rejections.inc(&[("tenant", &q.tenant), ("reason", "tenant_capacity")]);
                let mut headers = HeaderMap::new();
                headers.insert(RETRY_AFTER, "1".parse().unwrap());
                return (StatusCode::SERVICE_UNAVAILABLE, headers, "Tenant Capacity Exceeded").into_response();
            }
        }
    } else { return (StatusCode::BAD_REQUEST, "Unknown Tenant").into_response(); }

    app.metrics().ws_upgrades.inc(&[("tenant", &q.tenant), ("status", "ok")]);
    ws.on_upgrade(move |socket| async move {
        if let Err(e) = run_session(app, q, socket).await { tracing::error!("session error: {}", e); }
    })
}

async fn run_session(app: AppState, q: WsQuery, socket: WebSocket) -> Result<()> {
    let policy = app.tenant_policy(&q.tenant).ok_or(WsPrismError::BadRequest("unknown tenant".into()))?;
    let user_id = app.resolve_ticket(&q.ticket)?;
    let sid = q.sid.unwrap_or_else(gen_sid);
    let trace_id = gen_trace();
    let core = app.realtime();
    let dispatcher = app.dispatcher();
    let metrics = app.metrics();
    let user_key = format!("{}::{}", q.tenant, user_id);
    let session_key = format!("{}::{}::{}", q.tenant, user_id, sid);
    let span = tracing::info_span!("ws", %trace_id, t=%q.tenant, u=%user_id, s=%sid);
    let _enter = span.enter();
    let (out_tx, mut out_rx) = mpsc::channel(1024);
    let (mut ws_tx, mut ws_rx) = socket.split();

    let sp = policy.session_policy();
    let max_user_sessions = sp.max_sessions_per_user as usize;
    let current_user_sessions = core.sessions.count_user_sessions(&user_key);
    if current_user_sessions >= max_user_sessions {
         metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "session"), ("decision", "reject"), ("reason", "max_user_sessions")]);
         match sp.on_exceed {
             OnExceed::Deny => {
                 let _ = out_tx.send(Message::Text(sys_error_json("TOO_MANY_SESSIONS", "limit exceeded", &trace_id))).await;
                 return Ok(());
             }
             OnExceed::KickOldest => {
                 if let Some((victim, victim_conn)) = core.sessions.evict_oldest(&user_key) {
                     let _ = victim_conn.tx.try_send(Message::Text(sys_kicked_json("max_sessions_exceeded", &trace_id)));
                     let _ = victim_conn.tx.try_send(Message::Close(Some(CloseFrame { code: 1008, reason: "kicked".into() })));
                     core.presence.cleanup_session(&q.tenant, &user_key, &victim);
                     metrics.ws_active_sessions.dec(&[("tenant", &q.tenant)]);
                 }
             }
         }
    }

    let t_cfg = app.cfg().tenants.iter().find(|t| t.id == q.tenant).unwrap();
    core.sessions.try_insert(q.tenant.clone(), user_key.clone(), session_key.clone(), Connection{ tx: out_tx.clone() }, t_cfg.limits.max_sessions_total)?;
    metrics.ws_active_sessions.inc(&[("tenant", &q.tenant)]);
    let _cleanup = SessionCleanup { core: core.clone(), tenant_id: q.tenant.clone(), user_key: user_key.clone(), session_key: session_key.clone(), metrics: metrics.clone() };
    out_tx.send(Message::Text(sys_authed_json(&q.tenant, &user_id, &sid, &trace_id))).await.map_err(|_| WsPrismError::Internal("closed".into()))?;

    let gw = &app.cfg().gateway;
    let mut ping_tick = tokio::time::interval(Duration::from_millis(gw.ping_interval_ms));
    let mut idle_tick = tokio::time::interval(Duration::from_millis(1000));
    let idle_timeout = Duration::from_millis(gw.idle_timeout_ms);
    let writer_timeout = Duration::from_millis(gw.writer_send_timeout_ms);
    let mut sess = SessionState { active_room: None, last_activity: Instant::now(), conn_limiter: policy.new_connection_limiter() };
    
    // Sampling Counter
    let mut hot_op_counter: u64 = 0;

    loop {
        tokio::select! {
            maybe_out = out_rx.recv() => {
                match maybe_out {
                    Some(m) => {
                        if timeout(writer_timeout, ws_tx.send(m)).await.is_err() {
                             metrics.writer_timeouts.inc(&[("tenant", &q.tenant)]);
                             break;
                        }
                    }
                    None => break,
                }
            }
            incoming = ws_rx.next() => {
                let Some(Ok(msg)) = incoming else { break; };
                sess.last_activity = Instant::now();
                let decoded = match decode(msg) {
                    Ok(d) => d,
                    Err(e) => {
                        metrics.decode_errors.inc(&[("tenant", &q.tenant)]);
                        let _ = out_tx.send(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string(), &trace_id))).await;
                        break;
                    }
                };
                match decoded {
                    Inbound::Ping(p) => { let _ = out_tx.send(Message::Pong(p)).await; },
                    Inbound::Pong(_) => {},
                    Inbound::Close => break,
                    Inbound::Text { env, bytes_len } => {
                        if let Some(lim) = sess.conn_limiter.as_mut() {
                            if !lim.allow() { 
                                metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "ext"), ("decision", "drop"), ("reason", "conn_rate_limit")]);
                                continue; 
                            }
                        }
                        match policy.check_text(bytes_len, &env.svc, &env.msg_type) {
                            PolicyDecision::Pass => {},
                            PolicyDecision::Drop => { 
                                metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "ext"), ("decision", "drop"), ("reason", "policy")]);
                                continue; 
                            },
                            PolicyDecision::Reject { code, msg } => {
                                // SAFE LABEL: code.as_str()
                                metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "ext"), ("decision", "reject"), ("reason", code.as_str())]);
                                let _ = out_tx.send(Message::Text(sys_error_json(code.as_str(), msg, &trace_id))).await;
                                continue;
                            },
                            PolicyDecision::Close { code, msg } => {
                                // SAFE LABEL: code.as_str()
                                metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "ext"), ("decision", "close"), ("reason", code.as_str())]);
                                let _ = out_tx.send(Message::Text(sys_error_json(code.as_str(), msg, &trace_id))).await;
                                break;
                            }
                        }
                        if env.svc == "room" && env.msg_type == "join" {
                            let room = env.room.clone().unwrap_or_else(|| "default".to_string());
                            let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), trace_id.clone(), sess.active_room.clone(), core.clone());
                            match ctx.join_room_with_limits(&room, &t_cfg.limits) {
                                Ok(_) => {
                                    sess.active_room = Some(room.clone());
                                    let _ = out_tx.send(Message::Text(sys_joined_json(&room, &trace_id))).await;
                                },
                                Err(e) => {
                                    metrics.service_errors.inc(&[("tenant", &q.tenant), ("svc", "room"), ("type", "join_failed")]);
                                    let _ = out_tx.send(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string(), &trace_id))).await;
                                }
                            }
                            continue;
                        }
                        if env.svc == "room" && env.msg_type == "leave" {
                            if let Some(room) = sess.active_room.take() {
                                let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), trace_id.clone(), None, core.clone());
                                ctx.leave_room(&room);
                            }
                            let _ = out_tx.send(Message::Text(sys_left_json(&trace_id))).await;
                            continue;
                        }
                        let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), trace_id.clone(), sess.active_room.clone(), core.clone());
                        let start = Instant::now();
                        let res = dispatcher.dispatch_text(ctx, env).await;
                        // Always measure Ext lane
                        metrics.dispatch_duration.observe(&[("tenant", &q.tenant), ("lane", "ext")], start.elapsed());
                        if let Err(e) = res {
                             metrics.service_errors.inc(&[("tenant", &q.tenant), ("lane", "ext")]);
                             let _ = out_tx.send(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string(), &trace_id))).await;
                        }
                    },
                    Inbound::Hot { frame, bytes_len } => {
                         match policy.check_hot(bytes_len, frame.svc_id, frame.opcode) {
                            PolicyDecision::Pass => {},
                            PolicyDecision::Drop => { 
                                metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "hot"), ("decision", "drop"), ("reason", "policy")]);
                                continue; 
                            },
                            PolicyDecision::Reject { code, msg } => {
                                metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "hot"), ("decision", "reject"), ("reason", code.as_str())]);
                                if let HotErrorMode::SysError = policy.hot_error_mode() {
                                    let _ = out_tx.send(Message::Text(sys_error_json(code.as_str(), msg, &trace_id))).await;
                                }
                                continue;
                            },
                            PolicyDecision::Close { code, msg } => {
                                metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "hot"), ("decision", "close"), ("reason", code.as_str())]);
                                if let HotErrorMode::SysError = policy.hot_error_mode() {
                                    let _ = out_tx.send(Message::Text(sys_error_json(code.as_str(), msg, &trace_id))).await;
                                }
                                break;
                            }
                         }
                         if policy.hot_requires_active_room() && sess.active_room.is_none() {
                             if let HotErrorMode::SysError = policy.hot_error_mode() {
                                 let _ = out_tx.send(Message::Text(sys_error_json("BAD_REQUEST", "no active room", &trace_id))).await;
                             }
                             continue;
                         }
                         let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), trace_id.clone(), sess.active_room.clone(), core.clone());
                         
                         // Hot Lane Sampling (1/1024)
                         hot_op_counter = hot_op_counter.wrapping_add(1);
                         let should_sample = (hot_op_counter & 1023) == 0;
                         let start = if should_sample { Some(Instant::now()) } else { None };
                         
                         let res = dispatcher.dispatch_hot(ctx, frame).await;
                         if let Some(s) = start {
                             metrics.dispatch_duration.observe(&[("tenant", &q.tenant), ("lane", "hot")], s.elapsed());
                         }
                         if let Err(e) = res {
                             metrics.service_errors.inc(&[("tenant", &q.tenant), ("lane", "hot")]);
                             if let HotErrorMode::SysError = policy.hot_error_mode() {
                                 let _ = out_tx.send(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string(), &trace_id))).await;
                             }
                         }
                    }
                }
            }
            _ = ping_tick.tick() => { let _ = out_tx.send(Message::Ping(Vec::new())).await; }
            _ = idle_tick.tick() => {
                if sess.last_activity.elapsed() >= idle_timeout {
                    let _ = out_tx.send(Message::Text(sys_error_json("TIMEOUT", "idle", &trace_id))).await;
                    break;
                }
            }
        }
    }
    Ok(())
}
