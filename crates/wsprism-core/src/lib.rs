//! wsPrism core: transport-agnostic protocol primitives and shared error surface.
//!
//! This crate is intended to be reused by the gateway, services, and SDK-facing
//! tooling without pulling in transport or application-specific dependencies.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

pub mod error;
pub mod protocol;

/// Shared result type.
pub use error::{Result, WsPrismError};
