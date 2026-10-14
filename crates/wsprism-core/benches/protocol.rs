//! Protocol decode cost per inbound message (both lanes).

use std::hint::black_box;

use bytes::{BufMut, Bytes, BytesMut};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use wsprism_core::protocol::{hot::decode_hot_frame, text::decode_envelope};

fn hot_frame(payload_len: usize, with_seq: bool) -> Bytes {
    let mut b = BytesMut::new();
    b.put_u8(1); // v
    b.put_u8(1); // svc_id
    b.put_u8(1); // opcode
    b.put_u8(u8::from(with_seq)); // flags
    if with_seq {
        b.put_u32_le(42);
    }
    b.put_bytes(7, payload_len);
    b.freeze()
}

fn envelope(data_len: usize) -> String {
    let msg = "x".repeat(data_len);
    format!(
        r#"{{"v":1,"svc":"chat","type":"send","seq":7,"room":"match:1","data":{{"msg":"{msg}"}}}}"#
    )
}

fn hot(c: &mut Criterion) {
    let mut g = c.benchmark_group("decode_hot_frame");
    for (len, seq) in [(16, false), (256, true), (1024, true)] {
        let frame = hot_frame(len, seq);
        g.throughput(Throughput::Bytes(frame.len() as u64));
        g.bench_with_input(BenchmarkId::from_parameter(frame.len()), &frame, |b, f| {
            // Bytes clone is a refcount bump, mirroring the gateway's zero-copy path.
            b.iter(|| decode_hot_frame(black_box(f.clone())))
        });
    }
    g.finish();
}

fn ext(c: &mut Criterion) {
    let mut g = c.benchmark_group("decode_envelope");
    for data_len in [16, 1024] {
        let text = envelope(data_len);
        g.throughput(Throughput::Bytes(text.len() as u64));
        g.bench_with_input(BenchmarkId::from_parameter(text.len()), &text, |b, t| {
            b.iter(|| decode_envelope(black_box(t)))
        });
    }
    g.finish();
}

criterion_group!(benches, hot, ext);
criterion_main!(benches);
