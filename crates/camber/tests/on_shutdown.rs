use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

#[camber::test]
async fn on_shutdown_completes_after_request_shutdown() {
    let flag = Arc::new(AtomicBool::new(false));

    camber::spawn_async({
        let flag = Arc::clone(&flag);
        async move {
            camber::task::on_shutdown().await;
            flag.store(true, Ordering::Release);
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(!flag.load(Ordering::Acquire), "flag set before shutdown");

    camber::runtime::request_shutdown();
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(flag.load(Ordering::Acquire), "flag not set after shutdown");
}

#[camber::test]
async fn on_shutdown_completes_immediately_if_already_shutting_down() {
    camber::runtime::request_shutdown();

    let flag = Arc::new(AtomicBool::new(false));

    camber::spawn_async({
        let flag = Arc::clone(&flag);
        async move {
            camber::task::on_shutdown().await;
            flag.store(true, Ordering::Release);
        }
    });

    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        flag.load(Ordering::Acquire),
        "flag not set when already shutting down"
    );
}

#[camber::test]
async fn on_shutdown_stops_schedule_loop() {
    let counter = Arc::new(AtomicU32::new(0));

    let handle = camber::schedule::every_async(Duration::from_millis(20), {
        let c = Arc::clone(&counter);
        move || {
            let c = Arc::clone(&c);
            async move {
                c.fetch_add(1, Ordering::Release);
            }
        }
    })
    .unwrap();

    camber::spawn_async({
        let handle = handle.clone();
        async move {
            camber::task::on_shutdown().await;
            handle.cancel();
        }
    });

    tokio::time::sleep(Duration::from_millis(100)).await;
    camber::runtime::request_shutdown();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let final_count = counter.load(Ordering::Acquire);
    // After a short settle, counter should not increment further.
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert_eq!(
        counter.load(Ordering::Acquire),
        final_count,
        "schedule kept firing after on_shutdown cancelled it"
    );
}

#[camber::test]
async fn on_shutdown_works_in_select() {
    let counter = Arc::new(AtomicU32::new(0));

    camber::spawn_async({
        let counter = Arc::clone(&counter);
        async move {
            let mut interval = tokio::time::interval(Duration::from_millis(50));
            interval.tick().await; // skip immediate first tick

            loop {
                tokio::select! {
                    _ = camber::task::on_shutdown() => break,
                    _ = interval.tick() => {
                        counter.fetch_add(1, Ordering::Release);
                    }
                }
            }
        }
    });

    tokio::time::sleep(Duration::from_millis(175)).await;
    camber::runtime::request_shutdown();
    tokio::time::sleep(Duration::from_millis(100)).await;

    let count = counter.load(Ordering::Acquire);
    assert!(count >= 2 && count <= 5, "expected ~3 ticks, got {count}");
}
