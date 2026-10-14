//! wsPrism load generator.
//!
//! Opens `--conns` WebSocket connections, puts them in `--rooms` rooms, lets
//! `--publishers` of them send timestamped probes at `--rate`/s, and reports
//! end-to-end latency percentiles, throughput and delivery ratio.
//! See `docs/BENCHMARKS.md` for the gateway config and methodology.

mod client;
mod probe;
mod report;
mod scenario;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use tokio::sync::Semaphore;
use tokio::time::Instant;

use scenario::Args;

/// Extra time receivers keep reading after publishing stops.
const GRACE: Duration = Duration::from_secs(2);

#[tokio::main]
async fn main() {
    let args = Arc::new(Args::parse());
    probe::now_nanos(); // pin the epoch before any probe is sent

    // Ramp up: connect + join with bounded concurrency.
    let ramp = Instant::now();
    let gate = Arc::new(Semaphore::new(args.connect_concurrency.max(1)));
    let mut setups = Vec::with_capacity(args.conns);
    for conn in 0..args.conns {
        let (args, gate) = (args.clone(), gate.clone());
        setups.push(tokio::spawn(async move {
            let _permit = gate.acquire_owned().await.ok()?;
            match client::setup(&args.ws_url(conn), &args.room_of(conn)).await {
                Ok(ws) => Some(ws),
                Err(e) => {
                    eprintln!("conn {conn}: {e}");
                    None
                }
            }
        }));
    }
    let mut conns = Vec::with_capacity(args.conns);
    for s in setups {
        if let Ok(Some(ws)) = s.await {
            conns.push(ws);
        }
    }
    if conns.len() < args.conns {
        eprintln!(
            "only {}/{} connections set up; aborting",
            conns.len(),
            args.conns
        );
        std::process::exit(1);
    }
    if !args.json {
        eprintln!("ramp-up: {} conns in {:.2?}", conns.len(), ramp.elapsed());
    }

    let rooms = args.rooms.max(1);
    let mut room_sizes = vec![0u64; rooms];
    for conn in 0..args.conns {
        room_sizes[conn % rooms] += 1;
    }

    // Measure: everyone starts publishing at the same instant.
    let start = Instant::now() + Duration::from_millis(500);
    let end = start + args.duration();
    let mut runs = Vec::with_capacity(conns.len());
    for (conn, ws) in conns.into_iter().enumerate() {
        let publish = args.is_publisher(conn).then_some((args.rate, args.payload));
        runs.push(tokio::spawn(client::run(
            ws,
            args.lane,
            args.room_of(conn),
            publish,
            start,
            end,
            GRACE,
        )));
    }
    let mut stats = Vec::with_capacity(runs.len());
    for r in runs {
        if let Ok(s) = r.await {
            stats.push(s);
        }
    }

    let report = report::Report::aggregate(&args, stats, &room_sizes);
    if args.json {
        report.print_json(&args);
    } else {
        report.print(&args);
    }
}
