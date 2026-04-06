mod common;

use camber::http::{Request, Response, Router, StreamResponse};
use camber::runtime;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[test]
fn proxy_streams_large_response() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let mut backend = Router::new();
            backend.get_stream("/data", |_req: &Request| {
                Box::pin(async {
                    let (resp, sender) = StreamResponse::new(200);

                    tokio::spawn(async move {
                        let chunk = vec![b'A'; 100_000]; // 100KB
                        for _ in 0..10 {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            if sender.send(chunk.clone()).await.is_err() {
                                return;
                            }
                        }
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
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            write!(
                stream,
                "GET /api/data HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();

            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).unwrap();
            let header_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("no header/body separator");
            let header = String::from_utf8_lossy(&buf[..header_end]);

            assert!(
                header.starts_with("HTTP/1.1 200"),
                "expected 200, got: {header}"
            );

            // Body is 10 * 100KB = 1MB of 'A's (transported via chunked encoding)
            let body = &buf[header_end + 4..];
            let a_count = body.iter().filter(|&&b| b == b'A').count();
            assert!(
                a_count >= 1_000_000,
                "expected at least 1MB of A bytes, got {a_count}"
            );

            runtime::request_shutdown();
        })
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
            let mut backend = Router::new();
            backend.get_stream("/fail", |_req: &Request| {
                Box::pin(async {
                    let (resp, sender) = StreamResponse::new(200);

                    tokio::spawn(async move {
                        for i in 0..2 {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            if sender.send(format!("chunk-{i}")).await.is_err() {
                                return;
                            }
                        }
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

            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).unwrap();
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
        .run(|| {
            let mut backend = Router::new();
            backend.get_stream("/data", |_req: &Request| {
                Box::pin(async {
                    let (resp, sender) = StreamResponse::new(200);

                    tokio::spawn(async move {
                        let chunk = vec![b'B'; 100_000]; // 100KB
                        for _ in 0..10 {
                            tokio::time::sleep(Duration::from_millis(10)).await;
                            if sender.send(chunk.clone()).await.is_err() {
                                return;
                            }
                        }
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
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            write!(
                stream,
                "GET /api/data HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n"
            )
            .unwrap();
            stream.flush().unwrap();

            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).unwrap();
            let header_end = buf
                .windows(4)
                .position(|w| w == b"\r\n\r\n")
                .expect("no header/body separator");
            let header = String::from_utf8_lossy(&buf[..header_end]);

            assert!(
                header.starts_with("HTTP/1.1 200"),
                "expected 200, got: {header}"
            );

            let body = &buf[header_end + 4..];
            let b_count = body.iter().filter(|&&b| b == b'B').count();
            assert!(
                b_count >= 1_000_000,
                "expected at least 1MB of B bytes, got {b_count}"
            );

            runtime::request_shutdown();
        })
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

#[test]
fn proxy_stream_post_ignores_router_body_limit_for_upstream_streaming() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            // Backend echoes body length
            let mut backend = Router::new();
            backend.post("/echo", |req: &Request| {
                let len = req.body_bytes().len();
                async move { Response::text(200, &len.to_string()) }
            });
            let backend_addr = common::spawn_server(backend);

            // Proxy with very small body limit + streaming proxy
            let mut proxy = Router::new().max_request_body(100);
            proxy.proxy_stream("/api", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            // Send POST with body larger than the 100-byte limit
            let body = vec![b'X'; 1000];
            let resp = common::raw_request_with_body(proxy_addr, "POST", "/api/echo", &[], &body);
            let status = common::status_from_raw(&resp);
            assert_eq!(
                status, 200,
                "streaming proxy should bypass body limit, got: {resp}"
            );
            assert!(
                resp.contains("1000"),
                "upstream should receive full 1000 bytes, got: {resp}"
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
            let backend_hit = Arc::new(AtomicBool::new(false));
            let backend_flag = Arc::clone(&backend_hit);

            let mut backend = Router::new();
            backend.get("/anything", move |_req: &Request| {
                backend_flag.store(true, Ordering::SeqCst);
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

            std::thread::sleep(Duration::from_millis(50));
            assert!(
                !backend_hit.load(Ordering::SeqCst),
                "backend should not have been hit when middleware rejects"
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
