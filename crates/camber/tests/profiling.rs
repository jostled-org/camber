#![cfg(feature = "profiling")]

mod common;

use camber::http::{Request, Response, Router};
use camber::{runtime, spawn};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[test]
fn profiling_endpoint_returns_flamegraph() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(3))
        .with_profiling()
        .run(|| {
            let mut router = Router::new();
            router.get("/hello", |_req: &Request| async {
                Response::text(200, "hello")
            });

            let addr = common::spawn_server(router);

            // Spawn multiple tasks that burn CPU while profiling captures samples.
            let running = Arc::new(AtomicBool::new(true));
            let mut workers = Vec::new();
            for _ in 0..4 {
                let flag = Arc::clone(&running);
                workers.push(spawn(move || {
                    let mut x = 0u64;
                    while flag.load(Ordering::Relaxed) {
                        for _ in 0..10_000 {
                            x = x.wrapping_mul(6364136223846793005).wrapping_add(1);
                        }
                    }
                    std::hint::black_box(x);
                }));
            }

            // Let workers saturate CPU before starting the profile capture
            std::thread::sleep(Duration::from_millis(200));

            // Request a 1-second CPU profile
            let resp = common::block_on(camber::http::get(&format!(
                "http://{addr}/debug/pprof/cpu?seconds=1"
            )))
            .unwrap();
            assert_eq!(resp.status(), 200);

            let body = resp.body();
            assert!(
                !body.is_empty(),
                "expected non-empty flamegraph SVG, got empty body"
            );

            // Stop the work-generating tasks
            running.store(false, Ordering::Relaxed);
            for w in workers {
                let _ = w.join();
            }

            runtime::request_shutdown();
        })
        .unwrap();
}
