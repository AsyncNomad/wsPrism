//! TLS on the public listener (`gateway.tls`).
//!
//! The certificate lives behind a resolver that can be swapped while the
//! server runs: `reload` reads the files again (SIGHUP, `Gateway::reload_tls`)
//! and replaces the certificate for new handshakes only if the new files
//! load and match; otherwise the last good certificate stays. rustls'
//! defaults otherwise: TLS 1.2 and 1.3, its safe cipher suites, no client
//! certificates. ALPN offers HTTP/1.1 only (WebSocket upgrades are HTTP/1.1).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rustls::pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::ServerConfig;
use wsprism_protocol::error::{Result, WsPrismError};

use crate::config::schema::ListenerTlsConfig;

/// The listener's certificate, replaceable at runtime.
pub struct CertStore {
    cert_file: PathBuf,
    key_file: PathBuf,
    current: RwLock<Arc<CertifiedKey>>,
}

impl std::fmt::Debug for CertStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertStore")
            .field("cert_file", &self.cert_file)
            .field("key_file", &self.key_file)
            .finish_non_exhaustive()
    }
}

impl CertStore {
    /// Load the certificate and key (startup: errors fail the build).
    pub fn load(cfg: &ListenerTlsConfig) -> Result<Arc<Self>> {
        let (cert_file, key_file) = (PathBuf::from(&cfg.cert_file), PathBuf::from(&cfg.key_file));
        let current = read(&cert_file, &key_file).map_err(WsPrismError::BadRequest)?;
        Ok(Arc::new(Self {
            cert_file,
            key_file,
            current: RwLock::new(current),
        }))
    }

    /// Read the files again; on success new handshakes use the new
    /// certificate, on failure the current one stays.
    pub fn reload(&self) -> Result<()> {
        let fresh = read(&self.cert_file, &self.key_file).map_err(WsPrismError::BadRequest)?;
        *self.current.write().unwrap_or_else(|p| p.into_inner()) = fresh;
        Ok(())
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(Arc::clone(
            &self.current.read().unwrap_or_else(|p| p.into_inner()),
        ))
    }
}

/// The rustls server configuration for `store`.
pub fn server_config(store: Arc<CertStore>) -> Result<Arc<ServerConfig>> {
    let mut cfg =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .map_err(|e| WsPrismError::BadRequest(format!("gateway.tls: {e}")))?
            .with_no_client_auth()
            .with_cert_resolver(store);
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(cfg))
}

fn read(cert_file: &Path, key_file: &Path) -> std::result::Result<Arc<CertifiedKey>, String> {
    let certs = CertificateDer::pem_file_iter(cert_file)
        .map_err(|e| format!("gateway.tls.cert_file {}: {e}", cert_file.display()))?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(|e| format!("gateway.tls.cert_file {}: {e}", cert_file.display()))?;
    if certs.is_empty() {
        return Err(format!(
            "gateway.tls.cert_file {}: no certificate in the file",
            cert_file.display()
        ));
    }
    let key = PrivateKeyDer::from_pem_file(key_file)
        .map_err(|e| format!("gateway.tls.key_file {}: {e}", key_file.display()))?;
    let signer = rustls::crypto::ring::sign::any_supported_type(&key)
        .map_err(|e| format!("gateway.tls.key_file {}: {e}", key_file.display()))?;
    let certified = CertifiedKey::new(certs, signer);
    certified.keys_match().map_err(|e| {
        format!(
            "gateway.tls: the key in {} does not match the certificate in {} ({e})",
            key_file.display(),
            cert_file.display()
        )
    })?;
    Ok(Arc::new(certified))
}
