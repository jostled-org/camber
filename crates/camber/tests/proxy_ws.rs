#![cfg(feature = "ws")]

mod common;
mod support;

use camber::http::{self, Request, Response, Router, WsConn};
use camber::runtime;
use std::io::Write;
use std::net::TcpStream;
use std::time::Duration;
use support::ws_helpers::{
    read_until_double_crlf, read_ws_text_frame, write_ws_close_frame, write_ws_text_frame,
};

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
            assert!(
                resp.contains("101"),
                "expected 101 switching protocols: {resp}"
            );

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
            assert!(resp.contains("101"), "expected 101: {resp}");

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
            assert!(
                resp.contains("101"),
                "expected 101 for WS through proxy: {resp}"
            );

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
            assert!(
                resp.contains("403"),
                "expected 403 for cross-host origin on proxied WS, got: {resp}"
            );

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
                 Sec-WebSocket-Protocol: graphql-ws\r\n\
                 \r\n"
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert!(
                resp.contains("101"),
                "expected 101 switching protocols: {resp}"
            );
            // Client should see the subprotocol in the 101 response
            let lower = resp.to_lowercase();
            assert!(
                lower.contains("sec-websocket-protocol: graphql-ws"),
                "expected Sec-WebSocket-Protocol in 101 response: {resp}"
            );

            // Backend should have received the subprotocol and echoed it as a message
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
            assert!(
                resp.contains("101"),
                "expected 101 switching protocols: {resp}"
            );

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
            assert!(
                resp.contains("502"),
                "unsupported scheme should produce 502, got: {resp}"
            );

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
            assert!(
                resp.contains("101"),
                "expected 101 for proxied WS through proxy_stream, got: {resp}"
            );

            write_ws_text_frame(&mut stream, "hello");
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(msg, "hello");

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}
