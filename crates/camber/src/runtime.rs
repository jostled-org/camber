use crate::resource::{HealthState, MIN_HEALTH_INTERVAL, Resource};
use crate::resource_lifecycle::{
    run_initial_health_checks, shutdown_resources, spawn_health_tasks,
};
use crate::runtime_state::{
    RuntimeConfig, RuntimeInner, install_runtime, teardown_runtime, wait_for_tasks,
    wait_for_tasks_timeout,
};
use crate::tls::CertStore;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Re-export of `tokio::runtime::Handle` for use with [`tokio_handle()`].
pub use tokio::runtime::Handle as TokioHandle;

// Re-export crate-internal items so `use crate::runtime;` call sites keep working.
pub(crate) use crate::runtime_state::{
    cancel_channel, check_cancel, current_runtime, has_runtime, shutdown_notify, shutdown_signal,
};

/// Register an external shutdown signal. When `future` completes, Camber
/// treats it as a shutdown request. Calling again replaces the previous signal.
pub fn on_cancel<F>(future: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    crate::runtime_state::on_cancel(future);
}

/// Run a future to completion on the current Tokio runtime.
///
/// Calls `block_in_place` + `Handle::block_on` internally. Use inside
/// `runtime::run` closures or `camber::spawn` tasks to call async code.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    crate::runtime_state::block_on(f)
}

/// Signal the runtime to shut down.
pub fn request_shutdown() {
    crate::runtime_state::request_shutdown();
}

/// Return the underlying Tokio runtime handle.
///
/// Use this inside handlers to run async code via `handle.block_on(...)`.
/// Panics if called outside a Camber runtime.
pub fn tokio_handle() -> tokio::runtime::Handle {
    crate::runtime_state::tokio_handle()
}

/// Check whether shutdown has been requested.
pub fn is_shutting_down() -> bool {
    crate::runtime_state::is_shutting_down()
}

impl std::fmt::Debug for RuntimeBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RuntimeBuilder")
            .field("worker_threads", &self.config.worker_threads)
            .field("shutdown_timeout", &self.config.shutdown_timeout)
            .field("keepalive_timeout", &self.config.keepalive_timeout)
            .field("tracing_enabled", &self.config.tracing_enabled)
            .field("metrics_enabled", &self.config.metrics_enabled)
            .field("health_interval", &self.config.health_interval)
            .field("connection_limit", &self.config.connection_limit)
            .field("resource_count", &self.resources.len())
            .field("has_tls", &self.tls_cert_store.is_some())
            .finish()
    }
}

/// Configure a Camber runtime before running.
pub struct RuntimeBuilder {
    config: RuntimeConfig,
    resources: Vec<Box<dyn Resource>>,
    tls_cert_path: Option<std::path::PathBuf>,
    tls_key_path: Option<std::path::PathBuf>,
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

    /// Register a resource for lifecycle management. Resources are shut down
    /// in reverse registration order during runtime teardown.
    pub fn resource(mut self, r: impl Resource) -> Self {
        self.resources.push(Box::new(r));
        self
    }

    pub fn with_tracing(mut self) -> Self {
        self.config.tracing_enabled = true;
        self
    }

    pub fn with_metrics(mut self) -> Self {
        self.config.metrics_enabled = true;
        self
    }

    #[cfg(feature = "profiling")]
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
        self.tls_cert_path = Some(path.to_path_buf());
        self
    }

    /// Set the TLS private key PEM file path.
    pub fn tls_key(mut self, path: &std::path::Path) -> Self {
        self.tls_key_path = Some(path.to_path_buf());
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
        let (tls_cfg, store) =
            crate::tls::resolve_tls(self.tls_cert_store, self.tls_cert_path, self.tls_key_path)?;
        config.tls_config = tls_cfg;
        config.cert_store = store;

        #[cfg(feature = "otel")]
        if let Some(ref endpoint) = self.otel_endpoint {
            crate::http::otel::init_exporter(endpoint)?;
        }

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
            self.resources.into_boxed_slice(),
            f,
            #[cfg(feature = "acme")]
            acme_state,
            #[cfg(feature = "dns01")]
            self.dns01_setup,
        )
    }

    fn validate_tls_options(&self) -> Result<(), crate::RuntimeError> {
        let has_manual = self.tls_cert_path.is_some()
            || self.tls_key_path.is_some()
            || self.tls_cert_store.is_some();

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
    builder()
        .keepalive_timeout(Duration::from_millis(100))
        .shutdown_timeout(Duration::from_secs(1))
        .run(f)
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
    let shutdown_timeout = Duration::from_secs(1);

    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    let mut inner = RuntimeInner::with_config(RuntimeConfig {
        keepalive_timeout: Duration::from_millis(100),
        shutdown_timeout,
        ..RuntimeConfig::default()
    });
    inner.tokio_handle = Some(tokio_rt.handle().clone());
    let inner = Arc::new(inner);

    install_runtime(Arc::clone(&inner));

    let result = tokio_rt.block_on(f());

    wait_for_tasks_timeout(&inner, shutdown_timeout);

    teardown_runtime();

    tokio_rt.shutdown_timeout(shutdown_timeout);

    Ok(result)
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
    run_inner_impl(
        RuntimeConfig::default(),
        Vec::new().into_boxed_slice(),
        f,
        #[cfg(feature = "acme")]
        None,
        #[cfg(feature = "dns01")]
        None,
    )
}

fn run_inner_impl<F, T>(
    config: RuntimeConfig,
    resources: Box<[Box<dyn Resource>]>,
    f: F,
    #[cfg(feature = "acme")] acme_state: Option<crate::acme::AcmeState<std::io::Error>>,
    #[cfg(feature = "dns01")] dns01_setup: Option<crate::dns01::Dns01Setup>,
) -> Result<T, crate::RuntimeError>
where
    F: FnOnce() -> T,
{
    let shutdown_timeout = config.shutdown_timeout;
    let metrics_enabled = config.metrics_enabled;

    // Build a Tokio multi-thread runtime with configured worker threads.
    let tokio_rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(config.worker_threads)
        .enable_all()
        .build()?;

    // DNS-01 setup: provision or load cert before server starts.
    #[cfg(feature = "dns01")]
    let (config, dns01_state) = match dns01_setup {
        Some(setup) => {
            let state = tokio_rt.block_on(crate::dns01::init_dns01(setup))?;
            let mut cfg = config;
            cfg.tls_config = Some(state.tls_config.clone());
            cfg.cert_store = Some(state.store.clone());
            (cfg, Some(state))
        }
        None => (config, None),
    };

    let resources: Arc<[Box<dyn Resource>]> = resources.into();

    // Build health state from registered resources: one AtomicBool per resource.
    let health_state: Option<HealthState> = match resources.is_empty() {
        true => None,
        false => Some(
            resources
                .iter()
                .map(|r| (Box::from(r.name()), AtomicBool::new(true)))
                .collect::<Vec<_>>()
                .into_boxed_slice()
                .into(),
        ),
    };

    let health_interval = config.health_interval;

    let mut inner = RuntimeInner::with_config(config);

    inner.metrics_handle = install_metrics(metrics_enabled);

    inner.tokio_handle = Some(tokio_rt.handle().clone());
    inner.health_state = health_state.clone();

    let inner = Arc::new(inner);

    install_runtime(Arc::clone(&inner));

    // Run the user closure inside tokio's block_on so that tokio::spawn_blocking
    // and other tokio APIs are available on this thread.
    let result = tokio_rt.block_on(async {
        // Run initial health checks before the user closure starts serving
        // traffic. Runs inside block_on so Handle::current() is available
        // for resources that need async I/O (e.g. ProxyHealthResource).
        if let Some(ref hs) = health_state {
            run_initial_health_checks(&resources, hs).await;
        }

        let signal_task = crate::signals::spawn_signal_watcher(
            inner.shutdown.clone(),
            inner.shutdown_notify.clone(),
        );

        #[cfg(feature = "acme")]
        let acme_task = acme_state.map(spawn_acme_renewal);

        #[cfg(feature = "dns01")]
        let dns01_task = dns01_state.map(|s| s.acme.spawn_renewal(s.provider, s.store));

        let health_tasks = spawn_health_tasks(
            &resources,
            &health_state,
            health_interval,
            &inner.shutdown_notify,
        );

        let value = f();

        // Abort background tasks — no longer needed after the closure returns.
        signal_task.abort();
        for task in &health_tasks {
            task.abort();
        }
        #[cfg(feature = "acme")]
        if let Some(task) = acme_task {
            task.abort();
        }
        #[cfg(feature = "dns01")]
        if let Some(task) = dns01_task {
            task.abort();
        }

        value
    });

    // Wait for all spawned tasks to complete (structured concurrency).
    // When shutting down, apply the configured timeout as a safety net.
    match inner.shutdown.load(Ordering::Acquire) {
        true => wait_for_tasks_timeout(&inner, shutdown_timeout),
        false => wait_for_tasks(&inner),
    }

    shutdown_resources(&resources);

    #[cfg(feature = "otel")]
    crate::http::otel::shutdown_exporter();

    teardown_runtime();

    // Shut down the tokio runtime. Use shutdown_timeout to avoid blocking
    // indefinitely on spawn_blocking tasks that ignore the shutdown flag.
    tokio_rt.shutdown_timeout(shutdown_timeout);

    Ok(result)
}

fn init_prometheus_recorder() -> metrics_exporter_prometheus::PrometheusHandle {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    match metrics::set_global_recorder(recorder) {
        Ok(()) => {}
        Err(e) => tracing::warn!("failed to install global metrics recorder: {e}"),
    }
    handle
}

fn install_metrics(enabled: bool) -> Option<metrics_exporter_prometheus::PrometheusHandle> {
    static HANDLE: std::sync::OnceLock<metrics_exporter_prometheus::PrometheusHandle> =
        std::sync::OnceLock::new();
    match enabled {
        false => None,
        true => Some(HANDLE.get_or_init(init_prometheus_recorder).clone()),
    }
}

/// Spawn the ACME renewal stream as a background task.
/// Polls the AcmeState stream which handles cert provisioning, caching,
/// and renewal. Logs events and errors. Cancelled on shutdown.
#[cfg(feature = "acme")]
fn spawn_acme_renewal(
    state: crate::acme::AcmeState<std::io::Error>,
) -> tokio::task::JoinHandle<()> {
    use futures_util::StreamExt;

    tokio::spawn(async move {
        let mut state = std::pin::pin!(state);
        while let Some(event) = state.next().await {
            match event {
                Ok(ok) => tracing::info!("acme: {ok:?}"),
                Err(err) => tracing::warn!("acme: {err}"),
            }
        }
    })
}
