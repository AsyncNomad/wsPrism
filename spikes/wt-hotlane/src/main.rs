//! THROWAWAY validation relay (ADR 0004 / design §7): the same Hot-slot relay
//! behind plain WebSocket, WebSocket over TLS, and WebTransport datagrams.

mod hub;
mod ws;
mod wt;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio_rustls::TlsAcceptor;
use wtransport::Identity;

#[derive(Parser)]
struct Args {
    /// Plain WebSocket listen address.
    #[arg(long)]
    ws: Option<SocketAddr>,
    /// WebSocket-over-TLS listen address.
    #[arg(long)]
    wss: Option<SocketAddr>,
    /// WebTransport (UDP) listen address.
    #[arg(long)]
    wt: Option<SocketAddr>,
    /// Flush slots N times per second; 0 = flush as soon as something arrives.
    #[arg(long, default_value_t = 0)]
    tick_hz: u32,
    /// Queue every frame instead of keeping the latest per sender (reference mode).
    #[arg(long)]
    fifo: bool,
    /// Where to write cert.der and cert.sha256 (for clients / browsers).
    #[arg(long, default_value = "cert")]
    cert_dir: PathBuf,
    /// Extra subject alt names for the self-signed certificate.
    #[arg(
        long,
        value_delimiter = ',',
        default_value = "localhost,127.0.0.1,10.77.0.1"
    )]
    san: Vec<String>,
    /// WebTransport: QUIC buffers sized for small state frames instead of
    /// quinn's bulk-transfer defaults (memory experiment).
    #[arg(long)]
    quic_lean: bool,
    /// Print relay counters every N seconds to stderr (0 = never).
    #[arg(long, default_value_t = 0)]
    stats_every: u64,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();
    let hub = hub::Hub::new(args.fifo, args.tick_hz);

    // Browsers accept serverCertificateHashes only for certificates valid <= 14 days.
    let identity = Identity::self_signed_builder()
        .subject_alt_names(&args.san)
        .from_now_utc()
        .validity_days(13)
        .build()?;
    let cert = &identity.certificate_chain().as_slice()[0];
    std::fs::create_dir_all(&args.cert_dir)?;
    std::fs::write(args.cert_dir.join("cert.der"), cert.der())?;
    std::fs::write(args.cert_dir.join("cert.sha256"), cert.hash().to_string())?;

    let mut tasks = Vec::new();
    if let Some(addr) = args.ws {
        tasks.push(tokio::spawn(ws::serve(addr, hub.clone(), None)));
    }
    if let Some(addr) = args.wss {
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_single_cert(
            vec![CertificateDer::from(cert.der().to_vec())],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(
                identity.private_key().secret_der().to_vec(),
            )),
        )?;
        tasks.push(tokio::spawn(ws::serve(
            addr,
            hub.clone(),
            Some(TlsAcceptor::from(Arc::new(tls))),
        )));
    }
    if let Some(addr) = args.wt {
        tasks.push(tokio::spawn(wt::serve(
            addr,
            identity.clone_identity(),
            hub.clone(),
            args.quic_lean,
        )));
    }
    println!(
        "ready ws={:?} wss={:?} wt={:?} tick_hz={} fifo={}",
        args.ws, args.wss, args.wt, args.tick_hz, args.fifo
    );

    if args.stats_every > 0 {
        let hub = hub.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(args.stats_every)).await;
                let s = &hub.stats;
                eprintln!(
                    "frames_in={} batches_out={} overwritten={} overflow={} send_dropped={}",
                    s.frames_in.load(Ordering::Relaxed),
                    s.batches_out.load(Ordering::Relaxed),
                    s.overwritten.load(Ordering::Relaxed),
                    s.overflow.load(Ordering::Relaxed),
                    s.send_dropped.load(Ordering::Relaxed)
                );
            }
        });
    }
    tokio::signal::ctrl_c().await?;
    Ok(())
}
