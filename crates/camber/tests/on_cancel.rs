mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[test]
fn on_cancel_triggers_shutdown() {
    // on_cancel(sleep(50ms)) triggers shutdown, stopping a 20ms schedule loop.
    common::test_runtime()
        .run(|| {
            camber::on_cancel(tokio::time::sleep(Duration::from_millis(50)));

            let counter = Arc::new(AtomicU32::new(0));
            let _handle = camber::schedule::every_async(Duration::from_millis(20), {
                let c = Arc::clone(&counter);
                move || {
                    let c = Arc::clone(&c);
                    async move {
                        c.fetch_add(1, Ordering::Release);
                    }
                }
            })
            .unwrap();

            std::thread::sleep(Duration::from_millis(200));
            let count = counter.load(Ordering::Acquire);
            assert!(count <= 4, "schedule kept firing after on_cancel: {count}");
        })
        .unwrap();

    // CancellationToken passed to on_cancel stops schedules on cancel.
    common::test_runtime().run(|| {
        let token = tokio_util::sync::CancellationToken::new();
        camber::on_cancel(token.clone().cancelled_owned());

        let counter = Arc::new(AtomicU32::new(0));
        let _handle = camber::schedule::every_async(Duration::from_millis(20), {
            let c = Arc::clone(&counter);
            move || {
                let c = Arc::clone(&c);
                async move {
                    c.fetch_add(1, Ordering::Release);
                }
            }
        })
        .unwrap();

        std::thread::sleep(Duration::from_millis(100));
        let before_cancel = counter.load(Ordering::Acquire);
        assert!(
            before_cancel >= 3,
            "schedule should fire before cancel: {before_cancel}"
        );

        token.cancel();
        std::thread::sleep(Duration::from_millis(100));
        let after_cancel = counter.load(Ordering::Acquire);
        assert!(
            after_cancel <= before_cancel + 1,
            "schedule kept firing after token cancel: before={before_cancel}, after={after_cancel}"
        );
    }).unwrap();

    // on_cancel(sleep(150ms)) bounds a 50ms loop to ~2-3 ticks.
    common::test_runtime()
        .run(|| {
            camber::on_cancel(tokio::time::sleep(Duration::from_millis(150)));

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

            std::thread::sleep(Duration::from_millis(300));
            let count = counter.load(Ordering::Acquire);
            assert!(count <= 4, "schedule kept firing after cancel: {count}");
        })
        .unwrap();
}
