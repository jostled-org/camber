use crate::runtime_support as common;

use camber::http::{self, Request, Response, Router};
use camber::{runtime, spawn};
use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Barrier};
use std::time::Duration;

#[test]
fn tokio_runtime_runs_existing_closure() {
    let result = runtime::run(|| 42).unwrap();
    assert_eq!(result, 42);

    let result = runtime::run(|| spawn(|| 1).join().unwrap()).unwrap();
    assert_eq!(result, 1);
}

#[test]
fn spawn_runs_on_tokio_blocking_pool() {
    runtime::run(|| {
        const TASKS: usize = 10;
        let rendezvous = Arc::new(Barrier::new(TASKS));
        let handles: Vec<_> = (0..TASKS)
            .map(|_| {
                let rendezvous = Arc::clone(&rendezvous);
                spawn(move || {
                    // Every task must be resident before any can complete.
                    rendezvous.wait();
                    std::thread::current().id()
                })
            })
            .collect();

        let thread_ids: HashSet<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();

        assert!(
            thread_ids.len() >= 2,
            "expected at least 2 distinct thread IDs, got {}",
            thread_ids.len()
        );
    })
    .unwrap();
}

#[camber::test]
async fn hyper_serves_get_request() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "hi")
    });

    let addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "hi");

    runtime::request_shutdown();
}

#[camber::test]
async fn hyper_serves_post_with_body() {
    let mut router = Router::new();
    router.post("/echo", |req: &Request| {
        let body = req.body().to_owned();
        async move { Response::text(200, &body) }
    });

    let addr = common::spawn_server(router);

    let resp = http::post(&format!("http://{addr}/echo"), "payload")
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "payload");

    runtime::request_shutdown();
}

#[test]
fn hyper_keepalive_reuses_connection() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .run(|| {
            let mut router = Router::new();
            router.get("/ping", |_req: &Request| async {
                Response::text(200, "pong")
            });

            let addr = common::spawn_server(router);
            let mut stream = crate::http::connect(addr).unwrap();

            // First request
            let req1 = "GET /ping HTTP/1.1\r\nHost: localhost\r\n\r\n";
            stream.write_all(req1.as_bytes()).unwrap();
            let resp1 = crate::http::read_http_response_bounded(&mut stream).unwrap();
            assert_eq!(resp1.status, 200);
            assert_eq!(resp1.body.as_ref(), b"pong");

            // Second request on same connection
            let req2 = "GET /ping HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
            stream.write_all(req2.as_bytes()).unwrap();
            let resp2 = crate::http::read_http_response_bounded(&mut stream).unwrap();
            assert_eq!(resp2.status, 200);
            assert_eq!(resp2.body.as_ref(), b"pong");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[camber::test]
async fn hyper_graceful_shutdown() {
    let mut router = Router::new();
    router.get("/alive", |_req: &Request| async {
        Response::text(200, "yes")
    });

    let addr = common::spawn_server(router);

    let resp = http::get(&format!("http://{addr}/alive")).await.unwrap();
    assert_eq!(resp.status(), 200);

    runtime::request_shutdown();
    // If we reach here, runtime exited cleanly
}
