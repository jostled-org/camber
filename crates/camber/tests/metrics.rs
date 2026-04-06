mod common;

use camber::http::{self, Next, Request, Response, Router};
use camber::runtime;
use std::future::Future;
use std::pin::Pin;

#[test]
fn metrics_endpoint_returns_prometheus_format() {
    common::test_runtime()
        .with_metrics()
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hello")
            });

            let addr = common::spawn_server(router);

            // Send 5 requests to generate metrics
            for _ in 0..5 {
                let resp = common::block_on(http::get(&format!("http://{addr}/hello"))).unwrap();
                assert_eq!(resp.status(), 200);
            }

            // Fetch the metrics endpoint
            let resp = common::block_on(http::get(&format!("http://{addr}/metrics"))).unwrap();
            assert_eq!(resp.status(), 200);

            let body = resp.body();
            assert!(
                body.contains("http_requests_total"),
                "expected http_requests_total in metrics output, got: {body}"
            );
            assert!(
                body.contains("http_request_duration_seconds"),
                "expected http_request_duration_seconds in metrics output, got: {body}"
            );

            // Prometheus text format uses text/plain
            let content_type = resp
                .headers()
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case("Content-Type"))
                .map(|(_, v)| v.as_ref())
                .unwrap_or("");
            assert!(
                content_type.contains("text/plain"),
                "expected Content-Type containing text/plain, got: {content_type}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[camber::test]
async fn metrics_disabled_by_default() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "hello")
    });

    let addr = common::spawn_server(router);

    // /metrics should be 404 when metrics not enabled
    let resp = http::get(&format!("http://{addr}/metrics")).await.unwrap();
    assert_eq!(resp.status(), 404);

    runtime::request_shutdown();
}

fn auth_middleware(req: &Request, next: Next) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    let has_auth = req
        .headers()
        .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
    match has_auth {
        true => next.call(req),
        false => Box::pin(async { Response::text(401, "unauthorized").expect("valid status") }),
    }
}

#[test]
fn metrics_endpoint_goes_through_middleware() {
    common::test_runtime()
        .with_metrics()
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(auth_middleware);
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hello")
            });

            let addr = common::spawn_server(router);

            // No auth header → 401 (middleware blocks)
            let resp = common::block_on(http::get(&format!("http://{addr}/metrics"))).unwrap();
            assert_eq!(resp.status(), 401);

            // With auth header → 200
            let raw =
                common::raw_request(addr, "GET", "/metrics", &[("Authorization", "Bearer tok")]);
            let status = common::status_from_raw(&raw);
            assert_eq!(status, 200);

            runtime::request_shutdown();
        })
        .unwrap();
}
