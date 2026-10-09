//! Type-only probes every focused API contract shares.
//!
//! A probe proves a shape at compile time. It never polls a future and never
//! builds a runtime. Each probe compiles under the features whose contracts
//! use it, so no feature set carries an unused probe.

/// Compiles only when `future` can move across Tokio workers. The future is
/// handed back unpolled.
#[cfg(any(feature = "ws", feature = "nats", feature = "sqs", feature = "dns01"))]
pub(crate) fn require_send<F: Future + Send>(future: F) -> F {
    future
}

/// Compiles only when `T` can cross a thread boundary.
#[cfg(any(feature = "ws", feature = "nats", feature = "sqs"))]
pub(crate) fn assert_send<T: Send>() {}
