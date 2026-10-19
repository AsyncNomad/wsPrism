//! Realtime runtime (egress engine) for wsPrism Gateway.
//!
//! Provides session registry, presence tracking, QoS-aware publishing helpers,
//! and per-message context passed to services.

pub(crate) mod core;
pub(crate) mod types;

pub(crate) use core::RealtimeCtx;
pub(crate) use types::{HotOut, Outgoing, Payload, QoS};
