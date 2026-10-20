use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;

use std::sync::atomic::{AtomicU64, Ordering};
use wsprism_protocol::error::{Result, WsPrismError};

use super::signals::SessionSignals;
use crate::realtime::types::OutFrame;

/// Handles to one live session.
#[derive(Clone)]
pub struct Connection {
    /// Outbound message queue (drained by the session's writer loop).
    pub tx: mpsc::Sender<OutFrame>,
    /// Close requests and liveness timestamps: other tasks ask the session
    /// to close itself (kick, replace, drain, idle) and the session ends
    /// promptly even if its peer never answers (C8).
    pub signals: Arc<SessionSignals>,
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
    /// `user_key -> {session_key}`. Plain inner set: the outer shard lock
    /// serializes access (see `presence::SetIndex`).
    user_index: DashMap<String, HashSet<String>>,
    // Sprint 5: O(1) Tenant Counter
    tenant_counts: DashMap<String, AtomicU64>,
    seq: AtomicU64,
}

impl SessionRegistry {
    /// No sessions yet.
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
            user_index: DashMap::new(),
            tenant_counts: DashMap::new(),
            seq: AtomicU64::new(1),
        }
    }

    // Sprint 5: try_insert with limits enforcement
    /// Insert a session while enforcing a tenant-wide cap (`0` = none). The
    /// cap is exact: the check and the increment are one CAS (the old
    /// increment-then-check could refuse two racing sessions when only one
    /// was over).
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

        let mut n = counter.load(Ordering::Relaxed);
        loop {
            if max_total > 0 && n >= max_total {
                return Err(WsPrismError::ResourceExhausted(
                    "tenant session limit reached".into(),
                ));
            }
            match counter.compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => break,
                Err(actual) => n = actual,
            }
        }
        drop(counter);

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

    /// Remove a session; returns its handles if it was registered.
    pub fn remove_session(&self, user_key: &str, session_key: &str) -> Option<Connection> {
        // C5: remove the key and drop the user's set if it became empty in
        // one step under the shard lock; inserts go through `entry` and hold
        // the same lock.
        self.user_index.remove_if_mut(user_key, |_, set| {
            set.remove(session_key);
            set.is_empty()
        });

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

    /// Handles of a connected session.
    pub fn get_session(&self, session_key: &str) -> Option<Connection> {
        self.sessions
            .get(session_key)
            .map(|r| r.value().conn.clone())
    }

    /// Session keys of a user's live sessions (snapshot).
    pub fn user_session_keys(&self, user_key: &str) -> Vec<String> {
        self.user_index
            .get(user_key)
            .map(|set| set.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Connected sessions of one user.
    pub fn count_user_sessions(&self, user_key: &str) -> usize {
        self.user_index.get(user_key).map(|s| s.len()).unwrap_or(0)
    }

    // Sprint 5: O(1) Tenant Counter Lookup
    /// Connected sessions of one tenant.
    pub fn count_tenant_sessions(&self, tenant_id: &str) -> u64 {
        self.tenant_counts
            .get(tenant_id)
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// Visit every live session without copying keys (sweeper, drain). The
    /// map's shard read locks are held during `f`, so `f` must not touch the
    /// registry and must not block.
    pub fn for_each_session(&self, mut f: impl FnMut(&Connection)) {
        for entry in self.sessions.iter() {
            f(&entry.value().conn);
        }
    }

    /// Every session as (user key, session key, sid), e.g. to check them
    /// against the cluster's session table (ADR 0011 R5).
    pub fn local_sessions(&self) -> Vec<(String, String, String)> {
        let mut out = Vec::new();
        for user in self.user_index.iter() {
            for key in user.value() {
                if let Some(e) = self.sessions.get(key) {
                    out.push((user.key().clone(), key.clone(), e.sid.clone()));
                }
            }
        }
        out
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

    /// Connected sessions in total.
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

    /// Live connections of a user, optionally only the one(s) using `sid`.
    pub fn user_connections(&self, user_key: &str, sid: Option<&str>) -> Vec<Connection> {
        self.user_sessions_where(user_key, |e| sid.is_none_or(|s| e.sid == s))
            .into_iter()
            .filter_map(|(k, _)| self.get_session(&k))
            .collect()
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
            Some(set) => set.iter().cloned().collect(),
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
        Connection {
            tx,
            signals: SessionSignals::new(),
        }
    }

    /// The tenant cap is exact under contention: exactly `max_total`
    /// sessions get in, never fewer, never more.
    #[test]
    fn tenant_cap_is_exact_under_contention() {
        let r = SessionRegistry::new();
        let ok = std::sync::atomic::AtomicUsize::new(0);
        std::thread::scope(|s| {
            for t in 0..8 {
                let (r, ok) = (&r, &ok);
                s.spawn(move || {
                    for i in 0..200 {
                        let sk = format!("t::u{t}::{i}");
                        if r.try_insert(
                            "t".into(),
                            format!("t::u{t}"),
                            sk,
                            "s".into(),
                            conn(),
                            1000,
                        )
                        .is_ok()
                        {
                            ok.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                });
            }
        });
        assert_eq!(ok.load(Ordering::Relaxed), 1000);
        assert_eq!(r.count_tenant_sessions("t"), 1000);
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
