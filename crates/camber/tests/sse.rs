mod common;

use camber::http::{Request, Router};
use camber::runtime;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::time::Duration;

/// Read SSE events from a buffered reader. Each event ends with a blank line.
/// Skips HTTP chunked transfer encoding framing (hex size lines).
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

        // Skip chunked transfer encoding size lines (hex digits only)
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

/// Skip HTTP response headers, returning the reader positioned at the body.
fn skip_http_headers(reader: &mut BufReader<TcpStream>) {
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.trim().is_empty() {
            break;
        }
    }
}

#[test]
fn sse_streams_multiple_events() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.get_sse(
                "/events",
                |_req: &Request, writer: &mut camber::http::SseWriter| {
                    for i in 0..3 {
                        writer.event("message", &format!("data-{i}"))?;
                    }
                    Ok(())
                },
            );

            let addr = common::spawn_server(router);

            // Use raw TCP to read streaming SSE
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

            // Read and verify status line + headers
            let mut status_line = String::new();
            reader.read_line(&mut status_line).unwrap();
            assert!(
                status_line.starts_with("HTTP/1.1 200"),
                "expected 200, got: {status_line}"
            );

            // Collect headers
            let mut headers = Vec::new();
            let mut line = String::new();
            loop {
                line.clear();
                reader.read_line(&mut line).unwrap();
                let trimmed = line.trim_end();
                match trimmed.is_empty() {
                    true => break,
                    false => headers.push(trimmed.to_owned()),
                }
            }

            let has_event_stream = headers.iter().any(|h| {
                h.to_lowercase().contains("content-type") && h.contains("text/event-stream")
            });
            assert!(
                has_event_stream,
                "missing Content-Type: text/event-stream, got: {headers:?}"
            );

            let has_no_cache = headers
                .iter()
                .any(|h| h.to_lowercase().contains("cache-control") && h.contains("no-cache"));
            assert!(
                has_no_cache,
                "missing Cache-Control: no-cache, got: {headers:?}"
            );

            // Read SSE events
            let events = read_sse_events(&mut reader, 3);
            assert_eq!(events.len(), 3, "expected 3 events, got: {events:?}");

            for (i, event) in events.iter().enumerate() {
                assert_eq!(
                    event,
                    &format!("event: message\ndata: data-{i}"),
                    "event {i} mismatch: {event}"
                );
            }

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn sse_route_ignores_request_body_limit() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new().max_request_body(10);
            router.get_sse(
                "/events",
                |_req: &Request, writer: &mut camber::http::SseWriter| {
                    writer.event("ping", "hello")?;
                    Ok(())
                },
            );

            let addr = common::spawn_server(router);

            // Send a body larger than max_request_body to the SSE route.
            // Head-only dispatch skips body collection, so 413 is not returned.
            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let body = "x".repeat(1024);
            write!(
                stream,
                "GET /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body,
            )
            .unwrap();
            stream.flush().unwrap();

            let mut reader = BufReader::new(stream);

            // Read and verify status line
            let mut status_line = String::new();
            reader.read_line(&mut status_line).unwrap();
            assert!(
                status_line.starts_with("HTTP/1.1 200"),
                "expected 200, got: {status_line}"
            );

            // Verify content type
            let mut headers_text = String::new();
            loop {
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let trimmed = line.trim_end();
                match trimmed.is_empty() {
                    true => break,
                    false => {
                        headers_text.push_str(trimmed);
                        headers_text.push('\n');
                    }
                }
            }
            let lower = headers_text.to_lowercase();
            assert!(
                lower.contains("text/event-stream"),
                "expected text/event-stream, got headers: {headers_text}"
            );

            // Read the SSE event
            let events = read_sse_events(&mut reader, 1);
            assert_eq!(events.len(), 1);
            assert_eq!(events[0], "event: ping\ndata: hello");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn sse_client_disconnect_stops_handler() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let event_count = Arc::new(AtomicUsize::new(0));
            let handler_count = Arc::clone(&event_count);

            let mut router = Router::new();
            router.get_sse(
                "/stream",
                move |_req: &Request, writer: &mut camber::http::SseWriter| {
                    loop {
                        handler_count.fetch_add(1, Ordering::SeqCst);
                        match writer.event("tick", "ping") {
                            Ok(()) => {}
                            Err(_) => return Ok(()),
                        }
                        std::thread::sleep(Duration::from_millis(50));
                    }
                },
            );

            let addr = common::spawn_server(router);

            // Connect and read 2 events, then drop
            {
                let mut stream = TcpStream::connect(addr).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                write!(
                    stream,
                    "GET /stream HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
                )
                .unwrap();
                stream.flush().unwrap();

                let mut reader = BufReader::new(stream);
                skip_http_headers(&mut reader);
                let events = read_sse_events(&mut reader, 2);
                assert_eq!(events.len(), 2, "expected 2 events before disconnect");
            }
            // stream dropped here — handler should detect write error

            // Wait for handler to notice disconnect
            std::thread::sleep(Duration::from_millis(300));

            // Handler should have stopped (not writing indefinitely)
            let final_count = event_count.load(Ordering::SeqCst);
            // Give it one more sleep to confirm it stopped
            std::thread::sleep(Duration::from_millis(200));
            let after_wait = event_count.load(Ordering::SeqCst);
            assert_eq!(
                final_count, after_wait,
                "handler should have stopped writing events"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}
