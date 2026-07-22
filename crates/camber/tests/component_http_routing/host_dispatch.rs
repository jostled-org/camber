use crate::runtime_support as common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime, spawn};
use std::io::Write;

fn spawn_host_server(host_router: http::HostRouter) -> std::net::SocketAddr {
    let listener = camber::net::listen("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap().tcp().unwrap();
    spawn(move || -> Result<(), RuntimeError> { http::serve_hosts(listener, host_router) });
    addr
}

fn get_with_host(addr: std::net::SocketAddr, path: &str, host: &str) -> crate::http::HttpResponse {
    // A raw request is required because Host selection is the wire contract.
    let mut stream = crate::http::connect(addr).unwrap();
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    crate::http::read_http_response(&mut stream).unwrap()
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

            let response_a = get_with_host(addr, "/hello", "a.test");
            assert_eq!(response_a.status, 200);
            assert_eq!(response_a.body.as_ref(), b"from-a");

            let response_b = get_with_host(addr, "/hello", "b.test");
            assert_eq!(response_b.status, 200);
            assert_eq!(response_b.body.as_ref(), b"from-b");

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

            let response = get_with_host(addr, "/hello", "unknown.test");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"default");

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

            let response = get_with_host(addr, "/hello", "unknown.test");
            assert_eq!(response.status, 404);

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
                let response = get_with_host(addr, "/id", host);
                assert_eq!(response.status, 200, "expected 200 for host {host}");
                assert_eq!(
                    response.body.as_ref(),
                    host.as_bytes(),
                    "wrong router matched for host {host}"
                );
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

            let response = get_with_host(addr, "/id", "unknown.test");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"fallback");

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

            let response = get_with_host(addr, "/hello", "a.test:8080");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"from-a");

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

            let response = get_with_host(addr, "/hello", "EXAMPLE.COM");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"from-example");

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

            let response = get_with_host(addr, "/hello", "App.Example.Com:8080");
            assert_eq!(response.status, 200);
            assert_eq!(response.body.as_ref(), b"from-app");

            runtime::request_shutdown();
        })
        .unwrap();
}
