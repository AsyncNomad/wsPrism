//! wsPrism gateway binary entrypoint.
//!
//! Bootstraps tracing, loads configuration, builds application state, and
//! starts the WebSocket server.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tracing_subscriber::{fmt, EnvFilter};

use wsprism_gateway::{app_state, config, server};

/// wsPrism realtime gateway.
#[derive(Debug, Parser)]
#[command(version, about)]
struct Cli {
    /// Path to the gateway configuration file.
    #[arg(long, short, env = "WSPRISM_CONFIG", default_value = "wsprism.yaml")]
    config: PathBuf,

    /// Validate the configuration (parse, check, compile tenant policies)
    /// and exit without serving.
    #[arg(long)]
    check_config: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    fmt().with_env_filter(EnvFilter::from_default_env()).init();
    let cli = Cli::parse();

    let state = match load(&cli) {
        Ok(state) => state,
        Err(e) => {
            eprintln!("{}: {}", cli.config.display(), e.client_message());
            return ExitCode::FAILURE;
        }
    };
    if cli.check_config {
        println!("{}: OK", cli.config.display());
        return ExitCode::SUCCESS;
    }

    let listen: SocketAddr = match state.cfg().gateway.listen.parse() {
        Ok(addr) => addr,
        Err(e) => {
            eprintln!("gateway.listen must be host:port: {e}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(listen).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("failed to bind {listen}: {e}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(%listen, config = %cli.config.display(), "wsprism-gateway starting");

    if let Err(e) = server::serve(listener, state, shutdown_signal()).await {
        eprintln!("server failed: {e}");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}

/// Parse + validate the config and build the runtime (compiles policies).
fn load(cli: &Cli) -> wsprism_core::Result<app_state::AppState> {
    let path = cli.config.to_string_lossy();
    let cfg = config::load_from_file(&path)?;
    app_state::AppState::new(cfg)
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
