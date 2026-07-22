use std::io::Write;
use std::time::Duration;

use crate::common::{IsolatedRun, is_private_child, run_isolated_exact};

const PROCESS_TIMEOUT: Duration = Duration::from_secs(5);

fn assert_marker_once(run: &IsolatedRun, marker: &str) {
    let count = String::from_utf8_lossy(run.stdout())
        .matches(marker)
        .count()
        + String::from_utf8_lossy(run.stderr())
            .matches(marker)
            .count();
    assert_eq!(count, 1, "expected one isolated marker for {marker}");
}

fn assert_child_success(run: &IsolatedRun) {
    assert!(
        run.success(),
        "isolated contract failed\nstdout: {}\nstderr: {}",
        String::from_utf8_lossy(run.stdout()),
        String::from_utf8_lossy(run.stderr())
    );
}

#[test]
fn init_metrics_returns_handle() {
    const MODE: &str = "phase5-metrics-install-once";
    const MARKER: &str = "PHASE5_METRICS_INSTALLED_ONCE";

    if is_private_child(MODE) {
        let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        assert!(
            metrics::set_global_recorder(recorder).is_ok(),
            "isolated metrics recorder was already installed"
        );
        metrics::counter!("test_requests_total").increment(1);
        assert!(handle.render().contains("test_requests_total"));
        println!("{MARKER}");
        std::io::stdout().flush().unwrap();
        return;
    }

    let parent_id = std::process::id();
    let run = run_isolated_exact(
        "install_once::init_metrics_returns_handle",
        MODE,
        MARKER,
        PROCESS_TIMEOUT,
    )
    .unwrap();
    assert_child_success(&run);
    assert_marker_once(&run, MARKER);
    assert_eq!(std::process::id(), parent_id);
}

#[test]
fn init_logging_text_format_does_not_panic() {
    run_logging_contract(
        "install_once::init_logging_text_format_does_not_panic",
        "phase5-logging-text-install-once",
        "PHASE5_TEXT_LOGGING_INSTALLED_ONCE",
        camber::logging::LogFormat::Text,
        "PHASE5_TEXT_LOG_EVENT",
    );
}

#[test]
fn init_logging_json_format_does_not_panic() {
    run_logging_contract(
        "install_once::init_logging_json_format_does_not_panic",
        "phase5-logging-json-install-once",
        "PHASE5_JSON_LOGGING_INSTALLED_ONCE",
        camber::logging::LogFormat::Json,
        "PHASE5_JSON_LOG_EVENT",
    );
}

fn run_logging_contract(
    test_name: &str,
    mode: &str,
    marker: &str,
    format: camber::logging::LogFormat,
    event: &str,
) {
    if is_private_child(mode) {
        camber::logging::init_logging(format, camber::logging::LogLevel::Info);
        camber::tracing::info!(message = event);
        println!("{marker}");
        std::io::stdout().flush().unwrap();
        return;
    }

    let parent_id = std::process::id();
    let run = run_isolated_exact(test_name, mode, marker, PROCESS_TIMEOUT).unwrap();
    assert_child_success(&run);
    assert_marker_once(&run, marker);
    let output = format!(
        "{}{}",
        String::from_utf8_lossy(run.stdout()),
        String::from_utf8_lossy(run.stderr())
    );
    assert!(
        output.contains(event),
        "isolated log event missing: {output}"
    );
    assert_eq!(std::process::id(), parent_id);
}
