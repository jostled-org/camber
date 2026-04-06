//! Circuit breaker wrapper for [`Resource`] implementations.
//!
//! After a configurable number of consecutive health check failures, the
//! circuit opens and skips probing the inner resource. After a cooldown
//! period the circuit enters half-open, allowing one probe. A successful
//! probe closes the circuit; a failure re-opens it.
//!
//! ```rust,no_run
//! use std::time::Duration;
//! use camber::circuit_breaker;
//!
//! # fn example(pool: impl camber::Resource) {
//! let protected = circuit_breaker::wrap(pool)
//!     .failure_threshold(3)
//!     .cooldown(Duration::from_secs(30))
//!     .build();
//! # }
//! ```

use crate::RuntimeError;
use crate::resource::Resource;
use std::sync::atomic::{AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Circuit states stored in `AtomicU8`.
const CLOSED: u8 = 0;
const OPEN: u8 = 1;
const HALF_OPEN: u8 = 2;

/// Default number of consecutive failures before the circuit opens.
const DEFAULT_FAILURE_THRESHOLD: u32 = 5;

/// Default cooldown before the circuit transitions from open to half-open.
const DEFAULT_COOLDOWN: Duration = Duration::from_secs(30);

/// Sentinel for "no timestamp recorded yet".
const NO_TIMESTAMP: u64 = u64::MAX;

impl<R> std::fmt::Debug for CircuitBreaker<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state_label = match self.state.load(Ordering::Relaxed) {
            CLOSED => "Closed",
            OPEN => "Open",
            HALF_OPEN => "HalfOpen",
            _ => "Unknown",
        };
        f.debug_struct("CircuitBreaker")
            .field("state", &state_label)
            .field(
                "consecutive_failures",
                &self.consecutive_failures.load(Ordering::Relaxed),
            )
            .field("failure_threshold", &self.failure_threshold)
            .field("cooldown", &self.cooldown)
            .finish()
    }
}

/// A circuit breaker wrapping a [`Resource`].
///
/// Delegates `name()` and `shutdown()` directly. Intercepts `health_check()`
/// to implement closed -> open -> half-open state transitions.
pub struct CircuitBreaker<R> {
    inner: R,
    state: AtomicU8,
    consecutive_failures: AtomicU32,
    /// Nanos since `epoch` when the circuit last opened. Uses `NO_TIMESTAMP`
    /// as sentinel for "never opened".
    last_opened_nanos: AtomicU64,
    /// Monotonic reference point captured at construction.
    epoch: Instant,
    failure_threshold: u32,
    cooldown: Duration,
}

impl<R: Resource> CircuitBreaker<R> {
    /// Record a failure and open the circuit when the threshold is reached.
    /// Uses a compare-exchange loop to increment without wrapping past the threshold.
    fn record_failure(&self) {
        loop {
            let prev = self.consecutive_failures.load(Ordering::Acquire);
            // Already at or above threshold — circuit is open, nothing to increment.
            match prev >= self.failure_threshold {
                true => {
                    self.open_circuit();
                    return;
                }
                false => {}
            }
            let next = prev + 1;
            match self.consecutive_failures.compare_exchange(
                prev,
                next,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) if next >= self.failure_threshold => {
                    self.open_circuit();
                    return;
                }
                Ok(_) => return,
                Err(_) => {} // Another thread raced us — retry.
            }
        }
    }

    fn health_check_closed(&self) -> Result<(), RuntimeError> {
        match self.inner.health_check() {
            Ok(()) => {
                self.consecutive_failures.store(0, Ordering::Release);
                Ok(())
            }
            Err(e) => {
                self.record_failure();
                Err(e)
            }
        }
    }

    fn health_check_open(&self) -> Result<(), RuntimeError> {
        let nanos = self.last_opened_nanos.load(Ordering::Acquire);
        let opened_at = match nanos {
            NO_TIMESTAMP => Instant::now(),
            n => self.epoch + Duration::from_nanos(n),
        };

        let cooldown_expired = opened_at.elapsed() >= self.cooldown;
        // CAS: only one thread wins the transition to half-open.
        let cas_won = cooldown_expired
            && self
                .state
                .compare_exchange(OPEN, HALF_OPEN, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();

        match cas_won {
            true => self.health_check_half_open(),
            false => Err(RuntimeError::Http(
                format!("circuit breaker open for resource: {}", self.inner.name()).into(),
            )),
        }
    }

    fn health_check_half_open(&self) -> Result<(), RuntimeError> {
        match self.inner.health_check() {
            Ok(()) => {
                self.consecutive_failures.store(0, Ordering::Release);
                self.state.store(CLOSED, Ordering::Release);
                Ok(())
            }
            Err(e) => {
                self.open_circuit();
                Err(e)
            }
        }
    }

    fn open_circuit(&self) {
        // Write timestamp BEFORE storing OPEN state so readers always see
        // a valid timestamp when they observe the OPEN state.
        let nanos = self.epoch.elapsed().as_nanos() as u64;
        self.last_opened_nanos.store(nanos, Ordering::Release);
        self.state.store(OPEN, Ordering::Release);
    }
}

impl<R: Resource> Resource for CircuitBreaker<R> {
    fn name(&self) -> &str {
        self.inner.name()
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        match self.state.load(Ordering::Acquire) {
            OPEN => self.health_check_open(),
            HALF_OPEN => self.health_check_half_open(),
            _ => self.health_check_closed(),
        }
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        self.inner.shutdown()
    }
}

/// Begin building a circuit breaker around `resource`.
pub fn wrap<R: Resource>(resource: R) -> CircuitBreakerBuilder<R> {
    CircuitBreakerBuilder {
        inner: resource,
        failure_threshold: DEFAULT_FAILURE_THRESHOLD,
        cooldown: DEFAULT_COOLDOWN,
    }
}

/// Builder for configuring a [`CircuitBreaker`].
pub struct CircuitBreakerBuilder<R> {
    inner: R,
    failure_threshold: u32,
    cooldown: Duration,
}

impl<R: Resource> CircuitBreakerBuilder<R> {
    /// Number of consecutive health check failures before the circuit opens.
    ///
    /// Minimum: 1. Values below 1 are clamped.
    ///
    /// Default: 5.
    pub fn failure_threshold(mut self, n: u32) -> Self {
        self.failure_threshold = match n < 1 {
            true => 1,
            false => n,
        };
        self
    }

    /// Duration the circuit stays open before transitioning to half-open.
    ///
    /// Minimum: 1 second. Values below 1 second are clamped.
    ///
    /// Default: 30 seconds.
    pub fn cooldown(mut self, d: Duration) -> Self {
        self.cooldown = match d < Duration::from_secs(1) {
            true => Duration::from_secs(1),
            false => d,
        };
        self
    }

    /// Build the circuit breaker, consuming the builder.
    pub fn build(self) -> CircuitBreaker<R> {
        CircuitBreaker {
            inner: self.inner,
            state: AtomicU8::new(CLOSED),
            consecutive_failures: AtomicU32::new(0),
            last_opened_nanos: AtomicU64::new(NO_TIMESTAMP),
            epoch: Instant::now(),
            failure_threshold: self.failure_threshold,
            cooldown: self.cooldown,
        }
    }
}
