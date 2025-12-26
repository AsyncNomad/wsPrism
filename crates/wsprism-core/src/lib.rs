//! wsPrism core: shared protocol + error types (transport-agnostic).

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

pub mod error;
pub mod protocol;

/// Shared result type.
pub use error::{Result, WsPrismError};
