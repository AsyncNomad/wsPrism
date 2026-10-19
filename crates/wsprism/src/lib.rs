//! # wsPrism
//!
//! A multi-tenant realtime gateway. This is the crate to depend on: it
//! re-exports the **stable API** (ADR 0009 §4) of the wsPrism crates.
//!
//! - Run the gateway from a YAML config: [`Gateway::builder`].
//! - Add your own logic as services ([`TextService`] for JSON requests,
//!   [`HotService`] for binary state frames) that get a [`RealtimeCtx`].
//! - Add your own authentication with [`GatewayBuilder::auth_plugin`]
//!   (`kind: plugin` in `tenants[].auth`).
//!
//! ```no_run
//! use wsprism::prelude::*;
//!
//! struct Echo;
//!
//! #[async_trait]
//! impl TextService for Echo {
//!     fn svc(&self) -> &'static str { "echo" }
//!     async fn handle(&self, ctx: &mut RealtimeCtx, env: Envelope) -> Result<()> {
//!         ctx.reply(wsprism::serde_json::json!({ "v": 1, "svc": "echo", "type": "reply" }))
//!     }
//! }
//!
//! # async fn run() -> Result<()> {
//! Gateway::builder()
//!     .config_file("wsprism.yaml")?
//!     .text_service(Echo)
//!     .build()?
//!     .serve()
//!     .await
//! # }
//! ```
//!
//! Anything not re-exported here (the engine's `spi`, gateway internals) is
//! outside the compatibility promise. Wire contracts (protocol v2, the
//! `/api/v1` server API, config `version: 1`) only ever grow — see
//! `docs/API.md`.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]
#![warn(missing_docs)]

pub use async_trait::async_trait;
/// Replies and payloads are `serde_json` values; use this re-export to stay
/// on the same version.
pub use serde_json;

pub use wsprism_gateway::{auth, config, AuthPluginFactory, Gateway, GatewayBuilder};

pub use wsprism_engine::{
    services, DeferredReply, HotService, Joined, Outgoing, Payload, Permissions, QoS, RealtimeCtx,
    RoomLimits, RoomPattern, TextService,
};

pub use wsprism_protocol::error::ClientCode;
pub use wsprism_protocol::protocol::hot::HotFrame;
pub use wsprism_protocol::protocol::text::Envelope;
pub use wsprism_protocol::protocol::ProtocolVersion;
pub use wsprism_protocol::{Result, WsPrismError};

/// Everything a service or a custom binary usually needs.
pub mod prelude {
    pub use crate::auth::{AuthError, AuthRequest, Authenticator, Identity};
    pub use crate::{
        async_trait, Envelope, Gateway, HotFrame, HotService, Outgoing, Payload, QoS, RealtimeCtx,
        Result, TextService, WsPrismError,
    };
}
