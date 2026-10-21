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
//! Public API: [`Gateway`] / [`GatewayBuilder`], [`config`] and [`auth`]
//! (ADR 0009). The binary (`main.rs`) uses nothing else.
//! Like `wsprism-protocol`, panics/`unwrap`/`expect` are denied in library code
//! (enforced by `cargo clippy` in CI). The binary entrypoint is exempt.

#![deny(clippy::unwrap_used)]
#![deny(clippy::expect_used)]
#![deny(clippy::panic)]

// Public API (ADR 0009 §4): the builder, the config file schema and the
// authentication extension point. Everything else is internal.
pub mod auth;
pub mod config;
mod gateway;

pub use gateway::{AuthPluginFactory, Gateway, GatewayBuilder};

pub(crate) mod api;
pub(crate) mod app_state;
pub(crate) mod backend;
pub(crate) mod ident;
pub(crate) mod obs;
pub(crate) mod ops;
pub(crate) mod policy;
pub(crate) mod router;
pub(crate) mod rpc;
pub(crate) mod server;
pub(crate) mod transport;

/// Internals for this repository's tests, benchmarks and fuzz targets
/// (`--features internals`). Not an API: changes without notice.
#[cfg(feature = "internals")]
#[doc(hidden)]
pub mod __internal {
    pub use crate::app_state::AppState;
    pub use crate::obs::metrics::GatewayMetrics;
    pub use crate::policy::origin::OriginPolicy;
    pub use crate::policy::TenantPolicyRuntime;
    pub use crate::transport::client_ip::TrustedProxies;
    pub use crate::transport::subprotocol::{Negotiated, Offered, V1, V2_JSON};
}
