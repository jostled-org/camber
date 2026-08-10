use camber::http::mock::{LifecycleCheckpoint, lifecycle};
use camber::http::{Request, Response, Router};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn body_limit_enforced_via_router() {
    let mut router = Router::new();
    router.post("/upload", |_req: &Request| async {
        Response::text(200, "ok")
    });
    let router = router.max_request_body(1024);

    let server = crate::http::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let addr = server.local_addr();

    let body = vec![0u8; 2048];
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/upload"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn body_limit_default_allows_normal_requests() {
    let mut router = Router::new();
    router.post("/upload", |_req: &Request| async {
        Response::text(200, "ok")
    });

    let server = crate::http::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let addr = server.local_addr();

    let body = vec![0u8; 1000];
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/upload"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}

/// The hard ceiling clamps a configured `usize::MAX` at the router's own plan.
///
/// The observation is the resolved plan's, not the connection's: the limit a
/// request runs under is decided by the route it matched, and reading it off a
/// connection-wide value would keep passing after that authority moved.
#[tokio::test(flavor = "multi_thread")]
async fn configured_body_limit_clamps_at_hard_max() {
    const HARD_MAX: usize = 256 * 1024 * 1024;

    let mut router = Router::new();
    router.post("/upload", |_req: &Request| async {
        Response::text(200, "ok")
    });
    let router = router.max_request_body(usize::MAX);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let checkpoint = LifecycleCheckpoint::RequestBodyLimitConfigured(HARD_MAX);
    controller.pause_once(checkpoint).unwrap();

    let server = camber::http::serve_background(listener, router);
    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .post(format!("http://{addr}/upload"))
            .body(vec![0_u8])
            .send()
            .await
    });

    tokio::time::timeout(
        Duration::from_secs(2),
        controller.wait_until_paused(checkpoint),
    )
    .await
    .expect("the resolved-plan limit checkpoint timed out")
    .unwrap();
    controller.release(checkpoint).unwrap();

    let response = request.await.unwrap().unwrap();
    assert_eq!(response.status(), 200);
    server.shutdown();
    tokio::time::timeout(Duration::from_secs(2), server.join())
        .await
        .expect("server shutdown timed out")
        .unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn serve_background_applies_body_limit() {
    let mut router = Router::new();
    router.post("/upload", |_req: &Request| async {
        Response::text(200, "ok")
    });
    let router = router.max_request_body(512);

    let server = crate::http::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let addr = server.local_addr();

    let body = vec![0u8; 1024];
    let resp = reqwest::Client::new()
        .post(format!("http://{addr}/upload"))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 413);
    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}
