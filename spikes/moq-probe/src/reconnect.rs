//! M-5: what does a subscriber see when the publisher of a track goes away
//! and comes back under the same namespace?
//!
//! Publisher and subscriber are separate processes so the publisher can die
//! abruptly (`kill -9`: no CONNECTION_CLOSE, the relay only learns from the
//! idle timeout) or leave gracefully (`--graceful`: session closed). Events
//! carry wall-clock milliseconds so the two processes' logs line up.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use clap::Parser;
use moq_transport::serve::{self, Datagram, TrackReaderMode};

use crate::{conn, ns, now_ns, wire};

#[derive(Parser)]
pub struct PubArgs {
    #[arg(long)]
    url: url::Url,
    #[arg(long, default_value_t = 0)]
    id: u32,
    #[arg(long, default_value_t = 60)]
    rate: u32,
    /// Publish this long, then leave (or be killed before that).
    #[arg(long, default_value_t = 3.0)]
    secs: f64,
    /// Close the session on exit instead of just exiting.
    #[arg(long)]
    graceful: bool,
}

#[derive(Parser)]
pub struct SubArgs {
    #[arg(long)]
    url: url::Url,
    #[arg(long, default_value_t = 0)]
    id: u32,
    /// Observe this long, then print the event log.
    #[arg(long, default_value_t = 20.0)]
    secs: f64,
}

fn wall_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn event(kind: &str, extra: serde_json::Value) {
    let mut v = serde_json::json!({ "t_ms": wall_ms(), "event": kind });
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        o.extend(e.clone());
    }
    eprintln!("{v}");
}

pub async fn publish(a: PubArgs) -> anyhow::Result<()> {
    let c = conn::open(&a.url).await?;
    let (wt, session, mut publisher) = (c.wt, c.session, c.publisher);
    tokio::spawn(async move { session.run().await });
    let (mut tracks, _, reader) = serve::Tracks { namespace: ns(0, a.id) }.produce();
    let mut datagrams = tracks.create("state").expect("create track").datagrams()?;
    tokio::spawn(async move { publisher.publish_namespace(reader).await });
    event("pub_connected", serde_json::json!({}));

    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / f64::from(a.rate.max(1))));
    let end = tokio::time::Instant::now() + Duration::from_secs_f64(a.secs);
    let mut seq = 0u32;
    while tokio::time::Instant::now() < end {
        tick.tick().await;
        seq += 1;
        let _ = datagrams.write(Datagram {
            group_id: u64::from(seq),
            object_id: 0,
            priority: 0,
            payload: wire::frame(a.id, seq, now_ns(), 64),
            extension_headers: Default::default(),
        });
    }
    event("pub_leaving", serde_json::json!({ "sent": seq, "graceful": a.graceful }));
    if a.graceful {
        wt.close(0, "bye");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    Ok(())
}

/// Subscribe, read until the subscription ends, log why, resubscribe
/// (every 100 ms) — for the whole observation window.
pub async fn subscribe(a: SubArgs) -> anyhow::Result<()> {
    let c = conn::open(&a.url).await?;
    let (session, subscriber) = (c.session, c.subscriber);
    tokio::spawn(async move { session.run().await });
    let deadline = tokio::time::Instant::now() + Duration::from_secs_f64(a.secs);
    let observe = async {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let (w, r) = serve::Track::new(ns(0, a.id), "state".to_string()).produce();
            let mut sub = subscriber.clone();
            let subscribing = tokio::spawn(async move { sub.subscribe(w).await });
            match r.mode().await {
                Ok(TrackReaderMode::Datagrams(mut reader)) => {
                    event("sub_ok", serde_json::json!({ "attempt": attempt }));
                    let (mut n, mut last_seq, mut first) = (0u64, 0u32, true);
                    let end = loop {
                        match reader.read().await {
                            Ok(Some(d)) => {
                                if let Some(h) = wire::header(&d.payload) {
                                    if first || h.seq < last_seq {
                                        event("rx_epoch", serde_json::json!({ "attempt": attempt, "seq": h.seq }));
                                        first = false;
                                    }
                                    last_seq = h.seq;
                                    n += 1;
                                }
                            }
                            Ok(None) => break "end".to_string(),
                            Err(e) => break format!("error: {e}"),
                        }
                    };
                    event("sub_end", serde_json::json!({ "attempt": attempt, "reason": end, "received": n, "last_seq": last_seq }));
                }
                Ok(_) => event("sub_mode_other", serde_json::json!({ "attempt": attempt })),
                Err(e) => {
                    if attempt == 1 || attempt % 50 == 0 {
                        event("sub_fail", serde_json::json!({ "attempt": attempt, "error": e.to_string() }));
                    }
                }
            }
            subscribing.abort();
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    tokio::select! {
        _ = observe => {}
        () = tokio::time::sleep_until(deadline) => {}
    }
    event("sub_done", serde_json::json!({}));
    Ok(())
}
