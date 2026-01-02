//! Realtime core components for Gateway runtime.
//!
//! Session registry, presence tracking, and the egress runtime/context shared
//! across services.

mod presence;
mod realtime;
mod session_registry;

pub use presence::Presence;
pub use realtime::{egress_drop_count, egress_send_fail_count, RealtimeCore, RealtimeCtx};
pub use session_registry::{Connection, SessionRegistry};
