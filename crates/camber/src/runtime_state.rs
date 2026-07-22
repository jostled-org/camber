use crate::resource::HealthState;
use crate::runtime_test_support::{RuntimeCheckpoint, RuntimeSchedule};
use crate::tls::CertStore;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
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

/// Shared runtime state. Async tasks use Tokio task-local storage; synchronous
/// entry points use thread-local storage.
pub(crate) struct RuntimeInner {
    pub(crate) shutdown: Arc<AtomicBool>,
    pub(crate) shutdown_notify: Arc<tokio::sync::Notify>,
    task_tracker: TaskTracker,
    test_schedule: Option<Arc<RuntimeSchedule>>,
    cancel_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub(crate) config: RuntimeConfig,
    pub(crate) metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    pub(crate) tokio_handle: Option<tokio::runtime::Handle>,
    pub(crate) health_state: Option<HealthState>,
}

#[derive(Clone)]
pub(crate) struct ShutdownSignal {
    requested: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl ShutdownSignal {
    pub(crate) fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }

    pub(crate) async fn wait(&self) {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.is_requested() {
                true => return,
                false => notified.await,
            }
        }
    }
}

impl RuntimeInner {
    pub(crate) fn new() -> Self {
        Self::with_config(RuntimeConfig::default())
    }

    pub(crate) fn with_config(config: RuntimeConfig) -> Self {
        Self::with_config_and_schedule(config, None)
    }

    pub(crate) fn with_config_and_schedule(
        config: RuntimeConfig,
        test_schedule: Option<Arc<RuntimeSchedule>>,
    ) -> Self {
        Self {
            shutdown: Arc::new(AtomicBool::new(false)),
            shutdown_notify: Arc::new(tokio::sync::Notify::new()),
            task_tracker: TaskTracker::new(),
            test_schedule,
            cancel_task: Mutex::new(None),
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

    pub(crate) fn request_shutdown(&self) {
        self.shutdown.store(true, Ordering::Release);
        self.notify_shutdown();
    }

    pub(crate) fn shutdown_signal(&self) -> ShutdownSignal {
        ShutdownSignal {
            requested: Arc::clone(&self.shutdown),
            notify: Arc::clone(&self.shutdown_notify),
        }
    }

    pub(crate) fn task_started(&self) {
        self.task_tracker.start();
    }

    pub(crate) fn task_finished(&self) {
        self.task_tracker.finish();
    }

    pub(crate) fn pause_test_schedule(&self, checkpoint: RuntimeCheckpoint) {
        if let Some(schedule) = self.test_schedule.as_ref() {
            schedule.pause(checkpoint);
        }
    }

    fn replace_cancel_task(&self, task: tokio::task::JoinHandle<()>) {
        let mut current = self.cancel_task.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(previous) = current.replace(task) {
            previous.abort();
        }
    }

    fn abort_cancel_task(&self) {
        let mut current = self.cancel_task.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(task) = current.take() {
            task.abort();
        }
    }
}

struct TaskTracker {
    count: Mutex<usize>,
    done: Condvar,
}

impl TaskTracker {
    const fn new() -> Self {
        Self {
            count: Mutex::new(0),
            done: Condvar::new(),
        }
    }

    fn start(&self) {
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        *count += 1;
    }

    fn finish(&self) {
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        match *count {
            0 => tracing::error!("runtime task tracker completed an unregistered task"),
            1 => {
                *count = 0;
                self.done.notify_all();
            }
            current => *count = current - 1,
        }
    }

    fn wait(&self, schedule: Option<&RuntimeSchedule>) {
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        while *count > 0 {
            let checkpoint = RuntimeCheckpoint::TaskWaitPredicateObserved(*count);
            match schedule.filter(|schedule| schedule.is_armed(checkpoint)) {
                Some(schedule) => {
                    drop(count);
                    schedule.pause(checkpoint);
                    count = self.count.lock().unwrap_or_else(|e| e.into_inner());
                }
                None => {
                    count = self.done.wait(count).unwrap_or_else(|e| e.into_inner());
                }
            }
        }
    }

    fn wait_timeout(&self, timeout: Duration, schedule: Option<&RuntimeSchedule>) {
        let deadline = std::time::Instant::now() + timeout;
        let mut count = self.count.lock().unwrap_or_else(|e| e.into_inner());
        while *count > 0 {
            let checkpoint = RuntimeCheckpoint::TaskWaitPredicateObserved(*count);
            if let Some(schedule) = schedule.filter(|schedule| schedule.is_armed(checkpoint)) {
                drop(count);
                schedule.pause(checkpoint);
                count = self.count.lock().unwrap_or_else(|e| e.into_inner());
                continue;
            }
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return;
            }
            let (next_count, result) = self
                .done
                .wait_timeout(count, remaining)
                .unwrap_or_else(|e| e.into_inner());
            count = next_count;
            if result.timed_out() {
                return;
            }
        }
    }
}

tokio::task_local! {
    static TASK_RUNTIME: Arc<RuntimeInner>;
}

thread_local! {
    static RUNTIME: std::cell::RefCell<Option<Arc<RuntimeInner>>> = const { std::cell::RefCell::new(None) };
    static CANCEL_FLAG: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
    static CANCEL_CHANNEL: std::cell::RefCell<Option<crossbeam_channel::Receiver<()>>> = const { std::cell::RefCell::new(None) };
}

/// Restores the prior synchronous runtime context when its scope exits.
pub(crate) struct RuntimeContextGuard {
    previous: Option<Arc<RuntimeInner>>,
}

impl Drop for RuntimeContextGuard {
    fn drop(&mut self) {
        RUNTIME.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

/// Restores per-task cancellation state when a blocking worker is reused.
pub(crate) struct CancelContextGuard {
    previous_flag: Option<Arc<AtomicBool>>,
    previous_channel: Option<crossbeam_channel::Receiver<()>>,
}

impl Drop for CancelContextGuard {
    fn drop(&mut self) {
        CANCEL_FLAG.with(|cell| {
            *cell.borrow_mut() = self.previous_flag.take();
        });
        CANCEL_CHANNEL.with(|cell| {
            *cell.borrow_mut() = self.previous_channel.take();
        });
    }
}

/// Install cancellation state for a blocking task and restore it on drop.
pub(crate) fn install_cancel_context(
    flag: Arc<AtomicBool>,
    channel: crossbeam_channel::Receiver<()>,
) -> CancelContextGuard {
    let previous_flag = CANCEL_FLAG.with(|cell| cell.borrow_mut().replace(flag));
    let previous_channel = CANCEL_CHANNEL.with(|cell| cell.borrow_mut().replace(channel));
    CancelContextGuard {
        previous_flag,
        previous_channel,
    }
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

/// Register an external shutdown signal. When `future` completes, Camber
/// treats it as a shutdown request. Calling again replaces the previous signal.
pub fn on_cancel<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    let inner = ensure_context();
    let task_inner = Arc::clone(&inner);
    let handle = tokio::spawn(async move {
        future.await;
        task_inner.request_shutdown();
    });
    inner.replace_cancel_task(handle);
}

/// Ensure a runtime exists on the current thread. Creates one lazily if absent.
/// Returns an Arc to the runtime for immediate use.
pub(crate) fn ensure_context() -> Arc<RuntimeInner> {
    if let Ok(inner) = TASK_RUNTIME.try_with(Arc::clone) {
        return inner;
    }
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
    inner.request_shutdown();
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
    if let Ok(shutting_down) = TASK_RUNTIME.try_with(|inner| inner.shutdown.load(Ordering::Acquire))
    {
        return shutting_down;
    }
    RUNTIME.with(|cell| {
        let borrow = cell.borrow();
        match borrow.as_ref() {
            Some(inner) => inner.shutdown.load(Ordering::Acquire),
            None => false,
        }
    })
}

pub(crate) fn has_runtime() -> bool {
    TASK_RUNTIME.try_with(|_| ()).is_ok() || RUNTIME.with(|cell| cell.borrow().is_some())
}

/// Get the shutdown flag and notify from the current runtime.
/// Used by the schedule module to stop tasks on shutdown.
pub(crate) fn shutdown_signal() -> ShutdownSignal {
    ensure_context().shutdown_signal()
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

pub(crate) fn install_runtime(inner: Arc<RuntimeInner>) -> RuntimeContextGuard {
    let previous = RUNTIME.with(|cell| cell.borrow_mut().replace(inner));
    RuntimeContextGuard { previous }
}

/// Scope runtime context to a future so it follows that future across workers.
pub(crate) async fn scope_runtime<F>(inner: Arc<RuntimeInner>, future: F) -> F::Output
where
    F: Future,
{
    TASK_RUNTIME.scope(inner, future).await
}

/// Abort a lingering external cancellation watcher for this runtime.
pub(crate) fn teardown_runtime(inner: &RuntimeInner) {
    inner.abort_cancel_task();
}

pub(crate) fn wait_for_tasks(inner: &RuntimeInner) {
    inner.task_tracker.wait(inner.test_schedule.as_deref());
}

pub(crate) fn wait_for_tasks_timeout(inner: &RuntimeInner, timeout: Duration) {
    inner
        .task_tracker
        .wait_timeout(timeout, inner.test_schedule.as_deref());
}
