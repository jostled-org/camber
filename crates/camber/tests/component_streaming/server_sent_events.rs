use crate::runtime_support as common;

use camber::http::mock::InboundTerminal;
use camber::http::{Request, Router, TransferBudget};
use camber::runtime;
use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

/// The payload maximum the multi-event row's router names.
///
/// Above what the feed publishes: the claim is which policy an SSE registration
/// that named none inherited, not a crossing.
const FEED_MAX_BYTES: usize = 256;
/// The payload three `data-N` events add up to.
///
/// Each is `event: message\ndata: data-N\n\n`: twenty-nine bytes of framed feed.
const FEED_BYTES: usize = 87;

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

            // 11.T1: an SSE registration that names no budget of its own is
            // bounded by its router's download policy, and the events the peer
            // reads are what that owner admitted.
            let inherited = TransferBudget::unbounded()
                .with_max_bytes(FEED_MAX_BYTES)
                .expect("the router's download maximum is accepted");
            let port = crate::http::reserve_transfer_owner();
            let server = port.serve(router.download_budget(inherited));
            let addr = server.addr();

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

            let row = "a multi-event feed";
            let observed = crate::stream_support::released_download(server.controller(), row);
            assert_eq!(
                observed.download.max_bytes,
                Some(FEED_MAX_BYTES),
                "{row}: the registration inherited the router's maximum: {observed:?}"
            );
            assert_eq!(
                observed.download.admitted_bytes, FEED_BYTES,
                "{row}: every event the peer read was admitted by the owner: {observed:?}"
            );
            assert_eq!(
                observed.download.crossings_released, 0,
                "{row}: a feed under its maximum releases nothing"
            );
            assert_eq!(
                observed.download.terminal,
                Some(InboundTerminal::ResponseHead),
                "{row}: the producer's own end is the terminal: {observed:?}"
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
            let port = crate::http::reserve_request_body_owner();
            let asked = Arc::new(AtomicUsize::new(0));
            let mut router = Router::new().max_request_body(10);
            router.get_sse(
                "/events",
                |_req: &Request, writer: &mut camber::http::SseWriter| {
                    writer.event("ping", "hello")?;
                    Ok(())
                },
            );
            let router = router.body_admission(crate::http::refusing_body_admission(&asked));

            let server = port.serve(router);
            let addr = server.addr();

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

            assert_eq!(
                asked.load(Ordering::SeqCst),
                0,
                "an SSE route is bodyless, so no body policy is asked about it"
            );
            let body = server.controller().observed();
            assert_eq!(body.frames_polled, 0);
            assert_eq!(body.peak_retained_bytes, 0);
            assert_eq!(body.permit_owners_dropped, 0);

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

            let port = crate::http::reserve_transfer_owner();
            let server = port.serve(router);
            let addr = server.addr();

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

            // 11.T2: the writer the handler lost is the source one production
            // download owner released, and it released it exactly once.
            let row = "a departed feed peer";
            let observed = crate::stream_support::released_download(server.controller(), row);
            assert_eq!(
                observed.download.releases, 1,
                "{row}: the owner released its source and producer once: {observed:?}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}
