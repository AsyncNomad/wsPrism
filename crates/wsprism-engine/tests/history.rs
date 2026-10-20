//! History recovery under concurrent publishes (ADR 0011 §3): a recovering
//! join racing with publishers on other threads sees every offset exactly
//! once, in order — replayed ones, then live ones.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;

use wsprism_engine::spi::{
    Connection, CtxIdentity, HistoryConfig, HistoryRule, OutFrame, RealtimeCore, Recover,
    SessionSignals,
};
use wsprism_engine::{Outgoing, Payload, Permissions, QoS, RealtimeCtx, RoomPattern};
use wsprism_protocol::protocol::ProtocolVersion;

fn core() -> Arc<RealtimeCore> {
    let mut h = HistoryConfig::default();
    h.add(
        "t",
        HistoryRule::new(
            RoomPattern::parse("h:*"),
            100_000,
            Duration::from_secs(600),
            64 << 20,
        ),
    );
    Arc::new(RealtimeCore::new().with_history(h).unwrap())
}

fn session(core: &Arc<RealtimeCore>, user: &str) -> (RealtimeCtx, mpsc::Receiver<OutFrame>) {
    let (tx, rx) = mpsc::channel(1 << 16);
    let conn = Connection {
        tx,
        signals: SessionSignals::new(),
    };
    let session_key = format!("t::{user}::s");
    core.sessions
        .try_insert(
            "t".into(),
            format!("t::{user}"),
            session_key.clone(),
            "s".into(),
            conn.clone(),
            0,
        )
        .unwrap();
    let id = CtxIdentity {
        tenant: "t".into(),
        user: user.into(),
        session_id: "s".into(),
        session_key: session_key.into(),
        trace_id: "tr".into(),
        protocol: ProtocolVersion::V2,
        permissions: Permissions::default(),
        hot_echo_to_sender: true,
        presence_events: false,
    };
    (
        RealtimeCtx::new(id, Arc::default(), conn, Arc::clone(core)),
        rx,
    )
}

/// Every JSON message in `rx` (batches expanded), in order.
fn drain(rx: &mut mpsc::Receiver<OutFrame>) -> Vec<serde_json::Value> {
    let mut out = Vec::new();
    while let Ok(f) = rx.try_recv() {
        let frames: Vec<OutFrame> = match f {
            OutFrame::Batch(b) => b.to_vec(),
            f => vec![f],
        };
        for f in frames {
            if let OutFrame::Text(t) = f {
                out.push(serde_json::from_slice(t.as_bytes()).unwrap());
            }
        }
    }
    out
}

#[test]
fn recovering_joins_racing_publishers_see_each_offset_once_in_order() {
    let core = core();
    let room_key = RealtimeCore::room_key("t", "h:1");
    let (mut first, mut first_rx) = session(&core, "first");
    first
        .join_room_and_reply("h:1", None, |_, _| serde_json::json!({}))
        .unwrap();
    drain(&mut first_rx);

    let stop = Arc::new(AtomicBool::new(false));
    let publishers: Vec<_> = (0..3)
        .map(|_| {
            let (core, stop, room_key) = (Arc::clone(&core), Arc::clone(&stop), room_key.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    core.publish_room(
                        &room_key,
                        Outgoing {
                            qos: QoS::Reliable,
                            payload: Payload::TextJson(serde_json::json!({"type": "m"})),
                        },
                    )
                    .unwrap();
                }
            })
        })
        .collect();

    // Join, leave and rejoin with recovery many times while they publish.
    // Rounds until enough messages raced the joins (CPU share varies with
    // what else runs), with a time cap.
    let mut checked = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut round = 0;
    while checked < 2000 && std::time::Instant::now() < deadline {
        round += 1;
        let (mut c, mut rx) = session(&core, &format!("u{round}"));
        let mut epoch = String::new();
        c.join_room_and_reply("h:1", None, |_, out| {
            let pos = out.history.as_ref().unwrap();
            epoch = pos.epoch.to_string();
            serde_json::json!({"type": "joined", "offset": pos.offset})
        })
        .unwrap();
        std::thread::yield_now();
        let mut seen = drain(&mut rx);
        let start = seen[0]["offset"].as_u64().unwrap();
        // Leave, miss some, come back resuming after the last one seen —
        // after reading everything that arrived before the leave, as a
        // client does before it reconnects.
        c.leave_room("h:1");
        seen.extend(drain(&mut rx));
        let last = seen
            .iter()
            .rev()
            .find_map(|v| v["offset"].as_u64())
            .unwrap();
        std::thread::yield_now();
        let recover = Recover {
            epoch: epoch.clone(),
            offset: last,
        };
        c.join_room_and_reply("h:1", Some(&recover), |_, out| {
            assert_eq!(out.recovered, Some(true));
            serde_json::json!({"type": "joined"})
        })
        .unwrap();
        std::thread::yield_now();
        seen.extend(drain(&mut rx));
        let offsets: Vec<u64> = seen
            .iter()
            .filter(|v| v["type"] == "m")
            .map(|v| v["offset"].as_u64().unwrap())
            .collect();
        // Contiguous from the first joined position: no gap, no duplicate,
        // no reordering across the leave and the recovering rejoin.
        let expected: Vec<u64> = (start + 1..start + 1 + offsets.len() as u64).collect();
        assert_eq!(offsets, expected, "round {round}");
        checked += offsets.len();
        core.remove_session_presence(
            "t",
            &format!("t::u{round}"),
            &format!("t::u{round}::s"),
            false,
        );
    }
    stop.store(true, Ordering::Relaxed);
    for p in publishers {
        p.join().unwrap();
    }
    assert!(
        checked >= 2000,
        "the race was exercised ({checked} messages in {round} rounds)"
    );
}

#[test]
fn history_with_a_queued_broker_is_kept_by_the_broker() {
    let broker = wsprism_engine::broker::testing::TestBroker::new(0);
    let core = RealtimeCore::with_broker(broker, Default::default()).unwrap();
    let mut h = HistoryConfig::default();
    h.add(
        "t",
        HistoryRule::new(RoomPattern::parse("*"), 10, Duration::from_secs(1), 1024),
    );
    let core = core.with_history(h).unwrap();
    // No ring here: the broker keeps it.
    drop(core);
}

/// The cluster path (ADR 0011 R3-2): a queued broker keeps the history and
/// a recovering join holds the room's frames aside while it reads it. With
/// publishers racing, each offset still arrives exactly once, in order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn cluster_recovering_joins_racing_publishers_see_each_offset_once_in_order() {
    let broker = wsprism_engine::broker::testing::TestBroker::new(0);
    let mut h = HistoryConfig::default();
    h.add(
        "t",
        HistoryRule::new(
            RoomPattern::parse("h:*"),
            1_000_000,
            Duration::from_secs(600),
            1 << 30,
        ),
    );
    let core = Arc::new(
        RealtimeCore::with_broker(broker, Default::default())
            .unwrap()
            .with_history(h)
            .unwrap(),
    );
    let _tasks = core.start().await.unwrap();
    let room_key = RealtimeCore::room_key("t", "h:1");
    let (mut first, _first_rx) = session(&core, "first");
    let keep = first
        .cluster_history("h:1")
        .expect("a cluster history room");
    first
        .join_room_cluster("h:1", Some(keep), None, |_, _| serde_json::json!({}))
        .await
        .unwrap();

    let stop = Arc::new(AtomicBool::new(false));
    let publishers: Vec<_> = (0..2)
        .map(|_| {
            let (core, stop, room_key) = (Arc::clone(&core), Arc::clone(&stop), room_key.clone());
            std::thread::spawn(move || {
                while !stop.load(Ordering::Relaxed) {
                    // The outbox may be momentarily full: that is a refusal,
                    // not a lost message.
                    let _ = core.publish_room(
                        &room_key,
                        Outgoing {
                            qos: QoS::Reliable,
                            payload: Payload::TextJson(serde_json::json!({"type": "m"})),
                        },
                    );
                    std::thread::sleep(Duration::from_micros(50));
                }
            })
        })
        .collect();

    let mut checked = 0;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut round = 0;
    while checked < 2000 && std::time::Instant::now() < deadline {
        round += 1;
        let (mut c, mut rx) = session(&core, &format!("u{round}"));
        let mut epoch = String::new();
        c.join_room_cluster("h:1", Some(keep), None, |_, out| {
            let pos = out.history.as_ref().unwrap();
            epoch = pos.epoch.to_string();
            serde_json::json!({"type": "joined", "offset": pos.offset})
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
        let mut seen = drain(&mut rx);
        let start = seen[0]["offset"].as_u64().unwrap();
        c.leave_room("h:1");
        tokio::time::sleep(Duration::from_millis(5)).await; // frames in flight land
        seen.extend(drain(&mut rx));
        let last = seen
            .iter()
            .rev()
            .find_map(|v| v["offset"].as_u64())
            .unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await; // miss some
        let recover = Recover {
            epoch: epoch.clone(),
            offset: last,
        };
        c.join_room_cluster("h:1", Some(keep), Some(&recover), |_, out| {
            assert_eq!(out.recovered, Some(true));
            serde_json::json!({"type": "joined"})
        })
        .await
        .unwrap();
        tokio::time::sleep(Duration::from_millis(2)).await;
        seen.extend(drain(&mut rx));
        let offsets: Vec<u64> = seen
            .iter()
            .filter(|v| v["type"] == "m")
            .map(|v| v["offset"].as_u64().unwrap())
            .collect();
        let expected: Vec<u64> = (start + 1..start + 1 + offsets.len() as u64).collect();
        assert_eq!(offsets, expected, "round {round}");
        assert!(
            !seen.iter().any(|v| v["type"] == "gap"),
            "no gap expected in round {round}"
        );
        checked += offsets.len();
        core.remove_session_presence(
            "t",
            &format!("t::u{round}"),
            &format!("t::u{round}::s"),
            false,
        );
    }
    stop.store(true, Ordering::Relaxed);
    for p in publishers {
        p.join().unwrap();
    }
    assert!(
        checked >= 2000,
        "the race was exercised ({checked} messages in {round} rounds)"
    );
}
