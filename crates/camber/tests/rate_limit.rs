mod common;

use camber::RuntimeError;
use camber::http::rate_limit;
use camber::http::{Request, Response, Router};
use camber::runtime;
use std::time::Duration;

#[camber::test]
async fn rate_limit_allows_requests_within_limit() {
    let mut router = Router::new();
    router.use_middleware(rate_limit::per_second(10).unwrap());
    router.get("/ok", |_req: &Request| async { Response::text(200, "ok") });
    let addr = common::spawn_server(router);

    for _ in 0..5 {
        let resp = camber::http::get(&format!("http://{addr}/ok"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    runtime::request_shutdown();
}

#[camber::test]
async fn rate_limit_rejects_excess_requests() {
    let mut router = Router::new();
    router.use_middleware(rate_limit::per_second(2).unwrap());
    router.get("/ok", |_req: &Request| async { Response::text(200, "ok") });
    let addr = common::spawn_server(router);

    let mut statuses = Vec::new();
    for _ in 0..5 {
        let resp = camber::http::get(&format!("http://{addr}/ok"))
            .await
            .unwrap();
        statuses.push(resp.status());
    }

    let ok_count = statuses.iter().filter(|&&s| s == 200).count();
    let rejected_count = statuses.iter().filter(|&&s| s == 429).count();
    assert_eq!(ok_count, 2);
    assert_eq!(rejected_count, 3);

    runtime::request_shutdown();
}

#[camber::test]
async fn rate_limit_replenishes_over_time() {
    let mut router = Router::new();
    router.use_middleware(rate_limit::per_second(2).unwrap());
    router.get("/ok", |_req: &Request| async { Response::text(200, "ok") });
    let addr = common::spawn_server(router);

    // Exhaust tokens
    for _ in 0..2 {
        let resp = camber::http::get(&format!("http://{addr}/ok"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    // Wait for replenishment
    tokio::time::sleep(Duration::from_millis(1100)).await;

    // Should have tokens again
    for _ in 0..2 {
        let resp = camber::http::get(&format!("http://{addr}/ok"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
    }

    runtime::request_shutdown();
}

#[camber::test]
async fn rate_limit_builder_configures_burst() {
    let mut router = Router::new();
    router.use_middleware(
        rate_limit::builder()
            .tokens(2)
            .interval(Duration::from_secs(1))
            .burst(5)
            .build()
            .unwrap(),
    );
    router.get("/ok", |_req: &Request| async { Response::text(200, "ok") });
    let addr = common::spawn_server(router);

    // Burst allows 5 rapid requests
    for i in 0..5 {
        let resp = camber::http::get(&format!("http://{addr}/ok"))
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            200,
            "request {i} should succeed within burst"
        );
    }

    // 6th should be rejected
    let resp = camber::http::get(&format!("http://{addr}/ok"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);

    runtime::request_shutdown();
}

#[camber::test]
async fn rate_limit_per_minute_works() {
    let mut router = Router::new();
    router.use_middleware(rate_limit::per_minute(3).unwrap());
    router.get("/ok", |_req: &Request| async { Response::text(200, "ok") });
    let addr = common::spawn_server(router);

    let mut statuses = Vec::new();
    for _ in 0..4 {
        let resp = camber::http::get(&format!("http://{addr}/ok"))
            .await
            .unwrap();
        statuses.push(resp.status());
    }

    assert_eq!(statuses[0], 200);
    assert_eq!(statuses[1], 200);
    assert_eq!(statuses[2], 200);
    assert_eq!(statuses[3], 429);

    runtime::request_shutdown();
}

#[test]
fn rate_limit_rejects_zero_tokens() {
    let result = rate_limit::per_second(0);
    assert!(matches!(result, Err(RuntimeError::InvalidArgument(_))));
}

#[test]
fn rate_limit_rejects_zero_interval() {
    let result = rate_limit::builder()
        .tokens(10)
        .interval(Duration::ZERO)
        .build();
    assert!(matches!(result, Err(RuntimeError::InvalidArgument(_))));
}

#[test]
fn rate_limit_rejects_burst_less_than_tokens() {
    let result = rate_limit::builder()
        .tokens(10)
        .interval(Duration::from_secs(1))
        .burst(5)
        .build();
    assert!(matches!(result, Err(RuntimeError::InvalidArgument(_))));
}
