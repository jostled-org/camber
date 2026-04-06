mod common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[camber::test]
async fn e2e_async_server() {
    let counter = Arc::new(AtomicU32::new(0));
    let sched_counter = Arc::clone(&counter);

    let mut router = Router::new();

    // Async middleware — adds X-Duration header
    router.use_middleware(|req: &Request, next| {
        let start = std::time::Instant::now();
        let resp_fut = next.call(req);
        Box::pin(async move {
            let resp = resp_fut.await;
            let ms = start.elapsed().as_millis();
            resp.with_header("X-Duration-Ms", &ms.to_string())
        }) as Pin<Box<dyn Future<Output = Response> + Send>>
    });

    // Handler
    router.get("/sync", |_req: &Request| async {
        Response::text(200, "sync")
    });

    // Async handler with 50ms sleep
    router.get("/async", |_req: &Request| async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Response::text(200, "async")
    });

    let addr = common::spawn_server(router);

    // Scheduled task every 100ms
    let _handle = camber::schedule::every(Duration::from_millis(100), move || {
        sched_counter.fetch_add(1, Ordering::SeqCst);
    })
    .unwrap();

    // Send 10 concurrent requests to /async
    let start = std::time::Instant::now();
    let handles: Vec<_> = (0..10)
        .map(|_| {
            std::thread::spawn(move || {
                use std::io::{Read, Write};
                let mut stream = std::net::TcpStream::connect(addr).unwrap();
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .write_all(
                        b"GET /async HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .unwrap();
                let mut buf = String::new();
                stream.read_to_string(&mut buf).unwrap();
                buf
            })
        })
        .collect();

    let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
    let elapsed = start.elapsed();

    for resp in &results {
        assert!(
            resp.starts_with("HTTP/1.1 200"),
            "expected 200, got: {resp}"
        );
        assert!(resp.contains("async"), "body should contain 'async'");
    }

    assert!(
        elapsed < Duration::from_millis(200),
        "concurrent /async requests took {elapsed:?}, expected < 200ms"
    );

    // Sync-style handler works alongside async
    let sync_resp = http::get(&format!("http://{addr}/sync")).await.unwrap();
    assert_eq!(sync_resp.status(), 200);
    assert_eq!(sync_resp.body(), "sync");

    // Wait for scheduled tasks to accumulate
    tokio::time::sleep(Duration::from_millis(350)).await;
    let count = counter.load(Ordering::SeqCst);
    assert!(
        (2..=5).contains(&count),
        "scheduled counter: expected 2-5, got {count}"
    );

    runtime::request_shutdown();
}
