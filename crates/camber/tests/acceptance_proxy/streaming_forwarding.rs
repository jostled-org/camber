use crate::common;

use camber::http::{Request, Response, Router, StreamResponse};
use camber::runtime;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

type ChunkPermits = Arc<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<()>>>>;
type ProducerResultReceiver = mpsc::Receiver<Result<(), &'static str>>;

const LARGE_STREAM_CHUNKS: usize = 10;
const LARGE_STREAM_CHUNK_BYTES: usize = 100_000;

#[derive(Clone, Copy)]
enum ProxyRegistration {
    Buffered,
    Streaming,
}

fn chunk_permit_channel() -> (tokio::sync::mpsc::UnboundedSender<()>, ChunkPermits) {
    let (sender, receiver) = tokio::sync::mpsc::unbounded_channel();
    (sender, Arc::new(Mutex::new(Some(receiver))))
}

fn take_chunk_permits(permits: &ChunkPermits) -> tokio::sync::mpsc::UnboundedReceiver<()> {
    permits
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .expect("the test upstream serves one streaming request")
}

fn release_chunks(sender: &tokio::sync::mpsc::UnboundedSender<()>, count: usize) {
    (0..count)
        .try_for_each(|_| sender.send(()))
        .expect("streaming upstream still waits for chunk permits");
}

fn large_stream_backend(
    byte: u8,
) -> (
    std::net::SocketAddr,
    tokio::sync::mpsc::UnboundedSender<()>,
    ProducerResultReceiver,
) {
    let (chunk_permit_tx, chunk_permits) = chunk_permit_channel();
    let (producer_result_tx, producer_result_rx) = mpsc::sync_channel(1);
    let mut backend = Router::new();
    backend.get_stream("/data", move |_req: &Request| {
        let mut chunk_permits = take_chunk_permits(&chunk_permits);
        let producer_result_tx = producer_result_tx.clone();
        Box::pin(async move {
            let (response, sender) = StreamResponse::new(200);
            tokio::spawn(async move {
                let chunk = vec![byte; LARGE_STREAM_CHUNK_BYTES];
                for _ in 0..LARGE_STREAM_CHUNKS {
                    match chunk_permits.recv().await {
                        Some(()) => {}
                        None => {
                            producer_result_tx
                                .send(Err("chunk permit channel closed"))
                                .expect("streaming test still observes producer completion");
                            return;
                        }
                    }
                    match sender.send(chunk.clone()).await {
                        Ok(()) => {}
                        Err(_) => return,
                    }
                }
                producer_result_tx
                    .send(Ok(()))
                    .expect("streaming test still observes producer completion");
            });
            response
        })
    });
    (
        common::spawn_server(backend),
        chunk_permit_tx,
        producer_result_rx,
    )
}

fn large_response_proxy(
    backend_addr: std::net::SocketAddr,
    registration: ProxyRegistration,
) -> std::net::SocketAddr {
    let mut proxy = Router::new();
    let backend = format!("http://{backend_addr}");
    match registration {
        ProxyRegistration::Buffered => proxy.proxy("/api", &backend),
        ProxyRegistration::Streaming => proxy.proxy_stream("/api", &backend),
    }
    common::spawn_server(proxy)
}

fn receive_large_proxy_response(
    proxy_addr: std::net::SocketAddr,
    chunk_permits: &tokio::sync::mpsc::UnboundedSender<()>,
    producer_result: &ProducerResultReceiver,
) -> Vec<u8> {
    let mut stream = TcpStream::connect(proxy_addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "GET /api/data HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    stream.flush().unwrap();
    release_chunks(chunk_permits, LARGE_STREAM_CHUNKS);
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    producer_result
        .recv_timeout(Duration::from_secs(5))
        .expect("streaming producer reports bounded completion")
        .expect("streaming producer completed without synchronization failure");
    response
}

fn assert_large_proxy_response(response: &[u8], byte: u8) {
    let header_end = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("no header/body separator");
    let header = String::from_utf8_lossy(&response[..header_end]);
    assert!(
        header.starts_with("HTTP/1.1 200"),
        "expected 200, got: {header}"
    );
    // The proxy delivers the body with chunked transfer-encoding, so the raw
    // response interleaves chunk-size lines with payload. Count matching bytes
    // instead of comparing lengths.
    let byte_count = response[header_end + 4..]
        .iter()
        .filter(|candidate| **candidate == byte)
        .count();
    assert!(
        byte_count >= LARGE_STREAM_CHUNKS * LARGE_STREAM_CHUNK_BYTES,
        "expected at least {} bytes of {}, got {byte_count}",
        LARGE_STREAM_CHUNKS * LARGE_STREAM_CHUNK_BYTES,
        char::from(byte)
    );
}

fn run_large_response_case(registration: ProxyRegistration, byte: u8) {
    let (backend_addr, chunk_permits, producer_result) = large_stream_backend(byte);
    let proxy_addr = large_response_proxy(backend_addr, registration);
    let response = receive_large_proxy_response(proxy_addr, &chunk_permits, &producer_result);
    assert_large_proxy_response(&response, byte);
    runtime::request_shutdown();
}

#[test]
fn proxy_streams_large_response() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| run_large_response_case(ProxyRegistration::Buffered, b'A'))
        .unwrap();
}

#[test]
fn proxy_preserves_status_and_headers() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let mut backend = Router::new();
            backend.get_stream("/check", |_req: &Request| {
                Box::pin(async {
                    let (resp, sender) = StreamResponse::new(201);
                    let resp = resp.with_header("X-Upstream", "present");

                    tokio::spawn(async move {
                        let _ = sender.send("ok").await;
                    });

                    resp
                })
            });
            let backend_addr = common::spawn_server(backend);

            let mut proxy = Router::new();
            proxy.proxy("/api", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "GET /api/check HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();

            let mut reader = BufReader::new(stream);
            let mut status_line = String::new();
            reader.read_line(&mut status_line).unwrap();
            assert!(
                status_line.starts_with("HTTP/1.1 201"),
                "expected 201, got: {status_line}"
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

            let has_upstream_header = headers
                .iter()
                .any(|h| h.to_lowercase().starts_with("x-upstream") && h.contains("present"));
            assert!(
                has_upstream_header,
                "missing X-Upstream header, got: {headers:?}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn proxy_handles_upstream_error_mid_stream() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            // Backend sends 2 chunks then panics (simulating crash)
            let (chunk_permit_tx, chunk_permits) = chunk_permit_channel();
            let (producer_result_tx, producer_result_rx) = mpsc::sync_channel(1);
            let mut backend = Router::new();
            backend.get_stream("/fail", move |_req: &Request| {
                let mut chunk_permits = take_chunk_permits(&chunk_permits);
                let producer_result_tx = producer_result_tx.clone();
                Box::pin(async move {
                    let (resp, sender) = StreamResponse::new(200);

                    tokio::spawn(async move {
                        for i in 0..2 {
                            match chunk_permits.recv().await {
                                Some(()) => {}
                                None => {
                                    producer_result_tx
                                        .send(Err("chunk permit channel closed"))
                                        .expect(
                                            "streaming test still observes producer completion",
                                        );
                                    return;
                                }
                            }
                            if sender.send(format!("chunk-{i}")).await.is_err() {
                                return;
                            }
                        }
                        producer_result_tx
                            .send(Ok(()))
                            .expect("streaming test still observes producer completion");
                        // Drop sender abruptly — simulates upstream error
                    });

                    resp
                })
            });
            let backend_addr = common::spawn_server(backend);

            let mut proxy = Router::new();
            proxy.proxy("/api", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "GET /api/fail HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();
            release_chunks(&chunk_permit_tx, 2);

            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).unwrap();
            producer_result_rx
                .recv_timeout(Duration::from_secs(5))
                .expect("streaming producer reports bounded completion")
                .expect("streaming producer completed without synchronization failure");
            let response = String::from_utf8_lossy(&buf);

            assert!(
                response.starts_with("HTTP/1.1 200"),
                "expected 200, got start: {}",
                &response[..response.len().min(80)]
            );
            assert!(
                response.contains("chunk-0"),
                "expected at least first chunk in response"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

// ── Streaming proxy tests (proxy_stream) ─────────────────────────

#[test]
fn proxy_stream_forwards_large_response_incrementally() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| run_large_response_case(ProxyRegistration::Streaming, b'B'))
        .unwrap();
}

#[test]
fn proxy_stream_preserves_status_and_headers() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let mut backend = Router::new();
            backend.get_stream("/check", |_req: &Request| {
                Box::pin(async {
                    let (resp, sender) = StreamResponse::new(201);
                    let resp = resp.with_header("X-Upstream", "present");

                    tokio::spawn(async move {
                        let _ = sender.send("ok").await;
                    });

                    resp
                })
            });
            let backend_addr = common::spawn_server(backend);

            let mut proxy = Router::new();
            proxy.proxy_stream("/api", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            write!(
                stream,
                "GET /api/check HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();

            let mut reader = BufReader::new(stream);
            let mut status_line = String::new();
            reader.read_line(&mut status_line).unwrap();
            assert!(
                status_line.starts_with("HTTP/1.1 201"),
                "expected 201, got: {status_line}"
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

            let has_upstream_header = headers
                .iter()
                .any(|h| h.to_lowercase().starts_with("x-upstream") && h.contains("present"));
            assert!(
                has_upstream_header,
                "missing X-Upstream header, got: {headers:?}"
            );

            // Verify no upstream hop-by-hop headers leak through
            assert!(
                !headers
                    .iter()
                    .any(|h| h.to_lowercase().starts_with("proxy-connection:")),
                "proxy-connection header should be stripped, got: {headers:?}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

/// The host whose child router owns the streaming route under test.
const STREAM_HOST: &str = "stream.test";
/// The observed-byte ceiling that child forwards under.
const STREAM_CEILING: usize = 100;

fn streaming_limit_hosts(
    backend: &str,
    drops: &Arc<std::sync::atomic::AtomicUsize>,
    mapped: &common::Journal,
) -> camber::http::HostRouter {
    let mut child = Router::new();
    child.proxy_stream("/api", backend);
    let permits = Arc::clone(drops);
    let mut hosts = camber::http::HostRouter::new();
    hosts.add(
        STREAM_HOST,
        child
            .max_request_body(STREAM_CEILING)
            .body_admission(move |_context: &camber::http::BodyAdmissionContext<'_>| {
                Ok(camber::http::BodyAdmission::with_permit(
                    STREAM_CEILING,
                    common::permit_probe(&permits),
                ))
            })
            .rejection_mapper(common::recording_mapper(mapped, "child")),
    );
    hosts
}

#[test]
fn proxy_stream_crossing_frame_is_not_forwarded_and_maps_body_limit_once() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let upstream =
                common::raw_upstream(200, "upstream-answered", common::UpstreamAnswers::OnBodyEnd);
            let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let mapped: common::Journal = Arc::new(Mutex::new(Vec::new()));
            let port = common::reserve_observed();
            let hosts = streaming_limit_hosts(&upstream.backend(), &drops, &mapped);
            let server = port.serve_hosts(hosts);

            let admitted = [b'a'; STREAM_CEILING - 8];
            let crossing = [b'x'; STREAM_CEILING];
            let mut peer = common::connect(server.addr()).expect("the paced peer connected");
            common::write_chunked_head(
                &mut peer,
                common::KEEP_CONNECTION,
                "POST",
                "/api/echo",
                STREAM_HOST,
            )
            .expect("the upload head reached the proxy");
            common::tolerate_dead_socket(common::write_chunk(&mut peer, &admitted))
                .expect("the admitted frame reached the proxy");
            // Read the admitted frame off the upstream before the crossing one
            // is written, rather than both at the end. The outbound leg buffers
            // a forwarded frame and flushes it when the next poll finds nothing
            // ready, so a crossing frame the proxy can reach in that same poll
            // aborts the request with the admitted bytes still in the client's
            // write buffer — the upstream then holds neither frame, and the
            // claim below reads a positive control that the peer's own pacing,
            // not the proxy, decided.
            let admitted_forwarded =
                common::poll_until(Duration::from_secs(5), || upstream.forwarded(&admitted));
            assert!(
                admitted_forwarded,
                "every frame inside the bound reached the upstream"
            );
            common::tolerate_dead_socket(common::write_chunk(&mut peer, &crossing))
                .expect("the crossing frame reached the proxy");

            let refused = common::read_http_response_bounded(&mut peer)
                .expect("the crossing frame was answered");
            assert_eq!(
                refused.status, 413,
                "a streaming upload is bounded by the limit its route admitted"
            );
            let upstream_settled =
                common::poll_until(Duration::from_secs(5), || upstream.dropped() == 1);
            assert!(
                upstream_settled,
                "the refused upstream leg closed before its byte record was read"
            );
            assert!(
                !upstream.forwarded(&crossing),
                "the crossing frame is forwarded to no upstream"
            );

            let seen = common::only(&mapped, "streaming crossing");
            assert_eq!(seen.kind, camber::http::RejectionKind::BodyLimit);
            assert_eq!(seen.route.as_deref(), Some("/api/*proxy_path"));
            assert_eq!(seen.origin, "child", "the selected child's mapper answered");
            common::assert_request_id_shape(Some(&seen.request_id), "streaming crossing");

            let released = common::poll_until(Duration::from_secs(5), || {
                drops.load(std::sync::atomic::Ordering::Acquire) == 1
            });
            assert!(released, "the admitted permit is released exactly once");
            assert_eq!(server.controller().body_permit_owners_dropped(), 1);

            let reused =
                common::probe_connection_reuse(&mut peer, "POST", "/api/again", &[], b"probe");
            assert!(
                matches!(reused, Ok(None) | Err(_)),
                "payload left unread must not leave the connection reusable: {reused:?}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn buffered_proxy_still_enforces_request_body_limit() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            // Same backend
            let mut backend = Router::new();
            backend.post("/echo", |req: &Request| {
                let len = req.body_bytes().len();
                async move { Response::text(200, &len.to_string()) }
            });
            let backend_addr = common::spawn_server(backend);

            // Proxy with small body limit + buffered proxy
            let mut proxy = Router::new().max_request_body(100);
            proxy.proxy("/api", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            // Send same oversized body
            let body = vec![b'X'; 1000];
            let resp = common::raw_request_with_body(proxy_addr, "POST", "/api/echo", &[], &body);
            let status = common::status_from_raw(&resp);
            assert_eq!(
                status, 413,
                "buffered proxy should enforce body limit, got: {resp}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn proxy_stream_middleware_can_reject_before_upstream_call() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let (backend_hit_tx, backend_hit_rx) = std::sync::mpsc::channel();

            let mut backend = Router::new();
            backend.get("/anything", move |_req: &Request| {
                backend_hit_tx
                    .send(())
                    .expect("middleware-rejected upstream observer remains active");
                async { Response::text(200, "should-not-reach") }
            });
            let backend_addr = common::spawn_server(backend);

            let mut proxy = Router::new();
            proxy.use_middleware(|req, next| {
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
            proxy.proxy_stream("/api", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let resp = common::raw_request(proxy_addr, "GET", "/api/anything", &[]);
            let status = common::status_from_raw(&resp);
            assert_eq!(status, 401, "expected 401, got: {status}");
            assert!(
                resp.contains("unauthorized"),
                "expected unauthorized body, got: {resp}"
            );

            assert!(
                matches!(
                    backend_hit_rx.recv_timeout(Duration::from_millis(50)),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ),
                "backend must not be hit during the post-response observation window"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn proxy_stream_middleware_sees_params_and_remote_addr() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let mut backend = Router::new();
            backend.get("/echo", |_req: &Request| async {
                Response::text(200, "upstream-ok")
            });
            let backend_addr = common::spawn_server(backend);

            let mut proxy = Router::new();
            proxy.use_middleware(|req, next| {
                let path_ok = req.param("proxy_path") == Some("echo");
                let remote_ok = req.remote_addr().is_some();
                match (path_ok, remote_ok) {
                    (true, true) => next.call(req),
                    _ => Box::pin(async {
                        Response::text(460, "missing proxy middleware context")
                            .expect("valid status")
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
                }
            });
            proxy.proxy_stream("/api", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let resp = common::raw_request(proxy_addr, "GET", "/api/echo", &[]);
            let status = common::status_from_raw(&resp);
            assert_eq!(
                status, 200,
                "expected middleware to see params and remote address: {resp}"
            );
            assert!(
                resp.contains("upstream-ok"),
                "expected upstream body, got: {resp}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn streaming_proxy_does_not_turn_truncated_upstream_body_into_clean_chunked_eof() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let upstream_addr = listener.local_addr().unwrap();
            let upstream = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n",
                    )
                    .unwrap();
            });
            let mut proxy = Router::new();
            proxy.proxy_stream("/api", &format!("http://{upstream_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .write_all(
                    b"GET /api/data HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();

            assert!(response.windows(5).any(|window| window == b"hello"));
            assert!(
                !response.ends_with(b"0\r\n\r\n"),
                "a truncated upstream stream was framed as a successful downstream EOF"
            );
            upstream.join().unwrap();
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn streaming_proxy_drops_stalled_upstream_after_downstream_disconnect() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let upstream_addr = listener.local_addr().unwrap();
            let (closed_tx, closed_rx) = mpsc::sync_channel(1);
            let upstream = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n")
                    .unwrap();
                let mut byte = [0_u8; 1];
                closed_tx.send(stream.read(&mut byte)).unwrap();
            });
            let mut proxy = Router::new();
            proxy.proxy_stream("/api", &format!("http://{upstream_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let mut downstream = BufReader::new(TcpStream::connect(proxy_addr).unwrap());
            downstream
                .get_mut()
                .write_all(
                    b"GET /api/data HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            let mut line = String::new();
            loop {
                line.clear();
                downstream.read_line(&mut line).unwrap();
                if line == "\r\n" {
                    break;
                }
            }
            drop(downstream);

            assert_eq!(
                closed_rx
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap()
                    .unwrap(),
                0,
                "upstream connection remained open after downstream disconnect"
            );
            upstream.join().unwrap();
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn non_utf8_connection_value_still_strips_its_valid_named_header() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let mut backend = Router::new();
            backend.get("/headers", |req: &Request| {
                let leaked = req.header("x-remove").is_some();
                async move { Response::text(200, if leaked { "leaked" } else { "stripped" }) }
            });
            let backend_addr = common::spawn_server(backend);
            let mut proxy = Router::new();
            proxy.proxy_stream("/api", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let mut request =
                b"GET /api/headers HTTP/1.1\r\nHost: localhost\r\nConnection: X-Remove, ".to_vec();
            request.push(0xff);
            request.extend_from_slice(b"\r\nX-Remove: first-hop-only\r\nConnection: close\r\n\r\n");
            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream.write_all(&request).unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();

            assert!(
                response.windows(8).any(|window| window == b"stripped"),
                "connection-named header leaked through proxy: {}",
                String::from_utf8_lossy(&response)
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn non_utf8_upstream_connection_value_still_strips_its_named_response_header() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let upstream_addr = listener.local_addr().unwrap();
            let upstream = std::thread::spawn(move || {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                loop {
                    line.clear();
                    reader.read_line(&mut line).unwrap();
                    if line == "\r\n" {
                        break;
                    }
                }
                let mut response =
                    b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: X-Remove, ".to_vec();
                response.push(0xff);
                response.extend_from_slice(b"\r\nX-Remove: first-hop-only\r\n\r\nok");
                stream.write_all(&response).unwrap();
            });
            let mut proxy = Router::new();
            proxy.proxy_stream("/api", &format!("http://{upstream_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            stream
                .write_all(
                    b"GET /api/data HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .unwrap();
            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            let header_end = response
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .unwrap();
            let headers = response[..header_end].to_ascii_lowercase();

            assert!(
                !headers.windows(9).any(|window| window == b"x-remove:"),
                "connection-named upstream header leaked downstream: {}",
                String::from_utf8_lossy(&response)
            );
            upstream.join().unwrap();
            runtime::request_shutdown();
        })
        .unwrap();
}
