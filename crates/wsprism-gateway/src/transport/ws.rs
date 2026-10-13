//! WebSocket handler (transport + lifecycle).
//!
//! - Pre-upgrade defenses: IP handshake limiter (429) and tenant capacity (503)
//! - Trace ID generation/propagation into spans and sys.* messages
//! - Session/room governance and policy enforcement
//! - Labeled metrics for policy decisions/errors + sampled Hot Lane latency

use crate::app_state::AppState;
use crate::obs::metrics::GatewayMetrics;
use crate::policy::engine::{ConnRateLimiter, HotErrorMode, OnExceed, PolicyDecision};
use crate::realtime::core::Connection;
use crate::realtime::RealtimeCore;
use crate::realtime::RealtimeCtx;
use crate::transport::codec::{decode, Inbound};
use crate::transport::handshake::retry_after_header;
use axum::{
    extract::{
        connect_info::ConnectInfo, ws::CloseFrame, ws::Message, ws::WebSocket,
        ws::WebSocketUpgrade, Query, State,
    },
    http::{header::RETRY_AFTER, HeaderMap, StatusCode},
    response::IntoResponse,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc;
use tokio::time::{timeout, Duration, Instant};
use wsprism_core::error::{Result, WsPrismError};

static NEXT_SID: AtomicU64 = AtomicU64::new(1);
static NEXT_TRACE: AtomicU64 = AtomicU64::new(1);
/// Per-connection counter making internal session keys unique (S4).
static NEXT_CONN: AtomicU64 = AtomicU64::new(1);

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

/// RFC 6455 close code 1009: message too big to process.
const CLOSE_MESSAGE_TOO_BIG: u16 = 1009;

/// How long to keep the socket open after sending close 1009 (see S2).
const CLOSE_LINGER: Duration = Duration::from_millis(500);

/// True if the read failed because the frame/message exceeded the configured
/// codec limit (see `max_message_size` in `ws_upgrade`).
fn is_message_too_long(e: axum::Error) -> bool {
    matches!(
        e.into_inner().downcast_ref::<tungstenite::Error>(),
        Some(tungstenite::Error::Capacity(
            tungstenite::error::CapacityError::MessageTooLong { .. }
        ))
    )
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
    json!({ "v": 1, "svc": "sys", "type": "joined", "room": room, "trace_id": trace_id })
        .to_string()
}
fn sys_left_json(trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "left", "trace_id": trace_id }).to_string()
}

/// RAII guard that tears down session and presence entries on exit.
struct SessionCleanup {
    core: Arc<RealtimeCore>,
    tenant_id: String,
    user_key: String,
    session_key: String,
    metrics: Arc<GatewayMetrics>,
}
impl Drop for SessionCleanup {
    fn drop(&mut self) {
        // C2: whoever removes the registry entry owns the gauge decrement.
        // If this session was already evicted (kick), the evictor did it.
        let removed = self
            .core
            .sessions
            .remove_session(&self.user_key, &self.session_key)
            .is_some();
        // Presence cleanup is unconditional: an evicted session may have
        // re-joined rooms before its loop ended.
        self.core
            .presence
            .cleanup_session(&self.tenant_id, &self.user_key, &self.session_key);
        if removed {
            self.metrics
                .ws_active_sessions
                .dec(&[("tenant", &self.tenant_id)]);
        }
        tracing::debug!(s=%self.session_key, "session raii cleanup done");
    }
}

/// Evict a live session: unregister it (owning the gauge decrement, see C2),
/// clear its presence, then tell it why and close it.
fn evict_session(
    core: &RealtimeCore,
    metrics: &GatewayMetrics,
    tenant: &str,
    user_key: &str,
    victim_key: &str,
    reason: &str,
    trace_id: &str,
) {
    let Some(conn) = core.sessions.remove_session(user_key, victim_key) else {
        return; // already gone (raced with its own cleanup or another evictor)
    };
    core.presence.cleanup_session(tenant, user_key, victim_key);
    metrics.ws_active_sessions.dec(&[("tenant", tenant)]);
    let _ = conn
        .tx
        .try_send(Message::Text(sys_kicked_json(reason, trace_id)));
    let _ = conn.tx.try_send(Message::Close(Some(CloseFrame {
        code: 1008,
        reason: reason.to_owned().into(),
    })));
}

pub async fn ws_upgrade(
    State(app): State<AppState>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    ws: WebSocketUpgrade,
    Query(q): Query<WsQuery>,
) -> impl IntoResponse {
    let t_cfg = app.cfg().tenants.iter().find(|t| t.id == q.tenant);
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
        .on_upgrade(move |socket| async move {
            if let Err(e) = run_session(app, q, socket).await {
                tracing::error!("session error: {}", e);
            }
        })
}

async fn run_session(app: AppState, q: WsQuery, socket: WebSocket) -> Result<()> {
    let policy = app
        .tenant_policy(&q.tenant)
        .ok_or(WsPrismError::BadRequest("unknown tenant".into()))?;
    let user_id = app.resolve_ticket(&q.ticket)?;
    let sid = q.sid.unwrap_or_else(gen_sid);
    let trace_id = gen_trace();
    let core = app.realtime();
    let dispatcher = app.dispatcher();
    let metrics = app.metrics();
    let user_key = format!("{}::{}", q.tenant, user_id);
    // S4: the internal key is unique per connection. The client-facing `sid`
    // is client-controlled and may repeat (e.g. a reconnecting tab), so it must
    // never be the registry/presence key, or two connections would alias.
    let conn_id = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    let session_key = format!("{}::{}::{}#{:x}", q.tenant, user_id, sid, conn_id);
    let span = tracing::info_span!("ws", %trace_id, t=%q.tenant, u=%user_id, s=%sid);
    let _enter = span.enter();
    let (out_tx, mut out_rx) = mpsc::channel(1024);
    let (mut ws_tx, mut ws_rx) = socket.split();

    // S4: a reconnect with an in-use sid takes over: the old connection is
    // evicted before the session limit is evaluated.
    if let Some(prev) = core.sessions.find_session_by_sid(&user_key, &sid) {
        evict_session(
            &core, &metrics, &q.tenant, &user_key, &prev, "replaced", &trace_id,
        );
    }

    let sp = policy.session_policy();
    let max_user_sessions = sp.max_sessions_per_user as usize;
    let current_user_sessions = core.sessions.count_user_sessions(&user_key);
    if current_user_sessions >= max_user_sessions {
        metrics.policy_decisions.inc(&[
            ("tenant", &q.tenant),
            ("lane", "session"),
            ("decision", "reject"),
            ("reason", "max_user_sessions"),
        ]);
        match sp.on_exceed {
            OnExceed::Deny => {
                let _ = out_tx
                    .send(Message::Text(sys_error_json(
                        "TOO_MANY_SESSIONS",
                        "limit exceeded",
                        &trace_id,
                    )))
                    .await;
                return Ok(());
            }
            OnExceed::KickOldest => {
                if let Some(victim) = core.sessions.oldest_session(&user_key) {
                    evict_session(
                        &core,
                        &metrics,
                        &q.tenant,
                        &user_key,
                        &victim,
                        "max_sessions_exceeded",
                        &trace_id,
                    );
                }
            }
        }
    }

    let t_cfg = app
        .cfg()
        .tenants
        .iter()
        .find(|t| t.id == q.tenant)
        .ok_or_else(|| WsPrismError::BadRequest("unknown tenant".into()))?;
    core.sessions.try_insert(
        q.tenant.clone(),
        user_key.clone(),
        session_key.clone(),
        sid.clone(),
        Connection { tx: out_tx.clone() },
        t_cfg.limits.max_sessions_total,
    )?;
    metrics.ws_active_sessions.inc(&[("tenant", &q.tenant)]);
    let _cleanup = SessionCleanup {
        core: core.clone(),
        tenant_id: q.tenant.clone(),
        user_key: user_key.clone(),
        session_key: session_key.clone(),
        metrics: metrics.clone(),
    };
    out_tx
        .send(Message::Text(sys_authed_json(
            &q.tenant, &user_id, &sid, &trace_id,
        )))
        .await
        .map_err(|_| WsPrismError::Internal("closed".into()))?;

    let gw = &app.cfg().gateway;
    let mut ping_tick = tokio::time::interval(Duration::from_millis(gw.ping_interval_ms));
    let mut idle_tick = tokio::time::interval(Duration::from_millis(1000));
    let idle_timeout = Duration::from_millis(gw.idle_timeout_ms);
    let writer_timeout = Duration::from_millis(gw.writer_send_timeout_ms);
    let mut sess = SessionState {
        active_room: None,
        last_activity: Instant::now(),
        conn_limiter: policy.new_connection_limiter(),
    };

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
                let msg = match incoming {
                    Some(Ok(msg)) => msg,
                    Some(Err(e)) => {
                        if is_message_too_long(e) {
                            metrics.policy_decisions.inc(&[
                                ("tenant", &q.tenant),
                                ("lane", "transport"),
                                ("decision", "close"),
                                ("reason", "frame_too_large"),
                            ]);
                            let close = Message::Close(Some(CloseFrame {
                                code: CLOSE_MESSAGE_TOO_BIG,
                                reason: "message too big".into(),
                            }));
                            let _ = timeout(writer_timeout, ws_tx.send(close)).await;
                            // The oversized payload is still unread in the kernel
                            // buffer (tungstenite rejects on the header and cannot
                            // skip the body). Closing now would make the kernel send
                            // RST, and the peer may discard our close frame. Linger
                            // briefly so the client can read it first.
                            tokio::time::sleep(CLOSE_LINGER).await;
                        }
                        break;
                    }
                    None => break,
                };
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
                            let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), session_key.clone(), trace_id.clone(), sess.active_room.clone(), core.clone());
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
                                let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), session_key.clone(), trace_id.clone(), None, core.clone());
                                ctx.leave_room(&room);
                            }
                            let _ = out_tx.send(Message::Text(sys_left_json(&trace_id))).await;
                            continue;
                        }
                        let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), session_key.clone(), trace_id.clone(), sess.active_room.clone(), core.clone());
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
                        // S1: same per-connection budget as the Ext lane.
                        if let Some(lim) = sess.conn_limiter.as_mut() {
                            if !lim.allow() {
                                metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "hot"), ("decision", "drop"), ("reason", "conn_rate_limit")]);
                                continue;
                            }
                        }
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
                         let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), session_key.clone(), trace_id.clone(), sess.active_room.clone(), core.clone());

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
