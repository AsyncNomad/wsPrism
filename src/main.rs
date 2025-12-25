use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};

use wsprism::{
    api::router,
    infra::{InMemoryTicketStore, TicketStore},
    realtime::{core::Dispatcher, services},
    state::AppState,
};

#[tokio::main]
async fn main() {
    tracing_subscriber::registry()
        .with(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .with(tracing_subscriber::fmt::layer())
        .init();

    let dispatcher = Dispatcher::new();
    dispatcher.register(services::chat::ChatService::new());
    dispatcher.register(services::gameplay::GameplayService::new());

    // Binary routing in this skeleton: svc_id=1 => gameplay
    dispatcher.register_binary_id(1, services::gameplay::GameplayService::new());

    let ticket_store: std::sync::Arc<dyn TicketStore> = std::sync::Arc::new(InMemoryTicketStore::new());

    let state = AppState::new(dispatcher, ticket_store);
    let app = router(state);

    let addr = "0.0.0.0:8080";
    tracing::info!("listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .unwrap();
}

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install Ctrl+C handler");
    };

    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install signal handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("signal received, starting graceful shutdown");
}
