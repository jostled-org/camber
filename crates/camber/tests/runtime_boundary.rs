mod common;

use camber::{RuntimeError, runtime};
use std::path::Path;

/// 1.T1: run propagates TLS errors instead of exiting the process.
/// If the process exits, this test will never complete — so completing is the proof.
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

/// 1.T2: runtime::builder().run() returns Result.
#[test]
fn runtime_run_returns_result() {
    let result = runtime::builder().run(|| 42);
    assert_eq!(result.unwrap(), 42);

    let result = runtime::builder().run(|| Err::<i32, _>(RuntimeError::Config("bad".into())));
    let inner = result.unwrap();
    assert!(inner.is_err(), "expected inner Err");
}

/// 1.T2b: the free function runtime::run also returns Result.
#[test]
fn runtime_run_free_fn_returns_result() {
    let result = runtime::run(|| 42);
    assert_eq!(result.unwrap(), 42);
}
