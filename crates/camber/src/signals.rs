use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::signal::unix::{SignalKind, signal};

/// Spawn an async task that watches for OS signals.
///
/// On SIGINT/SIGTERM: sets `shutdown` to true and notifies waiters.
pub fn spawn_signal_watcher(
    shutdown: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        wait_for_shutdown().await;
        shutdown.store(true, Ordering::Release);
        notify.notify_waiters();
    })
}

async fn wait_for_shutdown() {
    let ctrl_c = tokio::signal::ctrl_c();

    let mut sigterm = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            // Unix signals unavailable — fall back to ctrl_c only.
            let _ = ctrl_c.await;
            return;
        }
    };

    tokio::select! {
        _ = ctrl_c => {}
        _ = sigterm.recv() => {}
    }
}
