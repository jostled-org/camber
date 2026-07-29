use crate::runtime_support as common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

fn send_request(stream: &mut TcpStream, path: &str, connection: Option<&str>) {
    let conn_header = match connection {
        Some(val) => format!("Connection: {val}\r\n"),
        None => String::new(),
    };
    let req = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n{conn_header}\r\n");
    stream.write_all(req.as_bytes()).expect("write request");
    stream.flush().expect("flush request");
}

fn assert_connection_eof(stream: &mut TcpStream, expected: &str) {
    let mut byte = [0_u8; 1];
    match stream.read(&mut byte) {
        Ok(0) => {}
        Ok(count) => panic!("expected {expected}, but read {count} byte(s): {byte:?}"),
        Err(error) => panic!("expected {expected}, but the EOF read failed: {error}"),
    }
}

#[test]
fn keepalive_serves_multiple_requests_on_one_connection() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/hello", |_req| async {
                camber::http::Response::text(200, "Hello, world!")
            });

            let listener = camber::net::listen("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr").tcp().unwrap();

            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });
            let mut stream = crate::http::connect(addr).expect("connect");

            // First request — no Connection header (HTTP/1.1 defaults to keep-alive)
            send_request(&mut stream, "/hello", None);
            let response =
                crate::http::read_http_response_bounded(&mut stream).expect("first response");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"Hello, world!");

            // Second request on the same connection
            send_request(&mut stream, "/hello", None);
            let response =
                crate::http::read_http_response_bounded(&mut stream).expect("second response");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"Hello, world!");

            // Third request with Connection: close
            send_request(&mut stream, "/hello", Some("close"));
            let response =
                crate::http::read_http_response_bounded(&mut stream).expect("third response");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"Hello, world!");

            // Server should have closed the connection
            assert_connection_eof(
                &mut stream,
                "server to close connection after Connection: close",
            );

            camber::runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn keepalive_timeout_closes_idle_connection() {
    // Short keepalive timeout so the test completes quickly
    camber::runtime::builder()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/hello", |_req| async {
                camber::http::Response::text(200, "Hello")
            });

            let listener = camber::net::listen("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr").tcp().unwrap();

            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });
            let mut stream = crate::http::connect(addr).expect("connect");

            // Send one request
            send_request(&mut stream, "/hello", None);
            let response = crate::http::read_http_response_bounded(&mut stream).expect("response");
            assert_eq!(response.status, 200);

            // Wait longer than the keepalive timeout (200ms)
            std::thread::sleep(Duration::from_millis(300));

            // Server should have closed the connection due to idle timeout
            assert_connection_eof(
                &mut stream,
                "server to close idle connection after keepalive timeout",
            );

            camber::runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn connection_close_header_prevents_keepalive() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/hello", |_req| async {
                camber::http::Response::text(200, "Hello")
            });

            let listener = camber::net::listen("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr").tcp().unwrap();

            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });
            let mut stream = crate::http::connect(addr).expect("connect");

            // Send request with Connection: close
            send_request(&mut stream, "/hello", Some("close"));
            let response = crate::http::read_http_response_bounded(&mut stream).expect("response");
            assert_eq!(response.status, 200);
            assert_eq!(response.header("connection"), Some("close"));

            // Server should have closed the connection
            assert_connection_eof(
                &mut stream,
                "server to close connection after Connection: close",
            );

            camber::runtime::request_shutdown();
        })
        .unwrap();
}
