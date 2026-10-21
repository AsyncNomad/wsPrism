//! Transport layer (WebSocket).
//!
//! Exposes the WS upgrade handler and codec that decodes messages once before
//! they reach policy/dispatcher layers.

pub mod admission;
pub mod client_ip;
pub mod codec;
pub mod credentials;
pub mod handshake;
pub(crate) mod session;
pub mod subprotocol;
pub mod ws;
