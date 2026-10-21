//! Serving the gateway on a bound listener: socket options, connect-info and
//! graceful drain. `Gateway::serve_on` (binary, embedders, tests) runs
//! this, so tests exercise the same server stack that ships.

use std::future::Future;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::TcpListener;
use tokio::time::Instant;

use crate::app_state::AppState;
use crate::router;

/// Serve until `shutdown` completes, then drain (see [`drain`]); also the
/// server API (ADR 0008 §3) on `api` if given.
/// The API listener stops with the process; it is not drained.
///
/// A cluster node whose slot another node took (it was cut off longer than
/// `slot_ttl_s`; the other node may issue the same member ids) drains on its
/// own and returns an error: it must not keep serving with that slot
/// (fencing, first step; pre-release review). The binary then exits with a
/// failure for the orchestrator to restart it.
pub async fn serve_with_api(
    listener: TcpListener,
    api: Option<TcpListener>,
    state: AppState,
    shutdown: impl Future<Output = ()> + Send + 'static,
) -> std::io::Result<()> {
    // Broker first (connect, claim the node slot; ADR 0011): member ids
    // carry the slot, so nobody may join before it is known.
    let _broker = state
        .realtime()
        .start()
        .await
        .map_err(|e| std::io::Error::other(format!("broker: {}", e.client_message())))?;
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
    let fenced = Arc::new(AtomicBool::new(false));
    let stop = {
        let (state, fenced) = (state.clone(), Arc::clone(&fenced));
        async move {
            tokio::select! {
                () = shutdown => {}
                () = slot_lost(state) => fenced.store(true, Ordering::Relaxed),
            }
        }
    };
    axum::serve(
        listener,
        // ConnectInfo feeds the handshake defender's per-IP limiter.
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .tcp_nodelay(socket.tcp_nodelay)
    .with_graceful_shutdown(drain(state.clone(), stop))
    .await?;
    if fenced.load(Ordering::Relaxed) {
        return Err(std::io::Error::other(
            "another node took this node's cluster slot (cut off from Redis longer than \
             slot_ttl_s); stopped serving: restart this node",
        ));
    }
    Ok(())
}

/// How often the node checks that it still owns its cluster slot (the
/// broker's heartbeat finds out; this only reads its flag).
const FENCE_CHECK: Duration = Duration::from_millis(250);

/// Completes when the broker reports that this node lost its slot (never
/// for one node).
async fn slot_lost(state: AppState) {
    let mut tick = tokio::time::interval(FENCE_CHECK);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        if !state.realtime().broker().healthy() {
            tracing::error!(
                "this node's cluster slot was taken by another node: closing every session and stopping"
            );
            return;
        }
    }
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
        // A pass visits every session (O(n) under shard read locks): run it on
        // the blocking pool so a million sessions don't stall an async worker
        // for tens of milliseconds every second. (Yielding mid-pass would hold
        // a shard lock across an await instead.)
        let (realtime, tickets) = (state.realtime(), state.tickets());
        let pass = tokio::task::spawn_blocking(move || {
            realtime.sweep(idle_ms, ping_ms);
            tickets.sweep(std::time::Instant::now());
        });
        if pass.await.is_err() {
            tracing::error!("sweeper pass panicked");
        }
    }
}

/// Aborts the task when the server future ends.
struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// After `shutdown`: stop admitting (readiness 503, upgrades rejected,
/// server API publishes 503 so back-ends move to another node), ask every
/// session to close with 1001, wait for the registry to empty, then for the
/// cluster broker's queue to be handed over (publishes already answered 200
/// and the closed sessions' membership removals; M6) — all within
/// `drain_grace_ms`.
async fn drain(state: AppState, shutdown: impl Future<Output = ()>) {
    shutdown.await;
    tracing::info!("shutdown signal received; entering draining mode");
    state.enter_draining();
    state.realtime().best_effort_shutdown_all("draining");

    let grace = Duration::from_millis(state.cfg().gateway.drain_grace_ms);
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline && state.realtime().sessions.len_sessions() > 0 {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    state.realtime().flush(deadline).await;
}
