//! WebTransport (QUIC datagram) transport for the relay.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use wtransport::config::QuicTransportConfig;
use wtransport::endpoint::IncomingSession;
use wtransport::quinn::VarInt;
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::hub::{parse_query, Hub};

/// Per-connection QUIC settings sized for small, frequent state frames
/// (memory experiment, BENCHMARKS.md "검증 V"). quinn defaults are sized for
/// bulk streams: 1 MB / 1.25 MB datagram buffers, 1.25 MB stream window,
/// 100 + 100 concurrent streams, MTU discovery.
fn lean_transport() -> QuicTransportConfig {
    let mut t = QuicTransportConfig::default();
    t.datagram_send_buffer_size(64 * 1024)
        .datagram_receive_buffer_size(Some(64 * 1024))
        .stream_receive_window(VarInt::from_u32(64 * 1024))
        .receive_window(VarInt::from_u32(256 * 1024))
        .send_window(256 * 1024)
        // WebTransport needs the CONNECT bidi stream and the HTTP/3 control /
        // QPACK uni streams (3 per side); leave headroom.
        .max_concurrent_bidi_streams(VarInt::from_u32(4))
        .max_concurrent_uni_streams(VarInt::from_u32(8))
        .mtu_discovery_config(None)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .max_idle_timeout(Some(
            Duration::from_secs(30).try_into().expect("idle timeout"),
        ));
    t
}

pub async fn serve(
    addr: SocketAddr,
    identity: Identity,
    hub: Arc<Hub>,
    lean: bool,
) -> std::io::Result<()> {
    let config = if lean {
        ServerConfig::builder()
            .with_bind_address(addr)
            .with_custom_transport(identity, lean_transport())
            .build()
    } else {
        ServerConfig::builder()
            .with_bind_address(addr)
            .with_identity(identity)
            .keep_alive_interval(Some(Duration::from_secs(3)))
            .max_idle_timeout(Some(Duration::from_secs(30)))
            .expect("idle timeout")
            .build()
    };
    let endpoint = Endpoint::server(config)?;
    loop {
        let incoming = endpoint.accept().await;
        tokio::spawn(session(incoming, hub.clone()));
    }
}

async fn session(incoming: IncomingSession, hub: Arc<Hub>) {
    let Ok(request) = incoming.await else {
        return;
    };
    let Some((room, id)) = parse_query(request.path()) else {
        request.not_found().await;
        return;
    };
    let Ok(conn) = request.accept().await else {
        return;
    };
    let membership = hub.join(room, id);

    let reader = async {
        while let Ok(dgram) = conn.receive_datagram().await {
            hub.publish(room, id, dgram.payload());
        }
    };
    let writer = async {
        let mut tick = hub.ticker();
        loop {
            for batch in hub.next_batches(&membership.slot, &mut tick).await {
                // Never blocks: a full send buffer or closed session drops the batch.
                match conn.send_datagram(batch) {
                    Ok(()) => hub.stats.batches_out.fetch_add(1, Ordering::Relaxed),
                    Err(_) => hub.stats.send_dropped.fetch_add(1, Ordering::Relaxed),
                };
            }
        }
    };
    tokio::select! {
        _ = reader => {},
        _ = writer => {},
        _ = conn.closed() => {},
    }
}
