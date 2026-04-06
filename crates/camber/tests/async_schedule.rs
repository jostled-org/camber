use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[camber::test]
async fn every_async_fires_on_interval() {
    let counter = Arc::new(AtomicU32::new(0));

    let handle = camber::schedule::every_async(Duration::from_millis(50), {
        let c = Arc::clone(&counter);
        move || {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::Release);
            }
        }
    })
    .unwrap();

    tokio::time::sleep(Duration::from_millis(175)).await;
    assert_eq!(counter.load(Ordering::Acquire), 3);
    handle.cancel();

    // Verify no further increments after cancel.
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(counter.load(Ordering::Acquire), 3);
}

#[camber::test]
async fn every_async_trigger_wakes_immediately() {
    let counter = Arc::new(AtomicU32::new(0));

    let handle = camber::schedule::every_async(Duration::from_secs(10), {
        let c = Arc::clone(&counter);
        move || {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::Release);
            }
        }
    })
    .unwrap();

    handle.trigger();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(counter.load(Ordering::Acquire), 1);
    handle.cancel();
}

#[camber::test]
async fn every_async_stops_on_shutdown() {
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
    let before_shutdown = counter.load(Ordering::Acquire);
    assert!(before_shutdown >= 2);

    camber::runtime::request_shutdown();
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(counter.load(Ordering::Acquire), before_shutdown);
}

#[camber::test]
async fn every_async_cancel_stops_task() {
    let counter = Arc::new(AtomicU32::new(0));

    let handle = camber::schedule::every_async(Duration::from_millis(50), {
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
    handle.cancel();

    let after_cancel = counter.load(Ordering::Acquire);
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(counter.load(Ordering::Acquire), after_cancel);
}
