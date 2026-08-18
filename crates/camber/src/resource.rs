use crate::RuntimeError;
use std::time::Duration;

pub(crate) mod entry;
pub(crate) mod registry;

/// Minimum allowed interval for resource health checks.
pub(crate) const MIN_HEALTH_INTERVAL: Duration = Duration::from_secs(1);

/// A managed external resource that participates in the runtime lifecycle.
///
/// Implement this trait on database pools, caches, message brokers, or any
/// long-lived resource that needs health checking and graceful shutdown.
///
/// # Ordering
///
/// One coordinator visits every registered resource. The startup readiness pass
/// and each periodic health pass visit them in registration order; teardown
/// visits them in reverse registration order. One resource never has two
/// callbacks running at once, so an implementation needs no lock of its own to
/// keep its probe and its shutdown apart.
///
/// # Bounds
///
/// Every callback runs on its own worker under the deadline
/// [`ResourceBudget`](crate::ResourceBudget) configured for its phase. The
/// deadline bounds how long Camber WAITS, not how long the callback runs: a
/// synchronous callback has no cancellation point, so a worker that misses its
/// deadline keeps the resource until the callback returns on its own. Camber
/// records the resource as having exceeded its deadline, refuses to start
/// another callback for it, and never claims the worker was terminated.
///
/// # Context
///
/// - `health_check` runs with this runtime's Camber context installed and its
///   Tokio handle entered, so it may bridge async work with
///   [`runtime::block_on`].
/// - `shutdown` runs after the root scope has drained, with no Tokio runtime
///   entered. Bridging async work there has no runtime to bridge onto: a
///   resource that needs async teardown must own and bound that facility
///   itself.
///
/// [`runtime::block_on`]: crate::runtime::block_on
pub trait Resource: Send + Sync + 'static {
    /// Human-readable name for health reporting and error messages.
    ///
    /// Must be non-blank and unique across one runtime's registrations. It is
    /// the only identity a `/health` entry, an operator event, and a lifecycle
    /// failure have for this resource, so a registration that breaks either
    /// rule is refused before the runtime is established.
    fn name(&self) -> &str;

    /// Check whether the resource is healthy. Called once before the service
    /// admits traffic, then on a background interval.
    ///
    /// # Errors
    ///
    /// Returns whatever the implementation uses to say the resource is unwell.
    /// A failure of the readiness pass prevents the service from serving at all
    /// and is returned from [`RuntimeBuilder::run`] as
    /// [`RuntimeError::Lifecycle`]; a later failure is published through
    /// `/health` and one structured operator event.
    ///
    /// Both passes run with a Tokio handle entered, so an implementation that
    /// bridges async work should resolve its handle with
    /// `tokio::runtime::Handle::try_current()` and return
    /// [`RuntimeError::NoRuntime`] when there is none, rather than aborting.
    /// That is what [`ProxyHealthResource::health_check`] does.
    ///
    /// Keep the probe inside its configured deadline. Give any network call its
    /// own timeout rather than letting it inherit the connect timeout of
    /// whatever library it uses: a probe that runs past the deadline blocks
    /// every later callback for this resource, including its own shutdown.
    ///
    /// [`RuntimeBuilder::run`]: crate::RuntimeBuilder::run
    /// [`ProxyHealthResource::health_check`]: crate::http::ProxyHealthResource
    fn health_check(&self) -> Result<(), RuntimeError>;

    /// Graceful shutdown. Called once, on its own worker, after the root scope
    /// has drained.
    ///
    /// # Errors
    ///
    /// Returns whatever the implementation uses to say teardown failed. Every
    /// such failure is retained in the [`RuntimeError::Lifecycle`] aggregate
    /// the runtime returns; none of them is reduced to a log line, and none of
    /// them prevents another resource's teardown from being attempted.
    fn shutdown(&self) -> Result<(), RuntimeError>;
}
