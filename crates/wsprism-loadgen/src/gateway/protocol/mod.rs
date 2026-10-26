//! What differs between gateways under the same fan-out scenario.
//!
//! The run loop (`client.rs`), timing and report are shared. A [`Protocol`]
//! says how a connection gets into a room, what a probe looks like on the
//! wire, and how to answer protocol housekeeping (application-level pings).
//! Adding a gateway means adding one implementation here, nothing else.

mod centrifugo;
mod pusher;
mod wsprism;

use std::future::Future;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use tokio::net::{TcpSocket, TcpStream};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::handshake::client::Request;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

pub use centrifugo::Centrifugo;
pub use pusher::Pusher;
pub use wsprism::Wsprism;

pub type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// The node connection `conn` uses when `urls` lists several (comma-
/// separated): the `conn / rooms`-th member of each room goes to node
/// `(conn / rooms) % nodes`, so every room spans every node (`conn % nodes`
/// would put a whole room on one node whenever the counts share a factor).
pub fn url_of(urls: &str, rooms: usize, conn: usize) -> &str {
    let nodes: Vec<&str> = urls.split(',').collect();
    nodes[(conn / rooms.max(1)) % nodes.len()]
}

/// How long connect + join may take per connection.
pub const SETUP_TIMEOUT: Duration = Duration::from_secs(10);

/// A probe as the run loop sees it: who sent it and when (process epoch ns).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Probe {
    pub sender: u32,
    pub ts_ns: u64,
}

/// What one received message carried.
#[derive(Debug, PartialEq)]
pub enum Incoming {
    Probe(Probe),
    /// Must be sent back (e.g. an application-level pong).
    Reply(Message),
}

pub trait Protocol: Send + Sync + 'static {
    /// Connect connection `conn` and join `room`; resolve when probes can flow.
    fn setup(&self, conn: usize, room: &str) -> impl Future<Output = Result<Ws, String>> + Send;
    /// The probe message for `room`. `seq` increases per connection (some
    /// protocols need distinct command ids). `payload` is the target body size.
    fn encode(&self, room: &str, probe: Probe, seq: u32, payload: usize) -> Message;
    /// Everything `msg` carries (one frame may hold several protocol messages).
    fn decode(&self, msg: &Message, out: &mut Vec<Incoming>);
}

/// Source addresses connections are spread over (`--local-addrs`): one
/// source address reaches one server address and port with at most ~64k
/// connections, so tests with more need several.
static LOCAL_ADDRS: OnceLock<Vec<IpAddr>> = OnceLock::new();
static NEXT_LOCAL: AtomicUsize = AtomicUsize::new(0);

/// TLS for wss:// URLs, trusting only `--ca-file` (a private test CA).
static TLS: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();

/// Trust the CA certificates in `path` for wss:// (once, before connecting).
pub fn set_ca_file(path: &std::path::Path) -> Result<(), String> {
    use rustls::pki_types::{pem::PemObject, CertificateDer};
    let mut roots = rustls::RootCertStore::empty();
    for cert in
        CertificateDer::pem_file_iter(path).map_err(|e| format!("{}: {e}", path.display()))?
    {
        roots
            .add(cert.map_err(|e| format!("{}: {e}", path.display()))?)
            .map_err(|e| format!("{}: {e}", path.display()))?;
    }
    let config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| e.to_string())?
    .with_root_certificates(roots)
    .with_no_client_auth();
    let _ = TLS.set(Arc::new(config));
    Ok(())
}

fn connector() -> Option<Connector> {
    TLS.get().map(|c| Connector::Rustls(Arc::clone(c)))
}

/// Set the source addresses (once, before connecting).
pub fn set_local_addrs(addrs: Vec<IpAddr>) {
    let _ = LOCAL_ADDRS.set(addrs);
}

/// Open a WebSocket with TCP_NODELAY, like browsers, so client-side Nagle
/// delays don't pollute the measured latency.
pub(in crate::gateway) async fn open(req: Request) -> Result<Ws, String> {
    let connect = async {
        let Some(addrs) = LOCAL_ADDRS.get().filter(|a| !a.is_empty()) else {
            let (ws, _) =
                tokio_tungstenite::connect_async_tls_with_config(req, None, true, connector())
                    .await
                    .map_err(|e| format!("connect: {e}"))?;
            return Ok(ws);
        };
        // ws:// or wss:// to an IP address only: `--local-addrs` is for
        // the large idle-connection tests on netlab.
        let local = addrs[NEXT_LOCAL.fetch_add(1, Ordering::Relaxed) % addrs.len()];
        let uri = req.uri();
        let peer: SocketAddr = format!(
            "{}:{}",
            uri.host().unwrap_or_default(),
            uri.port_u16()
                .unwrap_or(if uri.scheme_str() == Some("wss") {
                    443
                } else {
                    80
                })
        )
        .parse()
        .map_err(|e| format!("--local-addrs needs ws://<ip>:<port>: {e}"))?;
        let sock = if local.is_ipv4() {
            TcpSocket::new_v4()
        } else {
            TcpSocket::new_v6()
        }
        .map_err(|e| format!("socket: {e}"))?;
        sock.bind(SocketAddr::new(local, 0))
            .map_err(|e| format!("bind {local}: {e}"))?;
        let tcp = sock
            .connect(peer)
            .await
            .map_err(|e| format!("connect: {e}"))?;
        tcp.set_nodelay(true).map_err(|e| format!("nodelay: {e}"))?;
        let (ws, _) = tokio_tungstenite::client_async_tls_with_config(req, tcp, None, connector())
            .await
            .map_err(|e| format!("handshake: {e}"))?;
        Ok(ws)
    };
    tokio::time::timeout(SETUP_TIMEOUT, connect)
        .await
        .map_err(|_| "connect timeout".to_string())?
}

/// Read text messages until `pick` returns `Some(Ok)` / `Some(Err)`, within
/// the setup deadline. Each text frame is passed whole.
pub(in crate::gateway) async fn await_text<T>(
    ws: &mut Ws,
    what: &str,
    mut pick: impl FnMut(&str) -> Option<Result<T, String>>,
) -> Result<T, String> {
    let deadline = Instant::now() + SETUP_TIMEOUT;
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next())
            .await
            .map_err(|_| format!("timeout waiting for {what}"))?
            .ok_or_else(|| format!("closed before {what}"))?
            .map_err(|e| format!("read: {e}"))?;
        if let Message::Text(t) = &msg {
            if let Some(r) = pick(t) {
                return r;
            }
        }
    }
}

/// JSON probe body `{"s":sender,"t":ts,"p":"xx…"}` padded so the serialized
/// body is about `payload` bytes. Shared by the JSON protocols.
fn json_body(probe: Probe, payload: usize) -> serde_json::Value {
    const OVERHEAD: usize = 40; // keys, quotes, digits of a typical probe
    serde_json::json!({
        "s": probe.sender,
        "t": probe.ts_ns,
        "p": "x".repeat(payload.saturating_sub(OVERHEAD)),
    })
}

fn probe_from_json(v: &serde_json::Value) -> Option<Probe> {
    Some(Probe {
        sender: u32::try_from(v["s"].as_u64()?).ok()?,
        ts_ns: v["t"].as_u64()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_body_round_trips_and_has_the_requested_size() {
        let p = Probe {
            sender: 7,
            ts_ns: 123_456_789_012,
        };
        let v = json_body(p, 64);
        assert_eq!(probe_from_json(&v), Some(p));
        let n = v.to_string().len();
        assert!((50..=80).contains(&n), "body is {n} bytes");
    }
}
