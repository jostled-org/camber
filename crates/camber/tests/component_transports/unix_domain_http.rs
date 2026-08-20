use std::io::{Read, Write};
use std::path::Path;
use std::time::Duration;

use camber::http::{Request, Response, Router};
use camber::{RuntimeError, runtime, spawn};
use http_body_util::BodyExt;

use crate::{runtime_support, tls_support};

const PROTOCOL_TIMEOUT: Duration = Duration::from_secs(2);

fn uds_get(path: &Path, route: &str) -> (u16, Box<[u8]>) {
    runtime_support::block_on(async {
        tokio::time::timeout(PROTOCOL_TIMEOUT, async {
            let stream = tokio::net::UnixStream::connect(path).await.unwrap();
            let io = hyper_util::rt::TokioIo::new(stream);
            let (mut sender, connection) = hyper::client::conn::http1::handshake(io).await.unwrap();
            let connection = tokio::spawn(connection);
            let request = hyper::Request::get(format!("http://localhost{route}"))
                .body(http_body_util::Empty::<bytes::Bytes>::new())
                .unwrap();
            let response = sender.send_request(request).await.unwrap();
            let status = response.status().as_u16();
            let body = response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec()
                .into_boxed_slice();
            drop(sender);
            connection.await.unwrap().unwrap();
            (status, body)
        })
        .await
        .expect("Unix-domain HTTP exchange timed out")
    })
}

#[test]
fn uds_serves_http_request() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("camber.sock");
    let sock_addr = format!("unix:{}", sock_path.display());

    runtime_support::test_runtime()
        .header_timeout(Duration::from_millis(200))
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_: &Request| async { Response::text(200, "hi") });
            let listener = camber::net::listen(&sock_addr).unwrap();
            spawn(move || -> Result<(), RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            // Binding created the socket before ownership moved to the server task.
            let (status, body) = uds_get(&sock_path, "/hello");
            assert_eq!(status, 200);
            assert_eq!(body.as_ref(), b"hi");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn uds_cleans_up_socket_file() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("cleanup.sock");
    let sock_addr = format!("unix:{}", sock_path.display());

    runtime_support::test_runtime()
        .header_timeout(Duration::from_millis(200))
        .run(|| {
            let listener = camber::net::listen(&sock_addr).unwrap();
            assert!(sock_path.exists(), "socket file should exist after listen");
            let mut router = Router::new();
            router.get("/ping", |_: &Request| async { Response::text(200, "pong") });
            spawn(move || -> Result<(), RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            let (status, body) = uds_get(&sock_path, "/ping");
            assert_eq!(status, 200);
            assert_eq!(body.as_ref(), b"pong");
            runtime::request_shutdown();
        })
        .unwrap();

    assert!(
        !sock_path.exists(),
        "socket file should be removed after shutdown"
    );
}

#[test]
fn uds_bind_does_not_delete_an_existing_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("occupied.sock");
    std::fs::write(&path, b"do not delete").unwrap();
    let address = format!("unix:{}", path.display());

    let error = camber::net::listen(&address)
        .err()
        .expect("binding over an existing file must fail");

    assert!(matches!(error, RuntimeError::Io(_)));
    assert_eq!(std::fs::read(&path).unwrap(), b"do not delete");
}

/// A certificate named for a Unix listener is refused before anything serves.
///
/// The listener speaks cleartext and always has. Serving it anyway would set
/// `X-Forwarded-Proto: https` on everything the server proxied and answer
/// `Request::is_tls` with `true`, so an upstream trusting that header would read
/// a plaintext hop as terminated TLS.
#[test]
fn uds_refuses_a_certificate_it_cannot_serve() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("refused-tls.sock");
    let sock_addr = format!("unix:{}", sock_path.display());
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    let tls_config = tls_support::build_server_config(&cert_pem, &key_pem);

    runtime_support::test_runtime()
        .run(|| {
            let listener = camber::net::listen(&sock_addr).unwrap();
            let refusal = camber::http::server(Router::new())
                .tls(tls_config)
                .serve_listener(listener);
            match refusal {
                Err(RuntimeError::InvalidArgument(message)) => assert!(
                    message.contains("unix"),
                    "expected the listener kind named, got {message}"
                ),
                other => panic!("expected a refused certificate, got {other:?}"),
            }
        })
        .unwrap();
}

/// A runtime certificate reaches a Unix listener as no TLS at all.
///
/// Inheriting is not naming: the server asked for no certificate, so there is
/// nothing to refuse — but there is also no handshake, and what the hop forwards
/// has to say so. `X-Forwarded-Proto` is where that flag leaves this process,
/// and an upstream trusting it would otherwise read a cleartext Unix hop as
/// terminated TLS.
#[test]
fn uds_never_forwards_an_inherited_certificate_as_https() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("inherited-tls.sock");
    let sock_addr = format!("unix:{}", sock_path.display());
    let cert_dir = tempfile::tempdir().unwrap();
    let cert_path = cert_dir.path().join("cert.pem");
    let key_path = cert_dir.path().join("key.pem");
    let (cert_pem, key_pem) = tls_support::generate_self_signed_cert();
    std::fs::write(&cert_path, &cert_pem).unwrap();
    std::fs::write(&key_path, &key_pem).unwrap();

    let upstream_addr = spawn_forwarded_proto_echo();
    runtime_support::test_runtime()
        .tls_cert(&cert_path)
        .tls_key(&key_path)
        .run(|| {
            let mut router = Router::new();
            router.proxy("/via", &format!("http://{upstream_addr}"));
            let listener = camber::net::listen(&sock_addr).unwrap();
            spawn(move || -> Result<(), RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            let (status, body) = uds_get(&sock_path, "/via/scheme");
            assert_eq!(status, 200);
            assert_eq!(
                body.as_ref(),
                b"http",
                "a cleartext Unix hop forwarded itself as terminated TLS"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

/// A cleartext upstream that answers with the `X-Forwarded-Proto` it was sent.
///
/// Hand-rolled rather than served by Camber, because the runtime the row needs
/// configures a certificate and every TCP listener started inside it inherits
/// that one. The upstream has to stand outside the runtime under test.
fn spawn_forwarded_proto_echo() -> std::net::SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept the forwarded-proto probe");
        let head = read_request_head(&mut stream);
        let proto = forwarded_proto(&head);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{proto}",
            proto.len()
        )
        .expect("answer the forwarded-proto probe");
    });
    addr
}

/// Read one request head: every byte through the blank line that ends it.
fn read_request_head(stream: &mut std::net::TcpStream) -> String {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).expect("read the probe request") {
            0 => break,
            _ => head.push(byte[0]),
        }
    }
    String::from_utf8_lossy(&head).into_owned()
}

/// The `X-Forwarded-Proto` a head carries, or `none` when it carries none.
fn forwarded_proto(head: &str) -> String {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(name, _)| name.eq_ignore_ascii_case("x-forwarded-proto"))
        .map(|(_, value)| value.trim().to_owned())
        .unwrap_or_else(|| "none".to_owned())
}

/// A socket path replaced under the service reaches the serving caller.
///
/// The listener refuses to delete a path that is no longer its own socket, and
/// the supervisor is that listener's last owner: the failure has nowhere else to
/// go. Reported only through `Drop`, it would be a warning behind an `Ok(())`.
#[test]
fn uds_reports_a_replaced_socket_path_to_the_serving_caller() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("replaced-under-service.sock");
    let sock_addr = format!("unix:{}", sock_path.display());

    runtime_support::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/ping", |_: &Request| async { Response::text(200, "pong") });
            let listener = camber::net::listen(&sock_addr).unwrap();
            let serving = spawn(move || camber::http::serve_listener(listener, router));

            // Served first, so the replacement below lands under a running
            // service rather than one that has not bound yet.
            let (status, _) = uds_get(&sock_path, "/ping");
            assert_eq!(status, 200);
            std::fs::remove_file(&sock_path).unwrap();
            std::fs::write(&sock_path, b"replacement").unwrap();

            runtime::request_shutdown();
            let served = serving.join().expect("the serving task joined");
            match served {
                Err(RuntimeError::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                other => panic!("expected the refused removal, got {other:?}"),
            }
        })
        .unwrap();

    assert_eq!(
        std::fs::read(&sock_path).unwrap(),
        b"replacement",
        "the replacement file was deleted"
    );
}

#[tokio::test]
async fn uds_drop_does_not_delete_a_replacement_path() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("replaced.sock");
    let address = format!("unix:{}", path.display());
    let listener = camber::net::listen(&address).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"replacement").unwrap();

    drop(listener);

    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
}
