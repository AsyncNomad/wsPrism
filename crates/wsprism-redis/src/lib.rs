//! wsPrism cluster broker on Redis (ADR 0011 R3): gateway nodes share rooms
//! through Redis pub/sub.
//!
//! **Experimental** — like `wsprism_engine::broker`, this changes until the
//! cluster work (ADR 0011 R3-R5) is complete.
//!
//! - **Fan-out:** each room is a channel `<prefix>:r:{<tenant>::<room>}` (the
//!   braces are a Redis Cluster hash tag, chosen now so keys need not change
//!   later; A5). A node subscribes only to rooms with local members; the
//!   engine's outbox hands frames over in order and they are published with
//!   one pipeline per batch (A3).
//! - **Several subscriber connections** (A6): rooms are spread over
//!   `pubsub_connections` by hash, so one busy room does not delay others.
//!   A room always maps to the same connection, which keeps its order.
//! - **Node slot** (B1): claimed at start with `SET NX PX` and renewed by a
//!   heartbeat; member ids carry it.
//! - **Gaps** (§4): when a subscriber connection drops, it reconnects,
//!   resubscribes and reports its rooms as a gap (`sys.gap` to members).

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![warn(missing_docs)]

use std::collections::hash_map::RandomState;
use std::collections::HashSet;
use std::hash::{BuildHasher, Hasher};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::StreamExt;
use redis::aio::ConnectionManager;
use tokio::sync::{mpsc, oneshot, OnceCell};
use tokio::task::JoinHandle;

use wsprism_engine::broker::{
    wire, Broker, BrokerError, BrokerFrame, BrokerMode, Incoming, Subscription, MAX_NODE_SLOT,
};

/// How to reach Redis and how to use it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct RedisConfig {
    /// `redis://[user:password@]host:port[/db]` or `rediss://` (TLS).
    pub url: String,
    /// Prefix of every key and channel, so several clusters can share one
    /// Redis (A8).
    pub prefix: String,
    /// Subscriber connections (rooms are spread over them).
    pub pubsub_connections: usize,
    /// How long a node's slot survives without a heartbeat.
    pub slot_ttl: Duration,
}

impl RedisConfig {
    /// `url` with prefix `wsp`, 4 subscriber connections and a 30 s slot TTL.
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            prefix: "wsp".into(),
            pubsub_connections: 4,
            slot_ttl: Duration::from_secs(30),
        }
    }

    /// Key and channel prefix.
    pub fn prefix(mut self, prefix: impl Into<String>) -> Self {
        self.prefix = prefix.into();
        self
    }

    /// Subscriber connections (at least 1).
    pub fn pubsub_connections(mut self, n: usize) -> Self {
        self.pubsub_connections = n.max(1);
        self
    }

    /// Slot TTL (at least 3 s; renewed every third of it).
    pub fn slot_ttl(mut self, ttl: Duration) -> Self {
        self.slot_ttl = ttl.max(Duration::from_secs(3));
        self
    }
}

struct ShardCmd {
    sub: Subscription,
    ack: oneshot::Sender<Result<(), BrokerError>>,
}

/// See the crate docs.
pub struct RedisBroker {
    client: redis::Client,
    cfg: RedisConfig,
    node_id: String,
    conn: OnceCell<ConnectionManager>,
    shards: OnceLock<Vec<mpsc::UnboundedSender<ShardCmd>>>,
    incoming_tx: mpsc::Sender<Incoming>,
    incoming_rx: Mutex<Option<mpsc::Receiver<Incoming>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unavailable(e: redis::RedisError) -> BrokerError {
    BrokerError::Unavailable(format!("redis: {e}"))
}

/// Incoming frames buffered between the subscribers and the engine.
const INCOMING_QUEUE: usize = 65_536;
const RECONNECT_FIRST: Duration = Duration::from_millis(50);
const RECONNECT_MAX: Duration = Duration::from_secs(2);

impl RedisBroker {
    /// A broker for `cfg` (checks the URL; connects in `start`).
    pub fn new(cfg: RedisConfig) -> Result<Self, BrokerError> {
        let client = redis::Client::open(cfg.url.as_str())
            .map_err(|e| BrokerError::Rejected(format!("redis url: {e}")))?;
        let (incoming_tx, incoming_rx) = mpsc::channel(INCOMING_QUEUE);
        Ok(Self {
            client,
            cfg,
            node_id: random_id(),
            conn: OnceCell::new(),
            shards: OnceLock::new(),
            incoming_tx,
            incoming_rx: Mutex::new(Some(incoming_rx)),
            tasks: Mutex::default(),
        })
    }

    fn channel(&self, room_key: &str) -> String {
        format!("{}:r:{{{room_key}}}", self.cfg.prefix)
    }

    fn slot_key(&self, slot: u16) -> String {
        format!("{}:slot:{slot}", self.cfg.prefix)
    }

    fn shard_of(&self, room_key: &str, n: usize) -> usize {
        // FNV-1a: stable across nodes and restarts (unlike RandomState).
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        for b in room_key.bytes() {
            h ^= u64::from(b);
            h = h.wrapping_mul(0x0000_0100_0000_01b3);
        }
        (h % n as u64) as usize
    }

    /// Claim the first free node slot.
    async fn lease(&self, conn: &mut ConnectionManager) -> Result<u16, BrokerError> {
        let ttl_ms = self.cfg.slot_ttl.as_millis() as u64;
        for slot in 0..=MAX_NODE_SLOT {
            let got: Option<String> = redis::cmd("SET")
                .arg(self.slot_key(slot))
                .arg(&self.node_id)
                .arg("NX")
                .arg("PX")
                .arg(ttl_ms)
                .query_async(conn)
                .await
                .map_err(unavailable)?;
            if got.is_some() {
                return Ok(slot);
            }
        }
        Err(BrokerError::Unavailable(format!(
            "all {} node slots are taken",
            u32::from(MAX_NODE_SLOT) + 1
        )))
    }
}

impl Drop for RedisBroker {
    fn drop(&mut self) {
        for t in lock(&self.tasks).iter() {
            t.abort();
        }
    }
}

#[async_trait]
impl Broker for RedisBroker {
    fn mode(&self) -> BrokerMode {
        BrokerMode::Queued
    }

    async fn start(&self) -> Result<u16, BrokerError> {
        let mut conn = self
            .client
            .get_connection_manager()
            .await
            .map_err(unavailable)?;
        let slot = self.lease(&mut conn).await?;
        tracing::info!(slot, node = %self.node_id, "redis broker: node slot claimed");
        let mut tasks = lock(&self.tasks);
        tasks.push(tokio::spawn(heartbeat(
            conn.clone(),
            self.slot_key(slot),
            self.node_id.clone(),
            self.cfg.slot_ttl,
        )));
        let mut shards = Vec::with_capacity(self.cfg.pubsub_connections);
        for i in 0..self.cfg.pubsub_connections {
            let (tx, rx) = mpsc::unbounded_channel();
            shards.push(tx);
            tasks.push(tokio::spawn(run_shard(
                i,
                self.client.clone(),
                self.cfg.prefix.clone(),
                rx,
                self.incoming_tx.clone(),
            )));
        }
        drop(tasks);
        let _ = self.shards.set(shards);
        let _ = self.conn.set(conn);
        Ok(slot)
    }

    async fn publish(&self, batch: &[BrokerFrame]) -> Result<(), BrokerError> {
        let mut conn = self
            .conn
            .get()
            .cloned()
            .ok_or_else(|| BrokerError::Unavailable("not started".into()))?;
        let mut pipe = redis::pipe();
        for f in batch {
            if let Some(bytes) = wire::encode(f) {
                pipe.cmd("PUBLISH")
                    .arg(self.channel(&f.room_key))
                    .arg(bytes.as_ref())
                    .ignore();
            }
        }
        pipe.query_async::<()>(&mut conn).await.map_err(unavailable)
    }

    async fn subscription(&self, cmd: &Subscription) -> Result<(), BrokerError> {
        let shards = self
            .shards
            .get()
            .ok_or_else(|| BrokerError::Unavailable("not started".into()))?;
        let room = match cmd {
            Subscription::Subscribe(r) | Subscription::Unsubscribe(r) => r,
            _ => return Ok(()),
        };
        let (ack, done) = oneshot::channel();
        shards[self.shard_of(room, shards.len())]
            .send(ShardCmd {
                sub: cmd.clone(),
                ack,
            })
            .map_err(|_| BrokerError::Unavailable("subscriber stopped".into()))?;
        done.await
            .map_err(|_| BrokerError::Unavailable("subscriber stopped".into()))?
    }

    fn incoming(&self) -> Option<mpsc::Receiver<Incoming>> {
        lock(&self.incoming_rx).take()
    }
}

/// Keep the node slot: renew it every third of its TTL; if it expired
/// meanwhile (Redis was unreachable), take it back if still free.
async fn heartbeat(mut conn: ConnectionManager, key: String, node_id: String, ttl: Duration) {
    let script = redis::Script::new(
        r"if redis.call('GET', KEYS[1]) == ARGV[1] then
            return redis.call('PEXPIRE', KEYS[1], ARGV[2])
          end
          if redis.call('SET', KEYS[1], ARGV[1], 'NX', 'PX', ARGV[2]) then return 2 end
          return -1",
    );
    let ttl_ms = ttl.as_millis() as u64;
    loop {
        tokio::time::sleep(ttl / 3).await;
        match script
            .key(&key)
            .arg(&node_id)
            .arg(ttl_ms)
            .invoke_async::<i64>(&mut conn)
            .await
        {
            Ok(1) => {}
            Ok(2) => tracing::warn!(%key, "redis broker: node slot had expired; claimed again"),
            Ok(_) => tracing::error!(
                %key,
                "redis broker: node slot taken by another node; member ids may collide — restart this node"
            ),
            Err(e) => tracing::warn!(%key, "redis broker: slot heartbeat failed: {e}"),
        }
    }
}

/// One subscriber connection: applies (un)subscriptions in order, forwards
/// messages, and on a lost connection reconnects, resubscribes and reports
/// a gap for its rooms.
async fn run_shard(
    index: usize,
    client: redis::Client,
    prefix: String,
    mut cmds: mpsc::UnboundedReceiver<ShardCmd>,
    incoming: mpsc::Sender<Incoming>,
) {
    let channel = |room: &str| format!("{prefix}:r:{{{room}}}");
    let mut rooms: HashSet<Arc<str>> = HashSet::new();
    let mut connected_before = false;
    let mut wait = RECONNECT_FIRST;
    loop {
        let (mut sink, mut stream) = match client.get_async_pubsub().await {
            Ok(p) => p.split(),
            Err(e) => {
                tracing::warn!(
                    shard = index,
                    "redis broker: subscriber connect failed: {e}"
                );
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(RECONNECT_MAX);
                continue;
            }
        };
        let mut resubscribed = true;
        for room in &rooms {
            if sink.subscribe(channel(room)).await.is_err() {
                resubscribed = false;
                break;
            }
        }
        if !resubscribed {
            tokio::time::sleep(wait).await;
            continue;
        }
        wait = RECONNECT_FIRST;
        if connected_before && !rooms.is_empty() {
            tracing::warn!(
                shard = index,
                rooms = rooms.len(),
                "redis broker: resubscribed after a drop; reporting a gap"
            );
            let _ = incoming
                .send(Incoming::Gap(rooms.iter().cloned().collect()))
                .await;
        }
        connected_before = true;
        loop {
            tokio::select! {
                cmd = cmds.recv() => {
                    let Some(cmd) = cmd else { return };
                    let result = match &cmd.sub {
                        Subscription::Subscribe(r) => sink.subscribe(channel(r)).await.map(|()| {
                            rooms.insert(Arc::clone(r));
                        }),
                        Subscription::Unsubscribe(r) => sink.unsubscribe(channel(r)).await.map(|()| {
                            rooms.remove(r);
                        }),
                        _ => Ok(()),
                    };
                    let failed = result.is_err();
                    let _ = cmd.ack.send(result.map_err(unavailable));
                    if failed {
                        break; // reconnect; the engine retries the command
                    }
                }
                msg = stream.next() => {
                    let Some(msg) = msg else { break }; // connection lost
                    match wire::decode(&Bytes::copy_from_slice(msg.get_payload_bytes())) {
                        Ok(f) => {
                            let _ = incoming.send(Incoming::Frame(f)).await;
                        }
                        Err(e) => tracing::warn!(shard = index, "redis broker: dropped a frame: {e}"),
                    }
                }
            }
        }
    }
}

/// A node id nobody else has: process id, time and per-process randomness.
fn random_id() -> String {
    let mut h = RandomState::new().build_hasher();
    h.write_u32(std::process::id());
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("{:016x}", h.finish())
}
