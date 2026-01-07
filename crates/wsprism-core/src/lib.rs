//! wsPrism core: transport-agnostic protocol primitives, error types, and flags.
//!
//! This crate defines the wire-level contracts and error surface shared by the
//! gateway, services, and SDK tooling. It intentionally carries no transport or
//! runtime dependencies so it can be reused in multiple contexts.
//!
//! # Defensive guarantees
//! Panics, `unwrap`, and `expect` are compile-denied here
//! (`#![deny(clippy::panic, clippy::unwrap_used, clippy::expect_used)]`).
//! All fallible paths must surface as `WsPrismError`/`Result` so production
//! processes do not crash on malformed input or bad traffic.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

pub mod error;
pub mod protocol;

/// Shared result type.
pub use error::{Result, WsPrismError};
