//! One client link over WebSocket, WebSocket+TLS or WebTransport datagrams,
//! exposing the same frame-out / batch-in interface.
//!
//! TLS certificates are NOT verified: this is a benchmark client for a
//! self-signed test relay.

use std::sync::Arc;

use bytes::Bytes;
use clap::ValueEnum;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Transport {
    Ws,
    Wss,
    Wt,
    /// WebTransport first; if it is not up within the attempt budget, fall
    /// back to WSS (design §5, gate G4).
    Auto,
}

pub trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type Ws = WebSocketStream<Box<dyn Io>>;

pub enum Tx {
    Ws(SplitSink<Ws, Message>),
    Wt(Arc<wtransport::Connection>),
}

pub enum Rx {
    Ws(SplitStream<Ws>),
    Wt(Arc<wtransport::Connection>),
}

impl Tx {
    /// Send one state frame (WT: one datagram; never blocks on loss).
    pub async fn send(&mut self, frame: Bytes) -> Result<(), String> {
        match self {
            Tx::Ws(s) => s
                .send(Message::Binary(frame.to_vec()))
                .await
                .map_err(|e| e.to_string()),
            Tx::Wt(c) => c.send_datagram(frame).map_err(|e| e.to_string()),
        }
    }
}

impl Rx {
    /// Next batch from the relay, or None when the link is gone.
    pub async fn recv(&mut self) -> Option<Bytes> {
        match self {
            Rx::Ws(s) => loop {
                match s.next().await? {
                    Ok(Message::Binary(b)) => return Some(Bytes::from(b)),
                    Ok(Message::Close(_)) | Err(_) => return None,
                    Ok(_) => continue,
                }
            },
            Rx::Wt(c) => c.receive_datagram().await.ok().map(|d| d.payload()),
        }
    }
}

/// Connect with fallback: `Auto` tries WT on `addr` for `wt_budget`, then
/// WSS on `fallback_addr`. Returns the transport actually used.
pub async fn connect_with_fallback(
    transport: Transport,
    addr: &str,
    fallback_addr: Option<&str>,
    wt_budget: std::time::Duration,
    room: u32,
    id: u32,
) -> Result<(Tx, Rx, Transport), String> {
    if transport != Transport::Auto {
        let (tx, rx) = connect(transport, addr, room, id).await?;
        return Ok((tx, rx, transport));
    }
    match tokio::time::timeout(wt_budget, connect(Transport::Wt, addr, room, id)).await {
        Ok(Ok((tx, rx))) => Ok((tx, rx, Transport::Wt)),
        Ok(Err(_)) | Err(_) => {
            let fb = fallback_addr.ok_or("auto needs --fallback-addr")?;
            let (tx, rx) = connect(Transport::Wss, fb, room, id).await?;
            Ok((tx, rx, Transport::Wss))
        }
    }
}

/// Connect to `addr` (host:port) for `room`/`id`.
pub async fn connect(
    transport: Transport,
    addr: &str,
    room: u32,
    id: u32,
) -> Result<(Tx, Rx), String> {
    let query = format!("/?room={room}&id={id}");
    match transport {
        Transport::Auto => Err("use connect_with_fallback for auto".into()),
        Transport::Ws | Transport::Wss => {
            let tcp = TcpStream::connect(addr).await.map_err(|e| e.to_string())?;
            tcp.set_nodelay(true).map_err(|e| e.to_string())?;
            let (io, scheme): (Box<dyn Io>, _) = if transport == Transport::Wss {
                let tls = tls_connector()
                    .connect("localhost".try_into().map_err(|_| "server name")?, tcp)
                    .await
                    .map_err(|e| e.to_string())?;
                (Box::new(tls), "wss")
            } else {
                (Box::new(tcp), "ws")
            };
            let url = format!("{scheme}://{addr}{query}");
            let (ws, _) = tokio_tungstenite::client_async(url, io)
                .await
                .map_err(|e| e.to_string())?;
            let (tx, rx) = ws.split();
            Ok((Tx::Ws(tx), Rx::Ws(rx)))
        }
        Transport::Wt => {
            let config = wtransport::ClientConfig::builder()
                .with_bind_default()
                .with_no_cert_validation()
                .build();
            let endpoint = wtransport::Endpoint::client(config).map_err(|e| e.to_string())?;
            let conn = endpoint
                .connect(format!("https://{addr}{query}"))
                .await
                .map_err(|e| e.to_string())?;
            let conn = Arc::new(conn);
            Ok((Tx::Wt(conn.clone()), Rx::Wt(conn)))
        }
    }
}

fn tls_connector() -> tokio_rustls::TlsConnector {
    use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use rustls::{DigitallySignedStruct, SignatureScheme};

    #[derive(Debug)]
    struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);
    impl ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _: &CertificateDer<'_>,
            _: &[CertificateDer<'_>],
            _: &ServerName<'_>,
            _: &[u8],
            _: UnixTime,
        ) -> Result<ServerCertVerified, rustls::Error> {
            Ok(ServerCertVerified::assertion())
        }
        fn verify_tls12_signature(
            &self,
            m: &[u8],
            c: &CertificateDer<'_>,
            d: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls12_signature(
                m,
                c,
                d,
                &self.0.signature_verification_algorithms,
            )
        }
        fn verify_tls13_signature(
            &self,
            m: &[u8],
            c: &CertificateDer<'_>,
            d: &DigitallySignedStruct,
        ) -> Result<HandshakeSignatureValid, rustls::Error> {
            rustls::crypto::verify_tls13_signature(
                m,
                c,
                d,
                &self.0.signature_verification_algorithms,
            )
        }
        fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
            self.0.signature_verification_algorithms.supported_schemes()
        }
    }

    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
        .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(config))
}
