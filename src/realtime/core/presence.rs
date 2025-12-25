use dashmap::{DashMap, DashSet};
use crate::realtime::protocol::types::{RoomId, UserId};

#[derive(Default)]
pub struct Presence {
    room_users: DashMap<RoomId, DashSet<UserId>>,
    user_rooms: DashMap<UserId, DashSet<RoomId>>,
}

impl Presence {
    pub fn join(&self, user: &UserId, room: &RoomId) {
        self.room_users.entry(room.clone()).or_default().insert(user.clone());
        self.user_rooms.entry(user.clone()).or_default().insert(room.clone());
    }

    pub fn leave(&self, user: &UserId, room: &RoomId) {
        if let Some(set) = self.room_users.get(room) { set.remove(user); }
        if let Some(set) = self.user_rooms.get(user) { set.remove(room); }
    }

    pub fn rooms_of(&self, user: &UserId) -> Vec<RoomId> {
        self.user_rooms.get(user).map(|s| s.iter().map(|r| r.clone()).collect()).unwrap_or_default()
    }

    pub fn users_in(&self, room: &RoomId) -> Vec<UserId> {
        self.room_users.get(room).map(|s| s.iter().map(|u| u.clone()).collect()).unwrap_or_default()
    }

    pub fn cleanup_user(&self, user: &UserId) -> Vec<RoomId> {
        let rooms = self.rooms_of(user);
        for r in &rooms { self.leave(user, r); }
        rooms
    }
}
