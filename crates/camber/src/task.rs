use crate::RuntimeError;
use crate::runtime_state::{self as runtime, LifecycleSignals, RuntimeInner, ScopeSlot};
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
/// With no runtime established the observer never completes: nothing exists
/// that could request shutdown.
pub async fn on_shutdown() {
    match runtime::try_current_runtime() {
        Some(runtime) => observe_shutdown(runtime).await,
        // Nothing exists that could request shutdown, so the observer parks
        // rather than allocating a latch no owner could ever fire.
        None => std::future::pending().await,
    }
}

/// Await the runtime's shutdown signal, pausing at the seam's registration
/// checkpoint on each registration.
async fn observe_shutdown(runtime: Arc<RuntimeInner>) {
    runtime
        .shutdown_signal()
        .wait_observed(|| {
            runtime.pause_test_schedule(
                crate::runtime_test_support::RuntimeCheckpoint::ShutdownWaitRegistered,
            );
            std::future::ready(())
        })
        .await;
}

impl<T> std::fmt::Debug for JoinHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinHandle")
            .field("refused", &matches!(self.source, JoinSource::Refused(_)))
            .finish()
    }
}

/// Where a blocking task handle gets its result: a live task channel with the
/// cancellation state that task observes, or an already-terminal outcome for a
/// spawn the scope refused.
///
/// A refused spawn has no task to cancel, so it carries no cancellation state
/// at all — the variant is what says so.
enum JoinSource<T> {
    Task {
        rx: std::sync::mpsc::Receiver<Result<T, RuntimeError>>,
        cancel: Arc<AtomicBool>,
        cancel_tx: crossbeam_channel::Sender<()>,
    },
    Refused(RuntimeError),
}

/// Handle to a spawned task. Retrieve the result with `join()`.
pub struct JoinHandle<T> {
    source: JoinSource<T>,
}

/// The result a task whose channel closed delivers.
///
/// A task that was aborted or forcibly stopped drops its sender without ever
/// sending, and every delivery site reports that one condition the same way.
/// The wording is the documented forced-abort contract, so it is written once
/// rather than spelled out per site.
fn channel_closed() -> RuntimeError {
    RuntimeError::TaskPanicked("task channel closed".into())
}

/// Record a task result that had nowhere left to go.
///
/// A caller that releases its handle drops the receiver with it, so the send
/// fails and the value is discarded. Debug level: releasing a handle is
/// allowed, and this line exists only to explain a result that vanished. The
/// blocking and async deliveries differ in nothing but the flavor they name, so
/// the record is written once — generic over the send error, which each sender
/// type words differently and neither report reads.
fn report_dropped_result<E>(sent: Result<(), E>, flavor: &'static str) {
    match sent {
        Ok(()) => {}
        Err(_) => tracing::debug!(flavor, "task result dropped: handle was released"),
    }
}

/// Deliver a blocking task's result, mapping a dropped sender to a panic.
fn recv_task_result<T>(
    rx: std::sync::mpsc::Receiver<Result<T, RuntimeError>>,
) -> Result<T, RuntimeError> {
    match rx.recv() {
        Ok(result) => result,
        Err(_) => Err(channel_closed()),
    }
}

/// Run a synchronous body, mapping an unwind to `TaskPanicked`.
///
/// Every blocking entry point catches here — the handle-carrying spawn, the
/// handle-less per-response producer, and the detached WebSocket callback — so
/// "a panic becomes `TaskPanicked`" has one definition on the blocking side and
/// only the destination of that error differs. `catch_panic_async` is the same
/// definition for a future.
pub(crate) fn catch_panic<F, T>(f: F) -> Result<T, RuntimeError>
where
    F: FnOnce() -> T,
{
    std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(panic_to_error)
}

/// Run a future to completion, mapping an unwind to `TaskPanicked`.
///
/// The async sibling of `catch_panic`. A user-owned `spawn_async` body, a
/// Camber-owned scope child, and the owned server's supervisor all unwind the
/// same way and all must report it as the same error, so the
/// `AssertUnwindSafe`/`catch_unwind`/`panic_to_error` sequence has one
/// definition here instead of one per await site.
pub(crate) async fn catch_panic_async<F>(f: F) -> Result<F::Output, RuntimeError>
where
    F: Future,
{
    AssertUnwindSafe(f)
        .catch_unwind()
        .await
        .map_err(panic_to_error)
}

/// The message an unwind carried, when it carried one std panics can name.
///
/// `panic!` produces a `&'static str` for a literal and a `String` for a
/// formatted message; anything else is a payload only its own thrower can read,
/// and there is nothing safe to say about it.
///
/// Borrowed rather than owned, so a caller that only wants to record the text
/// pays nothing for it. `panic_to_error` is the one caller that needs an owned
/// copy, and it makes that copy itself.
pub(crate) fn panic_message(payload: &(dyn std::any::Any + Send)) -> Option<&str> {
    match payload.downcast_ref::<&'static str>() {
        Some(text) => Some(text),
        None => payload.downcast_ref::<String>().map(String::as_str),
    }
}

/// The message this unwind is reported to a joining caller under.
pub(crate) fn panic_to_error(payload: Box<dyn std::any::Any + Send>) -> RuntimeError {
    let message = panic_message(payload.as_ref()).unwrap_or("unknown panic");
    RuntimeError::TaskPanicked(message.into())
}

impl<T> JoinHandle<T> {
    /// Request cancellation. The task will observe `Cancelled` at the next
    /// Camber IO boundary (channel recv, http get/post, net read_request).
    /// Cancelling a completed task is a no-op.
    pub fn cancel(&self) {
        match &self.source {
            JoinSource::Task {
                cancel, cancel_tx, ..
            } => {
                cancel.store(true, Ordering::Release);
                // Signal the cancel channel to unblock any select!-based waits.
                let _ = cancel_tx.try_send(());
            }
            JoinSource::Refused(_) => {}
        }
    }

    /// Wait for the task to complete and return its result.
    ///
    /// Returns `Err(TaskPanicked)` if the task panicked, `Err(ScopeClosed)` if
    /// the root scope refused the spawn, or `Err(NoRuntime)` if no runtime
    /// context was established when `spawn` was called — a refusal carries
    /// which of the two it was, so a caller can tell "too late" from "no
    /// runtime at all".
    pub fn join(self) -> Result<T, RuntimeError> {
        match self.source {
            JoinSource::Refused(error) => Err(error),
            JoinSource::Task { rx, .. } => recv_task_result(rx),
        }
    }

    /// A handle for a spawn the scope refused: no task ran, and the caller
    /// observes the refusal through the handle's existing result channel.
    fn refused(error: RuntimeError) -> Self {
        Self {
            source: JoinSource::Refused(error),
        }
    }
}

/// Spawn a closure on Tokio's blocking thread pool.
///
/// The task is admitted to the runtime's root scope, and the runtime entry
/// point that owns that scope waits for it before returning: `runtime::run`,
/// `RuntimeBuilder::run`, or `#[camber::test]`. Once the scope closes, the
/// spawn is refused and the handle yields `RuntimeError::ScopeClosed`; with no
/// runtime context established it yields `RuntimeError::NoRuntime`.
pub fn spawn<F, T>(f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    match admit_blocking_child() {
        Ok((rt, slot)) => spawn_admitted(f, rt, slot),
        Err(error) => JoinHandle::refused(error),
    }
}

/// Claim one blocking slot in the current runtime's root scope, or report why
/// the spawn is refused: no runtime context, or admission already closed.
fn admit_blocking_child() -> Result<(Arc<RuntimeInner>, ScopeSlot), RuntimeError> {
    let rt = runtime::runtime_context()?;
    let slot = rt.admit_blocking()?;
    Ok((rt, slot))
}

fn spawn_admitted<F, T>(f: F, rt: Arc<RuntimeInner>, slot: ScopeSlot) -> JoinHandle<T>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    let (tx, rx) = std::sync::mpsc::sync_channel::<Result<T, RuntimeError>>(1);
    let cancel = Arc::new(AtomicBool::new(false));
    let cancel_child = Arc::clone(&cancel);

    // Create a cancel channel for instant cancellation of blocking channel ops.
    let (cancel_tx, cancel_rx) = crossbeam_channel::bounded::<()>(1);

    let launched = launch_on_admitting_executor(rt, slot, move || {
        deliver_task_result(f, tx, cancel_child, cancel_rx)
    });
    match launched {
        Ok(()) => JoinHandle {
            source: JoinSource::Task {
                rx,
                cancel,
                cancel_tx,
            },
        },
        // The body never ran, so its sender is dropped with it and the result
        // channel could only ever report a closed channel. The refusal names
        // the actual cause instead.
        Err(error) => JoinHandle::refused(error),
    }
}

/// Run one admitted body on the blocking pool of the runtime that admitted it,
/// under that runtime's context.
///
/// The ambient pool is never asked for — `RuntimeInner::executor` states why,
/// and every launch site resolves through it. A runtime that admitted a child
/// and then has no executor to run it on reports `NoRuntime`, which each caller
/// disposes of the way its own ownership requires — the handle-carrying spawn
/// returns it, the handle-less producer logs it. Either way the slot travels
/// with the body, so the claim admission took is released when the body returns
/// and when there is nowhere to run it alike.
fn launch_on_admitting_executor<F>(
    rt: Arc<RuntimeInner>,
    slot: ScopeSlot,
    body: F,
) -> Result<(), RuntimeError>
where
    F: FnOnce() + Send + 'static,
{
    let executor = rt.executor()?.clone();
    drop(executor.spawn_blocking(move || run_in_spawner_context(rt, slot, body)));
    Ok(())
}

/// Run one admitted body on a blocking worker under its spawner's runtime
/// context, releasing the scope claim when the body returns.
///
/// A blocking worker starts with no context of its own, so the runtime `Arc`
/// the spawner already held is re-installed here and restored on return. It
/// constructs nothing, so no path can fill runtime absence with a minted
/// orphan. The other site that writes the `RUNTIME` thread-local outside the
/// runtime-establishing entry points is the direct WebSocket callback, which
/// re-installs the runtime its own serving connection captured on exactly the
/// same terms.
fn run_in_spawner_context<F>(rt: Arc<RuntimeInner>, slot: ScopeSlot, body: F)
where
    F: FnOnce(),
{
    let runtime_guard = runtime::install_runtime(rt);
    body();
    drop(slot);
    drop(runtime_guard);
}

/// Run a user closure under its cancellation context and deliver the outcome,
/// mapping a panic to `TaskPanicked` for the handle's holder.
fn deliver_task_result<F, T>(
    f: F,
    tx: std::sync::mpsc::SyncSender<Result<T, RuntimeError>>,
    cancel: Arc<AtomicBool>,
    cancel_rx: crossbeam_channel::Receiver<()>,
) where
    F: FnOnce() -> T,
{
    let cancel_guard = runtime::install_cancel_context(cancel, cancel_rx);
    let mapped = catch_panic(f);
    report_dropped_result(tx.send(mapped), "blocking");
    drop(cancel_guard);
}

impl<T> std::fmt::Debug for AsyncJoinHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncJoinHandle")
            .field(
                "refused",
                &matches!(self.source, AsyncJoinSource::Refused(_)),
            )
            .finish()
    }
}

/// Where an async task handle gets its result: a live task channel, or an
/// already-terminal outcome for a spawn the scope refused.
enum AsyncJoinSource<T> {
    Task(tokio::sync::oneshot::Receiver<Result<T, RuntimeError>>),
    /// Delivered exactly once; `None` after the future has yielded it.
    ///
    /// `None` is also the spent state of a task that already delivered. Polling
    /// a future after it returned `Ready` is a caller mistake, and this type
    /// answers it by parking — the same answer for both arms. Leaving a
    /// delivered task in place would answer it instead with a panic raised
    /// inside tokio's oneshot receiver, worded for tokio and naming nothing in
    /// Camber.
    Refused(Option<RuntimeError>),
}

/// Handle to a spawned async task. Use `.await` to retrieve the result.
///
/// `IntoFuture` moves the source out and drops the cancel half, so the two are
/// separate fields here. A refused spawn has no future to cancel and so
/// carries no cancel half at all.
pub struct AsyncJoinHandle<T> {
    source: AsyncJoinSource<T>,
    cancel: Option<Arc<tokio::sync::Notify>>,
}

impl<T> AsyncJoinHandle<T> {
    /// Request cancellation. The spawned future is dropped and `.await`
    /// returns `Err(Cancelled)`.
    pub fn cancel(&self) {
        if let Some(cancel) = &self.cancel {
            cancel.notify_one();
        }
    }

    /// A handle for a spawn the scope refused: no future ran, and the caller
    /// observes the refusal through the handle's existing result channel.
    fn refused(error: RuntimeError) -> Self {
        Self {
            source: AsyncJoinSource::Refused(Some(error)),
            cancel: None,
        }
    }
}

/// Future returned by `AsyncJoinHandle::into_future()`.
///
/// Consuming the handle via `.await` (i.e. `IntoFuture`) drops the cancel
/// handle. Cancellation is no longer possible after conversion — call
/// `cancel()` before awaiting if you need cooperative cancellation.
pub struct AsyncJoinFuture<T> {
    source: AsyncJoinSource<T>,
}

impl<T> Future for AsyncJoinFuture<T> {
    type Output = Result<T, RuntimeError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Borrow-split first: the task arm has to write back the field it just
        // polled, and a local `&mut` whose reborrow ends with the poll is what
        // lets it do that without a second nesting layer.
        let source = &mut self.get_mut().source;
        match source {
            AsyncJoinSource::Task(rx) => {
                let outcome = poll_task_result(Pin::new(rx), cx);
                spend_delivered_task(source, outcome)
            }
            AsyncJoinSource::Refused(error) => take_refusal(error.take()),
        }
    }
}

/// Retire a task source that has delivered, so a second poll lands on the
/// already-total refused arm.
///
/// `tokio::sync::oneshot::Receiver` panics with "called after complete" once it
/// is spent. That would give one public future two opposite policies for the
/// same caller mistake — the refused arm parks, the task arm aborts the process
/// from inside a dependency. Replacing the delivered source makes both park.
fn spend_delivered_task<T>(
    source: &mut AsyncJoinSource<T>,
    outcome: Poll<Result<T, RuntimeError>>,
) -> Poll<Result<T, RuntimeError>> {
    match outcome {
        Poll::Ready(result) => {
            *source = AsyncJoinSource::Refused(None);
            Poll::Ready(result)
        }
        Poll::Pending => Poll::Pending,
    }
}

/// Map the task channel's outcome, treating a dropped sender — an aborted or
/// forcibly stopped task — as a panic.
fn poll_task_result<T>(
    rx: Pin<&mut tokio::sync::oneshot::Receiver<Result<T, RuntimeError>>>,
    cx: &mut Context<'_>,
) -> Poll<Result<T, RuntimeError>> {
    match rx.poll(cx) {
        Poll::Ready(Ok(result)) => Poll::Ready(result),
        Poll::Ready(Err(_)) => Poll::Ready(Err(channel_closed())),
        Poll::Pending => Poll::Pending,
    }
}

/// Yield a refusal once; a second poll has nothing left to deliver.
///
/// Parking is the honest answer there. Polling a future after it returned
/// `Ready` is a caller contract violation, not a task outcome, and reporting it
/// as `TaskPanicked` would name a panic that never happened — in logs and in
/// any caller matching on that variant.
fn take_refusal<T>(error: Option<RuntimeError>) -> Poll<Result<T, RuntimeError>> {
    match error {
        Some(error) => Poll::Ready(Err(error)),
        None => Poll::Pending,
    }
}

impl<T> AsyncJoinFuture<T> {
    pub(crate) fn closed() -> Self {
        Self {
            source: AsyncJoinSource::Refused(Some(channel_closed())),
        }
    }
}

impl<T> IntoFuture for AsyncJoinHandle<T> {
    type Output = Result<T, RuntimeError>;
    type IntoFuture = AsyncJoinFuture<T>;

    fn into_future(self) -> Self::IntoFuture {
        AsyncJoinFuture {
            source: self.source,
        }
    }
}

/// Spawn an async future on the Tokio runtime.
///
/// The task participates in structured concurrency — the runtime waits
/// for it before returning. Cancel via the returned handle. Once the scope
/// closes the spawn is refused with `RuntimeError::ScopeClosed`; with no
/// runtime context established it yields `RuntimeError::NoRuntime`.
pub fn spawn_async<F, T>(future: F) -> AsyncJoinHandle<T>
where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    // Context first, the way `spawn` resolves admission before it allocates:
    // with no runtime there is nothing to admit to, and refusing here spends
    // neither the result channel, the notifier, nor the body future on a
    // caller who cannot use them. The `ScopeClosed` refusal below cannot move
    // up with it — admission is the atomic step inside `admit_async`, and that
    // step takes the body.
    let rt = match runtime::runtime_context() {
        Ok(rt) => rt,
        Err(error) => return AsyncJoinHandle::refused(error),
    };

    let (tx, rx) = tokio::sync::oneshot::channel();
    let cancel = Arc::new(tokio::sync::Notify::new());
    let body = run_async_task(future, tx, Arc::clone(&cancel));

    // A refused admission drops `body` unpolled, so the caller's future never
    // runs and the refusal arrives on the handle. The caller holds that handle,
    // so this child's panic is delivered there and leaves the runtime result
    // untouched.
    match rt.admit_async(body) {
        Ok(()) => AsyncJoinHandle {
            source: AsyncJoinSource::Task(rx),
            cancel: Some(cancel),
        },
        Err(error) => AsyncJoinHandle::refused(error),
    }
}

/// Admit one Camber-owned perpetual loop to a NAMED runtime's root scope,
/// built from that same runtime's lifecycle signals.
///
/// Every Camber-owned background loop enters the scope through here, so runtime
/// teardown owns its completion instead of reaping it at the final Tokio
/// window. No user handle exists to carry a panic, so the scope records it and
/// `run` reports it.
///
/// This is the form that ENFORCES the invariant the ambient form below only
/// states: the signals the loop stops on cannot name a different runtime from
/// the scope that owns its completion, because one `&Arc` supplies both. Every
/// named subsystem reaches it through `admit_signalled_subsystem_on`; only a
/// caller with no `Arc` in hand resolves one from the ambient context.
pub(crate) fn admit_signalled_on<B, Fut>(
    runtime: &Arc<RuntimeInner>,
    build: B,
) -> Result<(), RuntimeError>
where
    B: FnOnce(LifecycleSignals) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    let signals = LifecycleSignals::from_runtime(runtime);
    runtime.admit_internal_async(build(signals))
}

/// Admit one Camber-owned perpetual loop to the ambient runtime's root scope,
/// built from that runtime's lifecycle signals.
///
/// The runtime is resolved once and both halves come from that one lookup, so
/// the signals the loop stops on cannot name a different runtime from the
/// scope that owns its completion. Absence is refused before `build` runs, so
/// no loop is constructed for a runtime that does not exist.
pub(crate) fn admit_signalled_loop<B, Fut>(build: B) -> Result<(), RuntimeError>
where
    B: FnOnce(LifecycleSignals) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    admit_signalled_on(&runtime::runtime_context()?, build)
}

/// Admit one Camber-owned perpetual loop as a named background subsystem of a
/// runtime the caller already holds.
///
/// The runtime's own setup holds the `Arc` it just established, so it names it
/// instead of re-reading a task-local that resolves to the same runtime only by
/// coincidence. Every subsystem that setup owns — the signal watcher, ACME and
/// DNS-01 renewal, and each per-resource health loop — is admitted through
/// here.
///
/// The doc-hidden test seams enter here too. A seam drives a runtime it did not
/// build, but it can still NAME one: it resolves the ambient context itself and
/// hands the `Arc` over, so the subsystems that most need the invariant proven
/// are admitted through the form that proves it rather than around it.
pub(crate) fn admit_signalled_subsystem_on<B, Fut>(
    runtime: &Arc<RuntimeInner>,
    subsystem: &str,
    build: B,
) -> Result<(), RuntimeError>
where
    B: FnOnce(LifecycleSignals) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    report_subsystem_outcome(subsystem, admit_signalled_on(runtime, build))
}

/// Log a subsystem admission that was refused, and hand the outcome back.
///
/// The runtime's own setup has no result to propagate a refusal through — it
/// runs inside the block that yields the user closure's value — so it discards
/// the outcome and relies on this log; the test seam returns it to the test.
/// Both need the same record, so the record is written once.
fn report_subsystem_outcome(
    subsystem: &str,
    outcome: Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    if let Err(error) = outcome.as_ref() {
        record_subsystem_refusal(subsystem, error);
    }
    outcome
}

/// Record a Camber-owned background subsystem the root scope refused.
///
/// Error level: the subsystem's loop is now unowned and nothing will run it,
/// and the name is the only thing that says which one was lost.
fn record_subsystem_refusal(subsystem: &str, error: &RuntimeError) {
    tracing::error!(
        subsystem,
        %error,
        "root scope refused a Camber-owned background subsystem"
    );
}

/// Run one Camber-internal blocking producer, admitted to the root scope when
/// a runtime context is established, and detached on Tokio's blocking pool
/// otherwise — the synchronous-entry path carries no context by contract, and
/// absence is dispositioned here rather than filled with a minted runtime.
///
/// A refusal is reported against the response that lost its producer: the
/// response was already produced, so the refusal has no caller left to return
/// to. Dropping the refused handle drops the unrun closure with it, which is
/// what closes the producer's channel and lets the body end its own stream.
pub(crate) fn spawn_internal_blocking<F>(producer: &'static str, path: &str, f: F)
where
    F: FnOnce() + Send + 'static,
{
    // Admitted directly rather than through `spawn`: this producer returns
    // nothing and no one holds its handle, so `spawn`'s result channel, cancel
    // channel and cancel context would be allocated per response for a reader
    // that never exists — and the dropped result channel made every streaming
    // response log a released-handle debug line. The scope slot is the only
    // part the runtime needs, and it is released when the closure returns,
    // panic included.
    match admit_blocking_child() {
        Ok((rt, slot)) => spawn_admitted_producer(producer, path, rt, slot, f),
        Err(RuntimeError::NoRuntime) => detach_producer(producer, path, f),
        Err(error) => record_producer_refusal(producer, path, &error),
    }
}

/// Run one unadmitted producer detached on the ambient Tokio pool, or report
/// that there is nowhere left to run it.
///
/// The synchronous-entry connection path carries no Camber runtime context by
/// contract, so there is no scope to admit into and no refusal to report — the
/// producer inherits that connection's detached lifetime. It still runs through
/// `run_producer`: a panic here reaches no handle, so the structured record is
/// the only report there is.
///
/// The ambient pool is NAMED rather than reached through a free
/// `spawn_blocking`, for the reason `launch_on_admitting_executor` states: the
/// free form panics when no Tokio runtime is entered, and this is the path with
/// the weakest guarantee that one is — no Camber runtime exists here at all. An
/// absent ambient runtime is the same loss a refusal is, so it is reported the
/// same way, against the response that lost its producer. Dropping `f` closes
/// the producer's channel, which is what lets the body end its own stream.
fn detach_producer<F>(producer: &'static str, path: &str, f: F)
where
    F: FnOnce() + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(executor) => drop(executor.spawn_blocking(move || run_producer(producer, f))),
        Err(_) => record_producer_refusal(producer, path, &RuntimeError::NoRuntime),
    }
}

/// Run one admitted producer on the blocking pool of the runtime that admitted
/// it.
///
/// A runtime that admits but has no executor leaves this producer with nowhere
/// to run, which is the same loss a refusal is — so it is reported the same
/// way, against the response that lost its producer.
fn spawn_admitted_producer<F>(
    producer: &'static str,
    path: &str,
    rt: Arc<RuntimeInner>,
    slot: ScopeSlot,
    f: F,
) where
    F: FnOnce() + Send + 'static,
{
    if let Err(error) = launch_on_admitting_executor(rt, slot, move || run_producer(producer, f)) {
        record_producer_refusal(producer, path, &error);
    }
}

/// Run a per-response producer, reporting a panic that would otherwise vanish.
///
/// Error level, unlike a refusal: no one holds this producer's handle, so an
/// uncaught panic here reaches nobody at all.
fn run_producer<F>(producer: &'static str, f: F)
where
    F: FnOnce() + Send + 'static,
{
    match catch_panic(f) {
        Ok(()) => {}
        Err(error) => tracing::error!(producer, %error, "per-response producer panicked"),
    }
}

/// Record a per-response producer the root scope refused.
///
/// Debug level, not warn: a refusal inside the shutdown window is the defined
/// disposition of a closed scope, not a fault. Naming the producer and the
/// request it belonged to is what keeps it from being silently dropped.
fn record_producer_refusal(producer: &'static str, path: &str, error: &RuntimeError) {
    tracing::debug!(
        producer,
        path,
        %error,
        "root scope refused a per-response producer"
    );
}

/// Run a blocking closure off the async poll path, on whichever runtime flavor
/// is entered — if any.
///
/// `tokio::task::block_in_place` hands the worker's other tasks to a
/// replacement thread, but it PANICS on a current-thread runtime, where there is
/// no worker core to hand off. Outside a runtime tokio runs the closure inline,
/// which is what the fallback below does anyway. A bare call in library source
/// is therefore correct only while every reachable entry point happens to build
/// a multi-thread executor: an invariant held one call away in
/// `runtime::build_executor` and asserted nowhere. Checking the flavor makes the
/// call total. Off a multi-thread worker the closure runs inline — there is no
/// other worker to hand anything to.
pub(crate) fn block_in_place<F, T>(f: F) -> T
where
    F: FnOnce() -> T,
{
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(f),
        _ => f(),
    }
}

/// Run one blocking wait under Camber's scheduler-safe policy.
///
/// The whole policy, in one place, for the Tokio-backed blocking waits the
/// crate ships. The crossbeam-backed `channel::sync` halves predate it and wait
/// without it. Off every runtime the wait belongs to the caller's own thread. On a
/// multi-thread runtime it goes through [`block_in_place`], which hands the
/// worker to a replacement so the rest of that runtime keeps running. A
/// current-thread runtime has no second thread to hand anything to, so a wait
/// there would stop the only thing that could ever end it: those callers are
/// refused before they wait rather than deadlocked by one.
///
/// Held apart from [`block_in_place`], which is total and answers "run this off
/// the poll path". This one can refuse, and refusing is the point.
pub(crate) fn wait_blocking<F, T>(wait: F) -> Result<T, RuntimeError>
where
    F: FnOnce() -> T,
{
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Err(_) => Ok(wait()),
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) => Ok(tokio::task::block_in_place(wait)),
        Ok(_) => Err(RuntimeError::BlockingInAsyncContext),
    }
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
) where
    F: Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    let result = tokio::select! {
        biased;
        () = cancel.notified() => Err(RuntimeError::Cancelled),
        outcome = catch_panic_async(future) => outcome,
    };
    report_dropped_result(tx.send(result), "async");
}
