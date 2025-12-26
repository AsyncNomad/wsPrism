//! wsPrism Gateway (Sprint 1)
//!
//! Focus: transport & lifecycle
//! - WebSocket endpoint: /v1/ws?tenant=...&ticket=...
//! - Decode-once pipeline: WS Message -> Inbound (Text Envelope / HotFrame)
//! - Tracing span per session
//! - Heartbeat ping + idle timeout

use std::net::SocketAddr;
use tracing_subscriber::{fmt, EnvFilter};

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

    let state = app_state::AppState::new(cfg);
    let app = router::build_router(state);

    tracing::info!(%listen, "wsprism-gateway starting");
    let listener = tokio::net::TcpListener::bind(listen).await.expect("failed to bind");

    axum::serve(listener, app).await.expect("server failed");
}
