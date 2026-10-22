//! wsPrism load generator.
//!
//! - `gateway`: fan-out latency/throughput against the wsPrism gateway.
//! - `churn`: sessions connecting, joining and leaving (wsPrism).
//! - `statesync`: state-sync freshness (Age of Information) and server cost
//!   over WebSocket, WebSocket+TLS or WebTransport datagrams (validation V,
//!   `docs/design/hot-lane-over-webtransport.md` §7).

mod cost;
mod gateway;
mod schedule;
mod statesync;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(about = "wsPrism load generator")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Fan-out latency and throughput against the wsPrism gateway.
    Gateway(gateway::Args),
    /// Session churn: connect, join, leave, as fast as `--workers` allow.
    Churn(gateway::churn::Args),
    /// State-sync freshness (AoI) and server cost across transports.
    Statesync(statesync::Args),
}

#[tokio::main]
async fn main() {
    match Cli::parse().cmd {
        Cmd::Gateway(args) => gateway::run(args).await,
        Cmd::Churn(args) => gateway::churn::run(args).await,
        Cmd::Statesync(args) => statesync::run(args).await,
    }
}
