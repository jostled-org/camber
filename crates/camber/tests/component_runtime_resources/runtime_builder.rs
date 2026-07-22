use crate::common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

const EVENT_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn builder_configures_concurrent_requests() {
    runtime::builder()
        .worker_threads(2)
        .keepalive_timeout(Duration::from_millis(100))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.get("/slow", |_req: &Request| async {
                Response::text(200, "done")
            });

            let server = common::spawn_server_ready(router, EVENT_TIMEOUT).unwrap();
            let addr = server.local_addr();

            let counter = AtomicUsize::new(0);
            let url = format!("http://{addr}/slow");
            let responses = common::block_on(futures_util::future::join_all(
                (0..3).map(|_| http::get(&url)),
            ));
            responses.into_iter().for_each(|response| {
                let resp = response.unwrap();
                assert_eq!(resp.status(), 200);
                counter.fetch_add(1, Ordering::SeqCst);
            });

            assert_eq!(counter.load(Ordering::SeqCst), 3);

            server.shutdown_bounded(EVENT_TIMEOUT).unwrap();
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn builder_configures_shutdown_timeout() {
    let start = Instant::now();

    let task = runtime::builder()
        .shutdown_timeout(Duration::from_millis(200))
        .run(|| {
            let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
            let task = camber::spawn_async(async move {
                let _ = entered_tx.send(());
                std::future::pending::<()>().await;
            });
            common::block_on(entered_rx).unwrap();
            runtime::request_shutdown();
            task
        })
        .unwrap();

    let elapsed = start.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "expected < 1s (safety-net timeout), got {elapsed:?}"
    );

    let join_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let result = join_runtime.block_on(async { tokio::time::timeout(EVENT_TIMEOUT, task).await });
    assert!(
        result.is_ok(),
        "timed-out task handle was not boundedly joined"
    );
    assert!(
        result.unwrap().is_err(),
        "pending task completed successfully"
    );
}
