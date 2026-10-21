//! WebSocket (plain or TLS) transport for the relay.

use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};
use tokio_tungstenite::tungstenite::Message;

use crate::hub::{parse_query, Hub};

pub async fn serve(
    addr: SocketAddr,
    hub: Arc<Hub>,
    tls: Option<TlsAcceptor>,
) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr).await?;
    loop {
        let (tcp, _) = listener.accept().await?;
        tcp.set_nodelay(true)?;
        let (hub, tls) = (hub.clone(), tls.clone());
        tokio::spawn(async move {
            match tls {
                Some(acceptor) => {
                    if let Ok(stream) = acceptor.accept(tcp).await {
                        session(stream, hub).await;
                    }
                }
                None => session(tcp, hub).await,
            }
        });
    }
}

// tungstenite fixes the callback error type; nothing to shrink here.
#[allow(clippy::result_large_err)]
async fn session<S: AsyncRead + AsyncWrite + Unpin>(stream: S, hub: Arc<Hub>) {
    let query = Arc::new(Mutex::new(None));
    let q = query.clone();
    let callback = move |req: &Request, resp: Response| {
        *q.lock().unwrap() = parse_query(&req.uri().to_string());
        Ok(resp)
    };
    let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
        return;
    };
    let Some((room, id)) = *query.lock().unwrap() else {
        return;
    };
    let membership = hub.join(room, id);
    let (mut tx, mut rx) = ws.split();

    let reader = async {
        while let Some(Ok(msg)) = rx.next().await {
            if let Message::Binary(b) = msg {
                hub.publish(room, id, Bytes::from(b));
            }
        }
    };
    let writer = async {
        let mut tick = hub.ticker();
        loop {
            for batch in hub.next_batches(&membership.slot, &mut tick).await {
                if tx.send(Message::Binary(batch.to_vec())).await.is_err() {
                    return;
                }
                hub.stats.batches_out.fetch_add(1, Ordering::Relaxed);
            }
        }
    };
    tokio::select! {
        _ = reader => {},
        _ = writer => {},
    }
}
