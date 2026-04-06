mod common;

use camber::{RuntimeError, channel, runtime, spawn};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn cancel_stops_task_at_next_io_boundary() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<i32>(1);
        // Barrier: task signals after first recv completes (uses std channel
        // to avoid camber cancellation checks).
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel::<()>(0);

        let handle = spawn(move || -> Result<(), RuntimeError> {
            // First recv should succeed (we send a value to unblock it)
            let _val = rx.recv()?;
            // Signal: first recv done, safe to set cancel flag
            let _ = ready_tx.send(());
            // Second recv should return Cancelled (flag set between recvs)
            match rx.recv() {
                Err(RuntimeError::Cancelled) => Ok(()),
                other => panic!("expected Cancelled, got {other:?}"),
            }
        });

        // Send a value so the first recv succeeds
        tx.send(1).unwrap();
        // Wait until the task has completed the first recv
        ready_rx.recv().unwrap();
        // Now cancel — the second recv will see the flag
        handle.cancel();

        // Task should exit cleanly via ? on Cancelled
        let result = handle.join();
        assert!(result.is_ok(), "expected Ok, got {result:?}");
    })
    .unwrap();
}

#[test]
fn cancel_before_task_starts_io() {
    runtime::run(|| {
        let handle = spawn(|| -> Result<(), RuntimeError> {
            // Sleep before doing IO — cancellation happens during sleep
            thread::sleep(Duration::from_millis(100));
            // http::get should return Cancelled without making a request
            common::block_on(camber::http::get("http://127.0.0.1:1"))?;
            Ok(())
        });

        // Cancel immediately (before sleep finishes)
        handle.cancel();

        let result = handle.join().unwrap();
        let err = result.unwrap_err();
        assert!(
            matches!(err, RuntimeError::Cancelled),
            "expected Cancelled, got {err:?}"
        );
    })
    .unwrap();
}

#[test]
fn join_after_cancel_returns_result() {
    runtime::run(|| {
        let handle = spawn(|| 42);

        // Join first — task completes before cancel
        let result = handle.join().unwrap();
        assert_eq!(result, 42);
        // cancel() after join is consumed — JoinHandle moved into join(),
        // so we test cancel on a completed-but-not-yet-joined task instead.
    })
    .unwrap();

    // Variant: cancel after task completes but before join
    runtime::run(|| {
        let handle = spawn(|| {
            thread::sleep(Duration::from_millis(10));
            42
        });
        thread::sleep(Duration::from_millis(50));
        handle.cancel();
        let result = handle.join().unwrap();
        assert_eq!(result, 42);
    })
    .unwrap();
}

#[test]
fn channel_iter_respects_cancellation() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<i32>(10);
        let count = Arc::new(AtomicUsize::new(0));
        let count_inner = Arc::clone(&count);

        // Barrier: task signals it has started iterating
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel::<()>(0);

        let handle = spawn(move || {
            started_tx.send(()).unwrap();
            for _val in rx.iter() {
                count_inner.fetch_add(1, Ordering::SeqCst);
            }
        });

        // Wait for the task to start iterating
        started_rx.recv().unwrap();

        // Cancel the task
        handle.cancel();

        // Send one value to unblock the current iteration
        let _ = tx.send(99);

        // Task should exit (iter stops yielding after cancellation)
        let result = handle.join();
        assert!(result.is_ok(), "expected Ok, got {result:?}");

        // Should have received at most 1 item (the unblock value)
        let received = count.load(Ordering::SeqCst);
        assert!(received <= 1, "expected at most 1 item, got {received}");
    })
    .unwrap();
}

#[test]
fn cancel_detected_after_io_completes() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<i32>(1);
        let reached_second_op = Arc::new(AtomicBool::new(false));
        let reached_inner = Arc::clone(&reached_second_op);

        let handle = spawn(move || -> Result<i32, RuntimeError> {
            // Block on recv — value will arrive, then cancel is set
            let val = rx.recv()?;
            // If we get here, recv succeeded. The next IO should detect cancel.
            reached_inner.store(true, Ordering::Release);
            // This recv should return Cancelled (cancel set after first recv)
            let second = rx.recv()?;
            Ok(val + second)
        });

        // Send value to unblock the first recv
        tx.send(10).unwrap();
        // Small delay to let the task receive the value
        thread::sleep(Duration::from_millis(20));
        // Cancel — task should detect this on the next IO operation
        handle.cancel();

        let result = handle.join().unwrap();
        assert!(
            reached_second_op.load(Ordering::Acquire),
            "task should have passed first recv"
        );
        assert!(
            matches!(result, Err(RuntimeError::Cancelled)),
            "expected Cancelled, got {result:?}"
        );
    })
    .unwrap();
}
