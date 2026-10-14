//! WebTransport (QUIC datagram) transport for the relay.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use wtransport::endpoint::IncomingSession;
use wtransport::{Endpoint, Identity, ServerConfig};

use crate::hub::{parse_query, Hub};

pub async fn serve(addr: SocketAddr, identity: Identity, hub: Arc<Hub>) -> std::io::Result<()> {
    let config = ServerConfig::builder()
        .with_bind_address(addr)
        .with_identity(identity)
        .keep_alive_interval(Some(Duration::from_secs(3)))
        .max_idle_timeout(Some(Duration::from_secs(30)))
        .expect("idle timeout")
        .build();
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
