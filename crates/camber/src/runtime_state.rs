use crate::resource::HealthState;
use crate::task::{TaskSpawner, TokioSpawner};
use crate::tls::CertStore;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub(crate) const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const DEFAULT_KEEPALIVE_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const DEFAULT_HEALTH_INTERVAL: Duration = Duration::from_secs(10);

pub(crate) type TlsConfig = Arc<rustls::ServerConfig>;

pub(crate) fn default_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() * 4)
        .unwrap_or(16)
}

/// Runtime configuration. Stored in RuntimeInner, read by server components.
#[derive(Clone)]
pub(crate) struct RuntimeConfig {
    pub(crate) worker_threads: usize,
    pub(crate) shutdown_timeout: Duration,
    pub(crate) keepalive_timeout: Duration,
    pub(crate) tracing_enabled: bool,
    pub(crate) metrics_enabled: bool,
    #[cfg(feature = "profiling")]
    pub(crate) profiling_enabled: bool,
    pub(crate) health_interval: Duration,
    pub(crate) connection_limit: Option<usize>,
    pub(crate) tls_config: Option<TlsConfig>,
    pub(crate) cert_store: Option<CertStore>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads: default_worker_threads(),
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            keepalive_timeout: DEFAULT_KEEPALIVE_TIMEOUT,
            tracing_enabled: false,
            metrics_enabled: false,
            #[cfg(feature = "profiling")]
            profiling_enabled: false,
            health_interval: DEFAULT_HEALTH_INTERVAL,
            connection_limit: None,
            tls_config: None,
            cert_store: None,
        }
    }
}

/// Shared runtime state. Stored as Arc<RuntimeInner> in thread-local.
pub(crate) struct RuntimeInner {
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) shutdown_notify: Arc<tokio::sync::Notify>,
    pub(crate) task_count: AtomicUsize,
    pub(crate) task_done: Condvar,
    pub(crate) task_done_mu: Mutex<()>,
    pub(crate) spawner: Box<dyn TaskSpawner>,
    pub(crate) config: RuntimeConfig,
    pub(crate) metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    pub(crate) tokio_handle: Option<tokio::runtime::Handle>,
    pub(crate) health_state: Option<HealthState>,
}

impl RuntimeInner {
    pub(crate) fn new() -> Self {
        Self::with_config(RuntimeConfig::default())
    }

    pub(crate) fn with_config(config: RuntimeConfig) -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            task_count: AtomicUsize::new(0),
            task_done: Condvar::new(),
            task_done_mu: Mutex::new(()),
            spawner: Box::new(TokioSpawner),
            config,
            metrics_handle: None,
            tokio_handle: None,
            health_state: None,
        }
    }

    /// Notify all listeners that shutdown has been requested.
    pub(crate) fn notify_shutdown(&self) {
        self.shutdown_notify.notify_waiters();
    }
}

thread_local! {
    static RUNTIME: std::cell::RefCell<Option<Arc<RuntimeInner>>> = const { std::cell::RefCell::new(None) };
    static CANCEL_FLAG: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
    static CANCEL_CHANNEL: std::cell::RefCell<Option<crossbeam_channel::Receiver<()>>> = const { std::cell::RefCell::new(None) };
}

/// Install a per-task cancellation flag on the current thread.
pub(crate) fn install_cancel_flag(flag: Arc<AtomicBool>) {
    CANCEL_FLAG.with(|cell| {
        *cell.borrow_mut() = Some(flag);
    });
}

/// Install a per-task cancellation channel on the current thread.
pub(crate) fn install_cancel_channel(rx: crossbeam_channel::Receiver<()>) {
    CANCEL_CHANNEL.with(|cell| {
        *cell.borrow_mut() = Some(rx);
    });
}

/// Get the current task's cancellation channel receiver (if any).
pub(crate) fn cancel_channel() -> Option<crossbeam_channel::Receiver<()>> {
    CANCEL_CHANNEL.with(|cell| cell.borrow().clone())
}

/// Check whether the current task has been cancelled.
pub(crate) fn check_cancel() -> Result<(), crate::RuntimeError> {
    CANCEL_FLAG.with(|cell| {
        let borrow = cell.borrow();
        match borrow.as_ref() {
            Some(flag) if flag.load(Ordering::Acquire) => Err(crate::RuntimeError::Cancelled),
            _ => Ok(()),
        }
    })
}

static GLOBAL_CANCEL: Mutex<Option<tokio::task::JoinHandle<()>>> = Mutex::new(None);

/// Register an external shutdown signal. When `future` completes, Camber
/// treats it as a shutdown request. Calling again replaces the previous signal.
pub fn on_cancel<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let inner = ensure_context();
    let shutdown = Arc::clone(&inner.shutdown);
    let shutdown_notify = Arc::clone(&inner.shutdown_notify);
    let handle = tokio::spawn(async move {
        future.await;
        shutdown.store(true, Ordering::Release);
        shutdown_notify.notify_waiters();
    });
    let mut guard = GLOBAL_CANCEL.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(prev) = guard.take() {
        prev.abort();
    }
    *guard = Some(handle);
}

/// Ensure a runtime exists on the current thread. Creates one lazily if absent.
/// Returns an Arc to the runtime for immediate use.
pub(crate) fn ensure_context() -> Arc<RuntimeInner> {
    RUNTIME.with(|cell| {
        {
            let borrow = cell.borrow();
            if let Some(inner) = borrow.as_ref() {
                return Arc::clone(inner);
            }
        }
        let inner = Arc::new(RuntimeInner::new());
        let cloned = Arc::clone(&inner);
        *cell.borrow_mut() = Some(inner);
        cloned
    })
}

/// Signal the runtime to shut down.
pub fn request_shutdown() {
    let inner = ensure_context();
    inner.shutdown.store(true, Ordering::Release);
    inner.notify_shutdown();
}

/// Return the underlying Tokio runtime handle.
///
/// Use this inside handlers to run async code via `handle.block_on(...)`.
/// Panics if called outside a Camber runtime.
pub fn tokio_handle() -> tokio::runtime::Handle {
    tokio::runtime::Handle::current()
}

/// Check whether shutdown has been requested.
pub fn is_shutting_down() -> bool {
    RUNTIME.with(|cell| {
        let borrow = cell.borrow();
        match borrow.as_ref() {
            Some(inner) => inner.shutdown.load(Ordering::Acquire),
            None => false,
        }
    })
}

pub(crate) fn has_runtime() -> bool {
    RUNTIME.with(|cell| cell.borrow().is_some())
}

/// Get the shutdown flag and notify from the current runtime.
/// Used by the schedule module to stop tasks on shutdown.
pub(crate) fn shutdown_signal() -> (Arc<AtomicBool>, Arc<tokio::sync::Notify>) {
    let inner = ensure_context();
    (
        Arc::clone(&inner.shutdown),
        Arc::clone(&inner.shutdown_notify),
    )
}

/// Get just the shutdown notify from the current runtime.
/// Used by accept loops that only need the notification, not the flag.
pub(crate) fn shutdown_notify() -> Arc<tokio::sync::Notify> {
    let inner = ensure_context();
    Arc::clone(&inner.shutdown_notify)
}

/// Bridge an async future to synchronous context.
///
/// Calls `block_in_place` + `Handle::block_on` internally. Use inside
/// `runtime::run` closures or `camber::spawn` tasks to call async code.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

/// Semantic alias for `ensure_context()`. Call sites that expect a runtime
/// to already exist (task spawning, server startup) use this name to
/// express intent. Delegates to `ensure_context` which lazily creates
/// a default runtime if none is installed.
pub(crate) fn current_runtime() -> Arc<RuntimeInner> {
    ensure_context()
}

pub(crate) fn install_runtime(inner: Arc<RuntimeInner>) {
    RUNTIME.with(|cell| {
        *cell.borrow_mut() = Some(inner);
    });
}

/// Abort lingering on_cancel task and clear the thread-local runtime.
pub(crate) fn teardown_runtime() {
    if let Some(handle) = GLOBAL_CANCEL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        handle.abort();
    }

    RUNTIME.with(|cell| {
        *cell.borrow_mut() = None;
    });
}

pub(crate) fn wait_for_tasks(inner: &Arc<RuntimeInner>) {
    if inner.task_count.load(Ordering::Acquire) == 0 {
        return;
    }
    let mut guard = inner.task_done_mu.lock().unwrap_or_else(|e| e.into_inner());
    while inner.task_count.load(Ordering::Acquire) > 0 {
        guard = inner
            .task_done
            .wait(guard)
            .unwrap_or_else(|e| e.into_inner());
    }
}

pub(crate) fn wait_for_tasks_timeout(inner: &Arc<RuntimeInner>, timeout: Duration) {
    if inner.task_count.load(Ordering::Acquire) == 0 {
        return;
    }
    let deadline = std::time::Instant::now() + timeout;
    let mut guard = inner.task_done_mu.lock().unwrap_or_else(|e| e.into_inner());
    while inner.task_count.load(Ordering::Acquire) > 0 {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return;
        }
        let (g, result) = inner
            .task_done
            .wait_timeout(guard, remaining)
            .unwrap_or_else(|e| e.into_inner());
        guard = g;
        if result.timed_out() {
            return;
        }
    }
}
