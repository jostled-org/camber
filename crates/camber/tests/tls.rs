mod common;

use camber::http::{Request, Response, Router};
use camber::tls::CertStore;
use camber::{RuntimeError, runtime, spawn};
use std::sync::Arc;
use std::time::Duration;

#[test]
fn tls_serves_https_request() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = common::write_cert_files(&tmp, &cert_pem, &key_pem);

    runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(1))
        .tls_cert(&cert_path)
        .tls_key(&key_path)
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hi")
            });

            let listener = camber::net::listen("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().tcp().unwrap();
            spawn(move || -> Result<(), RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            // Make HTTPS request with custom trust root
            let client_config = common::tls_client_config(&[&cert_pem]);
            let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

            let (status, body) = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
                    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
                    let tls_stream = connector.connect(server_name, tcp).await.unwrap();

                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let (mut sender, conn) =
                        hyper::client::conn::http1::handshake(io).await.unwrap();
                    tokio::spawn(conn);

                    let req = hyper::Request::get(format!("http://localhost/hello"))
                        .body(http_body_util::Empty::<bytes::Bytes>::new())
                        .unwrap();
                    let resp = sender.send_request(req).await.unwrap();
                    let status = resp.status().as_u16();

                    use http_body_util::BodyExt;
                    let body = resp.into_body().collect().await.unwrap().to_bytes();
                    let body = String::from_utf8(body.to_vec()).unwrap();
                    (status, body)
                })
            });

            assert_eq!(status, 200);
            assert_eq!(body, "hi");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn tls_rejects_plaintext() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = common::write_cert_files(&tmp, &cert_pem, &key_pem);

    runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(1))
        .tls_cert(&cert_path)
        .tls_key(&key_path)
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hi")
            });

            let listener = camber::net::listen("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().tcp().unwrap();
            spawn(move || -> Result<(), RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            // Plain HTTP request should fail against a TLS-only server
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current()
                    .block_on(async { camber::http::get(&format!("http://{addr}/hello")).await })
            });
            assert!(result.is_err());

            runtime::request_shutdown();
        })
        .unwrap();
}

/// Verifies that the cert resolver architecture serves HTTPS identically to
/// the old with_single_cert path. Passes if the refactor is transparent.
#[test]
fn tls_still_works_with_resolver_architecture() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = common::write_cert_files(&tmp, &cert_pem, &key_pem);

    runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(1))
        .tls_cert(&cert_path)
        .tls_key(&key_path)
        .run(|| {
            let mut router = Router::new();
            router.get("/resolver-check", |_req: &Request| async {
                Response::text(200, "resolver works")
            });

            let listener = camber::net::listen("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().tcp().unwrap();
            spawn(move || -> Result<(), RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            let client_config = common::tls_client_config(&[&cert_pem]);
            let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));

            let (status, body) = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
                    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
                    let tls_stream = connector.connect(server_name, tcp).await.unwrap();

                    let io = hyper_util::rt::TokioIo::new(tls_stream);
                    let (mut sender, conn) =
                        hyper::client::conn::http1::handshake(io).await.unwrap();
                    tokio::spawn(conn);

                    let req = hyper::Request::get("http://localhost/resolver-check")
                        .body(http_body_util::Empty::<bytes::Bytes>::new())
                        .unwrap();
                    let resp = sender.send_request(req).await.unwrap();
                    let status = resp.status().as_u16();

                    use http_body_util::BodyExt;
                    let body = resp.into_body().collect().await.unwrap().to_bytes();
                    let body = String::from_utf8(body.to_vec()).unwrap();
                    (status, body)
                })
            });

            assert_eq!(status, 200);
            assert_eq!(body, "resolver works");

            runtime::request_shutdown();
        })
        .unwrap();
}

/// Swap a cert at runtime and verify new connections use the new cert.
#[test]
fn cert_hot_swap() {
    // Generate two distinct certs — both for "localhost" so TLS handshake works.
    let (cert_a_pem, key_a_pem) = common::generate_cert_with_san("localhost");
    let (cert_b_pem, key_b_pem) = common::generate_cert_with_san("localhost");

    let key_a = common::certified_key_from_pem(&cert_a_pem, &key_a_pem);
    let key_b = common::certified_key_from_pem(&cert_b_pem, &key_b_pem);

    let cert_store = CertStore::new(key_a);

    runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(1))
        .tls_resolver(cert_store.clone())
        .run(|| {
            let mut router = Router::new();
            router.get("/swap", |_req: &Request| async {
                Response::text(200, "ok")
            });

            let listener = camber::net::listen("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().tcp().unwrap();
            spawn(move || -> Result<(), RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            // Connect with cert A trusted — should succeed.
            let config_a = common::tls_client_config(&[&cert_a_pem]);
            let connector_a = tokio_rustls::TlsConnector::from(Arc::new(config_a));
            let status = https_get(&connector_a, addr, "/swap");
            assert_eq!(status, 200);

            // Swap to cert B.
            cert_store.swap(key_b);

            // Connect with cert B trusted — should succeed.
            let config_b = common::tls_client_config(&[&cert_b_pem]);
            let connector_b = tokio_rustls::TlsConnector::from(Arc::new(config_b));
            let status = https_get(&connector_b, addr, "/swap");
            assert_eq!(status, 200);

            // Connect with ONLY cert A trusted — should fail (server now serves cert B).
            let config_a_only = common::tls_client_config(&[&cert_a_pem]);
            let connector_a_only = tokio_rustls::TlsConnector::from(Arc::new(config_a_only));
            let result = try_https_get(&connector_a_only, addr, "/swap");
            assert!(result.is_err(), "cert A should no longer be served");

            runtime::request_shutdown();
        })
        .unwrap();
}

fn https_get(
    connector: &tokio_rustls::TlsConnector,
    addr: std::net::SocketAddr,
    path: &str,
) -> u16 {
    try_https_get(connector, addr, path).unwrap()
}

fn try_https_get(
    connector: &tokio_rustls::TlsConnector,
    addr: std::net::SocketAddr,
    path: &str,
) -> Result<u16, Box<dyn std::error::Error>> {
    let connector = connector.clone();
    let path = path.to_owned();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let tcp = tokio::net::TcpStream::connect(addr).await?;
            let server_name = rustls::pki_types::ServerName::try_from("localhost")?;
            let tls_stream = connector.connect(server_name, tcp).await?;

            let io = hyper_util::rt::TokioIo::new(tls_stream);
            let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
            tokio::spawn(conn);

            let req = hyper::Request::get(format!("http://localhost{path}"))
                .body(http_body_util::Empty::<bytes::Bytes>::new())?;
            let resp = sender.send_request(req).await?;
            Ok(resp.status().as_u16())
        })
    })
}

#[test]
fn tls_accept_rejects_invalid_handshake() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = common::write_cert_files(&tmp, &cert_pem, &key_pem);

    runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(1))
        .tls_cert(&cert_path)
        .tls_key(&key_path)
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hi")
            });

            let listener = camber::net::listen("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().tcp().unwrap();
            spawn(move || -> Result<(), RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            // Connect with plain TCP (no TLS handshake) — server must not panic
            let mut stream = std::net::TcpStream::connect(addr).expect("connect");
            stream
                .set_read_timeout(Some(Duration::from_secs(2)))
                .expect("set timeout");
            use std::io::Write;
            stream
                .write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .expect("write");

            let mut buf = [0u8; 1024];
            match std::io::Read::read(&mut stream, &mut buf) {
                Ok(0) => {}  // connection closed — expected
                Err(_) => {} // error — expected
                Ok(n) => {
                    let response = String::from_utf8_lossy(&buf[..n]);
                    assert!(
                        !response.contains("200 OK"),
                        "plaintext should not get HTTP 200 from TLS server"
                    );
                }
            }

            // Verify server still works for legitimate TLS clients
            let client_config = common::tls_client_config(&[&cert_pem]);
            let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
            let status = https_get(&connector, addr, "/hello");
            assert_eq!(status, 200);

            runtime::request_shutdown();
        })
        .unwrap();
}

/// Validates that manual TLS (cert/key files) works correctly when the `acme`
/// feature flag is enabled. Guards against the ACME code path interfering with
/// the manual TLS path.
#[test]
#[cfg(feature = "acme")]
fn manual_tls_unaffected_by_acme_feature() {
    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = common::write_cert_files(&tmp, &cert_pem, &key_pem);

    runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(1))
        .tls_cert(&cert_path)
        .tls_key(&key_path)
        .run(|| {
            let mut router = Router::new();
            router.get("/acme-compat", |_req: &Request| async {
                Response::text(200, "manual tls with acme feature")
            });

            let listener = camber::net::listen("127.0.0.1:0").unwrap();
            let addr = listener.local_addr().unwrap().tcp().unwrap();
            spawn(move || -> Result<(), RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            let client_config = common::tls_client_config(&[&cert_pem]);
            let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
            let status = https_get(&connector, addr, "/acme-compat");
            assert_eq!(status, 200);

            runtime::request_shutdown();
        })
        .unwrap();
}
