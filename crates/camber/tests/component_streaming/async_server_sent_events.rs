//! Async SSE registrations: `post_sse`, `get_sse_async`, and their budgeted
//! forms, and the SSE responses a stream handler starts with
//! `StreamResponse::sse`.
//!
//! The handler reads the request, then returns a future that owns an
//! [`AsyncSseWriter`]. The feed ends when that future ends, when the peer
//! leaves, or when the feed's download policy ends it.

use crate::runtime_support as common;

use camber::http::mock::InboundTerminal;
use camber::http::{AsyncSseWriter, Request, Router, SseWriter, StreamResponse, TransferBudget};
use camber::{RuntimeError, runtime};
use std::future::Future;
use std::io::{BufReader, Write};
use std::net::TcpStream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::time::Duration;

const TIMEOUT: Duration = Duration::from_secs(5);

/// Sends on drop, so a row can see that the runtime dropped a handler future.
struct DropSignal(mpsc::SyncSender<()>);

impl Drop for DropSignal {
    fn drop(&mut self) {
        let _ = self.0.try_send(());
    }
}

fn runtime() -> runtime::RuntimeBuilder {
    common::test_runtime().shutdown_timeout(Duration::from_secs(2))
}

fn get(addr: std::net::SocketAddr, path: &str) -> crate::http::HttpResponse {
    crate::http::request(addr, "GET", path, &[], &[], TIMEOUT).expect("read the SSE response")
}

fn assert_event_stream(response: &crate::http::HttpResponse) {
    assert_eq!(response.status, 200);
    assert_eq!(response.header("content-type"), Some("text/event-stream"));
    assert_eq!(response.header("cache-control"), Some("no-cache"));
}

/// Open a feed and read its head and first event, leaving the peer connected.
fn open_feed(addr: std::net::SocketAddr, path: &str) -> BufReader<TcpStream> {
    let mut stream = TcpStream::connect(addr).unwrap();
    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    stream.flush().unwrap();
    let mut reader = BufReader::new(stream);
    let (status, _) = crate::wire::read_response_head(&mut reader);
    assert_eq!(status, 200);
    crate::wire::read_chunk(&mut reader, 1024)
        .expect("decode the first event")
        .expect("the feed stayed open for its first event");
    reader
}

#[test]
fn async_writer_frames_events_data_and_comments() {
    runtime()
        .run(|| {
            let mut router = Router::new();
            router.get_sse_async(
                "/events",
                |_req: &Request, writer: AsyncSseWriter| async move {
                    writer.event("update", "line-1\nline-2").await?;
                    writer.data("plain").await?;
                    writer.comment().await?;
                    Ok(())
                },
            );
            let addr = common::spawn_server(router);

            let response = get(addr, "/events");
            assert_event_stream(&response);
            assert_eq!(
                response.body.as_ref(),
                b"event: update\ndata: line-1\ndata: line-2\n\ndata: plain\n\n:\n\n"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn blocking_writer_frames_data_only_events() {
    runtime()
        .run(|| {
            let mut router = Router::new();
            router.get_sse("/events", |_req: &Request, writer: &mut SseWriter| {
                writer.data("first\nsecond")?;
                Ok(())
            });
            let addr = common::spawn_server(router);

            let response = get(addr, "/events");
            assert_event_stream(&response);
            assert_eq!(response.body.as_ref(), b"data: first\ndata: second\n\n");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn post_feed_reads_the_request_body() {
    runtime()
        .run(|| {
            let mut router = Router::new();
            router.post_sse("/jobs", |req: &Request, writer: AsyncSseWriter| {
                let job: Box<str> = req.body().into();
                async move {
                    for step in ["queued", "running", "done"] {
                        writer.event(step, &job).await?;
                    }
                    Ok(())
                }
            });
            let addr = common::spawn_server(router);

            let response = crate::http::request(addr, "POST", "/jobs", &[], b"job-7", TIMEOUT)
                .expect("read the POST feed");
            assert_event_stream(&response);
            assert_eq!(
                response.body.as_ref(),
                b"event: queued\ndata: job-7\n\nevent: running\ndata: job-7\n\nevent: done\ndata: job-7\n\n"
            );
            let refused = get(addr, "/jobs");
            assert_eq!(refused.status, 405, "post_sse registers POST only");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn post_feed_body_over_the_route_maximum_is_refused_before_the_feed() {
    runtime()
        .run(|| {
            let entered = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&entered);
            let mut router = Router::new().max_request_body(10);
            router.post_sse("/jobs", move |_req: &Request, writer: AsyncSseWriter| {
                counted.fetch_add(1, Ordering::SeqCst);
                async move { writer.data("never").await }
            });
            let addr = common::spawn_server(router);

            let body = "x".repeat(1024);
            let response =
                crate::http::request(addr, "POST", "/jobs", &[], body.as_bytes(), TIMEOUT)
                    .expect("read the refusal");
            assert_eq!(response.status, 413);
            assert_eq!(entered.load(Ordering::SeqCst), 0, "no handler ran");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn get_feed_publishes_from_an_async_source() {
    runtime()
        .run(|| {
            let mut router = Router::new();
            router.get_sse_async(
                "/ticks",
                |_req: &Request, writer: AsyncSseWriter| async move {
                    let (tx, mut rx) = tokio::sync::mpsc::channel::<u32>(1);
                    tokio::spawn(async move {
                        for tick in 0..3 {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            if tx.send(tick).await.is_err() {
                                return;
                            }
                        }
                    });
                    while let Some(tick) = rx.recv().await {
                        writer.event("tick", &tick.to_string()).await?;
                    }
                    Ok(())
                },
            );
            let addr = common::spawn_server(router);

            let response = get(addr, "/ticks");
            assert_event_stream(&response);
            assert_eq!(
                response.body.as_ref(),
                b"event: tick\ndata: 0\n\nevent: tick\ndata: 1\n\nevent: tick\ndata: 2\n\n"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn handler_error_ends_the_feed_after_the_sent_events() {
    runtime()
        .run(|| {
            let mut router = Router::new();
            router.get_sse_async(
                "/events",
                |_req: &Request, writer: AsyncSseWriter| async move {
                    writer.event("partial", "1").await?;
                    Err(RuntimeError::InvalidArgument("the source failed".into()))
                },
            );
            let addr = common::spawn_server(router);

            let response = get(addr, "/events");
            assert_event_stream(&response);
            assert_eq!(response.body.as_ref(), b"event: partial\ndata: 1\n\n");
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn peer_disconnect_drops_a_waiting_handler() {
    runtime()
        .run(|| {
            let (dropped_tx, dropped_rx) = mpsc::sync_channel(1);
            let mut router = Router::new();
            router.get_sse_async("/wait", move |_req: &Request, writer: AsyncSseWriter| {
                let signal = DropSignal(dropped_tx.clone());
                async move {
                    let _signal = signal;
                    writer.event("ready", "1").await?;
                    // Waits on nothing the writer can see, so only the
                    // runtime can end it when the peer leaves.
                    std::future::pending::<()>().await;
                    Ok(())
                }
            });
            let addr = common::spawn_server(router);

            drop(open_feed(addr, "/wait"));
            dropped_rx
                .recv_timeout(TIMEOUT)
                .expect("the departed peer dropped the waiting handler");
            runtime::request_shutdown();
        })
        .unwrap();
}

/// A server stop releases a feed whose peer stays connected at the server's own
/// shutdown deadline, as it releases any stream. Shorter than the runtime's,
/// so the release, not the runtime's drain timeout, is what ends the feed.
const SERVER_SHUTDOWN: Duration = Duration::from_millis(200);

#[test]
fn shutdown_drops_a_handler_that_never_ends() {
    let (dropped_tx, dropped_rx) = mpsc::sync_channel(1);
    runtime()
        .run(move || {
            let mut router = Router::new();
            router.get_sse_async("/wait", move |_req: &Request, writer: AsyncSseWriter| {
                let signal = DropSignal(dropped_tx.clone());
                async move {
                    let _signal = signal;
                    writer.event("ready", "1").await?;
                    std::future::pending::<()>().await;
                    Ok(())
                }
            });
            let policy = camber::http::ServerPolicy::default()
                .shutdown_timeout(SERVER_SHUTDOWN)
                .expect("the server deadline is accepted");
            let addr = common::spawn_server_with_policy(router, policy);
            let peer = open_feed(addr, "/wait");
            runtime::request_shutdown();
            peer
        })
        .unwrap();
    dropped_rx
        .recv_timeout(TIMEOUT)
        .expect("shutdown dropped the handler that never ended");
}

#[test]
fn head_runs_no_handler() {
    runtime()
        .run(|| {
            let entered = Arc::new(AtomicUsize::new(0));
            let counted = Arc::clone(&entered);
            let mut router = Router::new();
            router.get_sse_async("/events", move |_req: &Request, writer: AsyncSseWriter| {
                counted.fetch_add(1, Ordering::SeqCst);
                async move { writer.data("only for GET").await }
            });
            let addr = common::spawn_server(router);

            let mut stream = TcpStream::connect(addr).unwrap();
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            write!(
                stream,
                "HEAD /events HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            let (status, _) = crate::wire::read_response_head(&mut BufReader::new(stream));
            assert_eq!(status, 200);

            let response = get(addr, "/events");
            assert_eq!(response.body.as_ref(), b"data: only for GET\n\n");
            assert_eq!(
                entered.load(Ordering::SeqCst),
                1,
                "only the GET ran the handler"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn budgeted_feeds_end_at_their_own_maximum() {
    runtime()
        .run(|| {
            // Two events of `data: 0123456789\n\n` are 36 bytes; the third crosses.
            let budget = TransferBudget::unbounded()
                .with_max_bytes(36)
                .expect("the feed maximum is accepted");
            let feed = |_req: &Request, writer: AsyncSseWriter| async move {
                for _ in 0..3 {
                    writer.data("0123456789").await?;
                }
                std::future::pending::<()>().await;
                Ok(())
            };

            for (method, path) in [("GET", "/get"), ("POST", "/post")] {
                let port = crate::http::reserve_transfer_owner();
                let mut routed = Router::new();
                match method {
                    "GET" => routed.get_sse_async_with_budget(path, budget, feed),
                    _ => routed.post_sse_with_budget(path, budget, feed),
                }
                let server = port.serve(routed);
                let mut reader = {
                    let mut stream = TcpStream::connect(server.addr()).unwrap();
                    stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                    write!(
                        stream,
                        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\n\r\n"
                    )
                    .unwrap();
                    BufReader::new(stream)
                };
                let (status, _) = crate::wire::read_response_head(&mut reader);
                assert_eq!(status, 200, "{method}: the feed committed its 200");

                let row = "an async feed's own maximum";
                let observed = crate::stream_support::released_download(server.controller(), row);
                assert_eq!(
                    (observed.download.terminal, observed.download.max_bytes),
                    (Some(InboundTerminal::TransferBytes), Some(36)),
                    "{method}: the registration's maximum ended the feed: {observed:?}"
                );
                assert_eq!(
                    observed.download.admitted_bytes, 36,
                    "{method}: the two events under the maximum were admitted"
                );
            }
            runtime::request_shutdown();
        })
        .unwrap();
}

type StreamFuture = Pin<Box<dyn Future<Output = StreamResponse> + Send>>;

/// A `post_stream` handler that answers `400` for an empty body, and otherwise
/// starts an SSE feed that echoes the body.
fn choose_stream_or_error(req: &Request) -> StreamFuture {
    let job: Box<str> = req.body().into();
    Box::pin(async move {
        if job.is_empty() {
            let (response, sender) = StreamResponse::new(400);
            let _ = sender.send(&br#"{"error":"empty job"}"#[..]).await;
            return response.with_header("Content-Type", "application/json");
        }
        let (response, writer) = StreamResponse::sse();
        tokio::spawn(async move {
            writer.event("queued", &job).await?;
            writer.data("done").await?;
            writer.comment().await
        });
        response.with_header("X-Request-Id", "req-7")
    })
}

#[test]
fn stream_handler_answers_an_error_before_any_feed() {
    runtime()
        .run(|| {
            let mut router = Router::new();
            router.post_stream("/jobs", choose_stream_or_error);
            let addr = common::spawn_server(router);

            let response = crate::http::request(addr, "POST", "/jobs", &[], b"", TIMEOUT)
                .expect("read the refusal");
            assert_eq!(response.status, 400);
            assert_eq!(response.header("content-type"), Some("application/json"));
            assert_eq!(response.body.as_ref(), br#"{"error":"empty job"}"#);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_handler_starts_an_sse_feed_it_chose() {
    runtime()
        .run(|| {
            let mut router = Router::new();
            router.post_stream("/jobs", choose_stream_or_error);
            let addr = common::spawn_server(router);

            let response = crate::http::request(addr, "POST", "/jobs", &[], b"job-7", TIMEOUT)
                .expect("read the feed");
            assert_event_stream(&response);
            assert_eq!(response.header("x-request-id"), Some("req-7"));
            assert_eq!(
                response.body.as_ref(),
                b"event: queued\ndata: job-7\n\ndata: done\n\n:\n\n"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_sse_feed_ends_at_its_own_maximum() {
    runtime()
        .run(|| {
            // Two events of `data: 0123456789\n\n` are 36 bytes; the third crosses.
            let budget = TransferBudget::unbounded()
                .with_max_bytes(36)
                .expect("the feed maximum is accepted");
            let port = crate::http::reserve_transfer_owner();
            let mut router = Router::new();
            router.get_stream("/feed", move |_req: &Request| -> StreamFuture {
                Box::pin(async move {
                    let (response, writer) = StreamResponse::sse_with_budget(4, budget)
                        .expect("the feed buffer is accepted");
                    tokio::spawn(async move {
                        for _ in 0..3 {
                            writer.data("0123456789").await?;
                        }
                        std::future::pending::<()>().await;
                        Ok::<(), RuntimeError>(())
                    });
                    response
                })
            });
            let server = port.serve(router);
            let mut reader = {
                let mut stream = TcpStream::connect(server.addr()).unwrap();
                stream.set_read_timeout(Some(TIMEOUT)).unwrap();
                write!(stream, "GET /feed HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
                BufReader::new(stream)
            };
            let (status, _) = crate::wire::read_response_head(&mut reader);
            assert_eq!(status, 200);

            let row = "a stream handler's SSE maximum";
            let observed = crate::stream_support::released_download(server.controller(), row);
            assert_eq!(
                (observed.download.terminal, observed.download.max_bytes),
                (Some(InboundTerminal::TransferBytes), Some(36)),
                "the response's own maximum ended the feed: {observed:?}"
            );
            assert_eq!(observed.download.admitted_bytes, 36);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn stream_sse_refuses_a_zero_buffer() {
    let refused = StreamResponse::sse_with_budget(0, TransferBudget::unbounded());
    assert!(
        matches!(refused, Err(RuntimeError::InvalidArgument(_))),
        "a zero buffer is refused: {refused:?}"
    );
}
