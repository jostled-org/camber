use camber::{RuntimeError, runtime};

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

#[test]
fn runtime_runs_closure_and_returns_unit() {
    runtime::run(|| {}).unwrap();
}

#[test]
fn runtime_runs_closure_and_returns_value() {
    let result = runtime::run(|| 42).unwrap();
    assert_eq!(result, 42);
}
