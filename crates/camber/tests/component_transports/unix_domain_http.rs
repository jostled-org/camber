use std::path::Path;
use std::time::Duration;

use camber::http::{Request, Response, Router};
use camber::{RuntimeError, runtime, spawn};
use http_body_util::BodyExt;

use crate::runtime_support;

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
        .keepalive_timeout(Duration::from_millis(200))
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
        .keepalive_timeout(Duration::from_millis(200))
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
