//! The owned worker one resource callback runs on, and the two bounded waits a
//! coordinator observes it through.

use crate::lifecycle::{ResourceFailureKind, ResourcePhase};
use crate::resource::entry::{CallbackClaim, ResourceEntry};
use crate::runtime_state::RuntimeInner;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// What one callback did. `None` is success — the shape every caller of this
/// module folds straight into a published health entry.
pub(crate) type CallbackOutcome = Option<ResourceFailureKind>;

/// The context a startup or periodic worker runs its callback under.
///
/// Both halves are NAMED rather than resolved on the worker thread: a freshly
/// spawned thread inherits neither, and a callback that bridged async work
/// would otherwise see a Camber runtime on some passes and not on others.
#[derive(Clone)]
pub(crate) struct WorkerContext {
    runtime: Arc<RuntimeInner>,
    handle: tokio::runtime::Handle,
}

impl WorkerContext {
    /// Capture the context this runtime's health workers run under, or report
    /// the absence a runtime with no executor already reports.
    pub(crate) fn capture(runtime: &Arc<RuntimeInner>) -> Option<Self> {
        Some(Self {
            runtime: Arc::clone(runtime),
            handle: runtime.executor().ok()?.clone(),
        })
    }
}

/// Run one callback on its own worker, waiting no longer than `limit`.
///
/// The async form, for the readiness pass and the periodic coordinator. The
/// wait is the only thing `limit` bounds: on expiry the receiver is dropped and
/// the worker keeps the resource until its callback returns.
pub(crate) async fn run_callback(
    entry: &Arc<ResourceEntry>,
    phase: ResourcePhase,
    limit: Duration,
    context: Option<WorkerContext>,
    runtime: &RuntimeInner,
) -> CallbackOutcome {
    let claim = match entry.try_claim() {
        Some(claim) => claim,
        None => return Some(ResourceFailureKind::BlockedByActiveCallback),
    };
    let (result, delivery) = tokio::sync::oneshot::channel();
    if !admit_worker(claim, phase, context, runtime, move |outcome| {
        drop(result.send(outcome));
    }) {
        return Some(ResourceFailureKind::LostWorker);
    }
    match tokio::time::timeout(limit, delivery).await {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(_)) => Some(ResourceFailureKind::LostWorker),
        Err(_) => Some(ResourceFailureKind::DeadlineExceeded),
    }
}

/// Run one callback on its own worker from a thread with no runtime entered,
/// spending no more than `limit` on entering the resource and running it.
///
/// Teardown's form. It waits for the resource's callback slot as well as for
/// the callback, because a probe abandoned moments earlier is still finishing
/// and reporting that resource as blocked would name a hazard that resolved
/// itself. Both waits come out of one budget, so the phase limit bounds the
/// whole disposition of one resource rather than each half of it.
pub(crate) fn run_callback_blocking(
    entry: &Arc<ResourceEntry>,
    phase: ResourcePhase,
    limit: Duration,
    runtime: &RuntimeInner,
) -> CallbackOutcome {
    let started = Instant::now();
    let claim = match entry.claim_within(limit) {
        Some(claim) => claim,
        None => return Some(ResourceFailureKind::BlockedByActiveCallback),
    };
    let (result, delivery) = std::sync::mpsc::sync_channel(1);
    if !admit_worker(claim, phase, None, runtime, move |outcome| {
        drop(result.send(outcome));
    }) {
        return Some(ResourceFailureKind::LostWorker);
    }
    match delivery.recv_timeout(limit.saturating_sub(started.elapsed())) {
        Ok(outcome) => outcome,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            Some(ResourceFailureKind::DeadlineExceeded)
        }
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Some(ResourceFailureKind::LostWorker)
        }
    }
}

/// Admit one owned worker for a claimed callback, reporting whether it started.
///
/// The claim is taken by value, so a refused admission releases the resource on
/// the way out rather than leaving it claimed by a callback that never ran. The
/// join handle is dropped on purpose: a worker may outlive the wait, and the
/// coordinator's channel is what carries the result back.
fn admit_worker<D>(
    claim: CallbackClaim,
    phase: ResourcePhase,
    context: Option<WorkerContext>,
    runtime: &RuntimeInner,
    deliver: D,
) -> bool
where
    D: FnOnce(CallbackOutcome) + Send + 'static,
{
    if runtime.resource_worker_refused(claim.entry().name()) {
        return false;
    }
    let name = format!("camber-resource-{phase}");
    match std::thread::Builder::new()
        .name(name)
        .spawn(move || run_worker(claim, phase, context, deliver))
    {
        Ok(_worker) => true,
        Err(error) => {
            // The typed outcome the caller reports is the same either way, so
            // this log is the only place the operating system's own account of
            // the refusal survives.
            tracing::error!(%phase, %error, "no worker was available for a resource callback");
            false
        }
    }
}

/// Invoke one callback under its context, release the resource, and deliver.
///
/// The claim is released BEFORE the result is delivered: the callback has
/// returned by then, so a coordinator that reads the outcome and immediately
/// visits the same resource must not find its own completed callback in the
/// way.
fn run_worker<D>(
    claim: CallbackClaim,
    phase: ResourcePhase,
    context: Option<WorkerContext>,
    deliver: D,
) where
    D: FnOnce(CallbackOutcome),
{
    let outcome = with_context(context, || invoke(claim.entry(), phase));
    drop(claim);
    deliver(outcome);
}

/// Run `callback` with the named runtime context and Tokio handle installed,
/// or with neither when the phase carries none.
///
/// Absence stays absence: a teardown worker runs with no runtime entered,
/// rather than being handed a minted one no owner awaits.
fn with_context<T>(context: Option<WorkerContext>, callback: impl FnOnce() -> T) -> T {
    match context {
        Some(context) => {
            let _camber = crate::runtime_state::install_runtime(context.runtime);
            let _tokio = context.handle.enter();
            callback()
        }
        None => callback(),
    }
}

/// Call the callback this phase names, mapping every way it can end to the
/// closed vocabulary a coordinator publishes.
fn invoke(entry: &ResourceEntry, phase: ResourcePhase) -> CallbackOutcome {
    let called = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match phase {
        ResourcePhase::StartupHealth | ResourcePhase::PeriodicHealth => {
            entry.resource().health_check()
        }
        ResourcePhase::Shutdown => entry.resource().shutdown(),
    }));
    match called {
        Ok(Ok(())) => None,
        Ok(Err(cause)) => Some(ResourceFailureKind::Returned(Arc::new(cause))),
        Err(payload) => Some(ResourceFailureKind::Panicked(
            crate::task::panic_message(payload.as_ref())
                .unwrap_or("unknown panic")
                .into(),
        )),
    }
}
