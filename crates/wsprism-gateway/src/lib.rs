//! The wsPrism gateway: everything between a socket and the realtime engine.
//!
//! - **Transport:** WebSocket upgrade behind an admission pipeline: handshake
//!   rate limits (per IP, per IPv6 prefix, global), trusted proxies,
//!   browser `Origin` checks, protocol negotiation (v2 and v1), tenant
//!   capacity and, in a cluster, overload protection.
//! - **Authentication:** a chain per tenant: one-time tickets, JWT (pinned
//!   algorithm, JWKS, key rotation), a connect proxy to your backend, and
//!   plugins; in-band token refresh; per-user room permissions.
//! - **Sessions:** one task per connection that never waits on its own
//!   outbound queue; sid takeover and per-user limits decided atomically;
//!   slow consumers closed (reliable) or skipped (lossy).
//! - **Policy:** allowlists, rate limits, frame sizes, room limits, Hot lane
//!   behaviour, presence events, room history.
//! - **Backends:** the server API on an internal listener (publish,
//!   disconnect, tickets, presence) and the RPC proxy.
//! - **Cluster:** the Redis broker (`wsprism-redis`, feature `redis`).
//! - **Operations:** `/healthz`, `/readyz`, Prometheus `/metrics`, JSON logs
//!   with identity hashing, graceful draining.
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
