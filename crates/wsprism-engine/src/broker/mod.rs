//! The broker boundary (ADR 0011): how room traffic reaches other nodes.
//!
//! **Experimental.** The trait will gain history, shared storage, session
//! ownership and control operations (ADR 0011 B6) and may change in any
//! release until the Redis broker (R3) ships. Do not implement it outside
//! this repository yet.
//!
//! # Modes
//! - [`BrokerMode::Inline`] ([`MemoryBroker`], the default): one node. The
//!   engine delivers locally and never calls the broker, so a single node
//!   pays nothing for the boundary.
//! - [`BrokerMode::Queued`]: room frames go through the engine's outbox (a
//!   bounded queue with per-tenant shares) to one engine task that calls
//!   [`Broker::publish`] in order. Ext frames reach local members only
//!   through [`Broker::incoming`] (the cluster sees one order); Hot frames are
//!   delivered locally first and also published (latency first; ADR 0011 §2).

mod outbox;
#[cfg(any(test, feature = "testing"))]
pub mod testing;
pub mod wire;

use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::realtime::types::{OutFrame, QoS};

pub(crate) use outbox::{run_outbox, BrokerOutbox, Subscribed};
pub use outbox::{OutboxCounters, OutboxLimits};

/// How the engine uses a broker (see the module docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrokerMode {
    /// Single node: deliver locally, never call the broker.
    Inline,
    /// Room frames go through the outbox and come back via `incoming`.
    Queued,
}

/// Bits of a member id taken by the node slot (ADR 0011 B1).
pub const NODE_SLOT_BITS: u32 = 10;
/// Highest node slot (1,024 nodes).
pub const MAX_NODE_SLOT: u16 = (1 << NODE_SLOT_BITS) - 1;

/// One room frame on its way between nodes. Encoded once by the publisher;
/// cloning shares the bytes.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BrokerFrame {
    /// Node slot of the publishing node.
    pub origin: u16,
    /// Tenant id (the outbox's quota key).
    pub tenant: Arc<str>,
    /// Internal room key (`tenant::room`).
    pub room_key: Arc<str>,
    /// Delivery semantics at each recipient.
    pub qos: QoS,
    /// The encoded frame (`Text`, `Binary` or `Hot`).
    pub body: OutFrame,
    /// A session of the origin node to skip (e.g. the member a
    /// `member_joined` is about). Ignored by other nodes.
    pub except: Option<Arc<str>>,
    /// Keep this message in the room's history (ADR 0011 §3): the broker
    /// gives it the room's next offset. Text frames only.
    pub keep: Option<Keep>,
    /// The offset the broker gave a kept message (incoming frames).
    pub offset: Option<u64>,
    /// The history epoch that offset belongs to (incoming kept frames): a
    /// change means the history was lost and offsets restarted (R5).
    pub epoch: Option<Arc<str>>,
    /// Rises with every frame this node queues (0 = not numbered): lets a
    /// broker apply a retried frame once (R5: a write that timed out may
    /// still have happened).
    pub seq: u64,
}

/// How a history room keeps messages (from the tenant's `history` rule).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub struct Keep {
    /// Messages kept.
    pub size: usize,
    /// How long one is kept.
    pub ttl: std::time::Duration,
    /// Bytes kept.
    pub max_bytes: usize,
}

impl Keep {
    /// Keep `size` messages for `ttl`, at most `max_bytes`.
    pub fn new(size: usize, ttl: std::time::Duration, max_bytes: usize) -> Self {
        Self {
            size,
            ttl,
            max_bytes,
        }
    }
}

/// A history room's position and, if asked and possible, what a client
/// missed ([`Broker::history`]).
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct HistoryRead {
    /// The history's lifetime id.
    pub epoch: Arc<str>,
    /// The latest offset (0 = none yet).
    pub offset: u64,
    /// `Some` = the client can resume: these frames (with offsets), in order.
    /// `None` = it cannot (or did not ask).
    pub replay: Option<Vec<BrokerFrame>>,
}

impl HistoryRead {
    /// Position `epoch`/`offset` with `replay`.
    pub fn new(epoch: Arc<str>, offset: u64, replay: Option<Vec<BrokerFrame>>) -> Self {
        Self {
            epoch,
            offset,
            replay,
        }
    }
}

impl BrokerFrame {
    /// A frame published by node `origin`. `room_key` is `tenant::room`.
    pub fn new(origin: u16, room_key: &str, qos: QoS, body: OutFrame) -> Self {
        let tenant = room_key.split_once("::").map_or(room_key, |(t, _)| t);
        Self {
            origin,
            tenant: Arc::from(tenant),
            room_key: Arc::from(room_key),
            qos,
            body,
            except: None,
            keep: None,
            offset: None,
            epoch: None,
            seq: 0,
        }
    }

    /// Skip session `except` of the origin node.
    pub fn except(mut self, except: Option<&str>) -> Self {
        self.except = except.map(Arc::from);
        self
    }
}

/// This node starts or stops needing a room's traffic (ADR 0011 B5). The
/// engine records these in order, under the lock that creates or drops the
/// room, so applying them in order always ends in the right state.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Subscription {
    /// The room got its first local member.
    Subscribe(Arc<str>),
    /// The room lost its last local member.
    Unsubscribe(Arc<str>),
}

/// What a broker hands back to the engine.
#[derive(Debug)]
#[non_exhaustive]
pub enum Incoming {
    /// A room frame (from any node, this one included).
    Frame(BrokerFrame),
    /// Frames of these rooms (internal keys) may have been lost — the
    /// subscription was interrupted and is back. Their members get
    /// `sys.gap` (ADR 0011 §4).
    Gap(Vec<Arc<str>>),
    /// A command for this node from another (ADR 0011 R4-1).
    Control(Control),
}

/// A command one node sends another (ADR 0011 R4-1).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum Control {
    /// Close these local sessions (internal keys): `sys.kicked` with
    /// `message`, then the close for `reason` (`replaced`, `disconnected`,
    /// ...). Unknown keys are ignored.
    Close {
        /// Session keys.
        keys: Vec<Arc<str>>,
        /// The close reason (sets the close code).
        reason: String,
        /// Shown to the clients in `sys.kicked`.
        message: String,
    },
}

/// Why a broker call failed.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BrokerError {
    /// Temporary (connection down, overloaded): the engine retries the same
    /// call with backoff, keeping order. Frames queue up behind it and new
    /// Reliable publishes are refused with `UNAVAILABLE` once the outbox is
    /// full.
    Unavailable(String),
    /// Permanent for this call: the engine drops it and counts it.
    Rejected(String),
}

impl fmt::Display for BrokerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BrokerError::Unavailable(m) => write!(f, "broker unavailable: {m}"),
            BrokerError::Rejected(m) => write!(f, "broker rejected: {m}"),
        }
    }
}

impl std::error::Error for BrokerError {}

/// Fan-out between nodes (ADR 0011). **Experimental** — see the module docs.
///
/// The engine calls `publish` and `subscription` from one task, one call at
/// a time, in the order the frames and commands were recorded.
#[async_trait]
pub trait Broker: Send + Sync + 'static {
    /// Inline (single node) or queued.
    fn mode(&self) -> BrokerMode;

    /// Connect and claim this node's slot (0..=[`MAX_NODE_SLOT`]): the top
    /// bits of every member id it issues, so ids are unique across the
    /// cluster (ADR 0011 B1). Called once, before the node serves anyone.
    async fn start(&self) -> Result<u16, BrokerError> {
        Ok(0)
    }

    /// Send frames to every node subscribed to their rooms (this one too, for
    /// Ext frames). Queued mode only.
    async fn publish(&self, batch: &[BrokerFrame]) -> Result<(), BrokerError>;

    /// Apply one subscription change. Queued mode only.
    async fn subscription(&self, cmd: &Subscription) -> Result<(), BrokerError>;

    /// Apply several, in order for each room (different rooms are
    /// independent, so a broker may apply them concurrently). On an error
    /// the engine retries all of them: changes must be idempotent.
    /// Default: one by one.
    async fn subscriptions(&self, cmds: &[Subscription]) -> Result<(), BrokerError> {
        for cmd in cmds {
            self.subscription(cmd).await?;
        }
        Ok(())
    }

    /// A history room's position (`from` = `None`) or what was missed after
    /// `from` (ADR 0011 §3). Queued brokers that keep history implement it;
    /// the engine never calls it otherwise.
    async fn history(
        &self,
        _room_key: &str,
        _keep: &Keep,
        _from: Option<&crate::realtime::core::Recover>,
    ) -> Result<HistoryRead, BrokerError> {
        Err(BrokerError::Rejected("this broker keeps no history".into()))
    }

    /// The stream of incoming frames. Called once, when the engine starts;
    /// `None` for inline brokers.
    fn incoming(&self) -> Option<mpsc::Receiver<Incoming>>;

    /// Storage shared by every node (one-time tickets, ADR 0011 §6), if
    /// this broker has it.
    fn kv(&self) -> Option<&dyn SharedKv> {
        None
    }

    /// The cluster's session table (ADR 0011 R4-1), if this broker keeps
    /// one: then session limits and sid replacement are cluster-wide.
    fn sessions(&self) -> Option<&dyn SharedSessions> {
        None
    }

    /// The cluster's room membership (ADR 0011 R4-2), if this broker keeps
    /// it: then presence queries, join snapshots and leaves of a dead
    /// node's members are cluster-wide.
    fn presence(&self) -> Option<&dyn SharedPresence> {
        None
    }

    /// False once this node lost its slot to another node: its member ids
    /// may collide, so it should stop taking traffic (`/readyz` 503) and be
    /// restarted (ADR 0011 R5).
    fn healthy(&self) -> bool {
        true
    }

    /// False while the shared store is too slow or too busy to take new
    /// sessions: new connections are refused for a while (503 +
    /// `Retry-After`) instead of piling up behind it (overload protection,
    /// review #3). Connected sessions are not affected.
    fn admitting(&self) -> bool {
        true
    }
}

/// A change to the cluster's room membership, recorded in order through
/// the outbox (so it lands before this node's `member_joined` frame).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum PresenceOp {
    /// A session joined a room.
    Add {
        /// Internal room key.
        room_key: Arc<str>,
        /// Internal session key.
        session_key: Arc<str>,
        /// User id (without the tenant).
        user: Arc<str>,
        /// Its member id.
        member_id: u32,
        /// Tell the room when it leaves (tenant `presence_events`).
        notify: bool,
    },
    /// A session left a room.
    Remove {
        /// Internal room key.
        room_key: Arc<str>,
        /// Internal session key.
        session_key: Arc<str>,
    },
}

/// One room member in the cluster's membership.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct RoomMember {
    /// Internal session key.
    pub session_key: Arc<str>,
    /// User id (without the tenant).
    pub user: Arc<str>,
    /// Member id.
    pub member_id: u32,
}

impl RoomMember {
    /// A member.
    pub fn new(session_key: Arc<str>, user: Arc<str>, member_id: u32) -> Self {
        Self {
            session_key,
            user,
            member_id,
        }
    }
}

/// A member of a node that died, removed from the membership (exactly
/// once across the cluster): its room is told it left if `notify`.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct DeadMember {
    /// Internal room key.
    pub room_key: Arc<str>,
    /// User id.
    pub user: Arc<str>,
    /// Member id.
    pub member_id: u32,
    /// Send `member_left`.
    pub notify: bool,
}

impl DeadMember {
    /// A dead node's member.
    pub fn new(room_key: Arc<str>, user: Arc<str>, member_id: u32, notify: bool) -> Self {
        Self {
            room_key,
            user,
            member_id,
            notify,
        }
    }
}

/// The cluster's room membership (ADR 0011 R4-2).
#[async_trait]
pub trait SharedPresence: Send + Sync {
    /// Apply membership changes, in order.
    async fn apply(&self, ops: &[PresenceOp]) -> Result<(), BrokerError>;

    /// A room's live members on every node.
    async fn members(&self, room_key: &str) -> Result<Vec<RoomMember>, BrokerError>;

    /// Remove the members of nodes that died; each is returned by exactly
    /// one caller in the cluster.
    async fn reap_dead(&self) -> Result<Vec<DeadMember>, BrokerError>;

    /// Whether the cluster still knows this node. False after Redis lost
    /// its data or this node was taken for dead (cut off longer than its
    /// slot TTL): its sessions and memberships must then be put back.
    async fn registered(&self) -> Result<bool, BrokerError>;

    /// Make the cluster know this node again.
    async fn register(&self) -> Result<(), BrokerError>;
}

/// What to do when a user is at the session limit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum OnFull {
    /// Refuse the new session.
    Deny,
    /// Close the user's oldest session.
    KickOldest,
}

/// A new session asking for its place in the cluster's session table.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionClaim {
    /// `tenant::user`.
    pub user_key: Arc<str>,
    /// The new session's internal key (unique per connection).
    pub session_key: Arc<str>,
    /// Its client-facing sid: a live session with the same sid is replaced.
    pub sid: Arc<str>,
    /// The user's session limit.
    pub max_per_user: u32,
    /// What happens at the limit.
    pub on_full: OnFull,
}

impl SessionClaim {
    /// A claim for `session_key` of `user_key` with client sid `sid`.
    pub fn new(
        user_key: &str,
        session_key: &str,
        sid: &str,
        max_per_user: u32,
        on_full: OnFull,
    ) -> Self {
        Self {
            user_key: Arc::from(user_key),
            session_key: Arc::from(session_key),
            sid: Arc::from(sid),
            max_per_user,
            on_full,
        }
    }
}

/// A session that has to go, and where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Evicted {
    /// Node slot of its node.
    pub node: u16,
    /// Its internal key.
    pub session_key: Arc<str>,
    /// `replaced` or `max_sessions_exceeded`.
    pub reason: &'static str,
}

impl Evicted {
    /// `session_key` on node `node`, evicted for `reason`.
    pub fn new(node: u16, session_key: Arc<str>, reason: &'static str) -> Self {
        Self {
            node,
            session_key,
            reason,
        }
    }
}

/// The outcome of a [`SessionClaim`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Claimed {
    /// False = refused (the user is at its limit and `on_full` is `Deny`).
    pub admitted: bool,
    /// Sessions to close (on any node), whether or not this one was admitted.
    pub evicted: Vec<Evicted>,
}

impl Claimed {
    /// An outcome.
    pub fn new(admitted: bool, evicted: Vec<Evicted>) -> Self {
        Self { admitted, evicted }
    }
}

/// One live session in the cluster's table.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct SessionEntry {
    /// Node slot of its node.
    pub node: u16,
    /// Its internal key.
    pub session_key: Arc<str>,
    /// Its client-facing sid.
    pub sid: Arc<str>,
}

impl SessionEntry {
    /// An entry.
    pub fn new(node: u16, session_key: Arc<str>, sid: Arc<str>) -> Self {
        Self {
            node,
            session_key,
            sid,
        }
    }
}

/// The cluster's session table and node-to-node commands (ADR 0011 R4-1).
#[async_trait]
pub trait SharedSessions: Send + Sync {
    /// Take a place for a new session: atomically drop entries of dead
    /// nodes, replace a session with the same sid, apply the limit.
    async fn claim(&self, claim: &SessionClaim) -> Result<Claimed, BrokerError>;

    /// The session ended (idempotent).
    async fn release(&self, user_key: &str, session_key: &str) -> Result<(), BrokerError>;

    /// The user's live sessions on every node.
    async fn of_user(&self, user_key: &str) -> Result<Vec<SessionEntry>, BrokerError>;

    /// Send `control` to node `node` (this one included).
    async fn send_control(&self, node: u16, control: &Control) -> Result<(), BrokerError>;

    /// Of these (user key, session key) of this node, the ones the table no
    /// longer has: they were replaced or kicked while the command to close
    /// them was lost (ADR 0011 R5).
    async fn missing(
        &self,
        sessions: &[(Arc<str>, Arc<str>)],
    ) -> Result<Vec<Arc<str>>, BrokerError>;

    /// Put this node's sessions (user key, session key, sid) back after the
    /// table lost them (Redis restarted; ADR 0011 R5).
    async fn restore(&self, sessions: &[(Arc<str>, Arc<str>, Arc<str>)])
        -> Result<(), BrokerError>;
}

/// Small values shared by every node, each read at most once (ADR 0011
/// §6: one-time tickets). Keys are the caller's; the broker namespaces them.
#[async_trait]
pub trait SharedKv: Send + Sync {
    /// Store `value` under `key` for `ttl`, unless the key exists (false).
    async fn put_once(
        &self,
        key: &str,
        value: bytes::Bytes,
        ttl: std::time::Duration,
    ) -> Result<bool, BrokerError>;

    /// Remove and return the value under `key` (atomic: one caller wins).
    async fn take(&self, key: &str) -> Result<Option<bytes::Bytes>, BrokerError>;
}

/// The single-node broker (default): inline, so the engine never calls it.
#[derive(Debug, Default, Clone, Copy)]
pub struct MemoryBroker;

#[async_trait]
impl Broker for MemoryBroker {
    fn mode(&self) -> BrokerMode {
        BrokerMode::Inline
    }

    async fn publish(&self, _batch: &[BrokerFrame]) -> Result<(), BrokerError> {
        Ok(())
    }

    async fn subscription(&self, _cmd: &Subscription) -> Result<(), BrokerError> {
        Ok(())
    }

    fn incoming(&self) -> Option<mpsc::Receiver<Incoming>> {
        None
    }
}
