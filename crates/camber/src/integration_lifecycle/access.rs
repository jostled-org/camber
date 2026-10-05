//! Revocable access to one admitted integration, and the operations it runs.
//!
//! Access is shared authority, not ownership. Every clone reaches the same
//! entry, and the entry decides whether new work is admitted. The last clone's
//! drop requests close; the runtime still owns the entry until it settles. An
//! escaped handle keeps only the entry's inert settlement.
//!
//! Each operation runs as a root-scope child of the runtime that admitted the
//! entry, whichever thread submits it. Its waiter owns the caller-facing
//! result. Dropping the waiter requests cancellation; the entry still owns the
//! operation until its task ends.
//!
//! An adapter that can say when its work reached the peer submits with a
//! [`SubmissionMark`]. A waiter dropped after that mark leaves the operation's
//! outcome unknown, so its account moves into history instead of being
//! released.
//!
//! Work that raises remote records submits with a cleanup register it shares
//! with its report slot. The work names each record there; a forced stop that
//! drops the work leaves the unresolved records charged rather than released.
//!
//! An adapter whose transport needs its own close attaches one close owner: a
//! root-scope child woken once closing commits with no operation running.
//! Settlement waits for its result, so the close every closer reads is the
//! transport's own.
//!
//! Each operation reports one terminal. A refusal at admission or launch is
//! counted with no duration. An admitted operation reports from its committed
//! result, timed from admission. A forced stop that drops its work reports it
//! cancelled under shutdown. Work that keeps a cleanup register reports a
//! failure settled during the runtime's stop under shutdown, and a forced drop
//! or an unwind that leaves records owed as an incomplete cleanup naming them.

use super::accounts::{OperationAccount, PublishedFailure};
#[cfg(feature = "dns01")]
use super::cleanup::CleanupRegister;
#[cfg(feature = "nats")]
use super::entry::IntegrationEntryState;
use super::entry::{IntegrationEntry, IntegrationEntryObserver, OperationEnd, OperationGuard};
#[cfg(feature = "dns01")]
use super::telemetry::NestedTerminals;
use super::telemetry::{Outcome, OwedTerminal, TerminalOperation};
#[cfg(feature = "dns01")]
use crate::runtime_state::{LatchSignal, LifecycleSignals};
use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
    RuntimeError,
};
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(feature = "nats")]
use std::time::Duration;
use tokio::sync::oneshot;

/// Revocable access to one admitted integration.
///
/// Cheap to clone. Dropping the last clone requests close.
#[derive(Clone)]
pub(crate) struct IntegrationAccess {
    token: Arc<AccessToken>,
}

/// The shared token whose drop is the last access handle's drop.
struct AccessToken {
    entry: Arc<IntegrationEntry>,
}

impl Drop for AccessToken {
    fn drop(&mut self) {
        self.entry.request_close();
    }
}

impl IntegrationAccess {
    pub(super) fn new(entry: Arc<IntegrationEntry>) -> Self {
        Self {
            token: Arc::new(AccessToken { entry }),
        }
    }

    fn entry(&self) -> &Arc<IntegrationEntry> {
        &self.token.entry
    }

    /// The runtime-local identity admission assigned.
    pub(crate) fn id(&self) -> u64 {
        self.entry().id()
    }

    /// A read-only view of the entry's committed state.
    pub(crate) fn observer(&self) -> IntegrationEntryObserver {
        self.entry().observer()
    }

    /// The entry's committed state, read once without subscribing.
    #[cfg(feature = "nats")]
    pub(crate) fn state(&self) -> IntegrationEntryState {
        self.entry().state()
    }

    /// `operation` of this instance, as its terminal names it.
    pub(crate) fn terminal(&self, operation: IntegrationOperation) -> TerminalOperation {
        let entry = self.entry();
        TerminalOperation::new(entry.kind(), operation, Some(entry.id()))
    }

    /// The terminals of this instance's nested operations, which settle
    /// under shutdown once `stopping` fired.
    #[cfg(feature = "dns01")]
    pub(crate) fn nested(&self, stopping: LifecycleSignals) -> NestedTerminals {
        let entry = self.entry();
        NestedTerminals::new(entry.kind(), entry.id(), stopping)
    }

    /// Report this instance's settlement as its `Close` terminal: an owner
    /// that holds no connection still settles once.
    #[cfg(feature = "dns01")]
    pub(crate) fn reports_close(&self) {
        self.entry().reports_close();
    }

    /// The `Connect` terminal this instance owes since its admission.
    #[cfg(any(feature = "nats", feature = "sqs"))]
    pub(crate) fn connecting(&self) -> OwedTerminal {
        self.terminal(IntegrationOperation::Connect)
            .admitted_at(self.entry().admitted())
    }

    /// Pass an adapter's local `check` through, reporting its refusal as
    /// `operation`'s terminal before admission, charged to this instance.
    ///
    /// # Errors
    ///
    /// The refusal `check` holds, naming this instance.
    #[cfg(any(feature = "nats", feature = "sqs"))]
    pub(crate) fn checked<T>(
        &self,
        operation: IntegrationOperation,
        check: Result<T, IntegrationError>,
    ) -> Result<T, RuntimeError> {
        self.terminal(operation)
            .refusing(check.map_err(|error| integration(error.with_instance(self.id()))))
    }

    /// Record that the connection established readiness: from now on its
    /// settlement reports its `Close` terminal.
    ///
    /// # Errors
    ///
    /// `Closed`, naming `Connect`, once close is committed.
    #[cfg(any(feature = "nats", feature = "sqs"))]
    pub(crate) fn connected(&self) -> Result<(), RuntimeError> {
        self.entry().connected().map_err(integration)
    }

    /// Record that the integration established readiness.
    ///
    /// # Errors
    ///
    /// `Closed` once close is committed.
    pub(crate) fn ready(&self) -> Result<(), RuntimeError> {
        self.entry().ready().map_err(integration)
    }

    /// Admit `work` as one operation of this integration.
    ///
    /// The work runs as a root-scope child of the entry's own runtime.
    ///
    /// # Errors
    ///
    /// `Closed` once close is committed or the owning runtime is gone, `Busy`
    /// when the report budget is full, and the root scope's own refusal once
    /// its admission has closed. A refusal submits nothing.
    pub(crate) fn run<T, W>(
        &self,
        operation: IntegrationOperation,
        work: W,
    ) -> Result<OperationWaiter<T>, RuntimeError>
    where
        T: Send + 'static,
        W: Future<Output = Result<T, IntegrationError>> + Send + 'static,
    {
        self.admit(operation)?.submit(work)
    }

    /// Admit one operation before its work exists.
    ///
    /// An adapter admits first and copies the caller's input only after, so a
    /// refusal never pays for a copy.
    ///
    /// # Errors
    ///
    /// `Closed` once close is committed or the owning runtime is gone, and
    /// `Busy` at the entry's operation limit or a full report budget. A
    /// refusal submits nothing and is reported as the operation's terminal.
    pub(crate) fn admit(
        &self,
        operation: IntegrationOperation,
    ) -> Result<AdmittedOperation, RuntimeError> {
        self.admit_typed(operation).map_err(integration)
    }

    /// Admit one operation before its work exists, answering a refusal as
    /// the integration failure it is.
    ///
    /// # Errors
    ///
    /// As [`Self::admit`]: `Closed` or `Busy`, reported as the operation's
    /// terminal.
    pub(crate) fn admit_typed(
        &self,
        operation: IntegrationOperation,
    ) -> Result<AdmittedOperation, IntegrationError> {
        let entry = self.entry();
        let terminal = self.terminal(operation);
        let (runtime, (account, guard)) = entry
            .runtime(operation)
            .and_then(|runtime| Ok((runtime, entry.begin_operation(operation)?)))
            .inspect_err(|error| terminal.refused_integration(error))?;
        Ok(AdmittedOperation {
            runtime,
            account,
            guard,
            owed: terminal.admitted(),
            kind: entry.kind(),
            operation,
            instance: entry.id(),
        })
    }

    /// A handle that charges failures to this entry without keeping it open.
    #[cfg(feature = "nats")]
    pub(crate) fn monitor(&self) -> IntegrationMonitor {
        IntegrationMonitor {
            entry: Arc::clone(self.entry()),
        }
    }

    /// Admit `body` as this entry's one close owner, a root-scope child of the
    /// entry's runtime.
    ///
    /// # Errors
    ///
    /// `Closed` once close is committed, an owner is attached, or the runtime
    /// is gone, and the root scope's own refusal once its admission closed.
    #[cfg(feature = "nats")]
    pub(crate) fn spawn_close_owner<F, B>(&self, body: F) -> Result<(), RuntimeError>
    where
        F: FnOnce(CloseOwner) -> B,
        B: Future<Output = ()> + Send + 'static,
    {
        let entry = self.entry();
        let runtime = entry
            .runtime(IntegrationOperation::Close)
            .map_err(integration)?;
        let woken = entry
            .attach_close_owner(IntegrationOperation::Close)
            .map_err(integration)?;
        let owner = CloseOwner {
            entry: Arc::clone(entry),
            woken: Some(woken),
            finished: false,
        };
        runtime.admit_async(None, body(owner))
    }

    /// Request close and resolve with the entry's one fixed close result.
    ///
    /// Close commits when this is called, not when the future is first
    /// polled. Every closer, concurrent or later, reads the same result.
    pub(crate) fn close(
        &self,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send + 'static + use<> {
        let entry = Arc::clone(self.entry());
        entry.request_close();
        let settled = entry.observer().settled();
        async move {
            settled.await;
            entry.close_result()
        }
    }
}

/// One admitted operation whose work is not yet submitted.
///
/// Dropping it releases the reservation and ends the operation unsubmitted,
/// reported as cancelled with nothing sent.
pub(crate) struct AdmittedOperation {
    runtime: Arc<crate::runtime_state::RuntimeInner>,
    account: OperationAccount,
    guard: OperationGuard,
    /// The terminal owed since admission.
    owed: OwedTerminal,
    kind: IntegrationKind,
    operation: IntegrationOperation,
    instance: u64,
}

impl AdmittedOperation {
    /// Submit `work` as a root-scope child of the entry's runtime.
    ///
    /// # Errors
    ///
    /// The root scope's own refusal once its admission has closed.
    pub(crate) fn submit<T, W>(self, work: W) -> Result<OperationWaiter<T>, RuntimeError>
    where
        T: Send + 'static,
        W: Future<Output = Result<T, IntegrationError>> + Send + 'static,
    {
        self.submit_with(work, None)
    }

    /// Submit `work`, which sets `mark` once it hands anything to the peer.
    ///
    /// # Errors
    ///
    /// The root scope's own refusal once its admission has closed.
    #[cfg(any(feature = "nats", feature = "sqs"))]
    pub(crate) fn submit_marked<T, W>(
        self,
        work: W,
        mark: SubmissionMark,
    ) -> Result<OperationWaiter<T>, RuntimeError>
    where
        T: Send + 'static,
        W: Future<Output = Result<T, IntegrationError>> + Send + 'static,
    {
        self.submit_with(work, Some(mark))
    }

    /// Submit the work `build` makes, which keeps running when its waiter
    /// goes.
    ///
    /// `build` receives the latch the waiter's drop fires and the operation's
    /// cleanup register. The work reads the latch as a cancellation request
    /// and settles on its own terms, so the state it owns, and any cleanup
    /// that state needs, outlives the waiter. A cancellation the work reports
    /// as `Cancelled` answers that request, and its reservation is released
    /// rather than kept as history. A forced stop that drops the work instead
    /// charges whatever the register still names.
    ///
    /// # Errors
    ///
    /// The root scope's own refusal once its admission has closed.
    #[cfg(feature = "dns01")]
    pub(crate) fn submit_retained<T, B, W>(
        self,
        build: B,
    ) -> Result<OperationWaiter<T>, RuntimeError>
    where
        T: Send + 'static,
        B: FnOnce(LatchSignal, CleanupRegister) -> W,
        W: Future<Output = Result<T, IntegrationError>> + Send + 'static,
    {
        let (delivery, receiver) = oneshot::channel();
        let (handoff, handed) = oneshot::channel();
        let waiter_gone = LatchSignal::new();
        let (
            Self {
                runtime,
                account,
                guard,
                owed,
                kind,
                operation,
                instance,
            },
            cleanup,
        ) = self.registering();
        let work = build(waiter_gone.clone(), cleanup);
        let retained = Retained {
            account,
            guard,
            waiter_gone,
            stopping: LifecycleSignals::from_runtime(&runtime),
        };
        launched(
            runtime.admit_async(None, operate_retained(work, retained, delivery, handed)),
            owed,
            handoff,
        )?;
        Ok(OperationWaiter::new(receiver, kind, operation, instance))
    }

    /// Share one fresh cleanup register among the account, the guard, and the
    /// owed terminal, so whichever outlives the work reads the records it
    /// still owes. The register is returned for the work.
    #[cfg(feature = "dns01")]
    fn registering(self) -> (Self, CleanupRegister) {
        let cleanup = CleanupRegister::new(self.kind, self.operation);
        let registered = Self {
            account: self.account.with_cleanup(cleanup.clone()),
            guard: self.guard.with_cleanup(cleanup.clone()),
            owed: self.owed.with_cleanup(cleanup.clone()),
            ..self
        };
        (registered, cleanup)
    }

    /// Run the work `build` makes on the calling task, which is its waiter.
    ///
    /// For an owner that is itself a root-scope child and reads the result it
    /// ran: the result is delivered the moment it is fixed, so an ordinary
    /// failure releases its reservation. `build` receives the operation's
    /// cleanup register. A drop of the calling task mid-work is a forced stop,
    /// and it charges whatever the register still names.
    #[cfg(feature = "dns01")]
    pub(crate) async fn run_here<T, B, W>(self, build: B) -> Result<T, IntegrationError>
    where
        B: FnOnce(CleanupRegister) -> W,
        W: Future<Output = Result<T, IntegrationError>>,
    {
        let (
            Self {
                runtime,
                account,
                guard,
                owed,
                ..
            },
            cleanup,
        ) = self.registering();
        let stopping = LifecycleSignals::from_runtime(&runtime);
        // Only a forced stop drops the calling task mid-work.
        let owed = owed_handed_off(owed);
        let finished = build(cleanup).await;
        let result = settle_work(finished, account, owed, stopping.is_fired())
            .map_err(PublishedFailure::receive);
        guard.finish();
        result
    }

    fn submit_with<T, W>(
        self,
        work: W,
        mark: Option<SubmissionMark>,
    ) -> Result<OperationWaiter<T>, RuntimeError>
    where
        T: Send + 'static,
        W: Future<Output = Result<T, IntegrationError>> + Send + 'static,
    {
        let (delivery, receiver) = oneshot::channel();
        let (handoff, owed) = oneshot::channel();
        let abandoned = Abandonment {
            mark,
            unknown: cut_short(self.kind, self.operation),
        };
        launched(
            self.runtime.admit_async(
                None,
                operate(work, self.account, self.guard, delivery, abandoned, owed),
            ),
            self.owed,
            handoff,
        )?;
        Ok(OperationWaiter::new(
            receiver,
            self.kind,
            self.operation,
            self.instance,
        ))
    }
}

/// The terminal of work its task now owns: only a forced stop can drop that
/// work unsettled.
fn owed_handed_off(owed: OwedTerminal) -> OwedTerminal {
    owed.dropped_by_stop()
}

/// Hand the owed terminal to the operation task `launch` admitted, or report
/// the launch's refusal as the operation's terminal.
///
/// The terminal crosses only once the launch is confirmed: a refused launch
/// drops the task unpolled, and its refusal is the caller's, not a stop's.
/// A task the stop already dropped drops the terminal with it.
fn launched(
    launch: Result<(), RuntimeError>,
    owed: OwedTerminal,
    handoff: oneshot::Sender<OwedTerminal>,
) -> Result<(), RuntimeError> {
    match launch {
        Ok(()) => {
            drop(handoff.send(owed_handed_off(owed)));
            Ok(())
        }
        Err(error) => {
            owed.refused(&error);
            Err(error)
        }
    }
}

/// Whether one operation's work has handed anything to its peer.
///
/// The adapter's work sets it at the last point before the transport sends.
/// Cheap to clone; every clone is the same mark.
#[derive(Clone, Default)]
pub(crate) struct SubmissionMark {
    submitted: Arc<AtomicBool>,
}

impl SubmissionMark {
    /// Record that the work reached the transport.
    #[cfg(any(feature = "nats", feature = "sqs"))]
    pub(crate) fn submit(&self) {
        self.submitted.store(true, Ordering::Release);
    }

    /// Whether the work reached the transport.
    pub(crate) fn submitted(&self) -> bool {
        self.submitted.load(Ordering::Acquire)
    }
}

/// What an operation's abandonment leaves behind.
struct Abandonment {
    /// The mark its work sets on submission, when the adapter tracks one.
    mark: Option<SubmissionMark>,
    /// The failure an abandoned submitted operation is charged with.
    unknown: IntegrationError,
}

impl Abandonment {
    /// Settle the account of work dropped with its waiter: released when
    /// nothing was submitted, retained as an unknown outcome when it was.
    /// Either way the operation was cancelled; the outcome says which.
    fn settle(self, account: OperationAccount) -> Outcome {
        match self.mark.as_ref().is_some_and(SubmissionMark::submitted) {
            true => {
                let outcome = Outcome::failed(&self.unknown);
                account.fail(self.unknown).abandon();
                outcome
            }
            false => {
                account.succeed();
                Outcome::UNSENT
            }
        }
    }
}

/// Charges failures to one entry without holding access to it.
///
/// An adapter's event consumer holds this: it can report and close, but it is
/// not a handle, so it never keeps the entry open.
#[cfg(feature = "nats")]
#[derive(Clone)]
pub(crate) struct IntegrationMonitor {
    entry: Arc<IntegrationEntry>,
}

#[cfg(feature = "nats")]
impl IntegrationMonitor {
    /// Charge `error` to the close account and commit closing.
    pub(crate) fn fail_and_close(&self, error: IntegrationError) {
        self.entry.fail_and_close(error);
    }
}

/// The one task that closes an adapter's transport.
///
/// Dropping it unfinished is a forced stop's drop: the entry settles as
/// cancelled rather than waiting for a close that will not come.
#[cfg(feature = "nats")]
pub(crate) struct CloseOwner {
    entry: Arc<IntegrationEntry>,
    woken: Option<oneshot::Receiver<()>>,
    finished: bool,
}

#[cfg(feature = "nats")]
impl CloseOwner {
    /// Resolve once closing is committed and no operation is running.
    pub(crate) async fn closing(&mut self) {
        if let Some(woken) = self.woken.take() {
            // A dropped sender means the entry is gone; closing is all that is
            // left to do either way.
            drop(woken.await);
        }
    }

    /// How long the close may wait: `local`, narrowed by what the runtime's
    /// one aggregate shutdown deadline has left.
    pub(crate) fn bound(&self, local: Duration) -> Duration {
        match self.entry.runtime(IntegrationOperation::Close) {
            Ok(runtime) => runtime.shutdown_deadline_ref().bounded(
                &crate::lifecycle::ShutdownOwner::integration(self.entry.kind(), self.entry.id()),
                local,
            ),
            Err(_) => local,
        }
    }

    /// The transport's close ended with `closed`; settlement may proceed.
    pub(crate) fn finish(mut self, closed: Result<(), IntegrationError>) {
        self.finished = true;
        self.entry.end_close_owner(Some(closed));
    }
}

#[cfg(feature = "nats")]
impl Drop for CloseOwner {
    fn drop(&mut self) {
        if !self.finished {
            self.entry.end_close_owner(None);
        }
    }
}

/// What an operation's task hands its waiter.
enum Delivery<T> {
    /// The work's own value.
    Succeeded(T),
    /// The work's failure, charged until the waiter receives it.
    Failed(PublishedFailure),
    /// The work unwound.
    Panicked(RuntimeError),
}

/// Run one admitted operation to its result, or drop it when its waiter goes.
///
/// The work is polled first, so a result it reaches in the same poll as the
/// waiter's drop is published rather than discarded. The terminal is
/// reported before the result is published, and the guard ends last, so the
/// entry settles only after the result is fixed.
async fn operate<T, W>(
    work: W,
    account: OperationAccount,
    guard: OperationGuard,
    mut delivery: oneshot::Sender<Delivery<T>>,
    abandoned: Abandonment,
    owed: oneshot::Receiver<OwedTerminal>,
) where
    W: Future<Output = Result<T, IntegrationError>>,
{
    // Sent once the launch is confirmed; the launcher always sends it.
    let Ok(owed) = owed.await else { return };
    let finished = tokio::select! {
        biased;
        finished = crate::task::catch_panic_async(work) => finished,
        () = delivery.closed() => {
            owed.settle(abandoned.settle(account));
            guard.end(OperationEnd::Abandoned);
            return;
        }
    };
    deliver(settled(finished, account, owed, false), guard, delivery);
}

/// What one retained operation's task owns beside its work.
#[cfg(feature = "dns01")]
struct Retained {
    account: OperationAccount,
    guard: OperationGuard,
    /// Fired when the waiter drops: the work's cancellation request.
    waiter_gone: LatchSignal,
    /// The signals of the runtime that admitted the operation.
    stopping: LifecycleSignals,
}

/// Run one admitted operation to its own result, firing `waiter_gone` when
/// its waiter drops instead of dropping the work.
///
/// A `Cancelled` failure the work reports after that request answers it: the
/// work stopped because it was asked to, so nothing is left to report and the
/// reservation is released. Every other result settles as it would with the
/// waiter present, and an unread failure is abandoned into history. A failure
/// settled once the runtime's stop fired is reported under shutdown.
#[cfg(feature = "dns01")]
async fn operate_retained<T, W>(
    work: W,
    retained: Retained,
    mut delivery: oneshot::Sender<Delivery<T>>,
    owed: oneshot::Receiver<OwedTerminal>,
) where
    W: Future<Output = Result<T, IntegrationError>>,
{
    let Retained {
        account,
        guard,
        waiter_gone,
        stopping,
    } = retained;
    // Sent once the launch is confirmed; the launcher always sends it.
    let Ok(owed) = owed.await else { return };
    let mut work = std::pin::pin!(crate::task::catch_panic_async(work));
    let finished = loop {
        tokio::select! {
            biased;
            finished = &mut work => break finished,
            () = delivery.closed(), if !waiter_gone.is_fired() => waiter_gone.fire(),
        }
    };
    let stopped = stopping.is_fired();
    match finished {
        Ok(Err(error)) if waiter_gone.is_fired() && answers_cancellation(&error) => {
            account.succeed();
            owed.settle_error(&error, stopped);
            guard.finish();
        }
        finished => deliver(settled(finished, account, owed, stopped), guard, delivery),
    }
}

/// Whether `error` is the work's answer to a cancellation request that left
/// no side effect of unknown outcome.
#[cfg(feature = "dns01")]
fn answers_cancellation(error: &IntegrationError) -> bool {
    matches!(
        (error.failure(), error.retryability()),
        (
            IntegrationFailure::Cancelled,
            Retryability::Safe | Retryability::Never
        )
    )
}

/// Settle `account` and `owed` by the work's own result; `stopped` says
/// whether a failure is the runtime stop's.
fn settle_work<T>(
    result: Result<T, IntegrationError>,
    account: OperationAccount,
    owed: OwedTerminal,
    stopped: bool,
) -> Result<T, PublishedFailure> {
    match result {
        Ok(value) => {
            account.succeed();
            owed.settle(Outcome::Succeeded);
            Ok(value)
        }
        Err(error) => {
            owed.settle_error(&error, stopped);
            Err(account.fail(error))
        }
    }
}

/// Settle `account` and `owed` by the work's result and name what its
/// waiter reads; `stopped` says whether a failure is the runtime stop's.
fn settled<T>(
    finished: Result<Result<T, IntegrationError>, RuntimeError>,
    account: OperationAccount,
    owed: OwedTerminal,
    stopped: bool,
) -> Delivery<T> {
    match finished {
        Ok(result) => settle_work(result, account, owed, stopped)
            .map_or_else(Delivery::Failed, Delivery::Succeeded),
        // A panic is the waiter's error, not an integration account, so the
        // reservation is released rather than retained, unless the work's
        // cleanup register still names records the unwind left behind. The
        // terminal then names those records as the account does.
        Err(panicked) => {
            account.succeed();
            owed.unwound(stopped);
            Delivery::Panicked(panicked)
        }
    }
}

/// Hand a fixed result to its waiter and end the operation.
fn deliver<T>(
    result: Delivery<T>,
    mut guard: OperationGuard,
    delivery: oneshot::Sender<Delivery<T>>,
) {
    // The result is fixed: the limit slot frees before the waiter can read
    // it, so the waiter's next operation is not refused by this one.
    guard.release_limit();
    // A waiter gone by now leaves the delivery unread: dropping it abandons a
    // failure into history.
    drop(delivery.send(result));
    guard.end(OperationEnd::Finished);
}

/// The caller-facing result of one admitted operation.
///
/// Dropping it requests cancellation of the work.
#[must_use = "dropping the waiter cancels the operation"]
pub struct OperationWaiter<T> {
    receiver: oneshot::Receiver<Delivery<T>>,
    kind: IntegrationKind,
    operation: IntegrationOperation,
    instance: u64,
}

impl<T> OperationWaiter<T> {
    /// The waiter of one `operation` of a `kind` integration's `instance`.
    const fn new(
        receiver: oneshot::Receiver<Delivery<T>>,
        kind: IntegrationKind,
        operation: IntegrationOperation,
        instance: u64,
    ) -> Self {
        Self {
            receiver,
            kind,
            operation,
            instance,
        }
    }

    /// Resolve with the operation's result.
    ///
    /// # Errors
    ///
    /// The work's own integration failure, `TaskPanicked` when it unwound,
    /// and `Cancelled` when a forced stop dropped it before it reached a
    /// result. A dropped submission's outcome is unknown, never absent.
    pub async fn wait(self) -> Result<T, RuntimeError> {
        match self.receiver.await {
            Ok(Delivery::Succeeded(value)) => Ok(value),
            Ok(Delivery::Failed(failure)) => Err(integration(failure.receive())),
            Ok(Delivery::Panicked(panicked)) => Err(panicked),
            Err(_) => Err(integration(
                cut_short(self.kind, self.operation).with_instance(self.instance),
            )),
        }
    }
}

/// The failure of work dropped after it may have reached the peer: whether
/// it took effect is unknown.
const fn cut_short(kind: IntegrationKind, operation: IntegrationOperation) -> IntegrationError {
    IntegrationError::new(
        kind,
        operation,
        IntegrationFailure::Cancelled,
        Retryability::OutcomeUnknown,
    )
}

/// Carry one integration failure as the runtime error a caller reads.
pub(crate) fn integration(error: IntegrationError) -> RuntimeError {
    RuntimeError::Integration(Arc::new(error))
}
