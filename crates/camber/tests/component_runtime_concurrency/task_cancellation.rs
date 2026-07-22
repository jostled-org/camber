use camber::{RuntimeError, channel, runtime, spawn};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

#[test]
fn cancel_stops_task_at_next_io_boundary() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<i32>(1);
        let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(0);
        let handle = spawn(move || -> Result<(), RuntimeError> {
            assert_eq!(rx.recv()?, 1);
            ready_tx.send(()).unwrap();
            assert!(matches!(rx.recv(), Err(RuntimeError::Cancelled)));
            Ok(())
        });

        tx.send(1).unwrap();
        ready_rx.recv().unwrap();
        handle.cancel();
        assert!(matches!(handle.join(), Ok(Ok(()))));
    })
    .unwrap();
}

#[test]
fn cancel_before_task_starts_io() {
    runtime::run(|| {
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let handle = spawn(move || -> Result<(), RuntimeError> {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(camber::http::get("http://127.0.0.1:1"))
            })?;
            Ok(())
        });

        started_rx.recv().unwrap();
        handle.cancel();
        release_tx.send(()).unwrap();
        assert!(matches!(handle.join(), Ok(Err(RuntimeError::Cancelled))));
    })
    .unwrap();
}

#[test]
fn join_after_cancel_returns_result() {
    runtime::run(|| {
        assert_eq!(spawn(|| 42).join().unwrap(), 42);
    })
    .unwrap();

    runtime::run(|| {
        let (completed_tx, completed_rx) = std::sync::mpsc::sync_channel(0);
        let handle = spawn(move || {
            completed_tx.send(()).unwrap();
            42
        });
        completed_rx.recv().unwrap();
        handle.cancel();
        assert_eq!(handle.join().unwrap(), 42);
    })
    .unwrap();
}

#[test]
fn channel_iter_respects_cancellation() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<i32>(10);
        let count = Arc::new(AtomicUsize::new(0));
        let task_count = Arc::clone(&count);
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(0);
        let handle = spawn(move || {
            started_tx.send(()).unwrap();
            rx.iter().for_each(|_| {
                task_count.fetch_add(1, Ordering::SeqCst);
            });
        });

        started_rx.recv().unwrap();
        handle.cancel();
        drop(tx.send(99));
        assert!(handle.join().is_ok());
        assert!(count.load(Ordering::SeqCst) <= 1);
    })
    .unwrap();
}

#[test]
fn cancel_detected_after_io_completes() {
    runtime::run(|| {
        let (tx, rx) = channel::bounded::<i32>(1);
        let (first_complete_tx, first_complete_rx) = std::sync::mpsc::sync_channel(0);
        let handle = spawn(move || -> Result<i32, RuntimeError> {
            let value = rx.recv()?;
            first_complete_tx.send(()).unwrap();
            let second = rx.recv()?;
            Ok(value + second)
        });

        tx.send(10).unwrap();
        first_complete_rx.recv().unwrap();
        handle.cancel();
        assert!(matches!(handle.join(), Ok(Err(RuntimeError::Cancelled))));
    })
    .unwrap();
}
