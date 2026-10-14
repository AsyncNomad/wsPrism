//! State-sync freshness benchmark (validation V, design §7).
//!
//! `rooms` × `room_size` clients join a relay; `publishers_per_room` of each
//! room publish their state at `rate` Hz. Every client samples, at
//! `sample_hz`, the Age of Information of each sender it has heard from, and
//! records per-message latency as a secondary metric. With `--server-pid`
//! the server's CPU time and RSS over the window are reported, so freshness
//! and cost come from the **same run and configuration** (G1/G5).

mod aoi;
mod transport;
mod wire;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hdrhistogram::Histogram;
use tokio::sync::Semaphore;
use tokio::time::{interval_at, Instant, MissedTickBehavior};

use crate::cost;
use aoi::{new_hist, Aoi};
use transport::Transport;

#[derive(Debug, clap::Args)]
pub struct Args {
    #[arg(long, value_enum)]
    pub transport: Transport,
    /// Relay address (host:port).
    #[arg(long)]
    pub addr: String,
    #[arg(long, default_value_t = 10)]
    pub rooms: u32,
    #[arg(long, default_value_t = 10)]
    pub room_size: u32,
    /// Publishers per room (default: everyone, like players in a match).
    #[arg(long)]
    pub publishers_per_room: Option<u32>,
    /// State updates per second per publisher.
    #[arg(long, default_value_t = 60)]
    pub rate: u32,
    /// Frame size in bytes (min 16).
    #[arg(long, default_value_t = 64)]
    pub payload: usize,
    /// Render ticks per second at which AoI is sampled.
    #[arg(long, default_value_t = 60)]
    pub sample_hz: u32,
    /// AoI above this counts as a stall (ms).
    #[arg(long, default_value_t = 100)]
    pub stall_ms: u64,
    #[arg(long, default_value_t = 2)]
    pub warmup: u64,
    #[arg(long, default_value_t = 10)]
    pub duration: u64,
    /// Server process to sample CPU/RSS from.
    #[arg(long)]
    pub server_pid: Option<u32>,
    #[arg(long, default_value_t = 100)]
    pub connect_concurrency: usize,
    /// Free-form label copied into the JSON output (e.g. netem profile, tick).
    #[arg(long, default_value = "")]
    pub label: String,
}

fn now_ns() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_nanos() as u64
}

struct Plan {
    room: u32,
    id: u32,
    publish: bool,
}

struct Stats {
    sent: u64,
    received: u64,
    aoi: Aoi,
    latency_us: Histogram<u64>,
    broke: bool,
}

pub async fn run(args: Args) {
    now_ns(); // pin the epoch
    let args = Arc::new(args);
    let pubs = args
        .publishers_per_room
        .unwrap_or(args.room_size)
        .min(args.room_size);
    let rss_before = args.server_pid.and_then(cost::sample).map(|s| s.rss_kb);

    // Connect everyone.
    let gate = Arc::new(Semaphore::new(args.connect_concurrency.max(1)));
    let mut setups = Vec::new();
    for room in 0..args.rooms {
        for slot in 0..args.room_size {
            let id = room * args.room_size + slot;
            let plan = Plan {
                room,
                id,
                publish: slot < pubs,
            };
            let (args, gate) = (args.clone(), gate.clone());
            setups.push(tokio::spawn(async move {
                let _permit = gate.acquire_owned().await.ok()?;
                match transport::connect(args.transport, &args.addr, plan.room, plan.id).await {
                    Ok(link) => Some((plan, link)),
                    Err(e) => {
                        eprintln!("conn {}: {e}", plan.id);
                        None
                    }
                }
            }));
        }
    }
    let mut links = Vec::new();
    for s in setups {
        if let Ok(Some(l)) = s.await {
            links.push(l);
        }
    }
    let total = (args.rooms * args.room_size) as usize;
    if links.len() < total {
        eprintln!("only {}/{total} connections; aborting", links.len());
        std::process::exit(1);
    }

    let t0 = Instant::now() + Duration::from_millis(300);
    let window_start = t0 + Duration::from_secs(args.warmup);
    let window_end = window_start + Duration::from_secs(args.duration);
    let server_at = |at: Instant| {
        let pid = args.server_pid;
        async move {
            tokio::time::sleep_until(at).await;
            pid.and_then(cost::sample)
        }
    };
    let cost_start = tokio::spawn(server_at(window_start));
    let cost_end = tokio::spawn(server_at(window_end));

    let mut runs = Vec::new();
    for (plan, link) in links {
        runs.push(tokio::spawn(client(
            args.clone(),
            plan,
            link,
            t0,
            window_start,
            window_end,
        )));
    }
    let mut all = Vec::new();
    for r in runs {
        if let Ok(s) = r.await {
            all.push(s);
        }
    }
    let (c0, c1) = (
        cost_start.await.ok().flatten(),
        cost_end.await.ok().flatten(),
    );
    report(&args, pubs, all, rss_before, c0, c1);
}

async fn client(
    args: Arc<Args>,
    plan: Plan,
    (mut tx, mut rx): (transport::Tx, transport::Rx),
    t0: Instant,
    window_start: Instant,
    window_end: Instant,
) -> Stats {
    let mut stats = Stats {
        sent: 0,
        received: 0,
        aoi: Aoi::new(args.stall_ms * 1_000),
        latency_us: new_hist(),
        broke: false,
    };
    let mut send = interval_at(
        t0,
        Duration::from_secs_f64(1.0 / f64::from(args.rate.max(1))),
    );
    send.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut render = interval_at(
        window_start,
        Duration::from_secs_f64(1.0 / f64::from(args.sample_hz.max(1))),
    );
    render.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut seq: u32 = 0;
    let in_window = |now: Instant| now >= window_start && now < window_end;

    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(window_end) => break,
            _ = send.tick(), if plan.publish => {
                seq = seq.wrapping_add(1);
                let f = wire::frame(plan.id, seq, now_ns(), args.payload);
                if tx.send(f).await.is_err() {
                    stats.broke = true;
                    break;
                }
                if in_window(Instant::now()) {
                    stats.sent += 1;
                }
            }
            _ = render.tick() => stats.aoi.sample(now_ns()),
            batch = rx.recv() => {
                let Some(batch) = batch else {
                    stats.broke = true;
                    break;
                };
                let now = now_ns();
                let counted = in_window(Instant::now());
                for h in wire::batch_headers(&batch) {
                    stats.aoi.observe(h.sender, h.ts_ns);
                    if counted {
                        let _ = stats.latency_us.record((now.saturating_sub(h.ts_ns) / 1_000).max(1));
                        stats.received += 1;
                    }
                }
            }
        }
    }
    stats
}

fn report(
    args: &Args,
    pubs: u32,
    all: Vec<Stats>,
    rss_before: Option<u64>,
    c0: Option<cost::ProcSample>,
    c1: Option<cost::ProcSample>,
) {
    let mut aoi = new_hist();
    let mut lat = new_hist();
    let (mut sent, mut received, mut samples, mut stalls, mut broke) = (0, 0, 0, 0, 0);
    for s in &all {
        let _ = aoi.add(&s.aoi.hist);
        let _ = lat.add(&s.latency_us);
        sent += s.sent;
        received += s.received;
        samples += s.aoi.samples;
        stalls += s.aoi.stalls;
        broke += usize::from(s.broke);
    }
    // Each update reaches the other room_size-1 members (the relay skips the sender).
    let expected = sent * u64::from(args.room_size.saturating_sub(1));
    let secs = args.duration as f64;
    let q = |h: &Histogram<u64>, p: f64| h.value_at_quantile(p) as f64 / 1000.0;
    let cpu_us_per_msg = match (c0, c1) {
        (Some(a), Some(b)) if received > 0 => {
            Some((b.cpu_secs - a.cpu_secs) * 1e6 / received as f64)
        }
        _ => None,
    };
    let rss_per_conn_kb = match (rss_before, c1) {
        (Some(before), Some(after)) => {
            Some((after.rss_kb.saturating_sub(before)) as f64 / all.len().max(1) as f64)
        }
        _ => None,
    };
    let v = serde_json::json!({
        "label": args.label,
        "transport": format!("{:?}", args.transport).to_lowercase(),
        "rooms": args.rooms, "room_size": args.room_size, "publishers_per_room": pubs,
        "rate_hz": args.rate, "payload": args.payload, "sample_hz": args.sample_hz,
        "duration_s": args.duration,
        "aoi_ms": {"p50": q(&aoi, 0.5), "p90": q(&aoi, 0.9), "p99": q(&aoi, 0.99), "p999": q(&aoi, 0.999), "max": aoi.max() as f64 / 1000.0},
        "stall_ratio": if samples > 0 { stalls as f64 / samples as f64 } else { 0.0 },
        "latency_ms": {"p50": q(&lat, 0.5), "p99": q(&lat, 0.99), "p999": q(&lat, 0.999)},
        "sent": sent, "received": received, "expected": expected,
        "delivery_ratio": if expected > 0 { received as f64 / expected as f64 } else { 1.0 },
        "received_per_s": (received as f64 / secs).round(),
        "server_cpu_us_per_msg": cpu_us_per_msg,
        "server_rss_kb_per_conn": rss_per_conn_kb,
        "broken_links": broke,
    });
    println!("{v}");
}
