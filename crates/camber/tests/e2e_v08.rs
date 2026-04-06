#![cfg(feature = "ws")]

mod common;
mod support;

use camber::http::{self, Request, Response, Router, WsConn};
use camber::{RuntimeError, runtime, spawn};
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;
use support::ws_helpers::{
    read_until_double_crlf, read_ws_text_frame, write_ws_close_frame, write_ws_text_frame,
};

fn spawn_host_server(host_router: http::HostRouter) -> std::net::SocketAddr {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    spawn(move || -> Result<(), RuntimeError> { http::serve_hosts(listener, host_router) });
    std::thread::sleep(Duration::from_millis(50));
    addr
}

/// Send a GET request with a specific Host header via raw TCP.
fn get_with_host(addr: std::net::SocketAddr, path: &str, host: &str) -> (u16, String) {
    use std::io::Read;
    let mut stream = TcpStream::connect(addr).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();

    let status_line = buf.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let body = buf.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    let body = parse_chunked_body(body);
    (status, body)
}

/// Parse an HTTP response body, handling chunked transfer encoding.
fn parse_chunked_body(raw: &str) -> String {
    let trimmed = raw.trim();
    match trimmed
        .lines()
        .next()
        .and_then(|line| usize::from_str_radix(line.trim(), 16).ok())
    {
        Some(size) if size > 0 => {
            let data_start = trimmed.find('\n').map(|i| i + 1).unwrap_or(0);
            trimmed[data_start..data_start + size].to_string()
        }
        _ => trimmed.to_string(),
    }
}

/// 4.T1: Host routing + async proxy + connection pooling
///
/// Two backends with different responses. HostRouter with two sites,
/// each proxying to a different backend. Validates the full config-driven
/// proxy path with reqwest underneath.
#[test]
fn e2e_proxy_mode_with_async_forwarding() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend A: returns "site-a"
            let mut backend_a = Router::new();
            backend_a.get("/data", |_req: &Request| async {
                Response::text(200, "site-a")
            });
            let addr_a = common::spawn_server(backend_a);

            // Backend B: returns "site-b"
            let mut backend_b = Router::new();
            backend_b.get("/data", |_req: &Request| async {
                Response::text(200, "site-b")
            });
            let addr_b = common::spawn_server(backend_b);

            // Host router: a.test → backend_a, b.test → backend_b
            let mut router_a = Router::new();
            router_a.proxy("/api", &format!("http://{addr_a}"));

            let mut router_b = Router::new();
            router_b.proxy("/api", &format!("http://{addr_b}"));

            let mut host_router = http::HostRouter::new();
            host_router.add("a.test", router_a);
            host_router.add("b.test", router_b);

            let proxy_addr = spawn_host_server(host_router);

            // Hit site A through proxy
            let (status_a, body_a) = get_with_host(proxy_addr, "/api/data", "a.test");
            assert_eq!(status_a, 200);
            assert_eq!(body_a, "site-a");

            // Hit site B through proxy
            let (status_b, body_b) = get_with_host(proxy_addr, "/api/data", "b.test");
            assert_eq!(status_b, 200);
            assert_eq!(body_b, "site-b");

            runtime::request_shutdown();
        })
        .unwrap();
}

/// 4.T2: Handler outbound with reqwest
///
/// A handler calls http::get() to fetch from a backend, then returns the
/// result. Validates the sync http::get → reqwest → backend path.
#[camber::test]
async fn e2e_handler_outbound_with_reqwest() {
    // Backend returning upstream data
    let mut backend = Router::new();
    backend.get("/upstream", |_req: &Request| async {
        Response::text(200, "upstream-data")
    });
    let backend_addr = common::spawn_server(backend);

    // Server with handler that fetches from backend
    let backend_url: std::sync::Arc<str> = format!("http://{backend_addr}/upstream").into();
    let mut main = Router::new();
    main.get("/fetch", move |_req: &Request| {
        let backend_url = backend_url.clone();
        async move {
            match http::get(&backend_url).await {
                Ok(resp) => Response::text(200, resp.body()),
                Err(e) => Response::text(502, &format!("fetch failed: {e}")),
            }
        }
    });
    let main_addr = common::spawn_server(main);

    // Client hits server, gets upstream data via handler
    let resp = http::get(&format!("http://{main_addr}/fetch"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "upstream-data");

    runtime::request_shutdown();
}

/// 4.T3: WebSocket proxy still works after ureq removal
///
/// End-to-end: backend WS echo server → proxy → client sends messages,
/// receives echoes. Validates WS proxy path (tokio-tungstenite) is
/// unbroken by the ureq→reqwest migration.
#[test]
fn e2e_websocket_proxy_still_works() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: WebSocket echo server
            let mut backend = Router::new();
            backend.ws("/echo", |_req: &Request, mut conn: WsConn| {
                while let Some(msg) = conn.recv() {
                    if conn.send(&msg).is_err() {
                        break;
                    }
                }
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            // Proxy forwarding to backend
            let mut proxy = Router::new();
            proxy.proxy("/ws", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            // Client: WebSocket handshake through proxy
            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let key = "dGhlIHNhbXBsZSBub25jZQ==";
            let upgrade_req = format!(
                "GET /ws/echo HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert!(
                resp.contains("101"),
                "expected 101 switching protocols: {resp}"
            );

            // Send messages, receive echoes
            let messages = ["hello", "world", "v08"];
            for msg in &messages {
                write_ws_text_frame(&mut stream, msg);
                let echo = read_ws_text_frame(&mut stream);
                assert_eq!(echo, *msg, "echo mismatch for '{msg}'");
            }

            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}
