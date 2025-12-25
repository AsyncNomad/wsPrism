use std::sync::Arc;

use crate::{
    error::Result,
    realtime::protocol::{envelope::Outgoing, types::{RoomId, SessionId, UserId}},
};

use super::session::RealtimeCore;

#[derive(Clone)]
pub struct RealtimeCtx {
    pub session_id: SessionId,
    pub user_id: UserId,
    core: Arc<RealtimeCore>,
}

impl RealtimeCtx {
    pub fn new(session_id: SessionId, user_id: UserId, core: Arc<RealtimeCore>) -> Self {
        Self { session_id, user_id, core }
    }

    pub async fn send_to_user(&self, user: &UserId, msg: Outgoing) -> Result<()> {
        self.core.send_to_user(user, msg).await
    }

    pub fn publish_room_lossy(&self, room: &RoomId, msg: Outgoing) -> Result<()> {
        self.core.publish_room_lossy(room, msg)
    }

    pub async fn publish_room_reliable(&self, room: &RoomId, msg: Outgoing) -> Result<()> {
        self.core.publish_room_reliable(room, msg).await
    }

    pub fn join_room(&self, room: &RoomId) { self.core.join_room(&self.user_id, room); }
    pub fn leave_room(&self, room: &RoomId) { self.core.leave_room(&self.user_id, room); }
}
