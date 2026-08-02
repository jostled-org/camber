use crate::runtime_support as common;
use camber::RuntimeError;
use camber::http::{self, Request, Response, Router, rate_limit};
use camber::runtime;
use std::time::Duration;

#[test]
fn rate_limit_rejects_intervals_that_cannot_fit_its_monotonic_clock() {
    let too_large = Duration::from_nanos(u64::MAX).saturating_add(Duration::from_nanos(1));
    let result = rate_limit::builder().tokens(1).interval(too_large).build();

    assert!(matches!(result, Err(RuntimeError::InvalidArgument(_))));
}

#[camber::test]
async fn rate_limit_extreme_token_counts_do_not_overflow_refill() {
    let middleware = rate_limit::builder()
        .tokens(u64::MAX)
        .interval(Duration::from_nanos(1))
        .build()
        .unwrap();
    let mut router = Router::new();
    router.use_middleware(middleware);
    router.get("/limited", |_req: &Request| async {
        Response::text(200, "ok")
    });
    let addr = common::spawn_server(router);

    let response = http::get(&format!("http://{addr}/limited")).await.unwrap();
    assert_eq!(response.status(), 200);
    runtime::request_shutdown();
}
