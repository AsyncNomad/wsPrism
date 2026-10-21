//! wsPrism gateway binary entrypoint.
//!
//! Bootstraps tracing, then builds and serves the gateway through the public
//! API only (`Gateway::builder`, ADR 0009 §3) — the same path an embedder uses.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

use wsprism_gateway::Gateway;

mod logging;

/// Optional global allocator (`--features mimalloc`), measured against the
/// system allocator before it becomes a default (performance-architecture §9).
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

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

    /// Log lines: readable text, or one JSON object per line for log
    /// pipelines. `RUST_LOG` sets the levels.
    #[arg(long, env = "WSPRISM_LOG_FORMAT", value_enum, default_value = "text")]
    log_format: logging::LogFormat,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    logging::init(cli.log_format);

    let gateway = match Gateway::builder()
        .config_file(&cli.config)
        .and_then(|b| b.build())
    {
        Ok(g) => g,
        Err(e) => {
            eprintln!("{}: {}", cli.config.display(), e.client_message());
            return ExitCode::FAILURE;
        }
    };
    if cli.check_config {
        println!("{}: OK", cli.config.display());
        return ExitCode::SUCCESS;
    }
    tracing::info!(config = %cli.config.display(), "wsprism-gateway starting");
    match gateway.serve().await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{}", e.client_message());
            ExitCode::FAILURE
        }
    }
}
