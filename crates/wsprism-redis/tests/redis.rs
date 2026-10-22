//! The Redis broker against a real Redis (`WSPRISM_TEST_REDIS`, see
//! `bench/redis-test.sh`); without it these tests do nothing.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::sync::Arc;

use wsprism_engine::broker::Broker;
use wsprism_redis::{RedisBroker, RedisConfig};

fn redis_url() -> Option<String> {
    let url = std::env::var("WSPRISM_TEST_REDIS").ok();
    if url.is_none() {
        eprintln!("WSPRISM_TEST_REDIS not set: skipping");
    }
    url
}

fn prefix(name: &str) -> String {
    let n = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    format!("rt{}-{name}", n % 1_000_000_000)
}

async fn broker(url: &str, prefix: &str) -> Arc<RedisBroker> {
    let b = Arc::new(RedisBroker::new(RedisConfig::new(url).prefix(prefix)).unwrap());
    b.start().await.unwrap();
    b
}

/// A10: however many reapers race, each dead member is reported once.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn racing_reapers_report_each_dead_member_once() {
    let Some(url) = redis_url() else { return };
    let p = prefix("reap");
    let (a, b) = (broker(&url, &p).await, broker(&url, &p).await);
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let mut pipe = redis::pipe();
    for i in 0..20 {
        pipe.cmd("HSET")
            .arg(format!("{p}:pr:{{t::r{}}}", i % 4))
            .arg(format!("t::g{i}::s#1"))
            .arg(format!("deadnode|999|g{i}|{i}|1"))
            .ignore();
    }
    for r in 0..4 {
        pipe.cmd("SADD")
            .arg(format!("{p}:nr:deadnode"))
            .arg(format!("t::r{r}"))
            .ignore();
    }
    pipe.cmd("HSET")
        .arg(format!("{p}:nodes"))
        .arg("deadnode")
        .arg(999)
        .ignore();
    let _: () = pipe.query_async(&mut c).await.unwrap();

    let calls: Vec<_> = (0..16)
        .map(|i| {
            let x = if i % 2 == 0 {
                Arc::clone(&a)
            } else {
                Arc::clone(&b)
            };
            tokio::spawn(async move { x.presence().unwrap().reap_dead().await.unwrap() })
        })
        .collect();
    let mut users = Vec::new();
    for c in calls {
        users.extend(c.await.unwrap().into_iter().map(|d| d.user.to_string()));
    }
    users.sort();
    let mut want: Vec<String> = (0..20).map(|i| format!("g{i}")).collect();
    want.sort();
    assert_eq!(users, want, "each dead member exactly once");
    assert!(a
        .presence()
        .unwrap()
        .members("t::r0")
        .await
        .unwrap()
        .is_empty());
}

/// R5: a batch retried after its reply was lost (the first try did run) is
/// applied once: one copy published, one offset each.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_retried_kept_batch_is_applied_once() {
    use futures_util::StreamExt;
    use std::time::Duration;
    use wsprism_engine::broker::{BrokerFrame, Keep};
    use wsprism_engine::spi::OutFrame;
    use wsprism_engine::QoS;

    let Some(url) = redis_url() else { return };
    let p = prefix("retry");
    let b = broker(&url, &p).await;
    let client = redis::Client::open(url.as_str()).unwrap();
    let mut sub = client.get_async_pubsub().await.unwrap();
    sub.subscribe(format!("{p}:r:{{t::h}}")).await.unwrap();
    let mut stream = sub.into_on_message();

    let keep = Keep::new(100, Duration::from_secs(60), 1 << 20);
    let batch: Vec<BrokerFrame> = (1..=3)
        .map(|i| {
            let mut f = BrokerFrame::new(
                0,
                "t::h",
                QoS::Reliable,
                OutFrame::text(format!("{{\"n\":{i}}}")),
            );
            f.keep = Some(keep);
            f.seq = i;
            f
        })
        .collect();
    b.publish(&batch).await.unwrap();
    b.publish(&batch).await.unwrap(); // the retry
    let mut got = Vec::new();
    while let Ok(Some(m)) = tokio::time::timeout(Duration::from_millis(300), stream.next()).await {
        let f = wsprism_engine::broker::wire::decode(&bytes::Bytes::copy_from_slice(
            m.get_payload_bytes(),
        ))
        .unwrap();
        got.push(f.offset.unwrap());
    }
    assert_eq!(got, [1, 2, 3], "published once, one offset each");
}

/// Fencing (0.2): once another node holds this node's slot, Redis refuses
/// its publishes - plain and kept - and the broker reports itself unhealthy
/// at once (the server then drains), without waiting for the heartbeat.
#[tokio::test]
async fn a_node_whose_slot_was_taken_cannot_publish() {
    use std::time::Duration;
    use wsprism_engine::broker::{BrokerError, BrokerFrame, Keep};
    use wsprism_engine::spi::OutFrame;
    use wsprism_engine::QoS;

    let Some(url) = redis_url() else { return };
    let p = prefix("fence");
    let b = broker(&url, &p).await;
    let frame = |kept: bool| {
        let mut f = BrokerFrame::new(0, "t::f", QoS::Reliable, OutFrame::text("{}".to_string()));
        if kept {
            f.keep = Some(Keep::new(10, Duration::from_secs(60), 1 << 20));
            f.seq = 1;
        }
        f
    };
    b.publish(&[frame(false)]).await.unwrap();
    assert!(b.healthy());

    let client = redis::Client::open(url.as_str()).unwrap();
    let mut c = client.get_multiplexed_async_connection().await.unwrap();
    let _: () = redis::cmd("SET")
        .arg(format!("{p}:slot:0"))
        .arg("intruder")
        .query_async(&mut c)
        .await
        .unwrap();
    for kept in [false, true] {
        match b.publish(&[frame(kept)]).await {
            Err(BrokerError::Rejected(m)) => assert!(m.contains("fenced"), "{m}"),
            other => panic!("kept={kept}: {other:?}"),
        }
    }
    assert!(!b.healthy(), "unhealthy at once");
    let kept: Option<String> = redis::cmd("HGET")
        .arg(format!("{p}:h:{{t::f}}:st"))
        .arg("seq")
        .query_async(&mut c)
        .await
        .unwrap();
    assert_eq!(kept, None, "the kept frame was not stored");
}
