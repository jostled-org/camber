use crate::RuntimeError;
use crate::net::TlsStream;
use std::path::PathBuf;
use std::sync::Arc;

use arc_swap::ArcSwap;
use rustls::server::ResolvesServerCert;
use rustls::sign::CertifiedKey;

/// Atomic certificate store that supports hot-swapping at runtime.
///
/// Wraps a `CertifiedKey` behind `ArcSwap` so new TLS connections pick up
/// a replacement cert without restarting the server. Implements
/// `ResolvesServerCert` for use with `rustls::ServerConfig::with_cert_resolver`.
#[derive(Clone)]
pub struct CertStore {
    inner: Arc<ArcSwap<CertifiedKey>>,
}

impl CertStore {
    pub fn new(key: CertifiedKey) -> Self {
        Self {
            inner: Arc::new(ArcSwap::from_pointee(key)),
        }
    }

    /// Atomically replace the current certificate.
    pub fn swap(&self, key: CertifiedKey) {
        self.inner.store(Arc::new(key));
    }

    /// Load the current certificate.
    pub fn load(&self) -> Arc<CertifiedKey> {
        self.inner.load_full()
    }
}

impl std::fmt::Debug for CertStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertStore").finish_non_exhaustive()
    }
}

impl ResolvesServerCert for CertStore {
    fn resolve(&self, _client_hello: rustls::server::ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.inner.load_full())
    }
}

/// Parse a `CertifiedKey` from PEM-encoded certificate and key bytes.
pub fn parse_certified_key(cert_pem: &[u8], key_pem: &[u8]) -> Result<CertifiedKey, RuntimeError> {
    let certs: Vec<_> = rustls_pemfile::certs(&mut &*cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| RuntimeError::Tls(format!("failed to parse TLS cert PEM: {e}").into()))?;

    let key = rustls_pemfile::private_key(&mut &*key_pem)
        .map_err(|e| RuntimeError::Tls(format!("failed to parse TLS key PEM: {e}").into()))?
        .ok_or_else(|| RuntimeError::Tls("no private key found in PEM data".into()))?;

    let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key)
        .map_err(|e| RuntimeError::Tls(format!("unsupported private key type: {e}").into()))?;

    Ok(CertifiedKey::new(certs, signing_key))
}

/// Load a `CertifiedKey` from PEM file paths.
pub fn load_certified_key(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> Result<CertifiedKey, RuntimeError> {
    let cert_data = std::fs::read(cert_path).map_err(|e| {
        RuntimeError::Tls(format!("failed to read TLS cert {}: {e}", cert_path.display()).into())
    })?;
    let key_data = std::fs::read(key_path).map_err(|e| {
        RuntimeError::Tls(format!("failed to read TLS key {}: {e}", key_path.display()).into())
    })?;

    parse_certified_key(&cert_data, &key_data)
}

/// Determine TLS config from either a pre-built CertStore or PEM file paths.
///
/// Returns `(Some(ServerConfig), Some(CertStore))` when TLS is configured,
/// or `(None, None)` when no TLS arguments are provided.
pub fn resolve_tls(
    cert_store: Option<CertStore>,
    cert_path: Option<PathBuf>,
    key_path: Option<PathBuf>,
) -> Result<(Option<Arc<rustls::ServerConfig>>, Option<CertStore>), RuntimeError> {
    match (cert_store, cert_path, key_path) {
        (Some(store), _, _) => {
            let cfg = build_tls_config_from_resolver(store.clone())?;
            Ok((Some(cfg), Some(store)))
        }
        (None, Some(c), Some(k)) => {
            let key = load_certified_key(&c, &k)?;
            let store = CertStore::new(key);
            let cfg = build_tls_config_from_resolver(store.clone())?;
            Ok((Some(cfg), Some(store)))
        }
        (None, None, None) => Ok((None, None)),
        _ => Err(RuntimeError::Tls(
            "both tls_cert and tls_key must be provided".into(),
        )),
    }
}

/// Build a rustls ServerConfig that delegates cert resolution to a CertStore.
pub fn build_tls_config_from_resolver(
    store: CertStore,
) -> Result<Arc<rustls::ServerConfig>, RuntimeError> {
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| {
        RuntimeError::Tls(format!("failed to configure TLS protocol versions: {e}").into())
    })?
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(store));

    // ALPN negotiation: prefer h2, fall back to http/1.1
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];

    Ok(Arc::new(config))
}

/// Connect to a remote address over TLS using the system CA roots.
///
/// `addr` is `"host:port"`. `server_name` is the hostname for SNI/cert validation.
pub async fn connect(addr: &str, server_name: &str) -> Result<TlsStream, RuntimeError> {
    let config = default_client_config()?;
    connect_with(addr, server_name, config).await
}

/// Connect to a remote address over TLS using a custom `ClientConfig`.
///
/// `addr` is `"host:port"`. `server_name` is the hostname for SNI/cert validation.
pub async fn connect_with(
    addr: &str,
    server_name: &str,
    config: Arc<rustls::ClientConfig>,
) -> Result<TlsStream, RuntimeError> {
    let connector = tokio_rustls::TlsConnector::from(config);
    let sni = rustls::pki_types::ServerName::try_from(server_name)
        .map_err(|e| RuntimeError::Tls(format!("invalid server name: {e}").into()))?
        .to_owned();
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    let tls = connector
        .connect(sni, tcp)
        .await
        .map_err(|e| RuntimeError::Tls(e.to_string().into()))?;
    Ok(TlsStream::from_client(tls))
}

fn default_client_config() -> Result<Arc<rustls::ClientConfig>, RuntimeError> {
    static CONFIG: std::sync::OnceLock<Result<Arc<rustls::ClientConfig>, Box<str>>> =
        std::sync::OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let root_store =
                rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            rustls::ClientConfig::builder_with_provider(Arc::new(
                rustls::crypto::aws_lc_rs::default_provider(),
            ))
            .with_safe_default_protocol_versions()
            .map(|builder| {
                Arc::new(
                    builder
                        .with_root_certificates(root_store)
                        .with_no_client_auth(),
                )
            })
            .map_err(|e| format!("TLS config error: {e}").into())
        })
        .as_ref()
        .map(Arc::clone)
        .map_err(|e| RuntimeError::Tls(e.clone()))
}
