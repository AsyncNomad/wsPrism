use axum::extract::ws::Message;
use dashmap::{DashMap, DashSet};
use tokio::sync::mpsc;

use std::sync::atomic::{AtomicU64, Ordering};

/// One session's outbound queue sender.
/// One session's outbound queue sender.
#[derive(Clone)]
pub struct Connection {
    pub tx: mpsc::Sender<Message>,
}

#[derive(Clone)]
struct SessionEntry {
    conn: Connection,
    created_seq: u64,
}

/// Session registry:
/// - `session_key -> Connection`
/// - `user_key -> {session_key...}`
#[derive(Default)]
pub struct SessionRegistry {
    sessions: DashMap<String, SessionEntry>,
    user_index: DashMap<String, DashSet<String>>,
    seq: AtomicU64,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
            user_index: DashMap::new(),
            seq: AtomicU64::new(1),
        }
    }

    pub fn insert(&self, user_key: String, session_key: String, conn: Connection) {
        self.user_index
            .entry(user_key)
            .or_insert_with(DashSet::new)
            .insert(session_key.clone());

        let created_seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.sessions.insert(session_key, SessionEntry { conn, created_seq });
    }

    pub fn remove_session(&self, user_key: &str, session_key: &str) -> Option<Connection> {
        if let Some(set) = self.user_index.get(user_key) {
            set.remove(session_key);
            if set.is_empty() {
                drop(set);
                self.user_index.remove(user_key);
            }
        }
        self.sessions
            .remove(session_key)
            .map(|(_, entry)| entry.conn)
    }

    pub fn get_session(&self, session_key: &str) -> Option<Connection> {
        self.sessions.get(session_key).map(|r| r.value().conn.clone())
    }

    pub fn get_user_sessions(&self, user_key: &str) -> Vec<Connection> {
        let Some(set) = self.user_index.get(user_key) else { return vec![]; };
        set.iter()
            .filter_map(|sid| self.get_session(sid.key()))
            .collect()
    }

    pub fn count_user_sessions(&self, user_key: &str) -> usize {
        self.user_index.get(user_key).map(|s| s.len()).unwrap_or(0)
    }

    /// Evict the oldest session for this user.
    /// Returns (victim_session_key, victim_connection).
    pub fn evict_oldest(&self, user_key: &str) -> Option<(String, Connection)> {
        let set = self.user_index.get(user_key)?;
        let keys: Vec<String> = set.iter().map(|s| s.key().to_string()).collect();
        drop(set);

        let mut victim_key: Option<String> = None;
        let mut victim_seq: u64 = u64::MAX;

        for k in &keys {
            if let Some(e) = self.sessions.get(k) {
                if e.value().created_seq < victim_seq {
                    victim_seq = e.value().created_seq;
                    victim_key = Some(k.clone());
                }
            }
        }

        let victim_key = victim_key?;
        let conn = self.remove_session(user_key, &victim_key)?;
        Some((victim_key, conn))
    }
}
