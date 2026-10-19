use crate::RoomLimits;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use super::session_registry::Connection;
use wsprism_protocol::error::{Result, WsPrismError};

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

/// A set-valued index. The inner set is a plain `HashSet`: the outer map's
/// shard lock already serializes every access to it, and a nested `DashMap`
/// would allocate (CPU count x 4) shards per key (performance-architecture
/// §2.1). An empty set allocates nothing.
type SetIndex = DashMap<String, HashSet<String>>;

/// Remove `item` from the set at `key`, dropping the set if that left it
/// empty. Both happen in one step under the shard write lock, so an insert
/// racing with us is never lost (C5). Returns true if the set was dropped.
fn remove_from_set(map: &SetIndex, key: &str, item: &str) -> bool {
    map.remove_if_mut(key, |_, set| {
        set.remove(item);
        set.is_empty()
    })
    .is_some()
}

/// Add `item` to the set at `key`, creating the set if needed.
fn add_to_set(map: &SetIndex, key: &str, item: &str) {
    map.entry(key.to_string())
        .or_default()
        .insert(item.to_string());
}

/// One room member as the fan-out sees it: the session's internal key and
/// its delivery handles, so publishing needs no registry lookup, key copy or
/// handle clone per recipient (performance-architecture §2.2).
#[derive(Clone)]
pub struct Member {
    pub key: Arc<str>,
    pub conn: Connection,
    /// Assigned by `Presence::try_join` (callers pass 0).
    pub member_id: u32,
}

/// What a join hands back to the session (protocol v2 `sys.joined`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Joined {
    /// The room's id, the same for every member (v2 Hot `chan`, ADR 0006 R7).
    pub room_id: u32,
    /// This membership's id, unique per join (v2 Hot `sender`).
    pub member_id: u32,
}

/// A live room: its id (valid while the room exists) and its members.
struct Room {
    id: u32,
    members: Vec<Member>,
}

/// Room presence: `room_key -> members`, `session_key -> rooms`.
///
/// Sprint 5: Added user-level indexing and tenant counters for governance.
/// See `try_join` for which limits are exact and which are best-effort.
#[derive(Default)]
pub struct Presence {
    // Routing indices. Members are a plain `Vec`: publishing iterates it
    // under the shard read lock (contiguous, no allocation); joins push, and
    // a join is idempotent per session, so no duplicate check is needed.
    room_to_sessions: DashMap<String, Room>,
    session_to_rooms: SetIndex,

    // Governance indices
    room_to_users: SetIndex,
    user_to_rooms: SetIndex,

    // Multi-session ref-counting: user::room -> session_count
    user_room_refs: DashMap<String, usize>,

    // O(1) Counters
    tenant_room_counts: DashMap<String, AtomicU64>,

    // Id sources. Room ids live as long as the room (no table of past names
    // to grow); member ids are per join. u32 wrap-around after ~4 billion
    // rooms/joins is accepted (collisions need both to still be live).
    next_room_id: AtomicU32,
    next_member_id: AtomicU32,
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
            next_room_id: AtomicU32::new(1),
            next_member_id: AtomicU32::new(1),
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
    ///
    /// Idempotent: a session already in the room is left unchanged (joining
    /// twice used to count the user's room reference twice, so one leave
    /// left a dangling user<->room link).
    pub fn try_join(
        &self,
        tenant_id: &str,
        room_key: &str,
        user_key: &str,
        member: Member,
        limits: &RoomLimits,
    ) -> Result<Joined> {
        // O(1) check first; the member scan only runs for a repeated join.
        if self.is_member(room_key, &member.key) {
            if let Some(joined) = self.membership(room_key, &member.key) {
                return Ok(joined);
            }
        }
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
        let session_key = Arc::clone(&member.key);
        let member_id = self.next_member_id.fetch_add(1, Ordering::Relaxed);
        let member = Member {
            member_id,
            ..member
        };
        let room_id = match self.room_to_sessions.entry(room_key.to_string()) {
            Entry::Occupied(mut room) => {
                room.get_mut().members.push(member);
                room.get().id
            }
            Entry::Vacant(slot) => {
                self.reserve_room_slot(tenant_id, limits.max_rooms_total)?;
                let id = self.next_room_id.fetch_add(1, Ordering::Relaxed);
                slot.insert(Room {
                    id,
                    members: vec![member],
                });
                id
            }
        };
        add_to_set(&self.session_to_rooms, &session_key, room_key);

        // --- 4. Governance (ref-counted per user+room for multi-session).
        //        The user indices are updated while holding the ref entry, so
        //        join/leave of the same user+room are serialized.
        let ref_key = format!("{}::{}", user_key, room_key);
        let mut refs = self.user_room_refs.entry(ref_key).or_insert(0);
        *refs += 1;
        if *refs == 1 {
            add_to_set(&self.room_to_users, room_key, user_key);
            add_to_set(&self.user_to_rooms, user_key, room_key);
        }

        Ok(Joined { room_id, member_id })
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

    /// Leave `room_key`. A no-op unless the session is a member, so the
    /// governance reference counts stay balanced.
    pub fn leave(&self, tenant_id: &str, room_key: &str, user_key: &str, session_key: &str) {
        // Only this session's own task (or its final cleanup) changes its
        // room set, so membership checked here cannot change under us.
        let was_member = self
            .session_to_rooms
            .get_mut(session_key)
            .is_some_and(|mut rooms| rooms.remove(room_key));
        if !was_member {
            return;
        }
        self.session_to_rooms
            .remove_if(session_key, |_, rooms| rooms.is_empty());
        self.leave_routing_and_governance(tenant_id, room_key, user_key, session_key);
    }

    /// The part of a leave shared with `cleanup_session` (which has already
    /// removed the session's room set as a whole).
    fn leave_routing_and_governance(
        &self,
        tenant_id: &str,
        room_key: &str,
        user_key: &str,
        session_key: &str,
    ) {
        // 1. Routing. Removing the member and dropping an emptied room (and
        //    its tenant slot) is one step under the shard lock (C5).
        let room_dropped = self
            .room_to_sessions
            .remove_if_mut(room_key, |_, room| {
                if let Some(i) = room.members.iter().position(|m| &*m.key == session_key) {
                    room.members.swap_remove(i);
                }
                room.members.is_empty()
            })
            .is_some();
        if room_dropped {
            if let Some(counter) = self.tenant_room_counts.get(tenant_id) {
                let _ = cas_update(&counter, |n| n.checked_sub(1));
            }
        }

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

    /// Call `f` for every member of `room_key`, under the room's shard read
    /// lock: `f` must not block and must not touch presence (delivering via
    /// `try_send` / `request_close` is fine). Concurrent publishes share the
    /// lock; a join/leave in the same shard waits for them.
    pub fn for_each_member(&self, room_key: &str, mut f: impl FnMut(&Member)) {
        if let Some(room) = self.room_to_sessions.get(room_key) {
            room.members.iter().for_each(&mut f);
        }
    }

    /// The room's id, if the room exists (has members).
    pub fn room_id(&self, room_key: &str) -> Option<u32> {
        self.room_to_sessions.get(room_key).map(|r| r.id)
    }

    /// User keys (`tenant::user`) with at least one session in the room.
    pub fn user_keys_in(&self, room_key: &str) -> Vec<String> {
        self.room_to_users
            .get(room_key)
            .map(|users| {
                let mut v: Vec<String> = users.iter().cloned().collect();
                v.sort();
                v
            })
            .unwrap_or_default()
    }

    /// The session's membership in `room_key`, if it has joined.
    pub fn membership(&self, room_key: &str, session_key: &str) -> Option<Joined> {
        let room = self.room_to_sessions.get(room_key)?;
        let m = room.members.iter().find(|m| &*m.key == session_key)?;
        Some(Joined {
            room_id: room.id,
            member_id: m.member_id,
        })
    }

    #[cfg(test)]
    fn member_keys(&self, room_key: &str) -> Vec<String> {
        let mut keys = Vec::new();
        self.for_each_member(room_key, |m| keys.push(m.key.to_string()));
        keys
    }

    // Called by RAII Drop
    pub fn cleanup_session(&self, tenant_id: &str, user_key: &str, session_key: &str) {
        if let Some(rooms) = self.session_to_rooms.remove(session_key).map(|(_, v)| v) {
            for room_key in &rooms {
                self.leave_routing_and_governance(tenant_id, room_key, user_key, session_key);
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

    fn limits() -> RoomLimits {
        RoomLimits::default()
    }

    fn member(session_key: &str) -> Member {
        let (tx, _) = tokio::sync::mpsc::channel(1);
        Member {
            key: Arc::from(session_key),
            conn: Connection {
                tx,
                signals: super::super::SessionSignals::new(),
            },
            member_id: 0,
        }
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
                    let m = member(&sk);
                    for _ in 0..ITERS {
                        p.try_join("t", "t::r", &uk, m.clone(), &limits()).unwrap();
                        if !p.member_keys("t::r").contains(&sk) {
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
                    let m = member(&sk);
                    for n in 0..ITERS / 4 {
                        let rk = format!("t::r{}", n % 3);
                        p.try_join("t", &rk, &uk, m.clone(), &limits()).unwrap();
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
        let lim = RoomLimits {
            max_rooms_total: 5,
            ..RoomLimits::default()
        };
        std::thread::scope(|s| {
            for i in 0..8 {
                let (p, lim) = (&p, &lim);
                s.spawn(move || {
                    let uk = format!("t::u{i}");
                    let sk = format!("{uk}::s");
                    let m = member(&sk);
                    for n in 0..50 {
                        let _ = p.try_join("t", &format!("t::r{i}-{n}"), &uk, m.clone(), lim);
                    }
                });
            }
        });
        assert_eq!(p.room_to_sessions.len(), 5);
    }

    /// Joining the same room twice is a no-op, so one leave fully leaves
    /// (previously the user's room reference was counted twice).
    #[test]
    fn repeated_join_then_one_leave_leaves_no_residue() {
        let p = Presence::new();
        let m = member("t::u::s");
        p.try_join("t", "t::r", "t::u", m.clone(), &limits())
            .unwrap();
        p.try_join("t", "t::r", "t::u", m, &limits()).unwrap();
        assert_eq!(p.member_keys("t::r"), vec!["t::u::s".to_string()]);
        p.leave("t", "t::r", "t::u", "t::u::s");
        assert!(p.room_to_sessions.is_empty());
        assert!(p.user_to_rooms.is_empty(), "user<->room link left behind");
        assert!(p.user_room_refs.is_empty());
    }

    /// Leaving a room the session never joined changes nothing.
    #[test]
    fn leave_without_membership_is_a_no_op() {
        let p = Presence::new();
        p.try_join("t", "t::r", "t::u", member("t::u::a"), &limits())
            .unwrap();
        // Another session of the same user leaves a room it never joined.
        p.leave("t", "t::r", "t::u", "t::u::b");
        assert_eq!(p.member_keys("t::r"), vec!["t::u::a".to_string()]);
        assert!(p
            .user_to_rooms
            .get("t::u")
            .is_some_and(|r| r.contains("t::r")));
    }

    /// Every member of a room sees the same room id; member ids differ, and
    /// re-joining returns the existing membership.
    #[test]
    fn room_id_is_shared_and_member_ids_are_unique() {
        let p = Presence::new();
        let a = p
            .try_join("t", "t::r", "t::u1", member("t::u1::a"), &limits())
            .unwrap();
        let b = p
            .try_join("t", "t::r", "t::u2", member("t::u2::b"), &limits())
            .unwrap();
        assert_eq!(a.room_id, b.room_id);
        assert_ne!(a.member_id, b.member_id);
        let again = p
            .try_join("t", "t::r", "t::u1", member("t::u1::a"), &limits())
            .unwrap();
        assert_eq!(again, a);
        assert_eq!(p.membership("t::r", "t::u2::b"), Some(b));
    }
}
