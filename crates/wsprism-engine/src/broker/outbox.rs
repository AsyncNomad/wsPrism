//! The engine's queue in front of a queued broker (ADR 0011 B4): bounded,
//! with a share per tenant so one tenant's burst cannot block the others'
//! publishes, drained by one task.
//!
//! One FIFO per tenant, drained in turn (0.2): a tenant's items keep their
//! order where it matters (see `run_outbox`: subscriptions go out together,
//! ahead of a later frame of their room), and a
//! light tenant's items no longer wait behind a flooding tenant's backlog
//! (measured: BENCHMARKS "tenant fairness"). The wait limit is per tenant
//! too, so only the tenant that falls behind is refused.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use tokio::sync::Notify;

use super::{Broker, BrokerError, BrokerFrame, Subscription};

/// Outbox bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct OutboxLimits {
    /// Frames queued in total.
    pub capacity: usize,
    /// Frames one tenant may have queued.
    pub per_tenant: usize,
    /// Refuse new frames while the oldest queued one has waited this long:
    /// past saturation, fail fast (`UNAVAILABLE`) instead of letting
    /// latency grow toward a full queue (hundreds of ms, ADR 0011 A15).
    pub max_wait: Duration,
}

impl Default for OutboxLimits {
    /// 65,536 frames, a quarter of them per tenant, 1 s of waiting.
    fn default() -> Self {
        Self {
            capacity: 65_536,
            per_tenant: 16_384,
            max_wait: Duration::from_secs(1),
        }
    }
}

impl OutboxLimits {
    /// `capacity` frames in total, `per_tenant` for any one tenant.
    pub fn new(capacity: usize, per_tenant: usize) -> Self {
        Self {
            capacity,
            per_tenant: per_tenant.min(capacity),
            ..Self::default()
        }
    }

    /// The longest a frame may wait before new ones are refused.
    pub fn max_wait(mut self, max_wait: Duration) -> Self {
        self.max_wait = max_wait;
        self
    }
}

/// What happened to queued traffic (for metrics).
#[derive(Debug, Default)]
pub struct OutboxCounters {
    refused: AtomicU64,
    retries: AtomicU64,
    failed: AtomicU64,
}

impl OutboxCounters {
    /// Frames not queued because the outbox or the tenant's share was full
    /// (Reliable publishers got `UNAVAILABLE`; Lossy ones were dropped).
    pub fn refused(&self) -> u64 {
        self.refused.load(Ordering::Relaxed)
    }
    /// Broker calls retried after `Unavailable`.
    pub fn retries(&self) -> u64 {
        self.retries.load(Ordering::Relaxed)
    }
    /// Frames and commands dropped because the broker rejected them.
    pub fn failed(&self) -> u64 {
        self.failed.load(Ordering::Relaxed)
    }
}

/// Set to `true` once a `Subscribe` has been applied by the broker.
pub(crate) type Subscribed = Arc<tokio::sync::watch::Sender<bool>>;

enum Item {
    /// A frame and when it was queued.
    Frame(BrokerFrame, std::time::Instant),
    Sub(Subscription, Option<Subscribed>),
    Presence(super::PresenceOp),
}

impl Item {
    /// The tenant whose queue the item goes to (room keys are
    /// `tenant::room`).
    fn tenant(&self) -> Arc<str> {
        let room_key = match self {
            Item::Frame(f, _) => return Arc::clone(&f.tenant),
            Item::Sub(Subscription::Subscribe(r) | Subscription::Unsubscribe(r), _) => r,
            Item::Presence(super::PresenceOp::Add { room_key, .. })
            | Item::Presence(super::PresenceOp::Remove { room_key, .. }) => room_key,
        };
        Arc::from(room_key.split_once("::").map_or("", |(t, _)| t))
    }
}

/// One tenant's items, in order.
#[derive(Default)]
struct TenantQueue {
    items: VecDeque<Item>,
    /// Frames among `items` (counted against the tenant's share).
    frames: usize,
}

#[derive(Default)]
struct Queue {
    tenants: HashMap<Arc<str>, TenantQueue>,
    /// Tenants with items, in drain order.
    turn: VecDeque<Arc<str>>,
    /// Items and frames over all tenants.
    items: usize,
    frames: usize,
}

impl Queue {
    fn push(&mut self, tenant: Arc<str>, item: Item) {
        let q = self.tenants.entry(Arc::clone(&tenant)).or_default();
        if q.items.is_empty() {
            self.turn.push_back(tenant);
        }
        if matches!(item, Item::Frame(..)) {
            q.frames += 1;
            self.frames += 1;
        }
        q.items.push_back(item);
        self.items += 1;
    }
}

/// Items taken from one tenant before moving to the next.
const TURN: usize = 256;

/// See the module docs.
pub(crate) struct BrokerOutbox {
    queue: Mutex<Queue>,
    ready: Notify,
    /// Items the drain task took and has not finished with.
    busy: AtomicUsize,
    limits: OutboxLimits,
    counters: OutboxCounters,
}

/// The outbox is full (or the tenant's share is).
pub(crate) struct Full;

impl BrokerOutbox {
    pub(crate) fn new(limits: OutboxLimits) -> Self {
        Self {
            queue: Mutex::default(),
            ready: Notify::new(),
            busy: AtomicUsize::new(0),
            limits,
            counters: OutboxCounters::default(),
        }
    }

    pub(crate) fn counters(&self) -> &OutboxCounters {
        &self.counters
    }

    /// Never held across an await; a panic elsewhere cannot leave the queue
    /// half-updated, so a poisoned lock is still usable.
    fn lock(&self) -> MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Queue a frame unless the outbox or its tenant's share is full, or
    /// the tenant's oldest queued frame has waited too long.
    pub(crate) fn push_frame(&self, frame: BrokerFrame) -> Result<(), Full> {
        {
            let mut q = self.lock();
            let now = std::time::Instant::now();
            let (used, stale) = q.tenants.get(&frame.tenant).map_or((0, false), |t| {
                let stale = t.items.iter().find_map(|i| match i {
                    Item::Frame(_, at) => Some(now.duration_since(*at) > self.limits.max_wait),
                    _ => None,
                });
                (t.frames, stale.unwrap_or(false))
            });
            if q.frames >= self.limits.capacity || used >= self.limits.per_tenant || stale {
                drop(q);
                self.counters.refused.fetch_add(1, Ordering::Relaxed);
                return Err(Full);
            }
            let tenant = Arc::clone(&frame.tenant);
            q.push(tenant, Item::Frame(frame, now));
        }
        self.ready.notify_one();
        Ok(())
    }

    /// Queue a subscription change. Never refused: there is at most one per
    /// room creation or removal, and losing one would leave the node deaf to
    /// (or flooded by) a room.
    pub(crate) fn push_sub(&self, cmd: Subscription, done: Option<Subscribed>) {
        let item = Item::Sub(cmd, done);
        let tenant = item.tenant();
        self.lock().push(tenant, item);
        self.ready.notify_one();
    }

    /// Queue a membership change (never refused, like subscriptions).
    pub(crate) fn push_presence(&self, op: super::PresenceOp) {
        let item = Item::Presence(op);
        let tenant = item.tenant();
        self.lock().push(tenant, item);
        self.ready.notify_one();
    }

    /// Items queued or being handed to the broker (0 = everything accepted
    /// so far has been applied or rejected).
    pub(crate) fn pending(&self) -> usize {
        // Read under the lock `take` holds while it moves items to `busy`,
        // so an item is never counted in neither.
        let q = self.lock();
        q.items + self.busy.load(Ordering::Acquire)
    }

    /// Move up to `max` items into `out`, freeing their quota: up to `TURN`
    /// from each tenant in turn (each tenant's items in order), round after
    /// round, so a lone tenant still fills a whole batch.
    fn take(&self, out: &mut Vec<Item>, max: usize) {
        let mut guard = self.lock();
        let q = &mut *guard;
        let mut taken = 0;
        while taken < max {
            let Some(tenant) = q.turn.pop_front() else {
                break;
            };
            let Some(tq) = q.tenants.get_mut(&tenant) else {
                continue;
            };
            let n = TURN.min(max - taken).min(tq.items.len());
            for item in tq.items.drain(..n) {
                if matches!(item, Item::Frame(..)) {
                    tq.frames -= 1;
                    q.frames -= 1;
                }
                out.push(item);
            }
            taken += n;
            if tq.items.is_empty() {
                q.tenants.remove(&tenant);
            } else {
                q.turn.push_back(tenant);
            }
        }
        q.items -= taken;
        self.busy.store(taken, Ordering::Release);
    }
}

/// Items handed to the broker per wake-up (one `publish` per run of frames).
/// One batch is in flight at a time, so this sets the outbox throughput: at
/// 256 it capped at ~150k publishes/s on loopback Redis; at 1024 a 240k/s
/// load sat at the limit (delivery 0.66-1.0); 4096 keeps up (ADR 0011 A15,
/// M5). A batch is only as large as what queued meanwhile, so light loads
/// still go out at once.
const BATCH: usize = 4096;
const RETRY_FIRST: Duration = Duration::from_millis(50);
const RETRY_MAX: Duration = Duration::from_secs(2);

/// The outbox's drain task: hands frames and commands to the broker in
/// order. Runs until aborted.
///
/// What is kept in order is each room's items: its frames, membership
/// changes and subscriptions go out as queued. Frames and membership
/// changes otherwise keep their queue order across rooms (a kind switch
/// flushes the other kind). Subscriptions go out together at the end of a
/// batch, and the items of their rooms queued after them wait until then
/// (this node's Ext frames reach its members through its subscription), while
/// other rooms' items go on: so frames of different rooms may leave in a
/// different order than queued.
///
/// Before (0.2): every subscription was a barrier (flush, then one
/// SUBSCRIBE and its confirmation), and a join's `member_joined` frame
/// another; a node joined at most ~3,600 new rooms/s however idle Redis
/// was (~2,400 with presence events; BENCHMARKS "session churn").
pub(crate) async fn run_outbox(outbox: Arc<BrokerOutbox>, broker: Arc<dyn Broker>) {
    let mut items = Vec::with_capacity(BATCH);
    let mut runs = Runs::default();
    let mut subs = Subs::default();
    // Items of rooms with a subscription in `subs`, queued after it.
    let mut waiting = Vec::new();
    loop {
        outbox.take(&mut items, BATCH);
        if items.is_empty() {
            outbox.ready.notified().await;
            continue;
        }
        for item in items.drain(..) {
            match item {
                Item::Sub(cmd, done) => subs.push(cmd, done),
                item if subs.has(item_room(&item)) => waiting.push(item),
                item => runs.push(item, &outbox, &*broker).await,
            }
        }
        runs.flush(&outbox, &*broker).await;
        subs.flush(&outbox, &*broker).await;
        for item in waiting.drain(..) {
            runs.push(item, &outbox, &*broker).await;
        }
        runs.flush(&outbox, &*broker).await;
        outbox.busy.store(0, Ordering::Release);
    }
}

/// Frames and membership changes waiting to go out, one call per kind.
///
/// A room's frames and membership changes keep their order (a
/// `member_joined` frame after the membership that it announces). Both kinds
/// collect until a room needs the opposite order from one already
/// promised: membership changes first for one room (its frame came after
/// them) and frames first for another, or for the same room again (a
/// join's frame, then its leave). Only then do they go out, in the order
/// promised so far. Other rooms' items never force a round trip.
/// Before (0.2): any switch of kinds flushed, so with presence events
/// (membership change, frame, membership change, ...) every join and leave
/// cost a Redis round trip; BENCHMARKS "session churn".
#[derive(Default)]
struct Runs {
    frames: Vec<BrokerFrame>,
    /// Rooms in `frames`, built only once a membership change arrives (a
    /// publish-only load never pays for it).
    frame_rooms: Option<HashSet<Arc<str>>>,
    ops: Vec<super::PresenceOp>,
    op_rooms: HashSet<Arc<str>>,
    /// The order some room needs: `Some(true)` = membership changes before
    /// frames, `Some(false)` = frames first, `None` = no room needs one.
    ops_first: Option<bool>,
}

impl Runs {
    async fn push(&mut self, item: Item, outbox: &BrokerOutbox, broker: &dyn Broker) {
        match item {
            Item::Frame(f, _) => {
                if !self.ops.is_empty() && self.op_rooms.contains(&f.room_key) {
                    self.need(true, outbox, broker).await;
                }
                if let Some(rooms) = &mut self.frame_rooms {
                    rooms.insert(Arc::clone(&f.room_key));
                }
                self.frames.push(f);
            }
            Item::Presence(op) => {
                let room = op_room(&op);
                if !self.frames.is_empty() {
                    let frames = &self.frames;
                    let rooms = self.frame_rooms.get_or_insert_with(|| {
                        frames.iter().map(|f| Arc::clone(&f.room_key)).collect()
                    });
                    if rooms.contains(room) {
                        self.need(false, outbox, broker).await;
                    }
                }
                self.op_rooms.insert(Arc::clone(room));
                self.ops.push(op);
            }
            Item::Sub(cmd, done) => {
                // Not reached: the drain loop collects subscriptions.
                tracing::error!(?cmd, "outbox: a subscription in a frame run");
                drop(done);
            }
        }
    }

    /// A room needs `ops_first`: promise it, or, if the opposite was
    /// promised, send what is waiting in that order first.
    async fn need(&mut self, ops_first: bool, outbox: &BrokerOutbox, broker: &dyn Broker) {
        match self.ops_first {
            Some(promised) if promised != ops_first => self.flush(outbox, broker).await,
            _ => self.ops_first = Some(ops_first),
        }
    }

    /// Both kinds, in the promised order.
    async fn flush(&mut self, outbox: &BrokerOutbox, broker: &dyn Broker) {
        if self.ops_first == Some(false) {
            self.publish(outbox, broker).await;
            self.apply_presence(outbox, broker).await;
        } else {
            self.apply_presence(outbox, broker).await;
            self.publish(outbox, broker).await;
        }
        self.ops_first = None;
    }

    async fn publish(&mut self, outbox: &BrokerOutbox, broker: &dyn Broker) {
        publish(outbox, broker, &mut self.frames).await;
        self.frame_rooms = None;
    }

    async fn apply_presence(&mut self, outbox: &BrokerOutbox, broker: &dyn Broker) {
        apply_presence(outbox, broker, &mut self.ops).await;
        self.op_rooms.clear();
    }
}

fn op_room(op: &super::PresenceOp) -> &Arc<str> {
    match op {
        super::PresenceOp::Add { room_key, .. } | super::PresenceOp::Remove { room_key, .. } => {
            room_key
        }
    }
}

fn item_room(item: &Item) -> &Arc<str> {
    match item {
        Item::Frame(f, _) => &f.room_key,
        Item::Presence(super::PresenceOp::Add { room_key, .. })
        | Item::Presence(super::PresenceOp::Remove { room_key, .. }) => room_key,
        Item::Sub(Subscription::Subscribe(r) | Subscription::Unsubscribe(r), _) => r,
    }
}

/// Subscription changes waiting to go out together (see [`run_outbox`]).
#[derive(Default)]
struct Subs {
    cmds: Vec<Subscription>,
    done: Vec<Subscribed>,
    rooms: HashSet<Arc<str>>,
}

impl Subs {
    fn has(&self, room: &Arc<str>) -> bool {
        !self.cmds.is_empty() && self.rooms.contains(room)
    }

    fn push(&mut self, cmd: Subscription, done: Option<Subscribed>) {
        let room = match &cmd {
            Subscription::Subscribe(r) | Subscription::Unsubscribe(r) => Arc::clone(r),
        };
        self.rooms.insert(room);
        self.cmds.push(cmd);
        self.done.extend(done);
    }

    /// Apply them all (frames and membership changes queued before them
    /// went out already), then tell the joins.
    async fn flush(&mut self, outbox: &BrokerOutbox, broker: &dyn Broker) {
        if self.cmds.is_empty() {
            return;
        }
        retry(outbox, self.cmds.len(), || broker.subscriptions(&self.cmds)).await;
        for done in self.done.drain(..) {
            done.send_replace(true);
        }
        self.cmds.clear();
        self.rooms.clear();
    }
}

async fn publish(outbox: &BrokerOutbox, broker: &dyn Broker, frames: &mut Vec<BrokerFrame>) {
    if frames.is_empty() {
        return;
    }
    retry(outbox, frames.len(), || broker.publish(frames)).await;
    frames.clear();
}

async fn apply_presence(
    outbox: &BrokerOutbox,
    broker: &dyn Broker,
    ops: &mut Vec<super::PresenceOp>,
) {
    if ops.is_empty() {
        return;
    }
    if let Some(p) = broker.presence() {
        retry(outbox, ops.len(), || p.apply(ops)).await;
    }
    ops.clear();
}

/// Call `f` until it succeeds or is rejected (`n` = items it carries).
async fn retry<'a, F, Fut>(outbox: &BrokerOutbox, n: usize, mut f: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), BrokerError>> + 'a,
{
    let mut wait = RETRY_FIRST;
    loop {
        match f().await {
            Ok(()) => return,
            Err(BrokerError::Rejected(e)) => {
                outbox
                    .counters
                    .failed
                    .fetch_add(n as u64, Ordering::Relaxed);
                tracing::warn!(items = n, "broker rejected: {e}");
                return;
            }
            Err(e) => {
                let r = outbox.counters.retries.fetch_add(1, Ordering::Relaxed);
                if r.is_multiple_of(64) {
                    tracing::warn!(items = n, retry_in_ms = wait.as_millis() as u64, "{e}");
                }
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(RETRY_MAX);
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::realtime::types::{OutFrame, QoS};

    fn frame(tenant: &str, n: u64) -> BrokerFrame {
        let mut f = BrokerFrame::new(
            0,
            &format!("{tenant}::r"),
            QoS::Reliable,
            OutFrame::text("x".to_string()),
        );
        f.seq = n;
        f
    }

    fn seqs(items: &[Item]) -> Vec<(String, u64)> {
        items
            .iter()
            .map(|i| match i {
                Item::Frame(f, _) => (f.tenant.to_string(), f.seq),
                _ => panic!("not a frame"),
            })
            .collect()
    }

    /// A light tenant's frames go out in the first turn, not after a
    /// flooding tenant's backlog; each tenant's frames stay in order.
    #[test]
    fn tenants_take_turns_and_keep_their_order() {
        let o = BrokerOutbox::new(OutboxLimits::default());
        for n in 0..1000 {
            o.push_frame(frame("a", n)).ok().unwrap();
        }
        o.push_frame(frame("b", 0)).ok().unwrap();
        o.push_frame(frame("b", 1)).ok().unwrap();
        let mut out = Vec::new();
        o.take(&mut out, 300);
        let got = seqs(&out);
        assert_eq!(got.len(), 300);
        assert_eq!(&got[TURN..TURN + 2], &[("b".into(), 0), ("b".into(), 1)]);
        let a: Vec<u64> = got
            .iter()
            .filter(|(t, _)| t == "a")
            .map(|(_, n)| *n)
            .collect();
        assert_eq!(a, (0..298).collect::<Vec<_>>(), "a in order");
        assert_eq!(o.pending(), 702 + 300, "702 queued, 300 being handed over");

        // A lone tenant still fills the whole batch.
        out.clear();
        o.take(&mut out, 700);
        assert_eq!(out.len(), 700);
        assert_eq!(seqs(&out)[0], ("a".into(), 298));
    }

    /// Only the tenant whose oldest frame waited too long is refused.
    #[test]
    fn the_wait_limit_is_per_tenant() {
        let o = BrokerOutbox::new(OutboxLimits::default().max_wait(Duration::ZERO));
        o.push_frame(frame("a", 0)).ok().unwrap();
        std::thread::sleep(Duration::from_millis(2));
        assert!(o.push_frame(frame("a", 1)).is_err(), "a fell behind");
        assert!(o.push_frame(frame("b", 0)).is_ok(), "b did not");
    }

    /// Subscriptions and membership changes go to their room's tenant, in
    /// order with its frames.
    #[test]
    fn commands_queue_with_their_tenant() {
        let o = BrokerOutbox::new(OutboxLimits::default());
        o.push_frame(frame("a", 0)).ok().unwrap();
        o.push_sub(Subscription::Subscribe(Arc::from("b::r")), None);
        o.push_frame(frame("b", 7)).ok().unwrap();
        let mut out = Vec::new();
        o.take(&mut out, 10);
        assert!(matches!(&out[0], Item::Frame(f, _) if &*f.tenant == "a"));
        assert!(matches!(&out[1], Item::Sub(Subscription::Subscribe(r), _) if &**r == "b::r"));
        assert!(matches!(&out[2], Item::Frame(f, _) if f.seq == 7));
    }
}
