use axum::extract::ws::Message;
use dashmap::{DashMap, DashSet};
use tokio::sync::mpsc;

use std::sync::atomic::{AtomicU64, Ordering};
use wsprism_core::error::{Result, WsPrismError};

/// One session's outbound queue sender.
#[derive(Clone)]
pub struct Connection {
    pub tx: mpsc::Sender<Message>,
}

#[derive(Clone)]
struct SessionEntry {
    conn: Connection,
    created_seq: u64,
    // Sprint 5: store tenant here to facilitate cleanup without looking up other maps
    tenant_id: String,
}

/// Session registry:
/// - `session_key -> Connection`
/// - `user_key -> {session_key...}`
/// - `tenant_id -> count` (Atomic)
#[derive(Default)]
pub struct SessionRegistry {
    sessions: DashMap<String, SessionEntry>,
    user_index: DashMap<String, DashSet<String>>,
    // Sprint 5: O(1) Tenant Counter
    tenant_counts: DashMap<String, AtomicU64>,
    seq: AtomicU64,
}

impl SessionRegistry {
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
            user_index: DashMap::new(),
            tenant_counts: DashMap::new(),
            seq: AtomicU64::new(1),
        }
    }

    // Sprint 5: try_insert with limits enforcement
    /// Insert a session while enforcing a tenant-wide cap (best-effort).
    ///
    /// Concurrency note: For throughput, this uses lock-free atomics plus an
    /// optimistic increment/check pattern. Under extreme contention, a small
    /// temporary overshoot of `max_total` is possible before the counter is
    /// corrected. This is an intentional trade-off to avoid global locks.
    pub fn try_insert(
        &self,
        tenant_id: String,
        user_key: String,
        session_key: String,
        conn: Connection,
        max_total: u64
    ) -> Result<()> {
        let counter = self.tenant_counts.entry(tenant_id.clone()).or_insert_with(|| AtomicU64::new(0));

        // Strict enforcement
        if max_total > 0 {
            let current = counter.load(Ordering::Relaxed);
            if current >= max_total {
                 return Err(WsPrismError::ResourceExhausted("tenant session limit reached".into()));
            }
        }

        // Optimistic increment
        counter.fetch_add(1, Ordering::Relaxed);

        // Check again (race condition mitigation) - Optional but safer
        if max_total > 0 {
            if counter.load(Ordering::Relaxed) > max_total {
                counter.fetch_sub(1, Ordering::Relaxed);
                return Err(WsPrismError::ResourceExhausted("tenant session limit reached (race)".into()));
            }
        }

        self.user_index
            .entry(user_key)
            .or_insert_with(DashSet::new)
            .insert(session_key.clone());

        let created_seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.sessions.insert(session_key, SessionEntry { conn, created_seq, tenant_id });

        Ok(())
    }

    pub fn remove_session(&self, user_key: &str, session_key: &str) -> Option<Connection> {
        if let Some(set) = self.user_index.get(user_key) {
            set.remove(session_key);
            if set.is_empty() {
                drop(set);
                self.user_index.remove(user_key);
            }
        }

        if let Some((_, entry)) = self.sessions.remove(session_key) {
            // Sprint 5: Decrement tenant counter
            if let Some(counter) = self.tenant_counts.get(&entry.tenant_id) {
                counter.fetch_sub(1, Ordering::Relaxed);
            }
            Some(entry.conn)
        } else {
            None
        }
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

    // Sprint 5: O(1) Tenant Counter Lookup
    pub fn count_tenant_sessions(&self, tenant_id: &str) -> u64 {
        self.tenant_counts.get(tenant_id).map(|c| c.load(Ordering::Relaxed)).unwrap_or(0)
    }

    /// Snapshot of all active sessions.
    ///
    /// Returns a vector of (session_key, Connection). Intended for best-effort
    /// shutdown/draining logic.
    pub fn all_sessions(&self) -> Vec<(String, Connection)> {
        self.sessions
            .iter()
            .map(|r| (r.key().clone(), r.value().conn.clone()))
            .collect()
    }

    pub fn len_sessions(&self) -> usize {
        self.sessions.len()
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
