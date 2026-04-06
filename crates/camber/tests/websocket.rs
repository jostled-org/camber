#![cfg(feature = "ws")]

mod common;
#[path = "support/ws_binary_helpers.rs"]
mod ws_binary_helpers;
#[path = "support/ws_frame_io.rs"]
mod ws_frame_io;
#[path = "support/ws_text_helpers.rs"]
mod ws_text_helpers;

use camber::http::{Request, Response, Router, WsConn, WsMessage};
use camber::runtime;
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;
use ws_binary_helpers::{read_ws_binary_frame, write_ws_binary_frame};
use ws_frame_io::read_until_double_crlf;
use ws_text_helpers::{read_ws_text_frame, write_ws_close_frame, write_ws_text_frame};

fn ws_upgrade_request(path: &str, key: &str, extra_headers: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         {extra_headers}\
         Sec-WebSocket-Key: {key}\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    )
}

fn ws_connect(addr: std::net::SocketAddr) -> TcpStream {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let upgrade_req = format!(
        "GET /ws HTTP/1.1\r\n\
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
    stream
}

#[test]
fn websocket_echo() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                while let Some(msg) = conn.recv() {
                    if conn.send(&msg).is_err() {
                        break;
                    }
                }
                Ok(())
            });

            let addr = common::spawn_server(router);

            // Connect via raw TCP and perform WebSocket handshake
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            // Send WebSocket upgrade request
            let key = "dGhlIHNhbXBsZSBub25jZQ==";
            let upgrade_req = format!(
                "GET /ws HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            // Read upgrade response
            let resp = read_until_double_crlf(&mut stream);
            assert!(
                resp.contains("101"),
                "expected 101 switching protocols: {resp}"
            );

            // Send a text frame with "hello"
            write_ws_text_frame(&mut stream, "hello");

            // Read the echo response frame
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(msg, "hello");

            // Send close frame
            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_server_sends_multiple() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                conn.send("one")?;
                conn.send("two")?;
                conn.send("three")?;
                Ok(())
            });

            let addr = common::spawn_server(router);

            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let key = "dGhlIHNhbXBsZSBub25jZQ==";
            let upgrade_req = format!(
                "GET /ws HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert!(resp.contains("101"), "expected 101: {resp}");

            let mut messages = Vec::new();
            for _ in 0..3 {
                messages.push(read_ws_text_frame(&mut stream));
            }

            assert_eq!(messages, vec!["one", "two", "three"]);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_handler_sees_request_path_and_headers() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |req: &Request, mut conn: WsConn| {
                conn.send(req.path())?;
                Ok(())
            });

            let addr = common::spawn_server(router);

            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let key = "dGhlIHNhbXBsZSBub25jZQ==";
            let upgrade_req = format!(
                "GET /ws?token=abc HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert!(resp.contains("101"), "expected 101: {resp}");

            let msg = read_ws_text_frame(&mut stream);
            assert!(msg.contains("/ws"), "expected path in message: {msg}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_send_and_recv_binary_frames() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                while let Some(data) = conn.recv_binary() {
                    if conn.send_binary(&data).is_err() {
                        break;
                    }
                }
                Ok(())
            });

            let addr = common::spawn_server(router);
            let mut stream = ws_connect(addr);

            let payload = b"\x00\x01\x02\xff\xfe\xfd";
            write_ws_binary_frame(&mut stream, payload);

            let received = read_ws_binary_frame(&mut stream);
            assert_eq!(received, payload);

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_recv_message_returns_both_types() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                // Echo back a description of each received message type
                while let Some(msg) = conn.recv_message() {
                    let reply = match &msg {
                        WsMessage::Text(t) => format!("text:{t}"),
                        WsMessage::Binary(b) => format!("binary:{}", b.len()),
                    };
                    conn.send(&reply)?;
                }
                Ok(())
            });

            let addr = common::spawn_server(router);
            let mut stream = ws_connect(addr);

            // Send text, then binary
            write_ws_text_frame(&mut stream, "hello");
            let r1 = read_ws_text_frame(&mut stream);
            assert_eq!(r1, "text:hello");

            write_ws_binary_frame(&mut stream, &[0xDE, 0xAD]);
            let r2 = read_ws_text_frame(&mut stream);
            assert_eq!(r2, "binary:2");

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_recv_binary_skips_text_frames() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                // recv_binary should skip text frames
                if let Some(data) = conn.recv_binary() {
                    conn.send_binary(&data)?;
                }
                Ok(())
            });

            let addr = common::spawn_server(router);
            let mut stream = ws_connect(addr);

            // Send text first (should be skipped), then binary
            write_ws_text_frame(&mut stream, "ignored");
            write_ws_binary_frame(&mut stream, &[0xCA, 0xFE]);

            let received = read_ws_binary_frame(&mut stream);
            assert_eq!(received, &[0xCA, 0xFE]);

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_accepts_same_host_origin() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                conn.send("connected")?;
                Ok(())
            });

            let addr = common::spawn_server(router);
            let port = addr.port();

            // Origin matches Host after normalization (both include the same port)
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let req = format!(
                "GET /ws HTTP/1.1\r\n\
                 Host: localhost:{port}\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Origin: http://localhost:{port}\r\n\
                 Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 \r\n"
            );
            stream.write_all(req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert!(
                resp.contains("101"),
                "expected 101 for same-host origin, got: {resp}"
            );

            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(msg, "connected");

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_rejects_cross_host_origin() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                conn.send("should not reach")?;
                Ok(())
            });

            let addr = common::spawn_server(router);

            // Origin on a different host
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let req = ws_upgrade_request(
                "/ws",
                "dGhlIHNhbXBsZSBub25jZQ==",
                "Origin: http://evil.example.com\r\n",
            );
            stream.write_all(req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert!(
                resp.contains("403"),
                "expected 403 for cross-host origin, got: {resp}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_rejects_null_origin() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                conn.send("should not reach")?;
                Ok(())
            });

            let addr = common::spawn_server(router);

            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let req = ws_upgrade_request("/ws", "dGhlIHNhbXBsZSBub25jZQ==", "Origin: null\r\n");
            stream.write_all(req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert!(
                resp.contains("403"),
                "expected 403 for null origin, got: {resp}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn auth_middleware_blocks_unauthenticated_websocket() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(|req, next| {
                let has_auth = req
                    .headers()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                match has_auth {
                    true => next.call(req),
                    false => Box::pin(async {
                        Response::text(401, "unauthorized").expect("valid status")
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
                }
            });
            router.ws("/chat", |_req: &Request, mut conn: WsConn| {
                while let Some(msg) = conn.recv() {
                    if conn.send(&msg).is_err() {
                        break;
                    }
                }
                Ok(())
            });

            let addr = common::spawn_server(router);

            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let req = ws_upgrade_request("/chat", "dGhlIHNhbXBsZSBub25jZQ==", "");
            stream.write_all(req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert!(
                resp.contains("401"),
                "expected 401 for unauthenticated WS, got: {resp}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_upgrade_ignores_request_body_limit() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new().max_request_body(10);
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                conn.send("connected")?;
                Ok(())
            });

            let addr = common::spawn_server(router);

            // Send WS upgrade with Content-Length exceeding the body limit.
            // Head-only dispatch skips body collection, so 413 is not returned.
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let req = ws_upgrade_request(
                "/ws",
                "dGhlIHNhbXBsZSBub25jZQ==",
                "Content-Length: 99999\r\n",
            );
            stream.write_all(req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert!(
                resp.contains("101"),
                "expected 101 for WS upgrade with oversized Content-Length, got: {resp}"
            );

            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(msg, "connected");

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn auth_middleware_allows_authenticated_websocket() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(|req, next| {
                let has_auth = req
                    .headers()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                match has_auth {
                    true => next.call(req),
                    false => Box::pin(async {
                        Response::text(401, "unauthorized").expect("valid status")
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
                }
            });
            router.ws("/chat", |_req: &Request, mut conn: WsConn| {
                conn.send("welcome")?;
                while let Some(msg) = conn.recv() {
                    if conn.send(&msg).is_err() {
                        break;
                    }
                }
                Ok(())
            });

            let addr = common::spawn_server(router);

            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let req = ws_upgrade_request(
                "/chat",
                "dGhlIHNhbXBsZSBub25jZQ==",
                "Authorization: Bearer token\r\n",
            );
            stream.write_all(req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert!(
                resp.contains("101"),
                "expected 101 for authenticated WS, got: {resp}"
            );

            // Verify WS works end-to-end
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(msg, "welcome");

            write_ws_text_frame(&mut stream, "ping");
            let echo = read_ws_text_frame(&mut stream);
            assert_eq!(echo, "ping");

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}
