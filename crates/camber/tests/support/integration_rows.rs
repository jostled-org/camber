//! Row verdicts for the integration matrices.
//!
//! A row returns its own verdict instead of panicking, so a matrix runs every
//! row, cleans up after each, and reports every broken claim together. The
//! typed refusal a row compares is the operation, failure, and retryability an
//! integration error carries; its diagnostic text is never compared.

use camber::runtime_test_support::{IntegrationLifecycleProbe, IntegrationProbeHandle};
use camber::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation,
    LifecycleFailureKind, LifecycleParticipant, Retryability, RuntimeBuilder, RuntimeError,
    runtime,
};
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::task::Poll;
use std::time::Duration;
use tokio::sync::oneshot;

/// A row's verdict: `Err` names the broken claim.
pub type Row = Result<(), String>;

/// A typed integration refusal: operation, failure, and retryability.
pub type Refusal = (IntegrationOperation, IntegrationFailure, Retryability);

/// One row of a matrix: its name and the function that answers its verdict.
pub type NamedRow<'a> = (&'a str, fn() -> Row);

/// The hang guard every bounded wait runs under; never a timing assertion.
pub const ROW_BOUND: Duration = Duration::from_secs(10);

/// The integration report accounts one runtime retains.
pub const REPORT_BUDGET: usize = 256;

/// A bound a row exhausts on purpose: an operation or close the peer never
/// lets finish.
pub const EXHAUSTED: Duration = Duration::from_millis(300);

/// The fixed number of live integrations one runtime admits.
pub const LIVE_LIMIT: usize = 64;

/// Sequential controls a report-budget row retires one at a time: more than
/// the whole budget, so a control that leaked would saturate it.
pub const CONTROL_ROUNDS: usize = REPORT_BUDGET + 44;

/// A NATS subject the SDK refuses after admission and registration: the
/// refusal reaches the operation with nothing sent.
pub const INVALID_NATS_SUBJECT: &str = "bad subject";

/// Run every row and fail once, naming each row that failed.
///
/// # Panics
///
/// When any row failed.
pub fn run_rows(rows: &[NamedRow<'_>]) {
    run_rows_under("matrix", rows);
}

/// [`run_rows`], with the failure report headed by `diagnostic`.
///
/// # Panics
///
/// When any row failed.
pub fn run_rows_under(diagnostic: &str, rows: &[NamedRow<'_>]) {
    assert_verdicts(diagnostic, rows.iter().map(|(name, row)| (*name, row())));
}

/// Fail once, under `diagnostic`, naming each verdict that failed.
///
/// # Panics
///
/// When any verdict failed.
pub fn assert_verdicts<'a>(diagnostic: &str, verdicts: impl IntoIterator<Item = (&'a str, Row)>) {
    let mut total = 0_usize;
    let failures: Vec<String> = verdicts
        .into_iter()
        .inspect(|_| total += 1)
        .filter_map(|(name, verdict)| verdict.err().map(|reason| format!("{name}: {reason}")))
        .collect();
    assert!(
        failures.is_empty(),
        "{diagnostic}: {} of {total} rows failed:\n{}",
        failures.len(),
        failures.join("\n")
    );
}

/// Fold checks a row observed independently into one verdict that names
/// every failed check.
///
/// # Errors
///
/// Every failed check, in order.
pub fn all(checks: impl IntoIterator<Item = Row>) -> Row {
    let failed: Vec<String> = checks.into_iter().filter_map(Result::err).collect();
    match failed.is_empty() {
        true => Ok(()),
        false => Err(failed.join("; ")),
    }
}

/// Fail the row as `what` unless `holds`.
///
/// # Errors
///
/// `what`, when the claim does not hold.
pub fn expect(what: &str, holds: bool) -> Row {
    match holds {
        true => Ok(()),
        false => Err(what.to_owned()),
    }
}

/// Fail the row unless `actual` equals `expected`.
///
/// # Errors
///
/// Both values, named by `what`.
pub fn expect_eq<T: PartialEq + std::fmt::Debug>(what: &str, actual: T, expected: T) -> Row {
    match actual == expected {
        true => Ok(()),
        false => Err(format!("{what}: expected {expected:?}, got {actual:?}")),
    }
}

/// Drive `future` on the calling thread's runtime under the hang guard
/// `bound`; never a timing assertion.
///
/// # Errors
///
/// Names `what` when the guard passes first.
pub fn bounded<F: IntoFuture>(what: &str, bound: Duration, future: F) -> Result<F::Output, String> {
    runtime::block_on(tokio::time::timeout(bound, future.into_future()))
        .map_err(|_| format!("{what} did not finish within {bound:?}"))
}

/// [`bounded`] under [`ROW_BOUND`].
///
/// # Errors
///
/// Names `what` when the guard passes first.
pub fn row_bounded<F: IntoFuture>(what: &str, future: F) -> Result<F::Output, String> {
    bounded(what, ROW_BOUND, future)
}

/// Run `future` under the hang guard `bound` and require it to succeed.
///
/// # Errors
///
/// Names `what` when the guard passes first, or the error `future` answered.
pub fn settled_within<T>(
    what: &str,
    bound: Duration,
    future: impl IntoFuture<Output = Result<T, RuntimeError>>,
) -> Result<T, String> {
    expect_ok(what, bounded(what, bound, future)?)
}

/// [`settled_within`] under [`ROW_BOUND`].
///
/// # Errors
///
/// Names `what` when the guard passes first, or the error `future` answered.
pub fn settled<T>(
    what: &str,
    future: impl IntoFuture<Output = Result<T, RuntimeError>>,
) -> Result<T, String> {
    settled_within(what, ROW_BOUND, future)
}

/// Wait under the hang guard `bound` for the signal `signal` carries.
///
/// # Errors
///
/// Names `what` when the guard passes first, or when the sender was dropped
/// without signalling.
pub fn await_signal(what: &str, bound: Duration, signal: oneshot::Receiver<()>) -> Row {
    bounded(what, bound, signal)?
        .map_err(|_| format!("{what}: the sender was dropped without signalling"))
}

/// Hold `count` live slots under `kind`; the probes free them when dropped.
///
/// # Errors
///
/// When the runtime refuses a slot before `count` are held.
pub fn hold_live_slots(
    count: usize,
    kind: IntegrationKind,
) -> Result<Box<[IntegrationProbeHandle]>, String> {
    (0..count)
        .map(|_| IntegrationLifecycleProbe::admit(kind))
        .collect::<Result<Box<[_]>, _>>()
        .map_err(|error| format!("a live slot was refused early: {error:?}"))
}

/// A controlled operation that succeeds as soon as it is polled.
pub async fn instant_work() -> Result<(), IntegrationError> {
    Ok(())
}

/// A controlled operation: pending until the row sends on the returned sender.
pub fn held_work() -> (
    oneshot::Sender<()>,
    impl Future<Output = Result<(), IntegrationError>> + Send + 'static,
) {
    let (release, released) = oneshot::channel::<()>();
    let work = async move {
        // A dropped sender releases the work too, so an unwinding row cannot
        // park the teardown that follows it.
        let _released = released.await;
        Ok(())
    };
    (release, work)
}

/// Take the next message's payload from `subscription` under [`ROW_BOUND`].
///
/// # Errors
///
/// When the subscription completed or failed first, or the guard passed.
#[cfg(feature = "nats")]
pub fn next_payload(
    subscription: &mut camber::mq::nats::Subscription,
) -> Result<Box<[u8]>, String> {
    match row_bounded("the next message", subscription.next())? {
        Ok(Some(message)) => Ok(message.payload().into()),
        Ok(None) => Err("the subscription completed before a message".to_owned()),
        Err(error) => Err(format!("receive: {error:?}")),
    }
}

/// The instance a NATS connection was admitted as, read from the typed
/// refusal of a publish the SDK refuses: `Rejected` with room in the report
/// budget, `Busy` without it. Neither sends anything.
///
/// # Errors
///
/// When the publish hung, succeeded, or named no instance.
#[cfg(feature = "nats")]
pub fn nats_instance(connection: &camber::mq::nats::Connection) -> Result<u64, String> {
    instance_of(
        "the identifying publish",
        row_bounded(
            "an identifying publish",
            connection.publish(INVALID_NATS_SUBJECT, b"x"),
        )?,
    )
}

/// The typed refusal `error` carries, if it is an integration failure.
#[must_use]
pub fn refusal(error: &RuntimeError) -> Option<Refusal> {
    match error {
        RuntimeError::Integration(failure) => Some((
            failure.operation(),
            failure.failure(),
            failure.retryability(),
        )),
        _ => None,
    }
}

/// Whether `error` refuses configuration: a configuration or argument error,
/// or an integration's typed `InvalidConfig` refusal.
#[must_use]
pub fn is_configuration_refusal(error: &RuntimeError) -> bool {
    match error {
        RuntimeError::Config(_) | RuntimeError::InvalidArgument(_) => true,
        RuntimeError::Integration(failure) => {
            failure.failure() == IntegrationFailure::InvalidConfig
        }
        _ => false,
    }
}

/// A fresh temporary directory, removed when dropped.
///
/// # Errors
///
/// When the directory cannot be created.
pub fn tempdir() -> Result<tempfile::TempDir, String> {
    tempfile::TempDir::new().map_err(|error| format!("tempdir: {error}"))
}

/// The typed refusal `outcome` failed with, if any.
#[must_use]
pub fn refused<T>(outcome: Result<T, RuntimeError>) -> Option<Refusal> {
    outcome.err().as_ref().and_then(refusal)
}

/// `operation` refused at its limit, safe to repeat.
#[must_use]
pub fn busy(operation: IntegrationOperation) -> Refusal {
    (operation, IntegrationFailure::Busy, Retryability::Safe)
}

/// `operation` refused after close committed, never retryable.
#[must_use]
pub fn closed(operation: IntegrationOperation) -> Refusal {
    (operation, IntegrationFailure::Closed, Retryability::Never)
}

/// `operation` submitted with an outcome nobody can know.
#[must_use]
pub fn unknown(operation: IntegrationOperation) -> Refusal {
    (
        operation,
        IntegrationFailure::OutcomeUnknown,
        Retryability::OutcomeUnknown,
    )
}

/// `operation` cut after submission: its outcome is unknown.
#[must_use]
pub fn cancelled(operation: IntegrationOperation) -> Refusal {
    (
        operation,
        IntegrationFailure::Cancelled,
        Retryability::OutcomeUnknown,
    )
}

/// `operation` past a size or count maximum, never retryable.
#[must_use]
pub fn limit_exceeded(operation: IntegrationOperation) -> Refusal {
    (
        operation,
        IntegrationFailure::LimitExceeded,
        Retryability::Never,
    )
}

/// `operation` the service or provider rejected, never retryable.
#[must_use]
pub const fn rejected(operation: IntegrationOperation) -> Refusal {
    (operation, IntegrationFailure::Rejected, Retryability::Never)
}

/// `operation` past its bound, never retryable.
#[must_use]
pub const fn timed_out(operation: IntegrationOperation) -> Refusal {
    (operation, IntegrationFailure::Timeout, Retryability::Never)
}

/// `operation` could not reach its peer, safe to repeat: nothing was sent.
#[must_use]
pub const fn unavailable(operation: IntegrationOperation) -> Refusal {
    (
        operation,
        IntegrationFailure::Unavailable,
        Retryability::Safe,
    )
}

/// `operation` past its bound before it submitted, safe to repeat.
#[must_use]
pub const fn expired(operation: IntegrationOperation) -> Refusal {
    (operation, IntegrationFailure::Timeout, Retryability::Safe)
}

/// `operation` refused for its configuration, never retryable.
#[must_use]
pub const fn invalid_config(operation: IntegrationOperation) -> Refusal {
    (
        operation,
        IntegrationFailure::InvalidConfig,
        Retryability::Never,
    )
}

/// `operation` the service denied, never retryable.
#[must_use]
pub fn permission_denied(operation: IntegrationOperation) -> Refusal {
    (
        operation,
        IntegrationFailure::PermissionDenied,
        Retryability::Never,
    )
}

/// Fail the row unless `outcome` is the typed refusal `expected`.
///
/// # Errors
///
/// The success or the other error `outcome` held.
pub fn expect_refused<T>(what: &str, outcome: Result<T, RuntimeError>, expected: Refusal) -> Row {
    match outcome {
        Ok(_) => Err(format!("{what}: succeeded, expected {expected:?}")),
        Err(error) => expect_eq(what, refusal(&error), Some(expected)),
    }
}

/// Fail the row unless `outcome` succeeded.
///
/// # Errors
///
/// The error `outcome` held, named by `what`.
pub fn expect_ok<T>(what: &str, outcome: Result<T, RuntimeError>) -> Result<T, String> {
    outcome.map_err(|error| format!("{what}: {error:?}"))
}

/// Fold a runtime run into the row: the closure's verdict and value, or the
/// teardown error it could not see.
///
/// # Errors
///
/// The closure's own failure, or the error the runtime tore down with.
pub fn clean_run<T>(outcome: Result<Result<T, String>, RuntimeError>) -> Result<T, String> {
    outcome.unwrap_or_else(|error| Err(format!("the runtime tore down with {error:?}")))
}

/// Fail the row unless the run tore down with an aggregate `check` accepts;
/// a clean run fails as the closure's own failure, else as `missing`.
///
/// # Errors
///
/// The closure's failure or `missing`, or what `check` rejected.
pub fn expect_failed_run(
    outcome: (Option<Row>, Result<(), RuntimeError>),
    missing: &str,
    check: impl FnOnce(&RuntimeError) -> Row,
) -> Row {
    observed_verdict(outcome.0)?;
    match outcome.1 {
        Ok(()) => Err(missing.to_owned()),
        Err(error) => check(&error),
    }
}

/// Preserve the closure's observations even when teardown returns an aggregate.
pub fn run_observing<T>(
    builder: RuntimeBuilder,
    body: impl FnOnce() -> T,
) -> (Option<T>, Result<(), RuntimeError>) {
    let mut observed = None;
    let teardown = builder.run(|| observed = Some(body()));
    (observed, teardown)
}

/// The closure's own verdict, whatever the teardown answered.
///
/// # Errors
///
/// The closure's failure, or that the runtime never ran it.
pub fn observed_verdict<T>(observed: Option<Result<T, String>>) -> Result<T, String> {
    observed.unwrap_or_else(|| Err("the runtime never ran the closure".to_owned()))
}

/// Drive `future` on a private Tokio runtime, outside every Camber runtime.
///
/// # Errors
///
/// When the Tokio runtime cannot be built.
pub fn on_tokio<F: Future>(future: F) -> Result<F::Output, String> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map(|tokio| tokio.block_on(future))
        .map_err(|error| format!("tokio runtime: {error}"))
}

/// Fail the row unless `answer` is the error `expected` picks, one outside
/// the integration vocabulary.
///
/// # Errors
///
/// The success or the other error `answer` held.
pub fn expect_runtime_refusal<T>(
    what: &str,
    answer: Result<T, RuntimeError>,
    expected: fn(&RuntimeError) -> bool,
) -> Row {
    match answer {
        Err(error) if expected(&error) => Ok(()),
        other => Err(format!("{what} answered {:?}", other.map(drop))),
    }
}

/// Fail the row unless `answer` is the refusal outside any Camber runtime.
///
/// # Errors
///
/// The success or the other error `answer` held.
pub fn expect_no_runtime<T>(what: &str, answer: Result<T, RuntimeError>) -> Row {
    expect_runtime_refusal(what, answer, |error| {
        matches!(error, RuntimeError::NoRuntime)
    })
}

/// Fail the row unless `answer` is the refusal after root admission closed.
///
/// # Errors
///
/// The success or the other error `answer` held.
pub fn expect_scope_closed<T>(what: &str, answer: Result<T, RuntimeError>) -> Row {
    expect_runtime_refusal(what, answer, |error| {
        matches!(error, RuntimeError::ScopeClosed)
    })
}

/// Poll `operation` once on the runtime thread and require it still pending.
///
/// # Errors
///
/// When the first poll finished it.
pub fn expect_pending<F: Future + ?Sized>(what: &str, operation: Pin<&mut F>) -> Row {
    let first = runtime::block_on(async { futures_util::poll!(operation) });
    expect_polled_pending(what, &first)
}

/// Require the first poll an async row already took to be pending.
///
/// # Errors
///
/// When the first poll finished the operation.
pub fn expect_polled_pending<T>(what: &str, first: &Poll<T>) -> Row {
    expect(&format!("{what} finished at once"), first.is_pending())
}

/// Every `kind` failure in a lifecycle aggregate, as instance, operation,
/// failure, and retryability, in report order.
///
/// # Errors
///
/// When `error` is not a lifecycle aggregate.
pub fn integration_aggregate(
    error: &RuntimeError,
    kind: IntegrationKind,
) -> Result<Vec<(u64, Refusal)>, String> {
    Ok(integration_accounts(error)?
        .filter(|(owner, _, _)| *owner == kind)
        .filter_map(|(_, id, cause)| refusal(cause).map(|refused| (id, refused)))
        .collect())
}

/// Every integration operation failure in a lifecycle aggregate, as the kind
/// and instance its account was charged to and the error it carried, in
/// report order.
///
/// # Errors
///
/// When `error` is not a lifecycle aggregate.
pub fn integration_accounts(
    error: &RuntimeError,
) -> Result<impl Iterator<Item = (IntegrationKind, u64, &RuntimeError)>, String> {
    let RuntimeError::Lifecycle(failures) = error else {
        return Err(format!("the runtime returned {error:?}, not an aggregate"));
    };
    Ok(failures.iter().filter_map(|failure| {
        let LifecycleParticipant::Integration { kind, id } = *failure.participant() else {
            return None;
        };
        let LifecycleFailureKind::Operation(cause) = failure.kind() else {
            return None;
        };
        Some((kind, id, cause.as_ref()))
    }))
}

/// Fail the row unless the aggregate `error` holds exactly `expected`
/// failures of `kind`, in order.
///
/// # Errors
///
/// When `error` is no aggregate, or its `kind` failures differ.
pub fn expect_aggregate(error: &RuntimeError, kind: IntegrationKind, expected: &[Refusal]) -> Row {
    let held: Vec<Refusal> = integration_aggregate(error, kind)?
        .into_iter()
        .map(|(_, held)| held)
        .collect();
    expect_eq(&format!("the {kind} aggregate"), held.as_slice(), expected)
}

/// Fail the row unless every retained `kind` failure in the aggregate `error`
/// names the instance its account was charged to.
///
/// # Errors
///
/// When `error` is no aggregate, or an account names another instance.
pub fn expect_own_instances(error: &RuntimeError, kind: IntegrationKind) -> Row {
    let strays: Vec<(u64, Option<u64>)> = integration_accounts(error)?
        .filter_map(|(owner, id, cause)| {
            let RuntimeError::Integration(integration) = cause else {
                return None;
            };
            let named = integration.instance_id();
            (owner == kind && named != Some(id)).then_some((id, named))
        })
        .collect();
    expect_eq(
        &format!("{kind} accounts whose failure names another instance"),
        strays,
        Vec::new(),
    )
}

/// The instance an integration was admitted as, read from the typed
/// refusal `outcome` answered.
///
/// # Errors
///
/// When `outcome` succeeded, failed outside the integration vocabulary, or
/// named no instance.
pub fn instance_of<T>(what: &str, outcome: Result<T, RuntimeError>) -> Result<u64, String> {
    match outcome {
        Err(RuntimeError::Integration(failure)) => failure
            .instance_id()
            .ok_or_else(|| format!("{what}: the refusal named no instance")),
        other => Err(format!("{what} answered {:?}", other.map(drop))),
    }
}

/// Retry `attempt` while it is refused as `busy(operation)`, under the hang
/// guard `bound`: the operation that held the slot has ended once one is
/// admitted.
///
/// # Errors
///
/// Any other refusal, or a slot still held when `bound` passes.
pub async fn integration_admitted_after<F, Fut, T>(
    operation: IntegrationOperation,
    bound: Duration,
    mut attempt: F,
) -> Row
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, RuntimeError>>,
{
    tokio::time::timeout(bound, async {
        loop {
            match attempt().await {
                Ok(_) => return Ok(()),
                Err(error) if refusal(&error) == Some(busy(operation)) => {
                    tokio::task::yield_now().await;
                }
                Err(error) => return Err(format!("{operation} after the held slot: {error:?}")),
            }
        }
    })
    .await
    .map_err(|_| {
        format!("{operation} stayed busy past {bound:?}: the earlier operation kept its slot")
    })?
}
