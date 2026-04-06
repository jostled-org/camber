mod common;

use camber::http::{self, Request, Response, Router};
use camber::{runtime, spawn};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[test]
fn builder_configures_concurrent_requests() {
    runtime::builder()
        .worker_threads(2)
        .keepalive_timeout(Duration::from_millis(100))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.get("/slow", |_req: &Request| async {
                thread::sleep(Duration::from_millis(200));
                Response::text(200, "done")
            });

            let addr = common::spawn_server(router);

            let counter = Arc::new(AtomicUsize::new(0));
            let mut handles = Vec::new();

            for _ in 0..3 {
                let counter = Arc::clone(&counter);
                let url = format!("http://{addr}/slow");
                let h = spawn(move || {
                    let resp = common::block_on(http::get(&url)).unwrap();
                    assert_eq!(resp.status(), 200);
                    counter.fetch_add(1, Ordering::SeqCst);
                });
                handles.push(h);
            }

            for h in handles {
                h.join().unwrap();
            }

            assert_eq!(counter.load(Ordering::SeqCst), 3);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn builder_configures_shutdown_timeout() {
    let start = Instant::now();

    runtime::builder()
        .shutdown_timeout(Duration::from_millis(200))
        .run(|| {
            spawn(move || {
                thread::sleep(Duration::from_secs(5));
            });

            thread::sleep(Duration::from_millis(50));

            runtime::request_shutdown();
        })
        .unwrap();

    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "expected < 1s (safety-net timeout), got {elapsed:?}"
    );
}
