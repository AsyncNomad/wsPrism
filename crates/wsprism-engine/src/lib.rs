//! wsPrism realtime engine (ADR 0009): rooms and members, reliable / lossy
//! delivery, the per-session Hot slot, the service traits and the context
//! services get. It knows nothing about transports, the config file format,
//! HTTP or authentication mechanisms — the gateway crate builds on it.
//!
//! Like `wsprism-protocol`, panics/`unwrap`/`expect` are denied in library
//! code: errors surface as `WsPrismError`.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

pub mod dispatch;
pub mod limits;
pub mod permissions;
pub mod realtime;
pub mod services;
pub mod sys;

pub use limits::RoomLimits;
