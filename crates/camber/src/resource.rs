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
/// All methods are synchronous, and the two lifecycle hooks differ in what
/// context they run under:
///
/// - `health_check` runs inside the Tokio runtime — on a blocking worker for
///   the startup probe, through `block_in_place` for every later one — and
///   under the Camber runtime context, so it may bridge async work with
///   [`runtime::block_on`].
/// - `shutdown` runs on a plain OS thread after the root scope has drained,
///   with no Tokio runtime entered. Bridging async work there has no runtime
///   to bridge onto.
///
/// [`runtime::block_on`]: crate::runtime::block_on
pub trait Resource: Send + Sync + 'static {
    /// Human-readable name for health reporting and error messages.
    fn name(&self) -> &str;

    /// Check whether the resource is healthy. Called on a background interval.
    ///
    /// # Errors
    ///
    /// Returns whatever the implementation uses to say the resource is unwell;
    /// the runtime records the error and marks the resource unhealthy.
    ///
    /// Both probe passes run under a Tokio runtime, so an implementation that
    /// bridges async work should resolve its handle with
    /// `tokio::runtime::Handle::try_current()` and return
    /// `RuntimeError::NoRuntime` when there is none, rather than aborting. That
    /// is what [`ProxyHealthResource::health_check`] does.
    ///
    /// Keep the probe bounded well inside the runtime's `shutdown_timeout`. It
    /// is synchronous, so it has no await point and cannot observe the root
    /// scope closing: the interval loop breaks between probes, but one already
    /// under way runs to completion. A probe still running at that boundary
    /// makes `runtime::run` return `RuntimeError::ScopeDrainTimeout` on what was
    /// otherwise a clean exit. Give any network call its own timeout rather than
    /// letting it inherit the connect timeout of whatever library it uses.
    ///
    /// [`ProxyHealthResource::health_check`]: crate::http::ProxyHealthResource
    fn health_check(&self) -> Result<(), RuntimeError>;

    /// Graceful shutdown. Called on its own thread during teardown, concurrently
    /// with every other resource's — there is no ordering between them.
    fn shutdown(&self) -> Result<(), RuntimeError>;
}
