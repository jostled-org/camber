use crate::RuntimeError;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Minimum allowed interval for resource health checks.
pub(crate) const MIN_HEALTH_INTERVAL: Duration = Duration::from_secs(1);

/// Shared health state for all registered resources.
/// Fixed-size array allocated once from the resource registry. Each entry is
/// (resource name, healthy flag). Health check tasks write the AtomicBool;
/// the `/health` endpoint reads it. Zero allocation at request time.
pub(crate) type HealthState = Arc<[(Box<str>, AtomicBool)]>;

/// A managed external resource that participates in the runtime lifecycle.
///
/// Implement this trait on database pools, caches, message brokers, or any
/// long-lived resource that needs health checking and graceful shutdown.
///
/// All methods are synchronous. Health checks run via `block_in_place` on a
/// background thread. Shutdown runs during runtime teardown before Tokio
/// shuts down.
pub trait Resource: Send + Sync + 'static {
    /// Human-readable name for health reporting and error messages.
    fn name(&self) -> &str;

    /// Check whether the resource is healthy. Called on a background interval.
    fn health_check(&self) -> Result<(), RuntimeError>;

    /// Graceful shutdown. Called in reverse registration order during teardown.
    fn shutdown(&self) -> Result<(), RuntimeError>;
}
