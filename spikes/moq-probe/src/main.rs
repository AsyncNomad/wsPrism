//! THROWAWAY validation V6: state sync over a MoQ relay.
//!
//! `cell`: the judgment-cell scenario (same metrics/JSON as the loadgen).
//! `pub` / `sub`: one publisher and one subscriber in separate processes, to
//! observe what a subscriber sees when the publisher reconnects (M-5).

#[path = "../../../crates/wsprism-loadgen/src/statesync/aoi.rs"]
mod aoi;
#[path = "../../../crates/wsprism-loadgen/src/cost.rs"]
mod cost;
#[path = "../../../crates/wsprism-loadgen/src/statesync/wire.rs"]
#[allow(dead_code)]
mod wire;

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use clap::Parser;
use hdrhistogram::Histogram;
use moq_transport::coding::TrackNamespace;
use moq_transport::serve::{self, Datagram, TrackReaderMode};
use moq_transport::session::Session;
use tokio::sync::{mpsc, Semaphore};
use tokio::time::{interval_at, Instant, MissedTickBehavior};

mod conn;
mod reconnect;

#[derive(Parser)]
enum Cmd {
    /// The judgment-cell scenario.
    Cell(Args),
    /// Publish one participant's track (reconnect experiment).
    Pub(reconnect::PubArgs),
    /// Subscribe to one participant's track and log what happens (reconnect experiment).
    Sub(reconnect::SubArgs),
}

#[derive(Parser)]
struct Args {
    /// Relay URL, e.g. https://10.77.0.1:4443/wsprism
    #[arg(long)]
    url: url::Url,
    #[arg(long, default_value_t = 10)]
    rooms: u32,
    #[arg(long, default_value_t = 10)]
    room_size: u32,
    #[arg(long, default_value_t = 60)]
    rate: u32,
    #[arg(long, default_value_t = 64)]
    payload: usize,
    #[arg(long, default_value_t = 60)]
    sample_hz: u32,
    #[arg(long, default_value_t = 100)]
    stall_ms: u64,
    #[arg(long, default_value_t = 3)]
    warmup: u64,
    #[arg(long, default_value_t = 10)]
    duration: u64,
    #[arg(long)]
    server_pid: Option<u32>,
    #[arg(long, default_value_t = 50)]
    connect_concurrency: usize,
    #[arg(long, default_value = "")]
    label: String,
}

pub(crate) fn now_ns() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_nanos() as u64
}

pub(crate) fn ns(room: u32, id: u32) -> TrackNamespace {
    TrackNamespace::from_utf8_path(&format!("r{room}/p{id}"))
}

struct Stats {
    sent: u64,
    received: u64,
    aoi: aoi::Aoi,
    latency_us: Histogram<u64>,
    subscribed: u32,
}

/// One participant: connect, publish own track, subscribe to the others.
struct Client {
    room: u32,
    id: u32,
    session: Session,
    publisher: moq_transport::session::Publisher,
    subscriber: moq_transport::session::Subscriber,
}

async fn connect(url: &url::Url, room: u32, id: u32) -> anyhow::Result<Client> {
    let c = conn::open(url).await?;
    Ok(Client { room, id, session: c.session, publisher: c.publisher, subscriber: c.subscriber })
}

async fn run_client(
    args: Arc<Args>,
    c: Client,
    publish: bool,
    t0: Instant,
    window_start: Instant,
    window_end: Instant,
) -> Stats {
    let Client { room, id, session, mut publisher, subscriber } = c;
    tokio::spawn(async move {
        let _ = session.run().await;
    });

    // Publish own track (datagrams).
    let (mut tracks_writer, _, tracks_reader) = serve::Tracks { namespace: ns(room, id) }.produce();
    let track = tracks_writer.create("state").expect("create track");
    let mut datagrams = track.datagrams().expect("datagram track");
    tokio::spawn(async move {
        let _ = publisher.publish_namespace(tracks_reader).await;
    });

    // Subscribe to every other member; arrivals go to this client's loop.
    let (tx, mut rx) = mpsc::unbounded_channel::<(u32, u64)>();
    let (ok_tx, mut ok_rx) = mpsc::unbounded_channel::<()>();
    for other in 0..args.room_size {
        let other_id = room * args.room_size + other;
        if other_id == id {
            continue;
        }
        let (tx, ok_tx, mut sub) = (tx.clone(), ok_tx.clone(), subscriber.clone());
        tokio::spawn(async move {
            // The other participant may not have published yet: retry.
            for _ in 0..200 {
                let (w, r) = serve::Track::new(ns(room, other_id), "state".to_string()).produce();
                let mut sub2 = sub.clone();
                let subscribing = tokio::spawn(async move { sub2.subscribe(w).await });
                if let Ok(TrackReaderMode::Datagrams(mut reader)) = r.mode().await {
                    let _ = ok_tx.send(());
                    while let Ok(Some(d)) = reader.read().await {
                        if let Some(h) = wire::header(&d.payload) {
                            let _ = tx.send((h.sender, h.ts_ns));
                        }
                    }
                    return;
                }
                subscribing.abort();
                tokio::time::sleep(Duration::from_millis(100)).await;
                let _ = &mut sub;
            }
        });
    }
    drop(tx);
    drop(ok_tx);

    let mut stats = Stats {
        sent: 0,
        received: 0,
        aoi: aoi::Aoi::new(args.stall_ms * 1_000),
        latency_us: aoi::new_hist(),
        subscribed: 0,
    };
    let mut send = interval_at(t0, Duration::from_secs_f64(1.0 / f64::from(args.rate.max(1))));
    send.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let mut render = interval_at(window_start, Duration::from_secs_f64(1.0 / f64::from(args.sample_hz.max(1))));
    render.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut seq: u32 = 0;
    let in_window = |now: Instant| now >= window_start && now < window_end;
    loop {
        tokio::select! {
            _ = tokio::time::sleep_until(window_end) => break,
            _ = send.tick(), if publish => {
                seq = seq.wrapping_add(1);
                let frame = wire::frame(id, seq, now_ns(), args.payload);
                let _ = datagrams.write(Datagram {
                    group_id: u64::from(seq),
                    object_id: 0,
                    priority: 0,
                    payload: frame,
                    extension_headers: Default::default(),
                });
                if in_window(Instant::now()) {
                    stats.sent += 1;
                }
            }
            _ = render.tick() => stats.aoi.sample(now_ns()),
            Some(()) = ok_rx.recv() => stats.subscribed += 1,
            Some((sender, ts)) = rx.recv() => {
                stats.aoi.observe(sender, ts);
                if in_window(Instant::now()) {
                    let _ = stats.latency_us.record(((now_ns().saturating_sub(ts)) / 1_000).max(1));
                    stats.received += 1;
                }
            }
        }
    }
    stats
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    now_ns();
    let _ = rustls::crypto::ring::default_provider().install_default();
    match Cmd::parse() {
        Cmd::Cell(a) => cell(Arc::new(a)).await,
        Cmd::Pub(a) => reconnect::publish(a).await,
        Cmd::Sub(a) => reconnect::subscribe(a).await,
    }
}

async fn cell(args: Arc<Args>) -> anyhow::Result<()> {
    let rss_before = args.server_pid.and_then(cost::sample).map(|s| s.rss_kb);

    let gate = Arc::new(Semaphore::new(args.connect_concurrency.max(1)));
    let mut setups = Vec::new();
    for room in 0..args.rooms {
        for slot in 0..args.room_size {
            let id = room * args.room_size + slot;
            let (args, gate) = (args.clone(), gate.clone());
            setups.push(tokio::spawn(async move {
                let _p = gate.acquire_owned().await.ok()?;
                match connect(&args.url, room, id).await {
                    Ok(c) => Some(c),
                    Err(e) => {
                        eprintln!("conn {id}: {e:#}");
                        None
                    }
                }
            }));
        }
    }
    let mut clients = Vec::new();
    for s in setups {
        if let Ok(Some(c)) = s.await {
            clients.push(c);
        }
    }
    let total = (args.rooms * args.room_size) as usize;
    if clients.len() < total {
        anyhow::bail!("only {}/{total} connections", clients.len());
    }

    let t0 = Instant::now() + Duration::from_millis(300);
    let window_start = t0 + Duration::from_secs(args.warmup);
    let window_end = window_start + Duration::from_secs(args.duration);
    let pid = args.server_pid;
    let c0 = tokio::spawn(async move {
        tokio::time::sleep_until(window_start).await;
        pid.and_then(cost::sample)
    });
    let c1 = tokio::spawn(async move {
        tokio::time::sleep_until(window_end).await;
        pid.and_then(cost::sample)
    });
    let runs: Vec<_> = clients
        .into_iter()
        .map(|c| tokio::spawn(run_client(args.clone(), c, true, t0, window_start, window_end)))
        .collect();
    let mut all = Vec::new();
    for r in runs {
        all.push(r.await?);
    }
    let (c0, c1) = (c0.await?, c1.await?);

    let mut aoi_h = aoi::new_hist();
    let mut lat = aoi::new_hist();
    let (mut sent, mut received, mut samples, mut stalls, mut subscribed) = (0u64, 0u64, 0u64, 0u64, 0u64);
    for s in &all {
        let _ = aoi_h.add(&s.aoi.hist);
        let _ = lat.add(&s.latency_us);
        sent += s.sent;
        received += s.received;
        samples += s.aoi.samples;
        stalls += s.aoi.stalls;
        subscribed += u64::from(s.subscribed);
    }
    let expected = sent * u64::from(args.room_size.saturating_sub(1));
    let q = |h: &Histogram<u64>, p: f64| h.value_at_quantile(p) as f64 / 1000.0;
    let cpu = match (c0, c1) {
        (Some(a), Some(b)) if received > 0 => Some((b.cpu_secs - a.cpu_secs) * 1e6 / received as f64),
        _ => None,
    };
    let rss = match (rss_before, c1) {
        (Some(b), Some(a)) => Some(a.rss_kb.saturating_sub(b) as f64 / all.len().max(1) as f64),
        _ => None,
    };
    let v = serde_json::json!({
        "label": args.label, "transport": "moq",
        "rooms": args.rooms, "room_size": args.room_size, "publishers_per_room": args.room_size,
        "rate_hz": args.rate, "payload": args.payload, "sample_hz": args.sample_hz, "duration_s": args.duration,
        "aoi_ms": {"p50": q(&aoi_h,0.5), "p90": q(&aoi_h,0.9), "p99": q(&aoi_h,0.99), "p999": q(&aoi_h,0.999), "max": aoi_h.max() as f64/1000.0},
        "stall_ratio": if samples > 0 { stalls as f64 / samples as f64 } else { 0.0 },
        "latency_ms": {"p50": q(&lat,0.5), "p99": q(&lat,0.99), "p999": q(&lat,0.999)},
        "sent": sent, "received": received, "expected": expected,
        "delivery_ratio": if expected > 0 { received as f64 / expected as f64 } else { 1.0 },
        "received_per_s": (received as f64 / args.duration as f64).round(),
        "server_cpu_us_per_msg": cpu, "server_rss_kb_per_conn": rss,
        "broken_links": 0, "subscriptions_ok": subscribed,
        "subscriptions_expected": total as u64 * u64::from(args.room_size.saturating_sub(1)),
    });
    println!("{v}");
    Ok(())
}
