//! Two gateway nodes sharing rooms through Redis (ADR 0011 R3). Needs a
//! local Redis: `WSPRISM_TEST_REDIS=redis://<host>:6379` (see
//! `bench/redis-test.sh`, which runs one on an isolated docker network);
//! without it these tests pass without doing anything.

mod common;

use std::time::Duration;

use common::*;
use serde_json::{json, Value};

fn redis_url() -> Option<String> {
    let url = std::env::var("WSPRISM_TEST_REDIS").ok();
    if url.is_none() {
        eprintln!("WSPRISM_TEST_REDIS not set: skipping the cluster test");
    }
    url
}

/// A unique key prefix per test, so tests sharing a Redis never see each
/// other's rooms or node slots.
fn prefix(name: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("t{}-{name}", n % 1_000_000_000)
}

const KEY: &str = "key-c";
const KEY_D: &str = "key-d";

async fn node(url: &str, prefix: &str) -> ApiGateway {
    node_with(url, prefix, 30).await
}

async fn node_with(url: &str, prefix: &str, slot_ttl_s: u64) -> ApiGateway {
    let d = ring::digest::digest(&ring::digest::SHA256, KEY.as_bytes());
    let hash: String = d.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let d2 = ring::digest::digest(&ring::digest::SHA256, KEY_D.as_bytes());
    let hash_d: String = d2.as_ref().iter().map(|b| format!("{b:02x}")).collect();
    let yaml = format!(
        r#"
version: 1
gateway:
  api: {{ listen: "127.0.0.1:0" }}
  broker: {{ kind: redis, url: "{url}", prefix: "{prefix}", pubsub_connections: 2, slot_ttl_s: {slot_ttl_s} }}
tenants:
  - id: t
    auth: [{{ kind: dev }}, {{ kind: ticket }}]
    api_keys: [{{ name: backend, sha256: "{hash}" }}]
    policy: {{ ext_allowlist: ["room:*", "chat:*"], hot_allowlist: ["1:*"], presence_events: true }}
    history: [{{ rooms: "chat:*", size: 100, ttl_s: 600 }}, {{ rooms: "small:*", size: 3, ttl_s: 600 }}]
  - id: d
    api_keys: [{{ name: backend-d, sha256: "{hash_d}" }}]
    policy: {{ ext_allowlist: ["room:*"], sessions: {{ mode: single, max_sessions_per_user: 1, on_exceed: deny }} }}
  - id: k
    policy: {{ ext_allowlist: ["room:*"], sessions: {{ mode: multi, max_sessions_per_user: 2, on_exceed: kick_oldest }} }}
"#
    );
    let cfg = wsprism_gateway::config::load_from_str(&yaml).unwrap();
    // Not `common::builder()`: the broker comes from this config.
    spawn_built_with_api(
        wsprism_gateway::Gateway::builder()
            .config(cfg)
            .build()
            .unwrap(),
    )
    .await
}

async fn user(gw: &ApiGateway, name: &str) -> Ws {
    user_in(gw, "t", name, None).await
}

/// Connect as `name` to `tenant` (optionally with a client sid); retries
/// until the node has started.
async fn user_in(gw: &ApiGateway, tenant: &str, name: &str, sid: Option<&str>) -> Ws {
    try_user_in(gw, tenant, name, sid)
        .await
        .unwrap_or_else(|e| panic!("connect: {e}"))
}

async fn try_user_in(
    gw: &ApiGateway,
    tenant: &str,
    name: &str,
    sid: Option<&str>,
) -> Result<Ws, String> {
    let bearer = format!("Bearer dev:{name}");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    // The node accepts once its broker has started (slot claimed).
    loop {
        match try_connect_with(
            &ws_url(gw.addr, tenant, sid),
            &[
                ("authorization", &bearer),
                ("sec-websocket-protocol", "wsprism.v2.json"),
            ],
        )
        .await
        {
            Ok((mut ws, _)) => match next_event(&mut ws).await {
                Event::Json(v) if v["type"] == "authed" => return Ok(ws),
                other => return Err(format!("{other:?}")),
            },
            Err(e)
                if e.contains("Connection refused") && tokio::time::Instant::now() < deadline =>
            {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

async fn next_of(ws: &mut Ws, ty: &str) -> Value {
    loop {
        match next_event(ws).await {
            Event::Json(v) if v["type"] == ty => return v,
            Event::Json(_) | Event::Binary(_) => continue,
            other => panic!("expected {ty}, got {other:?}"),
        }
    }
}

async fn join(ws: &mut Ws, room: &str) -> Value {
    send_json(ws, json!({"svc":"room","type":"join","room":room,"seq":1})).await;
    let j = next_of(ws, "joined").await;
    // Subscriptions are applied asynchronously (ADR 0011 B5).
    tokio::time::sleep(Duration::from_millis(150)).await;
    j
}

#[tokio::test]
async fn rooms_span_nodes() {
    let Some(url) = redis_url() else { return };
    let p = prefix("span");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut alice = user(&a_node, "alice").await;
    let mut bob = user(&b_node, "bob").await;
    let ja = join(&mut alice, "r").await;
    let jb = join(&mut bob, "r").await;

    // Member ids carry different node slots: unique across the cluster.
    let (ma, mb) = (
        ja["member_id"].as_u64().unwrap(),
        jb["member_id"].as_u64().unwrap(),
    );
    assert_ne!(ma >> 22, mb >> 22, "{ja} {jb}");
    // Alice hears of bob, who joined on the other node.
    let ev = next_of(&mut alice, "member_joined").await;
    assert_eq!(ev["data"]["user"], "bob");

    send_json(
        &mut alice,
        json!({"svc":"chat","type":"send","room":"r","data":{"msg":"from a"}}),
    )
    .await;
    assert_eq!(next_of(&mut bob, "msg").await["data"]["msg"], "from a");
    assert_eq!(next_of(&mut alice, "msg").await["data"]["msg"], "from a");

    // The server API on node B reaches alice on node A.
    let (status, body) = http(
        b_node.api,
        "POST",
        "/api/v1/publish",
        Some(KEY),
        Some(json!({"room":"r","data":{"x":1}})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(next_of(&mut alice, "publish").await["data"]["x"], 1);

    // Bob leaves by disconnecting: alice on the other node is told.
    drop(bob);
    let ev = next_of(&mut alice, "member_left").await;
    assert_eq!(ev["data"]["member_id"].as_u64(), Some(mb));
}

#[tokio::test]
async fn hot_state_crosses_nodes_with_the_local_room_id() {
    let Some(url) = redis_url() else { return };
    let p = prefix("hot");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut alice = user(&a_node, "alice").await;
    let mut bob = user(&b_node, "bob").await;
    // Make the room ids differ between the nodes.
    join(&mut bob, "pad").await;
    let ja = join(&mut alice, "r").await;
    let jb = join(&mut bob, "r").await;
    let (chan_a, chan_b) = (
        ja["chan"].as_u64().unwrap() as u32,
        jb["chan"].as_u64().unwrap() as u32,
    );
    assert_ne!(chan_a, chan_b);

    // Alice publishes state on node A (v2: kind, flags, chan, svc, op, payload).
    let mut f = vec![0x01, 0x00];
    f.extend_from_slice(&chan_a.to_le_bytes());
    f.extend_from_slice(&[1, 2, b's']);
    send_bin(&mut alice, f).await;
    loop {
        match next_event(&mut bob).await {
            Event::Binary(b) => {
                assert_eq!(&b[2..6], &chan_b.to_le_bytes(), "bob's room id");
                let sender = u32::from_le_bytes(b[8..12].try_into().unwrap());
                assert_eq!(u64::from(sender), ja["member_id"].as_u64().unwrap());
                assert_eq!(b.last(), Some(&b's'));
                break;
            }
            Event::Json(_) => continue,
            other => panic!("{other:?}"),
        }
    }
}

/// The node slot outlives its TTL while the node runs (heartbeat), and a
/// second node takes another slot.
#[tokio::test]
async fn node_slots_are_kept_alive_and_distinct() {
    let Some(url) = redis_url() else { return };
    let p = prefix("slot");
    let a = node_with(&url, &p, 3).await;
    let _ = user(&a, "alice").await; // started
    let b = node_with(&url, &p, 3).await;
    let _ = user(&b, "bob").await;
    tokio::time::sleep(Duration::from_secs(4)).await;
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    for slot in 0..2 {
        let owner: Option<String> = redis::cmd("GET")
            .arg(format!("{p}:slot:{slot}"))
            .query_async(&mut c)
            .await
            .unwrap();
        assert!(owner.is_some(), "slot {slot} expired despite the heartbeat");
    }
}

/// Cluster history (ADR 0011 R3-2): offsets come from Redis, so they agree
/// across nodes, and a client that left node A resumes on node B.
#[tokio::test]
async fn history_is_shared_and_recovery_works_across_nodes() {
    let Some(url) = redis_url() else { return };
    let p = prefix("hist");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut alice = user(&a_node, "alice").await;
    let mut bob = user(&b_node, "bob").await;
    let ja = join(&mut alice, "chat:1").await;
    let epoch = ja["epoch"].as_str().expect("epoch").to_string();
    assert_eq!(ja["offset"], 0);
    let jb = join(&mut bob, "chat:1").await;
    assert_eq!(
        jb["epoch"].as_str(),
        Some(epoch.as_str()),
        "one history for the cluster"
    );

    send_json(
        &mut bob,
        json!({"svc":"chat","type":"send","room":"chat:1","data":{"msg":"one"}}),
    )
    .await;
    let on_a = next_of(&mut alice, "msg").await;
    let on_b = next_of(&mut bob, "msg").await;
    assert_eq!(
        (on_a["offset"].as_u64(), on_b["offset"].as_u64()),
        (Some(1), Some(1))
    );

    // Alice drops off node A; bob keeps talking on node B.
    drop(alice);
    for m in ["two", "three", "four"] {
        send_json(
            &mut bob,
            json!({"svc":"chat","type":"send","room":"chat:1","data":{"msg":m}}),
        )
        .await;
    }
    for _ in 0..3 {
        next_of(&mut bob, "msg").await;
    }

    // She comes back on node B and resumes after offset 1.
    let mut alice = user(&b_node, "alice").await;
    send_json(
        &mut alice,
        json!({"svc":"room","type":"join","room":"chat:1","seq":2,
               "data":{"recover":{"epoch": epoch, "offset": 1}}}),
    )
    .await;
    let j = next_of(&mut alice, "joined").await;
    assert_eq!(j["recovered"], true, "{j}");
    assert_eq!(j["offset"], 4);
    let mut got = Vec::new();
    for _ in 0..3 {
        let m = next_of(&mut alice, "msg").await;
        got.push((
            m["offset"].as_u64().unwrap(),
            m["data"]["msg"].as_str().unwrap().to_string(),
        ));
    }
    assert_eq!(
        got,
        [(2, "two".into()), (3, "three".into()), (4, "four".into())]
    );

    // Then live, from node A this time.
    let mut carol = user(&a_node, "carol").await;
    join(&mut carol, "chat:1").await;
    send_json(
        &mut carol,
        json!({"svc":"chat","type":"send","room":"chat:1","data":{"msg":"five"}}),
    )
    .await;
    let m = next_of(&mut alice, "msg").await;
    assert_eq!(
        (m["offset"].as_u64(), m["data"]["msg"].as_str()),
        (Some(5), Some("five"))
    );
}

/// If Redis loses part of a room's history (restart, eviction), a new epoch
/// starts: a client resyncs instead of resuming from offsets that now mean
/// something else.
#[tokio::test]
async fn lost_history_starts_a_new_epoch() {
    let Some(url) = redis_url() else { return };
    let p = prefix("lost");
    let a_node = node(&url, &p).await;
    let mut alice = user(&a_node, "alice").await;
    join(&mut alice, "chat:1").await;
    send_json(
        &mut alice,
        json!({"svc":"chat","type":"send","room":"chat:1","data":{"msg":"one"}}),
    )
    .await;
    let m = next_of(&mut alice, "msg").await;
    let epoch = {
        let mut probe = user(&a_node, "probe").await;
        join(&mut probe, "chat:1").await["epoch"].clone()
    };
    assert_eq!(m["offset"], 1);

    // Only the counter is lost.
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let _: i64 = redis::cmd("DEL")
        .arg(format!("{p}:h:{{t::chat:1}}:seq"))
        .query_async(&mut c)
        .await
        .unwrap();

    let mut bob = user(&a_node, "bob").await;
    send_json(
        &mut bob,
        json!({"svc":"room","type":"join","room":"chat:1","seq":2,
               "data":{"recover":{"epoch": epoch, "offset": 1}}}),
    )
    .await;
    let j = next_of(&mut bob, "joined").await;
    assert_eq!(j["recovered"], false, "{j}");
    assert_ne!(j["epoch"], epoch, "a new epoch");
    assert_eq!(j["offset"], 0);
}

/// One-time tickets are shared (ADR 0011 §6): issued by node A, redeemed on
/// node B, and only once anywhere. Redis holds a hash, never the ticket.
#[tokio::test]
async fn tickets_work_on_any_node_once() {
    let Some(url) = redis_url() else { return };
    let p = prefix("tk");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let _ = user(&a_node, "warm").await; // both nodes started
    let _ = user(&b_node, "warm").await;
    let (status, body) = http(
        a_node.api,
        "POST",
        "/api/v1/tickets",
        Some(KEY),
        Some(json!({"user": "tia", "permissions": {"subscribe": ["chat:*"]}})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    let ticket = body["ticket"].as_str().unwrap().to_string();

    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{p}:kv:*"))
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(keys.len(), 1);
    assert!(
        !keys[0].contains(&ticket[4..]),
        "the key is a hash: {keys:?}"
    );

    let connect = |gw: &ApiGateway| {
        let url = ws_url(gw.addr, "t", None);
        let bearer = format!("Bearer {ticket}");
        async move {
            try_connect_with(
                &url,
                &[
                    ("authorization", &bearer),
                    ("sec-websocket-protocol", "wsprism.v2.json"),
                ],
            )
            .await
        }
    };
    let (mut ws, _) = connect(&b_node).await.expect("redeemed on the other node");
    match next_event(&mut ws).await {
        Event::Json(v) if v["type"] == "authed" => assert_eq!(v["data"]["user"], "tia"),
        other => panic!("{other:?}"),
    }
    // Its permissions came along.
    send_json(
        &mut ws,
        json!({"svc":"room","type":"join","room":"lobby","seq":1}),
    )
    .await;
    assert_eq!(
        next_of(&mut ws, "error").await["data"]["code"],
        "NOT_ALLOWED"
    );

    let again = connect(&a_node).await;
    assert!(
        matches!(&again, Err(e) if e.contains("401")),
        "{:?}",
        again.err()
    );
}

/// The Redis script keeps at most `size` messages: resuming before them is
/// refused, resuming within them works (ADR 0011 R3-2).
#[tokio::test]
async fn redis_history_keeps_size_messages() {
    let Some(url) = redis_url() else { return };
    let p = prefix("size");
    let a_node = node(&url, &p).await;
    let mut bob = user(&a_node, "bob").await;
    let epoch = join(&mut bob, "small:1").await["epoch"].clone();
    for i in 1..=5 {
        send_json(
            &mut bob,
            json!({"svc":"chat","type":"send","room":"small:1","data":{"msg":format!("m{i}")}}),
        )
        .await;
        next_of(&mut bob, "msg").await;
    }
    let mut a = user(&a_node, "alice").await;
    for (from, ok) in [(1, false), (2, true), (4, true), (5, true)] {
        send_json(
            &mut a,
            json!({"svc":"room","type":"join","room":"small:1","seq":9,
                   "data":{"recover":{"epoch": epoch, "offset": from}}}),
        )
        .await;
        let j = next_of(&mut a, "joined").await;
        assert_eq!(j["recovered"], ok, "from {from}: {j}");
        if ok {
            for want in from + 1..=5 {
                assert_eq!(next_of(&mut a, "msg").await["offset"], want);
            }
        }
        send_json(
            &mut a,
            json!({"svc":"room","type":"leave","room":"small:1"}),
        )
        .await;
        next_of(&mut a, "left").await;
    }
}

/// A burst lands in the broker's batches, so one script call keeps many
/// messages (A15): offsets stay contiguous and in order, live and replayed.
#[tokio::test]
async fn a_burst_keeps_contiguous_offsets() {
    let Some(url) = redis_url() else { return };
    let p = prefix("burst");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut bob = user(&a_node, "bob").await;
    let epoch = join(&mut bob, "chat:1").await["epoch"].clone();
    let mut carol = user(&b_node, "carol").await;
    join(&mut carol, "chat:1").await;
    for i in 1..=50 {
        send_json(
            &mut bob,
            json!({"svc":"chat","type":"send","room":"chat:1","data":{"msg":format!("m{i}")}}),
        )
        .await;
    }
    for ws in [&mut bob, &mut carol] {
        let mut got = Vec::new();
        for _ in 0..50 {
            let m = next_of(ws, "msg").await;
            got.push((
                m["offset"].as_u64().unwrap(),
                m["data"]["msg"].as_str().unwrap().to_string(),
            ));
        }
        let want: Vec<(u64, String)> = (1..=50).map(|i| (i, format!("m{i}"))).collect();
        assert_eq!(got, want);
    }
    let mut a = user(&b_node, "alice").await;
    send_json(
        &mut a,
        json!({"svc":"room","type":"join","room":"chat:1","seq":2,
               "data":{"recover":{"epoch": epoch, "offset": 10}}}),
    )
    .await;
    assert_eq!(next_of(&mut a, "joined").await["recovered"], true);
    for want in 11..=50 {
        assert_eq!(next_of(&mut a, "msg").await["offset"], want);
    }
}

/// The next close frame's reason, skipping messages.
async fn closed_reason(ws: &mut Ws) -> String {
    loop {
        match next_event(ws).await {
            Event::Close(Some(f)) => return f.reason.to_string(),
            Event::Json(_) | Event::Binary(_) => continue,
            other => panic!("expected a close, got {other:?}"),
        }
    }
}

/// R4-1: a reconnect with the same sid on another node replaces the old
/// connection (S4 across nodes).
#[tokio::test]
async fn the_same_sid_on_another_node_replaces_the_old_connection() {
    let Some(url) = redis_url() else { return };
    let p = prefix("sid");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut old = user_in(&a_node, "t", "sam", Some("tab")).await;
    let mut new = user_in(&b_node, "t", "sam", Some("tab")).await;
    assert_eq!(closed_reason(&mut old).await, "replaced");
    // The new one is fine; another sid is not affected.
    let mut other = user_in(&a_node, "t", "sam", Some("tab2")).await;
    send_json(
        &mut new,
        json!({"svc":"room","type":"join","room":"r","seq":1}),
    )
    .await;
    next_of(&mut new, "joined").await;
    send_json(
        &mut other,
        json!({"svc":"room","type":"join","room":"r","seq":1}),
    )
    .await;
    next_of(&mut other, "joined").await;
}

/// R4-1: per-user limits hold across nodes - deny...
#[tokio::test]
async fn single_session_mode_denies_a_second_node() {
    let Some(url) = redis_url() else { return };
    let p = prefix("deny");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let _first = user_in(&a_node, "d", "solo", None).await;
    let second = try_user_in(&b_node, "d", "solo", None).await;
    assert!(
        matches!(&second, Err(e) if e.contains("TOO_MANY_SESSIONS")),
        "denied on the other node: {:?}",
        second.as_ref().err()
    );
    // Once the first is gone, the user may connect again.
    drop(_first);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let _again = user_in(&b_node, "d", "solo", None).await;
}

/// ...and kick_oldest closes the oldest session wherever it is.
#[tokio::test]
async fn kick_oldest_reaches_the_oldest_session_on_another_node() {
    let Some(url) = redis_url() else { return };
    let p = prefix("kick");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut first = user_in(&a_node, "k", "kim", None).await;
    tokio::time::sleep(Duration::from_millis(5)).await;
    let _second = user_in(&b_node, "k", "kim", None).await;
    let _third = user_in(&b_node, "k", "kim", None).await; // limit 2
    assert_eq!(closed_reason(&mut first).await, "max_sessions_exceeded");
}

/// R4-1: the server API's disconnect closes the user's sessions on every
/// node; a session of a node that died no longer counts.
#[tokio::test]
async fn api_disconnect_and_dead_nodes_span_the_cluster() {
    let Some(url) = redis_url() else { return };
    let p = prefix("disc");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut on_b = user_in(&b_node, "d", "dora", None).await;
    let (status, body) = http(
        a_node.api,
        "POST",
        "/api/v1/disconnect",
        Some(KEY_D),
        Some(json!({"user": "dora", "reason": "bye"})),
    )
    .await;
    assert_eq!((status, body["closed"].as_u64()), (200, Some(1)), "{body}");
    assert_eq!(closed_reason(&mut on_b).await, "disconnected");

    // An entry left by a node that died (its slot key is gone) does not
    // block a single-session user.
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let _: i64 = redis::cmd("HSET")
        .arg(format!("{p}:us:{{d::ghost}}"))
        .arg("d::ghost::s#dead")
        .arg("deadnode|777|s|1")
        .query_async(&mut c)
        .await
        .unwrap();
    let _ghost = user_in(&a_node, "d", "ghost", None).await;
}

/// R4-2: a joiner's snapshot and the server API's presence list cover
/// every node.
#[tokio::test]
async fn presence_spans_nodes() {
    let Some(url) = redis_url() else { return };
    let p = prefix("pres");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut alice = user(&a_node, "alice").await;
    join(&mut alice, "r").await;
    let mut bob = user(&b_node, "bob").await;
    let j = join(&mut bob, "r").await;
    let members: Vec<&str> = j["members"]
        .as_array()
        .expect("snapshot")
        .iter()
        .map(|m| m["user"].as_str().unwrap())
        .collect();
    assert_eq!(members, ["alice"], "{j}");
    let (status, body) = http(
        a_node.api,
        "GET",
        "/api/v1/rooms/r/presence",
        Some(KEY),
        None,
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["users"], json!(["alice", "bob"]));
}

/// R4-2 / A10: the members of a node that died are announced as having
/// left, once, although every live node looks for dead nodes.
#[tokio::test]
async fn a_dead_nodes_members_leave_once() {
    let Some(url) = redis_url() else { return };
    let p = prefix("dead");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut alice = user(&a_node, "alice").await;
    join(&mut alice, "r").await;
    let mut bob = user(&b_node, "bob").await;
    join(&mut bob, "r").await;
    // A node that died (no slot key) left "ghost" in the room.
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let _: () = redis::pipe()
        .cmd("HSET")
        .arg(format!("{p}:pr:{{t::r}}"))
        .arg("t::ghost::s#1")
        .arg("deadnode|999|ghost|4194305|1")
        .ignore()
        .cmd("SADD")
        .arg(format!("{p}:nr:deadnode"))
        .arg("t::r")
        .ignore()
        .cmd("HSET")
        .arg(format!("{p}:nodes"))
        .arg("deadnode")
        .arg(999)
        .ignore()
        .query_async(&mut c)
        .await
        .unwrap();
    for ws in [&mut alice, &mut bob] {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(8);
        let mut seen = 0;
        while tokio::time::Instant::now() < deadline {
            match next_event_within(ws, Duration::from_millis(500)).await {
                Event::Json(v) if v["type"] == "member_left" && v["data"]["user"] == "ghost" => {
                    seen += 1;
                }
                _ => {}
            }
        }
        assert_eq!(seen, 1, "exactly one member_left");
    }
    let (_, body) = http(
        a_node.api,
        "GET",
        "/api/v1/rooms/r/presence",
        Some(KEY),
        None,
    )
    .await;
    assert_eq!(body["users"], json!(["alice", "bob"]));
}

/// Delete every key under `prefix` (as if Redis restarted without data).
async fn wipe(url: &str, prefix: &str) {
    let client = redis::Client::open(url).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let keys: Vec<String> = redis::cmd("KEYS")
        .arg(format!("{prefix}:*"))
        .query_async(&mut c)
        .await
        .unwrap();
    if !keys.is_empty() {
        let _: i64 = redis::cmd("DEL")
            .arg(keys)
            .query_async(&mut c)
            .await
            .unwrap();
    }
}

/// R5: after Redis lost its data, each node puts its sessions and room
/// memberships back (within ~15 s): presence is whole again and the session
/// limit still holds.
#[tokio::test]
async fn nodes_restore_themselves_after_redis_lost_its_data() {
    let Some(url) = redis_url() else { return };
    let p = prefix("wipe");
    let (a_node, b_node) = (node(&url, &p).await, node(&url, &p).await);
    let mut alice = user(&a_node, "alice").await;
    join(&mut alice, "r").await;
    let mut dora = user_in(&b_node, "d", "dora", None).await;
    send_json(
        &mut dora,
        json!({"svc":"room","type":"join","room":"lobby","seq":1}),
    )
    .await;
    next_of(&mut dora, "joined").await;
    wipe(&url, &p).await;
    let (_, body) = http(
        a_node.api,
        "GET",
        "/api/v1/rooms/r/presence",
        Some(KEY),
        None,
    )
    .await;
    assert_eq!(body["users"], json!([]), "lost");
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        let (_, body) = http(
            a_node.api,
            "GET",
            "/api/v1/rooms/r/presence",
            Some(KEY),
            None,
        )
        .await;
        if body["users"] == json!(["alice"]) {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "not restored: {body}"
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    // dora's session is back in the table: single mode still denies a second.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        match try_user_in(&a_node, "d", "dora", None).await {
            Err(e) if e.contains("TOO_MANY_SESSIONS") => break,
            Ok(ws) => {
                drop(ws);
                assert!(
                    tokio::time::Instant::now() < deadline,
                    "session table not restored"
                );
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
            Err(e) => panic!("{e}"),
        }
    }
    // Nobody was disconnected by any of this.
    send_json(
        &mut alice,
        json!({"svc":"room","type":"join","room":"r2","seq":2}),
    )
    .await;
    next_of(&mut alice, "joined").await;
}

/// R5: a session the table no longer has (replaced while the command to
/// close it was lost) is closed by its node within ~15 s.
#[tokio::test]
async fn a_session_replaced_without_notice_is_closed() {
    let Some(url) = redis_url() else { return };
    let p = prefix("lost");
    let a_node = node(&url, &p).await;
    let mut sam = user_in(&a_node, "t", "sam", Some("tab")).await;
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let _: i64 = redis::cmd("DEL")
        .arg(format!("{p}:us:{{t::sam}}"))
        .query_async(&mut c)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(25);
    loop {
        match next_event_within(&mut sam, Duration::from_secs(1)).await {
            Event::Close(Some(f)) => {
                assert_eq!(f.reason, "replaced");
                break;
            }
            _ => assert!(tokio::time::Instant::now() < deadline, "not closed"),
        }
    }
}

/// R5: a node whose slot another node took stops being ready.
#[tokio::test]
async fn a_node_that_lost_its_slot_is_not_ready() {
    let Some(url) = redis_url() else { return };
    let p = prefix("steal");
    let a_node = node_with(&url, &p, 3).await;
    let _ = user(&a_node, "warm").await; // started: slot 0
    let ready = |addr| async move { http(addr, "GET", "/readyz", None, None).await.0 };
    assert_eq!(ready(a_node.addr).await, 200);
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let _: () = redis::cmd("SET")
        .arg(format!("{p}:slot:0"))
        .arg("intruder")
        .query_async(&mut c)
        .await
        .unwrap();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while ready(a_node.addr).await == 200 {
        assert!(tokio::time::Instant::now() < deadline, "still ready");
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}
