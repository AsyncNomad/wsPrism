//! Aggregate per-connection stats and print them.

use std::time::Duration;

use hdrhistogram::Histogram;

use super::client::ConnStats;
use super::scenario::Args;

pub struct Report {
    pub conns: usize,
    pub sent: u64,
    pub expected: u64,
    pub received: u64,
    pub closed_early: usize,
    pub duration: Duration,
    pub latency_us: Histogram<u64>,
}

impl Report {
    pub fn aggregate(args: &Args, stats: Vec<ConnStats>, room_sizes: &[u64]) -> Self {
        let mut latency_us =
            Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).expect("histogram bounds");
        let mut sent = 0;
        let mut expected = 0;
        let mut received = 0;
        let mut closed_early = 0;
        for (conn, s) in stats.iter().enumerate() {
            sent += s.sent;
            // Each probe is delivered to every member of the publisher's room.
            expected += s.sent * room_sizes[conn % room_sizes.len()];
            received += s.received;
            closed_early += usize::from(s.closed_early);
            let _ = latency_us.add(&s.latency_us);
        }
        Self {
            conns: stats.len(),
            sent,
            expected,
            received,
            closed_early,
            duration: args.duration(),
            latency_us,
        }
    }

    fn delivery_ratio(&self) -> f64 {
        if self.expected == 0 {
            return 1.0;
        }
        self.received as f64 / self.expected as f64
    }

    fn q(&self, quantile: f64) -> u64 {
        self.latency_us.value_at_quantile(quantile)
    }

    pub fn print(&self, args: &Args) {
        let secs = self.duration.as_secs_f64();
        println!(
            "lane={:?} conns={} rooms={} publishers={} rate={}/s payload={}B duration={}s",
            args.lane,
            self.conns,
            args.rooms,
            args.publishers,
            args.rate,
            args.payload,
            args.duration
        );
        println!(
            "sent={} ({:.0}/s)  delivered={} ({:.0}/s)  delivery={:.2}%  closed_early={}",
            self.sent,
            self.sent as f64 / secs,
            self.received,
            self.received as f64 / secs,
            self.delivery_ratio() * 100.0,
            self.closed_early
        );
        println!(
            "latency us: p50={} p90={} p99={} p99.9={} max={}",
            self.q(0.5),
            self.q(0.9),
            self.q(0.99),
            self.q(0.999),
            self.latency_us.max()
        );
    }

    pub fn print_json(&self, args: &Args) {
        let secs = self.duration.as_secs_f64();
        let v = serde_json::json!({
            "lane": format!("{:?}", args.lane).to_lowercase(),
            "conns": self.conns, "rooms": args.rooms, "publishers": args.publishers,
            "rate": args.rate, "payload": args.payload, "duration_s": args.duration,
            "sent": self.sent, "delivered": self.received, "expected": self.expected,
            "delivered_per_s": (self.received as f64 / secs).round(),
            "delivery_ratio": self.delivery_ratio(),
            "closed_early": self.closed_early,
            "latency_us": {
                "p50": self.q(0.5), "p90": self.q(0.9), "p99": self.q(0.99),
                "p999": self.q(0.999), "max": self.latency_us.max()
            }
        });
        println!("{v}");
    }
}
