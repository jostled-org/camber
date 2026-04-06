mod common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::io::{Read, Write};

#[camber::test]
async fn router_dispatches_to_matching_handler() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "Hello, world!")
    });

    let addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "Hello, world!");

    runtime::request_shutdown();
}

#[camber::test]
async fn router_returns_404_for_unknown_path() {
    let router = Router::new();
    let addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{addr}/nope")).await.unwrap();
    assert_eq!(resp.status(), 404);

    runtime::request_shutdown();
}

#[camber::test]
async fn router_handler_makes_outbound_call() {
    // Backend
    let mut backend_router = Router::new();
    backend_router.get("/data", |_req: &Request| async {
        Response::text(200, "backend-data")
    });
    let backend_addr = common::spawn_server(backend_router);

    // Main server proxying to backend
    let mut router = Router::new();
    let backend_url = format!("http://{backend_addr}/data");
    router.get("/proxy", move |_req: &Request| {
        let url = backend_url.clone();
        async move {
            match http::get(&url).await {
                Ok(resp) => Response::text(200, resp.body()),
                Err(_) => Response::text(502, "upstream error"),
            }
        }
    });
    let main_addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{main_addr}/proxy"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert!(resp.body().contains("backend-data"));

    runtime::request_shutdown();
}

#[camber::test]
async fn dispatch_parameterless_route_returns_correct_handler() {
    let mut router = Router::new();
    router.get("/health", |_req: &Request| async {
        Response::text(200, "healthy")
    });
    router.post("/submit", |_req: &Request| async {
        Response::text(201, "submitted")
    });

    let addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{addr}/health")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "healthy");

    let resp = http::post(&format!("http://{addr}/submit"), "")
        .await
        .unwrap();
    assert_eq!(resp.status(), 201);
    assert_eq!(resp.body(), "submitted");

    runtime::request_shutdown();
}

#[camber::test]
async fn sorted_static_children_match_correctly() {
    let mut router = Router::new();
    // Register in non-sorted order
    router.get("/z", |_req: &Request| async { Response::text(200, "z") });
    router.get("/a", |_req: &Request| async { Response::text(200, "a") });
    router.get("/c", |_req: &Request| async { Response::text(200, "c") });
    router.get("/b", |_req: &Request| async { Response::text(200, "b") });

    let addr = common::spawn_server(router);

    for path in ["a", "b", "c", "z"] {
        let resp = http::get(&format!("http://{addr}/{path}")).await.unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(resp.body(), path);
    }

    runtime::request_shutdown();
}

#[camber::test]
async fn method_indexed_lookup_returns_correct_handler() {
    let mut router = Router::new();
    router.get("/item", |_req: &Request| async {
        Response::text(200, "get-item")
    });
    router.post("/item", |_req: &Request| async {
        Response::text(200, "post-item")
    });

    let addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{addr}/item")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "get-item");

    let resp = http::post(&format!("http://{addr}/item"), "")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "post-item");

    runtime::request_shutdown();
}

/// Helper to send a raw HTTP request and read the response body.
fn raw_request(addr: std::net::SocketAddr, method: &str, path: &str) -> String {
    let mut stream = std::net::TcpStream::connect(addr).unwrap();
    let req = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).unwrap();
    let mut buf = String::new();
    stream.read_to_string(&mut buf).unwrap();
    buf
}

#[test]
fn router_registers_all_method_variants() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.patch("/item", |_req: &Request| async {
                Response::text(200, "patched")
            });
            router.head("/item", |_req: &Request| async {
                Response::text(200, "headed")
            });
            router.options("/item", |_req: &Request| async {
                Response::text(200, "optioned")
            });

            let addr = common::spawn_server(router);

            let resp = raw_request(addr, "PATCH", "/item");
            assert!(resp.contains("200"), "PATCH should return 200, got: {resp}");
            assert!(resp.contains("patched"), "PATCH body missing, got: {resp}");

            let resp = raw_request(addr, "HEAD", "/item");
            assert!(resp.contains("200"), "HEAD should return 200, got: {resp}");

            let resp = raw_request(addr, "OPTIONS", "/item");
            assert!(
                resp.contains("200"),
                "OPTIONS should return 200, got: {resp}"
            );
            assert!(
                resp.contains("optioned"),
                "OPTIONS body missing, got: {resp}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn head_auto_response_returns_status_and_headers() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/resource", |_req: &Request| async {
                Response::text(200, "get body").map(|r| r.with_header("X-Custom", "present"))
            });
            // No explicit HEAD handler registered

            let addr = common::spawn_server(router);

            let resp = raw_request(addr, "HEAD", "/resource");
            assert!(resp.contains("200"), "HEAD should return 200, got: {resp}");
            let resp_lower = resp.to_lowercase();
            assert!(
                resp_lower.contains("x-custom: present"),
                "HEAD should include custom header, got: {resp}"
            );
            // HEAD must not include body text
            assert!(
                !resp.contains("get body"),
                "HEAD should not include body, got: {resp}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn explicit_head_handler_takes_priority() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/resource", |_req: &Request| async {
                Response::text(200, "get body")
            });
            router.head("/resource", |_req: &Request| async {
                Response::empty(204).map(|r| r.with_header("X-Explicit", "yes"))
            });

            let addr = common::spawn_server(router);

            let resp = raw_request(addr, "HEAD", "/resource");
            assert!(
                resp.contains("204"),
                "Explicit HEAD should return 204, got: {resp}"
            );
            let resp_lower = resp.to_lowercase();
            assert!(
                resp_lower.contains("x-explicit: yes"),
                "Explicit HEAD header missing, got: {resp}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn head_auto_response_works_with_json_handler() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/api/data", |_req: &Request| async {
                Response::json(200, &serde_json::json!({"key": "value"}))
            });

            let addr = common::spawn_server(router);

            let resp = raw_request(addr, "HEAD", "/api/data");
            assert!(resp.contains("200"), "HEAD should return 200, got: {resp}");
            assert!(
                resp.contains("application/json"),
                "HEAD should include Content-Type, got: {resp}"
            );
            // Body should be empty for HEAD
            let parts: Vec<&str> = resp.splitn(2, "\r\n\r\n").collect();
            let body = parts.get(1).unwrap_or(&"");
            assert!(
                body.trim().is_empty(),
                "HEAD should have empty body, got: {body}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn router_registers_async_method_variants() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.patch("/item", |_req: &Request| async {
                Response::text(200, "async-patched")
            });
            router.head("/item", |_req: &Request| async {
                Response::text(200, "async-headed")
            });
            router.options("/item", |_req: &Request| async {
                Response::text(200, "async-optioned")
            });

            let addr = common::spawn_server(router);

            let resp = raw_request(addr, "PATCH", "/item");
            assert!(
                resp.contains("200"),
                "async PATCH should return 200, got: {resp}"
            );
            assert!(
                resp.contains("async-patched"),
                "async PATCH body missing, got: {resp}"
            );

            let resp = raw_request(addr, "OPTIONS", "/item");
            assert!(
                resp.contains("200"),
                "async OPTIONS should return 200, got: {resp}"
            );
            assert!(
                resp.contains("async-optioned"),
                "async OPTIONS body missing, got: {resp}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}
