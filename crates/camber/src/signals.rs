use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::signal::unix::{Signal, SignalKind, signal};

/// Spawn an async task that watches for OS signals.
///
/// On SIGINT/SIGTERM: sets `shutdown` to true and notifies waiters.
pub fn spawn_signal_watcher(
    shutdown: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    let sigint = signal(SignalKind::interrupt()).ok();
    let sigterm = signal(SignalKind::terminate()).ok();
    tokio::spawn(async move {
        wait_for_shutdown(sigint, sigterm).await;
        shutdown.store(true, Ordering::Release);
        notify.notify_waiters();
    })
}

async fn wait_for_shutdown(mut sigint: Option<Signal>, mut sigterm: Option<Signal>) {
    match (&mut sigint, &mut sigterm) {
        (Some(sigint), Some(sigterm)) => tokio::select! {
            _ = sigint.recv() => {}
            _ = sigterm.recv() => {}
        },
        (Some(sigint), None) => {
            sigint.recv().await;
        }
        (None, Some(sigterm)) => {
            sigterm.recv().await;
        }
        (None, None) => std::future::pending().await,
    }
}
