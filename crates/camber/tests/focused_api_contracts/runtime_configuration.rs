use std::path::Path;

use camber::{RuntimeError, runtime};

#[test]
fn run_returns_error_on_invalid_config() {
    let result = runtime::builder()
        .worker_threads(0)
        .run(|| "should not reach here");

    match result {
        Err(RuntimeError::InvalidArgument(msg)) => {
            assert!(
                msg.contains("worker_threads"),
                "error should mention worker_threads, got: {msg}"
            );
        }
        Ok(_) => panic!("expected Err, got Ok"),
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
