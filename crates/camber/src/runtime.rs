use crate::resource::{HealthState, MIN_HEALTH_INTERVAL, Resource};
use crate::resource_lifecycle::{
    admit_health_tasks, run_initial_health_checks, shutdown_resources,
};
use crate::runtime_state::{
    RuntimeConfig, RuntimeContextGuard, RuntimeInner, drain_root_scope, install_runtime,
    teardown_runtime,
};
use crate::runtime_test_support::{RuntimeController, RuntimeSchedule};
use crate::tls::CertStore;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

/// Re-export of `tokio::runtime::Handle` for use with [`tokio_handle()`].
pub use tokio::runtime::Handle as TokioHandle;

// Re-export crate-internal items so `use crate::runtime;` call sites keep working.
pub(crate) use crate::runtime_state::{
    cancel_channel, check_cancel, has_runtime, runtime_context, try_current_runtime,
};

// The public lifecycle entry points, re-exported from their definitions rather
// than forwarded through one-line wrappers. A wrapper's own doc comment is what
// a user reads, so each was quietly dropping the typed-absence semantics stated
// at the definition — that `request_shutdown` is a no-op and `is_shutting_down`
// false with no runtime, and that `on_cancel` drops `future` unpolled. The
// public paths — `camber::runtime::request_shutdown` and the rest — are
// unchanged.
pub use crate::runtime_state::{
    block_on, is_shutting_down, on_cancel, request_shutdown, tokio_handle,
};

impl std::fmt::Debug for RuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Non-exhaustive: the TLS paths, the test schedule, the OTLP endpoint
        // and the ACME/DNS-01 setups are deliberately not printed, and `..`
        // says so instead of implying the struct holds only what is listed.
        f.debug_struct("RuntimeBuilder")
            .field("worker_threads", &self.config.worker_threads)
            .field("shutdown_timeout", &self.config.shutdown_timeout)
            .field("keepalive_timeout", &self.config.keepalive_timeout)
            .field("tracing_enabled", &self.config.tracing_enabled)
            .field("metrics_enabled", &self.config.metrics_enabled)
            .field("health_interval", &self.config.health_interval)
            .field("connection_limit", &self.config.connection_limit)
            .field("resource_count", &self.resources.len())
            .field("has_tls", &self.has_manual_tls())
            .finish_non_exhaustive()
    }
}

/// Configure a Camber runtime before running.
pub struct RuntimeBuilder {
    config: RuntimeConfig,
    test_schedule: Option<Arc<RuntimeSchedule>>,
    resources: Vec<Box<dyn Resource>>,
    /// Written once by `tls_cert`/`tls_key` and thereafter only read, so the
    /// path is boxed rather than kept in a growable `PathBuf` buffer neither
    /// setter ever appends to.
    tls_cert_path: Option<Box<std::path::Path>>,
    tls_key_path: Option<Box<std::path::Path>>,
    tls_cert_store: Option<CertStore>,
    #[cfg(feature = "acme")]
    acme_config: Option<crate::acme::AcmeConfig>,
    #[cfg(feature = "dns01")]
    dns01_setup: Option<crate::dns01::Dns01Setup>,
    #[cfg(feature = "otel")]
    otel_endpoint: Option<Box<str>>,
}

impl RuntimeBuilder {
    fn new() -> Self {
        Self {
            config: RuntimeConfig::default(),
            test_schedule: None,
            resources: Vec::new(),
            tls_cert_path: None,
            tls_key_path: None,
            tls_cert_store: None,
            #[cfg(feature = "acme")]
            acme_config: None,
            #[cfg(feature = "dns01")]
            dns01_setup: None,
            #[cfg(feature = "otel")]
            otel_endpoint: None,
        }
    }

    /// Set the number of Tokio worker threads.
    ///
    /// A value of `0` is rejected when the runtime starts.
    pub fn worker_threads(mut self, n: usize) -> Self {
        self.config.worker_threads = n;
        self
    }

    /// Set the graceful shutdown timeout. Minimum: 100ms. Zero values are clamped.
    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        const MIN: Duration = Duration::from_millis(100);
        self.config.shutdown_timeout =
            crate::time::clamp_duration(timeout, MIN, "shutdown_timeout");
        self
    }

    /// Set the HTTP keep-alive timeout. Minimum: 100ms. Zero values are clamped.
    pub fn keepalive_timeout(mut self, timeout: Duration) -> Self {
        const MIN: Duration = Duration::from_millis(100);
        self.config.keepalive_timeout =
            crate::time::clamp_duration(timeout, MIN, "keepalive_timeout");
        self
    }

    /// Set the health check interval for registered resources.
    /// Default: 10 seconds. Minimum: 1 second (values below are clamped).
    pub fn health_interval(mut self, interval: Duration) -> Self {
        self.config.health_interval = interval.max(MIN_HEALTH_INTERVAL);
        self
    }

    /// Set the maximum number of concurrent connections per listener.
    /// The accept loop waits for a permit when the limit is reached.
    /// Existing callers that do not set a limit keep unbounded behavior.
    /// A value of 0 is rejected when the runtime starts.
    pub fn connection_limit(mut self, n: usize) -> Self {
        self.config.connection_limit = Some(n);
        self
    }

    /// Attach a runtime-instance scheduling seam for integration tests.
    #[doc(hidden)]
    pub fn with_test_schedule(mut self, controller: &RuntimeController) -> Self {
        self.test_schedule = Some(controller.schedule());
        self
    }

    /// Register a resource for lifecycle management. Resources are shut down
    /// concurrently during runtime teardown — one thread each, all joined
    /// before `run` returns — so a resource must not depend on another still
    /// being up while it shuts down.
    pub fn resource(mut self, r: impl Resource) -> Self {
        self.resources.push(Box::new(r));
        self
    }

    /// Enable tracing subscriber setup for the runtime.
    pub fn with_tracing(mut self) -> Self {
        self.config.tracing_enabled = true;
        self
    }

    /// Enable metrics export endpoints for the runtime.
    pub fn with_metrics(mut self) -> Self {
        self.config.metrics_enabled = true;
        self
    }

    #[cfg(feature = "profiling")]
    /// Enable profiling support for the runtime.
    pub fn with_profiling(mut self) -> Self {
        self.config.profiling_enabled = true;
        self
    }

    /// Set the OTLP exporter endpoint for OpenTelemetry span export.
    /// Default OTLP gRPC endpoint: `http://localhost:4317`.
    #[cfg(feature = "otel")]
    pub fn otel_endpoint(mut self, url: &str) -> Self {
        self.otel_endpoint = Some(Box::from(url));
        self
    }

    /// Set the TLS certificate PEM file path.
    pub fn tls_cert(mut self, path: &std::path::Path) -> Self {
        self.tls_cert_path = Some(Box::from(path));
        self
    }

    /// Set the TLS private key PEM file path.
    pub fn tls_key(mut self, path: &std::path::Path) -> Self {
        self.tls_key_path = Some(Box::from(path));
        self
    }

    /// Use a pre-built `CertStore` as the TLS cert resolver.
    /// Enables cert hot-swapping at runtime.
    pub fn tls_resolver(mut self, store: CertStore) -> Self {
        self.tls_cert_store = Some(store);
        self
    }

    /// Use automatic TLS via ACME (Let's Encrypt).
    /// Mutually exclusive with `tls_cert`/`tls_key`/`tls_resolver`.
    #[cfg(feature = "acme")]
    pub fn tls_auto(mut self, config: crate::acme::AcmeConfig) -> Self {
        self.acme_config = Some(config);
        self
    }

    /// Use automatic TLS via ACME DNS-01 challenges (for servers behind NAT).
    /// Mutually exclusive with `tls_cert`/`tls_key`/`tls_auto`.
    #[cfg(feature = "dns01")]
    pub fn tls_auto_dns01(
        mut self,
        acme: crate::dns01::AcmeDns01,
        api_token: Box<str>,
        domain: Box<str>,
    ) -> Self {
        self.dns01_setup = Some(crate::dns01::Dns01Setup {
            acme,
            api_token,
            domain,
        });
        self
    }

    /// Run the configured runtime, returning an error on misconfiguration.
    ///
    /// Library code should propagate the error; only CLI binaries should map
    /// it to a process exit.
    pub fn run<F, T>(self, f: F) -> Result<T, crate::RuntimeError>
    where
        F: FnOnce() -> T,
    {
        reject_nested_runtime()?;
        self.validate_tls_options()?;
        if self.config.worker_threads == 0 {
            return Err(crate::RuntimeError::InvalidArgument(
                "worker_threads must be at least 1".into(),
            ));
        }
        if self.config.connection_limit == Some(0) {
            return Err(crate::RuntimeError::InvalidArgument(
                "connection_limit must be at least 1".into(),
            ));
        }

        let mut config = self.config;

        // Resolve manual TLS (cert files or CertStore).
        // When ACME is active, these are all None (enforced by validate_tls_options).
        // `PathBuf::from(Box<Path>)` reuses the boxed allocation, so handing
        // `resolve_tls` the owned type it takes costs no copy.
        let (tls_cfg, store) = crate::tls::resolve_tls(
            self.tls_cert_store,
            self.tls_cert_path.map(std::path::PathBuf::from),
            self.tls_key_path.map(std::path::PathBuf::from),
        )?;
        config.tls_config = tls_cfg;
        config.cert_store = store;

        #[cfg(feature = "acme")]
        let acme_state = match self.acme_config {
            Some(acme_cfg) => {
                let (tls_cfg, state) = acme_cfg.build()?;
                config.tls_config = Some(tls_cfg);
                Some(state)
            }
            None => None,
        };

        run_inner_impl(
            config,
            self.test_schedule,
            self.resources.into(),
            f,
            #[cfg(feature = "acme")]
            acme_state,
            #[cfg(feature = "dns01")]
            self.dns01_setup,
            #[cfg(feature = "otel")]
            self.otel_endpoint,
        )
    }

    /// Whether any TLS material was configured by hand — a resolver, or either
    /// half of a cert/key pair.
    ///
    /// One definition for the mutual-exclusion check and the `Debug` field, so
    /// a builder that debug-prints `has_tls: false` cannot then be rejected for
    /// having manual TLS.
    fn has_manual_tls(&self) -> bool {
        self.tls_cert_path.is_some() || self.tls_key_path.is_some() || self.tls_cert_store.is_some()
    }

    fn validate_tls_options(&self) -> Result<(), crate::RuntimeError> {
        let has_manual = self.has_manual_tls();

        #[cfg(feature = "acme")]
        let has_acme = self.acme_config.is_some();
        #[cfg(not(feature = "acme"))]
        let has_acme = false;

        #[cfg(feature = "dns01")]
        let has_dns01 = self.dns01_setup.is_some();
        #[cfg(not(feature = "dns01"))]
        let has_dns01 = false;

        match (has_acme, has_dns01, has_manual) {
            (true, true, _) => Err(crate::RuntimeError::Tls(
                "tls_auto and tls_auto_dns01 are mutually exclusive".into(),
            )),
            (true, _, true) => Err(crate::RuntimeError::Tls(
                "tls_auto and tls_cert/tls_key are mutually exclusive".into(),
            )),
            (_, true, true) => Err(crate::RuntimeError::Tls(
                "tls_auto_dns01 and tls_cert/tls_key are mutually exclusive".into(),
            )),
            _ => Ok(()),
        }
    }
}

/// Create a RuntimeBuilder for configuring the runtime before running.
pub fn builder() -> RuntimeBuilder {
    RuntimeBuilder::new()
}

/// Run a closure within a test-optimized runtime.
///
/// Returns `Result<T, RuntimeError>` — callers should `.unwrap()` in tests.
pub fn test<F, T>(f: F) -> Result<T, crate::RuntimeError>
where
    F: FnOnce() -> T,
{
    // The config is INSTALLED, not replayed through the setters. Naming the
    // knobs by hand is what let the two test entries drift: the async one
    // consumes the config whole, so any knob added to `test_runtime_config` and
    // not also listed here would reach only one of them. The field is private
    // to this module, so this is not an API the setters guard. Their only other
    // effect is the 100 ms clamp, and both durations below already sit at or
    // above it — the clamps skipped here are no-ops.
    let mut builder = RuntimeBuilder::new();
    builder.config = test_runtime_config();
    builder.run(f)
}

/// What a test-optimized runtime is configured as.
///
/// One definition, so the sync and async test entry points cannot drift on a
/// `RuntimeConfig` knob only one of them got — which is what they had already
/// done on the worker count. Both entries take the config WHOLE, so a knob
/// added here reaches both. It is also the count the async entry builds its
/// executor from, so the worker threads `RuntimeInner` reports are the ones it
/// has.
///
/// The config is the only thing they share. `test` runs through
/// `run_inner_impl` and gets that path's owned subsystems — the signal watcher
/// and the per-resource health loops — while the async entry admits no
/// background subsystem at all; a test that wants one opts in through the
/// doc-hidden seam.
fn test_runtime_config() -> RuntimeConfig {
    RuntimeConfig {
        worker_threads: tokio_default_worker_threads(),
        keepalive_timeout: Duration::from_millis(100),
        shutdown_timeout: Duration::from_secs(1),
        ..RuntimeConfig::default()
    }
}

/// Run an async closure within a test-optimized runtime.
///
/// Hidden — `#[camber::test]` is the public interface.
#[doc(hidden)]
pub fn __test_async<F, Fut, T>(f: F) -> Result<T, crate::RuntimeError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    try_test_async(f)
}

fn try_test_async<F, Fut, T>(f: F) -> Result<T, crate::RuntimeError>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    reject_nested_runtime()?;
    let config = test_runtime_config();

    let tokio_rt = build_executor(config.worker_threads)?;
    let (inner, context) =
        establish_runtime(Some(tokio_rt.handle().clone()), config, None, None, None);

    let scoped = run_scoped(&tokio_rt, &inner, f);
    let drain = close_and_drain(&inner, &tokio_rt);

    finish_runtime(&inner, tokio_rt, context, drain, scoped)
}

/// Tokio's own default worker count.
///
/// The test entries size their executor the way `new_multi_thread()` would
/// have with no `worker_threads` call, which is not `RuntimeConfig`'s default:
/// a `#[camber::test]` builds a whole runtime per test case and many run at
/// once, so the server-shaped 4× oversubscription would be paid hundreds of
/// times over for tests that spawn a handful of tasks.
fn tokio_default_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Build the Tokio multi-thread executor one Camber runtime will run on.
fn build_executor(worker_threads: usize) -> Result<tokio::runtime::Runtime, crate::RuntimeError> {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(worker_threads)
        .enable_all()
        .build()?;
    Ok(executor)
}

/// Establish one Camber runtime: assemble the shared state its children reach
/// it through, publish it to an attached test schedule, and install it as this
/// thread's runtime context.
///
/// All three runtime-establishing entry points enter here — the two that build
/// their own executor, and the doc-hidden context seam — so what it takes to BE
/// a running Camber runtime is written once instead of kept in step by hand,
/// the same reason `finish_runtime` owns the teardown tail. The three
/// attachments are passed in rather than assigned by the caller because they
/// must be in place before the state is shared: once the `Arc` exists, a child
/// can already read them.
///
/// The executor is taken as an `Option<Handle>` rather than as the owning
/// `Runtime`, because the seam owns no executor: it establishes a runtime on
/// whatever Tokio context is already entered, and `None` is what a runtime with
/// nowhere to launch a child reports through `RuntimeInner::executor`.
pub(crate) fn establish_runtime(
    tokio_handle: Option<TokioHandle>,
    config: RuntimeConfig,
    test_schedule: Option<Arc<RuntimeSchedule>>,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    health_state: Option<HealthState>,
) -> (Arc<RuntimeInner>, RuntimeContextGuard) {
    let mut inner = RuntimeInner::with_config_and_schedule(config, test_schedule);
    inner.tokio_handle = tokio_handle;
    inner.metrics_handle = metrics_handle;
    inner.health_state = health_state;

    let inner = Arc::new(inner);
    inner.publish_to_test_schedule();
    let context = install_runtime(Arc::clone(&inner));

    (inner, context)
}

/// Run a future inside the runtime's task-local scope, catching an unwind
/// instead of letting it escape past teardown.
///
/// An unwind that escaped here would drop the Tokio runtime mid-unwind, and
/// `Runtime::drop` waits UNBOUNDED on every `spawn_blocking` child — the exact
/// hang `FORCED_JOIN_GRACE` and `shutdown_timeout` exist to prevent. It would
/// also skip the whole teardown precedence: `ScopeClosing` never fires, so no
/// Camber-owned loop is told to stop, and no registered resource is shut down.
/// The payload is carried to the far side of teardown and resumed there, so
/// the caller's panic semantics are unchanged. Every failed assertion inside
/// `#[camber::test]` takes this path.
fn run_scoped<B, Fut>(
    tokio_rt: &tokio::runtime::Runtime,
    inner: &Arc<RuntimeInner>,
    body: B,
) -> ScopedOutcome<Fut::Output>
where
    B: FnOnce() -> Fut,
    Fut: std::future::Future,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        tokio_rt.block_on(crate::runtime_state::scope_runtime(
            Arc::clone(inner),
            body(),
        ))
    }))
}

/// What the scoped run produced: the closure's value, or the payload its
/// unwind carried. `Box<dyn Any + Send>` is the payload type `catch_unwind`
/// and `resume_unwind` are defined in terms of, not a choice of dispatch.
type ScopedOutcome<T> = Result<T, Box<dyn std::any::Any + Send>>;

/// Close root-scope admission, then drain the scope.
///
/// Closing is what tells every Camber-owned child to stop, so it must precede
/// the wait that expects them to have stopped. Both entry points drain through
/// here, so neither can drift into closing at a different point than the other
/// — which is what they had already done.
fn close_and_drain(
    inner: &RuntimeInner,
    tokio_rt: &tokio::runtime::Runtime,
) -> Option<crate::RuntimeError> {
    inner.close_scope();
    drain_root_scope(inner, tokio_rt.handle())
}

/// Order the runtime-level failures that displace the closure's value.
///
/// A recorded internal panic outranks a drain timeout: a timeout is frequently
/// the consequence of a wedged panicking child, so reporting the panic
/// localizes the fault. The closure's value is never inspected — `T` is opaque,
/// so it is either returned or displaced.
///
/// The displaced timeout is LOGGED rather than dropped. Exactly one error
/// leaves through the `Result`, and this is the case the precedence above calls
/// common, so the loser has no return path left at all — and the value it
/// carries is the child count that failed to exit cooperatively, which nothing
/// else reports: `drain_root_scope` constructs it silently and the forced
/// stop's own warning fires only when its grace expires.
fn runtime_failure(
    inner: &RuntimeInner,
    drain: Option<crate::RuntimeError>,
) -> Option<crate::RuntimeError> {
    match (inner.take_internal_panic(), drain) {
        (Some(panicked), Some(displaced)) => {
            tracing::warn!(%displaced, "drain timeout displaced by a recorded internal panic");
            Some(panicked)
        }
        (panicked, drain) => panicked.or(drain),
    }
}

/// Run a closure within a scoped Camber runtime with default configuration.
///
/// Returns the closure's value on success, or a `RuntimeError` on runtime
/// misconfiguration. Library code should propagate the error; only CLI
/// binaries should map it to a process exit.
pub fn run<F, T>(f: F) -> Result<T, crate::RuntimeError>
where
    F: FnOnce() -> T,
{
    reject_nested_runtime()?;
    run_inner_impl(
        RuntimeConfig::default(),
        None,
        Vec::new().into(),
        f,
        #[cfg(feature = "acme")]
        None,
        #[cfg(feature = "dns01")]
        None,
        #[cfg(feature = "otel")]
        None,
    )
}

fn run_inner_impl<F, T>(
    config: RuntimeConfig,
    test_schedule: Option<Arc<RuntimeSchedule>>,
    resources: Arc<[Box<dyn Resource>]>,
    f: F,
    #[cfg(feature = "acme")] acme_state: Option<crate::acme::AcmeState<std::io::Error>>,
    #[cfg(feature = "dns01")] dns01_setup: Option<crate::dns01::Dns01Setup>,
    #[cfg(feature = "otel")] otel_endpoint: Option<Box<str>>,
) -> Result<T, crate::RuntimeError>
where
    F: FnOnce() -> T,
{
    // Resolved first: a recorder the process refuses is a startup failure, and
    // failing before an executor is built or a DNS-01 cert is provisioned means
    // nothing has to be unwound to report it.
    let metrics_handle = install_metrics(config.metrics_enabled)?;

    let tokio_rt = build_executor(config.worker_threads)?;

    // DNS-01 setup: provision or load cert before the server starts, so the
    // config the runtime is established with already carries the result.
    #[cfg(feature = "dns01")]
    let (config, dns01_renewal) = provision_dns01(&tokio_rt, config, dns01_setup)?;

    // The OTLP exporter is installed HERE, in the function that shuts it down,
    // and after the last step that can return early. `init_exporter` leaves a
    // running batch span processor in a process-global that only
    // `shutdown_exporter` takes back, so a `?` returning between the two would
    // leave that processor installed with its buffer never flushed — and the
    // next `RuntimeBuilder::run` asking for otel would overwrite the slot,
    // dropping the previous provider without `shutdown()` and losing its
    // buffered spans silently. Nothing below this line returns early before the
    // teardown at the bottom.
    #[cfg(feature = "otel")]
    if let Some(endpoint) = otel_endpoint {
        crate::http::otel::init_exporter(&endpoint)?;
    }

    let health_state = build_health_state(&resources);
    let health_interval = config.health_interval;

    let (inner, context) = establish_runtime(
        Some(tokio_rt.handle().clone()),
        config,
        test_schedule,
        metrics_handle,
        health_state.clone(),
    );

    // Run the user closure inside tokio's block_on so that tokio::spawn_blocking
    // and other tokio APIs are available on this thread.
    let runtime_scope = || async {
        // Run initial health checks before the user closure starts serving
        // traffic. Runs inside block_on so Handle::current() is available
        // for resources that need async I/O (e.g. ProxyHealthResource).
        if let Some(ref hs) = health_state {
            run_initial_health_checks(&resources, hs).await;
        }

        admit_owned_subsystems(
            &inner,
            &resources,
            &health_state,
            health_interval,
            #[cfg(feature = "acme")]
            acme_state,
            #[cfg(feature = "dns01")]
            dns01_renewal,
        );

        f()
    };
    // The closure's return closes root-scope admission and fires ScopeClosing.
    // It is not a shutdown request: ShutdownSignal stays unset. An unwinding
    // closure reaches the same close, which is why the run is caught.
    let scoped = run_scoped(&tokio_rt, &inner, runtime_scope);

    // Drain the root scope before anything else: no child the executor can
    // stop may still reach a registered resource once shutdown starts.
    let drain = close_and_drain(&inner, &tokio_rt);

    shutdown_resources(&resources);

    #[cfg(feature = "otel")]
    crate::http::otel::shutdown_exporter();

    finish_runtime(&inner, tokio_rt, context, drain, scoped)
}

/// Provision or load the DNS-01 certificate before the server starts, folding
/// the result into the config the runtime will be established with.
///
/// Runs on the executor the runtime will use, before that runtime exists: the
/// cert has to be in the config the shared state is built from, and no scope
/// child may be admitted until it is.
#[cfg(feature = "dns01")]
fn provision_dns01(
    tokio_rt: &tokio::runtime::Runtime,
    config: RuntimeConfig,
    setup: Option<crate::dns01::Dns01Setup>,
) -> Result<(RuntimeConfig, Option<Dns01Renewal>), crate::RuntimeError> {
    let setup = match setup {
        Some(setup) => setup,
        None => return Ok((config, None)),
    };
    let state = tokio_rt.block_on(crate::dns01::init_dns01(setup))?;

    let mut config = config;
    config.tls_config = Some(state.tls_config);
    config.cert_store = Some(state.store.clone());
    Ok((
        config,
        Some(Dns01Renewal {
            acme: state.acme,
            provider: state.provider,
            store: state.store,
        }),
    ))
}

/// What the DNS-01 renewal loop is built from, carried out of provisioning.
///
/// The provisioning state also holds the TLS material, which belongs to the
/// config and is consumed there; splitting it here means the admission step
/// receives only what its loop actually needs.
#[cfg(feature = "dns01")]
struct Dns01Renewal {
    acme: crate::dns01::AcmeDns01,
    provider: crate::dns01::CloudflareProvider,
    store: CertStore,
}

/// Build health state from registered resources: one `AtomicBool` per
/// resource, or nothing at all when none is registered.
fn build_health_state(resources: &[Box<dyn Resource>]) -> Option<HealthState> {
    match resources.is_empty() {
        true => None,
        // `Arc<[T]>: FromIterator<T>` over a `TrustedLen` map allocates once,
        // where collecting through a `Vec` allocates twice and memcpys.
        false => Some(
            resources
                .iter()
                .map(|r| (Box::from(r.name()), AtomicBool::new(true)))
                .collect(),
        ),
    }
}

/// Admit every Camber-owned background subsystem this runtime carries.
///
/// Each becomes a root-scope child with a lifecycle-signal arm, so teardown
/// awaits its completion instead of aborting it, and each is admitted through
/// the wrapper that names it in a refusal. Extracted whole: the entry point
/// that used to hold this inline was also building the runtime, provisioning
/// DNS-01, and draining the scope.
///
/// Every one of them is admitted to the runtime this function was HANDED, not
/// to whichever runtime the ambient task-local happens to resolve to. The two
/// coincide today; nothing in the types said so, and the signal watcher was the
/// plainest case — its `Arc` named this runtime in the shutdown request it
/// applies while the scope that owned it came from somewhere else entirely.
fn admit_owned_subsystems(
    inner: &Arc<RuntimeInner>,
    resources: &Arc<[Box<dyn Resource>]>,
    health_state: &Option<HealthState>,
    health_interval: Duration,
    #[cfg(feature = "acme")] acme_state: Option<crate::acme::AcmeState<std::io::Error>>,
    #[cfg(feature = "dns01")] dns01_renewal: Option<Dns01Renewal>,
) {
    // The refusal is already reported against the subsystem's own name, and
    // this setup runs inside the block that yields the user closure's value —
    // there is no result here to propagate one through.
    drop(admit_signal_watcher(inner));

    #[cfg(feature = "acme")]
    if let Some(state) = acme_state {
        drop(crate::task::admit_signalled_subsystem_on(
            inner,
            "acme renewal",
            move |signals| crate::acme::acme_renewal_loop(state, signals),
        ));
    }

    #[cfg(feature = "dns01")]
    if let Some(renewal) = dns01_renewal {
        drop(crate::task::admit_signalled_subsystem_on(
            inner,
            "dns01 renewal",
            move |signals| {
                crate::dns01::dns01_renewal_loop(
                    renewal.acme,
                    renewal.provider,
                    renewal.store,
                    signals,
                )
            },
        ));
    }

    admit_health_tasks(inner, resources, health_state, health_interval);
}

/// Admit the OS signal watcher one runtime owns.
///
/// The runtime's own setup and the doc-hidden test seam both enter here, so the
/// seam admits the construction production uses instead of rebuilding it: the
/// sources are registered synchronously before admission — so a signal raised
/// immediately afterwards reaches an installed handler — and the request the
/// watcher applies names this runtime.
///
/// That last part is now enforced rather than assumed: the ONE `Arc` supplies
/// the shutdown request the watcher applies, the signals it stops on, and the
/// scope that awaits it. The ambient form let the first come from the argument
/// while the other two came from a task-local lookup.
pub(crate) fn admit_signal_watcher(inner: &Arc<RuntimeInner>) -> Result<(), crate::RuntimeError> {
    // The one clone the watcher genuinely keeps: the request it applies at
    // shutdown outlives this call. Taking the `Arc` by value would add a
    // second, since the admission itself only borrows.
    let requested = Arc::clone(inner);
    crate::task::admit_signalled_subsystem_on(inner, "signal watcher", move |signals| {
        crate::signals::signal_watcher_loop(
            crate::signals::SignalSources::register(),
            crate::signals::ShutdownRequest::Runtime(requested),
            signals,
        )
    })
}

/// Close out a drained runtime: release the thread's context, give the
/// detached synchronous-entry connection tasks their shutdown window, and let
/// a runtime-level failure displace the closure's value.
///
/// Both runtime-establishing entry points end here, so the closing order — and
/// which failure wins — is written once rather than kept in step by hand.
///
/// A caught unwind resumes at the very end, once bounded teardown has run in
/// full. It outranks every runtime-level failure by construction: the payload
/// leaves through the panic, not through the `Result`.
fn finish_runtime<T>(
    inner: &RuntimeInner,
    tokio_rt: tokio::runtime::Runtime,
    runtime_guard: RuntimeContextGuard,
    drain: Option<crate::RuntimeError>,
    scoped: ScopedOutcome<T>,
) -> Result<T, crate::RuntimeError> {
    teardown_runtime(inner);
    drop(runtime_guard);

    // This is the detached synchronous-entry connection tasks' window, not a
    // second scope drain: the scope already drained above.
    tokio_rt.shutdown_timeout(inner.config.shutdown_timeout);

    // Read BEFORE the unwind resumes: `resume_unwind` diverges, and the panic
    // slot has exactly one reader.
    match (scoped, runtime_failure(inner, drain)) {
        (Ok(value), None) => Ok(value),
        (Ok(_), Some(error)) => Err(error),
        (Err(payload), failure) => resume_past_failure(failure, payload),
    }
}

/// Resume the closure's caught unwind, logging the runtime-level failure that
/// unwind displaces.
///
/// The payload leaves through the panic, not through the `Result`, so a
/// recorded internal panic or a drain timeout has no return path left at all.
/// The log is the only trace it can leave, and without it a Camber-owned child
/// that panicked inside a failing `#[camber::test]` — the common case, since
/// every failed assertion takes this path — disappeared completely.
fn resume_past_failure(
    failure: Option<crate::RuntimeError>,
    payload: Box<dyn std::any::Any + Send>,
) -> ! {
    if let Some(error) = failure {
        tracing::error!(%error, "runtime failure displaced by an unwinding closure");
    }
    std::panic::resume_unwind(payload)
}

fn reject_nested_runtime() -> Result<(), crate::RuntimeError> {
    match (has_runtime(), tokio::runtime::Handle::try_current().is_ok()) {
        (false, false) => Ok(()),
        _ => Err(crate::RuntimeError::InvalidArgument(
            "nested runtime creation is not supported".into(),
        )),
    }
}

/// Build the Prometheus recorder and install it as the process-global one.
///
/// A rejected installation is an ERROR, not a warning: the handle it yields is
/// wired to a recorder no metric will ever reach, so `/metrics` would render
/// empty for the life of the process. The caller asked for `with_metrics()`,
/// and an endpoint that can never report is worse than a startup refusal.
fn init_prometheus_recorder() -> Result<metrics_exporter_prometheus::PrometheusHandle, Box<str>> {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    match metrics::set_global_recorder(recorder) {
        Ok(()) => Ok(handle),
        Err(error) => {
            Err(format!("global metrics recorder is already installed: {error}").into_boxed_str())
        }
    }
}

/// The process's Prometheus handle, installing the recorder on first use.
///
/// `metrics` accepts one global recorder per process, so the install itself is
/// what the `OnceLock` runs: `get_or_init` admits exactly one initializer and
/// blocks every other caller until it finishes, so a second runtime asking for
/// metrics reuses the first one's handle instead of racing it to a refusal it
/// did not cause.
///
/// The outcome is memoized either way, and both halves are load-bearing. A
/// handle is shared because a rejected recorder's handle is dead — memoizing
/// one would leave `/metrics` permanently empty for code that never asked. A
/// refusal is shared because it means the APPLICATION installed its own
/// recorder: that answer cannot change later in the process, so reporting it
/// once per caller is the honest result rather than a retry that must fail
/// again.
fn shared_metrics_handle()
-> Result<Option<metrics_exporter_prometheus::PrometheusHandle>, crate::RuntimeError> {
    static HANDLE: std::sync::OnceLock<
        Result<metrics_exporter_prometheus::PrometheusHandle, Box<str>>,
    > = std::sync::OnceLock::new();
    match HANDLE.get_or_init(init_prometheus_recorder) {
        Ok(handle) => Ok(Some(handle.clone())),
        Err(reason) => Err(crate::RuntimeError::Config(reason.clone())),
    }
}

/// The metrics handle a runtime starts with: `None` unless it asked for one.
fn install_metrics(
    enabled: bool,
) -> Result<Option<metrics_exporter_prometheus::PrometheusHandle>, crate::RuntimeError> {
    match enabled {
        false => Ok(None),
        true => shared_metrics_handle(),
    }
}
