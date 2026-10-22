//! Overload protection (review #3): while Redis answers slower than
//! `gateway.broker.admission.max_latency_ms`, a node refuses new
//! connections (503 + `Retry-After`) instead of piling them up behind
//! Redis; connected sessions stay; once Redis answers in time, new
//! connections are taken again. A file of its own: it pauses the shared
//! test Redis (`WSPRISM_TEST_REDIS`), which would disturb other tests
//! running at the same time.
#![allow(clippy::unwrap_used)]
#![allow(clippy::expect_used)]
#![allow(clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::*;

async fn node(url: &str, prefix: &str) -> ApiGateway {
    let yaml = format!(
        r#"
version: 1
gateway:
  api: {{ listen: "127.0.0.1:0" }}
  broker:
    kind: redis
    url: "{url}"
    prefix: "{prefix}"
    admission: {{ max_latency_ms: 100 }}
tenants:
  - id: t
    policy: {{ ext_allowlist: ["room:*"] }}
"#
    );
    let cfg = wsprism_gateway::config::load_from_str(&yaml).unwrap();
    spawn_built_with_api(
        wsprism_gateway::Gateway::builder()
            .config(cfg)
            .build()
            .unwrap(),
    )
    .await
}

async fn try_connect(gw: &ApiGateway, user: &str) -> Result<Ws, String> {
    let bearer = format!("Bearer dev:{user}");
    try_connect_with(&ws_url(gw.addr, "t", None), &[("authorization", &bearer)])
        .await
        .map(|(ws, _)| ws)
}

#[tokio::test]
async fn new_connections_back_off_while_redis_is_slow() {
    let Ok(url) = std::env::var("WSPRISM_TEST_REDIS") else {
        eprintln!("WSPRISM_TEST_REDIS not set: skipping");
        return;
    };
    let gw = node(&url, &format!("adm{}", std::process::id())).await;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut early = loop {
        match try_connect(&gw, "early").await {
            Ok(ws) => break ws,
            Err(e) if Instant::now() < deadline => {
                let _ = e;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(e) => panic!("{e}"),
        }
    };
    assert!(matches!(next_event(&mut early).await, Event::Json(v) if v["type"] == "authed"));

    // Redis stops answering for 3 s.
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let paused = Instant::now();
    let _: () = redis::cmd("CLIENT")
        .arg("PAUSE")
        .arg(3000)
        .arg("ALL")
        .query_async(&mut c)
        .await
        .unwrap();
    // Within a probe or two (250 ms each), new connections are refused at
    // once, before any work.
    tokio::time::sleep(Duration::from_millis(700)).await;
    let t = Instant::now();
    let refused = try_connect(&gw, "storm").await;
    assert!(
        matches!(&refused, Err(e) if e.contains("503")),
        "{:?}",
        refused.as_ref().err()
    );
    assert!(t.elapsed() < Duration::from_millis(500), "refused at once");
    // The connected session is still there.
    assert!(matches!(
        next_event_within(&mut early, Duration::from_millis(100)).await,
        Event::Timeout
    ));

    // After the pause: taken again.
    let deadline = paused + Duration::from_secs(8);
    loop {
        assert!(
            Instant::now() < deadline,
            "not admitting 5 s after Redis recovered"
        );
        if let Ok(mut ws) = try_connect(&gw, "later").await {
            assert!(matches!(next_event(&mut ws).await, Event::Json(v) if v["type"] == "authed"));
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
