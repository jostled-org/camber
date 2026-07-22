use crate::common;

use camber::http::{self, Request, Response, Router};
use camber::{runtime, spawn_async};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const EVENT_TIMEOUT: Duration = Duration::from_secs(2);

#[camber::test]
async fn pool_dispatches_concurrent_http_requests() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "Hello, world!")
    });

    let server = common::spawn_server_ready(router, EVENT_TIMEOUT).unwrap();
    let addr = server.local_addr();

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

    tokio::time::timeout(EVENT_TIMEOUT, async {
        for handle in handles {
            handle.await.unwrap();
        }
    })
    .await
    .unwrap();

    assert_eq!(counter.load(Ordering::SeqCst), 20);

    server.shutdown_bounded(EVENT_TIMEOUT).unwrap();
    runtime::request_shutdown();
}

#[test]
fn pool_backpressure_under_load() {
    runtime::builder()
        .worker_threads(2)
        .keepalive_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            common::block_on(async {
                let completed = Arc::new(AtomicUsize::new(0));
                let release = Arc::new(tokio::sync::Semaphore::new(0));
                let handler_release = Arc::clone(&release);
                let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel();

                let mut router = Router::new();
                let completed_inner = Arc::clone(&completed);
                router.get("/slow", move |_req: &Request| {
                    let completed_inner = Arc::clone(&completed_inner);
                    let release = Arc::clone(&handler_release);
                    let entered_tx = entered_tx.clone();
                    async move {
                        entered_tx.send(()).unwrap();
                        let permit = release.acquire_owned().await.unwrap();
                        permit.forget();
                        completed_inner.fetch_add(1, Ordering::SeqCst);
                        Response::text(200, "done")
                    }
                });

                let server = common::spawn_server_ready(router, EVENT_TIMEOUT).unwrap();
                let addr = server.local_addr();
                let client = Arc::new(http::client());
                let mut handles = Vec::new();
                for _ in 0..10 {
                    let client = Arc::clone(&client);
                    let url = format!("http://{addr}/slow");
                    let handle = spawn_async(async move {
                        let resp = client.get(&url).await.unwrap();
                        assert_eq!(resp.status(), 200);
                    });
                    handles.push(handle);
                }

                tokio::time::timeout(EVENT_TIMEOUT, entered_rx.recv())
                    .await
                    .unwrap()
                    .unwrap();
                release.add_permits(10);
                tokio::time::timeout(EVENT_TIMEOUT, async {
                    for handle in handles {
                        handle.await.unwrap();
                    }
                })
                .await
                .unwrap();

                assert_eq!(completed.load(Ordering::SeqCst), 10);
                server.shutdown_bounded(EVENT_TIMEOUT).unwrap();
                runtime::request_shutdown();
            });
        })
        .unwrap();
}

#[camber::test]
async fn acceptor_stops_on_shutdown() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "Hello, world!")
    });

    let server = common::spawn_server_ready(router, EVENT_TIMEOUT).unwrap();
    let addr = server.local_addr();

    // Confirm server is alive
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(resp.status(), 200);

    // Request shutdown
    runtime::request_shutdown();
    server.shutdown_bounded(EVENT_TIMEOUT).unwrap();
}

#[camber::test]
async fn pool_workers_joined_on_shutdown() {
    let handler_finished = Arc::new(AtomicBool::new(false));
    let handler_flag = Arc::clone(&handler_finished);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let handler_release = Arc::clone(&release);

    let mut router = Router::new();
    router.get("/slow", move |_req: &Request| {
        let handler_flag = Arc::clone(&handler_flag);
        let entered_tx = Arc::clone(&entered_tx);
        let release = Arc::clone(&handler_release);
        async move {
            if let Some(sender) = entered_tx
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                let _ = sender.send(());
            }
            let permit = release.acquire_owned().await.unwrap();
            permit.forget();
            handler_flag.store(true, Ordering::Release);
            Response::text(200, "done")
        }
    });

    let server = common::spawn_server_ready(router, EVENT_TIMEOUT).unwrap();
    let addr = server.local_addr();

    // Send a request that will be in-flight during shutdown
    let url = format!("http://{addr}/slow");
    let client = spawn_async(async move { http::get(&url).await.unwrap() });

    tokio::time::timeout(EVENT_TIMEOUT, entered_rx)
        .await
        .unwrap()
        .unwrap();

    // Request shutdown while the checkpointed handler still owns the request.
    runtime::request_shutdown();
    release.add_permits(1);

    // Wait for client to get its response
    let resp = tokio::time::timeout(EVENT_TIMEOUT, async { client.await })
        .await
        .unwrap()
        .unwrap();
    assert_eq!(resp.status(), 200);

    // After runtime::run returns, serve_listener has returned, which means
    // pool.shutdown() joined all workers. The handler must have completed.
    assert!(handler_finished.load(Ordering::Acquire));
    server.shutdown_bounded(EVENT_TIMEOUT).unwrap();
}
