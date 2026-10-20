//! Room message history and recovery on one node (ADR 0011 §3, R2).
//!
//! Rooms matching a tenant's history rule keep their recent Ext messages in
//! a ring with a position (`epoch`, `offset`). A client that reconnects asks
//! to resume from the last offset it saw and gets what it missed, in order —
//! or `recovered: false` when that is no longer possible.
//!
//! Ordering: a history room's publish (offset, store, fan-out) and a
//! recovering join (membership, reply, replay) each run under the room's
//! ring lock, so a joiner never sees a message twice or out of order.

use std::collections::hash_map::RandomState;
use std::collections::{HashMap, VecDeque};
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use dashmap::DashMap;

use crate::permissions::RoomPattern;
use crate::realtime::types::OutFrame;

use super::Presence;

/// Which rooms of a tenant keep history, and how much.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HistoryRule {
    /// Rooms this rule covers (exact or `prefix*`).
    pub rooms: RoomPattern,
    /// Messages kept per room.
    pub size: usize,
    /// How long a message is kept.
    pub ttl: Duration,
    /// Bytes kept per room (oldest messages go first).
    pub max_bytes: usize,
}

impl HistoryRule {
    /// Keep the last `size` messages of matching rooms for `ttl`, at most
    /// `max_bytes` per room.
    pub fn new(rooms: RoomPattern, size: usize, ttl: Duration, max_bytes: usize) -> Self {
        Self {
            rooms,
            size,
            ttl,
            max_bytes,
        }
    }
}

/// History rules per tenant (first matching rule wins).
#[derive(Debug, Clone, Default)]
pub struct HistoryConfig {
    tenants: HashMap<String, Vec<HistoryRule>>,
}

impl HistoryConfig {
    /// Add a rule for `tenant` (rules are tried in the order added).
    pub fn add(&mut self, tenant: &str, rule: HistoryRule) {
        self.tenants
            .entry(tenant.to_string())
            .or_default()
            .push(rule);
    }

    /// True if no tenant keeps history.
    pub fn is_empty(&self) -> bool {
        self.tenants.is_empty()
    }
}

/// Where a client asks to resume a room from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recover {
    /// The epoch the client saw.
    pub epoch: String,
    /// The last offset it received.
    pub offset: u64,
}

/// A history room's position as a joiner sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryPos {
    /// This history's lifetime id: a different epoch means earlier offsets
    /// are meaningless (history was lost or the room was idle too long).
    pub epoch: Arc<str>,
    /// The latest offset (0 = nothing yet).
    pub offset: u64,
}

struct Entry {
    offset: u64,
    at: Instant,
    frame: OutFrame,
    bytes: usize,
}

/// One room's history.
pub(crate) struct Ring {
    epoch: Arc<str>,
    /// Offset of the next message.
    next: u64,
    entries: VecDeque<Entry>,
    bytes: usize,
    rule: HistoryRule,
    last_activity: Instant,
}

impl Ring {
    fn new(rule: HistoryRule, epoch: Arc<str>) -> Self {
        Self {
            epoch,
            next: 1,
            entries: VecDeque::new(),
            bytes: 0,
            rule,
            last_activity: Instant::now(),
        }
    }

    pub(crate) fn pos(&self) -> HistoryPos {
        HistoryPos {
            epoch: Arc::clone(&self.epoch),
            offset: self.next - 1,
        }
    }

    /// The offset the next message gets.
    pub(crate) fn next_offset(&self) -> u64 {
        self.next
    }

    /// Store a message (already carrying `offset` = [`Ring::next_offset`]).
    pub(crate) fn push(&mut self, frame: OutFrame, bytes: usize) {
        let now = Instant::now();
        self.entries.push_back(Entry {
            offset: self.next,
            at: now,
            frame,
            bytes,
        });
        self.next += 1;
        self.bytes += bytes;
        self.last_activity = now;
        self.prune(now);
    }

    /// Drop what is over the count, byte or age limit.
    fn prune(&mut self, now: Instant) {
        while let Some(e) = self.entries.front() {
            let over = self.entries.len() > self.rule.size
                || self.bytes > self.rule.max_bytes
                || now.duration_since(e.at) > self.rule.ttl;
            if !over {
                break;
            }
            self.bytes -= e.bytes;
            self.entries.pop_front();
        }
    }

    /// Everything after `from`, if the client can resume there: same epoch,
    /// not ahead of us, and nothing after it was dropped.
    pub(crate) fn since(&mut self, from: &Recover) -> Option<Vec<OutFrame>> {
        self.prune(Instant::now());
        let top = self.next - 1;
        let first = self.entries.front().map_or(self.next, |e| e.offset);
        if *from.epoch != *self.epoch || from.offset > top || from.offset + 1 < first {
            return None;
        }
        Some(
            self.entries
                .iter()
                .filter(|e| e.offset > from.offset)
                .map(|e| e.frame.clone())
                .collect(),
        )
    }
}

/// Every history room of one engine.
pub(crate) struct History {
    config: HistoryConfig,
    rings: DashMap<String, Arc<Mutex<Ring>>>,
    epochs: EpochSource,
}

pub(crate) fn lock(ring: &Mutex<Ring>) -> MutexGuard<'_, Ring> {
    // A panic elsewhere cannot leave a ring half-updated (every update is
    // one push/pop pair), so a poisoned lock is still usable.
    ring.lock().unwrap_or_else(PoisonError::into_inner)
}

impl History {
    pub(crate) fn new(config: HistoryConfig) -> Self {
        Self {
            config,
            rings: DashMap::new(),
            epochs: EpochSource::new(),
        }
    }

    /// The ring of `room_key` (`tenant::room`), created if a rule covers
    /// the room; `None` if none does.
    pub(crate) fn ring(&self, room_key: &str) -> Option<Arc<Mutex<Ring>>> {
        if let Some(r) = self.rings.get(room_key) {
            return Some(Arc::clone(&r));
        }
        let (tenant, room) = room_key.split_once("::")?;
        let rule = self
            .config
            .tenants
            .get(tenant)?
            .iter()
            .find(|r| r.rooms.matches(room))?;
        Some(Arc::clone(
            &self.rings.entry(room_key.to_string()).or_insert_with(|| {
                Arc::new(Mutex::new(Ring::new(rule.clone(), self.epochs.next())))
            }),
        ))
    }

    /// Forget rooms that have no local members and nothing kept (their next
    /// message starts a new epoch). A room with members keeps its epoch, so
    /// a quiet room never forces its clients to resync.
    pub(crate) fn sweep(&self, presence: &Presence) {
        let now = Instant::now();
        self.rings.retain(|room_key, ring| {
            let mut r = lock(ring);
            r.prune(now);
            !(r.entries.is_empty()
                && now.duration_since(r.last_activity) > r.rule.ttl
                && presence.member_count(room_key) == 0)
        });
    }
}

/// Epochs: unpredictable per process (a restart must not repeat one) and
/// distinct within it.
struct EpochSource {
    seed: u64,
    n: AtomicU64,
}

impl EpochSource {
    fn new() -> Self {
        let mut h = RandomState::new().build_hasher();
        h.write_u128(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos()),
        );
        Self {
            seed: h.finish(),
            n: AtomicU64::new(0),
        }
    }

    fn next(&self) -> Arc<str> {
        let n = self.n.fetch_add(1, Ordering::Relaxed);
        let mut h = RandomState::new().build_hasher();
        h.write_u64(self.seed);
        h.write_u64(n);
        Arc::from(format!("{:016x}", h.finish()))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn ring(size: usize, ttl_ms: u64, max_bytes: usize) -> Ring {
        Ring::new(
            HistoryRule::new(
                RoomPattern::parse("*"),
                size,
                Duration::from_millis(ttl_ms),
                max_bytes,
            ),
            Arc::from("e1"),
        )
    }

    fn push(r: &mut Ring, s: &str) {
        r.push(OutFrame::text(s.to_string()), s.len());
    }

    fn from(epoch: &str, offset: u64) -> Recover {
        Recover {
            epoch: epoch.into(),
            offset,
        }
    }

    #[test]
    fn resumes_only_without_a_gap() {
        let mut r = ring(3, 60_000, 1 << 20);
        assert_eq!(
            r.since(&from("e1", 0)).unwrap().len(),
            0,
            "empty, at the top"
        );
        for s in ["a", "b", "c", "d"] {
            push(&mut r, s);
        }
        // Kept: 2, 3, 4 (offset 1 dropped by size).
        assert_eq!(r.pos().offset, 4);
        assert_eq!(r.since(&from("e1", 1)).unwrap().len(), 3);
        assert_eq!(r.since(&from("e1", 3)).unwrap().len(), 1);
        assert_eq!(r.since(&from("e1", 4)).unwrap().len(), 0);
        assert!(r.since(&from("e1", 0)).is_none(), "offset 1 is gone");
        assert!(r.since(&from("e1", 5)).is_none(), "ahead of the room");
        assert!(r.since(&from("other", 4)).is_none(), "another epoch");
    }

    #[test]
    fn byte_and_age_limits_drop_the_oldest() {
        let mut r = ring(100, 60_000, 5);
        for s in ["aaa", "bbb", "c"] {
            push(&mut r, s);
        }
        assert_eq!(r.bytes, 4, "aaa dropped to stay within 5 bytes");
        assert!(r.since(&from("e1", 0)).is_none());
        assert_eq!(r.since(&from("e1", 1)).unwrap().len(), 2);

        let mut r = ring(100, 30, 1 << 20);
        push(&mut r, "old");
        std::thread::sleep(Duration::from_millis(60));
        assert!(r.since(&from("e1", 0)).is_none(), "expired");
        assert_eq!(r.since(&from("e1", 1)).unwrap().len(), 0);
    }

    #[test]
    fn epochs_differ() {
        let e = EpochSource::new();
        assert_ne!(e.next(), e.next());
        assert_ne!(EpochSource::new().next(), EpochSource::new().next());
    }
}
