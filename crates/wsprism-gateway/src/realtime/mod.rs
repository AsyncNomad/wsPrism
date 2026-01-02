//! Realtime runtime (egress engine) for wsPrism Gateway.
//!
//! Provides session registry, presence tracking, QoS-aware publishing helpers,
//! and per-message context passed to services.

pub mod core;
pub mod types;

pub use core::{Presence, RealtimeCore, RealtimeCtx, SessionRegistry};
pub use types::{Outgoing, Payload, PreparedMsg, QoS};
