mod common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[camber::test]
async fn client_retries_on_transient_error() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/retry", move |_req: &Request| {
        let n = c.fetch_add(1, Ordering::Relaxed);
        async move {
            match n < 2 {
                true => Response::empty(503),
                false => Response::text(200, "ok"),
            }
        }
    });
    let addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(3)
        .backoff(Duration::from_millis(10))
        .get(&format!("http://{addr}/retry"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "ok");
    assert_eq!(count.load(Ordering::Relaxed), 3);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_does_not_retry_on_4xx() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/bad", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async { Response::text(400, "bad request") }
    });
    let addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(3)
        .backoff(Duration::from_millis(10))
        .get(&format!("http://{addr}/bad"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 400);
    assert_eq!(count.load(Ordering::Relaxed), 1);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_exhausts_retries_and_returns_last_error() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/fail", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    let addr = common::spawn_server(backend);

    let resp = http::client()
        .retries(2)
        .backoff(Duration::from_millis(10))
        .get(&format!("http://{addr}/fail"))
        .await
        .unwrap();

    assert_eq!(resp.status(), 503);
    assert_eq!(count.load(Ordering::Relaxed), 3);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_free_functions_do_not_retry() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/once", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    let addr = common::spawn_server(backend);

    let resp = http::get(&format!("http://{addr}/once")).await.unwrap();

    assert_eq!(resp.status(), 503);
    assert_eq!(count.load(Ordering::Relaxed), 1);

    runtime::request_shutdown();
}

#[camber::test]
async fn client_retries_on_timeout() {
    let count = Arc::new(AtomicU32::new(0));
    let c = Arc::clone(&count);
    let mut backend = Router::new();
    backend.get("/slow", move |_req: &Request| {
        c.fetch_add(1, Ordering::Relaxed);
        async {
            std::thread::sleep(Duration::from_millis(200));
            Response::text(200, "slow")
        }
    });
    let addr = common::spawn_server(backend);

    let result = http::client()
        .retries(1)
        .backoff(Duration::from_millis(10))
        .read_timeout(Duration::from_millis(50))
        .get(&format!("http://{addr}/slow"))
        .await;

    match &result {
        Err(RuntimeError::Timeout) => {}
        Err(e) => panic!("expected Timeout, got error: {e}"),
        Ok(resp) => panic!("expected Timeout, got status {}", resp.status()),
    }

    assert_eq!(count.load(Ordering::Relaxed), 2);

    runtime::request_shutdown();
}
