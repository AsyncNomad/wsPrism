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
//! - **Sessions** (R4-1): the cluster's session table, one hash per user
//!   (`<prefix>:us:{tenant::user}`, field = session key, value =
//!   `node id|slot|sid|created µs`), changed only by `CLAIM`/`OF_USER`
//!   scripts that also drop entries of dead nodes (their slot key no longer
//!   holds their node id). Commands to a node go to `<prefix>:ctl:<slot>`.
//! - **Membership** (R4-2): `<prefix>:pr:{tenant::room}` hash, field =
//!   session key, value = `node id|slot|user|member id|notify`; each node
//!   lists its rooms in `<prefix>:nr:<node id>` and itself in
//!   `<prefix>:nodes`. Every node looks for dead nodes (slot key no longer
//!   theirs) every few seconds; one wins `<prefix>:reap:<node id>` (SET NX)
//!   and removes their members — a member is reported only by the HDEL
//!   that removed it, so each `member_left` is sent once (A10).
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
    wire, Broker, BrokerError, BrokerFrame, BrokerMode, Claimed, Control, DeadMember, Evicted,
    HistoryRead, Incoming, Keep, OnFull, PresenceOp, RoomMember, SessionClaim, SessionEntry,
    SharedKv, SharedPresence, SharedSessions, Subscription, MAX_NODE_SLOT,
};
use wsprism_engine::spi::Recover;

/// How long a quiet room's position (`seq`, `epoch`) outlives its last
/// join or read (and, refreshed every 64th message, its messages): a client
/// may resume that long after (with nothing missed) without being told to
/// resync.
const POSITION_TTL: Duration = Duration::from_secs(24 * 3600);

/// One room's part of an outbox batch, in order (A15: one script call per
/// room and batch, not per message — per message only the PUBLISH is left).
/// KEYS: seq, epoch, log, bytes. ARGV: channel, size, ttl ms, max bytes,
/// epoch candidate, position ttl ms, then pairs (head, text) per message:
/// a kept message gets the next offset; head = '' publishes text as is.
/// Returns the last offset.
const PUBLISH_KEPT: &str = r"
local ch, size, ttl, maxb, pttl = ARGV[1], tonumber(ARGV[2]), tonumber(ARGV[3]), tonumber(ARGV[4]), tonumber(ARGV[6])
if redis.call('EXISTS', KEYS[1], KEYS[2]) < 2 then
  redis.call('SET', KEYS[2], ARGV[5], 'PX', pttl)
  redis.call('SET', KEYS[1], 0, 'PX', pttl)
  redis.call('DEL', KEYS[3], KEYS[4])
end
local kept = 0
for i = 7, #ARGV, 2 do
  if ARGV[i] ~= '' then kept = kept + 1 end
end
local o = redis.call('INCRBY', KEYS[1], kept) - kept
local t = redis.call('TIME')
local now = (tonumber(t[1]) * 1000 + math.floor(tonumber(t[2]) / 1000)) .. ':'
local z, added = {}, 0
for i = 7, #ARGV, 2 do
  if ARGV[i] == '' then
    redis.call('PUBLISH', ch, ARGV[i + 1])
  else
    o = o + 1
    local s = tostring(o)
    local msg = ARGV[i] .. string.char(#s) .. s .. ARGV[i + 1]
    z[#z + 1] = o
    z[#z + 1] = now .. msg
    added = added + #msg
    redis.call('PUBLISH', ch, msg)
  end
end
if kept > 0 then
  redis.call('ZADD', KEYS[3], unpack(z))
  local b = redis.call('INCRBY', KEYS[4], added)
  local n = redis.call('ZCARD', KEYS[3])
  -- Count and byte limits (age is enforced when reading).
  if n > size then
    local gone = redis.call('ZPOPMIN', KEYS[3], n - size)
    local freed = 0
    for i = 1, #gone, 2 do
      freed = freed + #gone[i] - string.find(gone[i], ':', 1, true)
    end
    b = redis.call('DECRBY', KEYS[4], freed)
    n = size
  end
  while b > maxb and n > 1 do
    local head = redis.call('ZPOPMIN', KEYS[3])[1]
    b = redis.call('DECRBY', KEYS[4], #head - string.find(head, ':', 1, true))
    n = n - 1
  end
  redis.call('PEXPIRE', KEYS[3], ttl)
  redis.call('PEXPIRE', KEYS[4], ttl)
  redis.call('PEXPIRE', KEYS[1], pttl)
  redis.call('PEXPIRE', KEYS[2], pttl)
end
return o
";

/// KEYS: seq, epoch, log, bytes. ARGV: since ('' = position only), epoch
/// asked, ttl ms, epoch candidate, position ttl ms. Returns
/// `{epoch, top}` or `{epoch, top, ok, {members...}}`.
const READ_KEPT: &str = r"
local ttl, pttl = tonumber(ARGV[3]), tonumber(ARGV[5])
if redis.call('EXISTS', KEYS[1], KEYS[2]) < 2 then
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

/// KEYS: the user's session hash. ARGV: session key, sid, node id, slot,
/// max per user, 'kick' or 'deny', slot key prefix. Drops dead nodes'
/// entries, replaces the same sid, applies the limit, adds the session.
/// Returns `{admitted, slot, key, reason, slot, key, reason, ...}`.
const CLAIM: &str = r"
local live = {}
local out = {0}
local entries = redis.call('HGETALL', KEYS[1])
for i = 1, #entries, 2 do
  local key, v = entries[i], entries[i + 1]
  local node, slot, sid, created = string.match(v, '^([^|]*)|(%d+)|(.*)|(%d+)$')
  if not node or redis.call('GET', ARGV[7] .. slot) ~= node then
    redis.call('HDEL', KEYS[1], key)
  elseif sid == ARGV[2] then
    redis.call('HDEL', KEYS[1], key)
    out[#out + 1] = slot
    out[#out + 1] = key
    out[#out + 1] = 'replaced'
  else
    live[#live + 1] = {key, slot, tonumber(created)}
  end
end
if #live >= tonumber(ARGV[5]) then
  if ARGV[6] ~= 'kick' then return out end
  local oldest = 1
  for i = 2, #live do
    if live[i][3] < live[oldest][3] then oldest = i end
  end
  redis.call('HDEL', KEYS[1], live[oldest][1])
  out[#out + 1] = live[oldest][2]
  out[#out + 1] = live[oldest][1]
  out[#out + 1] = 'max_sessions_exceeded'
end
local t = redis.call('TIME')
-- As a string: Lua 5.1 prints numbers past 14 digits as 1.7e+15.
local created = t[1] .. string.format('%06d', tonumber(t[2]))
redis.call('HSET', KEYS[1], ARGV[1], ARGV[3] .. '|' .. ARGV[4] .. '|' .. ARGV[2] .. '|' .. created)
out[1] = 1
return out
";

/// KEYS: the user's session hash. ARGV: slot key prefix. The live
/// sessions as `{slot, key, sid, ...}` (dead nodes' entries dropped).
const OF_USER: &str = r"
local out = {}
local entries = redis.call('HGETALL', KEYS[1])
for i = 1, #entries, 2 do
  local key, v = entries[i], entries[i + 1]
  local node, slot, sid = string.match(v, '^([^|]*)|(%d+)|(.*)|%d+$')
  if not node or redis.call('GET', ARGV[1] .. slot) ~= node then
    redis.call('HDEL', KEYS[1], key)
  else
    out[#out + 1] = slot
    out[#out + 1] = key
    out[#out + 1] = sid
  end
end
return out
";

/// KEYS: a room's membership hash. ARGV: slot key prefix. Live members as
/// `{session key, user, member id, ...}` (dead nodes' entries skipped, not
/// removed: the reaper announces them).
const MEMBERS: &str = r"
local out = {}
local entries = redis.call('HGETALL', KEYS[1])
for i = 1, #entries, 2 do
  local node, slot, user, mid = string.match(entries[i + 1], '^([^|]*)|(%d+)|(.*)|(%d+)|[01]$')
  if node and redis.call('GET', ARGV[1] .. slot) == node then
    out[#out + 1] = entries[i]
    out[#out + 1] = user
    out[#out + 1] = mid
  end
end
return out
";

/// KEYS: a room's membership hash. ARGV: a dead node's id. Removes its
/// members; returns `{user, member id, notify, ...}` of those this call
/// removed (HDEL = 1), so concurrent reapers never report one twice.
const REAP_ROOM: &str = r"
local out = {}
local entries = redis.call('HGETALL', KEYS[1])
for i = 1, #entries, 2 do
  local node, user, mid, notify = string.match(entries[i + 1], '^([^|]*)|%d+|(.*)|(%d+)|([01])$')
  if node == ARGV[1] and redis.call('HDEL', KEYS[1], entries[i]) == 1 then
    out[#out + 1] = user
    out[#out + 1] = mid
    out[#out + 1] = notify
  end
end
return out
";

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
    claim: redis::Script,
    of_user: redis::Script,
    members: redis::Script,
    reap_room: redis::Script,
    /// This node's slot (set in `start`).
    slot: std::sync::atomic::AtomicU16,
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
            claim: redis::Script::new(CLAIM),
            of_user: redis::Script::new(OF_USER),
            members: redis::Script::new(MEMBERS),
            reap_room: redis::Script::new(REAP_ROOM),
            slot: std::sync::atomic::AtomicU16::new(0),
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
        // Known to the reapers of other nodes (R4-2).
        let _: i64 = redis::cmd("HSET")
            .arg(format!("{}:nodes", self.cfg.prefix))
            .arg(&self.node_id)
            .arg(slot)
            .query_async(&mut conn)
            .await
            .map_err(unavailable)?;
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
        // Commands for this node (R4-1).
        tasks.push(tokio::spawn(run_control(
            self.client.clone(),
            format!("{}:ctl:{slot}", self.cfg.prefix),
            self.incoming_tx.clone(),
        )));
        self.slot.store(slot, std::sync::atomic::Ordering::Relaxed);
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
        // Rooms with kept messages: their part of the batch, in order, for
        // one script call each. Everything else: a PUBLISH each.
        let mut kept_rooms: Vec<(&BrokerFrame, Vec<&BrokerFrame>)> = Vec::new();
        let mut index: std::collections::HashMap<&str, usize> = std::collections::HashMap::new();
        let mut pipe = redis::pipe();
        for f in batch {
            if let Some(&i) = index.get(&*f.room_key) {
                kept_rooms[i].1.push(f);
            } else if f.keep.is_some() {
                index.insert(&f.room_key, kept_rooms.len());
                kept_rooms.push((f, vec![f]));
            } else if let Some(bytes) = wire::encode(f) {
                pipe.cmd("PUBLISH")
                    .arg(self.channel(&f.room_key))
                    .arg(bytes.as_ref())
                    .ignore();
            }
        }
        for (first, frames) in &kept_rooms {
            let Some(keep) = first.keep else { continue };
            let [seq, epoch, log, bytes] = history_keys(&self.cfg.prefix, &first.room_key);
            let cmd = pipe
                .cmd("EVALSHA")
                .arg(self.publish_kept.get_hash())
                .arg(4)
                .arg(seq)
                .arg(epoch)
                .arg(log)
                .arg(bytes)
                .arg(self.channel(&first.room_key))
                .arg(keep.size)
                .arg(keep.ttl.as_millis() as u64)
                .arg(keep.max_bytes)
                .arg(random_id())
                .arg(POSITION_TTL.as_millis() as u64);
            for f in frames {
                match (f.keep.is_some(), wire::encode_kept(f)) {
                    (true, Some((head, text))) => cmd.arg(head.as_ref()).arg(text.as_ref()),
                    _ => match wire::encode(f) {
                        Some(b) => cmd.arg("").arg(b.as_ref()),
                        None => cmd,
                    },
                };
            }
            cmd.ignore();
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
            .map_err(|_| BrokerError::Unavailable("subscriber stopped".into()))??;
        // This node left the room: no longer in its room index (R4-2).
        if let Subscription::Unsubscribe(room) = cmd {
            let _: i64 = redis::cmd("SREM")
                .arg(self.nr_key(&self.node_id))
                .arg(&**room)
                .query_async(&mut self.conn()?)
                .await
                .map_err(unavailable)?;
        }
        Ok(())
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

    fn kv(&self) -> Option<&dyn SharedKv> {
        Some(self)
    }

    fn sessions(&self) -> Option<&dyn SharedSessions> {
        Some(self)
    }

    fn presence(&self) -> Option<&dyn SharedPresence> {
        Some(self)
    }
}

impl RedisBroker {
    fn pr_key(&self, room_key: &str) -> String {
        format!("{}:pr:{{{room_key}}}", self.cfg.prefix)
    }

    fn nr_key(&self, node_id: &str) -> String {
        format!("{}:nr:{node_id}", self.cfg.prefix)
    }
}

#[async_trait]
impl SharedPresence for RedisBroker {
    async fn apply(&self, ops: &[PresenceOp]) -> Result<(), BrokerError> {
        let slot = self.slot.load(std::sync::atomic::Ordering::Relaxed);
        let mut pipe = redis::pipe();
        for op in ops {
            match op {
                PresenceOp::Add {
                    room_key,
                    session_key,
                    user,
                    member_id,
                    notify,
                } => {
                    let value = format!(
                        "{}|{slot}|{user}|{member_id}|{}",
                        self.node_id,
                        u8::from(*notify)
                    );
                    pipe.cmd("HSET")
                        .arg(self.pr_key(room_key))
                        .arg(&**session_key)
                        .arg(value)
                        .ignore();
                    pipe.cmd("SADD")
                        .arg(self.nr_key(&self.node_id))
                        .arg(&**room_key)
                        .ignore();
                }
                PresenceOp::Remove {
                    room_key,
                    session_key,
                } => {
                    pipe.cmd("HDEL")
                        .arg(self.pr_key(room_key))
                        .arg(&**session_key)
                        .ignore();
                }
                _ => {}
            }
        }
        pipe.query_async::<()>(&mut self.conn()?)
            .await
            .map_err(unavailable)
    }

    async fn members(&self, room_key: &str) -> Result<Vec<RoomMember>, BrokerError> {
        let reply: redis::Value = self
            .members
            .key(self.pr_key(room_key))
            .arg(self.slot_prefix())
            .invoke_async(&mut self.conn()?)
            .await
            .map_err(unavailable)?;
        let bad = || BrokerError::Rejected("unexpected presence reply".into());
        strings(&reply)
            .ok_or_else(bad)?
            .chunks(3)
            .map(|m| match m {
                [key, user, mid] => Ok(RoomMember::new(
                    Arc::from(key.as_str()),
                    Arc::from(user.as_str()),
                    mid.parse().map_err(|_| bad())?,
                )),
                _ => Err(bad()),
            })
            .collect()
    }

    async fn reap_dead(&self) -> Result<Vec<DeadMember>, BrokerError> {
        let mut conn = self.conn()?;
        let nodes_key = format!("{}:nodes", self.cfg.prefix);
        let nodes: std::collections::HashMap<String, String> = redis::cmd("HGETALL")
            .arg(&nodes_key)
            .query_async(&mut conn)
            .await
            .map_err(unavailable)?;
        let mut dead = Vec::new();
        for (node, slot) in nodes {
            if node == self.node_id {
                continue;
            }
            let owner: Option<String> = redis::cmd("GET")
                .arg(format!("{}{slot}", self.slot_prefix()))
                .query_async(&mut conn)
                .await
                .map_err(unavailable)?;
            if owner.as_deref() == Some(node.as_str()) {
                continue; // alive
            }
            // One reaper per dead node; it may die too, so the claim expires.
            let won: Option<String> = redis::cmd("SET")
                .arg(format!("{}:reap:{node}", self.cfg.prefix))
                .arg(&self.node_id)
                .arg("NX")
                .arg("EX")
                .arg(60)
                .query_async(&mut conn)
                .await
                .map_err(unavailable)?;
            if won.is_none() {
                continue;
            }
            let before = dead.len();
            let rooms: Vec<String> = redis::cmd("SMEMBERS")
                .arg(self.nr_key(&node))
                .query_async(&mut conn)
                .await
                .map_err(unavailable)?;
            for room in &rooms {
                let reply: redis::Value = self
                    .reap_room
                    .key(self.pr_key(room))
                    .arg(&node)
                    .invoke_async(&mut conn)
                    .await
                    .map_err(unavailable)?;
                for m in strings(&reply).unwrap_or_default().chunks(3) {
                    if let [user, mid, notify] = m {
                        dead.push(DeadMember::new(
                            Arc::from(room.as_str()),
                            Arc::from(user.as_str()),
                            mid.parse().unwrap_or(0),
                            notify == "1",
                        ));
                    }
                }
            }
            tracing::warn!(
                %node,
                rooms = rooms.len(),
                members = dead.len() - before,
                "redis broker: reaped a dead node's members"
            );
            let _: () = redis::pipe()
                .cmd("DEL")
                .arg(self.nr_key(&node))
                .ignore()
                .cmd("HDEL")
                .arg(&nodes_key)
                .arg(&node)
                .ignore()
                .query_async(&mut conn)
                .await
                .map_err(unavailable)?;
        }
        Ok(dead)
    }
}

impl RedisBroker {
    fn user_key(&self, user_key: &str) -> String {
        format!("{}:us:{{{user_key}}}", self.cfg.prefix)
    }

    fn slot_prefix(&self) -> String {
        format!("{}:slot:", self.cfg.prefix)
    }
}

/// `{admitted, (slot, key, reason)*}` from `CLAIM`.
fn parse_claim(v: &redis::Value) -> Result<Claimed, BrokerError> {
    let bad = || BrokerError::Rejected("unexpected claim reply".into());
    let items = strings(v).ok_or_else(bad)?;
    let (head, rest) = items.split_first().ok_or_else(bad)?;
    let mut evicted = Vec::new();
    for e in rest.chunks(3) {
        let [slot, key, reason] = e else {
            return Err(bad());
        };
        let reason = if reason == "replaced" {
            "replaced"
        } else {
            "max_sessions_exceeded"
        };
        evicted.push(Evicted::new(
            slot.parse().map_err(|_| bad())?,
            Arc::from(key.as_str()),
            reason,
        ));
    }
    Ok(Claimed::new(head == "1", evicted))
}

/// A reply array as strings (integers included).
fn strings(v: &redis::Value) -> Option<Vec<String>> {
    let redis::Value::Array(items) = v else {
        return None;
    };
    items
        .iter()
        .map(|i| match i {
            redis::Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
            redis::Value::Int(n) => Some(n.to_string()),
            redis::Value::SimpleString(s) => Some(s.clone()),
            _ => None,
        })
        .collect()
}

#[async_trait]
impl SharedSessions for RedisBroker {
    async fn claim(&self, c: &SessionClaim) -> Result<Claimed, BrokerError> {
        let reply: redis::Value = self
            .claim
            .key(self.user_key(&c.user_key))
            .arg(&*c.session_key)
            .arg(&*c.sid)
            .arg(&self.node_id)
            .arg(self.slot.load(std::sync::atomic::Ordering::Relaxed))
            .arg(c.max_per_user)
            .arg(if c.on_full == OnFull::KickOldest {
                "kick"
            } else {
                "deny"
            })
            .arg(self.slot_prefix())
            .invoke_async(&mut self.conn()?)
            .await
            .map_err(unavailable)?;
        parse_claim(&reply)
    }

    async fn release(&self, user_key: &str, session_key: &str) -> Result<(), BrokerError> {
        let _: i64 = redis::cmd("HDEL")
            .arg(self.user_key(user_key))
            .arg(session_key)
            .query_async(&mut self.conn()?)
            .await
            .map_err(unavailable)?;
        Ok(())
    }

    async fn of_user(&self, user_key: &str) -> Result<Vec<SessionEntry>, BrokerError> {
        let reply: redis::Value = self
            .of_user
            .key(self.user_key(user_key))
            .arg(self.slot_prefix())
            .invoke_async(&mut self.conn()?)
            .await
            .map_err(unavailable)?;
        let bad = || BrokerError::Rejected("unexpected session reply".into());
        let items = strings(&reply).ok_or_else(bad)?;
        items
            .chunks(3)
            .map(|e| match e {
                [slot, key, sid] => Ok(SessionEntry::new(
                    slot.parse().map_err(|_| bad())?,
                    Arc::from(key.as_str()),
                    Arc::from(sid.as_str()),
                )),
                _ => Err(bad()),
            })
            .collect()
    }

    async fn send_control(&self, node: u16, control: &Control) -> Result<(), BrokerError> {
        let _: i64 = redis::cmd("PUBLISH")
            .arg(format!("{}:ctl:{node}", self.cfg.prefix))
            .arg(wire::encode_control(control).as_ref())
            .query_async(&mut self.conn()?)
            .await
            .map_err(unavailable)?;
        Ok(())
    }
}

/// This node's command channel: subscribe, forward, reconnect. A command
/// sent while the connection is down is lost (ADR 0011 R4-1; R5 reconciles).
async fn run_control(client: redis::Client, channel: String, incoming: mpsc::Sender<Incoming>) {
    let mut wait = RECONNECT_FIRST;
    loop {
        let (mut sink, mut stream) = match client.get_async_pubsub().await {
            Ok(p) => p.split(),
            Err(e) => {
                tracing::warn!("redis broker: control connect failed: {e}");
                tokio::time::sleep(wait).await;
                wait = (wait * 2).min(RECONNECT_MAX);
                continue;
            }
        };
        if sink.subscribe(&channel).await.is_err() {
            tokio::time::sleep(wait).await;
            continue;
        }
        wait = RECONNECT_FIRST;
        while let Some(msg) = stream.next().await {
            match wire::decode_control(&Bytes::copy_from_slice(msg.get_payload_bytes())) {
                Ok(c) => {
                    let _ = incoming.send(Incoming::Control(c)).await;
                }
                Err(e) => tracing::warn!("redis broker: dropped a command: {e}"),
            }
        }
    }
}

impl RedisBroker {
    fn conn(&self) -> Result<ConnectionManager, BrokerError> {
        self.conn
            .get()
            .cloned()
            .ok_or_else(|| BrokerError::Unavailable("not started".into()))
    }

    fn kv_key(&self, key: &str) -> String {
        format!("{}:kv:{key}", self.cfg.prefix)
    }
}

/// `SET NX PX` / `GETDEL` (Redis 6.2+): a value is read at most once, by
/// whichever node asks first (ADR 0011 §6).
#[async_trait]
impl SharedKv for RedisBroker {
    async fn put_once(&self, key: &str, value: Bytes, ttl: Duration) -> Result<bool, BrokerError> {
        let set: Option<String> = redis::cmd("SET")
            .arg(self.kv_key(key))
            .arg(value.as_ref())
            .arg("NX")
            .arg("PX")
            .arg((ttl.as_millis() as u64).max(1))
            .query_async(&mut self.conn()?)
            .await
            .map_err(unavailable)?;
        Ok(set.is_some())
    }

    async fn take(&self, key: &str) -> Result<Option<Bytes>, BrokerError> {
        let v: Option<Vec<u8>> = redis::cmd("GETDEL")
            .arg(self.kv_key(key))
            .query_async(&mut self.conn()?)
            .await
            .map_err(unavailable)?;
        Ok(v.map(Bytes::from))
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
