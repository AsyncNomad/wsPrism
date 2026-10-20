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
