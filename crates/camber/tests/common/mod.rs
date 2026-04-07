#![allow(dead_code)]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use camber::http::{self, Router};
use camber::{RuntimeError, runtime, spawn};
use rustls::pki_types::pem::PemObject;

pub fn test_runtime() -> runtime::RuntimeBuilder {
    runtime::builder()
        .keepalive_timeout(Duration::from_millis(100))
        .shutdown_timeout(Duration::from_secs(1))
}

pub fn spawn_server(router: Router) -> std::net::SocketAddr {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    spawn(move || -> Result<(), RuntimeError> { http::serve_listener(listener, router) });
    addr
}

/// Bridge an async future to sync context inside a `runtime.run()` closure.
/// Uses `block_in_place` + `Handle::block_on` so the tokio runtime remains usable.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// Send an HTTP request with custom headers via raw TCP and return the full response string.
pub fn raw_request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    let mut req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n");
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    buf
}

/// Send an HTTP request with a body via raw TCP and return the full response string.
pub fn raw_request_with_body(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> String {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    let mut req = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    for (name, value) in headers {
        req.push_str(&format!("{name}: {value}\r\n"));
    }
    req.push_str("\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    buf
}

/// Extract the HTTP status code from a raw response string.
pub fn status_from_raw(raw: &str) -> u16 {
    raw.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

// --- TLS test helpers ---

/// Generate a self-signed certificate and key for "localhost".
pub fn generate_self_signed_cert() -> (Vec<u8>, Vec<u8>) {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let cert_pem = cert.cert.pem().into_bytes();
    let key_pem = cert.key_pair.serialize_pem().into_bytes();
    (cert_pem, key_pem)
}

/// Generate a self-signed cert with a custom subject alt name.
pub fn generate_cert_with_san(san: &str) -> (Vec<u8>, Vec<u8>) {
    let cert = rcgen::generate_simple_self_signed(vec![san.to_owned()]).unwrap();
    let cert_pem = cert.cert.pem().into_bytes();
    let key_pem = cert.key_pair.serialize_pem().into_bytes();
    (cert_pem, key_pem)
}

/// Parse PEM-encoded certificate chain and private key.
fn parse_pem(
    cert_pem: &[u8],
    key_pem: &[u8],
) -> (
    Vec<rustls::pki_types::CertificateDer<'static>>,
    rustls::pki_types::PrivateKeyDer<'static>,
) {
    let certs: Vec<_> = rustls::pki_types::CertificateDer::pem_slice_iter(cert_pem)
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let key = rustls::pki_types::PrivateKeyDer::from_pem_slice(key_pem).unwrap();
    (certs, key)
}

/// Build a CertifiedKey from PEM bytes.
pub fn certified_key_from_pem(cert_pem: &[u8], key_pem: &[u8]) -> rustls::sign::CertifiedKey {
    let (certs, key) = parse_pem(cert_pem, key_pem);
    let signing_key = rustls::crypto::aws_lc_rs::sign::any_supported_type(&key).unwrap();
    rustls::sign::CertifiedKey::new(certs, signing_key)
}

/// Build a rustls ServerConfig from PEM bytes (direct, without CertStore).
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

/// Build a rustls ServerConfig using CertStore resolver.
pub fn server_tls_config(cert_pem: &[u8], key_pem: &[u8]) -> Arc<rustls::ServerConfig> {
    let certified = certified_key_from_pem(cert_pem, key_pem);
    let store = camber::CertStore::new(certified);
    camber::tls::build_tls_config_from_resolver(store).unwrap()
}

/// Build a rustls ClientConfig that trusts one or more self-signed cert PEMs.
pub fn tls_client_config(cert_pems: &[&[u8]]) -> rustls::ClientConfig {
    let mut root_store = rustls::RootCertStore::empty();
    for pem in cert_pems {
        let certs = rustls::pki_types::CertificateDer::pem_slice_iter(pem)
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        for cert in certs {
            root_store.add(cert).unwrap();
        }
    }
    rustls::ClientConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .unwrap()
    .with_root_certificates(root_store)
    .with_no_client_auth()
}

/// Write cert and key PEM bytes to temp files.
pub fn write_cert_files(
    dir: &tempfile::TempDir,
    cert_pem: &[u8],
    key_pem: &[u8],
) -> (std::path::PathBuf, std::path::PathBuf) {
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert_pem).unwrap();
    std::fs::write(&key_path, key_pem).unwrap();
    (cert_path, key_path)
}
