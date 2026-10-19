//! Realtime core components for Gateway runtime.
//!
//! Session registry, presence tracking, and the egress runtime/context shared
//! across services.

mod hot_slot;
mod presence;
mod realtime;
mod session_registry;
mod signals;

pub use hot_slot::{HotSlot, Put, SlotKey, DEFAULT_SLOT_CAP};
pub use presence::{Departed, Joined, Member, Presence};
pub use realtime::{
    CtxIdentity, DeferredReply, EgressCounters, MemberInfo, MemberSnapshot, Publisher,
    RealtimeCore, RealtimeCtx, MAX_SNAPSHOT_MEMBERS,
};
pub use session_registry::{Connection, SessionRegistry};
pub use signals::{now_ms, CloseRequest, SessionSignals};
