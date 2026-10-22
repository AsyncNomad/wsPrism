//! Session churn against wsPrism: what connecting, joining and leaving cost
//! (ADR 0013: the control Redis's ceiling, measured before sharding rooms).
//!
//! `--workers` loops run at once; each one, until `--duration` is over:
//! connects as a new session (its own user), waits for `sys.authed`, joins
//! `--joins` rooms (from a pool of `--rooms`, waiting for each
//! `sys.joined`), keeps the session `--hold-ms`, and closes it. Reports
//! sessions and joins per second, connect and join latency, and — with
//! `--server-pid` — server CPU. Redis's side comes from its INFO
//! (`bench/run-churn.sh`).

use std::sync::Arc;
use std::time::Duration;

use futures_util::SinkExt;
use hdrhistogram::Histogram;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::Message;

use super::protocol::{await_text, open};
use crate::cost;

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Gateway base URL, e.g. `ws://10.77.0.1:18080`.
    #[arg(long, default_value = "ws://127.0.0.1:8080")]
    pub url: String,
    #[arg(long, default_value = "bench")]
    pub tenant: String,
    /// Dev ticket; each session logs in as `<ticket>:w<worker>c<cycle>`.
    #[arg(long, default_value = "dev")]
    pub ticket: String,
    /// Concurrent session loops.
    #[arg(long, default_value_t = 100)]
    pub workers: usize,
    /// Rooms each session joins.
    #[arg(long, default_value_t = 1)]
    pub joins: usize,
    /// Room pool the joins pick from.
    #[arg(long, default_value_t = 1000)]
    pub rooms: usize,
    /// How long a session stays after its joins, ms.
    #[arg(long, default_value_t = 0)]
    pub hold_ms: u64,
    /// Seconds of churn.
    #[arg(long, default_value_t = 20)]
    pub duration: u64,
    /// Gateway process ids (comma-separated) for CPU.
    #[arg(long, value_delimiter = ',')]
    pub server_pid: Vec<u32>,
    #[arg(long, default_value = "")]
    pub label: String,
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    #[arg(long)]
    pub json: bool,
}

#[derive(Default)]
struct Stats {
    sessions: u64,
    joins: u64,
    errors: u64,
    first_error: Option<String>,
    connect_us: Option<Histogram<u64>>,
    join_us: Option<Histogram<u64>>,
}

fn hist() -> Histogram<u64> {
    Histogram::new_with_bounds(1, 60_000_000, 3).expect("histogram bounds")
}

fn sys(text: &str, ty: &str) -> Option<Result<(), String>> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    if v["svc"] != "sys" {
        return None;
    }
    if v["type"] == ty {
        return Some(Ok(()));
    }
    (v["type"] == "error").then(|| Err(format!("sys.error waiting for sys.{ty}: {v}")))
}

/// One session: connect, auth, joins, hold, close.
async fn session(
    args: &Args,
    worker: usize,
    cycle: u64,
    rng: &mut u64,
    st: &mut Stats,
) -> Result<(), String> {
    let url = format!(
        "{}/v1/ws?tenant={}&sid=w{worker}c{cycle}",
        args.url.trim_end_matches('/'),
        args.tenant
    );
    let mut req = url.into_client_request().map_err(|e| e.to_string())?;
    let auth = format!("Bearer {}:w{worker}c{cycle}", args.ticket)
        .parse()
        .map_err(|_| "ticket header".to_string())?;
    req.headers_mut().insert(AUTHORIZATION, auth);
    let t = Instant::now();
    let mut ws = open(req).await?;
    await_text(&mut ws, "sys.authed", |m| sys(m, "authed")).await?;
    let us = t.elapsed().as_micros() as u64;
    let _ = st.connect_us.get_or_insert_with(hist).record(us.max(1));
    st.sessions += 1;
    for _ in 0..args.joins {
        // xorshift: rooms spread evenly, reproducible per seed.
        *rng ^= *rng << 13;
        *rng ^= *rng >> 7;
        *rng ^= *rng << 17;
        let room = format!("room:{}", *rng % args.rooms.max(1) as u64);
        let join = serde_json::json!({"v": 1, "svc": "room", "type": "join", "room": room});
        let t = Instant::now();
        ws.send(Message::Text(join.to_string()))
            .await
            .map_err(|e| format!("join send: {e}"))?;
        await_text(&mut ws, "sys.joined", |m| sys(m, "joined")).await?;
        let us = t.elapsed().as_micros() as u64;
        let _ = st.join_us.get_or_insert_with(hist).record(us.max(1));
        st.joins += 1;
    }
    if args.hold_ms > 0 {
        tokio::time::sleep(Duration::from_millis(args.hold_ms)).await;
    }
    let _ = ws.close(None).await;
    Ok(())
}

pub async fn run(args: Args) {
    let args = Arc::new(args);
    let start = Instant::now();
    let end = start + Duration::from_secs(args.duration);
    let cost_start = cost::sample_all(&args.server_pid);
    let mut tasks = Vec::with_capacity(args.workers);
    for worker in 0..args.workers {
        let args = Arc::clone(&args);
        tasks.push(tokio::spawn(async move {
            let mut st = Stats::default();
            let mut rng = args.seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ (worker as u64 + 1);
            let mut cycle = 0u64;
            while Instant::now() < end {
                if let Err(e) = session(&args, worker, cycle, &mut rng, &mut st).await {
                    st.errors += 1;
                    st.first_error.get_or_insert(e);
                }
                cycle += 1;
            }
            st
        }));
    }
    let mut total = Stats::default();
    for t in tasks {
        let Ok(st) = t.await else { continue };
        total.sessions += st.sessions;
        total.joins += st.joins;
        total.errors += st.errors;
        if total.first_error.is_none() {
            total.first_error = st.first_error;
        }
        for (into, from) in [
            (&mut total.connect_us, st.connect_us),
            (&mut total.join_us, st.join_us),
        ] {
            if let Some(h) = from {
                let _ = into.get_or_insert_with(hist).add(&h);
            }
        }
    }
    let secs = start.elapsed().as_secs_f64();
    let cpu = match (cost_start, cost::sample_all(&args.server_pid)) {
        (Some(a), Some(b)) => Some(b.cpu_secs - a.cpu_secs),
        _ => None,
    };
    let pct = |h: &Option<Histogram<u64>>| {
        h.as_ref().map(|h| {
            serde_json::json!({
                "p50": h.value_at_quantile(0.5),
                "p99": h.value_at_quantile(0.99),
                "p999": h.value_at_quantile(0.999),
                "max": h.max(),
            })
        })
    };
    let report = serde_json::json!({
        "label": args.label,
        "workers": args.workers,
        "joins_per_session": args.joins,
        "hold_ms": args.hold_ms,
        "duration_s": secs,
        "sessions": total.sessions,
        "sessions_per_s": total.sessions as f64 / secs,
        "joins_per_s": total.joins as f64 / secs,
        "errors": total.errors,
        "first_error": total.first_error,
        "connect_us": pct(&total.connect_us),
        "join_us": pct(&total.join_us),
        "server_cpu_s": cpu,
        "server_cpu_us_per_session": cpu.map(|c| c * 1e6 / total.sessions.max(1) as f64),
    });
    if args.json {
        println!("{report}");
    } else {
        println!("{report:#}");
    }
}
