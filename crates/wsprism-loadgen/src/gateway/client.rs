//! One benchmark connection's send/receive loops, for any [`Protocol`].

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hdrhistogram::Histogram;
use tokio::sync::mpsc;
use tokio::time::{interval_at, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;

use super::probe;
use super::protocol::{Incoming, Probe, Protocol, Ws};

/// What one connection observed.
pub struct ConnStats {
    pub sent: u64,
    /// Probes from other connections.
    pub received: u64,
    /// This connection's own probes delivered back to it (protocol-dependent).
    pub self_echo: u64,
    /// End-to-end latency of probes from others, in microseconds. Made at
    /// the first probe: one histogram is ~150 KB, and idle connections
    /// (tens of thousands in the idle-scale test) never need one.
    pub latency_us: Option<Histogram<u64>>,
    pub closed_early: bool,
}

/// Publishing schedule of a connection, if it publishes.
pub struct Publish {
    pub first_at: Instant,
    pub rate: u64,
    pub payload: usize,
}

/// Run until `end`: publish probes if `publish`, record the latency of every
/// probe from others, answer protocol pings. Keeps reading for `grace`.
pub async fn run<P: Protocol>(
    proto: Arc<P>,
    ws: Ws,
    me: u32,
    room: String,
    publish: Option<Publish>,
    end: Instant,
    grace: Duration,
) -> ConnStats {
    let (mut tx, mut rx) = ws.split();
    // The receiver hands protocol replies (pongs) to the single writer.
    let (reply_tx, mut reply_rx) = mpsc::unbounded_channel::<Message>();

    let writer = {
        let proto = proto.clone();
        let room = room.clone();
        async move {
            let mut tick = publish.as_ref().map(|p| {
                let period = Duration::from_nanos(1_000_000_000 / p.rate.max(1));
                let mut t = interval_at(p.first_at, period);
                t.set_missed_tick_behavior(MissedTickBehavior::Delay);
                t
            });
            let payload = publish.as_ref().map_or(0, |p| p.payload);
            let (mut sent, mut seq) = (0u64, 0u32);
            loop {
                let publishing = tick.is_some() && Instant::now() < end;
                tokio::select! {
                    _ = async { tick.as_mut().expect("guarded").tick().await }, if publishing => {
                        let probe = Probe { sender: me, ts_ns: probe::now_nanos() };
                        if tx.send(proto.encode(&room, probe, seq, payload)).await.is_err() {
                            break;
                        }
                        seq = seq.wrapping_add(1);
                        sent += 1;
                    }
                    reply = reply_rx.recv() => match reply {
                        Some(m) => {
                            if tx.send(m).await.is_err() {
                                break;
                            }
                        }
                        None => break, // receiver finished
                    },
                }
            }
            sent
        }
    };

    let receiver = async move {
        let mut hist: Option<Histogram<u64>> = None;
        let (mut received, mut self_echo, mut closed_early) = (0u64, 0u64, false);
        let mut incoming = Vec::new();
        let stop = end + grace;
        loop {
            match tokio::time::timeout_at(stop, rx.next()).await {
                Err(_) => break,
                Ok(Some(Ok(Message::Close(_)))) | Ok(Some(Err(_))) | Ok(None) => {
                    closed_early = Instant::now() < end;
                    break;
                }
                Ok(Some(Ok(msg))) => {
                    proto.decode(&msg, &mut incoming);
                    let now = probe::now_nanos();
                    for i in incoming.drain(..) {
                        match i {
                            Incoming::Probe(p) if p.sender == me => self_echo += 1,
                            Incoming::Probe(p) => {
                                let h = hist.get_or_insert_with(|| {
                                    Histogram::new_with_bounds(1, 60_000_000, 3)
                                        .expect("histogram bounds")
                                });
                                let _ = h.record((now.saturating_sub(p.ts_ns) / 1_000).max(1));
                                received += 1;
                            }
                            Incoming::Reply(m) => {
                                let _ = reply_tx.send(m);
                            }
                        }
                    }
                }
            }
        }
        drop(reply_tx);
        (received, self_echo, hist, closed_early)
    };

    let (sent, (received, self_echo, latency_us, closed_early)) = tokio::join!(writer, receiver);
    ConnStats {
        sent,
        received,
        self_echo,
        latency_us,
        closed_early,
    }
}
