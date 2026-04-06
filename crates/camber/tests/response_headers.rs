mod common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;

#[camber::test]
async fn handler_sets_custom_headers_and_status() {
    let mut router = Router::new();
    router.get("/create", |_req: &Request| async {
        Response::text(201, "created").map(|r| {
            r.with_header("X-Custom", "test-value")
                .with_content_type("text/html")
        })
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/create")).await.unwrap();

    assert_eq!(resp.status(), 201);

    let headers: Vec<(&str, &str)> = resp
        .headers()
        .iter()
        .map(|(k, v)| (k.as_ref(), v.as_ref()))
        .collect();

    let has_custom = headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("x-custom") && *v == "test-value");
    assert!(has_custom, "missing X-Custom header, got: {headers:?}");

    let content_type = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    assert_eq!(
        content_type.map(|(_, v)| *v),
        Some("text/html"),
        "Content-Type should be text/html, got: {headers:?}"
    );

    runtime::request_shutdown();
}

#[camber::test]
async fn default_text_response_has_plain_content_type() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "hi")
    });

    let addr = common::spawn_server(router);
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hi");

    let content_type = resp
        .headers()
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("content-type"));
    assert_eq!(
        content_type.map(|(_, v)| v.as_ref()),
        Some("text/plain"),
        "default Content-Type should be text/plain"
    );

    runtime::request_shutdown();
}
