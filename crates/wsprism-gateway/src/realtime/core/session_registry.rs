use axum::extract::ws::Message;
use dashmap::{DashMap, DashSet};
use tokio::sync::mpsc;

use std::sync::atomic::{AtomicU64, Ordering};
use wsprism_core::error::{Result, WsPrismError};

use crate::realtime::types::CloseSpec;

/// Handles to one live session.
#[derive(Clone)]
pub struct Connection {
    /// Outbound message queue (drained by the session's writer loop).
    pub tx: mpsc::Sender<Message>,
    /// Control channel: asks the session to close itself (kick, replace,
    /// drain). The session ends promptly even if its peer never answers (C8).
    pub ctl: mpsc::Sender<CloseSpec>,
}

#[derive(Clone)]
struct SessionEntry {
    conn: Connection,
    created_seq: u64,
    // Sprint 5: store tenant here to facilitate cleanup without looking up other maps
    tenant_id: String,
    /// Client-facing session id (may be client-provided; not unique).
    sid: String,
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
        sid: String,
        conn: Connection,
        max_total: u64,
    ) -> Result<()> {
        let counter = self
            .tenant_counts
            .entry(tenant_id.clone())
            .or_insert_with(|| AtomicU64::new(0));

        // Strict enforcement
        if max_total > 0 {
            let current = counter.load(Ordering::Relaxed);
            if current >= max_total {
                return Err(WsPrismError::ResourceExhausted(
                    "tenant session limit reached".into(),
                ));
            }
        }

        // Optimistic increment
        counter.fetch_add(1, Ordering::Relaxed);

        // Check again (race condition mitigation) - Optional but safer
        if max_total > 0 && counter.load(Ordering::Relaxed) > max_total {
            counter.fetch_sub(1, Ordering::Relaxed);
            return Err(WsPrismError::ResourceExhausted(
                "tenant session limit reached (race)".into(),
            ));
        }

        self.user_index
            .entry(user_key)
            .or_default()
            .insert(session_key.clone());

        let created_seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.sessions.insert(
            session_key,
            SessionEntry {
                conn,
                created_seq,
                tenant_id,
                sid,
            },
        );

        Ok(())
    }

    pub fn remove_session(&self, user_key: &str, session_key: &str) -> Option<Connection> {
        if let Some(set) = self.user_index.get(user_key) {
            set.remove(session_key);
        }
        // C5: drop the user's set only if it is still empty under the shard
        // lock; inserts go through `entry` and hold the same lock.
        self.user_index.remove_if(user_key, |_, set| set.is_empty());

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
        self.sessions
            .get(session_key)
            .map(|r| r.value().conn.clone())
    }

    /// Session keys of a user's live sessions (snapshot).
    pub fn user_session_keys(&self, user_key: &str) -> Vec<String> {
        self.user_index
            .get(user_key)
            .map(|set| set.iter().map(|k| k.key().clone()).collect())
            .unwrap_or_default()
    }

    pub fn count_user_sessions(&self, user_key: &str) -> usize {
        self.user_index.get(user_key).map(|s| s.len()).unwrap_or(0)
    }

    // Sprint 5: O(1) Tenant Counter Lookup
    pub fn count_tenant_sessions(&self, tenant_id: &str) -> u64 {
        self.tenant_counts
            .get(tenant_id)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
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

    /// Session key of this user's oldest live session.
    pub fn oldest_session(&self, user_key: &str) -> Option<String> {
        self.user_sessions_where(user_key, |_| true)
            .into_iter()
            .min_by_key(|(_, seq)| *seq)
            .map(|(k, _)| k)
    }

    /// Session key of this user's live session using client-facing `sid`.
    pub fn find_session_by_sid(&self, user_key: &str, sid: &str) -> Option<String> {
        self.user_sessions_where(user_key, |e| e.sid == sid)
            .into_iter()
            .next()
            .map(|(k, _)| k)
    }

    /// (session_key, created_seq) of the user's sessions matching `pred`.
    fn user_sessions_where(
        &self,
        user_key: &str,
        pred: impl Fn(&SessionEntry) -> bool,
    ) -> Vec<(String, u64)> {
        // Snapshot keys first so no user_index guard is held while reading `sessions`.
        let keys: Vec<String> = match self.user_index.get(user_key) {
            Some(set) => set.iter().map(|k| k.key().clone()).collect(),
            None => return Vec::new(),
        };
        keys.into_iter()
            .filter_map(|k| {
                let seq = self
                    .sessions
                    .get(&k)
                    .filter(|e| pred(e.value()))
                    .map(|e| e.value().created_seq)?;
                Some((k, seq))
            })
            .collect()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn conn() -> Connection {
        let (tx, _) = mpsc::channel(1);
        let (ctl, _) = mpsc::channel(1);
        Connection { tx, ctl }
    }

    /// C5: removing one of a user's sessions must never drop a concurrently
    /// inserted sibling from the user index.
    #[test]
    fn concurrent_insert_remove_keeps_user_index_consistent() {
        let r = SessionRegistry::new();
        let lost = AtomicBool::new(false);
        std::thread::scope(|s| {
            for who in ["a", "b"] {
                let (r, lost) = (&r, &lost);
                s.spawn(move || {
                    let sk = format!("t::u::{who}");
                    for _ in 0..40_000 {
                        r.try_insert("t".into(), "t::u".into(), sk.clone(), who.into(), conn(), 0)
                            .unwrap();
                        if r.find_session_by_sid("t::u", who).is_none() {
                            lost.store(true, Ordering::Relaxed);
                        }
                        r.remove_session("t::u", &sk);
                    }
                });
            }
        });
        assert!(
            !lost.load(Ordering::Relaxed),
            "a session vanished from user_index"
        );
        assert!(r.user_index.is_empty());
        assert_eq!(r.count_tenant_sessions("t"), 0);
    }
}
