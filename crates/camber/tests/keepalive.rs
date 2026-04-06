mod common;

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
}

fn read_response(stream: &mut TcpStream) -> (u16, String) {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).expect("read response");
    let text = String::from_utf8_lossy(&buf[..n]);
    let status = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    (status, text.into_owned())
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
            std::thread::sleep(Duration::from_millis(50));

            let mut stream = TcpStream::connect(addr).expect("connect");
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

            // First request — no Connection header (HTTP/1.1 defaults to keep-alive)
            send_request(&mut stream, "/hello", None);
            let (status, resp) = read_response(&mut stream);
            assert_eq!(status, 200);
            assert!(resp.contains("Hello, world!"));

            // Second request on the same connection
            send_request(&mut stream, "/hello", None);
            let (status, resp) = read_response(&mut stream);
            assert_eq!(status, 200);
            assert!(resp.contains("Hello, world!"));

            // Third request with Connection: close
            send_request(&mut stream, "/hello", Some("close"));
            let (status, resp) = read_response(&mut stream);
            assert_eq!(status, 200);
            assert!(resp.contains("Hello, world!"));

            // Server should have closed the connection
            let mut eof_buf = [0u8; 1];
            let n = stream.read(&mut eof_buf).unwrap_or(0);
            assert_eq!(
                n, 0,
                "expected server to close connection after Connection: close"
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
            std::thread::sleep(Duration::from_millis(50));

            let mut stream = TcpStream::connect(addr).expect("connect");
            stream.set_read_timeout(Some(Duration::from_secs(2))).ok();

            // Send one request
            send_request(&mut stream, "/hello", None);
            let (status, _) = read_response(&mut stream);
            assert_eq!(status, 200);

            // Wait longer than the keepalive timeout (200ms)
            std::thread::sleep(Duration::from_millis(300));

            // Server should have closed the connection due to idle timeout
            let mut eof_buf = [0u8; 1];
            let n = stream.read(&mut eof_buf).unwrap_or(0);
            assert_eq!(
                n, 0,
                "expected server to close idle connection after keepalive timeout"
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
            std::thread::sleep(Duration::from_millis(50));

            let mut stream = TcpStream::connect(addr).expect("connect");
            stream.set_read_timeout(Some(Duration::from_secs(5))).ok();

            // Send request with Connection: close
            send_request(&mut stream, "/hello", Some("close"));
            let (status, resp) = read_response(&mut stream);
            assert_eq!(status, 200);
            assert!(
                resp.to_lowercase().contains("connection: close"),
                "response should include Connection: close header, got: {resp}"
            );

            // Server should have closed the connection
            let mut eof_buf = [0u8; 1];
            let n = stream.read(&mut eof_buf).unwrap_or(0);
            assert_eq!(
                n, 0,
                "expected server to close connection after Connection: close"
            );

            camber::runtime::request_shutdown();
        })
        .unwrap();
}
