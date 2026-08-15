use crate::common;

use camber::http::{self, Request, Response, Router};
use camber::{runtime, spawn_async};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const OVERLAP_REQUEST_COUNT: usize = 4;
const OVERLAP_TIMEOUT: Duration = Duration::from_secs(2);
const GENERATED_FORWARDING_CASE_COUNT: u64 = 32;
const FIXED_HOP_HEADERS: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

type ReceivedHeaders = Vec<(String, String)>;

fn complete_header_echo_upstream() -> SocketAddr {
    let mut upstream = Router::new();
    upstream.get("/headers", |request: &Request| {
        let received = request
            .headers()
            .map(|(name, value)| format!("{name}:{value}\n"))
            .collect::<String>();
        async move { Response::text(200, &received) }
    });
    common::spawn_server(upstream)
}

fn raw_proxy_header_request(proxy_addr: SocketAddr, request: &str) -> ReceivedHeaders {
    let mut stream = std::net::TcpStream::connect(proxy_addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream.write_all(request.as_bytes()).unwrap();
    stream.flush().unwrap();

    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert_eq!(
        common::status_from_raw(&response),
        200,
        "header-recording upstream request failed: {response}"
    );

    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .expect("response has a header/body separator");
    body.lines()
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

fn header_values<'a>(headers: &'a ReceivedHeaders, expected_name: &str) -> Vec<&'a str> {
    headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(expected_name))
        .map(|(_, value)| value.as_str())
        .collect()
}

fn assert_header_absent(headers: &ReceivedHeaders, name: &str, case: &str) {
    assert!(
        header_values(headers, name).is_empty(),
        "{case}: {name} must not reach upstream; received {headers:?}"
    );
}

fn assert_single_header(headers: &ReceivedHeaders, name: &str, value: &str, case: &str) {
    assert_eq!(
        header_values(headers, name),
        [value],
        "{case}: {name} must appear exactly once with Camber's value; received {headers:?}"
    );
}

fn generated_header_case(source: &str, generator: &mut common::DeterministicCase) -> String {
    source
        .chars()
        .map(
            |character| match (character.is_ascii_alphabetic(), generator.boolean()) {
                (true, true) => character.to_ascii_uppercase(),
                _ => character.to_ascii_lowercase(),
            },
        )
        .collect()
}

fn generated_connection_value(
    tokens: &[&str],
    generator: &mut common::DeterministicCase,
) -> String {
    let separators = [",", ", ", " ,", " , ", "\t,\t", ",\t"];
    tokens
        .iter()
        .enumerate()
        .fold(String::new(), |mut value, (index, token)| {
            match index {
                0 => {}
                _ => value.push_str(
                    generator
                        .select(&separators)
                        .copied()
                        .expect("separator set is not empty"),
                ),
            }
            value.push_str(token);
            value
        })
}

struct GeneratedForwardingCase {
    request: Box<str>,
    first_token: Box<str>,
    second_token: Box<str>,
    end_to_end_value: Box<str>,
    label: Box<str>,
}

fn generated_forwarding_case(
    index: u64,
    generator: &common::DeterministicGenerator,
) -> GeneratedForwardingCase {
    let mut case = generator.case(index);
    let connection_name = generated_header_case("connection", &mut case);
    let first_token = generated_header_case("x-generated-hop-one", &mut case);
    let second_token = generated_header_case("x-generated-hop-two", &mut case);
    let close_token = generated_header_case("close", &mut case);
    let tokens = [
        close_token.as_str(),
        first_token.as_str(),
        second_token.as_str(),
    ];
    let connection_tokens = generated_connection_value(&tokens, &mut case);
    let edge_ows = ["", " ", "\t"];
    let leading_ows = case.select(&edge_ows).copied().expect("non-empty OWS set");
    let trailing_ows = case.select(&edge_ows).copied().expect("non-empty OWS set");
    let connection_value = format!("{leading_ows}{connection_tokens}{trailing_ows}");
    let first_header_name = generated_header_case("x-generated-hop-one", &mut case);
    let second_header_name = generated_header_case("x-generated-hop-two", &mut case);
    let end_to_end_value = format!("preserved-{index}");
    let request = format!(
        concat!(
            "GET /api/headers HTTP/1.1\r\n",
            "Host: client.example\r\n",
            "{connection_name}: {connection_value}\r\n",
            "{first_header_name}: remove-one\r\n",
            "{second_header_name}: remove-two\r\n",
            "Keep-Alive: timeout=5\r\n",
            "Proxy-Authenticate: Basic realm=test\r\n",
            "Proxy-Authorization: Basic dGVzdA==\r\n",
            "Proxy-Connection: keep-alive\r\n",
            "TE: trailers\r\n",
            "Trailer: X-Checksum\r\n",
            "Upgrade: h2c\r\n",
            "Transfer-Encoding: chunked\r\n",
            "X-Forwarded-For: 203.0.113.7\r\n",
            "X-Forwarded-Host: spoofed.example\r\n",
            "X-Forwarded-Proto: https\r\n",
            "X-Real-IP: 198.51.100.8\r\n",
            "Forwarded: for=192.0.2.9;proto=https\r\n",
            "X-End-To-End: {end_to_end_value}\r\n",
            "\r\n",
            "0\r\n\r\n"
        ),
        connection_name = connection_name,
        connection_value = connection_value,
        first_header_name = first_header_name,
        second_header_name = second_header_name,
        end_to_end_value = end_to_end_value,
    );
    GeneratedForwardingCase {
        request: request.into_boxed_str(),
        first_token: first_token.into_boxed_str(),
        second_token: second_token.into_boxed_str(),
        end_to_end_value: end_to_end_value.into_boxed_str(),
        label: case.to_string().into_boxed_str(),
    }
}

fn assert_generated_forwarding_case(received: &ReceivedHeaders, case: &GeneratedForwardingCase) {
    assert_header_absent(received, &case.first_token, &case.label);
    assert_header_absent(received, &case.second_token, &case.label);
    FIXED_HOP_HEADERS
        .iter()
        .for_each(|name| assert_header_absent(received, name, &case.label));
    assert_header_absent(received, "forwarded", &case.label);
    assert_single_header(
        received,
        "x-end-to-end",
        &case.end_to_end_value,
        &case.label,
    );
    assert_single_header(received, "x-forwarded-for", "127.0.0.1", &case.label);
    assert_single_header(received, "x-forwarded-host", "client.example", &case.label);
    assert_single_header(received, "x-forwarded-proto", "http", &case.label);
    assert_single_header(received, "x-real-ip", "127.0.0.1", &case.label);
}

async fn wait_for_request_overlap(gate: &tokio::sync::Barrier) {
    tokio::time::timeout(OVERLAP_TIMEOUT, gate.wait())
        .await
        .expect("all proxy requests must reach the upstream concurrently");
}

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
    let overlap_gate = Arc::new(tokio::sync::Barrier::new(OVERLAP_REQUEST_COUNT + 1));
    let upstream_gate = Arc::clone(&overlap_gate);
    let mut backend = Router::new();
    backend.get("/slow", move |_req: &Request| {
        let request_gate = Arc::clone(&upstream_gate);
        async move {
            request_gate.wait().await;
            Response::text(200, "done")
        }
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let handles: Vec<_> = (0..OVERLAP_REQUEST_COUNT)
        .map(|_| {
            let addr = main_addr;
            spawn_async(async move {
                let resp = http::get(&format!("http://{addr}/api/slow")).await.unwrap();
                assert_eq!(resp.status(), 200);
                assert_eq!(resp.body(), "done");
            })
        })
        .collect();

    wait_for_request_overlap(&overlap_gate).await;

    for handle in handles {
        handle.await.unwrap();
    }

    runtime::request_shutdown();
}

#[camber::test]
async fn proxy_concurrent_requests_still_work() {
    let overlap_gate = Arc::new(tokio::sync::Barrier::new(OVERLAP_REQUEST_COUNT + 1));
    let upstream_gate = Arc::clone(&overlap_gate);
    let mut backend = Router::new();
    backend.get("/slow", move |_req: &Request| {
        let request_gate = Arc::clone(&upstream_gate);
        async move {
            request_gate.wait().await;
            Response::text(200, "done")
        }
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let handles: Vec<_> = (0..OVERLAP_REQUEST_COUNT)
        .map(|_| {
            let addr = main_addr;
            spawn_async(async move {
                let resp = http::get(&format!("http://{addr}/api/slow")).await.unwrap();
                assert_eq!(resp.status(), 200);
                assert_eq!(resp.body(), "done");
            })
        })
        .collect();

    wait_for_request_overlap(&overlap_gate).await;

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
async fn connection_header_tokens_are_removed_before_proxying() {
    let upstream_addr = complete_header_echo_upstream();
    let mut proxy = Router::new();
    proxy.proxy("/api", &format!("http://{upstream_addr}"));
    let proxy_addr = common::spawn_server(proxy);

    let request = concat!(
        "GET /api/headers HTTP/1.1\r\n",
        "Host: client.example\r\n",
        "Connection: close, X-Remove-One, invalid token\r\n",
        "cOnNeCtIoN:\tX-Remove-Two\r\n",
        "X-Remove-One: first-hop-only\r\n",
        "x-remove-two: second-hop-only\r\n",
        "X-End-To-End: preserved\r\n",
        "\r\n"
    );
    let received = raw_proxy_header_request(proxy_addr, request);

    assert_header_absent(&received, "connection", "fixed connection field");
    assert_header_absent(&received, "x-remove-one", "first Connection token");
    assert_header_absent(&received, "x-remove-two", "second Connection token");
    assert_single_header(&received, "x-end-to-end", "preserved", "end-to-end field");

    runtime::request_shutdown();
}

#[camber::test]
async fn connection_header_tokens_are_removed_from_proxy_responses() {
    let mut upstream = Router::new();
    upstream.get("/response-headers", |_request: &Request| async {
        Response::text(200, "ok").map(|response| {
            response
                .with_header("Connection", "X-Response-One, invalid token")
                .with_header("cOnNeCtIoN", "x-response-two")
                .with_header("X-Response-One", "first-hop-only")
                .with_header("X-Response-Two", "second-hop-only")
                .with_header("X-End-To-End", "preserved")
        })
    });
    let upstream_addr = common::spawn_server(upstream);
    let mut proxy = Router::new();
    proxy.proxy("/api", &format!("http://{upstream_addr}"));
    let proxy_addr = common::spawn_server(proxy);

    let response = common::request(
        proxy_addr,
        "GET",
        "/api/response-headers",
        &[("Connection", "close")],
        &[],
        Duration::from_secs(5),
    )
    .unwrap();

    assert_eq!(response.status, 200);
    assert_eq!(response.body.as_ref(), b"ok");
    assert_eq!(response.header("x-end-to-end"), Some("preserved"));
    assert_eq!(response.header("x-response-one"), None);
    assert_eq!(response.header("x-response-two"), None);
    runtime::request_shutdown();
}

#[camber::test]
async fn generated_forwarding_headers_strip_spoofed_and_connection_named_fields() {
    let upstream_addr = complete_header_echo_upstream();
    let mut proxy = Router::new();
    proxy.proxy("/api", &format!("http://{upstream_addr}"));
    let proxy_addr = common::spawn_server(proxy);
    let generator = common::DeterministicGenerator::stable();

    for index in 0..GENERATED_FORWARDING_CASE_COUNT {
        let case = generated_forwarding_case(index, &generator);
        let received = raw_proxy_header_request(proxy_addr, &case.request);
        assert_generated_forwarding_case(&received, &case);
    }

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
    assert_eq!(
        common::status_from_raw(&buf),
        200,
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
    let overlap_gate = Arc::new(tokio::sync::Barrier::new(OVERLAP_REQUEST_COUNT + 1));
    let upstream_gate = Arc::clone(&overlap_gate);
    let mut backend = Router::new();
    backend.get("/slow", move |_req: &Request| {
        let request_gate = Arc::clone(&upstream_gate);
        async move {
            request_gate.wait().await;
            let data = vec![0xABu8; 10_000];
            Response::bytes(200, data).map(|r| r.with_content_type("application/octet-stream"))
        }
    });
    let backend_addr = common::spawn_server(backend);

    let mut main = Router::new();
    main.proxy("/api", &format!("http://{backend_addr}"));
    let main_addr = common::spawn_server(main);

    let handles: Vec<_> = (0..OVERLAP_REQUEST_COUNT)
        .map(|_| {
            let addr = main_addr;
            spawn_async(async move {
                let resp = http::get(&format!("http://{addr}/api/slow")).await.unwrap();
                assert_eq!(resp.status(), 200);
                assert_eq!(resp.body_bytes().len(), 10_000);
            })
        })
        .collect();

    wait_for_request_overlap(&overlap_gate).await;

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

    let _handle = http::serve_background_tls(tls_listener, main, tls_config)
        .expect("owned server requires a Tokio runtime");

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
