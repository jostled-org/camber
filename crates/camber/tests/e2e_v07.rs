#![cfg(feature = "ws")]

mod common;
#[path = "support/ws_frame_io.rs"]
mod ws_frame_io;
#[path = "support/ws_text_helpers.rs"]
mod ws_text_helpers;

use camber::http::{self, Request, Response, Router, WsConn};
use camber::runtime;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use ws_frame_io::read_until_double_crlf;
use ws_text_helpers::{read_ws_text_frame, write_ws_close_frame, write_ws_text_frame};

#[camber::test]
async fn e2e_proxy_handles_mixed_content() {
    // Backend with text, binary echo, and large response
    let mut backend = Router::new();
    backend.get("/text", |_req: &Request| async {
        Response::text(200, "hello")
    });
    backend.post("/binary", |req: &Request| {
        let body = req.body_bytes().to_vec();
        async move {
            Response::bytes(200, body).map(|r| r.with_content_type("application/octet-stream"))
        }
    });
    backend.get("/large", |_req: &Request| async {
        let data = vec![0xCDu8; 500_000];
        Response::bytes(200, data).map(|r| r.with_content_type("application/octet-stream"))
    });
    let backend_addr = common::spawn_server(backend);

    // Proxy + normal handler on the same router
    let mut main = Router::new();
    main.get("/health", |_req: &Request| async {
        Response::text(200, "ok")
    });
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    // 1. Text GET through proxy
    let resp = http::get(&format!("http://{main_addr}/api/text"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello");

    // 2. Binary POST round-trip through proxy
    let binary_body: Vec<u8> = (0..=255u8).collect();
    let header = format!(
        "POST /api/binary HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        binary_body.len()
    );
    let mut stream = TcpStream::connect(main_addr).unwrap();
    stream.write_all(header.as_bytes()).unwrap();
    stream.write_all(&binary_body).unwrap();
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();
    let body_start = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("no header/body separator")
        + 4;
    let response_body = &raw[body_start..];
    assert_eq!(
        response_body,
        &binary_body[..],
        "binary data corrupted through proxy"
    );

    // 3. Large GET stream through proxy
    let resp = http::get(&format!("http://{main_addr}/api/large"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_bytes().len(), 500_000);

    // 4. Health check on normal handler
    let resp = http::get(&format!("http://{main_addr}/health"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "ok");

    runtime::request_shutdown();
}

#[test]
fn e2e_websocket_chat_through_proxy() {
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

            // Proxy forwarding
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

            // Send 3 messages, receive 3 echoes
            let messages = ["alpha", "beta", "gamma"];
            for msg in &messages {
                write_ws_text_frame(&mut stream, msg);
                let echo = read_ws_text_frame(&mut stream);
                assert_eq!(echo, *msg, "echo mismatch for message '{msg}'");
            }

            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}
