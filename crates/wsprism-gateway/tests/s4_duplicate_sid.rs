//! S4: reconnecting with an in-use `sid` must replace the old connection
//! cleanly, without aliasing registry/presence state between the two.

mod common;

use std::time::Duration;

use common::*;
use serde_json::json;

fn gauge(gw: &Gateway) -> Option<i64> {
    gw.state
        .metrics()
        .render(&[])
        .lines()
        .find_map(|l| l.strip_prefix(r#"wsprism_ws_sessions_active{tenant="t"} "#))
        .map(|v| v.parse().expect("gauge value"))
}

async fn join(ws: &mut Ws, room: &str) {
    send_json(ws, json!({"v":1,"svc":"room","type":"join","room":room})).await;
    match next_event(ws).await {
        Event::Json(v) if v["type"] == "joined" => {}
        other => panic!("expected sys.joined, got {other:?}"),
    }
}

/// Give finished session tasks time to run their RAII cleanup.
async fn settle() {
    tokio::time::sleep(Duration::from_millis(300)).await;
}

#[tokio::test]
async fn duplicate_sid_replaces_old_session_without_aliasing() {
    let gw = spawn(BASE_YAML).await;
    let sessions = gw.state.realtime().sessions.clone();

    let mut old = connect(gw.addr, "t", Some("tab")).await;
    let mut new = connect(gw.addr, "t", Some("tab")).await;

    // The old connection is told it was replaced.
    match next_event(&mut old).await {
        Event::Json(v) => {
            assert_eq!(v["type"], "kicked", "{v}");
            assert_eq!(v["data"]["reason"], "replaced", "{v}");
        }
        other => panic!("expected sys.kicked(replaced), got {other:?}"),
    }

    // The new session joins a room *before* the old one finishes cleanup...
    join(&mut new, "r").await;
    drop(old);
    settle().await;

    // ...and the old cleanup must not have removed the new session's state.
    assert_eq!(sessions.len_sessions(), 1);
    assert_eq!(sessions.count_tenant_sessions("t"), 1);
    assert_eq!(gauge(&gw), Some(1));

    let mut peer = connect(gw.addr, "t", Some("peer")).await;
    join(&mut peer, "r").await;
    send_json(
        &mut peer,
        json!({"v":1,"svc":"chat","type":"send","room":"r","data":{"msg":"hi"}}),
    )
    .await;
    match next_event(&mut new).await {
        Event::Json(v) => assert_eq!(v["data"]["msg"], "hi", "{v}"),
        other => panic!("new session lost its room membership: {other:?}"),
    }

    // Everything returns to zero once all connections are gone.
    drop(new);
    drop(peer);
    settle().await;
    assert_eq!(sessions.len_sessions(), 0);
    assert_eq!(
        sessions.count_tenant_sessions("t"),
        0,
        "tenant counter leaked"
    );
    assert_eq!(gauge(&gw), Some(0));
}
