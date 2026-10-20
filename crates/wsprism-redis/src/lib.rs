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
//! - **History** (§3, R3-2): a kept message gets its offset, is stored and
//!   published by one Lua script (`PUBLISH_KEPT`), so offsets, history and
//!   delivery order agree on every node. Per room (one hash tag):
//!   `:seq` (last offset), `:epoch`, `:log` (sorted set, score = offset,
//!   member = `<ms>:<frame>`), `:bytes`. If Redis loses part of them
//!   (restart, eviction), a new epoch starts: clients resync instead of
//!   resuming from offsets that mean something else now.

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
    wire, Broker, BrokerError, BrokerFrame, BrokerMode, HistoryRead, Incoming, Keep, Subscription,
    MAX_NODE_SLOT,
};
use wsprism_engine::spi::Recover;

/// How long a quiet room's position (`seq`, `epoch`) outlives its last
/// message or join: a client may resume that long after (with nothing
/// missed) without being told to resync.
const POSITION_TTL: Duration = Duration::from_secs(24 * 3600);

/// KEYS: seq, epoch, log, bytes. ARGV: frame head, text, channel, size,
/// ttl ms, max bytes, epoch candidate, position ttl ms. Returns the offset.
const PUBLISH_KEPT: &str = r"
local size, ttl, maxb, pttl = tonumber(ARGV[4]), tonumber(ARGV[5]), tonumber(ARGV[6]), tonumber(ARGV[8])
if redis.call('EXISTS', KEYS[1]) == 0 or redis.call('EXISTS', KEYS[2]) == 0 then
  redis.call('SET', KEYS[2], ARGV[7])
  redis.call('SET', KEYS[1], 0)
  redis.call('DEL', KEYS[3], KEYS[4])
end
local o = redis.call('INCR', KEYS[1])
local s = tostring(o)
local msg = ARGV[1] .. string.char(#s) .. s .. ARGV[2]
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
redis.call('ZADD', KEYS[3], o, now .. ':' .. msg)
local b = redis.call('INCRBY', KEYS[4], #msg)
local n = redis.call('ZCARD', KEYS[3])
while n > 0 do
  local head = redis.call('ZRANGE', KEYS[3], 0, 0)[1]
  local sep = string.find(head, ':', 1, true)
  if n > size or b > maxb or now - tonumber(string.sub(head, 1, sep - 1)) > ttl then
    redis.call('ZREMRANGEBYRANK', KEYS[3], 0, 0)
    b = redis.call('DECRBY', KEYS[4], #head - sep)
    n = n - 1
  else
    break
  end
end
redis.call('PEXPIRE', KEYS[3], ttl)
redis.call('PEXPIRE', KEYS[4], ttl)
redis.call('PEXPIRE', KEYS[1], pttl)
redis.call('PEXPIRE', KEYS[2], pttl)
redis.call('PUBLISH', ARGV[3], msg)
return o
";

/// KEYS: seq, epoch, log, bytes. ARGV: since ('' = position only), epoch
/// asked, ttl ms, epoch candidate, position ttl ms. Returns
/// `{epoch, top}` or `{epoch, top, ok, {members...}}`.
const READ_KEPT: &str = r"
local ttl, pttl = tonumber(ARGV[3]), tonumber(ARGV[5])
if redis.call('EXISTS', KEYS[1]) == 0 or redis.call('EXISTS', KEYS[2]) == 0 then
  redis.call('SET', KEYS[2], ARGV[4])
  redis.call('SET', KEYS[1], 0)
  redis.call('DEL', KEYS[3], KEYS[4])
end
redis.call('PEXPIRE', KEYS[1], pttl)
redis.call('PEXPIRE', KEYS[2], pttl)
local ep = redis.call('GET', KEYS[2])
local top = tonumber(redis.call('GET', KEYS[1]))
if ARGV[1] == '' then return {ep, top} end
local since = tonumber(ARGV[1])
if ARGV[2] ~= ep or since > top then return {ep, top, 0} end
local t = redis.call('TIME')
local now = tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)
while true do
  local head = redis.call('ZRANGE', KEYS[3], 0, 0)[1]
  if not head then break end
  local sep = string.find(head, ':', 1, true)
  if now - tonumber(string.sub(head, 1, sep - 1)) > ttl then
    redis.call('ZREMRANGEBYRANK', KEYS[3], 0, 0)
    redis.call('DECRBY', KEYS[4], #head - sep)
  else
    break
  end
end
local first = redis.call('ZRANGE', KEYS[3], 0, 0, 'WITHSCORES')
local f = top + 1
if first[2] then f = tonumber(first[2]) end
if since + 1 < f then return {ep, top, 0} end
return {ep, top, 1, redis.call('ZRANGEBYSCORE', KEYS[3], '(' .. since, '+inf')}
";

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

/// The four keys of a room's history (one hash tag).
fn history_keys(prefix: &str, room_key: &str) -> [String; 4] {
    ["seq", "epoch", "log", "bytes"].map(|k| format!("{prefix}:h:{{{room_key}}}:{k}"))
}

struct ShardCmd {
    sub: Subscription,
    ack: oneshot::Sender<Result<(), BrokerError>>,
}

/// See the crate docs.
pub struct RedisBroker {
    client: redis::Client,
    publish_kept: redis::Script,
    read_kept: redis::Script,
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
            publish_kept: redis::Script::new(PUBLISH_KEPT),
            read_kept: redis::Script::new(READ_KEPT),
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
        // Pipelined publishes call the script by hash: load it up front.
        let _: String = redis::cmd("SCRIPT")
            .arg("LOAD")
            .arg(PUBLISH_KEPT)
            .query_async(&mut conn)
            .await
            .map_err(unavailable)?;
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
            match (&f.keep, wire::encode_kept(f)) {
                (Some(keep), Some((head, text))) => {
                    let [seq, epoch, log, bytes] = history_keys(&self.cfg.prefix, &f.room_key);
                    pipe.cmd("EVALSHA")
                        .arg(self.publish_kept.get_hash())
                        .arg(4)
                        .arg(seq)
                        .arg(epoch)
                        .arg(log)
                        .arg(bytes)
                        .arg(head.as_ref())
                        .arg(text.as_ref())
                        .arg(self.channel(&f.room_key))
                        .arg(keep.size)
                        .arg(keep.ttl.as_millis() as u64)
                        .arg(keep.max_bytes)
                        .arg(random_id())
                        .arg(POSITION_TTL.as_millis() as u64)
                        .ignore();
                }
                _ => {
                    if let Some(bytes) = wire::encode(f) {
                        pipe.cmd("PUBLISH")
                            .arg(self.channel(&f.room_key))
                            .arg(bytes.as_ref())
                            .ignore();
                    }
                }
            }
        }
        match pipe.query_async::<()>(&mut conn).await {
            Ok(()) => Ok(()),
            // Redis restarted and forgot the script: load it and let the
            // outbox retry the batch. Commands before the failing one ran
            // already, so a message may then be delivered twice (a kept one
            // under a second offset) — rare, and clients drop what they
            // have already seen by offset only within one offset.
            Err(e) if e.code() == Some("NOSCRIPT") => {
                let _: Result<String, _> = redis::cmd("SCRIPT")
                    .arg("LOAD")
                    .arg(PUBLISH_KEPT)
                    .query_async(&mut conn)
                    .await;
                Err(BrokerError::Unavailable("script reloaded".into()))
            }
            Err(e) => Err(unavailable(e)),
        }
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

    async fn history(
        &self,
        room_key: &str,
        keep: &Keep,
        from: Option<&Recover>,
    ) -> Result<HistoryRead, BrokerError> {
        let mut conn = self
            .conn
            .get()
            .cloned()
            .ok_or_else(|| BrokerError::Unavailable("not started".into()))?;
        let [seq, epoch, log, bytes] = history_keys(&self.cfg.prefix, room_key);
        let reply: redis::Value = self
            .read_kept
            .key(seq)
            .key(epoch)
            .key(log)
            .key(bytes)
            .arg(from.map_or(String::new(), |f| f.offset.to_string()))
            .arg(from.map_or("", |f| f.epoch.as_str()))
            .arg(keep.ttl.as_millis() as u64)
            .arg(random_id())
            .arg(POSITION_TTL.as_millis() as u64)
            .invoke_async(&mut conn)
            .await
            .map_err(unavailable)?;
        parse_read(&reply)
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

/// `{epoch, top}` or `{epoch, top, ok, {members}}` from `READ_KEPT`.
fn parse_read(v: &redis::Value) -> Result<HistoryRead, BrokerError> {
    use redis::Value as V;
    let bad = || BrokerError::Rejected("unexpected history reply".into());
    let V::Array(items) = v else {
        return Err(bad());
    };
    let epoch: Arc<str> = match items.first() {
        Some(V::BulkString(b)) => Arc::from(String::from_utf8_lossy(b).as_ref()),
        _ => return Err(bad()),
    };
    let top = match items.get(1) {
        Some(V::Int(n)) => u64::try_from(*n).map_err(|_| bad())?,
        _ => return Err(bad()),
    };
    let replay = match (items.get(2), items.get(3)) {
        (Some(V::Int(1)), Some(V::Array(members))) => {
            let mut frames = Vec::with_capacity(members.len());
            for m in members {
                let V::BulkString(b) = m else {
                    return Err(bad());
                };
                // `<ms>:<frame>`
                let at = b.iter().position(|&c| c == b':').ok_or_else(bad)?;
                let frame = wire::decode(&Bytes::copy_from_slice(&b[at + 1..]))
                    .map_err(|e| BrokerError::Rejected(e.to_string()))?;
                frames.push(frame);
            }
            Some(frames)
        }
        _ => None,
    };
    Ok(HistoryRead::new(epoch, top, replay))
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
