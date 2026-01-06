//! wsPrism gateway binary entrypoint.
//!
//! Bootstraps tracing, loads configuration, builds application state, and
//! starts the WebSocket server.

use std::net::SocketAddr;
use tracing_subscriber::{fmt, EnvFilter};
use tokio::time::Instant;

use wsprism_gateway::{app_state, config, router};

#[tokio::main]
async fn main() {
    fmt().with_env_filter(EnvFilter::from_default_env()).init();

    // Config (strict parsing + validate already in Sprint 0)
    let cfg = config::load_from_file("wsprism.yaml").expect("config load failed");
    let listen: SocketAddr = cfg
        .gateway
        .listen
        .parse()
        .expect("gateway.listen must be a valid SocketAddr");

    let state = app_state::AppState::new(cfg).expect("failed to build app state");
    let app = router::build_router(state.clone());

    tracing::info!(%listen, "wsprism-gateway starting");
    let listener = tokio::net::TcpListener::bind(listen).await.expect("failed to bind");

    let drain_grace_ms = state.cfg().gateway.drain_grace_ms;
    
    // Sprint 5: Enable ConnectInfo for HandshakeDefender
    axum::serve(
        listener, 
        app.into_make_service_with_connect_info::<SocketAddr>()
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;

        tracing::info!("shutdown signal received; entering draining mode");
        state.enter_draining();
        state.realtime().best_effort_shutdown_all("draining");

        let deadline = Instant::now() + std::time::Duration::from_millis(drain_grace_ms);
        while Instant::now() < deadline {
            if state.realtime().sessions.len_sessions() == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("server failed");
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let term = async {
        use tokio::signal::unix::{signal, SignalKind};
        let mut sigterm = signal(SignalKind::terminate()).expect("install SIGTERM handler");
        let _ = sigterm.recv().await;
    };

    #[cfg(not(unix))]
    let term = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}
