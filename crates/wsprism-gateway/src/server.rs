//! Serving the gateway on a bound listener: socket options, connect-info and
//! graceful drain. The binary and the integration tests both use this, so
//! tests exercise the same server stack that ships.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::time::Instant;

use crate::app_state::AppState;
use crate::router;

/// Serve until `shutdown` completes, then drain (see [`drain`]).
pub async fn serve(
    listener: TcpListener,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let app = router::build_router(state.clone());
    let socket = &state.cfg().gateway.socket;
    axum::serve(
        listener,
        // ConnectInfo feeds the handshake defender's per-IP limiter.
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .tcp_nodelay(socket.tcp_nodelay)
    .with_graceful_shutdown(drain(state.clone(), shutdown))
    .await
}

/// After `shutdown`: stop admitting (readiness 503, upgrades rejected), ask
/// every session to close with 1001, and wait up to `drain_grace_ms` for the
/// registry to empty.
async fn drain(state: AppState, shutdown: impl Future<Output = ()>) {
    shutdown.await;
    tracing::info!("shutdown signal received; entering draining mode");
    state.enter_draining();
    state.realtime().best_effort_shutdown_all("draining");

    let deadline = Instant::now() + Duration::from_millis(state.cfg().gateway.drain_grace_ms);
    while Instant::now() < deadline && state.realtime().sessions.len_sessions() > 0 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
