use std::sync::Arc;

use axum::extract::ws::Message;
use futures_util::stream::FuturesUnordered;
use futures_util::StreamExt;
use tokio::time::{timeout, Duration};

use wsprism_core::error::{Result, WsPrismError};

use crate::realtime::types::{Outgoing, PreparedMsg, QoS};
use crate::realtime::core::{Presence, SessionRegistry};

/// RealtimeCore: egress engine (send to user / publish to room).
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

    pub fn send_to_user(&self, user: &str, out: Outgoing) -> Result<()> {
        let conn = self
            .sessions
            .get(user)
            .ok_or_else(|| WsPrismError::BadRequest("user not connected".into()))?;
        let prepared = PreparedMsg::prepare(&out)?;
        // default strategy: reliable uses async path (handled in publish helpers)
        conn.tx.try_send(prepared.to_ws_message()).ok();
        Ok(())
    }

    /// Lossy broadcast: try_send only, drop if queue is full.
    pub fn publish_room_lossy(&self, room: &str, out: Outgoing) -> Result<()> {
        let prepared = PreparedMsg::prepare(&out)?;
        let users = self.presence.users_in(room);
        for u in users {
            if let Some(conn) = self.sessions.get(&u) {
                let _ = conn.tx.try_send(prepared.to_ws_message());
            }
        }
        Ok(())
    }

    /// Reliable broadcast: send concurrently with optional timeout per user.
    pub async fn publish_room_reliable(&self, room: &str, out: Outgoing) -> Result<()> {
        let prepared = PreparedMsg::prepare(&out)?;
        let users = self.presence.users_in(room);

        let (timeout_ms, do_timeout) = match out.qos {
            QoS::Reliable { timeout_ms } => (timeout_ms, timeout_ms > 0),
            _ => (0, false),
        };

        let mut futs = FuturesUnordered::new();
        for u in users {
            if let Some(conn) = self.sessions.get(&u) {
                let msg = prepared.to_ws_message();
                futs.push(async move {
                    if do_timeout {
                        let _ = timeout(Duration::from_millis(timeout_ms), conn.tx.send(msg)).await;
                    } else {
                        let _ = conn.tx.send(msg).await;
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
    active_room: Option<Arc<str>>,
    core: Arc<RealtimeCore>,
}

impl RealtimeCtx {
    pub fn new(tenant: impl Into<Arc<str>>, user: impl Into<Arc<str>>, active_room: Option<String>, core: Arc<RealtimeCore>) -> Self {
        Self {
            tenant: tenant.into(),
            user: user.into(),
            active_room: active_room.map(|s| Arc::<str>::from(s)),
            core,
        }
    }

    pub fn tenant(&self) -> &str { &self.tenant }
    pub fn user(&self) -> &str { &self.user }
    pub fn active_room(&self) -> Option<&str> { self.active_room.as_deref() }
    pub fn core(&self) -> &RealtimeCore { &self.core }

    pub fn join_room(&self, room: &str) {
        self.core.presence.join(room, self.user());
    }

    pub fn leave_room(&self, room: &str) {
        self.core.presence.leave(room, self.user());
    }

    pub fn publish_room_lossy(&self, room: &str, out: crate::realtime::types::Outgoing) -> Result<()> {
        self.core.publish_room_lossy(room, out)
    }

    pub async fn publish_room_reliable(&self, room: &str, out: crate::realtime::types::Outgoing) -> Result<()> {
        self.core.publish_room_reliable(room, out).await
    }
}
