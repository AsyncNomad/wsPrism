//! TLS on the public listener (`gateway.tls`): wss:// end to end, bad
//! certificate files fail the build (so `--check-config` catches them),
//! certificates are replaced without a restart, and a client that never
//! finishes the TLS handshake is dropped.

mod common;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use rustls::pki_types::{pem::PemObject, CertificateDer, ServerName};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::Message;
use wsprism_gateway::{config, Gateway};

fn fixture(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/tls")
        .join(name)
}

fn yaml(cert: &Path, key: &Path) -> String {
    format!(
        r#"
version: 1
gateway:
  tls: {{ cert_file: "{}", key_file: "{}", handshake_timeout_ms: 500 }}
tenants:
  - id: t
    policy: {{ ext_allowlist: ["room:*"] }}
"#,
        cert.display(),
        key.display()
    )
}

fn build(yaml: &str) -> Result<Gateway, String> {
    Gateway::builder()
        .config(config::load_from_str(yaml).map_err(|e| e.client_message().to_string())?)
        .build()
        .map_err(|e| e.client_message().to_string())
}

async fn serve(gw: Gateway) -> SocketAddr {
    let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(gw.serve_on(l, None, std::future::pending()));
    addr
}

fn client() -> tokio_rustls::TlsConnector {
    let mut roots = rustls::RootCertStore::empty();
    for c in CertificateDer::pem_file_iter(fixture("ca.pem")).unwrap() {
        roots.add(c.unwrap()).unwrap();
    }
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::ring::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(roots)
    .with_no_client_auth();
    tokio_rustls::TlsConnector::from(Arc::new(cfg))
}

/// TLS-connect and return the stream and the certificate the server sent.
async fn tls_connect(
    addr: SocketAddr,
) -> (
    tokio_rustls::client::TlsStream<TcpStream>,
    CertificateDer<'static>,
) {
    let tcp = TcpStream::connect(addr).await.unwrap();
    let name = ServerName::IpAddress(addr.ip().into());
    let tls = client().connect(name, tcp).await.expect("TLS handshake");
    let leaf = tls.get_ref().1.peer_certificates().unwrap()[0].clone();
    (tls, leaf)
}

fn pem_cert(path: &Path) -> CertificateDer<'static> {
    CertificateDer::pem_file_iter(path)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
}

#[tokio::test]
async fn a_client_connects_over_wss_end_to_end() {
    let addr = serve(build(&yaml(&fixture("a.pem"), &fixture("a.key"))).unwrap()).await;
    let (tls, leaf) = tls_connect(addr).await;
    assert_eq!(leaf, pem_cert(&fixture("a.pem")));
    let mut req = format!("wss://127.0.0.1:{}/v1/ws?tenant=t", addr.port())
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert("authorization", "Bearer dev".parse().unwrap());
    let (mut ws, _) = tokio_tungstenite::client_async(req, tls)
        .await
        .expect("WebSocket over TLS");
    let first = tokio::time::timeout(Duration::from_secs(5), ws.next())
        .await
        .expect("a message")
        .unwrap()
        .unwrap();
    match first {
        Message::Text(t) => assert!(t.contains("\"authed\""), "{t}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn bad_certificate_files_fail_the_build() {
    let missing = build(&yaml(Path::new("/nonexistent/cert.pem"), &fixture("a.key")))
        .err()
        .unwrap();
    assert!(missing.contains("/nonexistent/cert.pem"), "{missing}");
    let mismatch = build(&yaml(&fixture("a.pem"), &fixture("b.key")))
        .err()
        .unwrap();
    assert!(mismatch.contains("does not match"), "{mismatch}");
    let not_a_cert = build(&yaml(&fixture("README.md"), &fixture("a.key")))
        .err()
        .unwrap();
    assert!(not_a_cert.contains("no certificate"), "{not_a_cert}");
}

#[tokio::test]
async fn certificates_are_replaced_without_a_restart_and_a_bad_reload_keeps_the_old_ones() {
    let dir = std::env::temp_dir().join(format!("wsprism-tls-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
    std::fs::copy(fixture("a.pem"), &cert).unwrap();
    std::fs::copy(fixture("a.key"), &key).unwrap();
    let gw = build(&yaml(&cert, &key)).unwrap();
    let addr = serve(gw.clone()).await;
    assert_eq!(tls_connect(addr).await.1, pem_cert(&fixture("a.pem")));

    std::fs::copy(fixture("b.pem"), &cert).unwrap();
    std::fs::copy(fixture("b.key"), &key).unwrap();
    gw.reload_tls().expect("reload");
    assert_eq!(tls_connect(addr).await.1, pem_cert(&fixture("b.pem")));

    std::fs::write(&cert, "not a certificate").unwrap();
    assert!(gw.reload_tls().is_err());
    assert_eq!(
        tls_connect(addr).await.1,
        pem_cert(&fixture("b.pem")),
        "a failed reload keeps serving the last good certificate"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn a_client_that_never_finishes_the_handshake_is_dropped() {
    let addr = serve(build(&yaml(&fixture("a.pem"), &fixture("a.key"))).unwrap()).await;
    let mut tcp = TcpStream::connect(addr).await.unwrap();
    let mut buf = [0u8; 16];
    let n = tokio::time::timeout(Duration::from_secs(3), tcp.read(&mut buf))
        .await
        .expect("closed within the handshake timeout (500 ms)")
        .unwrap_or(0);
    assert_eq!(n, 0, "the server closed the connection");
}
