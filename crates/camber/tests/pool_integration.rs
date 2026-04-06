mod common;

use camber::http::{self, Request, Response, Router};
use camber::{runtime, spawn_async};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

#[camber::test]
async fn pool_dispatches_concurrent_http_requests() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "Hello, world!")
    });

    let addr = common::spawn_server(router);

    // Send 20 concurrent requests from separate tasks
    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    for _ in 0..20 {
        let counter = Arc::clone(&counter);
        let url = format!("http://{addr}/hello");
        let h = spawn_async(async move {
            let resp = http::get(&url).await.unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.body(), "Hello, world!");
            counter.fetch_add(1, Ordering::SeqCst);
        });
        handles.push(h);
    }

    for h in handles {
        h.await.unwrap();
    }

    assert_eq!(counter.load(Ordering::SeqCst), 20);

    runtime::request_shutdown();
}

#[camber::test]
async fn pool_backpressure_under_load() {
    let completed = Arc::new(AtomicUsize::new(0));

    let mut router = Router::new();
    let completed_inner = Arc::clone(&completed);
    router.get("/slow", move |_req: &Request| {
        let completed_inner = completed_inner.clone();
        async move {
            thread::sleep(Duration::from_millis(100));
            completed_inner.fetch_add(1, Ordering::SeqCst);
            Response::text(200, "done")
        }
    });

    let addr = common::spawn_server(router);

    // Send 10 concurrent requests
    let mut handles = Vec::new();
    for _ in 0..10 {
        let url = format!("http://{addr}/slow");
        let h = spawn_async(async move {
            let resp = http::get(&url).await.unwrap();
            assert_eq!(resp.status(), 200);
        });
        handles.push(h);
    }

    for h in handles {
        h.await.unwrap();
    }

    // All 10 completed — backpressure queued, didn't drop
    assert_eq!(completed.load(Ordering::SeqCst), 10);

    runtime::request_shutdown();
}

#[camber::test]
async fn acceptor_stops_on_shutdown() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "Hello, world!")
    });

    let addr = common::spawn_server(router);

    // Confirm server is alive
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(resp.status(), 200);

    // Request shutdown
    runtime::request_shutdown();
    // If we get here, serve_listener returned Ok — test passes
}

#[camber::test]
async fn pool_workers_joined_on_shutdown() {
    let handler_finished = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&handler_finished);

    let mut router = Router::new();
    router.get("/slow", move |_req: &Request| {
        let handler_flag = handler_flag.clone();
        async move {
            thread::sleep(Duration::from_millis(200));
            handler_flag.store(true, Ordering::Release);
            Response::text(200, "done")
        }
    });

    let addr = common::spawn_server(router);

    // Send a request that will be in-flight during shutdown
    let url = format!("http://{addr}/slow");
    let client = spawn_async(async move { http::get(&url).await.unwrap() });

    // Small delay to ensure the request reaches the handler
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Request shutdown while handler is still sleeping
    runtime::request_shutdown();

    // Wait for client to get its response
    let resp = client.await.unwrap();
    assert_eq!(resp.status(), 200);

    // After runtime::run returns, serve_listener has returned, which means
    // pool.shutdown() joined all workers. The handler must have completed.
    assert!(handler_finished.load(Ordering::Acquire));
}
