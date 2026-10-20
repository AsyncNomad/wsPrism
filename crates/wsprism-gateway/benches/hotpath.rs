//! Per-message costs on the gateway hot path: policy, metrics, fan-out.

use std::hint::black_box;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bytes::Bytes;
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tokio::sync::mpsc;

use wsprism_engine::spi::{Connection, Member, SessionSignals};
use wsprism_engine::spi::{OutFrame, RealtimeCore};
use wsprism_engine::{Outgoing, Payload, QoS};
use wsprism_gateway::__internal::GatewayMetrics;
use wsprism_gateway::__internal::TenantPolicyRuntime;
use wsprism_gateway::config;

fn policy(c: &mut Criterion) {
    let rules: Vec<String> = (0..9)
        .map(|i| format!("svc{i}:*"))
        .chain(["chat:send".to_string()])
        .collect();
    let quote = |v: Vec<String>| {
        v.iter()
            .map(|r| format!("\"{r}\""))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let hot: Vec<String> = (0..9)
        .map(|i| format!("{i}:*"))
        .chain(["9:10".into()])
        .collect();
    let yaml = format!(
        "version: 1\ntenants:\n  - id: t\n    policy:\n      ext_allowlist: [{}]\n      hot_allowlist: [{}]\n",
        quote(rules),
        quote(hot)
    );
    let parsed = config::load_from_str(&yaml).expect("bench config");
    let cfg = parsed.tenants[0].policy.clone();
    let rt = TenantPolicyRuntime::new(4096, &cfg).expect("policy");

    let mut g = c.benchmark_group("policy");
    // Worst case for the linear allowlist scan: the matching rule is last.
    g.bench_function("check_text/10_rules_last_match", |b| {
        b.iter(|| rt.check_text(black_box(120), black_box("chat"), black_box("send")))
    });
    g.bench_function("check_hot/10_rules_last_match", |b| {
        b.iter(|| rt.check_hot(black_box(64), black_box(9), black_box(10)))
    });
    g.finish();
}

fn metrics(c: &mut Criterion) {
    let m = GatewayMetrics::default();
    c.bench_function("metrics/counter_inc_4_labels", |b| {
        b.iter(|| {
            m.policy_decisions.inc(black_box(&[
                ("tenant", "acme"),
                ("lane", "ext"),
                ("decision", "drop"),
                ("reason", "policy"),
            ]))
        })
    });
    // What a session does per Ext message: observe a pre-resolved series.
    let h = m
        .dispatch_duration
        .handle(&[("tenant", "acme"), ("lane", "ext")]);
    c.bench_function("metrics/histogram_cached_observe", |b| {
        b.iter(|| h.observe(black_box(std::time::Duration::from_micros(42))))
    });
}

/// A room of `members` sessions; returns the core and the receivers to drain.
fn room(members: usize) -> (Arc<RealtimeCore>, Vec<mpsc::Receiver<OutFrame>>) {
    let core = Arc::new(RealtimeCore::new());
    let limits = wsprism_engine::RoomLimits::default();
    let mut rxs = Vec::with_capacity(members);
    for i in 0..members {
        let (tx, rx) = mpsc::channel(BATCH * 2);
        let user_key = format!("t::u{i}");
        let session_key = format!("{user_key}::s#{i}");
        let conn = Connection {
            tx,
            signals: SessionSignals::new(),
        };
        core.sessions
            .try_insert(
                "t".into(),
                user_key.clone(),
                session_key.clone(),
                "s".into(),
                conn.clone(),
                0,
            )
            .expect("insert");
        let member = Member {
            key: session_key.as_str().into(),
            conn,
            member_id: 0,
            skip_upto: 0,
            user: user_key.clone().into(),
        };
        core.presence
            .try_join("t", "t::r", &user_key, member, &limits)
            .expect("join");
        rxs.push(rx);
    }
    (core, rxs)
}

/// Publishes per timed batch; receivers are drained (untimed) between batches.
const BATCH: usize = 64;

fn fanout(c: &mut Criterion) {
    let mut g = c.benchmark_group("publish_room");
    for members in [10, 100, 1000] {
        let (core, mut rxs) = room(members);
        let out = Outgoing {
            qos: QoS::Lossy,
            payload: Payload::Utf8Bytes(Bytes::from(vec![b'x'; 128])),
        };
        g.throughput(Throughput::Elements(members as u64));
        g.bench_function(BenchmarkId::new("text_128B", members), |b| {
            b.iter_custom(|iters| {
                let mut timed = Duration::ZERO;
                let mut done = 0;
                while done < iters {
                    let n = BATCH.min((iters - done) as usize);
                    let start = Instant::now();
                    for _ in 0..n {
                        core.publish_room("t::r", out.clone()).expect("publish");
                    }
                    timed += start.elapsed();
                    for rx in &mut rxs {
                        while rx.try_recv().is_ok() {}
                    }
                    done += n as u64;
                }
                timed
            })
        });
    }
    g.finish();
}

criterion_group!(benches, policy, metrics, fanout);
criterion_main!(benches);
