use crate::config::schema::TenantLimits;
use dashmap::mapref::entry::Entry;
use dashmap::{DashMap, DashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use wsprism_core::error::{Result, WsPrismError};

/// Atomically replace the value with `f(value)` (CAS loop); `None` from `f`
/// aborts without writing. Returns the previous value on success.
/// (Explicit loop rather than `fetch_update`, which newer toolchains deprecate
/// in favour of `try_update`; this keeps the MSRV unchanged.)
fn cas_update(a: &AtomicU64, f: impl Fn(u64) -> Option<u64>) -> std::result::Result<u64, u64> {
    let mut cur = a.load(Ordering::Relaxed);
    loop {
        let Some(next) = f(cur) else {
            return Err(cur);
        };
        match a.compare_exchange_weak(cur, next, Ordering::Relaxed, Ordering::Relaxed) {
            Ok(prev) => return Ok(prev),
            Err(actual) => cur = actual,
        }
    }
}

/// Remove `item` from the set at `key`, dropping the set if that left it
/// empty. The emptiness check and the removal are one atomic step under the
/// shard lock, so an insert racing with us is never lost (C5).
fn remove_from_set(map: &DashMap<String, DashSet<String>>, key: &str, item: &str) {
    if let Some(set) = map.get(key) {
        set.remove(item);
    }
    map.remove_if(key, |_, set| set.is_empty());
}

/// Room presence: `room_key -> sessions`, `session_key -> rooms`.
///
/// Sprint 5: Added user-level indexing and tenant counters for governance.
/// See `try_join` for which limits are exact and which are best-effort.
#[derive(Default)]
pub struct Presence {
    // Routing indices
    room_to_sessions: DashMap<String, DashSet<String>>,
    session_to_rooms: DashMap<String, DashSet<String>>,

    // Governance indices
    room_to_users: DashMap<String, DashSet<String>>,
    user_to_rooms: DashMap<String, DashSet<String>>,

    // Multi-session ref-counting: user::room -> session_count
    user_room_refs: DashMap<String, usize>,

    // O(1) Counters
    tenant_room_counts: DashMap<String, AtomicU64>,
}

impl Presence {
    pub fn new() -> Self {
        Self {
            room_to_sessions: DashMap::new(),
            session_to_rooms: DashMap::new(),
            room_to_users: DashMap::new(),
            user_to_rooms: DashMap::new(),
            user_room_refs: DashMap::new(),
            tenant_room_counts: DashMap::new(),
        }
    }

    /// Attempt to join a room with per-tenant/user/room limits enforced.
    ///
    /// Concurrency (C5): every "create" and "remove if empty" on a set-valued
    /// index happens under that key's DashMap shard write lock (`entry` /
    /// `remove_if`), so a leave can never delete a set that a concurrent join
    /// just added to. The tenant room counter moves only when a room set is
    /// actually created/removed, and the `max_rooms_total` cap is exact.
    /// `max_users_per_room` / `max_rooms_per_user` remain best-effort: their
    /// checks read the governance indices without locking them, so heavy
    /// contention can overshoot them slightly.
    pub fn try_join(
        &self,
        tenant_id: &str,
        room_key: &str,
        user_key: &str,
        session_key: &str,
        limits: &TenantLimits,
    ) -> Result<()> {
        // --- 1. Check Room Capacity (Max Users per Room) ---
        if limits.max_users_per_room > 0 {
            if let Some(users) = self.room_to_users.get(room_key) {
                // If user is not already in room, check limit
                if users.len() as u64 >= limits.max_users_per_room && !users.contains(user_key) {
                    return Err(WsPrismError::ResourceExhausted(
                        "room user limit reached".into(),
                    ));
                }
            }
        }

        // --- 2. Check User's Room Limit (Max Rooms per User) ---
        if limits.max_rooms_per_user > 0 {
            if let Some(rooms) = self.user_to_rooms.get(user_key) {
                if rooms.len() as u64 >= limits.max_rooms_per_user && !rooms.contains(room_key) {
                    return Err(WsPrismError::ResourceExhausted(
                        "user room limit reached".into(),
                    ));
                }
            }
        }

        // --- 3. Routing. Creating the room set reserves a tenant room slot
        //        atomically, under the room's shard lock.
        match self.room_to_sessions.entry(room_key.to_string()) {
            Entry::Occupied(room) => {
                room.get().insert(session_key.to_string());
            }
            Entry::Vacant(slot) => {
                self.reserve_room_slot(tenant_id, limits.max_rooms_total)?;
                let sessions = DashSet::new();
                sessions.insert(session_key.to_string());
                slot.insert(sessions);
            }
        }
        self.session_to_rooms
            .entry(session_key.to_string())
            .or_default()
            .insert(room_key.to_string());

        // --- 4. Governance (ref-counted per user+room for multi-session).
        //        The user indices are updated while holding the ref entry, so
        //        join/leave of the same user+room are serialized.
        let ref_key = format!("{}::{}", user_key, room_key);
        let mut refs = self.user_room_refs.entry(ref_key).or_insert(0);
        *refs += 1;
        if *refs == 1 {
            self.room_to_users
                .entry(room_key.to_string())
                .or_default()
                .insert(user_key.to_string());
            self.user_to_rooms
                .entry(user_key.to_string())
                .or_default()
                .insert(room_key.to_string());
        }

        Ok(())
    }

    /// Count a new room against the tenant, failing if `max_total` (0 =
    /// unlimited) is reached. Exact: the check and increment are one CAS.
    fn reserve_room_slot(&self, tenant_id: &str, max_total: u64) -> Result<()> {
        let counter = self
            .tenant_room_counts
            .entry(tenant_id.to_string())
            .or_default();
        cas_update(&counter, |n| {
            (max_total == 0 || n < max_total).then_some(n + 1)
        })
        .map(|_| ())
        .map_err(|_| WsPrismError::ResourceExhausted("tenant room limit reached".into()))
    }

    pub fn leave(&self, tenant_id: &str, room_key: &str, user_key: &str, session_key: &str) {
        // 1. Routing. Removing the room set and releasing its tenant slot
        //    only happens if it is still empty under the shard lock (C5).
        if let Some(set) = self.room_to_sessions.get(room_key) {
            set.remove(session_key);
        }
        if self
            .room_to_sessions
            .remove_if(room_key, |_, set| set.is_empty())
            .is_some()
        {
            if let Some(counter) = self.tenant_room_counts.get(tenant_id) {
                let _ = cas_update(&counter, |n| n.checked_sub(1));
            }
        }
        remove_from_set(&self.session_to_rooms, session_key, room_key);

        // 2. Governance: drop the user<->room link when this user's last
        //    session leaves, still holding the ref entry (see try_join).
        let ref_key = format!("{}::{}", user_key, room_key);
        if let Entry::Occupied(mut refs) = self.user_room_refs.entry(ref_key) {
            let n = refs.get_mut();
            *n = n.saturating_sub(1);
            if *n == 0 {
                remove_from_set(&self.user_to_rooms, user_key, room_key);
                remove_from_set(&self.room_to_users, room_key, user_key);
                refs.remove();
            }
        }
    }

    /// True if `session_key` has joined `room_key`.
    pub fn is_member(&self, room_key: &str, session_key: &str) -> bool {
        self.session_to_rooms
            .get(session_key)
            .is_some_and(|rooms| rooms.contains(room_key))
    }

    pub fn sessions_in(&self, room_key: &str) -> Vec<String> {
        self.room_to_sessions
            .get(room_key)
            .map(|set| set.iter().map(|u| u.key().to_string()).collect())
            .unwrap_or_default()
    }

    // Called by RAII Drop
    pub fn cleanup_session(&self, tenant_id: &str, user_key: &str, session_key: &str) {
        if let Some(rooms) = self.session_to_rooms.remove(session_key).map(|(_, v)| v) {
            for r in rooms.iter() {
                let room_key = r.key();
                // Use the full leave logic to ensure ref-counts and limits are updated correctly
                self.leave(tenant_id, room_key, user_key, session_key);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    const ITERS: usize = 40_000;

    fn limits() -> TenantLimits {
        TenantLimits::default()
    }

    /// C5: two sessions churn join/leave on the same room. A leave that sees
    /// the room momentarily empty must never delete the other's fresh join.
    #[test]
    fn concurrent_join_leave_never_loses_a_membership() {
        let p = Presence::new();
        let lost = AtomicBool::new(false);
        std::thread::scope(|s| {
            for who in ["a", "b"] {
                let (p, lost) = (&p, &lost);
                s.spawn(move || {
                    let sk = format!("t::{who}::s");
                    let uk = format!("t::{who}");
                    for _ in 0..ITERS {
                        p.try_join("t", "t::r", &uk, &sk, &limits()).unwrap();
                        if !p.sessions_in("t::r").contains(&sk) {
                            lost.store(true, Ordering::Relaxed);
                        }
                        p.leave("t", "t::r", &uk, &sk);
                    }
                });
            }
        });
        assert!(
            !lost.load(Ordering::Relaxed),
            "a join was wiped by a concurrent leave"
        );
    }

    /// C5: after any interleaving of joins/leaves that ends with everyone
    /// gone, every index and counter must be back to empty/zero.
    #[test]
    fn concurrent_churn_leaves_no_residue() {
        let p = Presence::new();
        std::thread::scope(|s| {
            for i in 0..4 {
                let p = &p;
                s.spawn(move || {
                    // Two sessions of the same user share the user<->room index.
                    let uk = format!("t::u{}", i % 2);
                    let sk = format!("{uk}::s{i}");
                    for n in 0..ITERS / 4 {
                        let rk = format!("t::r{}", n % 3);
                        p.try_join("t", &rk, &uk, &sk, &limits()).unwrap();
                        p.leave("t", &rk, &uk, &sk);
                    }
                });
            }
        });
        assert!(p.room_to_sessions.is_empty(), "room_to_sessions residue");
        assert!(p.session_to_rooms.is_empty(), "session_to_rooms residue");
        assert!(p.room_to_users.is_empty(), "room_to_users residue");
        assert!(p.user_to_rooms.is_empty(), "user_to_rooms residue");
        assert!(p.user_room_refs.is_empty(), "user_room_refs residue");
        let rooms = p
            .tenant_room_counts
            .get("t")
            .map(|c| c.load(Ordering::Relaxed))
            .unwrap_or(0);
        assert_eq!(rooms, 0, "tenant room counter drifted");
    }

    /// The tenant room cap is exact even under concurrent room creation.
    #[test]
    fn tenant_room_cap_is_never_exceeded() {
        let p = Presence::new();
        let lim = TenantLimits {
            max_rooms_total: 5,
            ..TenantLimits::default()
        };
        std::thread::scope(|s| {
            for i in 0..8 {
                let (p, lim) = (&p, &lim);
                s.spawn(move || {
                    let uk = format!("t::u{i}");
                    let sk = format!("{uk}::s");
                    for n in 0..50 {
                        let _ = p.try_join("t", &format!("t::r{i}-{n}"), &uk, &sk, lim);
                    }
                });
            }
        });
        assert_eq!(p.room_to_sessions.len(), 5);
    }
}
