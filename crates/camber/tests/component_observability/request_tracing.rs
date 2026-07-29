use crate::common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;

#[test]
fn tracing_logs_request_lifecycle() {
    // The capture bus is the binary's one global subscriber and fans out by
    // substring, so this route is named uniquely: a path shared with a sibling
    // test would let that test's events answer for this one.
    let captured = common::capture_events("/tracing-lifecycle");

    common::test_runtime()
        .with_tracing()
        .run(|| {
            let mut router = Router::new();
            router.get("/tracing-lifecycle", |_req: &Request| async {
                Response::text(200, "hello")
            });

            let addr = common::spawn_server(router);

            let resp =
                common::block_on(http::get(&format!("http://{addr}/tracing-lifecycle"))).unwrap();
            assert_eq!(resp.status(), 200);

            runtime::request_shutdown();
        })
        .unwrap();

    assert!(
        captured.recorded(&[
            "method=GET",
            "path=/tracing-lifecycle",
            "status=200",
            "latency_ms="
        ]),
        "the request lifecycle was not recorded: {:?}",
        captured.events()
    );
}

/// Without `with_tracing` the runtime records nothing: the request is served
/// and the capture bus stays empty for this route.
#[camber::test]
async fn tracing_disabled_by_default() {
    let captured = common::capture_events("/tracing-default");

    let mut router = Router::new();
    router.get("/tracing-default", |_req: &Request| async {
        Response::text(200, "hello")
    });

    let addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{addr}/tracing-default"))
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hello");

    assert!(
        captured.is_empty(),
        "an untraced request was recorded anyway: {:?}",
        captured.events()
    );

    runtime::request_shutdown();
}
