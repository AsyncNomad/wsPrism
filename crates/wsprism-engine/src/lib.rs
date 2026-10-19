//! wsPrism realtime engine (ADR 0009): rooms and members, reliable / lossy
//! delivery, the per-session Hot slot, the service traits and the context
//! services get. It knows nothing about transports, the config file format,
//! HTTP or authentication mechanisms — the gateway crate builds on it.
//!
//! # API levels
//! - The crate root is the **stable API** for writing services:
//!   [`TextService`], [`HotService`], [`RealtimeCtx`], [`Outgoing`] and the
//!   built-in [`services`].
//! - [`spi`] is for building a server or transport on the engine (the
//!   gateway uses it). It is **not covered by semver** in 0.x.
//!
//! Like `wsprism-protocol`, panics/`unwrap`/`expect` are denied in library
//! code: errors surface as `WsPrismError`.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![warn(unreachable_pub)]
#![warn(missing_docs)]

mod dispatch;
mod limits;
mod permissions;
mod realtime;
pub mod services;
mod sys;

pub use dispatch::{HotService, TextService};
pub use limits::RoomLimits;
pub use permissions::{Permissions, RoomPattern};
pub use realtime::core::{DeferredReply, Joined, RealtimeCtx};
pub use realtime::types::{Outgoing, Payload, QoS};

/// Server / transport provider interface: what the gateway (or another
/// transport) needs to run sessions on the engine. **Unstable**: may change
/// in any 0.x release without notice. Service authors don't need it.
pub mod spi {
    pub use crate::dispatch::Dispatcher;
    pub use crate::realtime::core::{
        egress_drop_count, egress_slow_consumer_count, hot_slot_full_count,
        hot_slot_overwrite_count, now_ms, CloseRequest, Connection, CtxIdentity, HotSlot, Member,
        Presence, Put, RealtimeCore, SessionRegistry, SessionSignals, SlotKey, DEFAULT_SLOT_CAP,
    };
    pub use crate::realtime::types::{close_code, CloseSpec, HotOut, OutFrame, Utf8Text};

    /// Builders for the gateway's `sys.*` messages.
    pub mod sys {
        pub use crate::sys::*;
    }
}
