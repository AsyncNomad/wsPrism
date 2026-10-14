//! State-sync freshness benchmark (validation V, design §7).
//!
//! `rooms` × `room_size` clients join a relay; `publishers_per_room` of each
//! room publish their state at `rate` Hz. Every client samples, at
//! `sample_hz`, the Age of Information of each sender it has heard from, and
//! records per-message latency as a secondary metric. With `--server-pid`
//! the server's CPU time and RSS over the window are reported, so freshness
//! and cost come from the **same run and configuration** (G1/G5).

mod aoi;
mod schedule;
mod transport;
mod wire;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use hdrhistogram::Histogram;
use tokio::sync::Semaphore;
use tokio::time::{interval_at, Instant, MissedTickBehavior};

use crate::cost;
use aoi::{new_hist, Aoi};
use schedule::{Phase, Schedule};
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
    /// Send/render timer phases across clients (see schedule.rs).
    #[arg(long, value_enum, default_value_t = Phase::Random)]
    pub phase: Phase,
    /// Seed for `--phase random`.
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    #[arg(long, default_value_t = 2)]
    pub warmup: u64,
    #[arg(long, default_value_t = 10)]
    pub duration: u64,
    /// `--transport auto`: WSS address used when WebTransport is not up in time.
    #[arg(long)]
    pub fallback_addr: Option<String>,
    /// `--transport auto`: how long to try WebTransport before falling back (ms).
    #[arg(long, default_value_t = 1500)]
    pub wt_budget_ms: u64,
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
                let started = Instant::now();
                let attempt = transport::connect_with_fallback(
                    args.transport,
                    &args.addr,
                    args.fallback_addr.as_deref(),
                    Duration::from_millis(args.wt_budget_ms),
                    plan.room,
                    plan.id,
                )
                .await;
                match attempt {
                    Ok((tx, rx, used)) => Some((plan, (tx, rx), used, started.elapsed())),
                    Err(e) => {
                        eprintln!("conn {}: {e}", plan.id);
                        None
                    }
                }
            }));
        }
    }
    let mut links = Vec::new();
    let mut connect_ms = new_hist();
    let (mut used_wt, mut used_wss) = (0u64, 0u64);
    for s in setups {
        if let Ok(Some((plan, link, used, took))) = s.await {
            let _ = connect_ms.record((took.as_micros() as u64).max(1));
            match used {
                Transport::Wt => used_wt += 1,
                _ => used_wss += 1,
            }
            links.push((plan, link));
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
    let connect = Connect {
        hist: connect_ms,
        used_wt,
        used_wss,
    };
    report(&args, pubs, all, rss_before, c0, c1, &connect);
}

/// How clients got connected (time to established link, transport used).
struct Connect {
    /// Microseconds from first attempt to an established link (incl. fallback).
    hist: Histogram<u64>,
    used_wt: u64,
    used_wss: u64,
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
    let send_period = Duration::from_secs_f64(1.0 / f64::from(args.rate.max(1)));
    let render_period = Duration::from_secs_f64(1.0 / f64::from(args.sample_hz.max(1)));
    let at = Schedule::new(
        args.phase,
        args.seed,
        plan.id,
        (t0, send_period),
        (window_start, render_period),
    );
    let mut send = interval_at(at.send_start, send_period);
    send.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut render = interval_at(at.render_start, render_period);
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

/// Share of AoI samples above each threshold (ms), from the pooled
/// histogram, so the stall verdict can be checked for threshold sensitivity.
/// Counting starts at the bucket after the threshold's own (buckets are
/// ~0.06 ms wide at 100 ms), so a sample at exactly the threshold is not a
/// stall, like `Aoi`'s per-sample count.
fn stall_ratio_at(aoi: &Histogram<u64>) -> serde_json::Value {
    const THRESHOLDS_MS: [u64; 5] = [50, 100, 150, 200, 300];
    let total = aoi.len().max(1) as f64;
    let mut m = serde_json::Map::new();
    for ms in THRESHOLDS_MS {
        let from = aoi.next_non_equivalent(ms * 1_000);
        let above = if aoi.max() >= from {
            aoi.count_between(from, aoi.max())
        } else {
            0
        };
        m.insert(ms.to_string(), serde_json::json!(above as f64 / total));
    }
    serde_json::Value::Object(m)
}

fn report(
    args: &Args,
    pubs: u32,
    all: Vec<Stats>,
    rss_before: Option<u64>,
    c0: Option<cost::ProcSample>,
    c1: Option<cost::ProcSample>,
    connect: &Connect,
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
        "stall_ratio_at": stall_ratio_at(&aoi),
        "phase": format!("{:?}", args.phase).to_lowercase(), "seed": args.seed,
        "latency_ms": {"p50": q(&lat, 0.5), "p99": q(&lat, 0.99), "p999": q(&lat, 0.999)},
        "sent": sent, "received": received, "expected": expected,
        "delivery_ratio": if expected > 0 { received as f64 / expected as f64 } else { 1.0 },
        "received_per_s": (received as f64 / secs).round(),
        "server_cpu_us_per_msg": cpu_us_per_msg,
        "server_rss_kb_per_conn": rss_per_conn_kb,
        "broken_links": broke,
        "connect_ms": {
            "p50": q(&connect.hist, 0.5), "p99": q(&connect.hist, 0.99), "max": connect.hist.max() as f64 / 1000.0
        },
        "connected_via": {"wt": connect.used_wt, "wss": connect.used_wss},
    });
    println!("{v}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stall_ratio_at_counts_samples_above_each_threshold() {
        let mut h = new_hist();
        for ms in [10u64, 60, 120, 120, 250, 400, 40, 40, 40, 40] {
            h.record(ms * 1_000).unwrap();
        }
        let v = stall_ratio_at(&h);
        let at = |k: &str| v[k].as_f64().unwrap();
        assert_eq!(at("50"), 0.5); // 60, 120, 120, 250, 400
        assert_eq!(at("100"), 0.4);
        assert_eq!(at("150"), 0.2);
        assert_eq!(at("200"), 0.2);
        assert_eq!(at("300"), 0.1);
    }

    #[test]
    fn stall_ratio_at_excludes_the_threshold_itself() {
        let mut h = new_hist();
        h.record(100_000).unwrap(); // exactly 100 ms is not a stall
        h.record(100_500).unwrap();
        assert_eq!(stall_ratio_at(&h)["100"].as_f64().unwrap(), 0.5);
    }
}
