//! Realtime runtime (egress engine) for wsPrism Gateway.
//!
//! Sprint 3: SessionRegistry + Presence + QoS-based publish helpers.

pub mod core;
pub mod types;

pub use core::{Presence, RealtimeCore, RealtimeCtx, SessionRegistry};
pub use types::{Outgoing, Payload, PreparedMsg, QoS};
