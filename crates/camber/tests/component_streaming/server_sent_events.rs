use crate::runtime_support as common;

use camber::http::{Request, Router};
use camber::runtime;
use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::sync::mpsc;
use std::time::Duration;

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

            let response =
                crate::http::request(addr, "GET", "/events", &[], &[], Duration::from_secs(5))
                    .expect("read complete SSE response");
            assert_eq!(response.status, 200);
            assert_eq!(response.header("content-type"), Some("text/event-stream"));
            assert_eq!(response.header("cache-control"), Some("no-cache"));
            assert_eq!(response.header("transfer-encoding"), Some("chunked"));
            assert_eq!(
                response.body.as_ref(),
                b"event: message\ndata: data-0\n\nevent: message\ndata: data-1\n\nevent: message\ndata: data-2\n\n"
            );

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

            let body = "x".repeat(1024);
            let response = crate::http::request(
                addr,
                "GET",
                "/events",
                &[],
                body.as_bytes(),
                Duration::from_secs(5),
            )
            .expect("read complete SSE response for oversized request body");
            assert_eq!(response.status, 200);
            assert_eq!(response.header("content-type"), Some("text/event-stream"));
            assert_eq!(response.header("cache-control"), Some("no-cache"));
            assert_eq!(response.header("transfer-encoding"), Some("chunked"));
            assert_eq!(response.body.as_ref(), b"event: ping\ndata: hello\n\n");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn sse_client_disconnect_stops_handler() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let (stopped_tx, stopped_rx) = mpsc::sync_channel(1);

            let mut router = Router::new();
            router.get_sse(
                "/stream",
                move |_req: &Request, writer: &mut camber::http::SseWriter| {
                    loop {
                        match writer.event("tick", "ping") {
                            Ok(()) => {}
                            Err(_) => {
                                stopped_tx.send(()).unwrap();
                                return Ok(());
                            }
                        }
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
                let (status, _) = crate::wire::read_response_head(&mut reader);
                assert_eq!(status, 200);
                for _ in 0..2 {
                    let event = crate::wire::read_chunk(&mut reader, 1024)
                        .expect("decode bounded SSE chunk")
                        .expect("SSE stream remained open for two events");
                    assert_eq!(event.as_ref(), b"event: tick\ndata: ping\n\n");
                }
            }
            stopped_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("SSE handler observed client disconnect");

            runtime::request_shutdown();
        })
        .unwrap();
}
