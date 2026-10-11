use crate::http::SETTLE_BOUND;
use crate::runtime_support as common;

use camber::http::ClientError;
use camber::http::ClientResponse;
use camber::http::ClientTimeout;
use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime};
use std::time::Duration;

/// The attempt lifetime is its own boundary, and a generous quiet interval
/// cannot lend it any time.
#[camber::test]
async fn client_read_timeout_fires() {
    let mut router = Router::new();
    let release = std::sync::Arc::new(tokio::sync::Notify::new());
    let held = std::sync::Arc::clone(&release);
    let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
    router.get("/slow", move |_req: &Request| {
        let held = std::sync::Arc::clone(&held);
        let entered = entered.clone();
        async move {
            // The waiter exists before the test can learn of the entry, so
            // the release cannot fall between the two.
            let released = held.notified();
            entered
                .send(())
                .expect("the test still observes the handler");
            released.await;
            Response::text(200, "slow")
        }
    });

    let addr = common::spawn_server(router);
    let slow = format!("http://{addr}/slow");

    let result: Result<ClientResponse, RuntimeError> = http::client()
        .request_timeout(Duration::from_millis(100))
        .get(&slow)
        .await;

    match result {
        Err(RuntimeError::Client(ClientError::Timeout(ClientTimeout::Attempt))) => {}
        Err(e) => panic!("expected the attempt timeout, got: {e}"),
        Ok(_) => panic!("expected the attempt timeout, got Ok"),
    }
    assert_eq!(
        tokio::time::timeout(SETTLE_BOUND, entries.recv()).await,
        Ok(Some(())),
        "the first request reached the held handler"
    );

    // The quiet interval between reads is a different boundary: one long
    // enough to admit this route cannot extend the attempt lifetime above.
    let with_idle: Result<ClientResponse, RuntimeError> = http::client()
        .request_timeout(Duration::from_millis(100))
        .response_idle_timeout(Duration::from_secs(30))
        .get(&slow)
        .await;

    match with_idle {
        Err(RuntimeError::Client(ClientError::Timeout(ClientTimeout::Attempt))) => {}
        Err(e) => panic!("expected the attempt timeout under a long idle bound, got: {e}"),
        Ok(_) => panic!("a long quiet interval must not extend the attempt lifetime"),
    }
    assert_eq!(
        tokio::time::timeout(SETTLE_BOUND, entries.recv()).await,
        Ok(Some(())),
        "the second request reached the held handler"
    );
    release.notify_waiters();
    runtime::request_shutdown();
}

#[camber::test]
async fn default_client_succeeds_for_fast_responses() {
    let mut router = Router::new();
    router.get("/fast", |_req: &Request| async {
        Response::text(200, "fast")
    });

    let addr = common::spawn_server(router);

    let resp: ClientResponse = http::get(&format!("http://{addr}/fast")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().unwrap(), "fast");

    runtime::request_shutdown();
}
