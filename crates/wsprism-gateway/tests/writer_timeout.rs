//! Slow-consumer protection on the write path: a client that stops reading
//! must be dropped once a write stays blocked for `writer_send_timeout_ms`,
//! even though the write is first attempted without a timer
//! (performance-architecture §3.6).

mod common;

use std::time::Duration;

use common::*;
use futures_util::StreamExt;
use serde_json::json;

const YAML: &str = r#"
version: 1
gateway:
  writer_send_timeout_ms: 200
tenants:
  - id: t
    limits:
      max_frame_bytes: 65536
    policy:
      rate_limit_rps: 100000
      rate_limit_burst: 100000
      ext_allowlist: ["room:*"]
      hot_allowlist: ["1:*"]
"#;

fn writer_timeouts(gw: &Gateway) -> u64 {
    gw.state
        .metrics()
        .render(&[])
        .lines()
        .filter(|l| l.starts_with("wsprism_writer_timeouts_total{"))
        .filter_map(|l| l.rsplit(' ').next()?.parse::<u64>().ok())
        .sum()
}

async fn join(ws: &mut Ws, room: &str) {
    send_json(ws, json!({"v":1,"svc":"room","type":"join","room":room})).await;
    match next_event(ws).await {
        Event::Json(v) if v["type"] == "joined" => {}
        other => panic!("expected sys.joined, got {other:?}"),
    }
}

#[tokio::test]
async fn stalled_reader_is_dropped_by_the_write_timeout() {
    let gw = spawn(YAML).await;
    let sessions = gw.state.realtime().sessions.clone();

    let mut stalled = connect(gw.addr, "t", Some("stalled")).await;
    join(&mut stalled, "r").await;
    let mut publisher = connect(gw.addr, "t", Some("pub")).await;
    join(&mut publisher, "r").await;
    // From here on `stalled` never reads; its socket buffers fill up.

    // The publisher drains its own echoes so only `stalled` backs up.
    let (mut pub_tx, mut pub_rx) = publisher.split();
    let drain = tokio::spawn(async move { while pub_rx.next().await.is_some() {} });

    let payload = vec![0u8; 60 * 1024];
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    while writer_timeouts(&gw) == 0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "stalled session was never dropped by the writer timeout"
        );
        for _ in 0..32 {
            use futures_util::SinkExt;
            pub_tx
                .send(tokio_tungstenite::tungstenite::Message::Binary(hot_frame(
                    1, 1, &payload,
                )))
                .await
                .expect("publish");
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // The stalled session is unregistered; the publisher stays connected.
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(sessions.len_sessions(), 1, "only the publisher remains");
    drain.abort();
    drop(stalled);
}
