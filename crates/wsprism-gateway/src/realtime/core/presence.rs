use dashmap::{DashMap, DashSet};

/// Room presence: room_key -> sessions, session_key -> rooms.
#[derive(Default)]
pub struct Presence {
    room_to_sessions: DashMap<String, DashSet<String>>,
    session_to_rooms: DashMap<String, DashSet<String>>,
}

impl Presence {
    pub fn new() -> Self {
        Self {
            room_to_sessions: DashMap::new(),
            session_to_rooms: DashMap::new(),
        }
    }

    pub fn join(&self, room_key: &str, session_key: &str) {
        self.room_to_sessions
            .entry(room_key.to_string())
            .or_insert_with(DashSet::new)
            .insert(session_key.to_string());

        self.session_to_rooms
            .entry(session_key.to_string())
            .or_insert_with(DashSet::new)
            .insert(room_key.to_string());
    }

    pub fn leave(&self, room_key: &str, session_key: &str) {
        if let Some(set) = self.room_to_sessions.get(room_key) {
            set.remove(session_key);
            if set.is_empty() {
                drop(set);
                self.room_to_sessions.remove(room_key);
            }
        }
        if let Some(set) = self.session_to_rooms.get(session_key) {
            set.remove(room_key);
            if set.is_empty() {
                drop(set);
                self.session_to_rooms.remove(session_key);
            }
        }
    }

    pub fn sessions_in(&self, room_key: &str) -> Vec<String> {
        self.room_to_sessions
            .get(room_key)
            .map(|set| set.iter().map(|u| u.key().to_string()).collect())
            .unwrap_or_default()
    }

    pub fn cleanup_session(&self, session_key: &str) {
        if let Some(rooms) = self.session_to_rooms.remove(session_key).map(|(_, v)| v) {
            for r in rooms.iter() {
                let room_key = r.key();
                if let Some(set) = self.room_to_sessions.get(room_key) {
                    set.remove(session_key);
                    if set.is_empty() {
                        drop(set);
                        self.room_to_sessions.remove(room_key);
                    }
                }
            }
        }
    }
}
