//! Per-session Hot slot: the latest frame per `(sender, svc, key)` (T0-3;
//! hot-lane design §4.2, performance-architecture §3.2).
//!
//! Keyed Hot frames are state: only the newest value for a key matters. A
//! FIFO queue would deliver every stale value a slow reader missed; the slot
//! overwrites instead, so a reader that falls behind gets the current state
//! as soon as it catches up. The key includes the sender: two players both
//! using `key = 0` ("my position") must not overwrite each other.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use tokio::sync::Notify;

use crate::realtime::types::HotOut;

/// `(sender member id, svc id, key)`.
pub type SlotKey = (u32, u8, u32);

/// Most distinct keys a session's slot holds (room members x keys). A new
/// key beyond it is dropped; overwriting an existing key is always allowed.
pub const DEFAULT_SLOT_CAP: usize = 1024;

/// What `put` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Put {
    /// A new key was stored.
    Inserted,
    /// Replaced a value the session had not sent yet (normal: latest wins).
    Overwritten,
    /// New key while the slot is at capacity: dropped.
    Full,
}

/// The latest pending Hot frame per `(sender, svc, key)` for one session.
#[derive(Debug)]
pub struct HotSlot {
    latest: Mutex<HashMap<SlotKey, Arc<HotOut>>>,
    /// Set by the first `put` after a drain; cleared by the session before
    /// it drains. Only the false -> true transition wakes the session.
    scheduled: AtomicBool,
    ready: Notify,
    cap: usize,
}

impl HotSlot {
    /// A slot holding at most `cap` distinct keys.
    pub fn new(cap: usize) -> Self {
        Self {
            latest: Mutex::new(HashMap::new()),
            scheduled: AtomicBool::new(false),
            ready: Notify::new(),
            cap,
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<SlotKey, Arc<HotOut>>> {
        // Plain data; a poisoned lock is still usable.
        self.latest.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Store `frame` as the latest value for `key` (publisher side; never
    /// blocks on the session).
    pub fn put(&self, key: SlotKey, frame: Arc<HotOut>) -> Put {
        let result = {
            let mut latest = self.lock();
            if latest.len() >= self.cap && !latest.contains_key(&key) {
                return Put::Full;
            }
            match latest.insert(key, frame) {
                Some(_) => Put::Overwritten,
                None => Put::Inserted,
            }
        };
        if !self.scheduled.swap(true, Ordering::AcqRel) {
            // A permit is stored if the session isn't waiting right now.
            self.ready.notify_one();
        }
        result
    }

    /// Resolves when frames may be waiting (session side).
    pub async fn ready(&self) {
        self.ready.notified().await;
    }

    /// Move every pending frame into `out` (session side). Clears the
    /// schedule flag first, so a `put` racing with the drain either lands in
    /// this drain or wakes the session again — never neither.
    pub fn drain_into(&self, out: &mut Vec<Arc<HotOut>>) {
        self.scheduled.store(false, Ordering::Release);
        out.extend(self.lock().drain().map(|(_, v)| v));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use bytes::Bytes;

    fn frame(tag: &'static [u8]) -> Arc<HotOut> {
        Arc::new(HotOut {
            v1: Bytes::from_static(tag),
            v2: Bytes::from_static(tag),
            slot_key: None,
        })
    }

    fn drained(s: &HotSlot) -> Vec<Bytes> {
        let mut out = Vec::new();
        s.drain_into(&mut out);
        let mut v: Vec<Bytes> = out.iter().map(|f| f.v1.clone()).collect();
        v.sort();
        v
    }

    #[test]
    fn latest_value_per_key_wins() {
        let s = HotSlot::new(8);
        assert_eq!(s.put((1, 1, 0), frame(b"a1")), Put::Inserted);
        assert_eq!(s.put((1, 1, 0), frame(b"a2")), Put::Overwritten);
        assert_eq!(drained(&s), vec![Bytes::from_static(b"a2")]);
        assert!(drained(&s).is_empty(), "drain empties the slot");
    }

    #[test]
    fn same_key_from_different_senders_does_not_collide() {
        let s = HotSlot::new(8);
        s.put((1, 1, 0), frame(b"p1"));
        s.put((2, 1, 0), frame(b"p2"));
        assert_eq!(
            drained(&s),
            vec![Bytes::from_static(b"p1"), Bytes::from_static(b"p2")]
        );
    }

    #[test]
    fn new_keys_beyond_capacity_are_dropped_but_overwrites_are_not() {
        let s = HotSlot::new(2);
        s.put((1, 1, 0), frame(b"a"));
        s.put((1, 1, 1), frame(b"b"));
        assert_eq!(s.put((1, 1, 2), frame(b"c")), Put::Full);
        assert_eq!(s.put((1, 1, 1), frame(b"b2")), Put::Overwritten);
        assert_eq!(
            drained(&s),
            vec![Bytes::from_static(b"a"), Bytes::from_static(b"b2")]
        );
    }

    #[tokio::test]
    async fn a_put_wakes_the_session_once_until_it_drains() {
        let s = HotSlot::new(8);
        s.put((1, 1, 0), frame(b"x"));
        s.put((1, 1, 1), frame(b"y")); // no second wake needed
        tokio::time::timeout(std::time::Duration::from_millis(100), s.ready())
            .await
            .expect("first put wakes the session");
        assert_eq!(drained(&s).len(), 2);
        // After a drain, the next put wakes it again.
        s.put((1, 1, 0), frame(b"z"));
        tokio::time::timeout(std::time::Duration::from_millis(100), s.ready())
            .await
            .expect("put after drain wakes again");
    }
}
