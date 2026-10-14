//! What to run: CLI arguments and the derived scenario.

use std::time::Duration;

use clap::ValueEnum;

/// Which protocol lane the publishers use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Lane {
    /// Binary Hot Lane frames to the echo service (lossy room fan-out).
    Hot,
    /// JSON `chat:send` on the Ext Lane (reliable room fan-out).
    Ext,
}

#[derive(Debug, clap::Args)]
pub struct Args {
    /// Gateway base URL.
    #[arg(long, default_value = "ws://127.0.0.1:8080")]
    pub url: String,
    #[arg(long, default_value = "bench")]
    pub tenant: String,
    #[arg(long, default_value = "dev")]
    pub ticket: String,
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

    pub fn ws_url(&self, conn: usize) -> String {
        format!(
            "{}/v1/ws?tenant={}&sid=c{conn}",
            self.url.trim_end_matches('/'),
            self.tenant
        )
    }
}
