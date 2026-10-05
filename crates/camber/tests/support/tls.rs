use std::sync::Arc;

use rustls::pki_types::pem::PemObject;

pub fn generate_self_signed_cert() -> (Vec<u8>, Vec<u8>) {
    generate_cert_with_san("localhost")
}

pub fn generate_cert_with_san(san: &str) -> (Vec<u8>, Vec<u8>) {
    let cert = rcgen::generate_simple_self_signed(vec![san.to_owned()]).unwrap();
    (
        cert.cert.pem().into_bytes(),
        cert.signing_key.serialize_pem().into_bytes(),
    )
}

pub fn certified_key_from_pem(cert_pem: &[u8], key_pem: &[u8]) -> rustls::sign::CertifiedKey {
    let (certs, key) = parse_pem(cert_pem, key_pem);
    let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key).unwrap();
    rustls::sign::CertifiedKey::new(certs, signing_key)
}

pub fn build_server_config(cert_pem: &[u8], key_pem: &[u8]) -> Arc<rustls::ServerConfig> {
    let (certs, key) = parse_pem(cert_pem, key_pem);
    let config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_no_client_auth()
    .with_single_cert(certs, key)
    .unwrap();
    Arc::new(config)
}

pub fn server_tls_config(cert_pem: &[u8], key_pem: &[u8]) -> Arc<rustls::ServerConfig> {
    let certified = certified_key_from_pem(cert_pem, key_pem);
    let store = camber::CertStore::new(certified);
    camber::tls::build_tls_config_from_resolver(store).unwrap()
}

/// Builds a matched server config and client connector from one self-signed cert.
///
/// The pair is produced together because the client must trust exactly the cert
/// the server presents. Handing the two halves out separately invites a
/// mismatched pair and a handshake failure that reads as a server bug.
pub fn self_signed_server_and_connector() -> (Arc<rustls::ServerConfig>, tokio_rustls::TlsConnector)
{
    let (cert_pem, key_pem) = generate_self_signed_cert();
    let server_config = server_tls_config(&cert_pem, &key_pem);
    let client_config = tls_client_config(&[&cert_pem]);
    (
        server_config,
        tokio_rustls::TlsConnector::from(Arc::new(client_config)),
    )
}

pub fn tls_client_config(cert_pems: &[&[u8]]) -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(root_store_trusting(cert_pems))
    .with_no_client_auth()
}

/// A root store that trusts every certificate in `cert_pems` and nothing else.
pub fn root_store_trusting(cert_pems: &[&[u8]]) -> rustls::RootCertStore {
    let mut root_store = rustls::RootCertStore::empty();
    cert_pems
        .iter()
        .flat_map(|pem| parse_certs(pem))
        .for_each(|cert| root_store.add(cert).unwrap());
    root_store
}

/// Whether a failed TLS handshake failed because the peer sent an alert.
///
/// A peer that refused the handshake says so with an alert. Every other
/// failure — a reset, an early end of stream, a local fault — is not a refusal,
/// and a fixture that reads it as one passes a refusal row for a reason the row
/// does not claim.
pub fn is_alert_received(error: &std::io::Error) -> bool {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
        .is_some_and(|inner| matches!(inner, rustls::Error::AlertReceived(_)))
}

/// Bounds the join of the client connection task after the exchange ends.
const HTTPS_JOIN_BOUND: std::time::Duration = std::time::Duration::from_secs(5);

/// GET `path` over one TLS connection verified against `localhost`.
///
/// Returns the status and UTF-8 body. The connection task is joined before
/// the result returns, so a connection failure is not hidden by a good body.
pub async fn https_get(
    connector: &tokio_rustls::TlsConnector,
    addr: std::net::SocketAddr,
    path: &str,
) -> Result<(u16, Box<str>), Box<str>> {
    use http_body_util::BodyExt;

    let req = hyper::Request::get(format!("http://localhost{path}"))
        .header("host", "localhost")
        .header("connection", "close")
        .body(http_body_util::Empty::<bytes::Bytes>::new())
        .map_err(|error| format!("HTTP request build failed: {error}").into_boxed_str())?;
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|error| format!("TLS TCP connect failed: {error}").into_boxed_str())?;
    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .map_err(|error| format!("invalid TLS server name: {error}").into_boxed_str())?;
    let tls_stream = connector
        .connect(server_name, tcp)
        .await
        .map_err(|error| format!("TLS handshake failed: {error}").into_boxed_str())?;

    let io = hyper_util::rt::TokioIo::new(tls_stream);
    let (mut sender, conn) = hyper::client::conn::http1::handshake(io)
        .await
        .map_err(|error| format!("HTTP handshake failed: {error}").into_boxed_str())?;
    let connection = tokio::spawn(conn);
    let exchange = async {
        let resp = sender
            .send_request(req)
            .await
            .map_err(|error| format!("HTTPS request failed: {error}").into_boxed_str())?;
        let status = resp.status().as_u16();
        let body = resp
            .into_body()
            .collect()
            .await
            .map_err(|error| format!("HTTPS body failed: {error}").into_boxed_str())?
            .to_bytes();
        let body = std::str::from_utf8(&body)
            .map(Box::from)
            .map_err(|error| format!("HTTPS body was not UTF-8: {error}").into_boxed_str())?;
        Ok::<_, Box<str>>((status, body))
    }
    .await;
    drop(sender);
    let driver = tokio::time::timeout(HTTPS_JOIN_BOUND, connection)
        .await
        .map_err(|error| format!("HTTP connection join timed out: {error}").into_boxed_str())?
        .map_err(|error| format!("HTTP connection task failed: {error}").into_boxed_str())?
        .map_err(|error| format!("HTTP connection failed: {error}").into_boxed_str());
    match (exchange, driver) {
        (Ok(response), Ok(())) => Ok(response),
        (Err(error), _) | (_, Err(error)) => Err(error),
    }
}

fn parse_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> (
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
) {
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem).unwrap();
    (parse_certs(cert_pem), key)
}

/// Every certificate in `pem`, in order.
fn parse_certs(pem: &[u8]) -> Vec<rustls::pki_types::CertificateDer<'static>> {
    rustls::pki_types::CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}
