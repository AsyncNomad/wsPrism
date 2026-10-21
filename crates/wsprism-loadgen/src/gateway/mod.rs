//! Gateway fan-out benchmark over WebSocket, for wsPrism and competing
//! gateways (Centrifugo, Pusher-protocol servers such as Sockudo).
//!
//! Opens `--conns` connections, puts them in `--rooms` rooms, lets
//! `--publishers` of them send timestamped probes at `--rate`/s, and reports
//! end-to-end latency percentiles, throughput, delivery ratio and — with
//! `--server-pid` — server CPU per delivered message and RSS per connection.
//! Only the wire protocol differs per gateway (`protocol/`); the scenario,
//! timing and report are shared. See `docs/BENCHMARKS.md`.

mod client;
mod probe;
mod protocol;
mod report;
mod scenario;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tokio::time::Instant;

use crate::cost;
use crate::schedule::Schedule;
use protocol::{Centrifugo, Protocol, Pusher, Wsprism};
pub use scenario::Args;
use scenario::Target;

/// Extra time receivers keep reading after publishing stops.
const GRACE: Duration = Duration::from_secs(2);

/// Run the gateway fan-out benchmark against `args.target`.
pub async fn run(args: Args) {
    let url = args.url.clone();
    match args.target {
        Target::Wsprism => {
            let p = Wsprism {
                url,
                tenant: args.tenant.clone(),
                ticket: args.ticket.clone(),
                distinct_users: args.distinct_users,
                rooms: args.rooms.max(1),
                lane: args.lane,
                chans: Default::default(),
            };
            run_with(Arc::new(p), args).await
        }
        Target::Centrifugo => run_with(Arc::new(Centrifugo { url }), args).await,
        Target::Pusher => {
            let p = Pusher {
                url,
                key: args.app_key.clone(),
                secret: args.app_secret.clone(),
            };
            run_with(Arc::new(p), args).await
        }
    }
}

async fn run_with<P: Protocol>(proto: Arc<P>, args: Args) {
    let args = Arc::new(args);
    probe::now_nanos(); // pin the epoch before any probe is sent
    let rss_before = cost::sample_all(&args.server_pid).map(|s| s.rss_kb);

    // Ramp up: connect + join with bounded concurrency.
    let ramp = Instant::now();
    let gate = Arc::new(Semaphore::new(args.connect_concurrency.max(1)));
    let mut setups = Vec::with_capacity(args.conns);
    for conn in 0..args.conns {
        let (args, gate, proto) = (args.clone(), gate.clone(), proto.clone());
        setups.push(tokio::spawn(async move {
            let _permit = gate.acquire_owned().await.ok()?;
            match proto.setup(conn, &args.room_of(conn)).await {
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

    // Measure: publishers start within one send period of `start`, at
    // per-connection phases (unless --phase aligned).
    let start = Instant::now() + Duration::from_millis(500);
    let end = start + args.duration();
    let period = Duration::from_nanos(1_000_000_000 / args.rate.max(1));
    let cost_at = |pids: Vec<u32>, at: Instant| {
        tokio::spawn(async move {
            tokio::time::sleep_until(at).await;
            cost::sample_all(&pids)
        })
    };
    // Until the end of the grace period: deliveries are counted until then too.
    let window = (start, end + GRACE);
    let (cost_start, cost_end) = (
        cost_at(args.server_pid.clone(), window.0),
        cost_at(args.server_pid.clone(), window.1),
    );
    // Our own CPU: if the load generator saturates, the comparison is void.
    let me = vec![std::process::id()];
    let (self_start, self_end) = (cost_at(me.clone(), window.0), cost_at(me, window.1));
    let mut runs = Vec::with_capacity(conns.len());
    for (conn, ws) in conns.into_iter().enumerate() {
        let me = u32::try_from(conn).expect("conn index fits u32");
        let publish = args.is_publisher(conn).then(|| client::Publish {
            first_at: Schedule::new(args.phase, args.seed, me, (start, period), (start, period))
                .send_start,
            rate: args.rate,
            payload: args.payload,
        });
        runs.push(tokio::spawn(client::run(
            proto.clone(),
            ws,
            me,
            args.room_of(conn),
            publish,
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
    let cost = report::Cost {
        rss_before_kb: rss_before,
        start: cost_start.await.ok().flatten(),
        end: cost_end.await.ok().flatten(),
        loadgen: (
            self_start.await.ok().flatten(),
            self_end.await.ok().flatten(),
        ),
        window: window.1 - window.0,
    };

    let report = report::Report::aggregate(&args, stats, &room_sizes, cost);
    if args.json {
        report.print_json(&args);
    } else {
        report.print(&args);
    }
}
