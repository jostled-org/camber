#![cfg(feature = "ws")]

use crate::common;
#[path = "../support/ws_frame_io.rs"]
mod ws_frame_io;
#[path = "../support/ws_text_helpers.rs"]
mod ws_text_helpers;

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleFault, lifecycle};
use camber::http::{self, Request, Response, Router, WsConn};
use camber::runtime;
use futures_util::{FutureExt, SinkExt, StreamExt};
use std::future::{Future, IntoFuture};
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use ws_frame_io::read_until_double_crlf;
use ws_text_helpers::{read_ws_text_frame, write_ws_close_frame, write_ws_text_frame};

const LIFECYCLE_EVENT_TIMEOUT: Duration = Duration::from_secs(5);

fn raw_http_status(response: &str) -> u16 {
    response
        .split("\r\n")
        .next()
        .and_then(|status_line| status_line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("invalid HTTP response head: {response:?}"))
}

async fn lifecycle_event<F>(context: &str, future: F) -> F::Output
where
    F: Future,
{
    tokio::time::timeout(LIFECYCLE_EVENT_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
}

fn proxy_upgrade_request(path: &str) -> String {
    format!(
        "GET {path} HTTP/1.1\r\n\
         Host: localhost\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    )
}

async fn send_async_proxy_upgrade(stream: &mut tokio::net::TcpStream) {
    tokio::io::AsyncWriteExt::write_all(stream, proxy_upgrade_request("/ws/echo").as_bytes())
        .await
        .expect("write proxied WebSocket upgrade request");
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

async fn connect_async_proxy_websocket(addr: std::net::SocketAddr) -> tokio::net::TcpStream {
    let mut stream = lifecycle_event(
        "proxied WebSocket TCP connection",
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .expect("connect proxied WebSocket peer");
    send_async_proxy_upgrade(&mut stream).await;
    let response = read_async_http_head(&mut stream).await;
    assert_eq!(
        raw_http_status(&response),
        101,
        "expected proxied WebSocket upgrade, got: {response}"
    );
    stream
}

async fn read_async_ws_frame_or_eof(stream: &mut tokio::net::TcpStream) -> Option<(u8, Vec<u8>)> {
    lifecycle_event("proxied WebSocket frame or EOF", async {
        let mut header = [0u8; 2];
        let first = tokio::io::AsyncReadExt::read(stream, &mut header[..1])
            .await
            .expect("read proxied WebSocket frame header");
        if first == 0 {
            return None;
        }
        tokio::io::AsyncReadExt::read_exact(stream, &mut header[1..])
            .await
            .expect("read proxied WebSocket frame header");
        assert_eq!(header[1] & 0x80, 0, "server frame must not be masked");

        let length = match header[1] & 0x7f {
            126 => {
                let mut extended = [0u8; 2];
                tokio::io::AsyncReadExt::read_exact(stream, &mut extended)
                    .await
                    .expect("read proxied WebSocket frame length");
                u16::from_be_bytes(extended) as usize
            }
            127 => {
                let mut extended = [0u8; 8];
                tokio::io::AsyncReadExt::read_exact(stream, &mut extended)
                    .await
                    .expect("read proxied WebSocket frame length");
                usize::try_from(u64::from_be_bytes(extended))
                    .expect("proxied WebSocket frame length fits usize")
            }
            length => length as usize,
        };
        let mut payload = vec![0u8; length];
        tokio::io::AsyncReadExt::read_exact(stream, &mut payload)
            .await
            .expect("read proxied WebSocket frame payload");
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
        .expect("write proxied WebSocket frame");
}

async fn assert_proxy_echo(stream: &mut tokio::net::TcpStream) {
    write_async_ws_frame(stream, 0x1, b"bridge-live").await;
    let (opcode, payload) = read_async_ws_frame_or_eof(stream)
        .await
        .expect("proxy echo arrives before EOF");
    assert_eq!(opcode, 0x1, "expected proxied WebSocket text frame");
    assert_eq!(payload, b"bridge-live");
}

async fn assert_transport_eof(stream: &mut tokio::net::TcpStream) {
    lifecycle_event("proxied transport EOF", async {
        let mut byte = [0u8; 1];
        match tokio::io::AsyncReadExt::read(stream, &mut byte).await {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            Ok(count) => panic!("expected transport EOF, read {count} bytes"),
            Err(error) => panic!("read transport EOF: {error}"),
        }
    })
    .await;
}

async fn assert_http_ok(addr: std::net::SocketAddr, path: &str) {
    let mut stream = lifecycle_event(
        "proxy HTTP probe connection",
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .expect("connect proxy HTTP probe");
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    tokio::io::AsyncWriteExt::write_all(&mut stream, request.as_bytes())
        .await
        .expect("write proxy HTTP probe");
    let response = read_async_http_head(&mut stream).await;
    assert_eq!(
        raw_http_status(&response),
        200,
        "expected successful proxy HTTP probe, got: {response}"
    );
}

struct LifecycleWsBackend {
    addr: std::net::SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl LifecycleWsBackend {
    async fn shutdown(self) {
        let _ = self.shutdown.send(());
        lifecycle_event("lifecycle WebSocket backend shutdown", self.task)
            .await
            .expect("join lifecycle WebSocket backend");
    }
}

async fn spawn_lifecycle_ws_backend(connection_count: Arc<AtomicUsize>) -> LifecycleWsBackend {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind lifecycle WebSocket backend");
    let addr = listener
        .local_addr()
        .expect("lifecycle WebSocket backend address");
    let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let accepted = tokio::select! {
            biased;
            _ = &mut shutdown_rx => return,
            accepted = listener.accept() => accepted.expect("accept lifecycle backend peer"),
        };
        let mut websocket = tokio_tungstenite::accept_async(accepted.0)
            .await
            .expect("accept lifecycle backend WebSocket");
        connection_count.fetch_add(1, Ordering::AcqRel);

        loop {
            let message = tokio::select! {
                biased;
                _ = &mut shutdown_rx => break,
                message = websocket.next() => message,
            };
            let message = match message {
                Some(Ok(message)) => message,
                Some(Err(_)) | None => break,
            };
            let closes = message.is_close();
            if websocket.send(message).await.is_err() || closes {
                break;
            }
        }
    });
    LifecycleWsBackend {
        addr,
        shutdown,
        task,
    }
}

fn lifecycle_proxy_router(backend_addr: std::net::SocketAddr) -> Router {
    let mut proxy = Router::new();
    proxy.proxy("/ws", &format!("http://{backend_addr}"));
    proxy
}

fn assert_cancelled(result: Result<(), RuntimeError>) {
    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}

#[test]
fn websocket_proxy_forwards_text_messages() {
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

            // Proxy: forward /ws/* to backend
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
            assert_eq!(raw_http_status(&resp), 101, "response: {resp}");

            // Send "hello", expect "hello" back
            write_ws_text_frame(&mut stream, "hello");
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(msg, "hello");

            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_proxy_handles_client_close() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: sends 3 messages then waits
            let mut backend = Router::new();
            backend.ws("/chat", |_req: &Request, mut conn: WsConn| {
                conn.send("one")?;
                conn.send("two")?;
                conn.send("three")?;
                // Wait for client to close
                let _ = conn.recv();
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            let mut proxy = Router::new();
            proxy.proxy("/ws", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let key = "dGhlIHNhbXBsZSBub25jZQ==";
            let upgrade_req = format!(
                "GET /ws/chat HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(raw_http_status(&resp), 101, "response: {resp}");

            // Receive 3 messages
            let m1 = read_ws_text_frame(&mut stream);
            let m2 = read_ws_text_frame(&mut stream);
            let m3 = read_ws_text_frame(&mut stream);
            assert_eq!(
                [m1.as_str(), m2.as_str(), m3.as_str()],
                ["one", "two", "three"]
            );

            // Client sends close — proxy should clean up without panic
            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_proxy_coexists_with_http_proxy() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: serves both HTTP and WebSocket
            let mut backend = Router::new();
            backend.get("/hello", |_req: &Request| async {
                Response::text(200, "http-ok")
            });
            backend.ws("/echo", |_req: &Request, mut conn: WsConn| {
                while let Some(msg) = conn.recv() {
                    if conn.send(&msg).is_err() {
                        break;
                    }
                }
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            // Single proxy prefix handles both HTTP and WS
            let mut proxy = Router::new();
            proxy.proxy("/api", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            // HTTP GET through proxy
            let resp =
                common::block_on(http::get(&format!("http://{proxy_addr}/api/hello"))).unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.body(), "http-ok");

            // WebSocket upgrade through same proxy prefix
            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();

            let key = "dGhlIHNhbXBsZSBub25jZQ==";
            let upgrade_req = format!(
                "GET /api/echo HTTP/1.1\r\n\
             Host: localhost\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(raw_http_status(&resp), 101, "response: {resp}");

            write_ws_text_frame(&mut stream, "ping");
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(msg, "ping");

            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_proxy_rejects_cross_host_origin_before_upstream_upgrade() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: WebSocket echo server
            let mut backend = Router::new();
            backend.ws("/echo", |_req: &Request, mut conn: WsConn| {
                conn.send("should not reach")?;
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            let mut proxy = Router::new();
            proxy.proxy("/ws", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            // Send proxied WS upgrade with mismatched Origin
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
                 Origin: http://evil.example.com\r\n\
                 Sec-WebSocket-Key: {key}\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(raw_http_status(&resp), 403, "response: {resp}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_proxy_forwards_sec_websocket_protocol() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: echo Sec-WebSocket-Protocol as first WS message
            let mut backend = Router::new();
            backend.ws("/echo", |req: &Request, mut conn: WsConn| {
                let proto = req
                    .headers()
                    .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-protocol"))
                    .map(|(_, v)| v.to_owned())
                    .unwrap_or_else(|| "none".to_owned());
                conn.send(&proto)?;
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            let mut proxy = Router::new();
            proxy.proxy("/ws", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

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
                  Sec-WebSocket-Protocol: graphql-ws, graphql-transport-ws\r\n\
                 \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(raw_http_status(&resp), 101, "response: {resp}");
            // Client should see the subprotocol in the 101 response
            let lower = resp.to_lowercase();
            assert!(
                lower.contains("sec-websocket-protocol: graphql-ws"),
                "expected Sec-WebSocket-Protocol in 101 response: {resp}"
            );

            // The backend receives only the protocol committed to the client.
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(
                msg, "graphql-ws",
                "backend should receive Sec-WebSocket-Protocol header"
            );

            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_proxy_strips_spoofed_forwarded_headers() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut backend = Router::new();
            backend.ws("/echo", |req: &Request, mut conn: WsConn| {
                let forwarded_for = req
                    .headers()
                    .find(|(k, _)| k.eq_ignore_ascii_case("x-forwarded-for"))
                    .map(|(_, v)| v.to_owned())
                    .unwrap_or_else(|| "none".to_owned());
                conn.send(&forwarded_for)?;
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            let mut proxy = Router::new();
            proxy.proxy("/ws", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

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
                 X-Forwarded-For: 6.6.6.6\r\n\
                 \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(raw_http_status(&resp), 101, "response: {resp}");

            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(msg, "none", "spoofed forwarding header reached backend");

            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_proxy_rejects_invalid_backend_scheme() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend configured with ftp:// — not http:// or https://, should return 502
            let mut proxy = Router::new();
            proxy.proxy("/ws", "ftp://127.0.0.1:1");
            let proxy_addr = common::spawn_server(proxy);

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
            assert_eq!(raw_http_status(&resp), 502, "response: {resp}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_proxy_stream_upgrade_ignores_request_body_limit() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
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

            let mut proxy = Router::new().max_request_body(10);
            proxy.proxy_stream("/ws", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

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
                 Content-Length: 99999\r\n\
                 Sec-WebSocket-Key: {key}\r\n\
                 Sec-WebSocket-Version: 13\r\n\
                 \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(raw_http_status(&resp), 101, "response: {resp}");

            write_ws_text_frame(&mut stream, "hello");
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(msg, "hello");

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

// 1.T9, proxied WebSocket portion.
#[test]
fn proxied_websocket_bridge_holds_permit_and_finishes_before_owned_completion() {
    runtime::builder()
        .connection_limit(1)
        .keepalive_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            runtime::block_on(async {
                let backend_connections = Arc::new(AtomicUsize::new(0));
                let backend = spawn_lifecycle_ws_backend(Arc::clone(&backend_connections)).await;
                let backend_addr = backend.addr;
                let (dispatched_tx, mut dispatched_rx) = tokio::sync::oneshot::channel();
                let dispatched_tx = Arc::new(Mutex::new(Some(dispatched_tx)));
                let mut proxy = lifecycle_proxy_router(backend_addr);
                proxy.get("/second", move |_request: &Request| {
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
                    .expect("bind owned proxy listener");
                let proxy_addr = listener.local_addr().expect("owned proxy listener address");
                let controller = lifecycle(proxy_addr).expect("install proxy lifecycle controller");
                let handle = camber::http::serve_background(listener, proxy);
                let mut websocket = connect_async_proxy_websocket(proxy_addr).await;
                assert_proxy_echo(&mut websocket).await;
                assert_eq!(backend_connections.load(Ordering::Acquire), 1);

                controller
                    .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .expect("pause when the proxy permit wait becomes pending");
                let mut second = tokio::net::TcpStream::connect(proxy_addr)
                    .await
                    .expect("connect permit-waiting proxy peer");
                tokio::io::AsyncWriteExt::write_all(
                    &mut second,
                    b"GET /second HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write permit-waiting proxy request");
                controller
                    .wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .await
                    .expect("proxy semaphore acquisition returned pending");
                assert!(
                    matches!(
                        dispatched_rx.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                    ),
                    "second request dispatched while the proxy bridge held the permit"
                );

                runtime::request_shutdown();
                controller
                    .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .expect("release pending proxy permit wait into shutdown");
                let mut owner = Box::pin(handle.into_future());
                assert!(
                    owner.as_mut().now_or_never().is_none(),
                    "owner completed while the proxy bridge still owned its transport"
                );
                let (opcode, _) = read_async_ws_frame_or_eof(&mut websocket)
                    .await
                    .expect("graceful proxy shutdown sends a close frame");
                assert_eq!(opcode, 0x8, "expected graceful proxied close frame");
                write_async_ws_frame(&mut websocket, 0x8, &[]).await;
                assert_transport_eof(&mut websocket).await;
                assert_transport_eof(&mut second).await;
                assert!(
                    lifecycle_event("owned proxy bridge completion", owner.as_mut())
                        .await
                        .is_ok()
                );
                assert!(
                    matches!(
                        dispatched_rx.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
                    ),
                    "permit-waiting proxy dispatch sender remained live after owner completion"
                );
                backend.shutdown().await;
            });
        })
        .unwrap();
}

// 1.T18, graceful proxied WebSocket portion.
#[camber::test]
async fn graceful_proxy_websocket_shutdown_sends_close_before_eof_and_join() {
    let backend = spawn_lifecycle_ws_backend(Arc::new(AtomicUsize::new(0))).await;
    let backend_addr = backend.addr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind graceful proxy listener");
    let proxy_addr = listener.local_addr().expect("graceful proxy address");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend_addr));
    let mut websocket = connect_async_proxy_websocket(proxy_addr).await;
    assert_proxy_echo(&mut websocket).await;

    runtime::request_shutdown();
    let mut owner = Box::pin(handle.into_future());
    let (opcode, _) = read_async_ws_frame_or_eof(&mut websocket)
        .await
        .expect("graceful proxy shutdown sends a frame before EOF");
    assert_eq!(opcode, 0x8, "expected graceful proxied close frame");
    write_async_ws_frame(&mut websocket, 0x8, &[]).await;
    assert_transport_eof(&mut websocket).await;
    assert!(
        lifecycle_event("graceful proxy bridge join", owner.as_mut())
            .await
            .is_ok()
    );
    backend.shutdown().await;
}

// 1.T18, forced proxied WebSocket portion.
#[camber::test]
async fn forced_proxy_websocket_abort_releases_transport_before_cancelled() {
    let backend = spawn_lifecycle_ws_backend(Arc::new(AtomicUsize::new(0))).await;
    let backend_addr = backend.addr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind forced proxy listener");
    let proxy_addr = listener.local_addr().expect("forced proxy address");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend_addr));
    let mut websocket = connect_async_proxy_websocket(proxy_addr).await;
    assert_proxy_echo(&mut websocket).await;

    handle.cancel();
    let mut owner = Box::pin(handle.into_future());
    match read_async_ws_frame_or_eof(&mut websocket).await {
        None => {}
        Some((0x8, _)) => {
            write_async_ws_frame(&mut websocket, 0x8, &[]).await;
            assert_transport_eof(&mut websocket).await;
        }
        Some((opcode, payload)) => {
            panic!("forced proxy shutdown emitted opcode {opcode:#x} with payload {payload:?}")
        }
    }
    assert_cancelled(lifecycle_event("forced proxy bridge join", owner.as_mut()).await);
    backend.shutdown().await;
}

async fn pending_proxy_upgrade_shutdown_is_rejected(forced: bool) {
    let backend_connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_lifecycle_ws_backend(Arc::clone(&backend_connections)).await;
    let backend_addr = backend.addr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind pending proxy-upgrade listener");
    let proxy_addr = listener.local_addr().expect("pending proxy address");
    let controller = lifecycle(proxy_addr).expect("install pending proxy controller");
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("pause after pending proxy ticket submission");
    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("pause pending proxy upgrade");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend_addr));
    let mut pending = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect pending proxied WebSocket peer");
    send_async_proxy_upgrade(&mut pending).await;
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .await
        .expect("pending proxy ticket reaches the supervisor channel");
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("release submitted pending proxy ticket");
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .await
        .expect("proxy upgrade reaches acknowledgement checkpoint");
    match forced {
        true => handle.cancel(),
        false => runtime::request_shutdown(),
    }
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release pending proxy upgrade into shutdown");
    let mut owner = Box::pin(handle.into_future());
    let response = read_async_http_head(&mut pending).await;
    let status = raw_http_status(&response);
    let response_lower = response.to_ascii_lowercase();
    assert_eq!(
        status, 503,
        "shutdown committed an unexpected proxy upgrade response: {response}"
    );
    assert!(
        response_lower.contains("connection: close"),
        "proxy upgrade rejection omitted Connection: close: {response}"
    );
    assert_ne!(status, 101, "shutdown committed a proxy 101 response");
    assert_eq!(
        backend_connections.load(Ordering::Acquire),
        0,
        "rejected proxy upgrade reached its backend"
    );
    assert_transport_eof(&mut pending).await;
    let result = lifecycle_event("pending proxy-upgrade drain", owner.as_mut()).await;
    match forced {
        true => assert_cancelled(result),
        false => assert!(result.is_ok(), "graceful proxy owner returned {result:?}"),
    }
    backend.shutdown().await;
}

// 1.T21, proxied WebSocket registrar-cancellation portion.
#[camber::test]
async fn cancelled_pending_proxy_upgrade_is_joined_and_connection_local() {
    let backend_connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_lifecycle_ws_backend(Arc::clone(&backend_connections)).await;
    let backend_addr = backend.addr;
    let mut proxy = lifecycle_proxy_router(backend_addr);
    proxy.get("/ok", |_request: &Request| async {
        Response::text(200, "ok")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy cancellation listener");
    let proxy_addr = listener.local_addr().expect("proxy cancellation address");
    let controller = lifecycle(proxy_addr).expect("install proxy cancellation controller");
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("pause after cancellable proxy ticket submission");
    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("pause cancellable proxy upgrade");
    controller
        .pause_once(LifecycleCheckpoint::UpgradePeerClosed)
        .expect("pause after proxy peer closure is observed");
    let handle = camber::http::serve_background(listener, proxy);
    let mut pending = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect cancellable proxy peer");
    send_async_proxy_upgrade(&mut pending).await;
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .await
        .expect("cancellable proxy ticket reaches the supervisor channel");
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("release submitted cancellable proxy ticket");
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .await
        .expect("cancellable proxy upgrade reaches acknowledgement checkpoint");
    drop(pending);
    lifecycle_event(
        "owned reader observation of proxy peer closure",
        controller.wait_until_paused(LifecycleCheckpoint::UpgradePeerClosed),
    )
    .await
    .expect("owned reader observes proxy peer closure");
    controller
        .release(LifecycleCheckpoint::UpgradePeerClosed)
        .expect("release observed proxy peer closure");
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release cancelled proxy registration");

    assert_http_ok(proxy_addr, "/ok").await;
    assert_eq!(
        backend_connections.load(Ordering::Acquire),
        0,
        "cancelled proxy upgrade reached its backend"
    );
    runtime::request_shutdown();
    assert!(
        lifecycle_event(
            "proxy owner join after registrar cancellation",
            handle.into_future()
        )
        .await
        .is_ok()
    );
    backend.shutdown().await;
}

// 1.T21, graceful proxied WebSocket rejection portion.
#[camber::test]
async fn graceful_shutdown_rejects_unacknowledged_proxy_upgrade() {
    pending_proxy_upgrade_shutdown_is_rejected(false).await;
}

// 1.T21, forced proxied WebSocket rejection portion.
#[camber::test]
async fn forced_shutdown_rejects_unacknowledged_proxy_upgrade() {
    pending_proxy_upgrade_shutdown_is_rejected(true).await;
}

// 1.T21, proxied WebSocket supervisor-unwind portion.
#[camber::test]
async fn supervisor_unwind_joins_acknowledged_and_pending_proxy_upgrades() {
    let backend_connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_lifecycle_ws_backend(Arc::clone(&backend_connections)).await;
    let backend_addr = backend.addr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy unwind listener");
    let proxy_addr = listener.local_addr().expect("proxy unwind address");
    let controller = lifecycle(proxy_addr).expect("install proxy unwind controller");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend_addr));
    let mut acknowledged = connect_async_proxy_websocket(proxy_addr).await;
    assert_proxy_echo(&mut acknowledged).await;
    assert_eq!(backend_connections.load(Ordering::Acquire), 1);

    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("pause after second proxy ticket submission");
    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("pause second proxy upgrade");
    let mut pending = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect pending proxy upgrade");
    send_async_proxy_upgrade(&mut pending).await;
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .await
        .expect("second proxy ticket reaches the supervisor channel");
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("release submitted second proxy ticket");
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .await
        .expect("second proxy upgrade reaches acknowledgement checkpoint");
    controller
        .inject_once(LifecycleFault::PanicSupervisorCore)
        .expect("inject proxy supervisor unwind");
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release proxy supervisor into unwind");

    let mut owner = Box::pin(handle.into_future());
    match read_async_ws_frame_or_eof(&mut acknowledged).await {
        None => {}
        Some((0x8, _)) => {
            write_async_ws_frame(&mut acknowledged, 0x8, &[]).await;
            assert_transport_eof(&mut acknowledged).await;
        }
        Some((opcode, payload)) => {
            panic!("proxy supervisor unwind emitted opcode {opcode:#x} with payload {payload:?}")
        }
    }
    let pending_response = read_async_http_head(&mut pending).await;
    let pending_status = raw_http_status(&pending_response);
    assert_eq!(
        pending_status, 500,
        "pending proxy upgrade committed an unexpected response: {pending_response}"
    );
    assert!(
        pending_response
            .to_ascii_lowercase()
            .contains("connection: close"),
        "pending proxy unwind response omitted Connection: close: {pending_response}"
    );
    assert_ne!(
        pending_status, 101,
        "supervisor unwind committed a proxy 101 response"
    );
    assert_eq!(
        backend_connections.load(Ordering::Acquire),
        1,
        "pending proxy upgrade reached its backend during unwind"
    );
    assert_transport_eof(&mut pending).await;
    match lifecycle_event("proxy supervisor unwind drain", owner.as_mut()).await {
        Err(RuntimeError::TaskPanicked(message)) => assert!(!message.is_empty()),
        other => panic!("expected TaskPanicked after proxy upgrade drain, got {other:?}"),
    }
    backend.shutdown().await;
}
