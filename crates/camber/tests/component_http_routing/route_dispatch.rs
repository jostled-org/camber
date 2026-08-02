use crate::runtime_support as common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::io::{Read, Write};
use std::time::Duration;

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

#[test]
fn unmatched_route_is_answered_without_waiting_for_its_declared_body() {
    common::test_runtime()
        .run(|| {
            let addr = common::spawn_server(Router::new());
            let mut stream = std::net::TcpStream::connect(addr).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(1)))
                .unwrap();
            stream
                .write_all(
                    b"POST /missing HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1048576\r\nConnection: close\r\n\r\n",
                )
                .unwrap();

            let mut response = Vec::new();
            stream.read_to_end(&mut response).unwrap();
            assert!(
                response.starts_with(b"HTTP/1.1 404"),
                "unexpected response: {}",
                String::from_utf8_lossy(&response)
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[tokio::test(start_paused = true)]
async fn buffered_request_body_has_a_finite_read_deadline() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let mut router = Router::new();
    router.post("/upload", |_req: &Request| async {
        Response::text(200, "uploaded")
    });
    let handle = http::serve_background(listener, router);
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(
            b"POST /upload HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1\r\nConnection: close\r\n\r\n",
        )
        .await
        .unwrap();
    tokio::task::yield_now().await;
    tokio::task::yield_now().await;

    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(60), stream.read_to_end(&mut response))
        .await
        .expect("request body read had no finite deadline")
        .unwrap();
    assert!(
        response.starts_with(b"HTTP/1.1 408"),
        "unexpected response: {}",
        String::from_utf8_lossy(&response)
    );

    handle.shutdown_and_join().await.unwrap();
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

fn request(addr: std::net::SocketAddr, method: &str, path: &str) -> crate::http::HttpResponse {
    crate::http::request(addr, method, path, &[], &[], Duration::from_secs(5)).unwrap()
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

            let resp = request(addr, "PATCH", "/item");
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body.as_ref(), b"patched");

            let resp = request(addr, "HEAD", "/item");
            assert_eq!(resp.status, 200);
            assert!(resp.body.is_empty());

            let resp = request(addr, "OPTIONS", "/item");
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body.as_ref(), b"optioned");

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

            let resp = request(addr, "HEAD", "/resource");
            assert_eq!(resp.status, 200);
            assert_eq!(resp.header("x-custom"), Some("present"));
            assert!(resp.body.is_empty());

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

            let resp = request(addr, "HEAD", "/resource");
            assert_eq!(resp.status, 204);
            assert_eq!(resp.header("x-explicit"), Some("yes"));
            assert!(resp.body.is_empty());

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

            let resp = request(addr, "HEAD", "/api/data");
            assert_eq!(resp.status, 200);
            assert_eq!(resp.header("content-type"), Some("application/json"));
            assert!(resp.body.is_empty());

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

            let resp = request(addr, "PATCH", "/item");
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body.as_ref(), b"async-patched");

            let resp = request(addr, "OPTIONS", "/item");
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body.as_ref(), b"async-optioned");

            runtime::request_shutdown();
        })
        .unwrap();
}
