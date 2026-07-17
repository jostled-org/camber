#![cfg(feature = "ws")]

mod common;
#[path = "support/ws_binary_helpers.rs"]
mod ws_binary_helpers;
#[path = "support/ws_frame_io.rs"]
mod ws_frame_io;
#[path = "support/ws_text_helpers.rs"]
mod ws_text_helpers;

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, LifecycleFault, lifecycle};
use camber::http::{Request, Response, Router, WsConn, WsMessage};
use camber::runtime;
use futures_util::FutureExt;
use std::future::{Future, IntoFuture};
use std::io::Write;
use std::net::TcpStream;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ws_binary_helpers::{read_ws_binary_frame, write_ws_binary_frame};
use ws_frame_io::read_until_double_crlf;
use ws_text_helpers::{read_ws_text_frame, write_ws_close_frame, write_ws_text_frame};

const LIFECYCLE_EVENT_TIMEOUT: Duration = Duration::from_secs(5);

struct CallbackDropProbe(Arc<AtomicBool>);

impl Drop for CallbackDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn lifecycle_event<F>(context: &str, future: F) -> F::Output
where
    F: Future,
{
    tokio::time::timeout(LIFECYCLE_EVENT_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
}

async fn async_ws_request(stream: &mut tokio::net::TcpStream, path: &str) {
    let request = ws_upgrade_request(path, "dGhlIHNhbXBsZSBub25jZQ==", "");
    tokio::io::AsyncWriteExt::write_all(stream, request.as_bytes())
        .await
        .expect("write WebSocket upgrade request");
}

async fn read_async_http_head(stream: &mut tokio::net::TcpStream) -> Box<str> {
    lifecycle_event("HTTP response head", async {
        let mut response = Vec::new();
        let mut byte = [0u8; 1];
        while !response.ends_with(b"\r\n\r\n") {
            let count = tokio::io::AsyncReadExt::read(stream, &mut byte)
                .await
                .expect("read HTTP response head");
            assert_ne!(count, 0, "peer closed before completing HTTP response head");
            response.push(byte[0]);
        }
        String::from_utf8(response)
            .expect("HTTP response head is UTF-8")
            .into_boxed_str()
    })
    .await
}

async fn connect_async_websocket(addr: std::net::SocketAddr, path: &str) -> tokio::net::TcpStream {
    let mut stream = lifecycle_event(
        "WebSocket TCP connection",
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .expect("connect WebSocket peer");
    async_ws_request(&mut stream, path).await;
    let response = read_async_http_head(&mut stream).await;
    assert!(
        response.starts_with("HTTP/1.1 101"),
        "expected WebSocket upgrade, got: {response}"
    );
    stream
}

async fn read_async_ws_frame_or_eof(stream: &mut tokio::net::TcpStream) -> Option<(u8, Vec<u8>)> {
    lifecycle_event("WebSocket frame or EOF", async {
        let mut header = [0u8; 2];
        let first = tokio::io::AsyncReadExt::read(stream, &mut header[..1])
            .await
            .expect("read WebSocket frame header");
        if first == 0 {
            return None;
        }
        tokio::io::AsyncReadExt::read_exact(stream, &mut header[1..])
            .await
            .expect("read WebSocket frame header");
        assert_eq!(header[1] & 0x80, 0, "server frame must not be masked");

        let length = match header[1] & 0x7f {
            126 => {
                let mut extended = [0u8; 2];
                tokio::io::AsyncReadExt::read_exact(stream, &mut extended)
                    .await
                    .expect("read WebSocket frame length");
                u16::from_be_bytes(extended) as usize
            }
            127 => {
                let mut extended = [0u8; 8];
                tokio::io::AsyncReadExt::read_exact(stream, &mut extended)
                    .await
                    .expect("read WebSocket frame length");
                usize::try_from(u64::from_be_bytes(extended))
                    .expect("WebSocket frame length fits usize")
            }
            length => length as usize,
        };
        let mut payload = vec![0u8; length];
        tokio::io::AsyncReadExt::read_exact(stream, &mut payload)
            .await
            .expect("read WebSocket frame payload");
        Some((header[0] & 0x0f, payload))
    })
    .await
}

async fn write_async_ws_frame(stream: &mut tokio::net::TcpStream, opcode: u8, payload: &[u8]) {
    assert!(payload.len() <= 125, "test frame payload must be short");
    let mask = [0x12, 0x34, 0x56, 0x78];
    let mut frame = Vec::with_capacity(payload.len() + 6);
    frame.extend_from_slice(&[0x80 | opcode, 0x80 | payload.len() as u8]);
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    tokio::io::AsyncWriteExt::write_all(stream, &frame)
        .await
        .expect("write WebSocket frame");
}

async fn read_ws_event_before_owner<F>(
    stream: &mut tokio::net::TcpStream,
    mut owner: Pin<&mut F>,
) -> Option<(u8, Vec<u8>)>
where
    F: Future<Output = Result<(), RuntimeError>>,
{
    lifecycle_event("WebSocket event before owner completion", async {
        tokio::select! {
            biased;
            result = owner.as_mut() => {
                panic!("owner completed before WebSocket transport event: {result:?}")
            }
            event = read_async_ws_frame_or_eof(stream) => event,
        }
    })
    .await
}

async fn assert_eof_before_owner<F>(stream: &mut tokio::net::TcpStream, mut owner: Pin<&mut F>)
where
    F: Future<Output = Result<(), RuntimeError>>,
{
    lifecycle_event("transport EOF before owner completion", async {
        let mut byte = [0u8; 1];
        tokio::select! {
            biased;
            result = owner.as_mut() => {
                panic!("owner completed before transport EOF: {result:?}")
            }
            read = tokio::io::AsyncReadExt::read(stream, &mut byte) => {
                assert_eq!(read.expect("read transport EOF"), 0, "expected transport EOF");
            }
        }
    })
    .await;
}

async fn assert_http_ok(addr: std::net::SocketAddr, path: &str) {
    let mut stream = lifecycle_event(
        "HTTP probe connection",
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .expect("connect HTTP probe");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
        .await
        .expect("write HTTP probe");
    let response = read_async_http_head(&mut stream).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "expected successful HTTP probe, got: {response}"
    );
}

fn lifecycle_websocket_router() -> Router {
    let mut router = Router::new();
    router.ws("/ws", |_request: &Request, mut connection: WsConn| {
        while connection.recv().is_some() {}
        Ok(())
    });
    router
}

fn assert_cancelled(result: Result<(), RuntimeError>) {
    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}

fn arm_unacknowledged_upgrade(controller: &LifecycleController) {
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("pause after upgrade-ticket submission");
    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("pause before upgrade acknowledgement");
}

async fn wait_for_unacknowledged_upgrade(controller: &LifecycleController) {
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .await
        .expect("upgrade ticket reaches the production registration channel");
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("release submitted upgrade ticket");
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .await
        .expect("submitted upgrade reaches acknowledgement checkpoint");
}

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

// 1.T9, direct WebSocket portion.
#[test]
fn direct_websocket_bridge_holds_permit_and_finishes_before_owned_completion() {
    runtime::builder()
        .connection_limit(1)
        .keepalive_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            runtime::block_on(async {
                let (dispatched_tx, mut dispatched_rx) = tokio::sync::oneshot::channel();
                let dispatched_tx = Arc::new(Mutex::new(Some(dispatched_tx)));
                let mut router = lifecycle_websocket_router();
                router.get("/second", move |_request: &Request| {
                    let dispatched_tx = Arc::clone(&dispatched_tx);
                    async move {
                        if let Some(sender) = dispatched_tx
                            .lock()
                            .unwrap_or_else(|error| error.into_inner())
                            .take()
                        {
                            let _ = sender.send(());
                        }
                        Response::text(200, "second")
                    }
                });

                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind owned listener");
                let addr = listener.local_addr().expect("owned listener address");
                let controller = lifecycle(addr).expect("install lifecycle controller");
                let handle = camber::http::serve_background(listener, router);
                let mut websocket = connect_async_websocket(addr, "/ws").await;

                controller
                    .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .expect("pause once the second client waits for a permit");
                let mut second = tokio::net::TcpStream::connect(addr)
                    .await
                    .expect("connect permit-waiting peer");
                tokio::io::AsyncWriteExt::write_all(
                    &mut second,
                    b"GET /second HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write permit-waiting request");
                controller
                    .wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .await
                    .expect("production permit acquisition returned Pending");
                assert!(
                    matches!(
                        dispatched_rx.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                    ),
                    "second request dispatched while the direct bridge held the permit"
                );

                runtime::request_shutdown();
                controller
                    .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .expect("release pending permit wait for shutdown");
                let mut owner = Box::pin(handle.into_future());
                assert!(
                    owner.as_mut().now_or_never().is_none(),
                    "owner completed while the direct bridge still owned its transport"
                );
                let (opcode, _) = read_ws_event_before_owner(&mut websocket, owner.as_mut())
                    .await
                    .expect("graceful shutdown sends a close frame");
                assert_eq!(opcode, 0x8, "expected graceful WebSocket close frame");
                write_async_ws_frame(&mut websocket, 0x8, &[]).await;
                assert_eof_before_owner(&mut websocket, owner.as_mut()).await;
                assert_eof_before_owner(&mut second, owner.as_mut()).await;
                assert!(
                    lifecycle_event("owned direct bridge completion", owner.as_mut())
                        .await
                        .is_ok()
                );
                assert!(
                    matches!(
                        dispatched_rx.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
                    ),
                    "permit-waiting dispatch sender remained live after owner completion"
                );
            });
        })
        .unwrap();
}

// 1.T15.
#[camber::test]
async fn owner_releases_direct_transport_without_claiming_blocking_callback_exit() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let (callback_result_tx, mut callback_result_rx) = tokio::sync::oneshot::channel();
    let callback_result_tx = Arc::new(Mutex::new(Some(callback_result_tx)));
    let callback_dropped = Arc::new(AtomicBool::new(false));

    let mut router = Router::new();
    router.ws("/ws", {
        let entered_tx = Arc::clone(&entered_tx);
        let release_rx = Arc::clone(&release_rx);
        let callback_result_tx = Arc::clone(&callback_result_tx);
        let callback_dropped = Arc::clone(&callback_dropped);
        move |_request: &Request, mut connection: WsConn| {
            let _probe = CallbackDropProbe(Arc::clone(&callback_dropped));
            if let Some(sender) = entered_tx
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                let _ = sender.send(());
            }
            let _ = release_rx
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv();
            let peers_were_closed = connection.send("after owner completion").is_err();
            if let Some(sender) = callback_result_tx
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                let _ = sender.send(peers_were_closed);
            }
            Ok(())
        }
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind callback-boundary listener");
    let addr = listener.local_addr().expect("callback listener address");
    let handle = camber::http::serve_background(listener, router);
    let mut websocket = connect_async_websocket(addr, "/ws").await;
    lifecycle_event("blocking WebSocket callback entry", entered_rx)
        .await
        .expect("blocking callback reports entry");

    runtime::request_shutdown();
    let mut owner = Box::pin(handle.into_future());
    let (opcode, _) = read_ws_event_before_owner(&mut websocket, owner.as_mut())
        .await
        .expect("graceful shutdown sends a close frame");
    assert_eq!(opcode, 0x8, "expected graceful WebSocket close frame");
    write_async_ws_frame(&mut websocket, 0x8, &[]).await;
    assert_eof_before_owner(&mut websocket, owner.as_mut()).await;
    assert!(
        lifecycle_event("owner completion across callback boundary", owner.as_mut())
            .await
            .is_ok()
    );
    assert!(
        !callback_dropped.load(Ordering::Acquire),
        "owner completion incorrectly claimed blocking callback exit"
    );
    assert!(
        matches!(
            callback_result_rx.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "blocking callback returned before its explicit release"
    );

    release_tx.send(()).expect("release blocking callback");
    assert!(
        lifecycle_event("callback-side channel failure", callback_result_rx)
            .await
            .expect("callback reports post-owner send result"),
        "callback-side WsConn retained a live supervisor peer after owner completion"
    );
    assert!(callback_dropped.load(Ordering::Acquire));
}

// 1.T18, graceful direct WebSocket portion.
#[camber::test]
async fn graceful_direct_websocket_shutdown_sends_close_before_eof_and_join() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind graceful WebSocket listener");
    let addr = listener.local_addr().expect("graceful listener address");
    let handle = camber::http::serve_background(listener, lifecycle_websocket_router());
    let mut websocket = connect_async_websocket(addr, "/ws").await;

    runtime::request_shutdown();
    let mut owner = Box::pin(handle.into_future());
    let (opcode, _) = read_ws_event_before_owner(&mut websocket, owner.as_mut())
        .await
        .expect("graceful shutdown sends a frame before EOF");
    assert_eq!(opcode, 0x8, "expected graceful WebSocket close frame");
    write_async_ws_frame(&mut websocket, 0x8, &[]).await;
    assert_eof_before_owner(&mut websocket, owner.as_mut()).await;
    assert!(
        lifecycle_event("graceful direct bridge join", owner.as_mut())
            .await
            .is_ok()
    );
}

// 1.T18, forced direct WebSocket portion.
#[camber::test]
async fn forced_direct_websocket_abort_releases_transport_before_cancelled() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind forced WebSocket listener");
    let addr = listener.local_addr().expect("forced listener address");
    let handle = camber::http::serve_background(listener, lifecycle_websocket_router());
    let mut websocket = connect_async_websocket(addr, "/ws").await;

    handle.cancel();
    let mut owner = Box::pin(handle.into_future());
    match read_ws_event_before_owner(&mut websocket, owner.as_mut()).await {
        None => {}
        Some((0x8, _)) => {
            write_async_ws_frame(&mut websocket, 0x8, &[]).await;
            assert_eof_before_owner(&mut websocket, owner.as_mut()).await;
        }
        Some((opcode, payload)) => {
            panic!("forced shutdown emitted opcode {opcode:#x} with payload {payload:?}")
        }
    }
    assert_cancelled(lifecycle_event("forced direct bridge join", owner.as_mut()).await);
}

async fn pending_direct_upgrade_shutdown_is_rejected(forced: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind pending-upgrade listener");
    let addr = listener.local_addr().expect("pending listener address");
    let controller = lifecycle(addr).expect("install lifecycle controller");
    arm_unacknowledged_upgrade(&controller);
    let handle = camber::http::serve_background(listener, lifecycle_websocket_router());
    let mut pending = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect pending WebSocket peer");
    async_ws_request(&mut pending, "/ws").await;
    wait_for_unacknowledged_upgrade(&controller).await;
    match forced {
        true => handle.cancel(),
        false => runtime::request_shutdown(),
    }
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release pending upgrade into shutdown");
    let mut owner = Box::pin(handle.into_future());
    let response = lifecycle_event("pending-upgrade rejection before owner completion", async {
        tokio::select! {
            biased;
            result = owner.as_mut() => {
                panic!("owner completed before writing upgrade rejection: {result:?}")
            }
            response = read_async_http_head(&mut pending) => response,
        }
    })
    .await;
    let response_lower = response.to_ascii_lowercase();
    assert!(
        response.starts_with("HTTP/1.1 503"),
        "shutdown committed an unexpected upgrade response: {response}"
    );
    assert!(
        response_lower.contains("connection: close"),
        "upgrade rejection omitted Connection: close: {response}"
    );
    assert!(
        !response.contains(" 101 "),
        "shutdown committed a 101 response"
    );
    assert_eof_before_owner(&mut pending, owner.as_mut()).await;
    let result = lifecycle_event("pending direct-upgrade drain", owner.as_mut()).await;
    match forced {
        true => assert_cancelled(result),
        false => assert!(result.is_ok(), "graceful owner returned {result:?}"),
    }
}

// 1.T21, direct WebSocket registrar-cancellation portion.
#[camber::test]
async fn cancelled_pending_direct_upgrade_is_joined_and_connection_local() {
    let callback_count = Arc::new(AtomicUsize::new(0));
    let mut router = Router::new();
    router.ws("/ws", {
        let callback_count = Arc::clone(&callback_count);
        move |_request: &Request, _connection: WsConn| {
            callback_count.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    });
    router.get("/ok", |_request: &Request| async {
        Response::text(200, "ok")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind cancellation listener");
    let addr = listener
        .local_addr()
        .expect("cancellation listener address");
    let controller = lifecycle(addr).expect("install lifecycle controller");
    arm_unacknowledged_upgrade(&controller);
    let handle = camber::http::serve_background(listener, router);
    let mut pending = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect cancellable WebSocket peer");
    async_ws_request(&mut pending, "/ws").await;
    wait_for_unacknowledged_upgrade(&controller).await;
    drop(pending);
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release cancelled registration");

    assert_http_ok(addr, "/ok").await;
    assert_eq!(
        callback_count.load(Ordering::Acquire),
        0,
        "cancelled upgrade reached its WebSocket callback"
    );
    runtime::request_shutdown();
    assert!(
        lifecycle_event(
            "owner join after registrar cancellation",
            handle.into_future()
        )
        .await
        .is_ok()
    );
}

// 1.T21, graceful direct WebSocket rejection portion.
#[camber::test]
async fn graceful_shutdown_rejects_unacknowledged_direct_upgrade() {
    pending_direct_upgrade_shutdown_is_rejected(false).await;
}

// 1.T21, forced direct WebSocket rejection portion.
#[camber::test]
async fn forced_shutdown_rejects_unacknowledged_direct_upgrade() {
    pending_direct_upgrade_shutdown_is_rejected(true).await;
}

// 1.T21, direct WebSocket supervisor-unwind portion.
#[camber::test]
async fn supervisor_unwind_joins_acknowledged_and_pending_direct_upgrades() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unwind listener");
    let addr = listener.local_addr().expect("unwind listener address");
    let controller = lifecycle(addr).expect("install lifecycle controller");
    let handle = camber::http::serve_background(listener, lifecycle_websocket_router());
    let mut acknowledged = connect_async_websocket(addr, "/ws").await;

    arm_unacknowledged_upgrade(&controller);
    let mut pending = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect pending direct upgrade");
    async_ws_request(&mut pending, "/ws").await;
    wait_for_unacknowledged_upgrade(&controller).await;
    controller
        .inject_once(LifecycleFault::PanicSupervisorCore)
        .expect("inject supervisor unwind");
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release supervisor into unwind");

    let mut owner = Box::pin(handle.into_future());
    match read_ws_event_before_owner(&mut acknowledged, owner.as_mut()).await {
        None => {}
        Some((0x8, _)) => {
            write_async_ws_frame(&mut acknowledged, 0x8, &[]).await;
            assert_eof_before_owner(&mut acknowledged, owner.as_mut()).await;
        }
        Some((opcode, payload)) => {
            panic!("supervisor unwind emitted opcode {opcode:#x} with payload {payload:?}")
        }
    }
    let pending_response =
        lifecycle_event("pending unwind response before owner completion", async {
            tokio::select! {
                biased;
                result = owner.as_mut() => {
                    panic!("owner completed before pending wrapper terminated: {result:?}")
                }
                response = read_async_http_head(&mut pending) => response,
            }
        })
        .await;
    assert!(
        pending_response.starts_with("HTTP/1.1 500"),
        "supervisor-unavailable direct upgrade did not return 500: {pending_response}"
    );
    assert!(
        pending_response
            .to_ascii_lowercase()
            .contains("connection: close"),
        "pending unwind response omitted Connection: close: {pending_response}"
    );
    assert!(
        !pending_response.starts_with("HTTP/1.1 101"),
        "supervisor-unavailable direct upgrade committed 101: {pending_response}"
    );
    assert_eof_before_owner(&mut pending, owner.as_mut()).await;
    match lifecycle_event("supervisor unwind drain", owner.as_mut()).await {
        Err(RuntimeError::TaskPanicked(message)) => assert!(!message.is_empty()),
        other => panic!("expected TaskPanicked after upgrade drain, got {other:?}"),
    }
}
