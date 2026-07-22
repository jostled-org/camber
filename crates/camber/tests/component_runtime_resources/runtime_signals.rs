use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use crate::common::{is_private_child, run_signal_contract_exact};

const SIGNAL_TIMEOUT: Duration = Duration::from_secs(5);

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

fn run_signal_contract(test_name: &str, mode: &str, marker: &str, signal: i32) {
    if is_private_child(mode) {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let shutdown = Arc::new(AtomicBool::new(false));
            let notify = Arc::new(tokio::sync::Notify::new());
            let watcher = camber::signals::spawn_signal_watcher(Arc::clone(&shutdown), notify);
            signal_hook::low_level::raise(signal).unwrap();
            tokio::time::timeout(SIGNAL_TIMEOUT, watcher)
                .await
                .unwrap()
                .unwrap();
            assert!(shutdown.load(Ordering::Acquire));
        });
        println!("{marker}");
        std::io::stdout().flush().unwrap();
        return;
    }

    let parent_id = std::process::id();
    let run = run_signal_contract_exact(test_name, mode, marker, SIGNAL_TIMEOUT).unwrap();
    assert!(
        run.success(),
        "signal child failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(run.stdout()),
        String::from_utf8_lossy(run.stderr())
    );
    assert_eq!(std::process::id(), parent_id);
}
