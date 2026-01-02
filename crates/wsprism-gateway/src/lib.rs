//! wsPrism gateway library entry.
//!
//! This crate wires the transport, policy, dispatcher, realtime core, and
//! built-in services into a cohesive gateway stack. It is intended to be
//! consumed by the binary (`main.rs`) and by integration tests.

pub mod app_state;
pub mod config;
pub mod context;
pub mod policy;
pub mod router;
pub mod transport;
pub mod dispatch;
pub mod realtime;
pub mod services;
