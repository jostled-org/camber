//! One admitted integration instance and its settlement.
//!
//! An entry owns what outlives any one access handle: its close account, its
//! count of operations still running, its committed state, and the one close
//! result every closer reads. It moves through admitted, ready, closing, and
//! settled. Closing commits before any observer wakes, and from that moment it
//! refuses new work. It settles once no admitted operation is still running
//! and, when an adapter attached one, its close owner has finished closing the
//! transport.
//!
//! Every transition is one short critical section. The lock is never held over
//! I/O or across the runtime calls settlement makes, so a closer, a finishing
//! operation, and the registry stop can race without ordering one another.

use super::access::integration;
use super::accounts::{InstanceAccount, OperationAccount};
#[cfg(feature = "dns01")]
use super::cleanup::CleanupRegister;
use super::telemetry::{Outcome, TerminalOperation};
use crate::lifecycle::ShutdownOwner;
use crate::runtime_state::{RuntimeInner, recover_poisoned};
use crate::runtime_test_support::ParticipantDisposition;
use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
    RuntimeError,
};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, Weak};
#[cfg(feature = "nats")]
use tokio::sync::oneshot;
use tokio::sync::watch;
use tokio::time::Instant;

/// Where one admitted integration stands.
///
/// Closed and exhaustively matchable. The states only move forward.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntegrationEntryState {
    /// Admitted and accepting work; readiness not yet established.
    Admitted,
    /// The integration established readiness and accepts work.
    Ready,
    /// Close is committed: new work is refused, admitted work may finish.
    Closing,
    /// Every admitted operation ended and the close result is fixed.
    Settled,
}

/// How one admitted operation ended, as its own task saw it.
#[derive(Clone, Copy)]
pub(super) enum OperationEnd {
    /// The work ran to its own result.
    Finished,
    /// The waiter was dropped first, and the task dropped the work.
    Abandoned,
    /// The task was dropped around the work: a forced stop.
    Forced,
}

/// How an entry settled, and what that settlement reports.
enum Settlement {
    /// Every operation ended on its own terms or its waiter's.
    Completed,
    /// The forced stop dropped running work and the task came back.
    CancelledAndJoined,
    /// The close failed or could not be proven complete: the failure takes the
    /// close account, and the disposition says whether Camber's own work ended.
    Failed(IntegrationError, ParticipantDisposition),
}

/// Where an adapter's close owner stands.
///
/// The owner is the one task that closes the adapter's transport. Settlement
/// waits for it, so the close result names the transport's own close.
enum CloseOwnerState {
    /// No close owner is attached; settlement needs only the operations.
    Absent,
    /// Attached and waiting for closing to commit with no work running.
    #[cfg(feature = "nats")]
    Waiting(oneshot::Sender<()>),
    /// Told to close; settlement waits for its result.
    Running,
}

/// What a transition found once it left the entry drained.
enum Drained {
    /// Nothing to do yet.
    Pending,
    /// Wake the close owner, outside the lock.
    #[cfg(feature = "nats")]
    WakeOwner(oneshot::Sender<()>),
    /// Settle with the taken close account, outside the lock.
    Settle(InstanceAccount, Settlement),
}

/// The part of an entry its transitions change.
struct Phase {
    state: IntegrationEntryState,
    /// Operations admitted and not yet ended.
    in_flight: usize,
    /// Admitted operations that still hold an operation-limit slot. An
    /// operation gives its slot back once its result is fixed, before its
    /// caller can read it, so a sequential caller is never refused by the
    /// operation it just finished.
    running: usize,
    /// Whether a forced stop dropped any of this entry's work.
    forced: bool,
    /// The adapter's close owner, when one is attached.
    close_owner: CloseOwnerState,
    /// The first failure charged to the close account before settlement.
    close_failure: Option<(IntegrationError, ParticipantDisposition)>,
    /// The close account, until settlement takes it.
    account: Option<InstanceAccount>,
    /// Whether settlement is this instance's `Close` terminal: set once a
    /// connection established readiness, so a connection that never existed
    /// reports no close, or at admission for an owner with no connection.
    reports_close: bool,
    /// Whether an ended operation left cleanup records unresolved. The close
    /// reports them as incomplete; a later success does not clear them.
    cleanup_owed: bool,
    /// When closing was committed.
    closing_at: Option<Instant>,
    /// Whether the runtime's stop committed closing or named the entry
    /// outstanding.
    stopped: bool,
}

/// Who committed an entry's closing.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CloseCause {
    /// A closer, the last handle's drop, or a charged failure.
    Requested,
    /// The runtime's stop.
    Stop,
}

/// One admitted integration instance.
pub(crate) struct IntegrationEntry {
    id: u64,
    kind: IntegrationKind,
    /// The most operations this entry runs at once.
    operation_limit: usize,
    /// When admission admitted this entry.
    admitted: Instant,
    /// The runtime that admitted this entry, never the runtime of the thread
    /// using a handle. Weak, so an escaped handle cannot keep a finished
    /// runtime alive.
    runtime: Weak<RuntimeInner>,
    phase: Mutex<Phase>,
    /// The committed state, published inside the transition that commits it.
    published: watch::Sender<IntegrationEntryState>,
    /// The one close result every closer reads, fixed before `Settled`.
    closed: OnceLock<Result<(), Arc<IntegrationError>>>,
}

impl IntegrationEntry {
    /// An admitted entry that owns `account`, belongs to `runtime`, and runs
    /// at most `operation_limit` operations at once.
    pub(super) fn new(
        account: InstanceAccount,
        runtime: Weak<RuntimeInner>,
        operation_limit: usize,
    ) -> Arc<Self> {
        Arc::new(Self {
            id: account.instance_id(),
            kind: account.kind(),
            operation_limit,
            admitted: Instant::now(),
            runtime,
            phase: Mutex::new(Phase {
                state: IntegrationEntryState::Admitted,
                in_flight: 0,
                running: 0,
                forced: false,
                close_owner: CloseOwnerState::Absent,
                close_failure: None,
                account: Some(account),
                reports_close: false,
                cleanup_owed: false,
                closing_at: None,
                stopped: false,
            }),
            published: watch::Sender::new(IntegrationEntryState::Admitted),
            closed: OnceLock::new(),
        })
    }

    fn phase(&self) -> MutexGuard<'_, Phase> {
        recover_poisoned(self.phase.lock())
    }

    /// The runtime-local identity admission assigned.
    pub(super) const fn id(&self) -> u64 {
        self.id
    }

    /// The integration's closed kind.
    pub(super) const fn kind(&self) -> IntegrationKind {
        self.kind
    }

    /// When admission admitted this entry.
    #[cfg(any(feature = "nats", feature = "sqs"))]
    pub(super) const fn admitted(&self) -> Instant {
        self.admitted
    }

    /// Commit `state` and wake every observer, inside the caller's section.
    fn commit(&self, phase: &mut Phase, state: IntegrationEntryState) {
        phase.state = state;
        self.published.send_replace(state);
    }

    /// The runtime that admitted this entry, or `Closed` once it is gone.
    pub(super) fn runtime(
        &self,
        operation: IntegrationOperation,
    ) -> Result<Arc<RuntimeInner>, IntegrationError> {
        self.runtime
            .upgrade()
            .ok_or_else(|| self.closed_error(operation))
    }

    /// The refusal a closed entry answers new work with.
    fn closed_error(&self, operation: IntegrationOperation) -> IntegrationError {
        IntegrationError::new(
            self.kind,
            operation,
            IntegrationFailure::Closed,
            Retryability::Never,
        )
        .with_instance(self.id)
    }

    /// Record that the integration established readiness.
    ///
    /// # Errors
    ///
    /// `Closed` once close is committed.
    pub(super) fn ready(&self) -> Result<(), IntegrationError> {
        self.establish(false, IntegrationOperation::Ready)
    }

    /// Record that a connection established readiness: from now on its
    /// settlement is its `Close` terminal.
    ///
    /// # Errors
    ///
    /// `Closed`, naming the `Connect` it refuses, once close is committed.
    #[cfg(any(feature = "nats", feature = "sqs"))]
    pub(super) fn connected(&self) -> Result<(), IntegrationError> {
        self.establish(true, IntegrationOperation::Connect)
    }

    /// Report settlement as this instance's `Close` terminal from admission.
    #[cfg(feature = "dns01")]
    pub(super) fn reports_close(&self) {
        self.phase().reports_close = true;
    }

    /// Commit readiness, arming the `Close` terminal when `reports_close`
    /// says so, in the same section. A refusal names `operation`, the one
    /// its caller ran.
    fn establish(
        &self,
        reports_close: bool,
        operation: IntegrationOperation,
    ) -> Result<(), IntegrationError> {
        let mut phase = self.phase();
        match phase.state {
            IntegrationEntryState::Admitted => {
                self.commit(&mut phase, IntegrationEntryState::Ready);
            }
            IntegrationEntryState::Ready => {}
            IntegrationEntryState::Closing | IntegrationEntryState::Settled => {
                return Err(self.closed_error(operation));
            }
        }
        phase.reports_close |= reports_close;
        Ok(())
    }

    /// Commit closing in the caller's section, recording when and by whom.
    fn commit_closing(&self, phase: &mut Phase, cause: CloseCause) {
        phase.closing_at = Some(Instant::now());
        phase.stopped = cause == CloseCause::Stop;
        self.commit(phase, IntegrationEntryState::Closing);
    }

    /// Admit one operation: reserve its report account and count it running.
    ///
    /// # Errors
    ///
    /// `Closed` once close is committed, `Busy` when the entry already runs its
    /// operation limit or the report budget is full. A refusal counts nothing
    /// and reserves nothing.
    pub(super) fn begin_operation(
        self: &Arc<Self>,
        operation: IntegrationOperation,
    ) -> Result<(OperationAccount, OperationGuard), IntegrationError> {
        let mut phase = self.phase();
        let reserved = match (phase.state, phase.account.as_ref()) {
            (IntegrationEntryState::Admitted | IntegrationEntryState::Ready, Some(_))
                if phase.running >= self.operation_limit =>
            {
                return Err(super::accounts::busy(self.kind, operation).with_instance(self.id));
            }
            (IntegrationEntryState::Admitted | IntegrationEntryState::Ready, Some(account)) => {
                account.reserve(operation)?
            }
            _ => return Err(self.closed_error(operation)),
        };
        phase.in_flight += 1;
        phase.running += 1;
        Ok((
            reserved,
            OperationGuard {
                entry: Arc::clone(self),
                end: OperationEnd::Forced,
                holds_limit: true,
                #[cfg(feature = "dns01")]
                cleanup: None,
            },
        ))
    }

    /// One admitted operation's result is fixed: its limit slot is free.
    fn release_limit(&self) {
        let mut phase = self.phase();
        phase.running = phase.running.saturating_sub(1);
    }

    /// One admitted operation ended, still holding its limit slot when
    /// `held_limit` says so and owing cleanup records when `owes_cleanup`
    /// does; settle when it was the last one of a closing entry.
    fn end_operation(&self, end: OperationEnd, held_limit: bool, owes_cleanup: bool) {
        let mut phase = self.phase();
        phase.in_flight = phase.in_flight.saturating_sub(1);
        if held_limit {
            phase.running = phase.running.saturating_sub(1);
        }
        phase.forced |= matches!(end, OperationEnd::Forced);
        phase.cleanup_owed |= owes_cleanup;
        let drained = self.drained(&mut phase);
        drop(phase);
        self.act_on(drained);
    }

    /// Commit closing, then settle at once when no work is running.
    ///
    /// Idempotent: a closing or settled entry keeps its state and its result.
    pub(super) fn request_close(&self) {
        self.close_by(CloseCause::Requested);
    }

    /// The runtime's stop commits closing; see [`Self::request_close`].
    pub(super) fn stop(&self) {
        self.close_by(CloseCause::Stop);
    }

    fn close_by(&self, cause: CloseCause) {
        let cause = self.owning_cause(cause);
        let mut phase = self.phase();
        let drained = match phase.state {
            IntegrationEntryState::Admitted | IntegrationEntryState::Ready => {
                self.commit_closing(&mut phase, cause);
                self.drained(&mut phase)
            }
            IntegrationEntryState::Closing | IntegrationEntryState::Settled => Drained::Pending,
        };
        drop(phase);
        self.act_on(drained);
    }

    /// Who owns a close committed now for `cause`.
    ///
    /// Root closure begins the runtime's stop before the registry stop reaches
    /// this entry. Work that wakes on `ScopeClosing` can drop its last handle
    /// in that window, so the stop owns every close committed once the scope
    /// is closed, whichever caller reaches the entry first.
    fn owning_cause(&self, cause: CloseCause) -> CloseCause {
        let stopping = self
            .runtime
            .upgrade()
            .is_some_and(|runtime| !runtime.admits_children());
        match stopping {
            true => CloseCause::Stop,
            false => cause,
        }
    }

    /// Charge `error` to the close account, then commit closing.
    ///
    /// The first charged failure is the one settlement keeps; a later one
    /// finds the account already answered. An entry already settled keeps
    /// its settlement.
    #[cfg(feature = "nats")]
    pub(super) fn fail_and_close(&self, error: IntegrationError) {
        {
            let mut phase = self.phase();
            if phase.account.is_some() && phase.close_failure.is_none() {
                phase.close_failure = Some((error, ParticipantDisposition::Completed));
            }
        }
        self.request_close();
    }

    /// Attach the one task that closes the adapter's transport.
    ///
    /// The returned receiver resolves once closing is committed and no
    /// operation is running.
    ///
    /// # Errors
    ///
    /// `Closed` once close is committed or an owner is already attached.
    #[cfg(feature = "nats")]
    pub(super) fn attach_close_owner(
        &self,
        operation: IntegrationOperation,
    ) -> Result<oneshot::Receiver<()>, IntegrationError> {
        let mut phase = self.phase();
        match (phase.state, &phase.close_owner) {
            (
                IntegrationEntryState::Admitted | IntegrationEntryState::Ready,
                CloseOwnerState::Absent,
            ) => {
                let (wake, woken) = oneshot::channel();
                phase.close_owner = CloseOwnerState::Waiting(wake);
                Ok(woken)
            }
            _ => Err(self.closed_error(operation)),
        }
    }

    /// The close owner ended: `Some` with its close result, or `None` when a
    /// forced stop dropped it first.
    #[cfg(feature = "nats")]
    pub(super) fn end_close_owner(&self, closed: Option<Result<(), IntegrationError>>) {
        let mut phase = self.phase();
        phase.close_owner = CloseOwnerState::Absent;
        match (closed, phase.close_failure.is_some()) {
            (Some(Err(error)), false) => {
                phase.close_failure = Some((error, ParticipantDisposition::Named));
            }
            (None, _) => phase.forced = true,
            (Some(_), _) => {}
        }
        let drained = self.drained(&mut phase);
        drop(phase);
        self.act_on(drained);
    }

    /// Name this entry outstanding: teardown can no longer wait for its work.
    ///
    /// Settles it as `Named`, retaining the incomplete close in its own
    /// account. An entry already settling keeps the settlement it reached.
    pub(super) fn name_outstanding(&self) {
        let mut phase = self.phase();
        if matches!(
            phase.state,
            IntegrationEntryState::Admitted | IntegrationEntryState::Ready
        ) {
            self.commit_closing(&mut phase, CloseCause::Stop);
        }
        // The stop settles whatever it names, whoever committed closing.
        phase.stopped = true;
        let account = phase.account.take();
        drop(phase);
        if let Some(account) = account {
            self.settle(
                account,
                Settlement::Failed(
                    IntegrationError::incomplete_close(self.kind),
                    ParticipantDisposition::Named,
                ),
            );
        }
    }

    /// What a closing entry with no work running does next: wake its close
    /// owner, wait for it, or take the close account to settle.
    fn drained(&self, phase: &mut Phase) -> Drained {
        if !matches!(
            (phase.state, phase.in_flight),
            (IntegrationEntryState::Closing, 0)
        ) {
            return Drained::Pending;
        }
        match std::mem::replace(&mut phase.close_owner, CloseOwnerState::Running) {
            #[cfg(feature = "nats")]
            CloseOwnerState::Waiting(wake) => return Drained::WakeOwner(wake),
            CloseOwnerState::Running => return Drained::Pending,
            CloseOwnerState::Absent => phase.close_owner = CloseOwnerState::Absent,
        }
        let settlement = match (phase.close_failure.take(), phase.forced) {
            (Some((error, disposition)), _) => Settlement::Failed(error, disposition),
            (None, true) => Settlement::CancelledAndJoined,
            (None, false) => Settlement::Completed,
        };
        match phase.account.take() {
            Some(account) => Drained::Settle(account, settlement),
            None => Drained::Pending,
        }
    }

    /// Carry out what [`Self::drained`] decided, outside the lock.
    fn act_on(&self, drained: Drained) {
        match drained {
            Drained::Pending => {}
            // A close owner already gone ends through its own drop.
            #[cfg(feature = "nats")]
            Drained::WakeOwner(wake) => drop(wake.send(())),
            Drained::Settle(account, settlement) => self.settle(account, settlement),
        }
    }

    /// Fix the close result, report it, retire the live slot, publish the
    /// disposition, and only then commit `Settled`.
    ///
    /// Retirement precedes the commit, so an observer woken by `Settled` finds
    /// the slot already free; the terminal precedes both, so a closer reads
    /// a result already reported.
    fn settle(&self, account: InstanceAccount, settlement: Settlement) {
        let (closed, disposition) = match settlement {
            Settlement::Completed => {
                account.close();
                (Ok(()), ParticipantDisposition::Completed)
            }
            Settlement::CancelledAndJoined => {
                account.close();
                (Ok(()), ParticipantDisposition::CancelledAndJoined)
            }
            Settlement::Failed(error, disposition) => {
                let error = error.with_instance(self.id);
                account.close_failed(error.clone());
                (Err(Arc::new(error)), disposition)
            }
        };
        self.report_close(&closed);
        // The account was taken exactly once, so this is the only writer.
        self.closed.get_or_init(|| closed);
        if let Some(runtime) = self.runtime.upgrade() {
            runtime.integrations().retire(self.id);
            runtime
                .shutdown_deadline_ref()
                .settle(&ShutdownOwner::integration(self.kind, self.id), disposition);
        }
        let mut phase = self.phase();
        self.commit(&mut phase, IntegrationEntryState::Settled);
    }

    /// Emit the `Close` terminal of a connection that established readiness.
    ///
    /// A failure charged to the close account reports under the operation
    /// that failed, so a delivered slow-consumer event is one receive
    /// terminal, not a close. A close that leaves cleanup records unresolved
    /// is incomplete; the records stay charged to the operation that owes
    /// them. The duration runs from the closing commit.
    fn report_close(&self, closed: &Result<(), Arc<IntegrationError>>) {
        let (reports_close, cleanup_owed, closing_at, stopped) = {
            let phase = self.phase();
            (
                phase.reports_close,
                phase.cleanup_owed,
                phase.closing_at,
                phase.stopped,
            )
        };
        if !reports_close {
            return;
        }
        let (operation, outcome) = match (closed, cleanup_owed) {
            (Err(error), _) => (error.operation(), Outcome::failed(error)),
            (Ok(()), true) => (
                IntegrationOperation::Close,
                Outcome::Failed(IntegrationFailure::CleanupIncomplete, Retryability::Never),
            ),
            (Ok(()), false) => (IntegrationOperation::Close, Outcome::Succeeded),
        };
        TerminalOperation::new(self.kind, operation, Some(self.id))
            .admitted_at(closing_at.unwrap_or(self.admitted))
            .settle_as(outcome, stopped);
    }

    /// The one close result, as every closer reads it.
    pub(super) fn close_result(&self) -> Result<(), RuntimeError> {
        match self.closed.get() {
            Some(Ok(())) => Ok(()),
            Some(Err(error)) => Err(RuntimeError::Integration(Arc::clone(error))),
            None => Err(integration(self.closed_error(IntegrationOperation::Close))),
        }
    }

    /// The committed state, read once without subscribing.
    #[cfg(feature = "nats")]
    pub(super) fn state(&self) -> IntegrationEntryState {
        *self.published.borrow()
    }

    /// A read-only view of this entry's committed state.
    pub(super) fn observer(&self) -> IntegrationEntryObserver {
        IntegrationEntryObserver {
            state: self.published.subscribe(),
        }
    }
}

/// One admitted operation's claim on its entry.
///
/// Dropping it ends the operation. A guard dropped without a recorded end was
/// dropped around its work, which only a forced stop does.
pub(super) struct OperationGuard {
    entry: Arc<IntegrationEntry>,
    end: OperationEnd,
    /// Whether the operation still holds its operation-limit slot.
    holds_limit: bool,
    /// The records the operation's cleanup owes, read when it ends.
    #[cfg(feature = "dns01")]
    cleanup: Option<CleanupRegister>,
}

impl OperationGuard {
    /// Read `cleanup` when the operation ends: records it still owes make
    /// the entry's close incomplete.
    #[cfg(feature = "dns01")]
    pub(super) fn with_cleanup(mut self, cleanup: CleanupRegister) -> Self {
        self.cleanup = Some(cleanup);
        self
    }

    /// Whether the operation's cleanup still owes records.
    #[cfg(feature = "dns01")]
    fn owes_cleanup(&self) -> bool {
        self.cleanup.as_ref().is_some_and(CleanupRegister::owes)
    }

    /// Without a cleanup register, an operation owes no records.
    #[cfg(not(feature = "dns01"))]
    const fn owes_cleanup(&self) -> bool {
        false
    }

    /// The operation's result is fixed: free its limit slot, keeping the
    /// operation counted until it ends.
    pub(super) fn release_limit(&mut self) {
        if std::mem::take(&mut self.holds_limit) {
            self.entry.release_limit();
        }
    }

    /// The operation ran to its own result: free its limit slot and end it.
    #[cfg(feature = "dns01")]
    pub(super) fn finish(mut self) {
        self.release_limit();
        self.end(OperationEnd::Finished);
    }

    /// End the operation as `end` says it ended.
    pub(super) fn end(mut self, end: OperationEnd) {
        self.end = end;
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.entry.end_operation(
            self.end,
            std::mem::take(&mut self.holds_limit),
            self.owes_cleanup(),
        );
    }
}

/// A read-only view of one entry's committed state.
///
/// Holds no access: it cannot keep an entry open, close it, or submit work.
#[derive(Clone, Debug)]
pub struct IntegrationEntryObserver {
    state: watch::Receiver<IntegrationEntryState>,
}

impl IntegrationEntryObserver {
    /// The committed state now.
    #[must_use]
    pub fn state(&self) -> IntegrationEntryState {
        *self.state.borrow()
    }

    /// Resolve once close is committed.
    pub fn closing(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        self.reached(|state| {
            matches!(
                state,
                IntegrationEntryState::Closing | IntegrationEntryState::Settled
            )
        })
    }

    /// Resolve once the entry has settled.
    pub fn settled(&self) -> impl Future<Output = ()> + Send + 'static + use<> {
        self.reached(|state| state == IntegrationEntryState::Settled)
    }

    /// Resolve once the committed state satisfies `reached`.
    ///
    /// An entry that no longer exists cannot move again, so its last state is
    /// final and the wait ends.
    fn reached(
        &self,
        reached: fn(IntegrationEntryState) -> bool,
    ) -> impl Future<Output = ()> + Send + 'static + use<> {
        let mut state = self.state.clone();
        async move {
            // A dropped sender is the entry's end, not a failure: either
            // answer ends the wait.
            drop(state.wait_for(|state| reached(*state)).await);
        }
    }
}
