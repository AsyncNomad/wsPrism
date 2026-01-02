//! Dispatcher module exports.
//!
//! Re-exports the dispatcher and service traits so downstream consumers can
//! depend on this module directly.

pub mod dispatcher;

pub use dispatcher::{BinaryService, Dispatcher, TextService};
