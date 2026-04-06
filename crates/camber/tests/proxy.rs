mod common;

use camber::http::{self, Request, Response, Router};
use camber::{runtime, spawn_async};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[camber::test]
async fn proxy_forwards_get_request() {
    let mut backend = Router::new();
    backend.get("/hello", |_req: &Request| async {
        Response::text(200, "from-backend")
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let resp = http::get(&format!("http://{main_addr}/api/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "from-backend");

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_forwards_post_with_body() {
    let mut backend = Router::new();
    backend.post("/echo", |req: &Request| {
        let body = req.body().to_owned();
        async move { Response::text(200, &body) }
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let resp = http::post(&format!("http://{main_addr}/api/echo"), "request-body")
        .await
        .unwrap();
    assert_eq!(resp.body(), "request-body");

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_forwards_headers() {
    let mut backend = Router::new();
    backend.get("/check", |req: &Request| {
        let value = req
            .headers()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-custom"))
            .map(|(_, v)| v)
            .unwrap_or("missing")
            .to_owned();
        async move { Response::text(200, &value) }
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    // Use raw TCP to verify header forwarding end-to-end.
    let mut stream = std::net::TcpStream::connect(main_addr).unwrap();
    stream
        .write_all(
            b"GET /api/check HTTP/1.1\r\nHost: localhost\r\nX-Custom: test-value\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    assert!(buf.contains("test-value"), "response was: {buf}");

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_returns_502_on_backend_failure() {
    let mut main = Router::new();
    main.proxy("/api", "http://127.0.0.1:1");
    let main_addr = common::spawn_server(main);

    let resp = http::get(&format!("http://{main_addr}/api/anything"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_coexists_with_normal_routes() {
    let mut backend = Router::new();
    backend.get("/hello", |_req: &Request| async {
        Response::text(200, "proxied")
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.get("/health", |_req: &Request| async {
        Response::text(200, "ok")
    });
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let health = http::get(&format!("http://{main_addr}/health"))
        .await
        .unwrap();
    assert_eq!(health.status(), 200);
    assert_eq!(health.body(), "ok");

    let proxied = http::get(&format!("http://{main_addr}/api/hello"))
        .await
        .unwrap();
    assert_eq!(proxied.status(), 200);
    assert_eq!(proxied.body(), "proxied");

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_round_trips_binary_data() {
    let mut backend = Router::new();
    backend.post("/echo", |req: &Request| {
        let data = req.body_bytes().to_vec();
        async move {
            Response::bytes(200, data).map(|r| r.with_content_type("application/octet-stream"))
        }
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    // Build 256 bytes: 0x00..0xFF
    let binary_body: Vec<u8> = (0..=255u8).collect();
    let content_length = binary_body.len();

    let mut stream = std::net::TcpStream::connect(main_addr).unwrap();
    let header = format!(
        "POST /api/echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: {content_length}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(header.as_bytes()).unwrap();
    stream.write_all(&binary_body).unwrap();

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).unwrap();

    // Parse HTTP response to extract body bytes
    let raw_str = String::from_utf8_lossy(&raw);
    let body_start = raw_str.find("\r\n\r\n").expect("no header/body separator") + 4;
    let response_body = &raw[body_start..];
    assert_eq!(
        response_body, &binary_body,
        "binary data corrupted through proxy round-trip"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_streams_large_response() {
    let mut backend = Router::new();
    backend.get("/large", |_req: &Request| async {
        // 1MB response body: repeated pattern
        let pattern = b"abcdefghij";
        let mut data = Vec::with_capacity(1_000_000);
        while data.len() < 1_000_000 {
            let remaining = 1_000_000 - data.len();
            let chunk = if remaining >= pattern.len() {
                pattern.as_slice()
            } else {
                &pattern[..remaining]
            };
            data.extend_from_slice(chunk);
        }
        Response::bytes(200, data).map(|r| r.with_content_type("application/octet-stream"))
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let resp = http::get(&format!("http://{main_addr}/api/large"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body_bytes().len(), 1_000_000);

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_preserves_backend_content_type() {
    let mut backend = Router::new();
    backend.get("/data", |_req: &Request| async {
        Response::bytes(200, vec![1, 2, 3]).map(|r| r.with_content_type("application/octet-stream"))
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let mut stream = std::net::TcpStream::connect(main_addr).unwrap();
    stream
        .write_all(b"GET /api/data HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    let lower = buf.to_lowercase();
    assert!(
        lower.contains("content-type: application/octet-stream"),
        "expected content-type header, got: {buf}"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_strips_upgrade_headers_from_backend() {
    let mut backend = Router::new();
    backend.get("/with-upgrade", |_req: &Request| async {
        Response::text(200, "ok").map(|r| r.with_header("Upgrade", "h2c"))
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    // Use raw TCP to inspect response headers
    let mut stream = std::net::TcpStream::connect(main_addr).unwrap();
    stream
        .write_all(
            b"GET /api/with-upgrade HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    let lower = buf.to_lowercase();
    assert!(
        !lower.contains("upgrade:"),
        "proxy should strip Upgrade header from backend response, got: {buf}"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_async_concurrent_requests() {
    // 4 concurrent 200ms requests. Proxy uses block_in_place at the IO
    // boundary, allowing Tokio to schedule other connections on separate
    // worker threads. All 4 overlap → total < 900ms.
    let mut backend = Router::new();
    backend.get("/slow", |_req: &Request| async {
        std::thread::sleep(Duration::from_millis(200));
        Response::text(200, "done")
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let start = Instant::now();
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let addr = main_addr;
            spawn_async(async move {
                let resp = http::get(&format!("http://{addr}/api/slow")).await.unwrap();
                assert_eq!(resp.status(), 200);
                assert_eq!(resp.body(), "done");
            })
        })
        .collect();

    for handle in handles {
        handle.await.unwrap();
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(900),
        "4 concurrent 200ms requests took {elapsed:?}, expected < 900ms (async overlap, serial would be 800ms+)"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_concurrent_requests_still_work() {
    // Verify concurrent proxy requests complete correctly. Under inline
    // dispatch, proxy handlers call block_in_place at the IO boundary,
    // allowing Tokio to process other connections concurrently.
    let mut backend = Router::new();
    backend.get("/slow", |_req: &Request| async {
        std::thread::sleep(Duration::from_millis(50));
        Response::text(200, "done")
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let addr = main_addr;
            spawn_async(async move {
                let resp = http::get(&format!("http://{addr}/api/slow")).await.unwrap();
                assert_eq!(resp.status(), 200);
                assert_eq!(resp.body(), "done");
            })
        })
        .collect();

    for handle in handles {
        handle.await.unwrap();
    }

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_strips_hop_by_hop_headers() {
    let mut backend = Router::new();
    backend.get("/check", |req: &Request| {
        // Report which hop-by-hop headers the backend received
        let mut found = Vec::new();
        for (name, _) in req.headers() {
            let lower = name.to_ascii_lowercase();
            match lower.as_str() {
                "connection" | "keep-alive" | "transfer-encoding" => {
                    found.push(lower);
                }
                _ => {}
            }
        }
        async move {
            match found.is_empty() {
                true => Response::text(200, "none"),
                false => Response::text(200, &found.join(",")),
            }
        }
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    // Send request with hop-by-hop headers via raw TCP
    let mut stream = std::net::TcpStream::connect(main_addr).unwrap();
    stream
        .write_all(
            b"GET /api/check HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\nKeep-Alive: timeout=5\r\nX-Custom: pass-through\r\n\r\n",
        )
        .unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    // The backend should not have received Connection or Keep-Alive
    assert!(
        buf.contains("none"),
        "backend should not receive hop-by-hop headers, got: {buf}"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn auth_middleware_blocks_unauthenticated_proxy() {
    let mut backend = Router::new();
    backend.get("/hello", |_req: &Request| async {
        Response::text(200, "from-backend")
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.use_middleware(|req, next| {
        let has_auth = req
            .headers()
            .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
        match has_auth {
            true => next.call(req),
            false => Box::pin(async { Response::text(401, "unauthorized").expect("valid status") })
                as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
        }
    });
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    // No auth header → 401 (not proxied)
    let resp = http::get(&format!("http://{main_addr}/api/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 401);
    assert_eq!(resp.body(), "unauthorized");

    // With auth header → 200 (proxied)
    let mut stream = std::net::TcpStream::connect(main_addr).unwrap();
    stream
        .write_all(b"GET /api/hello HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer token\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    assert!(
        buf.starts_with("HTTP/1.1 200"),
        "expected 200 with auth header, got: {buf}"
    );
    assert!(
        buf.contains("from-backend"),
        "expected proxied body, got: {buf}"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn logging_middleware_captures_proxy_status() {
    let logged_status = Arc::new(AtomicUsize::new(0));
    let mw_status = Arc::clone(&logged_status);

    let mut backend = Router::new();
    backend.get("/hello", |_req: &Request| async {
        Response::text(200, "ok")
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.use_middleware(move |req, next| {
        let mw_status = Arc::clone(&mw_status);
        let resp_fut = next.call(req);
        Box::pin(async move {
            let resp = resp_fut.await;
            mw_status.store(resp.status() as usize, Ordering::SeqCst);
            resp
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>
    });
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let resp = http::get(&format!("http://{main_addr}/api/hello"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        logged_status.load(Ordering::SeqCst),
        200,
        "middleware should have captured the proxy response status"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_concurrent_streaming() {
    let mut backend = Router::new();
    backend.get("/slow", |_req: &Request| async {
        std::thread::sleep(Duration::from_millis(100));
        let data = vec![0xABu8; 10_000];
        Response::bytes(200, data).map(|r| r.with_content_type("application/octet-stream"))
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let handles: Vec<_> = (0..4)
        .map(|_| {
            let addr = main_addr;
            spawn_async(async move {
                let resp = http::get(&format!("http://{addr}/api/slow")).await.unwrap();
                assert_eq!(resp.status(), 200);
                assert_eq!(resp.body_bytes().len(), 10_000);
            })
        })
        .collect();

    for handle in handles {
        handle.await.unwrap();
    }

    runtime::request_shutdown();
}

#[camber::test]
async fn buffered_proxy_still_materializes_response_body() {
    let mut backend = Router::new();
    backend.get("/data", |_req: &Request| async {
        Response::text(200, "fully-buffered-body")
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let resp = http::get(&format!("http://{main_addr}/api/data"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.body(),
        "fully-buffered-body",
        "buffered proxy must return the full response body"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_strips_client_forwarded_headers_before_adding_own() {
    let mut backend = Router::new();
    backend.get("/check-fwd", |req: &Request| {
        // Collect all X-Forwarded-For values the backend sees
        let values: Vec<String> = req
            .headers()
            .filter(|(k, _)| k.eq_ignore_ascii_case("x-forwarded-for"))
            .map(|(_, v)| v.to_owned())
            .collect();
        async move { Response::text(200, &values.join(",")) }
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    // Send request with a spoofed X-Forwarded-For header via raw TCP
    let mut stream = std::net::TcpStream::connect(main_addr).unwrap();
    stream
        .write_all(
            b"GET /api/check-fwd HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-For: 6.6.6.6\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();

    // The backend must NOT see the spoofed 6.6.6.6 — only Camber's value (127.0.0.1)
    let body_start = buf.find("\r\n\r\n").unwrap() + 4;
    let body = &buf[body_start..];
    assert!(
        !body.contains("6.6.6.6"),
        "client-supplied X-Forwarded-For should be stripped, got: {body}"
    );
    assert!(
        body.contains("127.0.0.1"),
        "Camber should add its own X-Forwarded-For, got: {body}"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_strips_client_x_forwarded_proto() {
    let backend_addr = proto_echo_backend();

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    // Send request with spoofed X-Forwarded-Proto: https through an HTTP proxy
    let mut stream = std::net::TcpStream::connect(main_addr).unwrap();
    stream
        .write_all(
            b"GET /api/check-proto HTTP/1.1\r\nHost: localhost\r\nX-Forwarded-Proto: https\r\nConnection: close\r\n\r\n",
        )
        .unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();

    let body_start = buf.find("\r\n\r\n").unwrap() + 4;
    let body = &buf[body_start..];
    assert_eq!(
        body.trim(),
        "http",
        "Camber should replace spoofed X-Forwarded-Proto with its own (http), got: {body}"
    );

    runtime::request_shutdown();
}

/// Backend that echoes the X-Forwarded-Proto header value.
fn proto_echo_backend() -> std::net::SocketAddr {
    let mut backend = Router::new();
    backend.get("/check-proto", |req: &Request| {
        let proto = req
            .headers()
            .find(|(k, _)| k.eq_ignore_ascii_case("x-forwarded-proto"))
            .map(|(_, v)| v)
            .unwrap_or("missing")
            .to_owned();
        async move { Response::text(200, &proto) }
    });
    common::spawn_server(backend)
}

#[camber::test]
async fn buffered_proxy_forwards_x_forwarded_proto_http() {
    let backend_addr = proto_echo_backend();

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    // Plain HTTP listener → upstream should see X-Forwarded-Proto: http
    let resp = http::get(&format!("http://{main_addr}/api/check-proto"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.body(),
        "http",
        "expected X-Forwarded-Proto: http for plain HTTP proxy"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn tls_proxy_forwards_x_forwarded_proto_https() {
    let backend_addr = proto_echo_backend();

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));

    let (cert_pem, key_pem) = common::generate_self_signed_cert();
    let tls_config = common::build_server_config(&cert_pem, &key_pem);

    let tls_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let tls_addr = tls_listener.local_addr().unwrap();

    let _handle = http::serve_background_tls(tls_listener, main, tls_config);

    // Build a reqwest client that trusts the self-signed cert
    let client_config = common::tls_client_config(&[&cert_pem]);
    let client = reqwest::ClientBuilder::new()
        .use_preconfigured_tls(client_config)
        .build()
        .unwrap();

    let resp = client
        .get(format!(
            "https://localhost:{}/api/check-proto",
            tls_addr.port()
        ))
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(
        resp.text().await.unwrap(),
        "https",
        "expected X-Forwarded-Proto: https for TLS proxy"
    );

    runtime::request_shutdown();
}
