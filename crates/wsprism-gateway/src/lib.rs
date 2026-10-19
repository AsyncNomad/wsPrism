//! wsPrism gateway library entry.
//!
//! This crate assembles the production gateway stack:
//! - Transport: Axum-based WebSocket upgrade with handshake defense, tenant caps,
//!   slow-consumer protection, and trace-id propagation.
//! - Policy: Allowlist, rate limiting, session/room governance, and hot-lane behavior.
//! - Dispatch: Routes ext/hot messages to registered services.
//! - Realtime core: Session/room registries, lossy/reliable egress.
//! - Observability: Labeled counters/gauges/histograms, sys.* envelopes with trace_id,
//!   and /metrics exposure via ops endpoints.
//! - Ops: /healthz, /readyz, /metrics, graceful drain.
//! - Built-ins: room/sys/chat services provided out-of-the-box; additional
//!   services can be registered via dispatcher modules.
//!
//! The gateway is designed for panic-free operation: upstream errors surface as
//! structured `WsPrismError` responses instead of crashing the process.
//! This crate is consumed by the binary (`main.rs`) and by integration tests.
//! Like `wsprism-protocol`, panics/`unwrap`/`expect` are denied in library code
//! (enforced by `cargo clippy` in CI). The binary entrypoint is exempt.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

pub mod api;
pub mod app_state;
pub mod auth;
pub mod backend;
pub mod config;
pub mod context;
pub mod ident;
pub mod obs;
pub mod ops;
pub mod policy;
pub mod router;
pub mod rpc;
pub mod server;
pub mod transport;
