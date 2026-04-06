mod common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime, spawn};
use std::io::{Read, Write};

fn spawn_host_server(host_router: http::HostRouter) -> std::net::SocketAddr {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    spawn(move || -> Result<(), RuntimeError> { http::serve_hosts(listener, host_router) });
    addr
}

/// Send a GET request with a specific Host header via raw TCP.
fn get_with_host(addr: std::net::SocketAddr, path: &str, host: &str) -> (u16, String) {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();

    let status_line = buf.lines().next().unwrap_or("");
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let body = buf.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
    // Handle chunked transfer encoding: extract the body content
    let body = parse_body(body);
    (status, body)
}

/// Parse an HTTP response body, handling chunked transfer encoding.
fn parse_body(raw: &str) -> String {
    // If it looks like chunked encoding (starts with a hex size line), decode it
    let trimmed = raw.trim();
    match trimmed
        .lines()
        .next()
        .and_then(|line| usize::from_str_radix(line.trim(), 16).ok())
    {
        Some(size) if size > 0 => {
            // Chunked: first line is hex size, next is the data
            let data_start = trimmed.find('\n').map(|i| i + 1).unwrap_or(0);
            trimmed[data_start..data_start + size].to_string()
        }
        _ => trimmed.to_string(),
    }
}

#[test]
fn host_routing_dispatches_by_host_header() {
    common::test_runtime()
        .run(|| {
            let mut router_a = Router::new();
            router_a.get("/hello", |_req: &Request| async {
                Response::text(200, "from-a")
            });

            let mut router_b = Router::new();
            router_b.get("/hello", |_req: &Request| async {
                Response::text(200, "from-b")
            });

            let mut host_router = http::HostRouter::new();
            host_router.add("a.test", router_a);
            host_router.add("b.test", router_b);

            let addr = spawn_host_server(host_router);

            let (status_a, body_a) = get_with_host(addr, "/hello", "a.test");
            assert_eq!(status_a, 200);
            assert_eq!(body_a, "from-a");

            let (status_b, body_b) = get_with_host(addr, "/hello", "b.test");
            assert_eq!(status_b, 200);
            assert_eq!(body_b, "from-b");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn host_routing_falls_back_to_default() {
    common::test_runtime()
        .run(|| {
            let mut router_a = Router::new();
            router_a.get("/hello", |_req: &Request| async {
                Response::text(200, "from-a")
            });

            let mut default_router = Router::new();
            default_router.get("/hello", |_req: &Request| async {
                Response::text(200, "default")
            });

            let mut host_router = http::HostRouter::new();
            host_router.add("a.test", router_a);
            host_router.set_default(default_router);

            let addr = spawn_host_server(host_router);

            let (status, body) = get_with_host(addr, "/hello", "unknown.test");
            assert_eq!(status, 200);
            assert_eq!(body, "default");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn host_routing_returns_404_without_default() {
    common::test_runtime()
        .run(|| {
            let mut router_a = Router::new();
            router_a.get("/hello", |_req: &Request| async {
                Response::text(200, "from-a")
            });

            let mut host_router = http::HostRouter::new();
            host_router.add("a.test", router_a);

            let addr = spawn_host_server(host_router);

            let (status, _body) = get_with_host(addr, "/hello", "unknown.test");
            assert_eq!(status, 404);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn host_router_matches_correct_host_after_freeze() {
    common::test_runtime()
        .run(|| {
            // Register 5 hosts in non-sorted order
            let hosts = [
                "delta.test",
                "alpha.test",
                "echo.test",
                "bravo.test",
                "charlie.test",
            ];
            let mut host_router = http::HostRouter::new();

            for host in &hosts {
                let mut router = Router::new();
                let tag: Box<str> = (*host).into();
                router.get("/id", move |_req: &Request| {
                    let tag = tag.clone();
                    async move { Response::text(200, tag.as_ref()) }
                });
                host_router.add(host, router);
            }

            let addr = spawn_host_server(host_router);

            // Dispatch requests for each host and verify correct routing
            for host in &hosts {
                let (status, body) = get_with_host(addr, "/id", host);
                assert_eq!(status, 200, "expected 200 for host {host}");
                assert_eq!(body, *host, "wrong router matched for host {host}");
            }

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn host_router_returns_fallback_for_unknown_host() {
    common::test_runtime()
        .run(|| {
            let hosts = ["alpha.test", "bravo.test", "charlie.test"];
            let mut host_router = http::HostRouter::new();

            for host in &hosts {
                let mut router = Router::new();
                router.get("/id", |_req: &Request| async {
                    Response::text(200, "named")
                });
                host_router.add(host, router);
            }

            let mut fallback = Router::new();
            fallback.get("/id", |_req: &Request| async {
                Response::text(200, "fallback")
            });
            host_router.set_default(fallback);

            let addr = spawn_host_server(host_router);

            let (status, body) = get_with_host(addr, "/id", "unknown.test");
            assert_eq!(status, 200);
            assert_eq!(body, "fallback");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn host_routing_strips_port_from_host_header() {
    common::test_runtime()
        .run(|| {
            let mut router_a = Router::new();
            router_a.get("/hello", |_req: &Request| async {
                Response::text(200, "from-a")
            });

            let mut host_router = http::HostRouter::new();
            host_router.add("a.test", router_a);

            let addr = spawn_host_server(host_router);

            let (status, body) = get_with_host(addr, "/hello", "a.test:8080");
            assert_eq!(status, 200);
            assert_eq!(body, "from-a");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn host_router_matches_uppercase_host_header() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "from-example")
            });

            let mut host_router = http::HostRouter::new();
            host_router.add("example.com", router);

            let addr = spawn_host_server(host_router);

            let (status, body) = get_with_host(addr, "/hello", "EXAMPLE.COM");
            assert_eq!(status, 200);
            assert_eq!(body, "from-example");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn host_router_matches_mixed_case_host_header_with_port() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "from-app")
            });

            let mut host_router = http::HostRouter::new();
            host_router.add("app.example.com", router);

            let addr = spawn_host_server(host_router);

            let (status, body) = get_with_host(addr, "/hello", "App.Example.Com:8080");
            assert_eq!(status, 200);
            assert_eq!(body, "from-app");

            runtime::request_shutdown();
        })
        .unwrap();
}
