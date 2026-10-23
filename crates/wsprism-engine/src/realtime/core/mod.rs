//! Realtime core components for Gateway runtime.
//!
//! Session registry, presence tracking, and the egress runtime/context shared
//! across services.

mod history;
mod hot_slot;
mod presence;
mod realtime;
mod session_registry;
mod signals;

pub use history::{HistoryConfig, HistoryPos, HistoryRule, Recover};
pub use hot_slot::{HotSlot, Put, SlotKey, DEFAULT_SLOT_CAP};
pub use presence::{Departed, Joined, Member, Presence};
pub use realtime::{
    BrokerTasks, CtxIdentity, DeferredReply, EgressCounters, JoinOutcome, MemberInfo,
    MemberSnapshot, Publisher, RealtimeCore, RealtimeCtx, MAX_SNAPSHOT_MEMBERS,
};
pub use session_registry::{
    Admitted, Connection, Evicted, NewSession, Refusal, SessionRegistry, UserLimit,
};
pub use signals::{now_ms, CloseRequest, SessionSignals};
