use crate::common;

use camber::circuit_breaker;
use camber::http::{self, Router};
use camber::{Resource, RuntimeError, runtime};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::time::Duration;

const TRANSITION_TIMEOUT: Duration = Duration::from_secs(2);

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

/// Holds the successful probe at the half-open boundary so the state is
/// observable through the circuit breaker's public `Debug` implementation.
struct ControlledProbeResource {
    state: Arc<MockState>,
    entered: SyncSender<()>,
    release: Mutex<Receiver<()>>,
}

impl Resource for ControlledProbeResource {
    fn name(&self) -> &str {
        "recovering-db"
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        let call = self.state.check_count.fetch_add(1, Ordering::AcqRel) + 1;
        match self.state.healthy.load(Ordering::Acquire) {
            false => Err(RuntimeError::InvalidArgument("unhealthy".into())),
            true if call == 4 => {
                self.entered.send(()).unwrap();
                self.release
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .recv_timeout(TRANSITION_TIMEOUT)
                    .unwrap();
                Ok(())
            }
            true => Ok(()),
        }
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
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
    let (entered_tx, entered_rx) = sync_channel(1);
    let (release_tx, release_rx) = sync_channel(1);
    let cb = circuit_breaker::wrap(ControlledProbeResource {
        state: Arc::clone(&state),
        entered: entered_tx,
        release: Mutex::new(release_rx),
    })
    .failure_threshold(3)
    // Zero exercises the public lower-bound normalization to one second.
    .cooldown(Duration::ZERO)
    .build();

    for _ in 0..3 {
        let _ = cb.health_check();
    }
    assert_eq!(state.calls(), 3);
    assert!(format!("{cb:?}").contains("Open"));

    // The normalized cooldown still prevents an immediate probe.
    assert!(cb.health_check().is_err());
    assert_eq!(state.calls(), 3);
    state.set_healthy(true);

    std::thread::scope(|scope| {
        let transition = scope.spawn(|| {
            loop {
                match cb.health_check() {
                    Ok(()) => return,
                    Err(_) => std::thread::yield_now(),
                }
            }
        });

        entered_rx.recv_timeout(TRANSITION_TIMEOUT).unwrap();
        assert!(format!("{cb:?}").contains("HalfOpen"));
        release_tx.send(()).unwrap();
        transition.join().unwrap();
    });

    assert_eq!(state.calls(), 4, "half-open should probe inner");
    assert!(format!("{cb:?}").contains("Closed"));

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
