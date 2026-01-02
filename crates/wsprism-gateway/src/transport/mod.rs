//! Transport layer (WebSocket).
//!
//! Exposes the WS upgrade handler and codec that decodes messages once before
//! they reach policy/dispatcher layers.

pub mod codec;
pub mod ws;
