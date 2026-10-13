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
use crate::realtime::{close_code, CloseSpec};
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
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio::time::{timeout, Duration, Instant};
use tracing::Instrument;
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

/// How long to keep the socket open after sending close 1009 (see S2).
const CLOSE_LINGER: Duration = Duration::from_millis(500);

/// How long to wait for the peer's close frame after sending ours (C3).
const CLOSE_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(1);

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
fn sys_left_json(room: &str, trace_id: &str) -> String {
    json!({ "v": 1, "svc": "sys", "type": "left", "room": room, "trace_id": trace_id }).to_string()
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
    // C8: ask the victim's loop to stop and close itself (notice + 1008).
    let _ = conn.ctl.try_send(CloseSpec::new(
        close_code::POLICY,
        reason,
        Some(sys_kicked_json(reason, trace_id)),
    ));
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
            async move {
                if let Err(e) = run_session(app, q, user_id, sid, trace_id, socket).await {
                    tracing::error!("session error: {}", e);
                }
            }
            .instrument(span)
        })
}

/// Why a session's main loop ended, i.e. how its socket must be closed.
enum Exit {
    /// The peer closed (or the stream ended): just flush our close reply.
    Peer,
    /// The socket is unusable (write stalled or read failed): drop it.
    Abort,
    /// Server-initiated close with a reason, optionally preceded by a notice.
    Close(CloseSpec),
    /// Oversized frame (S2): close 1009, then linger (input can't be drained).
    TooBig,
}

/// Queue a reply from the session's own loop without awaiting (C9). This loop
/// is the queue's only consumer, so awaiting a full queue would freeze the
/// session forever. A full queue means the client can't keep up with its own
/// replies plus broadcasts, so the session is closed as a slow consumer.
fn queue_reply(
    out_tx: &mpsc::Sender<Message>,
    msg: Message,
    metrics: &GatewayMetrics,
    tenant: &str,
    trace_id: &str,
) -> Option<Exit> {
    match out_tx.try_send(msg) {
        Ok(()) => None,
        Err(TrySendError::Full(_)) => {
            metrics.policy_decisions.inc(&[
                ("tenant", tenant),
                ("lane", "transport"),
                ("decision", "close"),
                ("reason", "slow_consumer"),
            ]);
            Some(Exit::Close(CloseSpec::new(
                close_code::POLICY,
                "slow_consumer",
                Some(sys_error_json(
                    "SLOW_CONSUMER",
                    "outbound queue full",
                    trace_id,
                )),
            )))
        }
        Err(TrySendError::Closed(_)) => Some(Exit::Abort),
    }
}

/// Close politely (C3): flush what is already queued (so the notice is not
/// reordered ahead of earlier messages), send the notice and the close frame,
/// then wait briefly for the peer's close so the TCP teardown is clean.
/// Writes share one `writer_timeout` budget; a stalled peer is abandoned.
async fn close_session(
    ws_tx: &mut SplitSink<WebSocket, Message>,
    ws_rx: &mut SplitStream<WebSocket>,
    out_rx: &mut mpsc::Receiver<Message>,
    spec: CloseSpec,
    writer_timeout: Duration,
) {
    out_rx.close(); // no new messages from here on
    let mut outgoing = Vec::new();
    while let Ok(m) = out_rx.try_recv() {
        if !matches!(m, Message::Close(_)) {
            outgoing.push(m);
        }
    }
    outgoing.extend(spec.notice.map(Message::Text));
    outgoing.push(Message::Close(Some(CloseFrame {
        code: spec.code,
        reason: spec.reason.into(),
    })));

    let deadline = Instant::now() + writer_timeout;
    for m in outgoing {
        if !matches!(
            tokio::time::timeout_at(deadline, ws_tx.send(m)).await,
            Ok(Ok(()))
        ) {
            return; // stalled or broken peer: give up
        }
    }

    let _ = timeout(CLOSE_HANDSHAKE_TIMEOUT, async {
        while let Some(Ok(m)) = ws_rx.next().await {
            if matches!(m, Message::Close(_)) {
                break;
            }
        }
    })
    .await;
}

async fn run_session(
    app: AppState,
    q: WsQuery,
    user_id: String,
    sid: String,
    trace_id: String,
    socket: WebSocket,
) -> Result<()> {
    let policy = app
        .tenant_policy(&q.tenant)
        .ok_or(WsPrismError::BadRequest("unknown tenant".into()))?;
    let t_cfg = app
        .cfg()
        .tenants
        .iter()
        .find(|t| t.id == q.tenant)
        .ok_or_else(|| WsPrismError::BadRequest("unknown tenant".into()))?;
    let gw = &app.cfg().gateway;
    let writer_timeout = Duration::from_millis(gw.writer_send_timeout_ms);
    let core = app.realtime();
    let dispatcher = app.dispatcher();
    let metrics = app.metrics();
    let user_key = format!("{}::{}", q.tenant, user_id);
    // S4: the internal key is unique per connection. The client-facing `sid`
    // is client-controlled and may repeat (e.g. a reconnecting tab), so it must
    // never be the registry/presence key, or two connections would alias.
    let conn_id = NEXT_CONN.fetch_add(1, Ordering::Relaxed);
    let session_key = format!("{}::{}::{}#{:x}", q.tenant, user_id, sid, conn_id);
    let (out_tx, mut out_rx) = mpsc::channel(gw.session_queue_capacity);
    let (ctl_tx, mut ctl_rx) = mpsc::channel::<CloseSpec>(1);
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
                let spec = CloseSpec::new(
                    close_code::POLICY,
                    "too_many_sessions",
                    Some(sys_error_json(
                        "TOO_MANY_SESSIONS",
                        "limit exceeded",
                        &trace_id,
                    )),
                );
                close_session(&mut ws_tx, &mut ws_rx, &mut out_rx, spec, writer_timeout).await;
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

    if let Err(e) = core.sessions.try_insert(
        q.tenant.clone(),
        user_key.clone(),
        session_key.clone(),
        sid.clone(),
        Connection {
            tx: out_tx.clone(),
            ctl: ctl_tx,
        },
        t_cfg.limits.max_sessions_total,
    ) {
        let spec = CloseSpec::new(
            close_code::TRY_AGAIN_LATER,
            "capacity",
            Some(sys_error_json(
                e.client_code().as_str(),
                &e.to_string(),
                &trace_id,
            )),
        );
        close_session(&mut ws_tx, &mut ws_rx, &mut out_rx, spec, writer_timeout).await;
        return Ok(());
    }
    metrics.ws_active_sessions.inc(&[("tenant", &q.tenant)]);
    let _cleanup = SessionCleanup {
        core: core.clone(),
        tenant_id: q.tenant.clone(),
        user_key: user_key.clone(),
        session_key: session_key.clone(),
        metrics: metrics.clone(),
    };
    // The queue is empty and only this task consumes it, so this cannot be full.
    out_tx
        .try_send(Message::Text(sys_authed_json(
            &q.tenant, &user_id, &sid, &trace_id,
        )))
        .map_err(|_| WsPrismError::Internal("outbound queue unavailable".into()))?;

    let mut ping_tick = tokio::time::interval(Duration::from_millis(gw.ping_interval_ms));
    let mut idle_tick = tokio::time::interval(Duration::from_millis(1000));
    let idle_timeout = Duration::from_millis(gw.idle_timeout_ms);
    let mut sess = SessionState {
        active_room: None,
        last_activity: Instant::now(),
        conn_limiter: policy.new_connection_limiter(),
    };

    // Sampling Counter
    let mut hot_op_counter: u64 = 0;

    // Replies from inside the loop: never await our own queue (C9).
    macro_rules! reply {
        ($msg:expr) => {
            if let Some(exit) = queue_reply(&out_tx, $msg, &metrics, &q.tenant, &trace_id) {
                break exit;
            }
        };
    }

    let exit = loop {
        tokio::select! {
            // C8: kick / replace / drain requests from other tasks.
            Some(spec) = ctl_rx.recv() => break Exit::Close(spec),
            maybe_out = out_rx.recv() => {
                match maybe_out {
                    Some(m) => {
                        match timeout(writer_timeout, ws_tx.send(m)).await {
                            Ok(Ok(())) => {}
                            Ok(Err(_)) => break Exit::Abort,
                            Err(_) => {
                                metrics.writer_timeouts.inc(&[("tenant", &q.tenant)]);
                                break Exit::Abort;
                            }
                        }
                    }
                    None => break Exit::Abort,
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
                            break Exit::TooBig;
                        }
                        break Exit::Abort;
                    }
                    None => break Exit::Peer,
                };
                sess.last_activity = Instant::now();
                let decoded = match decode(msg) {
                    Ok(d) => d,
                    Err(e) => {
                        metrics.decode_errors.inc(&[("tenant", &q.tenant)]);
                        break Exit::Close(CloseSpec::new(
                            close_code::PROTOCOL_ERROR,
                            "decode_error",
                            Some(sys_error_json(e.client_code().as_str(), &e.to_string(), &trace_id)),
                        ));
                    }
                };
                match decoded {
                    // tungstenite answers pings itself; replying again would double-pong.
                    Inbound::Ping(_) => {},
                    Inbound::Pong(_) => {},
                    Inbound::Close => break Exit::Peer,
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
                                reply!(Message::Text(sys_error_json(code.as_str(), msg, &trace_id)));
                                continue;
                            },
                            PolicyDecision::Close { code, msg } => {
                                // SAFE LABEL: code.as_str()
                                metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "ext"), ("decision", "close"), ("reason", code.as_str())]);
                                break Exit::Close(CloseSpec::new(
                                    close_code::POLICY,
                                    "policy",
                                    Some(sys_error_json(code.as_str(), msg, &trace_id)),
                                ));
                            }
                        }
                        if env.svc == "room" && env.msg_type == "join" {
                            let room = env.room.clone().unwrap_or_else(|| "default".to_string());
                            let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), session_key.clone(), trace_id.clone(), sess.active_room.clone(), core.clone());
                            match ctx.join_room_with_limits(&room, &t_cfg.limits) {
                                Ok(_) => {
                                    sess.active_room = Some(room.clone());
                                    reply!(Message::Text(sys_joined_json(&room, &trace_id)));
                                },
                                Err(e) => {
                                    metrics.service_errors.inc(&[("tenant", &q.tenant), ("svc", "room"), ("type", "join_failed")]);
                                    reply!(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string(), &trace_id)));
                                }
                            }
                            continue;
                        }
                        if env.svc == "room" && env.msg_type == "leave" {
                            // C4: leave the named room; without one, the active room.
                            let target = env.room.clone().or_else(|| sess.active_room.clone());
                            let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), session_key.clone(), trace_id.clone(), None, core.clone());
                            match target {
                                Some(room) if ctx.is_in_room(&room) => {
                                    ctx.leave_room(&room);
                                    if sess.active_room.as_deref() == Some(room.as_str()) {
                                        sess.active_room = None;
                                    }
                                    reply!(Message::Text(sys_left_json(&room, &trace_id)));
                                }
                                _ => {
                                    reply!(Message::Text(sys_error_json("BAD_REQUEST", "not a member of that room", &trace_id)));
                                }
                            }
                            continue;
                        }
                        let ctx = RealtimeCtx::new(q.tenant.clone(), user_id.clone(), sid.clone(), session_key.clone(), trace_id.clone(), sess.active_room.clone(), core.clone());
                        let start = Instant::now();
                        let res = dispatcher.dispatch_text(ctx, env).await;
                        // Always measure Ext lane
                        metrics.dispatch_duration.observe(&[("tenant", &q.tenant), ("lane", "ext")], start.elapsed());
                        if let Err(e) = res {
                             metrics.service_errors.inc(&[("tenant", &q.tenant), ("lane", "ext")]);
                             reply!(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string(), &trace_id)));
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
                                    reply!(Message::Text(sys_error_json(code.as_str(), msg, &trace_id)));
                                }
                                continue;
                            },
                            PolicyDecision::Close { code, msg } => {
                                metrics.policy_decisions.inc(&[("tenant", &q.tenant), ("lane", "hot"), ("decision", "close"), ("reason", code.as_str())]);
                                let notice = match policy.hot_error_mode() {
                                    HotErrorMode::SysError => Some(sys_error_json(code.as_str(), msg, &trace_id)),
                                    HotErrorMode::Silent => None,
                                };
                                break Exit::Close(CloseSpec::new(close_code::POLICY, "policy", notice));
                            }
                         }
                         if policy.hot_requires_active_room() && sess.active_room.is_none() {
                             if let HotErrorMode::SysError = policy.hot_error_mode() {
                                 reply!(Message::Text(sys_error_json("BAD_REQUEST", "no active room", &trace_id)));
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
                                 reply!(Message::Text(sys_error_json(e.client_code().as_str(), &e.to_string(), &trace_id)));
                             }
                         }
                    }
                }
            }
            // Keep-alive ping is optional: skip it if the queue is full (C9).
            _ = ping_tick.tick() => { let _ = out_tx.try_send(Message::Ping(Vec::new())); }
            _ = idle_tick.tick() => {
                if sess.last_activity.elapsed() >= idle_timeout {
                    break Exit::Close(CloseSpec::new(
                        close_code::NORMAL,
                        "idle_timeout",
                        Some(sys_error_json("TIMEOUT", "idle", &trace_id)),
                    ));
                }
            }
        }
    };

    // The session stays registered until the close completes: the drain
    // logic in main waits on the registry, and anything routed to us meanwhile
    // is rejected by the closed queue. `_cleanup` unregisters on return.
    match exit {
        Exit::Peer => {
            // Flush tungstenite's queued close reply.
            let _ = timeout(writer_timeout, ws_tx.close()).await;
        }
        Exit::Abort => {}
        Exit::Close(spec) => {
            close_session(&mut ws_tx, &mut ws_rx, &mut out_rx, spec, writer_timeout).await;
        }
        Exit::TooBig => {
            let close = Message::Close(Some(CloseFrame {
                code: close_code::MESSAGE_TOO_BIG,
                reason: "message too big".into(),
            }));
            let _ = timeout(writer_timeout, ws_tx.send(close)).await;
            // The oversized payload is still unread in the kernel buffer
            // (tungstenite rejects on the header and cannot skip the body).
            // Closing now would make the kernel send RST, and the peer may
            // discard our close frame. Linger briefly so it can read it first.
            tokio::time::sleep(CLOSE_LINGER).await;
        }
    }
    Ok(())
}
