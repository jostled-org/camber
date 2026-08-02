use crate::common;

use camber::http::{self, Request, Response, Router};
use camber::runtime;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

fn async_server_router() -> Router {
    let mut router = Router::new();
    router.use_middleware(|req: &Request, next| {
        let start = std::time::Instant::now();
        let resp_fut = next.call(req);
        Box::pin(async move {
            let resp = resp_fut.await;
            let ms = start.elapsed().as_millis();
            resp.with_header("X-Duration-Ms", &ms.to_string())
        }) as Pin<Box<dyn Future<Output = Response> + Send>>
    });
    router.get("/sync", |_req: &Request| async {
        Response::text(200, "sync")
    });
    router.get("/async", |_req: &Request| async {
        tokio::time::sleep(Duration::from_millis(50)).await;
        Response::text(200, "async")
    });
    router
}

fn run_concurrent_async_requests(
    addr: std::net::SocketAddr,
) -> (Box<[Box<str>]>, std::time::Duration) {
    let start = std::time::Instant::now();
    let handles: Vec<_> = (0..10)
        .map(|_| std::thread::spawn(move || crate::http::raw_request(addr, "GET", "/async", &[])))
        .collect();
    let results = handles
        .into_iter()
        .map(|handle| handle.join().unwrap())
        .collect();
    (results, start.elapsed())
}

fn assert_concurrent_async_responses(results: &[Box<str>], elapsed: Duration) {
    for response in results {
        assert_eq!(
            crate::http::status_from_raw(response),
            200,
            "expected 200, got: {response}"
        );
        assert!(response.contains("async"), "body should contain 'async'");
    }
    assert!(
        elapsed < Duration::from_millis(200),
        "concurrent /async requests took {elapsed:?}, expected < 200ms"
    );
}

async fn assert_scheduled_events(scheduled_rx: &mut tokio::sync::mpsc::UnboundedReceiver<()>) {
    let schedule_deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    for event in 1..=3 {
        let observed = tokio::time::timeout_at(schedule_deadline, scheduled_rx.recv()).await;
        assert!(
            matches!(observed, Ok(Some(()))),
            "scheduled event {event} did not arrive before the aggregate deadline: {observed:?}"
        );
    }
}

#[camber::test]
async fn e2e_async_server() {
    let addr = common::spawn_server(async_server_router());

    let (scheduled_tx, mut scheduled_rx) = tokio::sync::mpsc::unbounded_channel();
    let schedule_handle = camber::schedule::every(Duration::from_millis(100), move || {
        scheduled_tx.send(()).unwrap();
    })
    .unwrap();

    let (results, elapsed) = run_concurrent_async_requests(addr);
    assert_concurrent_async_responses(&results, elapsed);

    // Sync-style handler works alongside async
    let sync_resp = http::get(&format!("http://{addr}/sync")).await.unwrap();
    assert_eq!(sync_resp.status(), 200);
    assert_eq!(sync_resp.body(), "sync");

    assert_scheduled_events(&mut scheduled_rx).await;
    schedule_handle.cancel();

    runtime::request_shutdown();
}
