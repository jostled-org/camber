mod common;

use camber::http::{self, Request, Response, Router};
use camber::{RuntimeError, runtime, spawn};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn spawn_join_returns_result() {
    runtime::run(|| {
        let handle = spawn(|| 42);
        assert_eq!(handle.join().unwrap(), 42);

        let handle = spawn(|| {
            #[allow(clippy::panic)]
            {
                panic!("boom");
            }
        });
        let err = handle.join().unwrap_err();
        assert!(matches!(err, RuntimeError::TaskPanicked(_)));
    })
    .unwrap();
}

#[camber::test]
async fn spawn_inside_handler_does_not_deadlock() {
    let mut router = Router::new();
    router.get("/compute", |_req: &Request| async {
        // Handler calls spawn().join() — must NOT deadlock.
        // With hyper + block_in_place, no pool contention possible.
        let h = spawn(|| {
            thread::sleep(Duration::from_millis(50));
            "computed"
        });
        let result = h.join().unwrap();
        Response::text(200, result)
    });

    let addr = common::spawn_server(router);

    let counter = Arc::new(AtomicUsize::new(0));
    let mut handles = Vec::new();

    for _ in 0..4 {
        let counter = Arc::clone(&counter);
        let url = format!("http://{addr}/compute");
        let h = spawn(move || {
            let resp = common::block_on(http::get(&url)).unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.body(), "computed");
            counter.fetch_add(1, Ordering::SeqCst);
        });
        handles.push(h);
    }

    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(counter.load(Ordering::SeqCst), 4);

    runtime::request_shutdown();
}

#[test]
fn structured_concurrency_waits_for_spawned_tasks() {
    let counter = Arc::new(AtomicUsize::new(0));

    let counter_outer = Arc::clone(&counter);
    runtime::run(move || {
        for _ in 0..5 {
            let counter = Arc::clone(&counter_outer);
            spawn(move || {
                thread::sleep(Duration::from_millis(50));
                counter.fetch_add(1, Ordering::SeqCst);
            });
        }
    })
    .unwrap();

    assert_eq!(counter.load(Ordering::SeqCst), 5);
}
