mod common;

use camber::{RuntimeError, runtime};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

#[test]
fn scheduled_task_fires_on_interval() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let counter = Arc::new(AtomicU32::new(0));
            let c = Arc::clone(&counter);

            let _handle = camber::schedule::every(Duration::from_millis(100), move || {
                c.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();

            std::thread::sleep(Duration::from_millis(350));
            let count = counter.load(Ordering::SeqCst);

            // Should have fired ~3 times (±1 due to timing)
            assert!(
                (2..=4).contains(&count),
                "expected 2-4 firings, got {count}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn scheduled_task_stops_on_shutdown() {
    common::test_runtime().shutdown_timeout(Duration::from_secs(2)).run(|| {
        let counter = Arc::new(AtomicU32::new(0));
        let c = Arc::clone(&counter);

        let _handle = camber::schedule::every(Duration::from_millis(50), move || {
            c.fetch_add(1, Ordering::SeqCst);
        }).unwrap();

        std::thread::sleep(Duration::from_millis(200));
        runtime::request_shutdown();

        let count_at_shutdown = counter.load(Ordering::SeqCst);

        std::thread::sleep(Duration::from_millis(200));
        let count_after = counter.load(Ordering::SeqCst);

        // Counter should stop incrementing after shutdown (allow +1 for in-flight)
        assert!(
            count_after <= count_at_shutdown + 1,
            "task kept firing after shutdown: at_shutdown={count_at_shutdown}, after={count_after}"
        );
    }).unwrap();
}

#[test]
fn cron_expression_parsing() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let handle = camber::schedule::cron("*/5 * * * *", || {});

            // Valid cron expression should succeed
            assert!(handle.is_ok(), "valid cron expression should parse");

            // Cancel immediately
            handle.unwrap().cancel();

            // Invalid cron expression should fail
            let bad = camber::schedule::cron("not a cron", || {});
            assert!(bad.is_err(), "invalid cron expression should error");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn multiple_scheduled_tasks_independent() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let counter_fast = Arc::new(AtomicU32::new(0));
            let counter_slow = Arc::new(AtomicU32::new(0));

            let cf = Arc::clone(&counter_fast);
            let cs = Arc::clone(&counter_slow);

            let _h1 = camber::schedule::every(Duration::from_millis(100), move || {
                cf.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();

            let _h2 = camber::schedule::every(Duration::from_millis(200), move || {
                cs.fetch_add(1, Ordering::SeqCst);
            })
            .unwrap();

            std::thread::sleep(Duration::from_millis(450));

            let fast = counter_fast.load(Ordering::SeqCst);
            let slow = counter_slow.load(Ordering::SeqCst);

            // Fast (~100ms): expect 3-5 firings in 450ms
            assert!(
                (3..=5).contains(&fast),
                "fast counter: expected 3-5, got {fast}"
            );
            // Slow (~200ms): expect 1-3 firings in 450ms
            assert!(
                (1..=3).contains(&slow),
                "slow counter: expected 1-3, got {slow}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn cron_parse_error_is_schedule_variant() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let err = camber::schedule::cron("not a cron", || {}).unwrap_err();

            assert!(
                matches!(err, RuntimeError::Schedule(_)),
                "expected Schedule variant, got: {err}"
            );

            let msg = err.to_string();
            assert!(
                msg.contains("schedule"),
                "error message should contain 'schedule': {msg}"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}
