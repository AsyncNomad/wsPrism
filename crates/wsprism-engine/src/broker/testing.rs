//! A queued broker for tests (ADR 0011 B3): one node talking to itself.
//!
//! - **Recording:** keeps every published frame and subscription command, so
//!   tests can check that every room path goes through the broker. In queued
//!   mode, an Ext frame that bypassed the broker would still be wrong in a
//!   cluster but invisible on one node — this is how such a bypass shows up.
//! - **Loopback with real subscription semantics:** a frame comes back only
//!   if its room is subscribed, so a missing `Subscribe` loses messages here
//!   just as it would on a real broker.
//! - **Faults:** delay, temporary failure (`Unavailable`), rejection, and a
//!   pause that holds every publish (to fill the outbox).
//!
//! Enabled by the crate feature `testing` (this repository's tests only).

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use tokio::sync::{mpsc, watch};

use super::{
    wire, Broker, BrokerError, BrokerFrame, BrokerMode, HistoryRead, Incoming, Keep, Subscription,
};
use crate::realtime::core::Recover;

/// One room's history, kept like the Redis broker keeps it: offsets from
/// 1, an epoch, and the count / byte / age limits of the room's rule.
struct Kept {
    epoch: Arc<str>,
    top: u64,
    entries: std::collections::VecDeque<(std::time::Instant, BrokerFrame, usize)>,
    bytes: usize,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// See the module docs.
pub struct TestBroker {
    slot: u16,
    tx: mpsc::Sender<Incoming>,
    rx: Mutex<Option<mpsc::Receiver<Incoming>>>,
    published: Mutex<Vec<BrokerFrame>>,
    commands: Mutex<Vec<Subscription>>,
    subscribed: Mutex<HashSet<Arc<str>>>,
    /// Subscribe for a subscribed room, or Unsubscribe for one that isn't.
    anomalies: AtomicUsize,
    fail_next: Mutex<usize>,
    reject_next: Mutex<usize>,
    publish_delay_us: AtomicU64,
    subscription_delay_us: AtomicU64,
    open: watch::Sender<bool>,
    history: Mutex<std::collections::HashMap<Arc<str>, Kept>>,
    kv: Mutex<std::collections::HashMap<String, (bytes::Bytes, std::time::Instant)>>,
    /// The session table.
    sessions: Mutex<std::collections::HashMap<Arc<str>, Vec<TableEntry>>>,
    /// The membership: room -> session key -> member.
    members: Mutex<std::collections::HashMap<Arc<str>, Vec<super::RoomMember>>>,
    /// What the next `reap_dead` returns (see [`TestBroker::dead_member`]).
    dead: Mutex<Vec<super::DeadMember>>,
    arrivals: AtomicU64,
    epochs: std::sync::atomic::AtomicU64,
}

impl TestBroker {
    /// A broker for node `slot`.
    pub fn new(slot: u16) -> Arc<Self> {
        let (tx, rx) = mpsc::channel(4096);
        Arc::new(Self {
            slot,
            tx,
            rx: Mutex::new(Some(rx)),
            published: Mutex::default(),
            commands: Mutex::default(),
            subscribed: Mutex::default(),
            anomalies: AtomicUsize::new(0),
            fail_next: Mutex::new(0),
            reject_next: Mutex::new(0),
            publish_delay_us: AtomicU64::new(0),
            subscription_delay_us: AtomicU64::new(0),
            open: watch::Sender::new(true),
            history: Mutex::default(),
            kv: Mutex::default(),
            sessions: Mutex::default(),
            members: Mutex::default(),
            dead: Mutex::default(),
            arrivals: AtomicU64::new(0),
            epochs: AtomicU64::new(1),
        })
    }

    /// Every frame published so far.
    pub fn published(&self) -> Vec<BrokerFrame> {
        lock(&self.published).clone()
    }

    /// Frames published so far, clearing the record.
    pub fn take_published(&self) -> Vec<BrokerFrame> {
        std::mem::take(&mut *lock(&self.published))
    }

    /// Forget every room's history (as if Redis lost its data): the next
    /// message starts a new epoch.
    pub fn lose_history(&self) {
        lock(&self.history).clear();
    }

    fn kept<'a>(
        &self,
        map: &'a mut std::collections::HashMap<Arc<str>, Kept>,
        room: &Arc<str>,
    ) -> &'a mut Kept {
        map.entry(Arc::clone(room)).or_insert_with(|| Kept {
            epoch: Arc::from(format!(
                "test-{}",
                self.epochs.fetch_add(1, Ordering::SeqCst)
            )),
            top: 0,
            entries: Default::default(),
            bytes: 0,
        })
    }

    /// Give a kept frame its offset and store it (what the Redis script does).
    fn keep(&self, f: &BrokerFrame, keep: &Keep) -> BrokerFrame {
        let mut map = lock(&self.history);
        let k = self.kept(&mut map, &f.room_key);
        k.top += 1;
        let mut g = f.clone();
        g.offset = Some(k.top);
        g.keep = None;
        let size = match &g.body {
            crate::spi::OutFrame::Text(t) => t.as_bytes().len(),
            _ => 0,
        };
        let now = std::time::Instant::now();
        k.entries.push_back((now, g.clone(), size));
        k.bytes += size;
        while let Some((at, _, n)) = k.entries.front() {
            if k.entries.len() > keep.size
                || k.bytes > keep.max_bytes
                || now.duration_since(*at) > keep.ttl
            {
                k.bytes -= *n;
                k.entries.pop_front();
            } else {
                break;
            }
        }
        g
    }

    /// Every subscription command applied so far, in order.
    pub fn commands(&self) -> Vec<Subscription> {
        lock(&self.commands).clone()
    }

    /// Rooms currently subscribed.
    pub fn subscribed(&self) -> HashSet<String> {
        lock(&self.subscribed)
            .iter()
            .map(|r| r.to_string())
            .collect()
    }

    /// Commands that did not fit the state (double subscribe, unsubscribe of
    /// an unsubscribed room). Always 0 if the engine keeps its order.
    pub fn anomalies(&self) -> usize {
        self.anomalies.load(Ordering::SeqCst)
    }

    /// The next `n` publish calls fail with `Unavailable` (retried).
    pub fn fail_next(&self, n: usize) {
        *lock(&self.fail_next) = n;
    }

    /// The next `n` publish calls are rejected (dropped).
    pub fn reject_next(&self, n: usize) {
        *lock(&self.reject_next) = n;
    }

    /// Delay every publish call.
    pub fn set_publish_delay(&self, d: Duration) {
        self.publish_delay_us
            .store(d.as_micros() as u64, Ordering::SeqCst);
    }

    /// Delay every subscription command.
    pub fn set_subscription_delay(&self, d: Duration) {
        self.subscription_delay_us
            .store(d.as_micros() as u64, Ordering::SeqCst);
    }

    /// Hold every publish call until [`TestBroker::resume`].
    pub fn pause(&self) {
        self.open.send_replace(false);
    }

    /// Let held publish calls through.
    pub fn resume(&self) {
        self.open.send_replace(true);
    }

    /// Pretend a node died with this member: the next reap returns it.
    pub fn dead_member(&self, room_key: &str, user: &str, member_id: u32) {
        lock(&self.dead).push(super::DeadMember::new(
            Arc::from(room_key),
            Arc::from(user),
            member_id,
            true,
        ));
    }

    /// The membership of `room_key` as the broker has it.
    pub fn room_members(&self, room_key: &str) -> Vec<String> {
        let mut v: Vec<String> = lock(&self.members)
            .get(room_key)
            .map(|ms| ms.iter().map(|m| m.user.to_string()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    /// Deliver `frame` as if another node had published it.
    pub async fn inject(&self, frame: BrokerFrame) {
        let _ = self.tx.send(Incoming::Frame(frame)).await;
    }

    /// Report a subscription gap for `rooms` (internal keys).
    pub async fn gap(&self, rooms: &[&str]) {
        let rooms = rooms.iter().map(|r| Arc::from(*r)).collect();
        let _ = self.tx.send(Incoming::Gap(rooms)).await;
    }

    fn take_one(counter: &Mutex<usize>) -> bool {
        let mut n = lock(counter);
        let hit = *n > 0;
        *n = n.saturating_sub(1);
        hit
    }
}

async fn delay(us: &AtomicU64) {
    let us = us.load(Ordering::SeqCst);
    if us > 0 {
        tokio::time::sleep(Duration::from_micros(us)).await;
    }
}

#[async_trait]
impl Broker for TestBroker {
    fn mode(&self) -> BrokerMode {
        BrokerMode::Queued
    }

    async fn start(&self) -> Result<u16, BrokerError> {
        Ok(self.slot)
    }

    async fn publish(&self, batch: &[BrokerFrame]) -> Result<(), BrokerError> {
        let mut open = self.open.subscribe();
        let _ = open.wait_for(|open| *open).await;
        delay(&self.publish_delay_us).await;
        if Self::take_one(&self.fail_next) {
            return Err(BrokerError::Unavailable("injected".into()));
        }
        if Self::take_one(&self.reject_next) {
            return Err(BrokerError::Rejected("injected".into()));
        }
        lock(&self.published).extend(batch.iter().cloned());
        for f in batch {
            let kept;
            let f = match &f.keep {
                Some(keep) => {
                    kept = self.keep(f, keep);
                    &kept
                }
                None => f,
            };
            if lock(&self.subscribed).contains(&f.room_key) {
                // Through the wire format, as a real broker would.
                let Some(bytes) = wire::encode(f) else {
                    continue;
                };
                let back =
                    wire::decode(&bytes).map_err(|e| BrokerError::Rejected(e.to_string()))?;
                let _ = self.tx.send(Incoming::Frame(back)).await;
            }
        }
        Ok(())
    }

    async fn subscription(&self, cmd: &Subscription) -> Result<(), BrokerError> {
        delay(&self.subscription_delay_us).await;
        let fits = {
            let mut subs = lock(&self.subscribed);
            match cmd {
                Subscription::Subscribe(r) => subs.insert(Arc::clone(r)),
                Subscription::Unsubscribe(r) => subs.remove(r),
            }
        };
        if !fits {
            self.anomalies.fetch_add(1, Ordering::SeqCst);
        }
        lock(&self.commands).push(cmd.clone());
        Ok(())
    }

    async fn history(
        &self,
        room_key: &str,
        keep: &Keep,
        from: Option<&Recover>,
    ) -> Result<HistoryRead, BrokerError> {
        let mut map = lock(&self.history);
        let k = self.kept(&mut map, &Arc::from(room_key));
        let now = std::time::Instant::now();
        while let Some((at, _, n)) = k.entries.front() {
            if now.duration_since(*at) > keep.ttl {
                k.bytes -= *n;
                k.entries.pop_front();
            } else {
                break;
            }
        }
        let replay = from.and_then(|from| {
            let first = k
                .entries
                .front()
                .and_then(|(_, f, _)| f.offset)
                .unwrap_or(k.top + 1);
            (*from.epoch == *k.epoch && from.offset <= k.top && from.offset + 1 >= first).then(
                || {
                    k.entries
                        .iter()
                        .filter(|(_, f, _)| f.offset.is_some_and(|o| o > from.offset))
                        .map(|(_, f, _)| f.clone())
                        .collect()
                },
            )
        });
        Ok(HistoryRead::new(Arc::clone(&k.epoch), k.top, replay))
    }

    fn incoming(&self) -> Option<mpsc::Receiver<Incoming>> {
        lock(&self.rx).take()
    }

    fn kv(&self) -> Option<&dyn super::SharedKv> {
        Some(self)
    }

    fn sessions(&self) -> Option<&dyn super::SharedSessions> {
        Some(self)
    }

    fn presence(&self) -> Option<&dyn super::SharedPresence> {
        Some(self)
    }
}

#[async_trait]
impl super::SharedPresence for TestBroker {
    async fn apply(&self, ops: &[super::PresenceOp]) -> Result<(), BrokerError> {
        let mut all = lock(&self.members);
        for op in ops {
            match op {
                super::PresenceOp::Add {
                    room_key,
                    session_key,
                    user,
                    member_id,
                    ..
                } => {
                    let ms = all.entry(Arc::clone(room_key)).or_default();
                    ms.retain(|m| m.session_key != *session_key);
                    ms.push(super::RoomMember::new(
                        Arc::clone(session_key),
                        Arc::clone(user),
                        *member_id,
                    ));
                }
                super::PresenceOp::Remove {
                    room_key,
                    session_key,
                } => {
                    if let Some(ms) = all.get_mut(room_key) {
                        ms.retain(|m| m.session_key != *session_key);
                        if ms.is_empty() {
                            all.remove(room_key);
                        }
                    }
                }
            }
        }
        Ok(())
    }

    async fn members(&self, room_key: &str) -> Result<Vec<super::RoomMember>, BrokerError> {
        Ok(lock(&self.members)
            .get(room_key)
            .cloned()
            .unwrap_or_default())
    }

    async fn reap_dead(&self) -> Result<Vec<super::DeadMember>, BrokerError> {
        Ok(std::mem::take(&mut *lock(&self.dead)))
    }
}

/// One session in the table: key, sid, order of arrival.
type TableEntry = (Arc<str>, Arc<str>, u64);

/// The session table with the Redis script's rules, on one node (every
/// session is this node's).
#[async_trait]
impl super::SharedSessions for TestBroker {
    async fn claim(&self, c: &super::SessionClaim) -> Result<super::Claimed, BrokerError> {
        let mut table = lock(&self.sessions);
        let entries = table.entry(Arc::clone(&c.user_key)).or_default();
        let mut evicted = Vec::new();
        entries.retain(|(key, sid, _)| {
            let same = *sid == c.sid;
            if same {
                evicted.push(super::Evicted::new(self.slot, Arc::clone(key), "replaced"));
            }
            !same
        });
        if entries.len() >= c.max_per_user as usize {
            match c.on_full {
                super::OnFull::KickOldest => {
                    if let Some(i) = (0..entries.len()).min_by_key(|&i| entries[i].2) {
                        let (key, _, _) = entries.remove(i);
                        evicted.push(super::Evicted::new(self.slot, key, "max_sessions_exceeded"));
                    }
                }
                _ => return Ok(super::Claimed::new(false, evicted)),
            }
        }
        let n = self.arrivals.fetch_add(1, Ordering::SeqCst);
        entries.push((Arc::clone(&c.session_key), Arc::clone(&c.sid), n));
        Ok(super::Claimed::new(true, evicted))
    }

    async fn release(&self, user_key: &str, session_key: &str) -> Result<(), BrokerError> {
        let mut table = lock(&self.sessions);
        if let Some(entries) = table.get_mut(user_key) {
            entries.retain(|(key, _, _)| &**key != session_key);
            if entries.is_empty() {
                table.remove(user_key);
            }
        }
        Ok(())
    }

    async fn of_user(&self, user_key: &str) -> Result<Vec<super::SessionEntry>, BrokerError> {
        Ok(lock(&self.sessions)
            .get(user_key)
            .map(|entries| {
                entries
                    .iter()
                    .map(|(key, sid, _)| {
                        super::SessionEntry::new(self.slot, Arc::clone(key), Arc::clone(sid))
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn send_control(&self, node: u16, control: &super::Control) -> Result<(), BrokerError> {
        if node == self.slot {
            let _ = self.tx.send(Incoming::Control(control.clone())).await;
        }
        Ok(())
    }
}

#[async_trait]
impl super::SharedKv for TestBroker {
    async fn put_once(
        &self,
        key: &str,
        value: bytes::Bytes,
        ttl: Duration,
    ) -> Result<bool, BrokerError> {
        let mut kv = lock(&self.kv);
        let now = std::time::Instant::now();
        if kv.get(key).is_some_and(|(_, until)| *until > now) {
            return Ok(false);
        }
        kv.insert(key.to_string(), (value, now + ttl));
        Ok(true)
    }

    async fn take(&self, key: &str) -> Result<Option<bytes::Bytes>, BrokerError> {
        let now = std::time::Instant::now();
        Ok(lock(&self.kv)
            .remove(key)
            .filter(|(_, until)| *until > now)
            .map(|(v, _)| v))
    }
}
