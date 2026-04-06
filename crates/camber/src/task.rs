use crate::RuntimeError;
use crate::runtime_state::{self as runtime, RuntimeInner};
use futures_util::FutureExt;
use std::future::{Future, IntoFuture};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

/// Future that completes when the runtime begins shutting down.
///
/// Spawned tasks can race against shutdown without needing a
/// `CancellationToken` threaded through every function signature.
/// If shutdown has already been requested, the future completes immediately.
pub async fn on_shutdown() {
    let (shutdown, shutdown_notify) = runtime::shutdown_signal();
    loop {
        match shutdown.load(Ordering::Acquire) {
            true => return,
            false => shutdown_notify.notified().await,
        }
    }
}

/// Abstraction over task execution.
pub(crate) trait TaskSpawner: Send + Sync {
    fn spawn_task(&self, f: Box<dyn FnOnce() + Send>);
}

/// Spawner that submits tasks to Tokio's blocking thread pool.
pub(crate) struct TokioSpawner;

impl TaskSpawner for TokioSpawner {
    fn spawn_task(&self, f: Box<dyn FnOnce() + Send>) {
        tokio::task::spawn_blocking(f);
    }
}

impl<T> std::fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinHandle")
            .field("completed", &self.rx.is_none())
            .finish()
    }
}

/// Handle to a spawned task. Retrieve the result with `join()`.
pub struct JoinHandle<T> {
    rx: Option<std::sync::mpsc::Receiver<Result<T, RuntimeError>>>,
    cancel: Arc<AtomicBool>,
    cancel_tx: Option<crossbeam_channel::Sender<()>>,
}

fn panic_to_error(payload: Box<dyn std::any::Any + Send>) -> RuntimeError {
    let msg = match (
        payload.downcast_ref::<&str>(),
        payload.downcast_ref::<String>(),
    ) {
        (Some(s), _) => (*s).into(),
        (_, Some(s)) => s.as_str().into(),
        _ => "unknown panic".into(),
    };
    RuntimeError::TaskPanicked(msg)
}

impl<T> JoinHandle<T> {
    /// Request cancellation. The task will observe `Cancelled` at the next
    /// Camber IO boundary (channel recv, http get/post, net read_request).
    /// Cancelling a completed task is a no-op.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Release);
        // Signal the cancel channel to unblock any select!-based waits.
        if let Some(tx) = &self.cancel_tx {
            let _ = tx.try_send(());
        }
    }

    /// Wait for the task to complete and return its result.
    ///
    /// Returns `Err(TaskPanicked)` if the task panicked.
    pub fn join(mut self) -> Result<T, RuntimeError> {
        let rx = match self.rx.take() {
            Some(rx) => rx,
            None => return Err(RuntimeError::TaskPanicked("handle already consumed".into())),
        };
        match rx.recv() {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::TaskPanicked(
                "task channel closed unexpectedly".into(),
            )),
        }
    }
}

/// Drop guard that decrements task_count and notifies condvar.
/// Ensures structured concurrency even if the task panics.
/// Used by both sync (`spawn`) and async (`spawn_async`) tasks.
struct TaskGuard {
    rt: Arc<RuntimeInner>,
}

impl Drop for TaskGuard {
    fn drop(&mut self) {
        let prev = self.rt.task_count.fetch_sub(1, Ordering::AcqRel);
        if prev == 1 {
            self.rt.task_done.notify_all();
        }
    }
}

/// Spawn a closure on Tokio's blocking thread pool.
///
/// The task is tracked for structured concurrency — `runtime::run` or
/// `http::serve` will wait for it before returning. The runtime context
/// is auto-initialized if needed.
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let rt = runtime::current_runtime();
    rt.task_count.fetch_add(1, Ordering::AcqRel);

    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<T, RuntimeError>>(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_child = Arc::clone(&cancel);
    let rt_child = Arc::clone(&rt);

    // Create a cancel channel for instant cancellation of blocking channel ops.
    let (cancel_tx, cancel_rx) = crossbeam_channel::bounded::<()>(1);

    rt.spawner.spawn_task(Box::new(move || {
        runtime::install_runtime(Arc::clone(&rt_child));
        runtime::install_cancel_flag(cancel_child);
        runtime::install_cancel_channel(cancel_rx);
        let _guard = TaskGuard { rt: rt_child };

        let result = std::panic::catch_unwind(AssertUnwindSafe(f));
        let mapped = match result {
            Ok(val) => Ok(val),
            Err(payload) => Err(panic_to_error(payload)),
        };
        let _ = tx.send(mapped);
    }));

    JoinHandle {
        rx: Some(rx),
        cancel,
        cancel_tx: Some(cancel_tx),
    }
}

impl<T> std::fmt::Debug for AsyncJoinHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncJoinHandle").finish_non_exhaustive()
    }
}

/// Handle to a spawned async task. Use `.await` to retrieve the result.
pub struct AsyncJoinHandle<T> {
    rx: tokio::sync::oneshot::Receiver<Result<T, RuntimeError>>,
    cancel: Arc<tokio::sync::Notify>,
}

impl<T> AsyncJoinHandle<T> {
    /// Request cancellation. The spawned future is dropped and `.await`
    /// returns `Err(Cancelled)`.
    pub fn cancel(&self) {
        self.cancel.notify_one();
    }
}

/// Future returned by `AsyncJoinHandle::into_future()`.
///
/// Consuming the handle via `.await` (i.e. `IntoFuture`) drops the cancel
/// handle. Cancellation is no longer possible after conversion — call
/// `cancel()` before awaiting if you need cooperative cancellation.
pub struct AsyncJoinFuture<T> {
    rx: tokio::sync::oneshot::Receiver<Result<T, RuntimeError>>,
}

impl<T> Future for AsyncJoinFuture<T> {
    type Output = Result<T, RuntimeError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.rx).poll(cx) {
            Poll::Ready(Ok(result)) => Poll::Ready(result),
            Poll::Ready(Err(_)) => Poll::Ready(Err(RuntimeError::TaskPanicked(
                "task channel closed".into(),
            ))),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<T> IntoFuture for AsyncJoinHandle<T> {
    type Output = Result<T, RuntimeError>;
    type IntoFuture = AsyncJoinFuture<T>;

    fn into_future(self) -> Self::IntoFuture {
        AsyncJoinFuture { rx: self.rx }
    }
}

/// Spawn an async future on the Tokio runtime.
///
/// The task participates in structured concurrency — the runtime waits
/// for it before returning. Cancel via the returned handle.
pub fn spawn_async<F, T>(future: F) -> AsyncJoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let rt = runtime::current_runtime();
    rt.task_count.fetch_add(1, Ordering::AcqRel);

    let (tx, rx) = tokio::sync::oneshot::channel();
    let cancel = Arc::new(tokio::sync::Notify::new());
    let cancel_inner = Arc::clone(&cancel);
    let rt_inner = Arc::clone(&rt);

    tokio::spawn(run_async_task(future, tx, cancel_inner, rt_inner));

    AsyncJoinHandle { rx, cancel }
}

/// Race two futures. Returns the output of whichever completes first;
/// the other is dropped (cancelled). Zero-allocation — no spawn needed.
///
/// If both futures are ready simultaneously, `a` wins (deterministic).
pub async fn race<A, B, T>(a: A, b: B) -> T
where
    A: Future<Output = T>,
    B: Future<Output = T>,
{
    tokio::select! {
        biased;
        result = a => result,
        result = b => result,
    }
}

/// Race N futures. Returns the output of whichever completes first;
/// the rest are dropped. Returns an error if the vec is empty.
pub async fn race_all<F, T>(futures: Vec<F>) -> Result<T, RuntimeError>
where
    F: Future<Output = T> + Send,
{
    match futures.is_empty() {
        true => Err(RuntimeError::InvalidArgument(
            "race_all called with empty futures list".into(),
        )),
        false => {
            let pinned: Vec<Pin<Box<F>>> = futures.into_iter().map(Box::pin).collect();
            let (result, _, _) = futures_util::future::select_all(pinned).await;
            Ok(result)
        }
    }
}

async fn run_async_task<F, T>(
    future: F,
    tx: tokio::sync::oneshot::Sender<Result<T, RuntimeError>>,
    cancel: Arc<tokio::sync::Notify>,
    rt: Arc<RuntimeInner>,
) where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    runtime::install_runtime(Arc::clone(&rt));
    let _guard = TaskGuard { rt };
    let result = tokio::select! {
        biased;
        () = cancel.notified() => Err(RuntimeError::Cancelled),
        outcome = AssertUnwindSafe(future).catch_unwind() => match outcome {
            Ok(val) => Ok(val),
            Err(payload) => Err(panic_to_error(payload)),
        },
    };
    let _ = tx.send(result);
}
