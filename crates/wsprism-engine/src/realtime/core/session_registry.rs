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

/// A session to register ([`SessionRegistry::admit`]).
pub struct NewSession {
    /// Tenant id.
    pub tenant_id: String,
    /// `tenant::user`.
    pub user_key: String,
    /// Internal, unique per connection.
    pub session_key: String,
    /// Client-facing session id.
    pub sid: String,
    /// Its handles.
    pub conn: Connection,
    /// Tenant-wide cap (`0` = none).
    pub max_total: u64,
}

/// A user's session limit on this node.
#[derive(Debug, Clone, Copy)]
pub struct UserLimit {
    /// Sessions a user may have.
    pub max: usize,
    /// At the limit, evict the oldest instead of refusing the new one.
    pub kick_oldest: bool,
}

/// Why a session was not admitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// The user's limit, with `kick_oldest` off.
    TooManySessions,
    /// The tenant's cap.
    TenantFull,
    /// Cluster: the session table replaced or evicted it before it
    /// registered here.
    Replaced,
}

/// A session [`SessionRegistry::admit`] unregistered to make room.
pub struct Evicted {
    /// Its key.
    pub session_key: String,
    /// Its handles (to tell it and close it).
    pub conn: Connection,
    /// `replaced` or `max_sessions_exceeded`.
    pub reason: &'static str,
}

impl Evicted {
    fn new(session_key: String, conn: Connection, reason: &'static str) -> Self {
        Self {
            session_key,
            conn,
            reason,
        }
    }
}

/// What [`SessionRegistry::admit`] did.
pub struct Admitted {
    /// Sessions unregistered to make room (also when refused: a replaced
    /// sid stays replaced).
    pub evicted: Vec<Evicted>,
    /// `Some` = the new session was not registered.
    pub refused: Option<Refusal>,
}

impl Admitted {
    fn refused(evicted: Vec<Evicted>, why: Refusal) -> Self {
        Self {
            evicted,
            refused: Some(why),
        }
    }
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
    /// Cluster: sessions the session table already replaced or evicted
    /// before they registered here (their handshake was between its claim
    /// and its registration). Their registration is refused. Entries go
    /// after [`DOOMED_FOR`].
    doomed: DashMap<String, std::time::Instant>,
}

/// How long a doomed key is remembered: far longer than a handshake takes
/// between its claim and its registration.
const DOOMED_FOR: std::time::Duration = std::time::Duration::from_secs(60);

/// The user key of a session key (`tenant::user::sid#n`; sids have no `::`).
fn user_key_of(session_key: &str) -> &str {
    session_key
        .rsplit_once("::")
        .map_or(session_key, |(u, _)| u)
}

impl SessionRegistry {
    /// No sessions yet.
    pub fn new() -> Self {
        Self {
            sessions: DashMap::new(),
            user_index: DashMap::new(),
            tenant_counts: DashMap::new(),
            seq: AtomicU64::new(1),
            doomed: DashMap::new(),
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

    /// Admit a session of one user on this node (no cluster session table):
    /// replace the user's session using the same `sid`, apply the per-user
    /// limit, and register — all under the user's index entry, so racing
    /// handshakes of one user see each other (separate steps let 7 of 32
    /// concurrent connections with one sid all live, and could let a limit
    /// of 1 admit two). The tenant cap is exact as in [`Self::try_insert`].
    /// The returned sessions are already unregistered: the caller tells
    /// them why and closes them.
    pub fn admit(&self, s: NewSession, limit: UserLimit) -> Admitted {
        let mut evicted = Vec::new();
        let mut set = self.user_index.entry(s.user_key.clone()).or_default();
        // Lock order everywhere: `user_index`, then `sessions`.
        let same_sid: Vec<String> = set
            .iter()
            .filter(|k| self.sessions.get(*k).is_some_and(|e| e.sid == s.sid))
            .cloned()
            .collect();
        for k in same_sid {
            set.remove(&k);
            if let Some(conn) = self.unregister(&k) {
                evicted.push(Evicted::new(k, conn, "replaced"));
            }
        }
        if set.len() >= limit.max {
            if !limit.kick_oldest {
                let empty = set.is_empty();
                drop(set);
                if empty {
                    self.user_index
                        .remove_if(&s.user_key, |_, set| set.is_empty());
                }
                return Admitted::refused(evicted, Refusal::TooManySessions);
            }
            while set.len() >= limit.max.max(1) {
                let oldest = set
                    .iter()
                    .filter_map(|k| self.sessions.get(k).map(|e| (k.clone(), e.created_seq)))
                    .min_by_key(|(_, seq)| *seq)
                    .map(|(k, _)| k);
                let Some(k) = oldest else { break };
                set.remove(&k);
                if let Some(conn) = self.unregister(&k) {
                    evicted.push(Evicted::new(k, conn, "max_sessions_exceeded"));
                }
            }
        }
        if !self.reserve_tenant_slot(&s.tenant_id, s.max_total) {
            let empty = set.is_empty();
            drop(set);
            if empty {
                self.user_index
                    .remove_if(&s.user_key, |_, set| set.is_empty());
            }
            return Admitted::refused(evicted, Refusal::TenantFull);
        }
        set.insert(s.session_key.clone());
        let created_seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.sessions.insert(
            s.session_key,
            SessionEntry {
                conn: s.conn,
                created_seq,
                tenant_id: s.tenant_id,
                sid: s.sid,
            },
        );
        Admitted {
            evicted,
            refused: None,
        }
    }

    /// Cluster mode: register a session the cluster's session table
    /// admitted, unless the table already replaced or evicted it meanwhile
    /// ([`Self::remove_or_doom`], [`Self::find_or_doom`]: checked under the
    /// same user entry, so one of the two sees the other).
    pub fn insert_claimed(&self, s: NewSession) -> std::result::Result<(), Refusal> {
        let mut set = self.user_index.entry(s.user_key.clone()).or_default();
        let refused = if self.doomed.remove(&s.session_key).is_some() {
            Some(Refusal::Replaced)
        } else if !self.reserve_tenant_slot(&s.tenant_id, s.max_total) {
            Some(Refusal::TenantFull)
        } else {
            None
        };
        if let Some(why) = refused {
            let empty = set.is_empty();
            drop(set);
            if empty {
                self.user_index
                    .remove_if(&s.user_key, |_, set| set.is_empty());
            }
            return Err(why);
        }
        set.insert(s.session_key.clone());
        let created_seq = self.seq.fetch_add(1, Ordering::Relaxed);
        self.sessions.insert(
            s.session_key,
            SessionEntry {
                conn: s.conn,
                created_seq,
                tenant_id: s.tenant_id,
                sid: s.sid,
            },
        );
        Ok(())
    }

    /// Cluster mode, an eviction decided by this node's claim: unregister
    /// the session, or if it is not registered (yet), refuse its
    /// registration later.
    pub fn remove_or_doom(&self, user_key: &str, session_key: &str) -> Option<Connection> {
        let mut set = self.user_index.entry(user_key.to_string()).or_default();
        let conn = if set.remove(session_key) {
            self.unregister(session_key)
        } else {
            self.doom(session_key);
            None
        };
        let empty = set.is_empty();
        drop(set);
        if empty {
            self.user_index.remove_if(user_key, |_, set| set.is_empty());
        }
        conn
    }

    /// Cluster mode, a close asked by another node: the session's handles
    /// (it closes and unregisters itself), or if it is not registered
    /// (yet), refuse its registration later.
    pub fn find_or_doom(&self, session_key: &str) -> Option<Connection> {
        let user_key = user_key_of(session_key);
        let set = self.user_index.entry(user_key.to_string()).or_default();
        let conn = if set.contains(session_key) {
            self.sessions.get(session_key).map(|e| e.conn.clone())
        } else {
            self.doom(session_key);
            None
        };
        let empty = set.is_empty();
        drop(set);
        if empty {
            self.user_index.remove_if(user_key, |_, set| set.is_empty());
        }
        conn
    }

    /// Remember `session_key` as doomed; forget old entries now and then.
    fn doom(&self, session_key: &str) {
        let now = std::time::Instant::now();
        if self.doomed.len() >= 1024 {
            self.doomed
                .retain(|_, at| now.duration_since(*at) < DOOMED_FOR);
        }
        self.doomed.insert(session_key.to_string(), now);
    }

    /// Count one more session of `tenant_id` unless that reaches `max_total`
    /// (`0` = no cap); one CAS, so the cap is exact.
    fn reserve_tenant_slot(&self, tenant_id: &str, max_total: u64) -> bool {
        let counter = self
            .tenant_counts
            .entry(tenant_id.to_string())
            .or_insert_with(|| AtomicU64::new(0));
        let mut n = counter.load(Ordering::Relaxed);
        loop {
            if max_total > 0 && n >= max_total {
                return false;
            }
            match counter.compare_exchange_weak(n, n + 1, Ordering::Relaxed, Ordering::Relaxed) {
                Ok(_) => return true,
                Err(actual) => n = actual,
            }
        }
    }

    /// Drop a session's entry and its tenant count (not its user index
    /// entry: the caller holds that).
    fn unregister(&self, session_key: &str) -> Option<Connection> {
        let (_, entry) = self.sessions.remove(session_key)?;
        if let Some(counter) = self.tenant_counts.get(&entry.tenant_id) {
            counter.fetch_sub(1, Ordering::Relaxed);
        }
        Some(entry.conn)
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

    fn new_session(user: &str, key: &str, sid: &str) -> NewSession {
        NewSession {
            tenant_id: "t".into(),
            user_key: format!("t::{user}"),
            session_key: key.into(),
            sid: sid.into(),
            conn: conn(),
            max_total: 0,
        }
    }

    /// One user's racing admissions: the limit and the one-session-per-sid
    /// rule hold (they were separate steps before; 7 of 32 racing
    /// connections with one sid all stayed).
    #[test]
    fn admit_holds_the_user_limit_and_one_sid_under_contention() {
        let r = SessionRegistry::new();
        let limit = UserLimit {
            max: 3,
            kick_oldest: false,
        };
        std::thread::scope(|s| {
            for t in 0..8 {
                let r = &r;
                s.spawn(move || {
                    for i in 0..500 {
                        let a = r.admit(
                            new_session("u", &format!("t::u::s{t}-{i}#1"), &format!("s{t}-{i}")),
                            limit,
                        );
                        assert!(a.evicted.is_empty());
                    }
                });
            }
        });
        assert_eq!(r.count_user_sessions("t::u"), 3);
        assert_eq!(r.count_tenant_sessions("t"), 3);

        let r = SessionRegistry::new();
        let limit = UserLimit {
            max: 1000,
            kick_oldest: false,
        };
        std::thread::scope(|s| {
            for t in 0..8 {
                let r = &r;
                s.spawn(move || {
                    for i in 0..200 {
                        let a = r.admit(
                            new_session("u", &format!("t::u::tab#{t}-{i}"), "tab"),
                            limit,
                        );
                        assert!(a.refused.is_none());
                    }
                });
            }
        });
        assert_eq!(r.count_user_sessions("t::u"), 1, "one session per sid");
        assert_eq!(r.count_tenant_sessions("t"), 1);
    }

    /// Cluster: an eviction of a session not registered yet refuses its
    /// registration, whichever comes first.
    #[test]
    fn a_doomed_session_cannot_register() {
        let r = SessionRegistry::new();
        assert!(r.remove_or_doom("t::u", "t::u::tab#1").is_none());
        assert_eq!(
            r.insert_claimed(new_session("u", "t::u::tab#1", "tab")),
            Err(Refusal::Replaced)
        );
        assert!(r.find_or_doom("t::u::tab#2").is_none());
        assert_eq!(
            r.insert_claimed(new_session("u", "t::u::tab#2", "tab")),
            Err(Refusal::Replaced)
        );
        assert_eq!(
            r.insert_claimed(new_session("u", "t::u::tab#3", "tab")),
            Ok(())
        );
        assert!(r.find_or_doom("t::u::tab#3").is_some());
        assert!(r.remove_or_doom("t::u", "t::u::tab#3").is_some());
        assert_eq!(r.count_user_sessions("t::u"), 0);
        assert_eq!(r.count_tenant_sessions("t"), 0);
        assert!(r.user_index.is_empty());
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
