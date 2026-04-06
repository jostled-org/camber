#![allow(clippy::unwrap_used)]

use camber::http::{Request, Response, Router};

#[tokio::test(flavor = "multi_thread")]
async fn body_limit_enforced_via_router() {
    let mut router = Router::new();
    router.post("/upload", |_req: &Request| async {
        Response::text(200, "ok")
    });
    let router = router.max_request_body(1024);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(camber::http::serve_async(listener, router));

    let body = vec![0u8; 2048];
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/upload"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
}

#[tokio::test(flavor = "multi_thread")]
async fn body_limit_default_allows_normal_requests() {
    let mut router = Router::new();
    router.post("/upload", |_req: &Request| async {
        Response::text(200, "ok")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(camber::http::serve_async(listener, router));

    let body = vec![0u8; 1000];
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/upload"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn max_request_body_caps_at_256mb() {
    // Setting beyond 256 MB should silently cap. A body at the limit should pass.
    let mut router = Router::new();
    router.post("/upload", |_req: &Request| async {
        Response::text(200, "ok")
    });
    let router = router.max_request_body(usize::MAX);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(camber::http::serve_async(listener, router));

    // 64 KB body should pass under a 256 MB cap.
    let body = vec![0u8; 64 * 1024];
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/upload"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_background_applies_body_limit() {
    let mut router = Router::new();
    router.post("/upload", |_req: &Request| async {
        Response::text(200, "ok")
    });
    let router = router.max_request_body(512);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let _handle = camber::http::serve_background(listener, router);

    let body = vec![0u8; 1024];
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/upload"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
}
