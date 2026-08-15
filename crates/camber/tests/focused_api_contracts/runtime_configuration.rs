use std::path::Path;

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

/// 1.T1: run propagates TLS errors instead of exiting the process.
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
