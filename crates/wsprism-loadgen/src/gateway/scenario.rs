//! What to run: CLI arguments and the derived scenario.

use std::time::Duration;

use clap::ValueEnum;

use crate::schedule::Phase;

/// Which gateway (wire protocol) to drive. See `protocol/`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Target {
    Wsprism,
    Centrifugo,
    /// Pusher protocol 7 (Sockudo).
    Pusher,
}

/// Which protocol lane the publishers use (wsPrism only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Lane {
    /// Binary Hot Lane frames to the echo service (lossy room fan-out).
    Hot,
    /// JSON `chat:send` on the Ext Lane (reliable room fan-out).
    Ext,
    /// Protocol v2 keyed Hot frames (`wsprism.v2.json`): each publisher sends
    /// its state under one key, so lagging readers are conflated (latest
    /// value wins) and `hot_tick_hz` pacing applies. wsPrism only.
    Hot2,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    #[arg(long, value_enum, default_value_t = Target::Wsprism)]
    pub target: Target,
    /// Gateway base URL (the protocol adds its own path). wsPrism: several,
    /// comma-separated, for a cluster: each room's members are spread over
    /// them (see `Wsprism::url_of`).
    #[arg(long, default_value = "ws://127.0.0.1:8080")]
    pub url: String,
    #[arg(long, default_value = "bench")]
    pub tenant: String,
    #[arg(long, default_value = "dev")]
    pub ticket: String,
    /// wsPrism: log every connection in as its own user (`<ticket>:u<conn>`,
    /// i.e. `dev:u17` with the dev ticket) instead of all as one user. Real
    /// deployments have one user per connection; per-user state shows up in
    /// memory only this way.
    #[arg(long)]
    pub distinct_users: bool,
    /// Source IP addresses to spread connections over, comma-separated
    /// (plain ws:// to an IP only). One source address reaches one server
    /// address with at most ~64k connections.
    #[arg(long, value_delimiter = ',')]
    pub local_addrs: Vec<std::net::IpAddr>,
    /// Total connections (all of them join a room and receive).
    #[arg(long, default_value_t = 1000)]
    pub conns: usize,
    /// Number of rooms; connections are spread round-robin.
    #[arg(long, default_value_t = 10)]
    pub rooms: usize,
    /// How many connections also publish (spread across rooms). 0 = idle test.
    #[arg(long, default_value_t = 10)]
    pub publishers: usize,
    /// Messages per second per publisher.
    #[arg(long, default_value_t = 100)]
    pub rate: u64,
    /// Measurement duration in seconds.
    #[arg(long, default_value_t = 10)]
    pub duration: u64,
    /// Payload size in bytes (includes the 8-byte timestamp).
    #[arg(long, default_value_t = 64)]
    pub payload: usize,
    #[arg(long, value_enum, default_value_t = Lane::Hot)]
    pub lane: Lane,
    /// Pusher app key / secret (`--target pusher`).
    #[arg(long, default_value = "app-key")]
    pub app_key: String,
    #[arg(long, default_value = "app-secret")]
    pub app_secret: String,
    /// Publisher start phases (see schedule.rs).
    #[arg(long, value_enum, default_value_t = Phase::Random)]
    pub phase: Phase,
    #[arg(long, default_value_t = 1)]
    pub seed: u64,
    /// Server process(es) to sample CPU/RSS from (comma-separated: summed,
    /// e.g. every node of a cluster).
    #[arg(long, value_delimiter = ',')]
    pub server_pid: Vec<u32>,
    /// Free-form label copied into the JSON output.
    #[arg(long, default_value = "")]
    pub label: String,
    /// Concurrent handshakes while ramping up.
    #[arg(long, default_value_t = 200)]
    pub connect_concurrency: usize,
    /// Print a single JSON line instead of the table.
    #[arg(long)]
    pub json: bool,
}

impl Args {
    pub fn duration(&self) -> Duration {
        Duration::from_secs(self.duration)
    }

    pub fn room_of(&self, conn: usize) -> String {
        format!("bench-{}", conn % self.rooms.max(1))
    }

    /// Publishers are the first `publishers` connections, so they cover rooms
    /// round-robin like everyone else.
    pub fn is_publisher(&self, conn: usize) -> bool {
        conn < self.publishers
    }
}
