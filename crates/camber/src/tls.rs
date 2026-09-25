use crate::RuntimeError;
use crate::net::TlsStream;
use rustls::pki_types::pem::PemObject;
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
    /// Create a certificate store from an initial certified key.
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
    let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| RuntimeError::Tls(format!("failed to parse TLS cert PEM: {e}").into()))?;
    if certs.is_empty() {
        return Err(RuntimeError::Tls("TLS certificate chain is empty".into()));
    }

    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| RuntimeError::Tls(format!("failed to parse TLS key PEM: {e}").into()))?;

    let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key)
        .map_err(|e| RuntimeError::Tls(format!("unsupported private key type: {e}").into()))?;

    let certified_key = CertifiedKey::new(certs, signing_key);
    certified_key.keys_match().map_err(|error| {
        RuntimeError::Tls(format!("TLS private key does not match certificate: {error}").into())
    })?;
    Ok(certified_key)
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
    config.alpn_protocols = vec![b"h2".to_vec(), HTTP1_ALPN.to_vec()];

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
    let sni = client_server_name(server_name)?;
    let tcp = tokio::net::TcpStream::connect(addr).await?;
    let tls = client_handshake(config, sni, tcp).await?;
    Ok(TlsStream::from_client(tls))
}

/// The name a client verifies its peer's certificate against, and sends as SNI.
///
/// Parsed before anything is dialled, so a name no certificate could carry
/// never costs a connection.
pub(crate) fn client_server_name(
    server_name: &str,
) -> Result<rustls::pki_types::ServerName<'static>, RuntimeError> {
    rustls::pki_types::ServerName::try_from(server_name)
        .map(|name| name.to_owned())
        .map_err(|e| RuntimeError::Tls(format!("invalid server name: {e}").into()))
}

/// Authenticate an established transport as the client side of TLS.
pub(crate) async fn client_handshake(
    config: Arc<rustls::ClientConfig>,
    server_name: rustls::pki_types::ServerName<'static>,
    transport: tokio::net::TcpStream,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, RuntimeError> {
    tokio_rustls::TlsConnector::from(config)
        .connect(server_name, transport)
        .await
        .map_err(|e| RuntimeError::Tls(e.to_string().into()))
}

/// A client config cell built once per process from the public WebPKI roots.
type CachedClientConfig = std::sync::OnceLock<Result<Arc<rustls::ClientConfig>, Box<str>>>;

fn default_client_config() -> Result<Arc<rustls::ClientConfig>, RuntimeError> {
    static CONFIG: CachedClientConfig = std::sync::OnceLock::new();
    webpki_client_config(&CONFIG, |roots| client_config(roots, &[]))
}

/// The config a proxied WebSocket verifies its `wss` backend with.
///
/// The public WebPKI roots, like every other outbound TLS connection Camber
/// makes, under the offer [`backend_client_config`] owns.
#[cfg(feature = "ws")]
pub(crate) fn http1_client_config() -> Result<Arc<rustls::ClientConfig>, RuntimeError> {
    static CONFIG: CachedClientConfig = std::sync::OnceLock::new();
    webpki_client_config(&CONFIG, backend_client_config)
}

/// The config a proxied WebSocket verifies a `wss` backend under `roots` with.
///
/// The one place the backend upgrade's ALPN offer is named: HTTP/1.1 alone,
/// because the upgrade is an HTTP/1.1 exchange and a backend that negotiated
/// anything else could not answer it. Production's public roots and the test
/// adapter's local ones are both built here, so neither can offer a protocol
/// set the other does not.
#[cfg(feature = "ws")]
pub(crate) fn backend_client_config(
    roots: rustls::RootCertStore,
) -> Result<rustls::ClientConfig, Box<str>> {
    client_config(roots, &[HTTP1_ALPN])
}

/// The ALPN identifier of HTTP/1.1.
const HTTP1_ALPN: &[u8] = b"http/1.1";

/// Build one cached config from the public WebPKI roots on first use.
fn webpki_client_config(
    cell: &CachedClientConfig,
    build: impl FnOnce(rustls::RootCertStore) -> Result<rustls::ClientConfig, Box<str>>,
) -> Result<Arc<rustls::ClientConfig>, RuntimeError> {
    cell.get_or_init(|| {
        let roots =
            rustls::RootCertStore::from_iter(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        build(roots).map(Arc::new)
    })
    .as_ref()
    .map(Arc::clone)
    .map_err(|e| RuntimeError::Tls(e.clone()))
}

/// Build a client config that trusts `roots` and offers `alpn`, under the
/// same crypto provider and protocol versions as every Camber TLS endpoint.
fn client_config(
    roots: rustls::RootCertStore,
    alpn: &[&[u8]],
) -> Result<rustls::ClientConfig, Box<str>> {
    let mut config = rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .map_err(|e| format!("TLS config error: {e}"))?
    .with_root_certificates(roots)
    .with_no_client_auth();
    config.alpn_protocols = alpn.iter().map(|protocol| protocol.to_vec()).collect();
    Ok(config)
}
