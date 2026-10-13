//! Type-only probes every focused API contract shares.
//!
//! A probe proves a shape at compile time. It never polls a future and never
//! builds a runtime. The streaming client contract uses both probes under
//! every feature set, so neither carries a feature gate.

/// Compiles only when `future` can move across Tokio workers. The future is
/// handed back unpolled.
pub(crate) fn require_send<F: Future + Send>(future: F) -> F {
    future
}

/// Compiles only when `T` can cross a thread boundary.
pub(crate) fn assert_send<T: Send>() {}
