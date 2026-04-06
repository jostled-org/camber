use camber::signals::spawn_signal_watcher;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

#[tokio::test]
async fn signal_watcher_sets_flag_on_ctrl_c() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());

    let handle = spawn_signal_watcher(shutdown.clone(), notify.clone());

    // Let the spawned task register signal handlers with the OS.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    signal_hook::low_level::raise(signal_hook::consts::SIGINT).expect("failed to raise SIGINT");

    tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("signal watcher did not complete within 1s")
        .expect("signal watcher task panicked");

    assert!(shutdown.load(Ordering::Acquire));
}

#[tokio::test]
async fn signal_watcher_sets_flag_on_sigterm() {
    let shutdown = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());

    let handle = spawn_signal_watcher(shutdown.clone(), notify.clone());

    // Let the spawned task register signal handlers with the OS.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    signal_hook::low_level::raise(signal_hook::consts::SIGTERM).expect("failed to raise SIGTERM");

    tokio::time::timeout(std::time::Duration::from_secs(1), handle)
        .await
        .expect("signal watcher did not complete within 1s")
        .expect("signal watcher task panicked");

    assert!(shutdown.load(Ordering::Acquire));
}
