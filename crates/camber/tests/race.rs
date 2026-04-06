use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use camber::RuntimeError;

#[camber::test]
async fn race_returns_first_to_complete() {
    let fast = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        "fast"
    };
    let slow = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        "slow"
    };

    let start = Instant::now();
    let result = camber::task::race(fast, slow).await;
    let elapsed = start.elapsed();

    assert_eq!(result, "fast");
    assert!(elapsed < Duration::from_millis(100));
}

#[camber::test]
async fn race_all_returns_first_from_vec() {
    let delays = [50, 200, 10, 300, 100];
    let futures: Vec<_> = delays
        .iter()
        .enumerate()
        .map(|(i, &ms)| {
            Box::pin(async move {
                tokio::time::sleep(Duration::from_millis(ms)).await;
                i
            })
        })
        .collect();

    let result = camber::task::race_all(futures).await.unwrap();
    assert_eq!(result, 2);
}

#[camber::test]
async fn race_cancels_loser() {
    let completed = Arc::new(AtomicBool::new(false));
    let completed_inner = Arc::clone(&completed);

    let fast = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        "done"
    };
    let slow = async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        completed_inner.store(true, Ordering::Release);
        "slow"
    };

    let _ = camber::task::race(fast, slow).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!completed.load(Ordering::Acquire));
}

#[camber::test]
async fn race_propagates_error_from_first() {
    let fast = async {
        tokio::time::sleep(Duration::from_millis(10)).await;
        Err::<&str, RuntimeError>(RuntimeError::Timeout)
    };
    let slow = async {
        tokio::time::sleep(Duration::from_millis(200)).await;
        Ok("ok")
    };

    let result = camber::task::race(fast, slow).await;
    assert!(matches!(result, Err(RuntimeError::Timeout)));
}

#[camber::test]
async fn race_all_empty_returns_error() {
    let futures: Vec<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>> = vec![];
    let result = camber::task::race_all(futures).await;
    assert!(matches!(result, Err(RuntimeError::InvalidArgument(_))));
}
