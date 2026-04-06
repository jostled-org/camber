mod common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime};
use std::time::Duration;

#[camber::test]
async fn client_read_timeout_fires() {
    let mut router = Router::new();
    router.get("/slow", |_req: &Request| {
        std::thread::sleep(Duration::from_secs(2));
        async { Response::text(200, "slow") }
    });

    let addr = common::spawn_server(router);

    let result = http::client()
        .read_timeout(Duration::from_millis(100))
        .get(&format!("http://{addr}/slow"))
        .await;

    match result {
        Err(RuntimeError::Timeout) => {}
        Err(e) => panic!("expected Timeout error, got: {e}"),
        Ok(_) => panic!("expected Timeout error, got Ok"),
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
