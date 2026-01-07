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

pub mod app_state;
pub mod config;
pub mod context;
pub mod policy;
pub mod router;
pub mod transport;
pub mod dispatch;
pub mod realtime;
pub mod services;
pub mod ops;
pub mod obs;
