use camber::RuntimeError;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[camber::test]
async fn spawn_async_returns_value() {
    let result = camber::spawn_async(async { 42 }).await;
    assert_eq!(result.unwrap_or(0), 42);
}

#[camber::test]
async fn spawn_async_cancel_returns_error() {
    let handle = camber::spawn_async(async {
        tokio::time::sleep(Duration::from_secs(10)).await;
        42
    });
    handle.cancel();
    let result = handle.await;
    assert!(matches!(result, Err(RuntimeError::Cancelled)));
}

#[camber::test]
async fn spawn_async_participates_in_structured_concurrency() {
    let flag = Arc::new(AtomicBool::new(false));
    let flag_inner = Arc::clone(&flag);

    // Spawn without awaiting the handle — task runs in background.
    camber::spawn_async(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        flag_inner.store(true, Ordering::Release);
    });

    // Sleep longer than the spawned task to verify it ran concurrently.
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(
        flag.load(Ordering::Acquire),
        "spawned async task did not run"
    );
}

#[camber::test]
async fn spawn_async_panic_returns_error() {
    let result = camber::spawn_async(async {
        #[allow(clippy::panic)]
        {
            panic!("intentional test panic");
        }
    })
    .await;
    assert!(matches!(result, Err(RuntimeError::TaskPanicked(_))));
}

#[camber::test]
async fn timeout_returns_error_on_expiry() {
    let result = camber::timeout(
        Duration::from_millis(50),
        tokio::time::sleep(Duration::from_secs(10)),
    )
    .await;
    assert!(matches!(result, Err(RuntimeError::Timeout)));
}

#[camber::test]
async fn timeout_returns_value_on_success() {
    let result = camber::timeout(Duration::from_secs(1), async { 42 }).await;
    assert_eq!(result.unwrap_or(0), 42);
}
