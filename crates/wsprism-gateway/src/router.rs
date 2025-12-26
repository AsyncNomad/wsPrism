//! Axum router wiring (HTTP -> WS upgrade).

use axum::{routing::get, Router};

use crate::{app_state::AppState, transport};

pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/ws", get(transport::ws::ws_upgrade))
        .with_state(state)
}
