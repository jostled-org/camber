#![cfg(feature = "ws")]

mod common;
mod support;

use camber::http::{self, Request, Response, Router, SseWriter, WsConn};
use camber::runtime;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;
use support::ws_helpers::{
    read_until_double_crlf, read_ws_text_frame, write_ws_close_frame, write_ws_text_frame,
};

#[camber::test]
async fn single_server_rest_sse_websocket() {
    let mut router = Router::new();

    // Middleware: add X-Server header to all responses
    router.use_middleware(|req: &Request, next| {
        let fut = next.call(req);
        Box::pin(async move { fut.await.with_header("X-Server", "camber") })
    });

    // REST endpoint
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "hello")
    });

    // SSE endpoint: sends 3 events
    router.get_sse("/events", |_req: &Request, writer: &mut SseWriter| {
        for i in 0..3 {
            writer.event("message", &format!("event-{i}"))?;
        }
        Ok(())
    });

    // WebSocket endpoint: echo
    router.ws("/ws", |_req: &Request, mut conn: WsConn| {
        while let Some(msg) = conn.recv() {
            if conn.send(&msg).is_err() {
                break;
            }
        }
        Ok(())
    });

    let addr = common::spawn_server(router);

    // --- REST ---
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello");
    let has_server_header = resp
        .headers()
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("x-server") && v.as_ref() == "camber");
    assert!(
        has_server_header,
        "missing X-Server header on REST response, got: {:?}",
        resp.headers()
    );

    // --- SSE ---
    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    stream.flush().unwrap();

    let mut reader = BufReader::new(stream);

    // Verify status line
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    assert!(
        status_line.starts_with("HTTP/1.1 200"),
        "SSE expected 200, got: {status_line}"
    );

    // Skip headers to reach body
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
    }

    // Read 3 SSE events
    let events = read_sse_events(&mut reader, 3);
    assert_eq!(events.len(), 3, "expected 3 SSE events, got: {events:?}");
    for (i, event) in events.iter().enumerate() {
        assert_eq!(
            event,
            &format!("event: message\ndata: event-{i}"),
            "SSE event {i} mismatch"
        );
    }

    // --- WebSocket ---
    let mut ws_stream = TcpStream::connect(addr).unwrap();
    ws_stream
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
    ws_stream.write_all(upgrade_req.as_bytes()).unwrap();

    let ws_resp = read_until_double_crlf(&mut ws_stream);
    assert!(
        ws_resp.contains("101"),
        "expected 101 switching protocols: {ws_resp}"
    );

    // Echo test
    write_ws_text_frame(&mut ws_stream, "ping");
    let msg = read_ws_text_frame(&mut ws_stream);
    assert_eq!(msg, "ping", "WebSocket echo failed");

    // Close
    write_ws_close_frame(&mut ws_stream);

    // --- Clean shutdown ---
    runtime::request_shutdown();
}

// --- SSE helpers ---

fn read_sse_events(reader: &mut BufReader<TcpStream>, count: usize) -> Vec<String> {
    let mut events = Vec::new();
    let mut current = String::new();
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        let trimmed = line.trim_end_matches(&['\r', '\n'][..]);

        // Skip chunked transfer encoding size lines
        if !trimmed.is_empty() && trimmed.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }

        match trimmed.is_empty() {
            true if !current.is_empty() => {
                events.push(std::mem::take(&mut current));
                if events.len() >= count {
                    break;
                }
            }
            true => {}
            false => {
                if !current.is_empty() {
                    current.push('\n');
                }
                current.push_str(trimmed);
            }
        }
    }
    events
}
