//! Realtime core components for Gateway runtime.
//!
//! Session registry, presence tracking, and the egress runtime/context shared
//! across services.

mod presence;
mod realtime;
mod session_registry;

pub use presence::Presence;
pub use realtime::{RealtimeCore, RealtimeCtx};
pub use session_registry::{Connection, SessionRegistry};
