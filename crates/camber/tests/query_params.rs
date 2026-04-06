mod common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;

// ── Step 1.T2: query_all_returns_iterator_over_repeated_values ──
#[camber::test]
async fn query_all_returns_iterator_over_repeated_values() {
    let mut router = Router::new();
    router.get("/tags", |req: &Request| {
        let joined: String = req.query_all("tag").collect::<Vec<_>>().join(",");
        async move { Response::text(200, &joined) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/tags?tag=a&tag=b&tag=c"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "a,b,c");

    runtime::request_shutdown();
}

#[camber::test]
async fn query_param_extracts_single_value() {
    let mut router = Router::new();
    router.get("/search", |req: &Request| {
        let q = req.query("q").unwrap_or("none").to_owned();
        async move { Response::text(200, &q) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/search?q=hello"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello");

    runtime::request_shutdown();
}

#[camber::test]
async fn query_param_returns_none_when_missing() {
    let mut router = Router::new();
    router.get("/search", |req: &Request| {
        let q = req.query("missing").unwrap_or("none").to_owned();
        async move { Response::text(200, &q) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/search?q=hello"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "none");

    runtime::request_shutdown();
}

#[camber::test]
async fn query_param_handles_multiple_values() {
    let mut router = Router::new();
    router.get("/filter", |req: &Request| {
        let tags = req.query_all("tag").collect::<Vec<_>>().join(",");
        async move { Response::text(200, &tags) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/filter?tag=rust&tag=go"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "rust,go");

    runtime::request_shutdown();
}

#[camber::test]
async fn query_param_decodes_percent_encoding() {
    let mut router = Router::new();
    router.get("/search", |req: &Request| {
        let q = req.query("q").unwrap_or("none").to_owned();
        async move { Response::text(200, &q) }
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/search?q=hello%20world"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello world");

    runtime::request_shutdown();
}
