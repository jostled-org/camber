use crate::RuntimeError;
use crate::resource::{MIN_HEALTH_INTERVAL, Resource};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The whole deadline one probe attempt gets, transport included.
///
/// Carried on the request rather than on the client, which is what makes it the
/// only bound a probe has: a reqwest request timeout spans connect through body
/// end, so a separate connect allowance sitting above this one could never
/// expire before it did.
const HEALTH_CHECK_TIMEOUT: Duration = Duration::from_secs(5);

/// The client every backend health probe in this process shares.
///
/// The health checker's own transport, not a proxy route's. Its probes are one
/// configuration — one fixed per-attempt deadline, stated above, and no policy a
/// caller can vary — so one client is the whole population rather than a cache
/// two configurations would share. A proxied route reaches its upstream through
/// the client its own registration froze, which is a different owner entirely.
static HEALTH_CLIENT: std::sync::LazyLock<Result<reqwest::Client, Arc<str>>> =
    std::sync::LazyLock::new(|| {
        reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|error| -> Arc<str> { error.to_string().into() })
    });

/// The client a health probe runs through.
///
/// # Errors
///
/// Returns [`RuntimeError::Http`] carrying the account of the build this
/// process could not complete.
fn health_client() -> Result<&'static reqwest::Client, RuntimeError> {
    HEALTH_CLIENT
        .as_ref()
        .map_err(|error| RuntimeError::Http(Arc::clone(error)))
}

/// Why a backend health probe failed.
///
/// A probe has exactly two ways to fail and they call for different operator
/// action, so the outcome stays typed all the way to the report: a transport
/// failure means the backend was unreachable, a status failure means it
/// answered and declared itself unwell.
#[derive(Debug, thiserror::Error)]
enum ProbeError {
    /// The request never produced a response.
    #[error("{0}")]
    Transport(#[from] reqwest::Error),

    /// The backend answered with a non-success status.
    #[error("backend returned status {0}")]
    Status(reqwest::StatusCode),
}

/// Probe a URL, reporting why the backend is unwell rather than only that it is.
async fn probe_url(client: &reqwest::Client, url: &str) -> Result<(), ProbeError> {
    let response = client.get(url).timeout(HEALTH_CHECK_TIMEOUT).send().await?;
    let status = response.status();
    match status.is_success() {
        true => Ok(()),
        false => Err(ProbeError::Status(status)),
    }
}

/// Join a backend base URL and a health path into the URL that gets probed.
///
/// The resource form and the standalone checker describe the same endpoint, so
/// they compose it once — a second spelling could drift into probing a
/// different path for identical configuration.
fn health_url(backend: &str, path: &str) -> Box<str> {
    format!("{backend}{path}").into_boxed_str()
}

/// A proxy backend health check that implements [`Resource`] for lifecycle
/// integration.
///
/// Create with [`ProxyHealthResource::new`], pass the [`routing_flag`] to
/// [`Router::proxy_checked`], and register the resource with
/// [`RuntimeBuilder::resource`]. The runtime manages the health check interval.
///
/// [`routing_flag`]: ProxyHealthResource::routing_flag
/// [`Router::proxy_checked`]: super::Router::proxy_checked
/// [`RuntimeBuilder::resource`]: crate::RuntimeBuilder::resource
#[derive(Debug)]
pub struct ProxyHealthResource {
    name: Box<str>,
    url: Box<str>,
    routing_flag: Arc<AtomicBool>,
}

impl ProxyHealthResource {
    /// Create a new proxy health resource.
    ///
    /// `backend` is the base URL (e.g., `"http://localhost:8080"`).
    /// `path` is the health endpoint path (e.g., `"/health"`).
    ///
    /// The routing flag starts `true` (healthy).
    pub fn new(backend: &str, path: &str) -> Self {
        Self {
            name: Box::from(backend),
            url: health_url(backend, path),
            routing_flag: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Get the routing flag for use with [`Router::proxy_checked`].
    ///
    /// The runtime updates this flag based on health check results.
    /// The proxy router reads it to decide whether to forward requests.
    ///
    /// [`Router::proxy_checked`]: super::Router::proxy_checked
    pub fn routing_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.routing_flag)
    }
}

impl Resource for ProxyHealthResource {
    fn name(&self) -> &str {
        &self.name
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        let client = health_client()?;
        // Absence is typed, not fatal: this is a public method returning
        // `Result`, so a caller holding no runtime gets the error its signature
        // promises rather than an abort.
        let handle = tokio::runtime::Handle::try_current().map_err(|_| RuntimeError::NoRuntime)?;
        let url: &str = &self.url;
        let result = match handle.runtime_flavor() {
            // `Handle::block_on` enters the runtime, so it aborts the process on
            // any thread already inside one — which is every async caller of this
            // public method, `CircuitBreaker` included. `block_in_place` hands
            // this worker's core to a replacement thread and leaves the runtime
            // context first, so the bridge is legal from a poll. Called directly
            // rather than through `crate::task::block_in_place`, because this
            // match is already the flavor guard that wrapper exists to apply.
            // A resource callback worker holds a handle but has not entered the
            // runtime, so tokio's own detection runs the closure inline there.
            tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| handle.block_on(probe_url(client, url)))
            }
            // A current-thread runtime has no worker core to hand off, so there
            // is no way to leave the poll context and block at all. Refused with
            // the flavor named: a context exists here, so reporting absence
            // would send an operator after a runtime that is not missing.
            _ => {
                return Err(RuntimeError::Http(
                    "backend health check requires a multi-thread runtime".into(),
                ));
            }
        };
        self.routing_flag.store(result.is_ok(), Ordering::Release);
        result.map_err(|e| RuntimeError::Http(format!("backend health check failed: {e}").into()))
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
}

/// Admit a root-scope child that polls a backend health endpoint.
///
/// Performs an initial probe before returning, so the flag reflects the
/// backend's real state immediately. The background loop then continues
/// polling at `interval` until either lifecycle signal fires, and runtime
/// teardown awaits it.
///
/// Returns an `Arc<AtomicBool>` that reflects the backend's health state.
/// On poll failure it flips to `false`; on success it flips back to `true`.
///
/// For lifecycle integration (health reporting via `/health`, structured
/// shutdown), use [`ProxyHealthResource`] with [`RuntimeBuilder::resource`]
/// instead.
///
/// # Errors
///
/// Returns `RuntimeError::InvalidArgument` if `interval` is below the minimum
/// health interval, `RuntimeError::Http` if the shared health-probe client could
/// not be built, or `RuntimeError::ScopeClosed` if the root scope has already
/// closed to admission.
///
/// `RuntimeError::NoRuntime` has two origins. Resolving the runtime context
/// returns it when none is established, before any probe runs — a perpetual
/// loop with no owner to await it is exactly what the root scope exists to
/// prevent. Admission returns it again when the resolved runtime carries no
/// executor to admit onto, and that is after the initial probe, so the caller
/// gets the refusal with one probe already spent.
///
/// [`RuntimeBuilder::resource`]: crate::RuntimeBuilder::resource
pub async fn spawn_health_checker(
    backend: &str,
    path: &str,
    interval: Duration,
) -> Result<Arc<AtomicBool>, RuntimeError> {
    if interval < MIN_HEALTH_INTERVAL {
        // The bound is the constant, so the message reads it rather than
        // restating it — a changed constant cannot leave a lie behind.
        return Err(RuntimeError::InvalidArgument(
            format!("health check interval must be at least {MIN_HEALTH_INTERVAL:?}")
                .into_boxed_str(),
        ));
    }
    // Resolved once, before the initial probe: a perpetual loop with no owner
    // to await it is what the root scope exists to prevent.
    let runtime = crate::runtime::runtime_context()?;
    let url = health_url(backend, path);

    // The health checker's own client, shared by every probe in this process so
    // the loop below adds no second connection pool or TLS session cache. The
    // borrow is `'static`, so the scope child's future carries it without a
    // clone.
    let client = health_client()?;

    // Initial probe before admitting the background loop. It publishes through
    // the same recorder as every later probe, so a backend that is already down
    // is reported once here rather than only on some later transition. It runs
    // unguarded because it is the caller's own await, not a scope child's: the
    // caller holds this future and can drop it, and admission — not the probe —
    // is where a closed scope is refused.
    let healthy = Arc::new(AtomicBool::new(true));
    record_probe(&url, &healthy, probe_url(client, &url).await);

    let loop_healthy = Arc::clone(&healthy);
    crate::task::admit_signalled_subsystem_on(&runtime, "health checker", move |signals| {
        run_health_checker(client, url, interval, loop_healthy, signals)
    })?;

    Ok(healthy)
}

/// Poll the backend until either lifecycle signal fires.
///
/// Both awaits break on the signals: `tick` ends the wait between probes, and
/// `guard` ends a probe already in flight. A probe carries its own
/// `HEALTH_CHECK_TIMEOUT`, so an unguarded one would keep this scope child
/// alive for seconds after the scope closed — spending the drain's escalation
/// budget on a result nobody will read.
async fn run_health_checker(
    client: &'static reqwest::Client,
    url: Box<str>,
    interval: Duration,
    healthy: Arc<AtomicBool>,
    signals: crate::runtime_state::LifecycleSignals,
) {
    while let ControlFlow::Continue(()) = signals.tick(interval).await {
        match signals.guard(probe_url(client, &url)).await {
            ControlFlow::Break(()) => return,
            ControlFlow::Continue(result) => record_probe(&url, &healthy, result),
        }
    }
}

/// Publish one probe outcome and report the edge into unhealthy.
///
/// Only the healthy-to-unhealthy edge warns. This path has no caller to return
/// an error to, so the cause would otherwise be destroyed with the `Result` —
/// but a backend that stays down is polled every interval, and reporting each
/// poll would bury the transition that matters under identical repeats.
fn record_probe(url: &str, healthy: &AtomicBool, result: Result<(), ProbeError>) {
    let was_healthy = healthy.swap(result.is_ok(), Ordering::Release);
    if let (true, Err(e)) = (was_healthy, result) {
        tracing::warn!(url = %url, error = %e, "backend health probe failed");
    }
}
