//! The engine's queue in front of a queued broker (ADR 0011 B4): bounded,
//! with a share per tenant so one tenant's burst cannot block the others'
//! publishes, drained in order by one task.

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
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
}

impl Default for OutboxLimits {
    /// 65,536 frames, a quarter of them per tenant.
    fn default() -> Self {
        Self {
            capacity: 65_536,
            per_tenant: 16_384,
        }
    }
}

impl OutboxLimits {
    /// `capacity` frames in total, `per_tenant` for any one tenant.
    pub fn new(capacity: usize, per_tenant: usize) -> Self {
        Self {
            capacity,
            per_tenant: per_tenant.min(capacity),
        }
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
    Frame(BrokerFrame),
    Sub(Subscription, Option<Subscribed>),
}

#[derive(Default)]
struct Queue {
    items: VecDeque<Item>,
    frames: usize,
    per_tenant: HashMap<Arc<str>, usize>,
}

/// See the module docs.
pub(crate) struct BrokerOutbox {
    queue: Mutex<Queue>,
    ready: Notify,
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

    /// Queue a frame unless the outbox or its tenant's share is full.
    pub(crate) fn push_frame(&self, frame: BrokerFrame) -> Result<(), Full> {
        {
            let mut q = self.lock();
            let used = q.per_tenant.get(&frame.tenant).copied().unwrap_or(0);
            if q.frames >= self.limits.capacity || used >= self.limits.per_tenant {
                drop(q);
                self.counters.refused.fetch_add(1, Ordering::Relaxed);
                return Err(Full);
            }
            q.frames += 1;
            *q.per_tenant.entry(Arc::clone(&frame.tenant)).or_insert(0) += 1;
            q.items.push_back(Item::Frame(frame));
        }
        self.ready.notify_one();
        Ok(())
    }

    /// Queue a subscription change. Never refused: there is at most one per
    /// room creation or removal, and losing one would leave the node deaf to
    /// (or flooded by) a room.
    pub(crate) fn push_sub(&self, cmd: Subscription, done: Option<Subscribed>) {
        self.lock().items.push_back(Item::Sub(cmd, done));
        self.ready.notify_one();
    }

    /// Move up to `max` items into `out` (in order), freeing their quota.
    fn take(&self, out: &mut Vec<Item>, max: usize) {
        let mut guard = self.lock();
        let q = &mut *guard;
        let n = max.min(q.items.len());
        for item in q.items.drain(..n) {
            if let Item::Frame(f) = &item {
                if let Some(used) = q.per_tenant.get_mut(&f.tenant) {
                    *used -= 1;
                    if *used == 0 {
                        q.per_tenant.remove(&f.tenant);
                    }
                }
                q.frames -= 1;
            }
            out.push(item);
        }
    }
}

/// Items handed to the broker per wake-up (one `publish` per run of frames).
/// One batch is in flight at a time, so this sets the outbox throughput: at
/// 256 it capped at ~150k publishes/s on loopback Redis (ADR 0011 A15).
const BATCH: usize = 1024;
const RETRY_FIRST: Duration = Duration::from_millis(50);
const RETRY_MAX: Duration = Duration::from_secs(2);

/// The outbox's drain task: hands frames and commands to the broker in
/// order. Runs until aborted.
pub(crate) async fn run_outbox(outbox: Arc<BrokerOutbox>, broker: Arc<dyn Broker>) {
    let mut items = Vec::with_capacity(BATCH);
    let mut frames = Vec::with_capacity(BATCH);
    loop {
        outbox.take(&mut items, BATCH);
        if items.is_empty() {
            outbox.ready.notified().await;
            continue;
        }
        for item in items.drain(..) {
            match item {
                Item::Frame(f) => frames.push(f),
                Item::Sub(cmd, done) => {
                    publish(&outbox, &*broker, &mut frames).await;
                    retry(&outbox, 1, || broker.subscription(&cmd)).await;
                    if let Some(done) = done {
                        done.send_replace(true);
                    }
                }
            }
        }
        publish(&outbox, &*broker, &mut frames).await;
    }
}

async fn publish(outbox: &BrokerOutbox, broker: &dyn Broker, frames: &mut Vec<BrokerFrame>) {
    if frames.is_empty() {
        return;
    }
    retry(outbox, frames.len(), || broker.publish(frames)).await;
    frames.clear();
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
