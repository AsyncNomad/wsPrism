//! wsPrism gateway library (shared code for the binary and tests).

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]

pub mod app_state;
pub mod config;
pub mod router;
pub mod transport;
