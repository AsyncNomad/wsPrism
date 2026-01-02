//! Axum router wiring.
//!
//! Exposes:
//! - `/v1/ws`    : WebSocket upgrade
//! - `/healthz`  : liveness
//! - `/readyz`   : readiness
//! - `/metrics`  : Prometheus metrics

use axum::{routing::get, Router};

use crate::{app_state::AppState, ops, transport};

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/ws", get(transport::ws::ws_upgrade))
        .route("/healthz", get(ops::healthz))
        .route("/readyz", get(ops::readyz))
        .route("/metrics", get(ops::metrics))
        .with_state(state)
}
