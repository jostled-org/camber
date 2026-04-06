mod common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime, spawn};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

#[test]
fn uds_serves_http_request() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("camber.sock");
    let sock_addr = format!("unix:{}", sock_path.display());

    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hi")
            });

            let listener = camber::net::listen(&sock_addr).unwrap();
            spawn(move || -> Result<(), RuntimeError> { http::serve_listener(listener, router) });

            // Brief pause for the server to start accepting
            std::thread::sleep(Duration::from_millis(50));

            let mut stream = UnixStream::connect(&sock_path).unwrap();
            stream
                .write_all(b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();

            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let response = std::str::from_utf8(&buf[..n]).unwrap();

            assert!(response.contains("200"), "expected 200 in: {response}");
            assert!(response.contains("hi"), "expected 'hi' in: {response}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn uds_cleans_up_socket_file() {
    let dir = tempfile::tempdir().unwrap();
    let sock_path = dir.path().join("cleanup.sock");
    let sock_addr = format!("unix:{}", sock_path.display());

    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .run(|| {
            let listener = camber::net::listen(&sock_addr).unwrap();
            let check_path = sock_path.clone();

            // Socket file should exist after listen
            assert!(check_path.exists(), "socket file should exist after listen");

            let mut router = Router::new();
            router.get("/ping", |_req: &Request| async {
                Response::text(200, "pong")
            });

            spawn(move || -> Result<(), RuntimeError> { http::serve_listener(listener, router) });

            std::thread::sleep(Duration::from_millis(50));

            // Verify it works
            let mut stream = UnixStream::connect(&check_path).unwrap();
            stream
                .write_all(b"GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).unwrap();
            let response = std::str::from_utf8(&buf[..n]).unwrap();
            assert!(response.contains("200"));

            runtime::request_shutdown();
        })
        .unwrap();

    // After runtime exits, socket file should be cleaned up
    assert!(
        !sock_path.exists(),
        "socket file should be removed after shutdown"
    );
}
