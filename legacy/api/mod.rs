pub mod ws;

use axum::Router;
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    Router::new()
        .merge(ws::router())
        .with_state(state)
}
