//! Serving the gateway on a bound listener: socket options, connect-info and
//! graceful drain. `Gateway::serve_on` (binary, embedders, tests) runs
//! this, so tests exercise the same server stack that ships.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::time::Instant;

use crate::app_state::AppState;
use crate::router;

/// Serve until `shutdown` completes, then drain (see [`drain`]); also the
/// server API (ADR 0008 §3) on `api` if given.
/// The API listener stops with the process; it is not drained.
pub async fn serve_with_api(
    listener: TcpListener,
    api: Option<TcpListener>,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    let _api = api.map(|l| {
        let app = crate::api::router(state.clone());
        AbortOnDrop(tokio::spawn(async move {
            if let Err(e) = axum::serve(l, app).await {
                tracing::error!("server api stopped: {e}");
            }
        }))
    });
    let app = router::build_router(state.clone());
    let _sweeper = AbortOnDrop(tokio::spawn(sweeper(state.clone())));
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

/// How often the sweeper visits every session (idle check granularity and
/// keep-alive jitter).
const SWEEP_INTERVAL: Duration = Duration::from_secs(1);

/// One task for all sessions' keep-alive pings and idle timeouts, instead of
/// two timers per session (performance-architecture §5).
async fn sweeper(state: AppState) {
    let gw = &state.cfg().gateway;
    let (idle_ms, ping_ms) = (gw.idle_timeout_ms, gw.ping_interval_ms);
    let mut tick = tokio::time::interval(SWEEP_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        state.realtime().sweep(idle_ms, ping_ms);
        state
            .tickets()
            .sweep(tokio::time::Instant::now().into_std());
    }
}

/// Aborts the task when the server future ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
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
