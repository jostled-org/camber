use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[camber::test]
async fn external_trigger_wakes_loop() {
    let trigger = Arc::new(tokio::sync::Notify::new());
    let counter = Arc::new(AtomicU32::new(0));

    let handle =
        camber::schedule::every_async_notified(Duration::from_secs(10), Arc::clone(&trigger), {
            let c = Arc::clone(&counter);
            move || {
                let c = Arc::clone(&c);
                async move {
                    c.fetch_add(1, Ordering::Release);
                }
            }
        })
        .unwrap();

    trigger.notify_one();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(counter.load(Ordering::Acquire), 1);
    handle.cancel();
}

#[camber::test]
async fn handle_trigger_also_works() {
    let trigger = Arc::new(tokio::sync::Notify::new());
    let counter = Arc::new(AtomicU32::new(0));

    let handle =
        camber::schedule::every_async_notified(Duration::from_secs(10), Arc::clone(&trigger), {
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
async fn interval_still_fires() {
    let trigger = Arc::new(tokio::sync::Notify::new());
    let counter = Arc::new(AtomicU32::new(0));

    let handle =
        camber::schedule::every_async_notified(Duration::from_millis(50), Arc::clone(&trigger), {
            let c = Arc::clone(&counter);
            move || {
                let c = Arc::clone(&c);
                async move {
                    c.fetch_add(1, Ordering::Release);
                }
            }
        })
        .unwrap();

    // Don't trigger externally — let the interval fire on its own.
    tokio::time::sleep(Duration::from_millis(175)).await;
    let count = counter.load(Ordering::Acquire);
    assert_eq!(
        count, 3,
        "interval should fire ~3 times in 175ms: got {count}"
    );
    handle.cancel();
}

#[camber::test]
async fn cancel_stops_loop() {
    let trigger = Arc::new(tokio::sync::Notify::new());
    let counter = Arc::new(AtomicU32::new(0));

    let handle =
        camber::schedule::every_async_notified(Duration::from_millis(50), Arc::clone(&trigger), {
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
