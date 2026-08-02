#![cfg(feature = "ws")]

use crate::common;
#[path = "../support/ws_frame_io.rs"]
mod ws_frame_io;
#[path = "../support/ws_text_helpers.rs"]
mod ws_text_helpers;

use camber::http::{self, Request, Response, Router, SseWriter, WsConn};
use camber::runtime;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;
use ws_frame_io::read_until_double_crlf;
use ws_text_helpers::{read_ws_text_frame, write_ws_close_frame, write_ws_text_frame};

fn unified_protocol_router() -> Router {
    let mut router = Router::new();
    router.use_middleware(|req: &Request, next| {
        let fut = next.call(req);
        Box::pin(async move { fut.await.with_header("X-Server", "camber") })
    });
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "hello")
    });
    router.get_sse("/events", |_req: &Request, writer: &mut SseWriter| {
        for i in 0..3 {
            writer.event("message", &format!("event-{i}"))?;
        }
        Ok(())
    });
    router.ws("/ws", |_req: &Request, mut conn: WsConn| {
        while let Some(msg) = conn.recv() {
            if conn.send(&msg).is_err() {
                break;
            }
        }
        Ok(())
    });
    router
}

async fn assert_rest_journey(addr: std::net::SocketAddr) {
    let response = http::get(&format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.body(), "hello");
    let has_server_header = response
        .headers()
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("x-server") && v.as_ref() == "camber");
    assert!(
        has_server_header,
        "missing X-Server header on REST response, got: {:?}",
        response.headers()
    );
}

fn open_sse_stream(addr: std::net::SocketAddr) -> BufReader<TcpStream> {
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
    BufReader::new(stream)
}

fn assert_sse_response_head(reader: &mut BufReader<TcpStream>) {
    let mut status_line = String::new();
    reader.read_line(&mut status_line).unwrap();
    assert_eq!(
        crate::http::status_from_raw(&status_line),
        200,
        "SSE expected 200, got: {status_line}"
    );
    let mut line = String::new();
    loop {
        line.clear();
        reader.read_line(&mut line).unwrap();
        if line.trim().is_empty() {
            break;
        }
    }
}

/// Returns the SSE socket so the caller decides when it closes.
fn assert_sse_journey(addr: std::net::SocketAddr) -> BufReader<TcpStream> {
    let mut reader = open_sse_stream(addr);
    assert_sse_response_head(&mut reader);
    let events = read_sse_events(&mut reader, 3);
    assert_eq!(events.len(), 3, "expected 3 SSE events, got: {events:?}");
    for (i, event) in events.iter().enumerate() {
        assert_eq!(
            event,
            &format!("event: message\ndata: event-{i}"),
            "SSE event {i} mismatch"
        );
    }
    reader
}

/// Returns the WebSocket socket so the caller decides when it closes.
fn assert_websocket_journey(addr: std::net::SocketAddr) -> TcpStream {
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
    assert_eq!(
        crate::http::status_from_raw(&ws_resp),
        101,
        "expected 101 switching protocols: {ws_resp}"
    );
    write_ws_text_frame(&mut ws_stream, "ping");
    let msg = read_ws_text_frame(&mut ws_stream);
    assert_eq!(&*msg, "ping", "WebSocket echo failed");
    write_ws_close_frame(&mut ws_stream);
    ws_stream
}

#[camber::test]
async fn single_server_rest_sse_websocket() {
    let addr = common::spawn_server(unified_protocol_router());
    assert_rest_journey(addr).await;
    // Both sockets stay open across the shutdown request, so shutdown has to
    // drain live client connections rather than an already-idle server.
    let _sse = assert_sse_journey(addr);
    let _ws = assert_websocket_journey(addr);
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

        match (
            trimmed.is_empty(),
            current.is_empty(),
            events.len().saturating_add(1) >= count,
        ) {
            (true, false, true) => {
                events.push(std::mem::take(&mut current));
                break;
            }
            (true, false, false) => events.push(std::mem::take(&mut current)),
            (true, true, _) => {}
            (false, _, _) => append_sse_line(&mut current, trimmed),
        }
    }
    events
}

fn append_sse_line(current: &mut String, line: &str) {
    match current.is_empty() {
        true => {}
        false => current.push('\n'),
    }
    current.push_str(line);
}
