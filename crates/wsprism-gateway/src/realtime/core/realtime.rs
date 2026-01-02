//! Realtime egress engine and per-message context.
//!
//! Provides helpers to send to a single session, all sessions of a user, or to
//! broadcast to a room with lossy/reliable QoS semantics.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use tokio::time::{timeout, Duration};

use wsprism_core::error::{Result, WsPrismError};

use crate::realtime::core::{Presence, SessionRegistry};
use crate::realtime::types::{Outgoing, PreparedMsg, QoS};

static DROP_COUNT: AtomicU64 = AtomicU64::new(0);
static SEND_FAIL_COUNT: AtomicU64 = AtomicU64::new(0);

fn sample_every_1024(n: u64) -> bool {
    (n & 1023) == 1
}

/// RealtimeCore: egress engine (send to user / session / room).
pub struct RealtimeCore {
    pub sessions: Arc<SessionRegistry>,
    pub presence: Arc<Presence>,
}

impl RealtimeCore {
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(SessionRegistry::new()),
            presence: Arc::new(Presence::new()),
        }
    }

    /// Send to all sessions of a user_key (tenant-scoped).
    pub fn send_to_user(&self, user_key: &str, out: Outgoing) -> Result<()> {
        let conns = self.sessions.get_user_sessions(user_key);
        if conns.is_empty() {
            return Err(WsPrismError::BadRequest("user not connected".into()));
        }

        let prepared = PreparedMsg::prepare(&out)?;
        for c in conns {
            if c.tx.try_send(prepared.to_ws_message()).is_err() {
                let n = DROP_COUNT.fetch_add(1, Ordering::Relaxed);
                if sample_every_1024(n) {
                    tracing::warn!(user_key=%user_key, drops=%n, "egress drop (queue full)");
                }
            }
        }
        Ok(())
    }

    pub fn send_to_session(&self, session_key: &str, out: Outgoing) -> Result<()> {
        let conn = self
            .sessions
            .get_session(session_key)
            .ok_or_else(|| WsPrismError::BadRequest("session not connected".into()))?;

        let prepared = PreparedMsg::prepare(&out)?;
        let _ = conn.tx.try_send(prepared.to_ws_message());
        Ok(())
    }


    /// Lossy broadcast: try_send only, drop if queue is full.
    pub fn publish_room_lossy(&self, room_key: &str, out: Outgoing) -> Result<()> {
        let prepared = PreparedMsg::prepare(&out)?;
        let sessions = self.presence.sessions_in(room_key);
        for sid in sessions {
            if let Some(conn) = self.sessions.get_session(&sid) {
                if conn.tx.try_send(prepared.to_ws_message()).is_err() {
                    let n = DROP_COUNT.fetch_add(1, Ordering::Relaxed);
                    if sample_every_1024(n) {
                        tracing::warn!(room_key=%room_key, drops=%n, "lossy drop (queue full)");
                    }
                }
            }
        }
        Ok(())
    }

    /// Reliable broadcast: send concurrently with optional timeout per session.
    pub async fn publish_room_reliable(&self, room_key: &str, out: Outgoing) -> Result<()> {
        let prepared = PreparedMsg::prepare(&out)?;
        let sessions = self.presence.sessions_in(room_key);

        let (timeout_ms, do_timeout) = match out.qos {
            QoS::Reliable { timeout_ms } => (timeout_ms, timeout_ms > 0),
            _ => (0, false),
        };

        let mut futs = FuturesUnordered::new();
        for sid in sessions {
            if let Some(conn) = self.sessions.get_session(&sid) {
                let msg = prepared.to_ws_message();
                futs.push(async move {
                    if do_timeout {
                        let res = timeout(Duration::from_millis(timeout_ms), conn.tx.send(msg)).await;
                        if res.is_err() {
                            let n = SEND_FAIL_COUNT.fetch_add(1, Ordering::Relaxed);
                            if sample_every_1024(n) {
                                tracing::warn!(fails=%n, "reliable send timeout");
                            }
                        }
                    } else if conn.tx.send(msg).await.is_err() {
                        let n = SEND_FAIL_COUNT.fetch_add(1, Ordering::Relaxed);
                        if sample_every_1024(n) {
                            tracing::warn!(fails=%n, "reliable send failed (channel closed)");
                        }
                    }
                });
            }
        }

        while futs.next().await.is_some() {}
        Ok(())
    }
}

/// Per-message context passed to services (borrow tools instead of owning).
#[derive(Clone)]
pub struct RealtimeCtx {
    tenant: Arc<str>,
    user: Arc<str>,
    user_key: Arc<str>,
    session_id: Arc<str>,
    session_key: Arc<str>,
    active_room: Option<Arc<str>>,
    core: Arc<RealtimeCore>,
}

impl RealtimeCtx {
    pub fn new(
        tenant: impl Into<Arc<str>>,
        user: impl Into<Arc<str>>,
        session_id: impl Into<Arc<str>>,
        active_room: Option<String>,
        core: Arc<RealtimeCore>,
    ) -> Self {
        let tenant = tenant.into();
        let user = user.into();
        let session_id = session_id.into();

        let user_key: Arc<str> = Arc::<str>::from(format!("{}::{}", tenant, user));
        let session_key: Arc<str> = Arc::<str>::from(format!("{}::{}::{}", tenant, user, session_id));

        Self {
            tenant,
            user,
            user_key,
            session_id,
            session_key,
            active_room: active_room.map(|s| Arc::<str>::from(s)),
            core,
        }
    }

    pub fn tenant(&self) -> &str { &self.tenant }
    pub fn user(&self) -> &str { &self.user }

    /// tenant-scoped key ("tenant::user")
    pub fn user_key(&self) -> &str { &self.user_key }

    pub fn session_id(&self) -> &str { &self.session_id }

    /// tenant-scoped key ("tenant::user::sid")
    pub fn session_key(&self) -> &str { &self.session_key }

    pub fn active_room(&self) -> Option<&str> { self.active_room.as_deref() }

    fn room_key(&self, room: &str) -> String {
        format!("{}::{}", self.tenant(), room)
    }

    pub fn join_room(&self, room: &str) {
        let rk = self.room_key(room);
        self.core.presence.join(&rk, self.session_key());
    }

    pub fn leave_room(&self, room: &str) {
        let rk = self.room_key(room);
        self.core.presence.leave(&rk, self.session_key());
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

    pub async fn publish_room_reliable(&self, room: &str, out: Outgoing) -> Result<()> {
        let rk = self.room_key(room);
        self.core.publish_room_reliable(&rk, out).await
    }
}
