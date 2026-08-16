use std::path::Path;
use std::time::Duration;

use camber::{RuntimeError, runtime};

#[test]
fn run_returns_error_on_invalid_config() {
    assert_refused(
        runtime::builder()
            .worker_threads(0)
            .run(|| "should not reach here"),
        "worker_threads",
    );
    // The infallible policy setters cannot report a refusal themselves, so the
    // value they could not take is held and returned here — before the runtime
    // is established, and naming the dimension that was refused.
    assert_refused(
        runtime::builder()
            .connection_limit(0)
            .run(|| "should not reach here"),
        "connection_limit",
    );
    assert_refused(
        runtime::builder()
            .connection_limit(usize::MAX)
            .run(|| "should not reach here"),
        "connection_limit",
    );
    // The two duration setters clamp their low end only, so a deadline past the
    // policy ceiling reaches the same held refusal rather than a silent lower
    // value. Both are entered here because the builder is the only spelling
    // that routes a duration through the clamp before the policy setter.
    assert_refused(
        runtime::builder()
            .header_timeout(Duration::MAX)
            .run(|| "should not reach here"),
        "header_timeout",
    );
    assert_refused(
        runtime::builder()
            .shutdown_timeout(Duration::MAX)
            .run(|| "should not reach here"),
        "shutdown_timeout",
    );
    // Zero is not invalid config on those two setters. The builder raises it to
    // the documented 100ms minimum and warns, so the runtime starts. This is
    // the boundary `docs/reference/runtime.md` publishes for the builder
    // spelling, and the policy setters refuse the same zero themselves.
    assert_eq!(
        runtime::builder()
            .worker_threads(1)
            .header_timeout(Duration::ZERO)
            .shutdown_timeout(Duration::ZERO)
            .run(|| "raised, not refused")
            .expect("a zero builder deadline is raised, never refused"),
        "raised, not refused",
    );
}

fn assert_refused<T: std::fmt::Debug>(result: Result<T, RuntimeError>, expected_name: &str) {
    match result {
        Err(RuntimeError::InvalidArgument(msg)) => {
            assert!(
                msg.contains(expected_name),
                "error should mention {expected_name}, got: {msg}"
            );
        }
        Ok(value) => panic!("expected Err, got Ok({value:?})"),
        Err(other) => panic!("expected InvalidArgument, got: {other}"),
    }
}

/// Run propagates TLS errors instead of exiting the process.
/// If the process exits, this test will never complete, so completing is the proof.
#[test]
fn runtime_run_propagates_tls_error_instead_of_exiting() {
    let result = runtime::builder()
        .tls_cert(Path::new("/nonexistent/cert.pem"))
        .tls_key(Path::new("/nonexistent/key.pem"))
        .run(|| {});

    assert!(result.is_err(), "expected TLS error, got Ok");
    assert!(
        matches!(&result.unwrap_err(), RuntimeError::Tls(_)),
        "expected RuntimeError::Tls"
    );
}
