//! The one emitter of integration operation terminals.
//!
//! Every managed operation settles into one terminal: one structured event, one
//! increment of `camber_integration_operations_total`, and, for an admitted
//! operation, one `camber_integration_operation_duration_seconds` sample from
//! its admission to its settlement. A refusal before admission is counted once
//! and records no duration, because nothing was admitted to measure.
//!
//! Labels come from the closed vocabulary alone: kind, operation, and outcome.
//! The instance identity, the failure, its retryability, and whether the
//! runtime's stop settled the operation are event fields, never labels. No
//! source error, subject, or URL is a label, and no payload or credential
//! reaches either.
//!
//! A terminal is emitted from the committed result, before any waiter can read
//! it. Reading a result again or transferring it into the runtime aggregate
//! emits nothing.
//!
//! A DNS-01 order's nested operations report their own terminals under the
//! order's instance, and spend no report account of their own. A record
//! operation names its exact record ID, and an incomplete cleanup names every
//! record it left unresolved, as event fields.

#[cfg(feature = "dns01")]
use super::cleanup::CleanupRegister;
#[cfg(feature = "dns01")]
use crate::runtime_state::LifecycleSignals;
use crate::{
    CleanupItem, IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation,
    Retryability, RuntimeError,
};
use tokio::time::Instant;

/// The fixed sentence every terminal event carries as its whole message.
const MESSAGE: &str = "integration operation finished";

/// The counter every terminal increments once.
const OPERATIONS_TOTAL: &str = "camber_integration_operations_total";

/// The admission-to-settlement duration of each admitted operation.
const DURATION: &str = "camber_integration_operation_duration_seconds";

/// The outcome label a successful terminal carries.
const SUCCESS: &str = "success";

/// The operation one terminal reports.
#[derive(Clone, Copy)]
pub(crate) struct TerminalOperation {
    kind: IntegrationKind,
    operation: IntegrationOperation,
    /// The admitted instance, or `None` before admission assigned one.
    instance: Option<u64>,
}

impl TerminalOperation {
    pub(crate) const fn new(
        kind: IntegrationKind,
        operation: IntegrationOperation,
        instance: Option<u64>,
    ) -> Self {
        Self {
            kind,
            operation,
            instance,
        }
    }

    /// Count `error` as this operation's refusal before admission.
    pub(crate) fn refused(self, error: &RuntimeError) {
        let (failure, retryability) = match error {
            RuntimeError::Integration(error) => return self.refused_integration(error),
            // A configuration the caller wrote that admission refused.
            RuntimeError::Config(_) => (IntegrationFailure::InvalidConfig, Retryability::Never),
            // No runtime can own the work: none is established, or its
            // admission has closed. Repeating the call there cannot succeed.
            _ => (IntegrationFailure::Closed, Retryability::Never),
        };
        self.refuse_as(Outcome::Failed(failure, retryability));
    }

    /// Count the integration failure `error` as this operation's refusal
    /// before admission, under the instance either one names.
    pub(super) fn refused_integration(self, error: &IntegrationError) {
        let instance = self.instance.or(error.instance_id());
        Self { instance, ..self }.refuse_as(Outcome::failed(error));
    }

    /// Count `outcome` as this operation's refusal before admission: nothing
    /// was admitted, so no duration is recorded.
    fn refuse_as(self, outcome: Outcome) {
        Terminal {
            operation: self,
            outcome,
            shutdown: false,
            detail: Detail::NONE,
        }
        .emit(None);
    }

    /// Pass `result` through, counting its error as this operation's refusal.
    #[cfg(any(feature = "nats", feature = "sqs", feature = "dns01"))]
    pub(crate) fn refusing<T>(self, result: Result<T, RuntimeError>) -> Result<T, RuntimeError> {
        result.inspect_err(|error| self.refused(error))
    }

    /// Admit this operation now: its terminal is owed from this instant.
    pub(crate) fn admitted(self) -> OwedTerminal {
        self.admitted_at(Instant::now())
    }

    /// The terminal this operation owes since its admission at `admitted`.
    pub(crate) const fn admitted_at(self, admitted: Instant) -> OwedTerminal {
        OwedTerminal {
            operation: self,
            admitted,
            dropped: Some(Dropped::Caller),
            detail: Detail::NONE,
            #[cfg(feature = "dns01")]
            cleanup: None,
        }
    }
}

/// What a terminal names beyond its closed fields: event fields only, never
/// labels, and never a payload or credential.
struct Detail {
    /// The exact record ID a record operation acted on.
    record_id: Option<Box<str>>,
    /// Every record a cleanup left unresolved: its exact ID, or its domain
    /// when no acknowledgement named one.
    unresolved: Option<Box<str>>,
}

impl Detail {
    /// Nothing beyond the closed fields.
    const NONE: Self = Self {
        record_id: None,
        unresolved: None,
    };

    /// Name every record in `items` as unresolved, or none when it is empty.
    fn name_unresolved(&mut self, items: &[CleanupItem]) {
        let named: Box<[&str]> = items
            .iter()
            .map(|item| item.record_id().unwrap_or_else(|| item.domain()))
            .collect();
        self.unresolved = (!named.is_empty()).then(|| named.join(",").into_boxed_str());
    }
}

/// How one operation settled.
#[derive(Clone, Copy)]
pub(crate) enum Outcome {
    Succeeded,
    Failed(IntegrationFailure, Retryability),
}

impl Outcome {
    /// Work cut short by an unwind or a stop: what it had sent is unknown.
    pub(crate) const CUT_SHORT: Self =
        Self::Failed(IntegrationFailure::Cancelled, Retryability::OutcomeUnknown);

    /// Work cancelled before it sent anything: repeating it is safe.
    pub(crate) const UNSENT: Self = Self::Failed(IntegrationFailure::Cancelled, Retryability::Safe);

    /// The outcome `error` commits.
    pub(crate) const fn failed(error: &IntegrationError) -> Self {
        Self::Failed(error.failure(), error.retryability())
    }

    /// The outcome `result` commits.
    ///
    /// An error outside the integration vocabulary is the work's unwind.
    #[cfg(any(feature = "nats", feature = "sqs"))]
    pub(crate) fn of<T>(result: &Result<T, RuntimeError>) -> Self {
        match result {
            Ok(_) => Self::Succeeded,
            Err(RuntimeError::Integration(error)) => Self::failed(error),
            Err(_) => Self::CUT_SHORT,
        }
    }

    const fn label(self) -> &'static str {
        match self {
            Self::Succeeded => SUCCESS,
            Self::Failed(failure, _) => failure.label(),
        }
    }
}

/// Who dropped an owed terminal that never settled.
enum Dropped {
    /// The caller dropped its own call: nothing was taken or sent.
    Caller,
    /// The runtime's forced stop dropped admitted work, whose effect is
    /// unknown.
    Stop,
    /// The enclosing work cut this nested operation short, after it may have
    /// sent something: a deadline, a stop, or a forced stop that dropped the
    /// whole frame. The runtime's stop settled it when these signals fired.
    #[cfg(feature = "dns01")]
    Cut(LifecycleSignals),
}

impl Dropped {
    const fn outcome(&self) -> Outcome {
        match self {
            Self::Caller => Outcome::UNSENT,
            Self::Stop => Outcome::CUT_SHORT,
            #[cfg(feature = "dns01")]
            Self::Cut(_) => Outcome::CUT_SHORT,
        }
    }

    fn shutdown(&self) -> bool {
        match self {
            Self::Caller => false,
            Self::Stop => true,
            #[cfg(feature = "dns01")]
            Self::Cut(stopping) => stopping.is_fired(),
        }
    }
}

/// One admitted operation's terminal, owed until it settles.
///
/// Settles once. Dropping it unsettled settles it as cancelled.
#[must_use = "an admitted operation owes its terminal until it settles"]
pub(crate) struct OwedTerminal {
    operation: TerminalOperation,
    admitted: Instant,
    /// How a drop settles it, or `None` once it settled.
    dropped: Option<Dropped>,
    detail: Detail,
    /// The records the operation's cleanup owes, which a drop reports.
    #[cfg(feature = "dns01")]
    cleanup: Option<CleanupRegister>,
}

impl OwedTerminal {
    /// Only the runtime's forced stop can drop this terminal's work, so a
    /// drop settles it as cancelled under shutdown with an unknown outcome.
    pub(crate) fn dropped_by_stop(mut self) -> Self {
        self.dropped = Some(Dropped::Stop);
        self
    }

    /// Only the enclosing work can drop this nested operation: a drop
    /// settles it as cut short, under shutdown once `stopping` fired.
    #[cfg(feature = "dns01")]
    fn cut_under(mut self, stopping: LifecycleSignals) -> Self {
        self.dropped = Some(Dropped::Cut(stopping));
        self
    }

    /// Name the exact record this operation acts on.
    #[cfg(feature = "dns01")]
    pub(crate) fn with_record(mut self, record_id: &str) -> Self {
        self.detail.record_id = Some(record_id.into());
        self
    }

    /// A drop reports the records `cleanup` still owes as an incomplete
    /// cleanup naming each one, rather than as a bare cancellation.
    #[cfg(feature = "dns01")]
    pub(super) fn with_cleanup(mut self, cleanup: CleanupRegister) -> Self {
        self.cleanup = Some(cleanup);
        self
    }

    /// Settle with `outcome`, outside any runtime stop.
    pub(crate) fn settle(self, outcome: Outcome) {
        self.settle_as(outcome, false);
    }

    /// Settle with `outcome`; `shutdown` says whether the runtime's stop
    /// settled it.
    pub(crate) fn settle_as(mut self, outcome: Outcome, shutdown: bool) {
        self.dropped = None;
        self.emit(outcome, shutdown);
    }

    /// Settle as `error`, naming every record its cleanup left unresolved;
    /// `shutdown` says whether the runtime's stop settled it.
    pub(super) fn settle_error(mut self, error: &IntegrationError, shutdown: bool) {
        self.detail.name_unresolved(error.cleanup());
        self.settle_as(Outcome::failed(error), shutdown);
    }

    /// Settle with the outcome `result` commits. A failure settled while
    /// `stopping` says the runtime's stop is underway is the stop's.
    #[cfg(feature = "dns01")]
    fn settle_result<T>(self, result: &Result<T, IntegrationError>, stopping: bool) {
        match result {
            Ok(_) => self.settle(Outcome::Succeeded),
            Err(error) => self.settle_error(error, stopping),
        }
    }

    /// Settle as `error`'s refusal: the work was never launched, so no
    /// duration is recorded.
    pub(crate) fn refused(mut self, error: &RuntimeError) {
        self.dropped = None;
        self.operation.refused(error);
    }

    /// Settle with the outcome `result` commits, and pass it through.
    #[cfg(any(feature = "nats", feature = "sqs"))]
    pub(crate) fn settled<T>(self, result: Result<T, RuntimeError>) -> Result<T, RuntimeError> {
        self.settle(Outcome::of(&result));
        result
    }

    fn emit(&mut self, outcome: Outcome, shutdown: bool) {
        Terminal {
            operation: self.operation,
            outcome,
            shutdown,
            detail: std::mem::replace(&mut self.detail, Detail::NONE),
        }
        .emit(Some(self.admitted));
    }

    /// Settle work that unwound before it reached a result. Records its
    /// cleanup still owes make it an incomplete cleanup naming each one;
    /// `shutdown` says whether the runtime's stop settled it.
    pub(super) fn unwound(mut self, shutdown: bool) {
        self.dropped = None;
        self.settle_owed(Outcome::CUT_SHORT, shutdown);
    }

    /// Settle work dropped unsettled as `dropped` says.
    fn cut_short(&mut self, dropped: &Dropped) {
        self.settle_owed(dropped.outcome(), dropped.shutdown());
    }

    /// Settle work that left no result of its own: as the incomplete cleanup
    /// naming every record it still owes, or as `fallback` when it owes none.
    fn settle_owed(&mut self, fallback: Outcome, shutdown: bool) {
        match self.owed_cleanup() {
            Some(incomplete) => {
                self.detail.name_unresolved(incomplete.cleanup());
                self.emit(Outcome::failed(&incomplete), shutdown);
            }
            None => self.emit(fallback, shutdown),
        }
    }

    /// The incomplete cleanup the dropped work leaves, when it owes one.
    #[cfg(feature = "dns01")]
    fn owed_cleanup(&self) -> Option<IntegrationError> {
        self.cleanup.as_ref().and_then(CleanupRegister::incomplete)
    }

    /// Without a cleanup register, dropped work owes no records.
    #[cfg(not(feature = "dns01"))]
    const fn owed_cleanup(&self) -> Option<IntegrationError> {
        None
    }
}

impl Drop for OwedTerminal {
    fn drop(&mut self) {
        if let Some(dropped) = self.dropped.take() {
            self.cut_short(&dropped);
        }
    }
}

/// The terminals of one DNS-01 instance's nested operations: preparation,
/// record writes, and cache work inside an order or a renewal check.
///
/// Each is timed from its own start and reported under the instance, and
/// none reserves a report account: they spend the enclosing order's. A
/// nested operation the enclosing work cuts short reports as cancelled.
#[cfg(feature = "dns01")]
#[derive(Clone)]
pub(crate) struct NestedTerminals {
    kind: IntegrationKind,
    instance: u64,
    /// The signals of the runtime that admitted the instance: a failure
    /// settled once they fired is the stop's.
    stopping: LifecycleSignals,
}

#[cfg(feature = "dns01")]
impl NestedTerminals {
    pub(crate) const fn new(
        kind: IntegrationKind,
        instance: u64,
        stopping: LifecycleSignals,
    ) -> Self {
        Self {
            kind,
            instance,
            stopping,
        }
    }

    /// `operation` of this instance, as its terminal names it.
    const fn terminal(&self, operation: IntegrationOperation) -> TerminalOperation {
        TerminalOperation::new(self.kind, operation, Some(self.instance))
    }

    /// Start `operation` now; its terminal is owed until it settles.
    pub(crate) fn begin(&self, operation: IntegrationOperation) -> OwedTerminal {
        self.terminal(operation)
            .admitted()
            .cut_under(self.stopping.clone())
    }

    /// Settle `owed` with the outcome `result` commits.
    pub(crate) fn settle<T>(&self, owed: OwedTerminal, result: &Result<T, IntegrationError>) {
        owed.settle_result(result, self.stopping.is_fired());
    }

    /// Run `work` as one `operation` and report its result.
    ///
    /// # Errors
    ///
    /// The error `work` returns.
    pub(crate) fn run<T>(
        &self,
        operation: IntegrationOperation,
        work: impl FnOnce() -> Result<T, IntegrationError>,
    ) -> Result<T, IntegrationError> {
        let owed = self.begin(operation);
        let result = work();
        self.settle(owed, &result);
        result
    }

    /// Report `error` as `operation`'s refusal before it sent anything: no
    /// duration is recorded.
    pub(crate) fn refused(&self, operation: IntegrationOperation, error: &IntegrationError) {
        self.terminal(operation).refuse_as(Outcome::failed(error));
    }
}

/// One settled operation, ready to report.
struct Terminal {
    operation: TerminalOperation,
    outcome: Outcome,
    shutdown: bool,
    detail: Detail,
}

impl Terminal {
    /// Record the event, count it, and sample the duration since `admitted`
    /// when the operation was admitted.
    fn emit(&self, admitted: Option<Instant>) {
        self.record_event();
        // Built once and read by both instruments, so the counter and the
        // histogram cannot disagree about one terminal's labels.
        let labels = [
            metrics::Label::from_static_parts("kind", self.operation.kind.label()),
            metrics::Label::from_static_parts("operation", self.operation.operation.label()),
            metrics::Label::from_static_parts("outcome", self.outcome.label()),
        ];
        metrics::counter!(OPERATIONS_TOTAL, labels.iter()).increment(1);
        if let Some(admitted) = admitted {
            metrics::histogram!(DURATION, labels.iter()).record(admitted.elapsed().as_secs_f64());
        }
    }

    /// Record the event. A field with no value is left out, not recorded
    /// empty: a success names no failure, and a refusal before admission
    /// names no instance.
    fn record_event(&self) {
        let (failure, retryability) = match self.outcome {
            Outcome::Succeeded => (None, None),
            Outcome::Failed(failure, retryability) => {
                (Some(failure.label()), Some(retryability.label()))
            }
        };
        tracing::info!(
            kind = self.operation.kind.label(),
            operation = self.operation.operation.label(),
            outcome = self.outcome.label(),
            shutdown = self.shutdown,
            instance_id = self.operation.instance,
            failure,
            retryability,
            record_id = self.detail.record_id.as_deref(),
            unresolved = self.detail.unresolved.as_deref(),
            "{MESSAGE}"
        );
    }
}
