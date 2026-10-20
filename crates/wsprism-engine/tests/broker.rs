//! The broker boundary with a queued test broker (ADR 0011 R1, B3-B5):
//! every room path goes through the broker, Hot is local-first, the outbox
//! is bounded per tenant, failures are retried in order, subscriptions
//! follow room creation/removal exactly, and member ids carry the node slot.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc;

use wsprism_engine::broker::testing::TestBroker;
use wsprism_engine::broker::{OutboxLimits, Subscription};
use wsprism_engine::services::HotRelayService;
use wsprism_engine::spi::{
    BrokerTasks, Connection, CtxIdentity, OutFrame, RealtimeCore, SessionSignals,
};
use wsprism_engine::{HotService, Outgoing, Payload, Permissions, QoS, RealtimeCtx};
use wsprism_protocol::error::WsPrismError;
use wsprism_protocol::protocol::hot::HotFrame;
use wsprism_protocol::protocol::ProtocolVersion;

struct Node {
    core: Arc<RealtimeCore>,
    broker: Arc<TestBroker>,
    _tasks: BrokerTasks,
}

async fn node(slot: u16, limits: OutboxLimits) -> Node {
    let broker = TestBroker::new(slot);
    let core = Arc::new(RealtimeCore::with_broker(broker.clone(), limits).unwrap());
    let tasks = core.start().await.unwrap();
    Node {
        core,
        broker,
        _tasks: tasks,
    }
}

struct Client {
    ctx: RealtimeCtx,
    rx: mpsc::Receiver<OutFrame>,
    signals: Arc<SessionSignals>,
}

impl Client {
    fn ctx_signals(&self) -> &SessionSignals {
        &self.signals
    }
}

fn client(n: &Node, tenant: &str, user: &str, presence_events: bool) -> Client {
    let (tx, rx) = mpsc::channel(1024);
    let signals = SessionSignals::new();
    let conn = Connection {
        tx,
        signals: Arc::clone(&signals),
    };
    let session_key = format!("{tenant}::{user}::s");
    n.core
        .sessions
        .try_insert(
            tenant.into(),
            format!("{tenant}::{user}"),
            session_key.clone(),
            "s".into(),
            conn.clone(),
            0,
        )
        .unwrap();
    let id = CtxIdentity {
        tenant: tenant.into(),
        user: user.into(),
        session_id: "s".into(),
        session_key: session_key.into(),
        trace_id: "tr".into(),
        protocol: ProtocolVersion::V2,
        permissions: Permissions::default(),
        hot_echo_to_sender: false,
        presence_events,
    };
    Client {
        ctx: RealtimeCtx::new(id, Arc::default(), conn, Arc::clone(&n.core)),
        rx,
        signals,
    }
}

fn text(s: &str) -> Outgoing {
    Outgoing {
        qos: QoS::Reliable,
        payload: Payload::Utf8Bytes(Bytes::from(s.to_string())),
    }
}

async fn recv_text(c: &mut Client) -> String {
    match tokio::time::timeout(Duration::from_secs(2), c.rx.recv()).await {
        Ok(Some(OutFrame::Text(t))) => String::from_utf8(t.as_bytes().to_vec()).unwrap(),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

/// Nothing arrives within a short wait.
async fn quiet(c: &mut Client) {
    if let Ok(f) = tokio::time::timeout(Duration::from_millis(100), c.rx.recv()).await {
        panic!("unexpected frame {f:?}");
    }
}

fn published_rooms(b: &TestBroker) -> Vec<(String, String)> {
    b.take_published()
        .into_iter()
        .map(|f| {
            let body = match &f.body {
                OutFrame::Text(t) => String::from_utf8(t.as_bytes().to_vec()).unwrap(),
                OutFrame::Hot(_) => "<hot>".into(),
                other => format!("{other:?}"),
            };
            (f.room_key.to_string(), body)
        })
        .collect()
}

async fn settle() {
    tokio::time::sleep(Duration::from_millis(50)).await;
}

#[tokio::test]
async fn ext_publishes_reach_members_only_through_the_broker() {
    let n = node(0, OutboxLimits::default()).await;
    let mut a = client(&n, "t", "a", false);
    let mut b = client(&n, "t", "b", false);
    a.ctx.join_room("r").unwrap();
    b.ctx.join_room("r").unwrap();

    n.broker.pause();
    a.ctx.publish_room_reliable("r", text("one")).unwrap();
    quiet(&mut b).await; // not delivered until the broker passes it on
    n.broker.resume();
    assert_eq!(recv_text(&mut b).await, "one");
    assert_eq!(recv_text(&mut a).await, "one"); // the publisher too (Reliable room publish)
    assert_eq!(
        published_rooms(&n.broker),
        vec![("t::r".to_string(), "one".to_string())]
    );
}

/// Every room path of the engine is recorded by the broker (B3).
#[tokio::test]
async fn every_engine_room_path_goes_through_the_broker() {
    let n = node(0, OutboxLimits::default()).await;
    let mut a = client(&n, "t", "a", true);
    let mut b = client(&n, "t", "b", true);
    a.ctx.join_room("r").unwrap();
    settle().await;
    n.broker.take_published();

    // Presence: join (member_joined to the others).
    b.ctx.join_room("r").unwrap();
    // Service publishes: reliable, lossy, others.
    b.ctx.publish_room_reliable("r", text("rel")).unwrap();
    b.ctx.publish_room_lossy("r", text("lossy")).unwrap();
    b.ctx.publish_room_others("r", text("others")).unwrap();
    // From a spawned task.
    b.ctx
        .publisher()
        .publish_room_reliable("r", text("pub"))
        .unwrap();
    // Server side (what the server API's publish calls).
    n.core
        .publish_room(&RealtimeCore::room_key("t", "r"), text("api"))
        .unwrap();
    // Presence: leave, then a disconnect.
    b.ctx.leave_room("r");
    b.ctx.join_room("r").unwrap();
    settle().await;
    n.core.remove_session_presence("t", "t::b", "t::b::s", true);
    settle().await;

    let got: Vec<String> = published_rooms(&n.broker)
        .into_iter()
        .map(|(room, body)| {
            assert_eq!(room, "t::r");
            let v: serde_json::Value = serde_json::from_str(&body).unwrap_or(body.into());
            match v["type"].as_str() {
                Some(t) => t.to_string(),
                None => v.as_str().unwrap().to_string(),
            }
        })
        .collect();
    assert_eq!(
        got,
        [
            "member_joined",
            "rel",
            "lossy",
            "others",
            "pub",
            "api",
            "member_left",
            "member_joined",
            "member_left"
        ]
    );

    // And `a` got them back through the broker, `except` honoured: b's
    // member_joined is not sent to b, "others" is not sent to b.
    let mut seen = Vec::new();
    while let Ok(Some(OutFrame::Text(t))) =
        tokio::time::timeout(Duration::from_millis(100), a.rx.recv()).await
    {
        seen.push(String::from_utf8(t.as_bytes().to_vec()).unwrap());
    }
    assert_eq!(seen.len(), 9, "{seen:?}");
}

#[tokio::test]
async fn hot_is_delivered_locally_first_and_published() {
    let n = node(0, OutboxLimits::default()).await;
    let mut a = client(&n, "t", "a", false);
    let mut b = client(&n, "t", "b", false);
    let joined = a.ctx.join_room("r").unwrap();
    b.ctx.join_room("r").unwrap();
    settle().await;
    n.broker.take_published();

    n.broker.pause();
    let relay = HotRelayService::new(1);
    let frame = HotFrame {
        v: 2,
        svc_id: 1,
        opcode: 1,
        flags: 0,
        seq: None,
        chan: Some(joined.room_id),
        key: None,
        payload: Bytes::from_static(b"state"),
    };
    relay.handle_hot(&mut a.ctx, frame).await.unwrap();
    // Delivered at once although the broker is held, to b only (no echo),
    // and not a second time when the broker passes it on.
    match tokio::time::timeout(Duration::from_secs(1), b.rx.recv()).await {
        Ok(Some(OutFrame::Hot(h))) => assert_eq!(&h.v1[..], b"state"),
        other => panic!("{other:?}"),
    }
    n.broker.resume();
    settle().await;
    assert_eq!(
        published_rooms(&n.broker),
        vec![("t::r".to_string(), "<hot>".to_string())]
    );
    quiet(&mut b).await;
    quiet(&mut a).await;
}

#[tokio::test]
async fn a_full_outbox_refuses_reliable_and_drops_lossy() {
    let n = node(0, OutboxLimits::new(4, 4)).await;
    let mut a = client(&n, "t", "a", false);
    a.ctx.join_room("r").unwrap();
    settle().await;
    n.broker.pause();
    // The drain task holds one batch at the paused broker; fill the rest.
    let mut accepted = 0;
    let mut refused = None;
    for i in 0..20 {
        match a.ctx.publish_room_reliable("r", text(&format!("m{i}"))) {
            Ok(()) => accepted += 1,
            Err(e) => {
                refused = Some(e);
                break;
            }
        }
    }
    assert!(
        matches!(refused, Some(WsPrismError::Unavailable(_))),
        "{refused:?}"
    );
    assert!((4..20).contains(&accepted), "{accepted}");
    // Lossy is dropped silently.
    a.ctx.publish_room_lossy("r", text("dropped")).unwrap();
    assert!(n.core.broker_counters().unwrap().refused() >= 2);

    // Accepted frames all arrive, in order, once the broker is back.
    n.broker.resume();
    for i in 0..accepted {
        assert_eq!(recv_text(&mut a).await, format!("m{i}"));
    }
    quiet(&mut a).await;
}

#[tokio::test]
async fn one_tenant_cannot_take_another_tenants_share() {
    let n = node(0, OutboxLimits::new(100, 3)).await;
    let mut a = client(&n, "noisy", "a", false);
    let mut b = client(&n, "quiet", "b", false);
    a.ctx.join_room("r").unwrap();
    b.ctx.join_room("r").unwrap();
    settle().await;
    n.broker.pause();
    let mut noisy_refused = false;
    for i in 0..50 {
        if a.ctx
            .publish_room_reliable("r", text(&format!("n{i}")))
            .is_err()
        {
            noisy_refused = true;
            break;
        }
    }
    assert!(noisy_refused, "the noisy tenant hits its share");
    for i in 0..3 {
        b.ctx
            .publish_room_reliable("r", text(&format!("q{i}")))
            .expect("the quiet tenant still has its share");
    }
    n.broker.resume();
    for i in 0..3 {
        assert_eq!(recv_text(&mut b).await, format!("q{i}"));
    }
}

#[tokio::test]
async fn unavailable_is_retried_in_order_and_rejected_is_dropped() {
    let n = node(0, OutboxLimits::default()).await;
    let mut a = client(&n, "t", "a", false);
    a.ctx.join_room("r").unwrap();
    settle().await;

    n.broker.fail_next(3);
    for i in 0..5 {
        a.ctx
            .publish_room_reliable("r", text(&format!("m{i}")))
            .unwrap();
    }
    for i in 0..5 {
        assert_eq!(recv_text(&mut a).await, format!("m{i}"));
    }
    let c = n.core.broker_counters().unwrap();
    assert!(c.retries() >= 3, "{}", c.retries());

    n.broker.reject_next(1);
    a.ctx.publish_room_reliable("r", text("lost")).unwrap();
    settle().await;
    a.ctx.publish_room_reliable("r", text("after")).unwrap();
    assert_eq!(recv_text(&mut a).await, "after");
    assert_eq!(c.failed(), 1);
}

/// B5: with slow subscription commands and joins/leaves racing on many
/// threads, the broker ends subscribed to exactly the rooms that exist, and
/// never sees a command out of order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn subscriptions_follow_rooms_exactly_under_races() {
    // Many rooms for 8 sessions, so rooms are created and dropped often.
    const ROOMS: u32 = 20;
    let n = node(0, OutboxLimits::default()).await;
    n.broker.set_subscription_delay(Duration::from_micros(200));
    let mut handles = Vec::new();
    for t in 0..8 {
        let mut c = client(&n, "t", &format!("u{t}"), false);
        handles.push(tokio::spawn(async move {
            for i in 0..300u32 {
                let room = format!("r{}", (i * 7 + t) % ROOMS);
                c.ctx.join_room(&room).unwrap();
                if i % 3 != 0 {
                    c.ctx.leave_room(&room);
                }
                if i % 50 == 0 {
                    tokio::task::yield_now().await;
                }
            }
            c
        }));
    }
    let mut clients = Vec::new();
    for h in handles {
        clients.push(h.await.unwrap());
    }
    // Wait for the commands to drain.
    let expected: std::collections::HashSet<String> = (0..ROOMS)
        .map(|r| format!("t::r{r}"))
        .filter(|rk| n.core.presence.member_count(rk) > 0)
        .collect();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while n.broker.subscribed() != expected && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(n.broker.subscribed(), expected);
    assert_eq!(n.broker.anomalies(), 0);
    // (Whether a room emptied during the race depends on scheduling; the
    // phase below unsubscribes every room deterministically.)

    // Everyone leaves: every room is unsubscribed.
    for c in &mut clients {
        for r in 0..ROOMS {
            c.ctx.leave_room(&format!("r{r}"));
        }
    }
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while !n.broker.subscribed().is_empty() && tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(n.broker.subscribed().is_empty());
    assert_eq!(n.broker.anomalies(), 0);
    assert!(n
        .broker
        .commands()
        .iter()
        .any(|c| matches!(c, Subscription::Unsubscribe(_))));
}

#[tokio::test]
async fn member_ids_carry_the_node_slot() {
    let n = node(5, OutboxLimits::default()).await;
    let mut a = client(&n, "t", "a", false);
    let mut b = client(&n, "t", "b", false);
    let ja = a.ctx.join_room("r").unwrap();
    let jb = b.ctx.join_room("r").unwrap();
    assert_eq!(ja.member_id >> 22, 5);
    assert_eq!(jb.member_id >> 22, 5);
    assert_ne!(ja.member_id, jb.member_id);
    // Per room: another room starts its own counter.
    let jc = a.ctx.join_room("other").unwrap();
    assert_eq!(jc.member_id, ja.member_id);
}

#[tokio::test]
async fn node_slots_are_bounded() {
    let broker = TestBroker::new(1024);
    let core = Arc::new(RealtimeCore::with_broker(broker, OutboxLimits::default()).unwrap());
    assert!(core.start().await.is_err());
}

/// Another node's Hot frame gets this node's room id (`chan`) in the bytes
/// and in the slot key; its `except` does not apply here.
#[tokio::test]
async fn remote_hot_frames_get_the_local_room_id() {
    let n = node(0, OutboxLimits::default()).await;
    let mut a = client(&n, "t", "a", false);
    // Make this node's room id differ from the publisher's (which used 9).
    n.core
        .presence
        .try_join(
            "t",
            "t::pad",
            "t::x",
            wsprism_engine::spi::Member {
                key: "t::x::s".into(),
                conn: Connection {
                    tx: mpsc::channel(1).0,
                    signals: SessionSignals::new(),
                },
                member_id: 0,
                skip_upto: 0,
                user: "x".into(),
            },
            &Default::default(),
        )
        .unwrap();
    let joined = a.ctx.join_room("r").unwrap();
    settle().await;
    let v2 = wsprism_protocol::protocol::hot::encode_hot_frame_v2(
        &wsprism_protocol::protocol::hot::HotOutV2 {
            chan: 9,
            svc_id: 1,
            opcode: 2,
            key: Some(5),
            seq: None,
            sender: (3 << 22) | 1,
            payload: Bytes::from_static(b"s"),
        },
    );
    let hot = OutFrame::Hot(Arc::new(wsprism_engine::spi::HotOut {
        v1: Bytes::from_static(b"s"),
        v2,
        slot_key: Some((9, (3 << 22) | 1, 1, 5)),
    }));
    let mut f = wsprism_engine::broker::BrokerFrame::new(3, "t::r", QoS::Lossy, hot);
    f = f.except(Some("t::a::s")); // another node's session key: ignored here
    n.broker.inject(f).await;
    settle().await;
    // Keyed: it lands in a's Hot slot, under the local room id.
    let mut got = Vec::new();
    a.ctx_signals().hot.drain_into(&mut got);
    assert_eq!(got.len(), 1);
    let h = &got[0];
    assert_ne!(joined.room_id, 9);
    assert_eq!(&h.v2[2..6], &joined.room_id.to_le_bytes());
    assert_eq!(h.slot_key, Some((joined.room_id, (3 << 22) | 1, 1, 5)));
}

#[tokio::test]
async fn a_subscription_gap_is_reported_to_the_room() {
    let n = node(0, OutboxLimits::default()).await;
    let mut a = client(&n, "t", "a", false);
    a.ctx.join_room("r").unwrap();
    settle().await;
    n.broker.gap(&["t::r", "t::nobody-here"]).await;
    let v: serde_json::Value = serde_json::from_str(&recv_text(&mut a).await).unwrap();
    assert_eq!(
        (v["type"].as_str(), v["room"].as_str()),
        (Some("gap"), Some("r"))
    );
}

/// Past saturation the outbox fails fast: once its oldest frame has waited
/// `max_wait`, new Reliable frames are refused although there is room.
#[tokio::test]
async fn a_stuck_outbox_refuses_after_max_wait() {
    let n = node(
        0,
        OutboxLimits::new(10_000, 10_000).max_wait(Duration::from_millis(50)),
    )
    .await;
    let mut a = client(&n, "t", "a", false);
    a.ctx.join_room("r").unwrap();
    settle().await;
    n.broker.pause();
    // The drain task takes the first frame and hangs on the paused broker;
    // the second stays queued and ages.
    a.ctx.publish_room_reliable("r", text("one")).unwrap();
    settle().await;
    a.ctx.publish_room_reliable("r", text("two")).unwrap();
    assert!(
        a.ctx.publish_room_reliable("r", text("three")).is_ok(),
        "fresh"
    );
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(
        matches!(
            a.ctx.publish_room_reliable("r", text("late")),
            Err(WsPrismError::Unavailable(_))
        ),
        "refused once the oldest frame waited too long"
    );
    n.broker.resume();
    for m in ["one", "two", "three"] {
        assert_eq!(recv_text(&mut a).await, m);
    }
    // Drained: accepted again.
    settle().await;
    a.ctx.publish_room_reliable("r", text("again")).unwrap();
    assert_eq!(recv_text(&mut a).await, "again");
}
