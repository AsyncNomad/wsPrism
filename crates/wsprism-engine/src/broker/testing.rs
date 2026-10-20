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

use super::{wire, Broker, BrokerError, BrokerFrame, BrokerMode, Incoming, Subscription};

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

    fn incoming(&self) -> Option<mpsc::Receiver<Incoming>> {
        lock(&self.rx).take()
    }
}
