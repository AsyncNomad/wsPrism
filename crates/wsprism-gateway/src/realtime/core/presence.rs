use dashmap::{DashMap, DashSet};

/// Room presence: room -> users, user -> rooms.
#[derive(Default)]
pub struct Presence {
    room_to_users: DashMap<String, DashSet<String>>,
    user_to_rooms: DashMap<String, DashSet<String>>,
}

impl Presence {
    pub fn new() -> Self {
        Self {
            room_to_users: DashMap::new(),
            user_to_rooms: DashMap::new(),
        }
    }

    pub fn join(&self, room: &str, user: &str) {
        self.room_to_users
            .entry(room.to_string())
            .or_insert_with(DashSet::new)
            .insert(user.to_string());

        self.user_to_rooms
            .entry(user.to_string())
            .or_insert_with(DashSet::new)
            .insert(room.to_string());
    }

    pub fn leave(&self, room: &str, user: &str) {
        if let Some(set) = self.room_to_users.get(room) {
            set.remove(user);
        }
        if let Some(set) = self.user_to_rooms.get(user) {
            set.remove(room);
        }
    }

    pub fn users_in(&self, room: &str) -> Vec<String> {
        self.room_to_users
            .get(room)
            .map(|set| set.iter().map(|u| u.to_string()).collect())
            .unwrap_or_default()
    }

    pub fn cleanup_user(&self, user: &str) {
        if let Some(rooms) = self.user_to_rooms.remove(user).map(|(_, v)| v) {
            for r in rooms.iter() {
                if let Some(set) = self.room_to_users.get(r.key()) {
                    set.remove(user);
                }
            }
        }
    }
}
