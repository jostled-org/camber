use crate::common::BOUND;
use crate::scope_builders::scope_runtime;
use camber::runtime_test_support::wait_scope_closing;
use camber::{RuntimeError, runtime};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Closure return fires `ScopeClosing` so perpetual children can exit, and it
/// is not a shutdown request: `ShutdownSignal` stays unset.
#[test]
fn closure_return_fires_scope_closing_without_setting_shutdown() {
    let closing_observed = Arc::new(AtomicBool::new(false));
    let shutdown_observed = Arc::new(AtomicBool::new(true));
    let child_closing = Arc::clone(&closing_observed);
    let child_shutdown = Arc::clone(&shutdown_observed);

    scope_runtime(BOUND)
        .run(move || {
            camber::spawn_async(async move {
                let woke = tokio::time::timeout(BOUND, wait_scope_closing())
                    .await
                    .is_ok();
                child_closing.store(woke, Ordering::SeqCst);
                child_shutdown.store(runtime::is_shutting_down(), Ordering::SeqCst);
            });
        })
        .unwrap();

    assert!(
        closing_observed.load(Ordering::SeqCst),
        "closure return did not fire ScopeClosing"
    );
    assert!(
        !shutdown_observed.load(Ordering::SeqCst),
        "closure return set the shutdown signal"
    );
}

/// Requesting shutdown fires `ScopeClosing` and closes admission on the same
/// transition, so any later spawn is refused.
#[test]
fn request_shutdown_closes_admission_and_fires_scope_closing() {
    let closing_observed = Arc::new(AtomicBool::new(false));
    let child_closing = Arc::clone(&closing_observed);
    let (woke_tx, woke_rx) = std::sync::mpsc::channel::<()>();

    let refused = scope_runtime(BOUND)
        .run(move || {
            camber::spawn_async(async move {
                let woke = tokio::time::timeout(BOUND, wait_scope_closing())
                    .await
                    .is_ok();
                child_closing.store(woke, Ordering::SeqCst);
                woke_tx.send(()).unwrap();
            });

            runtime::request_shutdown();
            woke_rx.recv_timeout(BOUND).unwrap();
            camber::spawn(|| 42).join()
        })
        .unwrap();

    assert!(
        closing_observed.load(Ordering::SeqCst),
        "request_shutdown did not fire ScopeClosing"
    );
    assert!(
        matches!(refused, Err(RuntimeError::ScopeClosed)),
        "a spawn after request_shutdown was not refused: {refused:?}"
    );
}
