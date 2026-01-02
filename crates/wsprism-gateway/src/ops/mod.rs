//! Operational HTTP endpoints.
//!
//! - `/healthz` : liveness
//! - `/readyz`  : readiness (503 when draining)
//! - `/metrics` : Prometheus text format

use axum::{http::StatusCode, response::{IntoResponse, Response}};

use crate::app_state::AppState;

pub async fn healthz() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

pub async fn readyz(axum::extract::State(state): axum::extract::State<AppState>) -> impl IntoResponse {
    if state.is_draining() {
        (StatusCode::SERVICE_UNAVAILABLE, "draining")
    } else {
        (StatusCode::OK, "ready")
    }
}

pub async fn metrics(axum::extract::State(state): axum::extract::State<AppState>) -> Response {
    let extra = state.metrics_extra();
    let body = state.metrics().render(&extra);

    (
        StatusCode::OK,
        [(axum::http::header::CONTENT_TYPE, "text/plain; version=0.0.4; charset=utf-8")],
        body,
    )
        .into_response()
}
