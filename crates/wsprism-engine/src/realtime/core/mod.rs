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
pub use presence::{Joined, Member, Presence};
pub use realtime::{
    egress_drop_count, egress_slow_consumer_count, hot_slot_full_count, hot_slot_overwrite_count,
    CtxIdentity, DeferredReply, Publisher, RealtimeCore, RealtimeCtx,
};
pub use session_registry::{Connection, SessionRegistry};
pub use signals::{now_ms, CloseRequest, SessionSignals};
