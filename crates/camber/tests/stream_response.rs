mod common;

use camber::http::{Request, Router, StreamResponse};
use camber::{RuntimeError, runtime};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[test]
fn stream_response_sends_chunks_incrementally() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.get_stream("/stream", |_req: &Request| {
                Box::pin(async {
                    let (stream_resp, sender) = StreamResponse::new(200);

                    tokio::spawn(async move {
                        for i in 0..3 {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            sender.send(format!("chunk-{i}")).await.unwrap();
                        }
                    });

                    stream_resp
                })
            });

            let addr = common::spawn_server(router);

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

            let mut buf = String::new();
            stream.read_to_string(&mut buf).unwrap();

            assert!(buf.starts_with("HTTP/1.1 200"), "expected 200, got: {buf}");
            assert!(buf.contains("chunk-0"), "missing chunk-0 in: {buf}");
            assert!(buf.contains("chunk-1"), "missing chunk-1 in: {buf}");
            assert!(buf.contains("chunk-2"), "missing chunk-2 in: {buf}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_response_with_custom_headers() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.get_stream("/stream", |_req: &Request| {
                Box::pin(async {
                    let (stream_resp, sender) = StreamResponse::new(200);
                    let stream_resp = stream_resp.with_header("X-Custom", "value");

                    tokio::spawn(async move {
                        sender.send("hello").await.unwrap();
                    });

                    stream_resp
                })
            });

            let addr = common::spawn_server(router);

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
            let mut status_line = String::new();
            reader.read_line(&mut status_line).unwrap();
            assert!(
                status_line.starts_with("HTTP/1.1 200"),
                "expected 200, got: {status_line}"
            );

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

            let has_custom = headers
                .iter()
                .any(|h| h.to_lowercase().starts_with("x-custom") && h.contains("value"));
            assert!(has_custom, "missing X-Custom header, got: {headers:?}");

            let mut body = String::new();
            reader.read_to_string(&mut body).unwrap();
            assert!(body.contains("hello"), "missing body content in: {body}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_response_client_disconnect_drops_sender() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let send_failed = Arc::new(AtomicBool::new(false));
            let send_failed_clone = Arc::clone(&send_failed);

            let mut router = Router::new();
            router.get_stream("/stream", move |_req: &Request| {
                let send_failed = Arc::clone(&send_failed_clone);
                Box::pin(async move {
                    let (stream_resp, sender) = StreamResponse::new(200);

                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            if sender.send("tick").await.is_err() {
                                send_failed.store(true, Ordering::Release);
                                return;
                            }
                        }
                    });

                    stream_resp
                })
            });

            let addr = common::spawn_server(router);

            // Connect, read first chunk, then drop
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
                let mut status_line = String::new();
                reader.read_line(&mut status_line).unwrap();
                assert!(status_line.starts_with("HTTP/1.1 200"));
            }
            // connection dropped

            std::thread::sleep(Duration::from_millis(300));
            assert!(
                send_failed.load(Ordering::Acquire),
                "sender should have failed after client disconnect"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_response_empty_body() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.get_stream("/empty", |_req: &Request| {
                Box::pin(async {
                    let (stream_resp, _sender) = StreamResponse::new(204);
                    // sender dropped immediately — empty body
                    stream_resp
                })
            });

            let addr = common::spawn_server(router);

            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "GET /empty HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();

            let mut buf = String::new();
            stream.read_to_string(&mut buf).unwrap();

            assert!(buf.starts_with("HTTP/1.1 204"), "expected 204, got: {buf}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_response_with_buffer_rejects_zero_capacity() {
    let result = StreamResponse::with_buffer(200, 0);
    match result {
        Err(RuntimeError::InvalidArgument(msg)) => {
            assert!(
                msg.contains("capacity"),
                "error should mention capacity, got: {msg}"
            );
        }
        Err(other) => panic!("expected InvalidArgument, got: {other}"),
        Ok(_) => panic!("expected error for zero capacity"),
    }
}

#[test]
fn stream_response_with_buffer_preserves_streaming_behavior() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.get_stream("/buffered", |_req: &Request| {
                Box::pin(async {
                    let (stream_resp, sender) = StreamResponse::with_buffer(200, 1).unwrap();

                    tokio::spawn(async move {
                        for i in 0..3 {
                            tokio::time::sleep(Duration::from_millis(50)).await;
                            sender.send(format!("chunk-{i}")).await.unwrap();
                        }
                    });

                    stream_resp
                })
            });

            let addr = common::spawn_server(router);

            let mut stream = TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "GET /buffered HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();

            let mut buf = String::new();
            stream.read_to_string(&mut buf).unwrap();

            assert!(buf.starts_with("HTTP/1.1 200"), "expected 200, got: {buf}");
            assert!(buf.contains("chunk-0"), "missing chunk-0 in: {buf}");
            assert!(buf.contains("chunk-1"), "missing chunk-1 in: {buf}");
            assert!(buf.contains("chunk-2"), "missing chunk-2 in: {buf}");

            runtime::request_shutdown();
        })
        .unwrap();
}
