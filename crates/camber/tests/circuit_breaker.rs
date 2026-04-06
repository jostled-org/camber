mod common;

use camber::circuit_breaker;
use camber::http::{self, Router};
use camber::{Resource, RuntimeError, runtime};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

/// Shared state for observing mock resource behavior from tests.
struct MockState {
    check_count: AtomicU32,
    healthy: AtomicBool,
    shutdown_called: AtomicBool,
}

impl MockState {
    fn new(healthy: bool) -> Arc<Self> {
        Arc::new(Self {
            check_count: AtomicU32::new(0),
            healthy: AtomicBool::new(healthy),
            shutdown_called: AtomicBool::new(false),
        })
    }

    fn calls(&self) -> u32 {
        self.check_count.load(Ordering::Acquire)
    }

    fn set_healthy(&self, v: bool) {
        self.healthy.store(v, Ordering::Release);
    }
}

/// Mock resource that delegates to shared state for test observability.
struct MockResource {
    label: &'static str,
    state: Arc<MockState>,
}

impl MockResource {
    fn new(label: &'static str, state: &Arc<MockState>) -> Self {
        Self {
            label,
            state: Arc::clone(state),
        }
    }
}

impl Resource for MockResource {
    fn name(&self) -> &str {
        self.label
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        self.state.check_count.fetch_add(1, Ordering::AcqRel);
        match self.state.healthy.load(Ordering::Acquire) {
            true => Ok(()),
            false => Err(RuntimeError::InvalidArgument("unhealthy".into())),
        }
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        self.state.shutdown_called.store(true, Ordering::Release);
        Ok(())
    }
}

#[test]
fn circuit_breaker_stays_closed_when_healthy() {
    let state = MockState::new(true);
    let cb = circuit_breaker::wrap(MockResource::new("healthy-db", &state))
        .failure_threshold(3)
        .build();

    for _ in 0..10 {
        assert!(cb.health_check().is_ok());
    }
    assert_eq!(state.calls(), 10, "all checks should delegate to inner");
}

#[test]
fn circuit_breaker_opens_after_threshold_failures() {
    let state = MockState::new(false);
    let cb = circuit_breaker::wrap(MockResource::new("failing-db", &state))
        .failure_threshold(3)
        .cooldown(Duration::from_secs(60))
        .build();

    // First 3 calls delegate to inner (all fail, reaching threshold)
    for i in 0..3 {
        assert!(cb.health_check().is_err(), "call {i} should fail");
    }
    assert_eq!(state.calls(), 3);

    // Next 2 calls: circuit is open, inner is NOT called
    for i in 0..2 {
        assert!(cb.health_check().is_err(), "open call {i} should fail");
    }
    assert_eq!(state.calls(), 3, "open circuit should not call inner");
}

#[test]
fn circuit_breaker_half_opens_after_cooldown() {
    let state = MockState::new(false);
    let cb = circuit_breaker::wrap(MockResource::new("recovering-db", &state))
        .failure_threshold(3)
        .cooldown(Duration::from_secs(1))
        .build();

    // Trip the circuit
    for _ in 0..3 {
        let _ = cb.health_check();
    }
    assert_eq!(state.calls(), 3);

    // Wait for cooldown
    std::thread::sleep(Duration::from_millis(1100));

    // Resource is now healthy
    state.set_healthy(true);

    // Half-open: probes inner, succeeds, circuit closes
    assert!(cb.health_check().is_ok(), "half-open probe should succeed");
    assert_eq!(state.calls(), 4, "half-open should probe inner");

    // Circuit is closed again — next call also delegates
    assert!(cb.health_check().is_ok());
    assert_eq!(state.calls(), 5);
}

#[test]
fn circuit_breaker_delegates_name_and_shutdown() {
    let state = MockState::new(true);
    let cb = circuit_breaker::wrap(MockResource::new("test-db", &state))
        .failure_threshold(3)
        .build();

    assert_eq!(cb.name(), "test-db");
    assert!(cb.shutdown().is_ok());
    assert!(
        state.shutdown_called.load(Ordering::Acquire),
        "shutdown should delegate to inner"
    );
}

#[test]
fn circuit_breaker_composes_with_runtime() {
    let cb = circuit_breaker::wrap(MockResource::new("runtime-db", &MockState::new(true)))
        .failure_threshold(3)
        .build();

    common::test_runtime()
        .resource(cb)
        .run(|| {
            let addr = common::spawn_server(Router::new());
            let resp = common::block_on(http::get(&format!("http://{addr}/health"))).unwrap();
            assert_eq!(resp.status(), 200);
            assert!(resp.body().contains(r#""runtime-db":"ok""#));
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn failure_threshold_zero_clamped_to_one() {
    let state = MockState::new(false);
    let cb = circuit_breaker::wrap(MockResource::new("clamp-db", &state))
        .failure_threshold(0)
        .cooldown(Duration::from_secs(60))
        .build();

    // A single failure should trip the circuit (threshold clamped to 1)
    assert!(cb.health_check().is_err());
    assert_eq!(state.calls(), 1);

    // Circuit is now open — inner not called
    assert!(cb.health_check().is_err());
    assert_eq!(state.calls(), 1, "open circuit should not call inner");
}

#[test]
fn cooldown_zero_clamped_to_one_second() {
    let state = MockState::new(false);
    let cb = circuit_breaker::wrap(MockResource::new("cooldown-db", &state))
        .failure_threshold(1)
        .cooldown(Duration::ZERO)
        .build();

    // Trip the circuit
    assert!(cb.health_check().is_err());
    assert_eq!(state.calls(), 1);

    // Immediately after: cooldown is 1s, so circuit stays open
    assert!(cb.health_check().is_err());
    assert_eq!(
        state.calls(),
        1,
        "zero cooldown clamped to 1s, circuit stays open"
    );
}

#[test]
fn open_circuit_error_includes_resource_name() {
    let state = MockState::new(false);
    let cb = circuit_breaker::wrap(MockResource::new("named-db", &state))
        .failure_threshold(1)
        .cooldown(Duration::from_secs(60))
        .build();

    // Trip the circuit
    let _ = cb.health_check();

    // Open-circuit error should mention the resource name
    let err = cb.health_check().unwrap_err();
    let msg = format!("{err}");
    assert!(
        msg.contains("named-db"),
        "error should contain resource name, got: {msg}"
    );
}
