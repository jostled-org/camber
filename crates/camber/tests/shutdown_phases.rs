mod common;

use camber::http::{self, Request, Response, Router};
use camber::{runtime, spawn};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

#[camber::test]
async fn shutdown_stops_accepting_immediately() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "Hello")
    });

    let addr = common::spawn_server(router);

    // Wait for server to be ready (spawned task may not have entered accept loop yet)
    let url = format!("http://{addr}/hello");
    let resp = loop {
        match http::get(&url).await {
            Ok(r) => break r,
            Err(_) => tokio::time::sleep(Duration::from_millis(5)).await,
        }
    };
    assert_eq!(resp.status(), 200);

    // Measure shutdown speed
    let start = Instant::now();
    runtime::request_shutdown();

    // Wait for server to exit by trying to connect until refused
    loop {
        tokio::time::sleep(Duration::from_millis(5)).await;
        if start.elapsed() > Duration::from_millis(100) {
            break;
        }
        match TcpStream::connect_timeout(&addr, Duration::from_millis(10)) {
            Err(_) => break,
            Ok(_) => continue,
        }
    }

    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_millis(100),
        "shutdown took {elapsed:?}, expected < 100ms"
    );
}

#[camber::test]
async fn shutdown_drains_inflight_requests() {
    let completed = Arc::new(AtomicBool::new(false));
    let completed_inner = Arc::clone(&completed);
    let handler_entered = Arc::new(AtomicBool::new(false));
    let handler_entered_inner = Arc::clone(&handler_entered);

    let mut router = Router::new();
    router.get("/slow", move |_req: &Request| {
        handler_entered_inner.store(true, Ordering::SeqCst);
        thread::sleep(Duration::from_millis(200));
        completed_inner.store(true, Ordering::SeqCst);
        async { Response::text(200, "done") }
    });

    let addr = common::spawn_server(router);

    // Send a request that will be in-flight during shutdown
    let url = format!("http://{addr}/slow");
    let handle = spawn(move || common::block_on(http::get(&url)));

    // Wait until the handler is actually executing before triggering shutdown
    while !handler_entered.load(Ordering::SeqCst) {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // Request shutdown while the request is still in-flight
    runtime::request_shutdown();

    // The in-flight request should still complete
    let resp = handle.join().unwrap().unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.body(), "done");
    assert!(completed.load(Ordering::SeqCst));
}

#[camber::test]
async fn sigterm_triggers_shutdown() {
    let mut router = Router::new();
    router.get("/hello", |_req: &Request| async {
        Response::text(200, "ok")
    });

    let addr = common::spawn_server(router);

    // Confirm server alive
    let resp = http::get(&format!("http://{addr}/hello")).await.unwrap();
    assert_eq!(resp.status(), 200);

    // Install a signal watcher that connects SIGTERM to the runtime's shutdown flag.
    // The #[camber::test] runtime does not install signal watchers (unlike runtime::run),
    // so we must register one explicitly before raising the signal.
    let shutdown = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let _signal_task =
        camber::signals::spawn_signal_watcher(Arc::clone(&shutdown), Arc::clone(&notify));

    // Let the spawned task register signal handlers with the OS.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Raise SIGTERM to ourselves — signal handler should trigger shutdown
    signal_hook::low_level::raise(signal_hook::consts::SIGTERM).unwrap();

    // Wait for signal handler to propagate
    tokio::time::sleep(Duration::from_millis(50)).await;

    assert!(shutdown.load(Ordering::Acquire));
}
