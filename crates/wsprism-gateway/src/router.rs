//! Axum router wiring.
//!
//! Exposes:
//! - `/v1/ws`    : WebSocket upgrade
//! - `/healthz`  : liveness
//! - `/readyz`   : readiness
//! - `/metrics`  : Prometheus metrics, only with `gateway.public_metrics`
//!   (otherwise on the internal listener, see `api::router`)

use axum::{routing::get, Router};

use crate::{app_state::AppState, ops, transport};

pub fn build_router(state: AppState) -> Router {
    let router = Router::new()
        .route("/v1/ws", get(transport::ws::ws_upgrade))
        .route("/healthz", get(ops::healthz))
        .route("/readyz", get(ops::readyz));
    let router = if state.cfg().gateway.public_metrics {
        router.route("/metrics", get(ops::metrics))
    } else {
        router
    };
    router.with_state(state)
}
