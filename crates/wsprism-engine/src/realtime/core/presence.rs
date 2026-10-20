use crate::RoomLimits;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use std::collections::HashSet;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;

use super::session_registry::Connection;
use crate::broker::{BrokerOutbox, Subscribed, Subscription, NODE_SLOT_BITS};
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
    /// The member session's internal key.
    pub key: Arc<str>,
    /// Delivery handles of the member session.
    pub conn: Connection,
    /// The member's user id (without the tenant), for presence events.
    pub user: Arc<str>,
    /// Assigned by `Presence::try_join` (callers pass 0).
    pub member_id: u32,
    /// Cluster history rooms: kept messages up to this offset already
    /// reached the client (its recovering join's position; callers pass 0).
    pub skip_upto: u64,
    /// Its room hears when it leaves (tenant `presence_events`); kept for
    /// the cluster's membership (ADR 0011 R5).
    pub notify: bool,
}

/// What a join hands back to the session (protocol v2 `sys.joined`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Joined {
    /// The room's id, the same for every member (v2 Hot `chan`, ADR 0006 R7).
    pub room_id: u32,
    /// This membership's id, unique per join (v2 Hot `sender`).
    pub member_id: u32,
}

/// A membership that ended (ADR 0010: `sys.member_left`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Departed {
    /// Internal room key (`tenant::room`).
    pub room_key: String,
    /// The user id (without the tenant).
    pub user: Arc<str>,
    /// The membership's id (the Hot `sender` other members saw).
    pub member_id: u32,
}

/// A live room: its id (valid while the room exists) and its members.
struct Room {
    id: u32,
    members: Vec<Member>,
    /// Next member counter (low bits of a member id, ADR 0011 B1).
    next_member: u32,
    /// The counter has wrapped: new ids must skip live members' ids.
    wrapped: bool,
    /// Queued broker: becomes true once this node's subscription to the
    /// room is in effect (a recovering join waits for it, ADR 0011 §3).
    subscribed: Option<Subscribed>,
}

/// Bits of a member id counted per room (the rest is the node slot).
const MEMBER_BITS: u32 = 32 - NODE_SLOT_BITS;
const MEMBER_MASK: u32 = (1 << MEMBER_BITS) - 1;

impl Room {
    fn new(id: u32) -> Self {
        Self {
            id,
            members: Vec::new(),
            next_member: 1,
            wrapped: false,
            subscribed: None,
        }
    }

    /// A member id no live member of this room has: `slot | counter`
    /// (ADR 0011 B1). The counter starts at 1, so slot 0 never issues 0
    /// (the server's `sender`). Until the counter first wraps (~4.2 million
    /// joins of this room on this node) ids are fresh by construction; after
    /// that, live members' ids are skipped.
    fn issue_member_id(&mut self, slot: u32) -> Result<u32> {
        for _ in 0..MEMBER_MASK {
            let c = self.next_member;
            if c == MEMBER_MASK {
                self.next_member = 1;
                self.wrapped = true;
            } else {
                self.next_member = c + 1;
            }
            let id = slot | c;
            if !self.wrapped || !self.members.iter().any(|m| m.member_id == id) {
                return Ok(id);
            }
        }
        Err(WsPrismError::ResourceExhausted(
            "no free member id in this room".into(),
        ))
    }
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

    // Room ids live as long as the room (no table of past names to grow);
    // u32 wrap-around after ~4 billion rooms is accepted (a collision needs
    // both rooms to still be live). Member ids are per room (`Room`).
    next_room_id: AtomicU32,
    /// This node's slot, pre-shifted into a member id's top bits (set when
    /// the broker starts, before anyone joins).
    slot_bits: AtomicU32,
    /// Queued broker: where room creation/removal records its
    /// (un)subscription, under the same shard lock (ADR 0011 B5).
    outbox: Option<Arc<BrokerOutbox>>,
}

impl Presence {
    /// No rooms yet (single node: slot 0, no broker subscriptions).
    pub fn new() -> Self {
        Self::with_routing(0, None)
    }

    /// For node `slot` (already validated), recording subscriptions into
    /// `outbox` if the broker is queued.
    pub(crate) fn with_routing(slot: u16, outbox: Option<Arc<BrokerOutbox>>) -> Self {
        Self {
            room_to_sessions: DashMap::new(),
            session_to_rooms: DashMap::new(),
            room_to_users: DashMap::new(),
            user_to_rooms: DashMap::new(),
            user_room_refs: DashMap::new(),
            tenant_room_counts: DashMap::new(),
            next_room_id: AtomicU32::new(1),
            slot_bits: AtomicU32::new(u32::from(slot) << MEMBER_BITS),
            outbox,
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
    ) -> Result<(Joined, bool)> {
        // O(1) check first; the member scan only runs for a repeated join.
        if self.is_member(room_key, &member.key) {
            if let Some(joined) = self.membership(room_key, &member.key) {
                return Ok((joined, false));
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
        let (room_id, member_id) = match self.room_to_sessions.entry(room_key.to_string()) {
            Entry::Occupied(mut room) => {
                let room = room.get_mut();
                let member_id = room.issue_member_id(self.slot_bits.load(Ordering::Relaxed))?;
                room.members.push(Member {
                    member_id,
                    ..member
                });
                (room.id, member_id)
            }
            Entry::Vacant(slot) => {
                self.reserve_room_slot(tenant_id, limits.max_rooms_total)?;
                let mut room = Room::new(self.next_room_id.fetch_add(1, Ordering::Relaxed));
                let member_id = room.issue_member_id(self.slot_bits.load(Ordering::Relaxed))?;
                room.members.push(Member {
                    member_id,
                    ..member
                });
                let id = room.id;
                // Still under the shard lock: commands keep the real order.
                if let Some(outbox) = &self.outbox {
                    let done = Arc::new(tokio::sync::watch::Sender::new(false));
                    room.subscribed = Some(Arc::clone(&done));
                    outbox.push_sub(Subscription::Subscribe(Arc::from(room_key)), Some(done));
                }
                slot.insert(room);
                (id, member_id)
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

        Ok((Joined { room_id, member_id }, true))
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
    pub fn leave(
        &self,
        tenant_id: &str,
        room_key: &str,
        user_key: &str,
        session_key: &str,
    ) -> Option<Departed> {
        // Only this session's own task (or its final cleanup) changes its
        // room set, so membership checked here cannot change under us.
        let was_member = self
            .session_to_rooms
            .get_mut(session_key)
            .is_some_and(|mut rooms| rooms.remove(room_key));
        if !was_member {
            return None;
        }
        self.session_to_rooms
            .remove_if(session_key, |_, rooms| rooms.is_empty());
        self.leave_routing_and_governance(tenant_id, room_key, user_key, session_key)
    }

    /// The part of a leave shared with `cleanup_session` (which has already
    /// removed the session's room set as a whole).
    fn leave_routing_and_governance(
        &self,
        tenant_id: &str,
        room_key: &str,
        user_key: &str,
        session_key: &str,
    ) -> Option<Departed> {
        // 1. Routing. Removing the member and dropping an emptied room (and
        //    its tenant slot) is one step under the shard lock (C5).
        let mut departed = None;
        let room_dropped = self
            .room_to_sessions
            .remove_if_mut(room_key, |_, room| {
                if let Some(i) = room.members.iter().position(|m| &*m.key == session_key) {
                    let m = room.members.swap_remove(i);
                    departed = Some(Departed {
                        room_key: room_key.to_string(),
                        user: m.user,
                        member_id: m.member_id,
                    });
                }
                let empty = room.members.is_empty();
                if empty {
                    // Under the shard lock, like the Subscribe (B5).
                    if let Some(outbox) = &self.outbox {
                        outbox.push_sub(Subscription::Unsubscribe(Arc::from(room_key)), None);
                    }
                }
                empty
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
        departed
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

    /// Use node slot `slot` (already validated) for member ids from now on.
    pub(crate) fn set_node_slot(&self, slot: u16) {
        self.slot_bits
            .store(u32::from(slot) << MEMBER_BITS, Ordering::Relaxed);
    }

    /// Queued broker: resolves to `true` once this node's subscription to
    /// the room is in effect. `None` without a queued broker or room.
    pub(crate) fn subscribed(&self, room_key: &str) -> Option<tokio::sync::watch::Receiver<bool>> {
        self.room_to_sessions
            .get(room_key)?
            .subscribed
            .as_ref()
            .map(|s| s.subscribe())
    }

    /// Point member `session_key` of `room_key` at `conn`, then run `then`
    /// while still holding the room's write lock (no publish can reach the
    /// room in between). Returns false if the session is not a member.
    pub(crate) fn replace_member_conn(
        &self,
        room_key: &str,
        session_key: &str,
        conn: Connection,
        skip_upto: u64,
        then: impl FnOnce(),
    ) -> bool {
        let Some(mut room) = self.room_to_sessions.get_mut(room_key) else {
            return false;
        };
        let Some(m) = room.members.iter_mut().find(|m| &*m.key == session_key) else {
            return false;
        };
        m.conn = conn;
        m.skip_upto = skip_upto;
        then();
        true
    }

    /// Every membership on this node: (room key, member), e.g. to restore
    /// the cluster's membership after Redis lost it (ADR 0011 R5).
    pub fn local_members(&self) -> Vec<(String, Member)> {
        let mut out = Vec::new();
        for room in self.room_to_sessions.iter() {
            for m in &room.members {
                out.push((room.key().clone(), m.clone()));
            }
        }
        out
    }

    /// How many sessions are in the room.
    pub fn member_count(&self, room_key: &str) -> usize {
        self.room_to_sessions
            .get(room_key)
            .map_or(0, |r| r.members.len())
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
    /// Remove a closed session from every room it was in.
    pub fn cleanup_session(
        &self,
        tenant_id: &str,
        user_key: &str,
        session_key: &str,
    ) -> Vec<Departed> {
        let Some((_, rooms)) = self.session_to_rooms.remove(session_key) else {
            return Vec::new();
        };
        rooms
            .iter()
            .filter_map(|room_key| {
                self.leave_routing_and_governance(tenant_id, room_key, user_key, session_key)
            })
            .collect()
    }

    /// The other members of `room_key` (`user`, `member_id`), at most `cap`;
    /// the flag says whether some were left out (ADR 0010 snapshot).
    pub fn members_except(
        &self,
        room_key: &str,
        except_session: &str,
        cap: usize,
    ) -> (Vec<(Arc<str>, u32)>, bool) {
        let Some(room) = self.room_to_sessions.get(room_key) else {
            return (Vec::new(), false);
        };
        let mut others = room.members.iter().filter(|m| &*m.key != except_session);
        let list: Vec<_> = others
            .by_ref()
            .take(cap)
            .map(|m| (Arc::clone(&m.user), m.member_id))
            .collect();
        let truncated = others.next().is_some();
        (list, truncated)
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
            skip_upto: 0,
            notify: false,
            user: Arc::from("u"),
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
                        let _ = p.leave("t", "t::r", &uk, &sk);
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
                        let _ = p.leave("t", &rk, &uk, &sk);
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
        let _ = p.leave("t", "t::r", "t::u", "t::u::s");
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
        let _ = p.leave("t", "t::r", "t::u", "t::u::b");
        assert_eq!(p.member_keys("t::r"), vec!["t::u::a".to_string()]);
        assert!(p
            .user_to_rooms
            .get("t::u")
            .is_some_and(|r| r.contains("t::r")));
    }

    /// Every member of a room sees the same room id; member ids differ, and
    /// re-joining returns the existing membership.
    #[test]
    fn departures_and_snapshots_name_user_and_member() {
        let p = Presence::new();
        let mut a = member("t::u1::a");
        a.user = Arc::from("u1");
        let mut b = member("t::u2::b");
        b.user = Arc::from("u2");
        let (ja, _) = p.try_join("t", "t::r", "t::u1", a, &limits()).unwrap();
        let (jb, _) = p.try_join("t", "t::r", "t::u2", b, &limits()).unwrap();
        let (others, truncated) = p.members_except("t::r", "t::u1::a", 10);
        assert_eq!(others, vec![(Arc::from("u2"), jb.member_id)]);
        assert!(!truncated);
        assert!(p.members_except("t::r", "x", 1).1, "cap reached");
        let gone = p.cleanup_session("t", "t::u1", "t::u1::a");
        assert_eq!(
            gone,
            vec![Departed {
                room_key: "t::r".into(),
                user: Arc::from("u1"),
                member_id: ja.member_id
            }]
        );
        assert_eq!(
            p.leave("t", "t::r", "t::u2", "t::u2::b")
                .map(|d| d.member_id),
            Some(jb.member_id)
        );
    }

    #[test]
    fn room_id_is_shared_and_member_ids_are_unique() {
        let p = Presence::new();
        let (a, new_a) = p
            .try_join("t", "t::r", "t::u1", member("t::u1::a"), &limits())
            .unwrap();
        let (b, _) = p
            .try_join("t", "t::r", "t::u2", member("t::u2::b"), &limits())
            .unwrap();
        assert!(new_a);
        assert_eq!(a.room_id, b.room_id);
        assert_ne!(a.member_id, b.member_id);
        let (again, new_again) = p
            .try_join("t", "t::r", "t::u1", member("t::u1::a"), &limits())
            .unwrap();
        assert_eq!(again, a);
        assert!(!new_again, "a repeated join is not a new membership");
        assert_eq!(p.membership("t::r", "t::u2::b"), Some(b));
    }

    /// ADR 0011 B1: after the per-room counter wraps, new ids skip the ids
    /// of members still in the room, never issue 0, and keep the node slot.
    #[test]
    fn member_ids_skip_live_members_after_the_counter_wraps() {
        let p = Presence::with_routing(3, None);
        let join = |s: &str| {
            p.try_join("t", "t::r", "t::u", member(s), &limits())
                .unwrap()
                .0
                .member_id
        };
        let slot = 3 << MEMBER_BITS;
        let first = join("t::u::s1"); // counter 1, stays in the room
        let second = join("t::u::s2"); // counter 2, stays too
        assert_eq!((first, second), (slot | 1, slot | 2));
        // Jump to the end of the counter.
        p.room_to_sessions.get_mut("t::r").unwrap().next_member = MEMBER_MASK - 1;
        assert_eq!(join("t::u::s3"), slot | (MEMBER_MASK - 1));
        assert_eq!(join("t::u::s4"), slot | MEMBER_MASK);
        // Wraps to 1 and 2 (live: skipped), then 3.
        assert_eq!(join("t::u::s5"), slot | 3);
        // Once a live member leaves, its id may be issued again.
        p.leave("t", "t::r", "t::u", "t::u::s1");
        p.room_to_sessions.get_mut("t::r").unwrap().next_member = 1;
        assert_eq!(join("t::u::s6"), slot | 1);
        assert_eq!(join("t::u::s7"), slot | 4);
    }
}
