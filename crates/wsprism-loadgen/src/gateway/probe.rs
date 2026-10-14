//! Probe timestamps: nanoseconds since a process-wide epoch, so a receiver
//! computes end-to-end latency without clock sync (publishers and receivers
//! live in the same process). Wire encodings live in `protocol/`.

use std::sync::OnceLock;
use std::time::Instant;

fn epoch() -> Instant {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    *EPOCH.get_or_init(Instant::now)
}

pub fn now_nanos() -> u64 {
    epoch().elapsed().as_nanos() as u64
}
