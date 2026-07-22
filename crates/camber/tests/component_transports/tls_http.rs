use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use camber::http::{Request, Response, Router};
use camber::tls::CertStore;
use camber::{RuntimeError, runtime, spawn};
use http_body_util::BodyExt;

use crate::{temp_support, tls_support};

const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, thiserror::Error)]
enum HttpsRequestError {
    #[error("HTTPS I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("HTTPS protocol failed: {0}")]
    Hyper(#[from] hyper::Error),
    #[error("HTTPS request construction failed: {0}")]
    Http(#[from] ::http::Error),
    #[error("HTTPS operation timed out: {0}")]
    Timeout(#[from] tokio::time::error::Elapsed),
}

fn https_request(
    connector: &tokio_rustls::TlsConnector,
    addr: SocketAddr,
    path: &str,
) -> Result<(u16, Box<[u8]>), HttpsRequestError> {
    crate::runtime_support::block_on(async {
        tokio::time::timeout(PROTOCOL_TIMEOUT, async {
            let tcp = tokio::net::TcpStream::connect(addr).await?;
            let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            let tls_stream = connector.connect(server_name, tcp).await?;
            let io = hyper_util::rt::TokioIo::new(tls_stream);
            let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await?;
            let connection = tokio::spawn(connection);
            let request = hyper::Request::get(format!("http://localhost{path}"))
                .body(http_body_util::Empty::<bytes::Bytes>::new())?;
            let response = sender.send_request(request).await?;
            let status = response.status().as_u16();
            let body = response
                .into_body()
                .collect()
                .await?
                .to_bytes()
                .to_vec()
                .into_boxed_slice();
            drop(sender);
            connection.await.unwrap()?;
            Ok((status, body))
        })
        .await?
    })
}

fn configured_runtime(
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
) -> runtime::RuntimeBuilder {
    runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(1))
        .tls_cert(cert_path)
        .tls_key(key_path)
}

fn spawn_router(router: Router) -> SocketAddr {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    spawn(move || -> Result<(), RuntimeError> { camber::http::serve_listener(listener, router) });
    addr
}

#[test]
fn tls_serves_https_request() {
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = temp_support::write_cert_files(&tmp, &cert_pem, &key_pem);

    configured_runtime(&cert_path, &key_path)
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_: &Request| async { Response::text(200, "hi") });
            let addr = spawn_router(router);
            let connector =
                tokio_rustls::TlsConnector::from(Arc::new(tls_support::tls_client_config(&[
                    &cert_pem,
                ])));
            let (status, body) = https_request(&connector, addr, "/hello").unwrap();
            assert_eq!(status, 200);
            assert_eq!(body.as_ref(), b"hi");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn tls_rejects_plaintext() {
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = temp_support::write_cert_files(&tmp, &cert_pem, &key_pem);

    configured_runtime(&cert_path, &key_path)
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_: &Request| async { Response::text(200, "hi") });
            let addr = spawn_router(router);
            let result = crate::runtime_support::block_on(tokio::time::timeout(
                PROTOCOL_TIMEOUT,
                camber::http::get(&format!("http://{addr}/hello")),
            ))
            .expect("plaintext rejection timed out");
            match result {
                Err(RuntimeError::Http(message)) => {
                    assert!(
                        !message.is_empty(),
                        "HTTP transport error must have context"
                    );
                }
                Err(RuntimeError::Timeout) => {
                    panic!("Camber HTTP timeout is not proof that TLS rejected plaintext");
                }
                Err(error) => panic!("expected RuntimeError::Http, got {error:?}"),
                Ok(response) => panic!(
                    "plaintext unexpectedly produced HTTP response {}",
                    response.status()
                ),
            }
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn tls_still_works_with_resolver_architecture() {
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = temp_support::write_cert_files(&tmp, &cert_pem, &key_pem);

    configured_runtime(&cert_path, &key_path)
        .run(|| {
            let mut router = Router::new();
            router.get("/resolver-check", |_: &Request| async {
                Response::text(200, "resolver works")
            });
            let addr = spawn_router(router);
            let connector =
                tokio_rustls::TlsConnector::from(Arc::new(tls_support::tls_client_config(&[
                    &cert_pem,
                ])));
            let (status, body) = https_request(&connector, addr, "/resolver-check").unwrap();
            assert_eq!(status, 200);
            assert_eq!(body.as_ref(), b"resolver works");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn cert_hot_swap() {
    let (cert_a_pem, key_a_pem) = tls_support::generate_cert_with_san("localhost");
    let (cert_b_pem, key_b_pem) = tls_support::generate_cert_with_san("localhost");
    let cert_store = CertStore::new(tls_support::certified_key_from_pem(&cert_a_pem, &key_a_pem));

    runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(1))
        // The runtime and test intentionally share the live certificate store.
        .tls_resolver(cert_store.clone())
        .run(|| {
            let mut router = Router::new();
            router.get("/swap", |_: &Request| async { Response::text(200, "ok") });
            let addr = spawn_router(router);

            let connector_a =
                tokio_rustls::TlsConnector::from(Arc::new(tls_support::tls_client_config(&[
                    &cert_a_pem,
                ])));
            let (status, body) = https_request(&connector_a, addr, "/swap").unwrap();
            assert_eq!(status, 200);
            assert_eq!(body.as_ref(), b"ok");

            cert_store.swap(tls_support::certified_key_from_pem(&cert_b_pem, &key_b_pem));
            let connector_b =
                tokio_rustls::TlsConnector::from(Arc::new(tls_support::tls_client_config(&[
                    &cert_b_pem,
                ])));
            let (status, body) = https_request(&connector_b, addr, "/swap").unwrap();
            assert_eq!(status, 200);
            assert_eq!(body.as_ref(), b"ok");

            let connector_a_only =
                tokio_rustls::TlsConnector::from(Arc::new(tls_support::tls_client_config(&[
                    &cert_a_pem,
                ])));
            match https_request(&connector_a_only, addr, "/swap") {
                Err(HttpsRequestError::Io(error)) => {
                    let tls_error = error
                        .get_ref()
                        .and_then(|source| source.downcast_ref::<rustls::Error>());
                    assert!(
                        matches!(tls_error, Some(rustls::Error::InvalidCertificate(_))),
                        "expected certificate validation failure, got {error:?}"
                    );
                }
                Err(HttpsRequestError::Timeout(error)) => {
                    panic!("timeout is not proof that cert A was rejected: {error}");
                }
                Err(error) => panic!("expected certificate validation failure, got {error:?}"),
                Ok((status, _)) => {
                    panic!("cert-A-only client unexpectedly received HTTP status {status}");
                }
            }
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn tls_accept_rejects_invalid_handshake() {
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = temp_support::write_cert_files(&tmp, &cert_pem, &key_pem);

    configured_runtime(&cert_path, &key_path)
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_: &Request| async { Response::text(200, "hi") });
            let addr = spawn_router(router);

            let mut stream = std::net::TcpStream::connect_timeout(&addr, PROTOCOL_TIMEOUT).unwrap();
            stream.set_read_timeout(Some(PROTOCOL_TIMEOUT)).unwrap();
            stream.set_write_timeout(Some(PROTOCOL_TIMEOUT)).unwrap();
            std::io::Write::write_all(
                &mut stream,
                b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n",
            )
            .unwrap();
            let mut rejection_bytes = Vec::new();
            match std::io::Read::read_to_end(&mut stream, &mut rejection_bytes) {
                // read_to_end returns successfully only after the peer sends EOF.
                Ok(_) => {}
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::ConnectionAborted | std::io::ErrorKind::ConnectionReset
                    ) => {}
                Err(error) => {
                    panic!("expected TLS peer EOF or connection abort/reset, got {error:?}")
                }
            }

            let connector =
                tokio_rustls::TlsConnector::from(Arc::new(tls_support::tls_client_config(&[
                    &cert_pem,
                ])));
            let (status, body) = https_request(&connector, addr, "/hello").unwrap();
            assert_eq!(status, 200);
            assert_eq!(body.as_ref(), b"hi");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[cfg(feature = "acme")]
#[test]
fn manual_tls_unaffected_by_acme_feature() {
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    let tmp = tempfile::tempdir().unwrap();
    let (cert_path, key_path) = temp_support::write_cert_files(&tmp, &cert_pem, &key_pem);

    configured_runtime(&cert_path, &key_path)
        .run(|| {
            let mut router = Router::new();
            router.get("/acme-compat", |_: &Request| async {
                Response::text(200, "manual tls with acme feature")
            });
            let addr = spawn_router(router);
            let connector =
                tokio_rustls::TlsConnector::from(Arc::new(tls_support::tls_client_config(&[
                    &cert_pem,
                ])));
            let (status, body) = https_request(&connector, addr, "/acme-compat").unwrap();
            assert_eq!(status, 200);
            assert_eq!(body.as_ref(), b"manual tls with acme feature");
            runtime::request_shutdown();
        })
        .unwrap();
}
