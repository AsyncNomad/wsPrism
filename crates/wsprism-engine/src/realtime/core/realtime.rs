//! Realtime egress engine and per-message context.
//!
//! The realtime core fans out messages to users, sessions, or rooms with
//! lossy/reliable QoS and keeps lightweight counters for observability. It is
//! intentionally lock-free; small temporary overshoots are possible under high
//! contention to preserve throughput.

use crate::realtime::core::{
    now_ms, CloseRequest, Connection, Joined, Member, Presence, Put, SessionRegistry,
};
use crate::realtime::types::{close_code, CloseSpec, OutFrame, Outgoing, Payload, QoS};
use crate::sys;
use crate::RoomLimits;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc::error::TrySendError;
use wsprism_protocol::error::ClientCode;
use wsprism_protocol::error::{Result, WsPrismError};
use wsprism_protocol::protocol::hot::HotFrame;
use wsprism_protocol::protocol::ProtocolVersion;

use crate::permissions::Permissions;

static DROP_COUNT: AtomicU64 = AtomicU64::new(0);
static SLOT_OVERWRITE_COUNT: AtomicU64 = AtomicU64::new(0);
static SLOT_FULL_COUNT: AtomicU64 = AtomicU64::new(0);

/// Keyed Hot frames replaced in a recipient's slot before being sent
/// (normal: latest state wins; high values mean readers lag the update rate).
pub fn hot_slot_overwrite_count() -> u64 {
    SLOT_OVERWRITE_COUNT.load(Ordering::Relaxed)
}
/// Keyed Hot frames dropped because a recipient's slot was at capacity.
pub fn hot_slot_full_count() -> u64 {
    SLOT_FULL_COUNT.load(Ordering::Relaxed)
}
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

/// Shared realtime state: every session and every room of every tenant.
pub struct RealtimeCore {
    /// Connected sessions.
    pub sessions: Arc<SessionRegistry>,
    /// Room membership.
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

    /// Ask every session to close with 1001 during draining. A session that
    /// is already closing keeps its first reason.
    pub fn best_effort_shutdown_all(&self, reason: &str) {
        self.sessions.for_each_session(|conn| {
            let spec = CloseSpec::new(close_code::GOING_AWAY, reason, None);
            conn.signals.request_close(CloseRequest::Spec(spec));
        });
    }

    /// One sweeper pass over every session (performance-architecture §5):
    /// sessions idle for `idle_timeout_ms` are asked to close; sessions due a
    /// keep-alive get a Ping, skipped if their queue is full (C9). Replaces
    /// two timers per session.
    pub fn sweep(&self, idle_timeout_ms: u64, ping_interval_ms: u64) {
        let now = now_ms();
        self.sessions.for_each_session(|conn| {
            if conn.signals.expired(now) {
                conn.signals
                    .request_close(CloseRequest::Spec(CloseSpec::new(
                        close_code::POLICY,
                        "auth_expired",
                        Some(sys::kicked("auth_expired", "").to_string()),
                    )));
            } else if conn.signals.idle_ms(now) >= idle_timeout_ms {
                conn.signals.request_close(CloseRequest::Idle);
            } else if conn.signals.take_ping_due(now, ping_interval_ms) {
                let _ = conn.tx.try_send(OutFrame::Ping);
            }
        });
    }

    /// Queue one message for one session according to `qos`. Never awaits,
    /// so a slow recipient can never stall the publisher (C1):
    /// - `Lossy`: if the queue is full, drop it for this recipient.
    /// - `Reliable`: if the queue is full, close the recipient as a slow
    ///   consumer so it resyncs on reconnect, instead of silently missing it.
    fn deliver(&self, session_key: &str, conn: &Connection, frame: OutFrame, qos: &QoS) {
        // Keyed state (latest value wins) goes through the recipient's Hot
        // slot, so a lagging reader never receives a backlog of stale values.
        if let (OutFrame::Hot(h), QoS::Lossy) = (&frame, qos) {
            if let Some(key) = h.slot_key {
                match conn.signals.hot.put(key, Arc::clone(h)) {
                    Put::Inserted => {}
                    Put::Overwritten => {
                        SLOT_OVERWRITE_COUNT.fetch_add(1, Ordering::Relaxed);
                    }
                    Put::Full => {
                        SLOT_FULL_COUNT.fetch_add(1, Ordering::Relaxed);
                    }
                }
                return;
            }
        }
        match conn.tx.try_send(frame) {
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
                    let notice =
                        sys::error(ClientCode::SlowConsumer, "outbound queue full", "").to_string();
                    conn.signals
                        .request_close(CloseRequest::Spec(CloseSpec::new(
                            close_code::POLICY,
                            "slow_consumer",
                            Some(notice),
                        )));
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
        let frame = OutFrame::from_payload(&out.payload)?;
        for key in keys {
            if let Some(conn) = self.sessions.get_session(&key) {
                self.deliver(&key, &conn, frame.clone(), &out.qos);
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
        let frame = OutFrame::from_payload(&out.payload)?;
        self.deliver(session_key, &conn, frame, &out.qos);
        Ok(())
    }

    /// The internal key of `room` in `tenant` (shared by sessions and the
    /// server API so both address the same room).
    pub fn room_key(tenant: &str, room: &str) -> String {
        format!("{tenant}::{room}")
    }

    /// Publish to every session in a room (honours `out.qos`); returns how
    /// many sessions it was delivered to. Members carry
    /// their delivery handles, so this is one shard read lock per publish and
    /// a `try_send` per recipient — no lookup, key copy or handle clone.
    pub fn publish_room(&self, room_key: &str, out: Outgoing) -> Result<usize> {
        let frame = OutFrame::from_payload(&out.payload)?;
        let mut n = 0;
        self.presence.for_each_member(room_key, |m| {
            self.deliver(&m.key, &m.conn, frame.clone(), &out.qos);
            n += 1;
        });
        Ok(n)
    }

    /// Publish to a room with `QoS::Lossy`, whatever `out.qos` says.
    pub fn publish_room_lossy(&self, room_key: &str, out: Outgoing) -> Result<usize> {
        self.publish_room(
            room_key,
            Outgoing {
                qos: QoS::Lossy,
                ..out
            },
        )
    }

    /// Publish to a room with `QoS::Reliable`, whatever `out.qos` says.
    pub fn publish_room_reliable(&self, room_key: &str, out: Outgoing) -> Result<usize> {
        self.publish_room(
            room_key,
            Outgoing {
                qos: QoS::Reliable,
                ..out
            },
        )
    }
}

/// An answer to one request, sent later from another task (Reliable, with
/// `reply_to` set like a direct reply). Dropping it without sending frees
/// the slot too; a session that ended in the meantime just ignores it.
pub struct DeferredReply {
    core: Arc<RealtimeCore>,
    conn: Connection,
    session_key: Arc<str>,
    seq: Option<u64>,
}

impl DeferredReply {
    /// Send `msg` as the answer (Reliable; `reply_to` added).
    pub fn send(self, msg: serde_json::Value) {
        let frame = OutFrame::text(sys::reply_to(msg, self.seq).to_string());
        self.core
            .deliver(&self.session_key, &self.conn, frame, &QoS::Reliable);
    }
}

impl Drop for DeferredReply {
    fn drop(&mut self) {
        self.conn.signals.end_deferred();
    }
}

fn check_publish(permissions: &Permissions, room: &str) -> Result<()> {
    if permissions.can_publish(room) {
        Ok(())
    } else {
        Err(WsPrismError::NotAllowed(
            "not allowed to publish to that room".into(),
        ))
    }
}

/// Sends on behalf of one session from any task (see
/// [`RealtimeCtx::publisher`]). Cheap to clone.
#[derive(Clone)]
pub struct Publisher {
    core: Arc<RealtimeCore>,
    tenant: Arc<str>,
    user_key: Arc<str>,
    permissions: Arc<Permissions>,
}

impl Publisher {
    /// Publish to `room`, dropped for recipients whose queue is full. Requires publish permission.
    pub fn publish_room_lossy(&self, room: &str, out: Outgoing) -> Result<usize> {
        check_publish(&self.permissions, room)?;
        self.core
            .publish_room_lossy(&RealtimeCore::room_key(&self.tenant, room), out)
    }

    /// Publish to `room`; a recipient whose queue is full is closed as a slow consumer. Requires publish permission.
    pub fn publish_room_reliable(&self, room: &str, out: Outgoing) -> Result<usize> {
        check_publish(&self.permissions, room)?;
        self.core
            .publish_room_reliable(&RealtimeCore::room_key(&self.tenant, room), out)
    }

    /// Send to every session of this user.
    pub fn send_to_user(&self, out: Outgoing) -> Result<()> {
        self.core.send_to_user(&self.user_key, out)
    }
}

/// A session's view of the realtime core, handed to services.
///
/// One context lives for the whole session (owned by the session task) and
/// is lent to services as `&mut RealtimeCtx`, so services can update
/// session-scoped state such as the active room without locks, and nothing
/// is re-allocated per message.
///
/// Not `Clone`: room membership must only change on the session's own task
/// (presence relies on it). From a spawned task, answer with
/// [`RealtimeCtx::defer_reply`] and send with [`RealtimeCtx::publisher`].
pub struct RealtimeCtx {
    tenant: Arc<str>,
    user: Arc<str>,
    user_key: Arc<str>,
    session_id: Arc<str>,
    session_key: Arc<str>,
    trace_id: Arc<str>,
    active_room: Option<Arc<str>>,
    /// `seq` of the request being handled, for reply correlation.
    request_seq: Option<u64>,
    limits: Arc<RoomLimits>,
    /// This session's own delivery handles (room membership, replies).
    conn: Connection,
    /// Wire protocol of this session (reply shapes, Hot routing).
    protocol: ProtocolVersion,
    /// What this user may do with rooms (ADR 0008 §2).
    permissions: Permissions,
    /// Rooms this session has joined, with their ids (small; linear scan).
    joined: Vec<(Arc<str>, Joined)>,
    core: Arc<RealtimeCore>,
}

/// Who a session is, fixed for its lifetime (input to [`RealtimeCtx::new`]).
pub struct CtxIdentity {
    /// Tenant id.
    pub tenant: Arc<str>,
    /// User id.
    pub user: Arc<str>,
    /// Client-facing sid (may repeat across connections).
    pub session_id: Arc<str>,
    /// Gateway-internal, per-connection unique key (see S4 in docs/HARDENING.md).
    pub session_key: Arc<str>,
    /// Trace id (logs, `sys.*` messages).
    pub trace_id: Arc<str>,
    /// Negotiated wire protocol.
    pub protocol: ProtocolVersion,
    /// Room permissions from the authenticator.
    pub permissions: Permissions,
}

impl RealtimeCtx {
    /// Construct the context for one session.
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn new(
        id: CtxIdentity,
        limits: Arc<RoomLimits>,
        conn: Connection,
        core: Arc<RealtimeCore>,
    ) -> Self {
        let user_key: Arc<str> = Arc::from(format!("{}::{}", id.tenant, id.user));
        Self {
            tenant: id.tenant,
            user: id.user,
            user_key,
            session_id: id.session_id,
            session_key: id.session_key,
            trace_id: id.trace_id,
            active_room: None,
            request_seq: None,
            limits,
            conn,
            protocol: id.protocol,
            permissions: id.permissions,
            joined: Vec::new(),
            core,
        }
    }

    /// Trace id of this session (also in its `sys.*` messages).
    pub fn trace_id(&self) -> &str {
        &self.trace_id
    }

    /// Tenant this session belongs to.
    pub fn tenant(&self) -> &str {
        &self.tenant
    }
    /// Authenticated user id.
    pub fn user(&self) -> &str {
        &self.user
    }
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn user_key(&self) -> &str {
        &self.user_key
    }
    /// Client-facing session id (`sid`; may repeat across connections).
    pub fn session_id(&self) -> &str {
        &self.session_id
    }
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn session_key(&self) -> &str {
        &self.session_key
    }
    /// The session's active room: the Hot Lane target (see PROTOCOL.md).
    pub fn active_room(&self) -> Option<&str> {
        self.active_room.as_deref()
    }
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn set_active_room(&mut self, room: Option<&str>) {
        self.active_room = room.map(Arc::from);
    }

    fn room_key(&self, room: &str) -> String {
        RealtimeCore::room_key(self.tenant(), room)
    }

    /// `seq` of the request currently being handled, if the client sent one.
    pub fn request_seq(&self) -> Option<u64> {
        self.request_seq
    }

    /// Set by the session around each inbound request.
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn begin_request(&mut self, seq: Option<u64>) {
        self.request_seq = seq;
    }

    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn end_request(&mut self) {
        self.request_seq = None;
    }

    /// Answer the current request on this session (Reliable). Adds
    /// `reply_to: <seq>` when the request carried a `seq`, so clients can
    /// match responses to pipelined requests.
    pub fn reply(&self, msg: serde_json::Value) -> Result<()> {
        self.send_to_session(Outgoing {
            qos: QoS::Reliable,
            payload: Payload::TextJson(sys::reply_to(msg, self.request_seq)),
        })
    }

    /// Wire protocol this session negotiated (v1 / v2).
    pub fn protocol(&self) -> ProtocolVersion {
        self.protocol
    }

    /// A handle that answers the current request later, from another task
    /// (e.g. after a backend call), so the session loop never waits for it.
    /// `None` if this session already has `max_outstanding` such answers
    /// pending.
    pub fn defer_reply(&self, max_outstanding: u32) -> Option<DeferredReply> {
        self.conn
            .signals
            .try_begin_deferred(max_outstanding)
            .then(|| DeferredReply {
                core: Arc::clone(&self.core),
                conn: self.conn.clone(),
                session_key: Arc::clone(&self.session_key),
                seq: self.request_seq,
            })
    }

    /// A handle for sending from another task (a timer, a backend call):
    /// room publishes and sends to this user, with the same permission
    /// checks, but no membership changes.
    pub fn publisher(&self) -> Publisher {
        Publisher {
            core: Arc::clone(&self.core),
            tenant: Arc::clone(&self.tenant),
            user_key: Arc::clone(&self.user_key),
            permissions: Arc::new(self.permissions.clone()),
        }
    }

    /// Every publish through the context is checked against the user's
    /// publish permission, whichever service does it.
    fn check_publish(&self, room: &str) -> Result<()> {
        check_publish(&self.permissions, room)
    }

    /// Join `room`, enforcing the tenant's room limits. Returns the room id
    /// and this membership's id (protocol v2 `chan` / `sender`).
    pub fn join_room(&mut self, room: &str) -> Result<Joined> {
        crate::limits::validate_room_name(room)?;
        if !self.permissions.can_subscribe(room) {
            return Err(WsPrismError::NotAllowed(
                "not allowed to join that room".into(),
            ));
        }
        let rk = self.room_key(room);
        let member = Member {
            key: Arc::clone(&self.session_key),
            conn: self.conn.clone(),
            member_id: 0,
        };
        let joined = self.core.presence.try_join(
            self.tenant(),
            &rk,
            self.user_key(),
            member,
            &self.limits,
        )?;
        if !self.joined.iter().any(|(r, _)| &**r == room) {
            self.joined.push((Arc::from(room), joined));
        }
        Ok(joined)
    }

    /// The room a Hot frame targets and this session's membership in it:
    /// v2 names the room id (`chan`) and must be a room this session joined
    /// (ADR 0006 R5, R7); v1 targets the active room.
    /// SPI (unstable): for the session runtime, not services.
    #[doc(hidden)]
    pub fn hot_target(&self, frame: &HotFrame) -> Option<(&str, Joined)> {
        let found = match frame.chan {
            Some(chan) => self.joined.iter().find(|(_, j)| j.room_id == chan),
            None => {
                let active = self.active_room()?;
                self.joined.iter().find(|(r, _)| &**r == active)
            }
        };
        found.map(|(r, j)| (&**r, *j))
    }

    /// True if this session has joined `room`.
    pub fn is_in_room(&self, room: &str) -> bool {
        let rk = self.room_key(room);
        self.core.presence.is_member(&rk, self.session_key())
    }

    /// Leave `room` (no-op if not joined).
    pub fn leave_room(&mut self, room: &str) {
        if let Some((_, j)) = self.joined.iter().find(|(r, _)| &**r == room) {
            // Nothing from the room may reach the client after its sys.left.
            self.conn.signals.hot.purge_room(j.room_id);
        }
        self.joined.retain(|(r, _)| &**r != room);
        let rk = self.room_key(room);
        self.core
            .presence
            .leave(self.tenant(), &rk, self.user_key(), self.session_key());
    }

    /// Send to every session of this user (Reliable or Lossy per `out.qos`).
    pub fn send_to_user(&self, out: Outgoing) -> Result<()> {
        self.core.send_to_user(self.user_key(), out)
    }
    /// Send to this session, through its own handles (no registry lookup).
    pub fn send_to_session(&self, out: Outgoing) -> Result<()> {
        let frame = OutFrame::from_payload(&out.payload)?;
        self.core
            .deliver(self.session_key(), &self.conn, frame, &out.qos);
        Ok(())
    }
    /// Publish to `room`, dropped for recipients whose queue is full. Requires publish permission.
    pub fn publish_room_lossy(&self, room: &str, out: Outgoing) -> Result<()> {
        self.check_publish(room)?;
        let rk = self.room_key(room);
        self.core.publish_room_lossy(&rk, out).map(|_| ())
    }
    /// Publish to `room`; a recipient whose queue is full is closed as a slow consumer. Requires publish permission.
    pub fn publish_room_reliable(&self, room: &str, out: Outgoing) -> Result<()> {
        self.check_publish(room)?;
        let rk = self.room_key(room);
        self.core.publish_room_reliable(&rk, out).map(|_| ())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::realtime::core::SessionSignals;
    use tokio::sync::mpsc;

    fn ctx(
        core: &Arc<RealtimeCore>,
        permissions: Permissions,
    ) -> (RealtimeCtx, mpsc::Receiver<OutFrame>) {
        let (tx, rx) = mpsc::channel(8);
        let conn = Connection {
            tx,
            signals: SessionSignals::new(),
        };
        let id = CtxIdentity {
            tenant: "t".into(),
            user: "u".into(),
            session_id: "s".into(),
            session_key: "t::u::s#1".into(),
            trace_id: "tr".into(),
            protocol: ProtocolVersion::V2,
            permissions,
        };
        (
            RealtimeCtx::new(id, Arc::default(), conn, Arc::clone(core)),
            rx,
        )
    }

    fn text(s: &str) -> Outgoing {
        Outgoing {
            qos: QoS::Reliable,
            payload: Payload::TextJson(serde_json::json!(s)),
        }
    }

    #[tokio::test]
    async fn leaving_a_room_drops_its_pending_state() {
        let core = Arc::new(RealtimeCore::new());
        let (mut c, _rx) = ctx(&core, Permissions::default());
        let j = c.join_room("arena").unwrap();
        let hot = Arc::new(crate::realtime::types::HotOut {
            v1: bytes::Bytes::from_static(b"x"),
            v2: bytes::Bytes::from_static(b"x"),
            slot_key: Some((j.room_id, 7, 1, 0)),
        });
        c.publish_room_lossy(
            "arena",
            Outgoing {
                qos: QoS::Lossy,
                payload: Payload::Hot(hot),
            },
        )
        .unwrap();
        c.leave_room("arena");
        let mut out = Vec::new();
        c.conn.signals.hot.drain_into(&mut out);
        assert!(out.is_empty(), "no state from a left room");
    }

    #[tokio::test]
    async fn publisher_sends_from_another_task_with_the_same_permission_checks() {
        let core = Arc::new(RealtimeCore::new());
        let (mut c, mut rx) = ctx(&core, Permissions::restricted(&["*"], &["open"]));
        c.join_room("open").unwrap();
        c.join_room("closed").unwrap();
        let p = c.publisher();
        let n = tokio::spawn(async move {
            assert!(
                p.publish_room_reliable("closed", text("x")).is_err(),
                "no publish permission"
            );
            p.publish_room_reliable("open", text("hi")).unwrap()
        })
        .await
        .unwrap();
        assert_eq!(n, 1);
        assert_eq!(rx.recv().await, Some(OutFrame::text("\"hi\"".into())));
    }
}
