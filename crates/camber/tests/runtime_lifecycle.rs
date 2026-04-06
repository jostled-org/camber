use camber::runtime;

#[test]
fn runtime_runs_closure_and_returns_unit() {
    runtime::run(|| {}).unwrap();
}

#[test]
fn runtime_runs_closure_and_returns_value() {
    let result = runtime::run(|| 42).unwrap();
    assert_eq!(result, 42);
}
