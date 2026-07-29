use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::common::run_in_child;

/// Bounds the parent's wait on the whole isolated child run.
const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds the child's own wait on its watcher.
///
/// Deliberately short of the parent's bound: a watcher that never observes its
/// signal must fail inside the child, under the message below, rather than race
/// the parent's kill — which reports only that the child died.
const WATCHER_TIMEOUT: Duration = Duration::from_secs(2);

#[test]
fn signal_watcher_sets_flag_on_ctrl_c() {
    run_signal_contract(
        "runtime_signals::signal_watcher_sets_flag_on_ctrl_c",
        "phase5-signal-sigint",
        "PHASE5_SIGINT_OBSERVED",
        signal_hook::consts::SIGINT,
    );
}

#[test]
fn signal_watcher_sets_flag_on_sigterm() {
    run_signal_contract(
        "runtime_signals::signal_watcher_sets_flag_on_sigterm",
        "phase5-signal-sigterm",
        "PHASE5_SIGTERM_OBSERVED",
        signal_hook::consts::SIGTERM,
    );
}

/// Raise `signal` at a live watcher, in a process of its own.
///
/// The child is where the whole contract lives: a raised signal reaches the
/// raiser's own process, and this test binary runs every other case in that
/// same process. The parent only reports whether the child came back clean.
fn run_signal_contract(test_name: &str, mode: &str, marker: &str, signal: i32) {
    run_in_child(test_name, mode, marker, SIGNAL_TIMEOUT, || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let shutdown = Arc::new(AtomicBool::new(false));
            let notify = Arc::new(tokio::sync::Notify::new());
            let watcher = camber::signals::spawn_signal_watcher(Arc::clone(&shutdown), notify);
            signal_hook::low_level::raise(signal).unwrap();
            // Bounded: a watcher that never observes its signal would park the
            // child on a task that never resolves, and only the parent's kill
            // would end it.
            tokio::time::timeout(WATCHER_TIMEOUT, watcher)
                .await
                .expect("the signal watcher never returned")
                .expect("the signal watcher task failed");
            assert!(
                shutdown.load(Ordering::Acquire),
                "the watcher returned without raising the shutdown flag"
            );
        });
    });
}
