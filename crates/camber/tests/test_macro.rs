use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[camber::test]
async fn test_macro_runs_async_body() {
    assert_eq!(1 + 1, 2);
}

#[camber::test]
async fn test_macro_supports_spawn_async() {
    let result = camber::spawn_async(async { 42 }).await;
    assert_eq!(result.unwrap_or(0), 42);
}

#[camber::test]
async fn test_macro_supports_schedule() {
    let counter = Arc::new(AtomicU32::new(0));

    let _handle = camber::schedule::every_async(Duration::from_millis(50), {
        let c = Arc::clone(&counter);
        move || {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::Release);
            }
        }
    })
    .unwrap();

    tokio::time::sleep(Duration::from_millis(125)).await;
    assert!(counter.load(Ordering::Acquire) >= 2);
}
