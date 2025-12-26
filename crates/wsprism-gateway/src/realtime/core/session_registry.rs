use axum::extract::ws::Message;
use dashmap::DashMap;
use tokio::sync::mpsc;

/// One user's outbound queue sender.
#[derive(Clone)]
pub struct Connection {
    pub tx: mpsc::Sender<Message>,
}

/// user_id -> Connection
#[derive(Default)]
pub struct SessionRegistry {
    conns: DashMap<String, Connection>,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self { conns: DashMap::new() }
    }

    pub fn insert(&self, user_id: String, conn: Connection) {
        self.conns.insert(user_id, conn);
    }

    pub fn remove(&self, user_id: &str) {
        self.conns.remove(user_id);
    }

    pub fn get(&self, user_id: &str) -> Option<Connection> {
        self.conns.get(user_id).map(|r| r.value().clone())
    }
}
