//! Aggregate per-connection stats and print them.

use std::time::Duration;

use hdrhistogram::Histogram;

use super::client::ConnStats;
use super::scenario::Args;
use crate::cost::ProcSample;

/// Server cost samples (all `None` without `--server-pid`).
pub struct Cost {
    pub rss_before_kb: Option<u64>,
    pub start: Option<ProcSample>,
    pub end: Option<ProcSample>,
}

pub struct Report {
    pub conns: usize,
    pub sent: u64,
    /// Deliveries to the *other* members of each publisher's room.
    pub expected: u64,
    pub received: u64,
    /// Probes delivered back to their own publisher (Centrifugo and the
    /// wsPrism room services do this, Pusher does not). Counted as server
    /// work in the CPU figure, not in `delivery_ratio`.
    pub self_echo: u64,
    pub closed_early: usize,
    pub duration: Duration,
    pub latency_us: Histogram<u64>,
    pub cpu_us_per_delivery: Option<f64>,
    pub rss_kb_per_conn: Option<f64>,
}

impl Report {
    pub fn aggregate(args: &Args, stats: Vec<ConnStats>, room_sizes: &[u64], cost: Cost) -> Self {
        let mut latency_us =
            Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).expect("histogram bounds");
        let (mut sent, mut expected, mut received, mut self_echo, mut closed_early) =
            (0, 0, 0, 0, 0);
        for (conn, s) in stats.iter().enumerate() {
            sent += s.sent;
            expected += s.sent * room_sizes[conn % room_sizes.len()].saturating_sub(1);
            received += s.received;
            self_echo += s.self_echo;
            closed_early += usize::from(s.closed_early);
            let _ = latency_us.add(&s.latency_us);
        }
        let deliveries = received + self_echo;
        let cpu_us_per_delivery = match (cost.start, cost.end) {
            (Some(a), Some(b)) if deliveries > 0 => {
                Some((b.cpu_secs - a.cpu_secs) * 1e6 / deliveries as f64)
            }
            _ => None,
        };
        let rss_kb_per_conn = match (cost.rss_before_kb, cost.end) {
            (Some(before), Some(after)) => {
                Some(after.rss_kb.saturating_sub(before) as f64 / stats.len().max(1) as f64)
            }
            _ => None,
        };
        Self {
            conns: stats.len(),
            sent,
            expected,
            received,
            self_echo,
            closed_early,
            duration: args.duration(),
            latency_us,
            cpu_us_per_delivery,
            rss_kb_per_conn,
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
            "target={:?} lane={:?} conns={} rooms={} publishers={} rate={}/s payload={}B duration={}s",
            args.target,
            args.lane,
            self.conns,
            args.rooms,
            args.publishers,
            args.rate,
            args.payload,
            args.duration
        );
        println!(
            "sent={} ({:.0}/s)  delivered={} ({:.0}/s)  delivery={:.2}%  self_echo={}  closed_early={}",
            self.sent,
            self.sent as f64 / secs,
            self.received,
            self.received as f64 / secs,
            self.delivery_ratio() * 100.0,
            self.self_echo,
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
        if let (Some(cpu), Some(rss)) = (self.cpu_us_per_delivery, self.rss_kb_per_conn) {
            println!("server: {cpu:.2} us CPU/delivery, {rss:.1} KB RSS/conn");
        }
    }

    pub fn print_json(&self, args: &Args) {
        let secs = self.duration.as_secs_f64();
        let v = serde_json::json!({
            "label": args.label,
            "target": format!("{:?}", args.target).to_lowercase(),
            "lane": format!("{:?}", args.lane).to_lowercase(),
            "conns": self.conns, "rooms": args.rooms, "publishers": args.publishers,
            "rate": args.rate, "payload": args.payload, "duration_s": args.duration,
            "phase": format!("{:?}", args.phase).to_lowercase(), "seed": args.seed,
            "sent": self.sent, "delivered": self.received, "expected": self.expected,
            "self_echo": self.self_echo,
            "delivered_per_s": (self.received as f64 / secs).round(),
            "delivery_ratio": self.delivery_ratio(),
            "closed_early": self.closed_early,
            "latency_us": {
                "p50": self.q(0.5), "p90": self.q(0.9), "p99": self.q(0.99),
                "p999": self.q(0.999), "max": self.latency_us.max()
            },
            "server_cpu_us_per_delivery": self.cpu_us_per_delivery,
            "server_rss_kb_per_conn": self.rss_kb_per_conn,
        });
        println!("{v}");
    }
}
