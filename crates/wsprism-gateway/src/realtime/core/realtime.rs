//! Realtime egress engine and per-message context.
//!
//! The realtime core fans out messages to users, sessions, or rooms with
//! lossy/reliable QoS and keeps lightweight counters for observability. It is
//! intentionally lock-free; small temporary overshoots are possible under high
//! contention to preserve throughput.

use crate::config::schema::TenantLimits;
use crate::realtime::core::{Connection, Presence, SessionRegistry};
use crate::realtime::types::{close_code, CloseSpec, Outgoing, PreparedMsg, QoS};
use axum::extract::ws::Message;
use serde_json::json;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::error::TrySendError;
use wsprism_core::error::{Result, WsPrismError};

static DROP_COUNT: AtomicU64 = AtomicU64::new(0);
static SLOW_CONSUMER_COUNT: AtomicU64 = AtomicU64::new(0);

/// Lossy messages dropped because the recipient's queue was full.
pub fn egress_drop_count() -> u64 {
    DROP_COUNT.load(Ordering::Relaxed)
}
/// Recipients closed because a reliable message didn't fit their queue.
pub fn egress_slow_consumer_count() -> u64 {
    SLOW_CONSUMER_COUNT.load(Ordering::Relaxed)
}
fn sample_every_1024(n: u64) -> bool {
    (n & 1023) == 1
}

pub struct RealtimeCore {
    pub sessions: Arc<SessionRegistry>,
    pub presence: Arc<Presence>,
}

impl Default for RealtimeCore {
    fn default() -> Self {
        Self::new()
    }
}

impl RealtimeCore {
    /// Create a new realtime core with fresh registries.
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(SessionRegistry::new()),
            presence: Arc::new(Presence::new()),
        }
    }

    /// Ask every session to close with 1001 during draining (best-effort).
    pub fn best_effort_shutdown_all(&self, reason: &str) {
        let sessions = self.sessions.all_sessions();
        for (session_key, conn) in sessions {
            let spec = CloseSpec::new(close_code::GOING_AWAY, reason, None);
            if conn.ctl.try_send(spec).is_err() {
                let n = DROP_COUNT.fetch_add(1, Ordering::Relaxed);
                if sample_every_1024(n) {
                    tracing::warn!(%session_key, drops=%n, "close request dropped while draining");
                }
            }
        }
    }

    /// Queue one message for one session according to `qos`. Never awaits,
    /// so a slow recipient can never stall the publisher (C1):
    /// - `Lossy`: if the queue is full, drop it for this recipient.
    /// - `Reliable`: if the queue is full, close the recipient as a slow
    ///   consumer so it resyncs on reconnect, instead of silently missing it.
    fn deliver(&self, session_key: &str, conn: &Connection, msg: Message, qos: &QoS) {
        match conn.tx.try_send(msg) {
            Ok(()) | Err(TrySendError::Closed(_)) => {} // closed = session ending
            Err(TrySendError::Full(_)) => match qos {
                QoS::Lossy => {
                    let n = DROP_COUNT.fetch_add(1, Ordering::Relaxed);
                    if sample_every_1024(n) {
                        tracing::warn!(%session_key, drops=%n, "lossy egress drop");
                    }
                }
                QoS::Reliable => {
                    let n = SLOW_CONSUMER_COUNT.fetch_add(1, Ordering::Relaxed);
                    if sample_every_1024(n) {
                        tracing::warn!(%session_key, closes=%n, "reliable egress: closing slow consumer");
                    }
                    let notice = json!({
                        "v": 1, "svc": "sys", "type": "error",
                        "data": { "code": "SLOW_CONSUMER", "msg": "outbound queue full" }
                    });
                    let _ = conn.ctl.try_send(CloseSpec::new(
                        close_code::POLICY,
                        "slow_consumer",
                        Some(notice.to_string()),
                    ));
                }
            },
        }
    }

    /// Send to every session of a user (honours `out.qos`).
    pub fn send_to_user(&self, user_key: &str, out: Outgoing) -> Result<()> {
        let keys = self.sessions.user_session_keys(user_key);
        if keys.is_empty() {
            return Err(WsPrismError::BadRequest("user not connected".into()));
        }
        let prepared = PreparedMsg::prepare(&out)?;
        for key in keys {
            if let Some(conn) = self.sessions.get_session(&key) {
                self.deliver(&key, &conn, prepared.to_ws_message(), &out.qos);
            }
        }
        Ok(())
    }

    /// Send to a single session (honours `out.qos`).
    pub fn send_to_session(&self, session_key: &str, out: Outgoing) -> Result<()> {
        let conn = self
            .sessions
            .get_session(session_key)
            .ok_or_else(|| WsPrismError::BadRequest("session not connected".into()))?;
        let prepared = PreparedMsg::prepare(&out)?;
        self.deliver(session_key, &conn, prepared.to_ws_message(), &out.qos);
        Ok(())
    }

    /// Publish to every session in a room (honours `out.qos`).
    pub fn publish_room(&self, room_key: &str, out: Outgoing) -> Result<()> {
        let prepared = PreparedMsg::prepare(&out)?;
        for key in self.presence.sessions_in(room_key) {
            if let Some(conn) = self.sessions.get_session(&key) {
                self.deliver(&key, &conn, prepared.to_ws_message(), &out.qos);
            }
        }
        Ok(())
    }

    /// Publish to a room with `QoS::Lossy`, whatever `out.qos` says.
    pub fn publish_room_lossy(&self, room_key: &str, out: Outgoing) -> Result<()> {
        self.publish_room(
            room_key,
            Outgoing {
                qos: QoS::Lossy,
                ..out
            },
        )
    }

    /// Publish to a room with `QoS::Reliable`, whatever `out.qos` says.
    pub fn publish_room_reliable(&self, room_key: &str, out: Outgoing) -> Result<()> {
        self.publish_room(
            room_key,
            Outgoing {
                qos: QoS::Reliable,
                ..out
            },
        )
    }
}

/// A session's view of the realtime core, handed to services.
///
/// One context lives for the whole session (owned by the session task) and
/// is lent to services as `&mut RealtimeCtx`, so services can update
/// session-scoped state such as the active room without locks, and nothing
/// is re-allocated per message. Clone it to use it from a spawned task.
#[derive(Clone)]
pub struct RealtimeCtx {
    tenant: Arc<str>,
    user: Arc<str>,
    user_key: Arc<str>,
    session_id: Arc<str>,
    session_key: Arc<str>,
    pub trace_id: Arc<str>,
    active_room: Option<Arc<str>>,
    limits: Arc<TenantLimits>,
    core: Arc<RealtimeCore>,
}

impl RealtimeCtx {
    /// Construct the context for one session.
    ///
    /// `session_key` is the gateway-internal, per-connection unique key (see
    /// S4 in docs/HARDENING.md); `session_id` is the client-facing sid.
    pub fn new(
        tenant: impl Into<Arc<str>>,
        user: impl Into<Arc<str>>,
        session_id: impl Into<Arc<str>>,
        session_key: impl Into<Arc<str>>,
        trace_id: impl Into<Arc<str>>,
        limits: Arc<TenantLimits>,
        core: Arc<RealtimeCore>,
    ) -> Self {
        let tenant = tenant.into();
        let user = user.into();
        let user_key: Arc<str> = Arc::from(format!("{}::{}", tenant, user));

        Self {
            tenant,
            user,
            user_key,
            session_id: session_id.into(),
            session_key: session_key.into(),
            trace_id: trace_id.into(),
            active_room: None,
            limits,
            core,
        }
    }

    pub fn tenant(&self) -> &str {
        &self.tenant
    }
    pub fn user(&self) -> &str {
        &self.user
    }
    pub fn user_key(&self) -> &str {
        &self.user_key
    }
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    pub fn session_key(&self) -> &str {
        &self.session_key
    }
    /// The session's active room: the Hot Lane target (see PROTOCOL.md).
    pub fn active_room(&self) -> Option<&str> {
        self.active_room.as_deref()
    }
    pub fn set_active_room(&mut self, room: Option<&str>) {
        self.active_room = room.map(Arc::from);
    }

    fn room_key(&self, room: &str) -> String {
        format!("{}::{}", self.tenant(), room)
    }

    /// Join `room`, enforcing the tenant's room limits.
    pub fn join_room(&self, room: &str) -> Result<()> {
        let rk = self.room_key(room);
        self.core.presence.try_join(
            self.tenant(),
            &rk,
            self.user_key(),
            self.session_key(),
            &self.limits,
        )
    }

    /// True if this session has joined `room`.
    pub fn is_in_room(&self, room: &str) -> bool {
        let rk = self.room_key(room);
        self.core.presence.is_member(&rk, self.session_key())
    }

    pub fn leave_room(&self, room: &str) {
        let rk = self.room_key(room);
        self.core
            .presence
            .leave(self.tenant(), &rk, self.user_key(), self.session_key());
    }

    pub fn send_to_user(&self, out: Outgoing) -> Result<()> {
        self.core.send_to_user(self.user_key(), out)
    }
    pub fn send_to_session(&self, out: Outgoing) -> Result<()> {
        self.core.send_to_session(self.session_key(), out)
    }
    pub fn publish_room_lossy(&self, room: &str, out: Outgoing) -> Result<()> {
        let rk = self.room_key(room);
        self.core.publish_room_lossy(&rk, out)
    }
    pub fn publish_room_reliable(&self, room: &str, out: Outgoing) -> Result<()> {
        let rk = self.room_key(room);
        self.core.publish_room_reliable(&rk, out)
    }
}
