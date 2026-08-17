use crate::runtime_support as common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime};
use std::time::Duration;

/// The attempt lifetime is its own boundary, and a generous quiet interval
/// cannot lend it any time.
#[camber::test]
async fn client_read_timeout_fires() {
    let mut router = Router::new();
    // Slow enough to outlast the client's 100ms attempt bound by a wide margin,
    // and short enough to release the connection its server now joins before
    // that server's shutdown deadline.
    router.get("/slow", |_req: &Request| {
        std::thread::sleep(Duration::from_millis(500));
        async { Response::text(200, "slow") }
    });

    let addr = common::spawn_server(router);
    let slow = format!("http://{addr}/slow");

    let result = http::client()
        .request_timeout(Duration::from_millis(100))
        .get(&slow)
        .await;

    match result {
        Err(RuntimeError::Timeout) => {}
        Err(e) => panic!("expected Timeout error, got: {e}"),
        Ok(_) => panic!("expected Timeout error, got Ok"),
    }

    // The quiet interval between reads is a different boundary: one long
    // enough to admit this route cannot extend the attempt lifetime above.
    let with_idle = http::client()
        .request_timeout(Duration::from_millis(100))
        .response_idle_timeout(Duration::from_secs(30))
        .get(&slow)
        .await;

    match with_idle {
        Err(RuntimeError::Timeout) => {}
        Err(e) => panic!("expected Timeout error under a long idle bound, got: {e}"),
        Ok(_) => panic!("a long quiet interval must not extend the attempt lifetime"),
    }

    runtime::request_shutdown();
}

#[camber::test]
async fn default_client_succeeds_for_fast_responses() {
    let mut router = Router::new();
    router.get("/fast", |_req: &Request| async {
        Response::text(200, "fast")
    });

    let addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{addr}/fast")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "fast");

    runtime::request_shutdown();
}
