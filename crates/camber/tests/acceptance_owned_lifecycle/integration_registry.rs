//! 4.T2: the runtime's integration registry settles every admitted entry
//! before any resource shuts down, under the one aggregate expiry.
//!
//! Each row builds a real runtime and admits controlled integration work
//! through the narrow `IntegrationLifecycleProbe`. It then ends that work
//! through one public path: root return, a graceful stop, a forced stop at the
//! aggregate expiry, the last access handle's drop, the operation waiter's
//! drop, or a panic in the work. The probe decides nothing. The registry
//! commits closing, settles, and transfers its retained failures; each row
//! reads those commits back through the entry's read-only observer, the
//! runtime's settlement inventory, and the returned aggregate.
//!
//! A registered witness resource reads the entry's state from its own shutdown
//! callback. Resources shut down after the registry settles, so that callback
//! must see the entry settled.
//!
//! This is runtime wiring, not broker proof: no row reaches a peer.

use crate::integration_rows::{
    ROW_BOUND, Row, expect, expect_eq, held_work, instant_work, integration_accounts, row_bounded,
    run_rows,
};
use crate::lifecycle_kinds;
use crate::scripted_peer::lock;
use camber::__private::FORCED_JOIN_GRACE;
use camber::runtime_test_support::{
    IntegrationEntryObserver, IntegrationEntryState, IntegrationErrorDriver,
    IntegrationLifecycleProbe, IntegrationProbeHandle, OperationWaiter, ParticipantDisposition,
    RuntimeController, runtime_schedule,
};
use camber::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation,
    LifecycleFailureKind, LifecycleParticipant, Resource, Retryability, RuntimeError, runtime,
};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The aggregate grace every row here configures.
///
/// Long enough that a cooperating entry is never cut off by it, and short
/// enough that the forced row reaches it well inside the suite's own bound.
const AGGREGATE_GRACE: Duration = Duration::from_millis(1_500);

/// The ceiling a forced teardown must return under, measured from the moment
/// its root returned.
///
/// Twice the grace: a teardown that minted a second grace for the registry
/// would reach it, and one that used the single expiry stays clear of it.
const ONE_GRACE_CEILING: Duration = Duration::from_millis(3_000);

/// The name the witness resource registers under.
const WITNESS: &str = "integration-order-witness";

/// The panic payload the panicking operation unwinds with.
const PANIC_PAYLOAD: &str = "integration probe panic";

/// Admit one controlled integration, or fail the row naming the refusal.
fn admitted(kind: IntegrationKind) -> Result<IntegrationProbeHandle, String> {
    IntegrationLifecycleProbe::admit(kind)
        .map_err(|error| format!("the {kind:?} instance was refused: {error:?}"))
}

/// Admit `work` as one operation of `handle`, or fail the row naming the
/// `what` operation's refusal.
fn started<W>(
    handle: &IntegrationProbeHandle,
    what: &str,
    work: W,
) -> Result<OperationWaiter<()>, String>
where
    W: Future<Output = Result<(), IntegrationError>> + Send + 'static,
{
    handle
        .run(work)
        .map_err(|error| format!("the {what} operation was refused: {error:?}"))
}

/// A failure a controlled operation charges to its integration's report
/// history.
pub(crate) fn charged_failure() -> IntegrationError {
    IntegrationErrorDriver::new(
        IntegrationKind::Nats,
        IntegrationOperation::Publish,
        IntegrationFailure::Unavailable,
        Retryability::Safe,
    )
    .build()
}

/// A controlled operation that unwinds as soon as it is polled.
async fn panicking_work() -> Result<(), IntegrationError> {
    panic!("{PANIC_PAYLOAD}")
}

/// Sets its flag when dropped: the witness that production dropped the
/// operation future it was holding.
struct DropWitness(Arc<AtomicBool>);

impl Drop for DropWitness {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A controlled operation that never completes on its own, and the flag its
/// drop sets.
fn endless_work() -> (
    Arc<AtomicBool>,
    impl Future<Output = Result<(), IntegrationError>> + Send + 'static,
) {
    let dropped = Arc::new(AtomicBool::new(false));
    let witness = DropWitness(Arc::clone(&dropped));
    let work = async move {
        let _witness = witness;
        std::future::pending::<Result<(), IntegrationError>>().await
    };
    (dropped, work)
}

/// A resource whose shutdown reads the observed entry's state.
///
/// Registration happens before the entry exists, so the observer arrives
/// through a shared slot once the row admits it.
#[derive(Clone, Default)]
struct OrderWitness {
    observer: Arc<Mutex<Option<IntegrationEntryObserver>>>,
    seen: Arc<Mutex<Option<IntegrationEntryState>>>,
    fails: bool,
}

impl OrderWitness {
    /// A witness whose shutdown also fails, so its entry sits beside the
    /// integration's in the returned aggregate.
    fn failing() -> Self {
        Self {
            fails: true,
            ..Self::default()
        }
    }

    /// Hand the witness the entry it reads at shutdown.
    fn watch(&self, observer: &IntegrationEntryObserver) {
        *lock(&self.observer) = Some(observer.clone());
    }

    /// Fail the row unless the resource's shutdown saw the entry settled.
    fn expect_saw_settled(&self) -> Row {
        let seen = *lock(&self.seen);
        expect_eq(
            "the entry state the resource shutdown read",
            seen,
            Some(IntegrationEntryState::Settled),
        )
    }
}

impl Resource for OrderWitness {
    fn name(&self) -> &str {
        WITNESS
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        let state = lock(&self.observer)
            .as_ref()
            .map(IntegrationEntryObserver::state);
        *lock(&self.seen) = state;
        match self.fails {
            true => Err(RuntimeError::InvalidArgument(
                "witness shutdown refused".into(),
            )),
            false => Ok(()),
        }
    }
}

/// What one row's runtime returned, and what its schedule recorded.
struct RowRun {
    controller: RuntimeController,
    outcome: Result<Row, RuntimeError>,
}

/// Run `body` inside a real runtime carrying the witness resource.
fn run_row(witness: &OrderWitness, body: impl FnOnce() -> Row) -> RowRun {
    let controller = runtime_schedule();
    let outcome = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .resource(witness.clone())
        .run(body);
    RowRun {
        controller,
        outcome,
    }
}

/// The name one integration settles under in the settlement inventory.
fn settled_name(kind: IntegrationKind, id: u64) -> String {
    format!(
        "integration {} {id}",
        lifecycle_kinds::integration_kind_name(kind)
    )
}

/// How the named integration settled, if it settled at all.
fn settlement_of(
    controller: &RuntimeController,
    kind: IntegrationKind,
    id: u64,
) -> Option<ParticipantDisposition> {
    let name = settled_name(kind, id);
    controller
        .participant_settlements()
        .iter()
        .find(|settlement| settlement.participant() == name)
        .map(|settlement| settlement.disposition())
}

/// Fail the row unless the named integration settled with one of `allowed`.
fn expect_settled_as(
    controller: &RuntimeController,
    kind: IntegrationKind,
    id: u64,
    allowed: &[ParticipantDisposition],
) -> Row {
    match settlement_of(controller, kind, id) {
        Some(disposition) if allowed.contains(&disposition) => Ok(()),
        Some(disposition) => Err(format!(
            "{} settled as {}, not one of {allowed:?}",
            settled_name(kind, id),
            disposition.label()
        )),
        None => Err(format!(
            "{} never settled: {:?}",
            settled_name(kind, id),
            settlement_inventory(controller)
        )),
    }
}

/// Every settlement the runtime recorded, as participant and disposition.
fn settlement_inventory(controller: &RuntimeController) -> Vec<String> {
    controller
        .participant_settlements()
        .iter()
        .map(|settlement| {
            format!(
                "{}={}",
                settlement.participant(),
                settlement.disposition().label()
            )
        })
        .collect()
}

/// Fail the row unless the observed entry settled by the time `run` returned.
fn expect_settled_after_run(observer: &IntegrationEntryObserver) -> Row {
    expect_eq(
        "entry state after the run",
        observer.state(),
        IntegrationEntryState::Settled,
    )
}

/// The closure's own verdict, or a failure naming the aggregate a teardown
/// that should have been clean returned.
fn clean_teardown(outcome: &Result<Row, RuntimeError>, what: &str) -> Row {
    match outcome {
        Ok(verdict) => verdict.clone(),
        Err(error) => Err(format!(
            "{what} did not tear down clean: {:?}",
            lifecycle_kinds::aggregate_identities(error)
        )),
    }
}

/// Fail the row unless teardown minted exactly `mints` expiries and every
/// owner that read the shared deadline read that one.
fn expect_one_aggregate_expiry(controller: &RuntimeController, mints: usize) -> Row {
    expect_eq(
        "aggregate deadline mints",
        controller.shutdown_deadline_mints(),
        mints,
    )?;
    let Some(mint) = controller.shutdown_deadline_mint() else {
        return expect_eq(
            "readings with no mint",
            controller.shutdown_deadline_readings().len(),
            0,
        );
    };
    let strays: Vec<String> = controller
        .shutdown_deadline_readings()
        .iter()
        .filter(|reading| reading.expiry() != mint.expiry())
        .map(|reading| reading.participant().to_owned())
        .collect();
    expect(
        &format!("owners read an expiry other than the one minted: {strays:?}"),
        strays.is_empty(),
    )
}

/// Every aggregate entry naming this integration.
fn integration_entries(
    error: &RuntimeError,
    kind: IntegrationKind,
    id: u64,
) -> Vec<LifecycleFailureKind> {
    let participant = LifecycleParticipant::Integration { kind, id };
    lifecycle_kinds::aggregate(error)
        .iter()
        .filter(|failure| *failure.participant() == participant)
        .map(|failure| failure.kind().clone())
        .collect()
}

/// The integration failure each operation entry charged to this integration
/// carries, as instance, operation, and failure class; entries of any other
/// shape are left out.
fn integration_failures(
    error: &RuntimeError,
    kind: IntegrationKind,
    id: u64,
) -> Result<Vec<(Option<u64>, IntegrationOperation, IntegrationFailure)>, String> {
    Ok(integration_accounts(error)?
        .filter(|(owner, instance, _)| *owner == kind && *instance == id)
        .filter_map(|(_, _, cause)| match cause {
            RuntimeError::Integration(failure) => Some((
                failure.instance_id(),
                failure.operation(),
                failure.failure(),
            )),
            _ => None,
        })
        .collect())
}

/// Fail the row unless the run returned no aggregate entry for this
/// integration.
fn expect_not_in_aggregate(
    outcome: &Result<Row, RuntimeError>,
    kind: IntegrationKind,
    id: u64,
) -> Row {
    match outcome {
        Ok(_) => Ok(()),
        Err(error @ RuntimeError::Lifecycle(_)) => {
            let entries = integration_entries(error, kind, id);
            expect(
                &format!(
                    "a settled integration reached the aggregate: {:?}",
                    lifecycle_kinds::aggregate_identities(error)
                ),
                entries.is_empty(),
            )
        }
        Err(other) => Err(format!("the runtime returned {other:?}")),
    }
}

/// The closure's own verdict, or a failure naming the teardown result.
fn closure_verdict(outcome: &Result<Row, RuntimeError>) -> Row {
    match outcome {
        Ok(verdict) => verdict.clone(),
        Err(RuntimeError::Lifecycle(_)) => Ok(()),
        Err(other) => Err(format!("the runtime returned {other:?}")),
    }
}

/// The aggregate a row's teardown returned, once the closure's own verdict
/// passed; `missing` names a run that returned none.
fn returned_aggregate<'a>(
    outcome: &'a Result<Row, RuntimeError>,
    missing: &str,
) -> Result<&'a RuntimeError, String> {
    match outcome {
        Ok(verdict) => {
            verdict.clone()?;
            Err(missing.to_owned())
        }
        Err(error @ RuntimeError::Lifecycle(_)) => Ok(error),
        Err(other) => Err(format!("the runtime returned {other:?}")),
    }
}

/// An admitted entry's identity and observer, read by the row after `run`
/// returns.
type Published = Option<(u64, IntegrationEntryObserver)>;

/// Record one admitted entry where the row reads it after `run` returns.
fn publish(published: &mut Published, handle: &IntegrationProbeHandle) {
    *published = Some((handle.id(), handle.observer()));
}

/// The entry a row's runtime admitted, or a failure naming its absence.
fn published_entry(published: Published) -> Result<(u64, IntegrationEntryObserver), String> {
    published.ok_or_else(|| "the runtime never admitted its entry".to_owned())
}

// ── 4.T2 ──────────────────────────────────────────────────────────────

#[test]
fn aggregate_orders_integration_failures_by_admission() {
    let outcome = runtime::builder().run(|| {
        let first = IntegrationLifecycleProbe::admit(IntegrationKind::Nats).expect("first");
        let second = IntegrationLifecycleProbe::admit(IntegrationKind::Nats).expect("second");
        assert_eq!((first.id(), second.id()), (1, 2));
        for handle in [second, first] {
            let (returning, returned) = std::sync::mpsc::sync_channel(1);
            let operation = handle
                .run(async move {
                    returning.send(()).expect("failure witness");
                    Err(charged_failure())
                })
                .expect("operation");
            returned.recv_timeout(ROW_BOUND).expect("operation ran");
            row_bounded("entry close", handle.close())
                .expect("close settled")
                .expect("close");
            drop(operation);
        }
    });
    let Err(RuntimeError::Lifecycle(failures)) = outcome else {
        panic!("expected retained failures, got {outcome:?}");
    };
    let identities: Vec<_> = failures
        .iter()
        .filter_map(|failure| match failure.participant() {
            LifecycleParticipant::Integration { id, .. } => Some(*id),
            _ => None,
        })
        .collect();
    assert_eq!(
        identities,
        [1, 2],
        "failure order must not determine aggregate order"
    );
}

#[test]
fn root_stop_settles_integrations_before_resources_without_new_grace() {
    run_rows(&[
        (
            "root return starts the registry stop before the root drain",
            root_return_starts_the_registry_stop_before_the_root_drain,
        ),
        (
            "graceful stop lets accepted work finish",
            graceful_stop_lets_accepted_work_finish,
        ),
        (
            "forced stop settles under the one expiry",
            forced_stop_settles_under_the_one_expiry,
        ),
        (
            "forced stop names work it cannot join",
            forced_stop_names_work_it_cannot_join,
        ),
        (
            "last handle drop requests close",
            last_handle_drop_requests_close,
        ),
        (
            "waiter drop cancels and the entry keeps settlement",
            waiter_drop_cancels_and_the_entry_keeps_settlement,
        ),
        (
            "operation panic is settled by the entry",
            operation_panic_is_settled_by_the_entry,
        ),
        (
            "abandoned failure transfers once before resources",
            abandoned_failure_transfers_once_before_resources,
        ),
    ]);
}

/// Root return closes admission and starts the registry stop at once.
///
/// A root-scope child holds an access clone and waits for the entry's closing
/// commit before it releases the held work and exits. Were the registry stop to
/// wait for the root drain, the drain would wait for that child, and the child
/// for the stop: the run would reach its grace and name the root instead of
/// returning clean.
fn root_return_starts_the_registry_stop_before_the_root_drain() -> Row {
    let witness = OrderWitness::default();
    let mut published = Published::default();
    let (report, reported) = std::sync::mpsc::sync_channel::<Row>(1);

    let run = run_row(&witness, || {
        let handle = admitted(IntegrationKind::Nats)?;
        handle
            .ready()
            .map_err(|error| format!("marking the instance ready: {error:?}"))?;
        publish(&mut published, &handle);
        let observer = handle.observer();
        witness.watch(&observer);

        let (release, work) = held_work();
        let operation = started(&handle, "held", work)?;
        let clone = handle.clone();
        drop(camber::spawn_async(async move {
            let stopped = tokio::time::timeout(ROW_BOUND, observer.closing()).await;
            let _released = release.send(());
            let finished = tokio::time::timeout(ROW_BOUND, operation.wait()).await;
            drop(clone);
            let verdict = match (stopped, finished) {
                (Err(_), _) => Err("the registry stop never reached the entry".to_owned()),
                (Ok(()), Ok(Ok(()))) => Ok(()),
                (Ok(()), other) => Err(format!("the accepted work finished as {other:?}")),
            };
            let _sent = report.send(verdict);
        }));
        Ok(())
    });

    clean_teardown(&run.outcome, "root return")?;
    reported
        .recv_timeout(ROW_BOUND)
        .map_err(|_| "the waiting child never reported".to_owned())??;
    let (id, observer) = published_entry(published)?;
    expect_settled_after_run(&observer)?;
    expect_settled_as(
        &run.controller,
        IntegrationKind::Nats,
        id,
        &[ParticipantDisposition::Completed],
    )?;
    expect_one_aggregate_expiry(&run.controller, 1)?;
    witness.expect_saw_settled()
}

/// A graceful stop requested while the root still runs commits the entry's
/// closing, and the work it had already accepted still finishes.
fn graceful_stop_lets_accepted_work_finish() -> Row {
    let witness = OrderWitness::default();
    let mut published = Published::default();

    let run = run_row(&witness, || {
        let handle = admitted(IntegrationKind::Sqs)?;
        publish(&mut published, &handle);
        let observer = handle.observer();
        witness.watch(&observer);

        let (release, work) = held_work();
        let operation = started(&handle, "held", work)?;
        runtime::request_shutdown();
        row_bounded("the graceful stop's closing commit", observer.closing())?;
        let refused = handle.run(instant_work());
        expect(
            &format!(
                "a stopping entry accepted new work: {:?}",
                refused.as_ref().err()
            ),
            refused.is_err(),
        )?;
        let _released = release.send(());
        let finished = row_bounded("the accepted work", operation.wait())?;
        expect_eq(
            "the accepted work",
            finished.map_err(|error| error.to_string()),
            Ok(()),
        )
    });

    clean_teardown(&run.outcome, "the graceful stop")?;
    let (id, observer) = published_entry(published)?;
    expect_settled_after_run(&observer)?;
    expect_settled_as(
        &run.controller,
        IntegrationKind::Sqs,
        id,
        &[ParticipantDisposition::Completed],
    )?;
    expect_one_aggregate_expiry(&run.controller, 1)?;
    witness.expect_saw_settled()
}

/// Work that never finishes is dropped at the one aggregate expiry, and the
/// registry takes no grace of its own.
///
/// The waiter is kept alive and unread past teardown, so no waiter drop
/// cancels the work first: only the forced stop can end it. The entry then
/// settles cancelled-and-joined, or it is named in the returned aggregate.
fn forced_stop_settles_under_the_one_expiry() -> Row {
    let witness = OrderWitness::default();
    let mut published = Published::default();
    let mut dropped = None;
    let mut waiters = Vec::new();
    let mut returned_at = None;

    let run = run_row(&witness, || {
        let handle = admitted(IntegrationKind::Nats)?;
        publish(&mut published, &handle);
        witness.watch(&handle.observer());
        let (flag, work) = endless_work();
        dropped = Some(flag);
        let operation = started(&handle, "endless", work)?;
        waiters.push(operation);
        returned_at = Some(Instant::now());
        Ok(())
    });
    let torn_down_at = Instant::now();

    closure_verdict(&run.outcome)?;
    let root_returned = returned_at.ok_or("the root never returned")?;
    let teardown = torn_down_at.saturating_duration_since(root_returned);
    expect(
        &format!("the forced teardown took {teardown:?}, past one grace"),
        teardown < ONE_GRACE_CEILING,
    )?;
    let flag = dropped.ok_or("the endless work never started")?;
    expect(
        "the forced stop never dropped the endless work",
        flag.load(Ordering::SeqCst),
    )?;
    let (id, observer) = published_entry(published)?;
    expect_settled_after_run(&observer)?;
    expect_settled_as(
        &run.controller,
        IntegrationKind::Nats,
        id,
        &[
            ParticipantDisposition::CancelledAndJoined,
            ParticipantDisposition::Named,
        ],
    )?;
    let named_in_aggregate = match (
        settlement_of(&run.controller, IntegrationKind::Nats, id),
        &run.outcome,
    ) {
        (Some(ParticipantDisposition::Named), Err(error @ RuntimeError::Lifecycle(_))) => {
            !integration_entries(error, IntegrationKind::Nats, id).is_empty()
        }
        (Some(ParticipantDisposition::Named), _) => false,
        _ => true,
    };
    expect(
        "a named integration is missing from the returned aggregate",
        named_in_aggregate,
    )?;
    expect_one_aggregate_expiry(&run.controller, 1)?;
    drop(waiters);
    witness.expect_saw_settled()
}

/// Work that blocks its worker thread outlives the aggregate expiry and the
/// forced-join grace, so the registry names it before any resource shuts down.
///
/// The operation parks its thread on a std receive the row holds the sender
/// for. Aborting it does nothing, so neither the drain nor the forced stop gets
/// it back; only the registry's settlement can end the entry. The row releases
/// the thread after `run` returns, and a row that fails first releases it by
/// dropping the sender.
fn forced_stop_names_work_it_cannot_join() -> Row {
    let witness = OrderWitness::default();
    let mut published = Published::default();
    let (release, released) = std::sync::mpsc::channel::<()>();
    let mut waiters = Vec::new();
    let mut returned_at = None;

    let run = run_row(&witness, || {
        let handle = admitted(IntegrationKind::Nats)?;
        publish(&mut published, &handle);
        witness.watch(&handle.observer());
        let (entering, entered) = std::sync::mpsc::sync_channel::<()>(1);
        let operation = started(&handle, "blocking", async move {
            let _sent = entering.send(());
            let _released = released.recv();
            Ok::<(), IntegrationError>(())
        })?;
        entered
            .recv_timeout(ROW_BOUND)
            .map_err(|_| "the blocking operation never ran".to_owned())?;
        // Unread and alive past teardown, so only teardown ends the entry.
        waiters.push(operation);
        returned_at = Some(Instant::now());
        Ok(())
    });
    let torn_down_at = Instant::now();
    drop(release);
    drop(waiters);

    let root_returned = returned_at.ok_or("the root never returned")?;
    let teardown = torn_down_at.saturating_duration_since(root_returned);
    let ceiling = ONE_GRACE_CEILING + FORCED_JOIN_GRACE;
    expect(
        &format!("the forced teardown took {teardown:?}, past {ceiling:?}"),
        teardown < ceiling,
    )?;
    let error = returned_aggregate(
        &run.outcome,
        "work the forced stop could not join left no aggregate",
    )?;
    let (id, observer) = published_entry(published)?;
    expect_eq(
        "the integration's settlement",
        settlement_of(&run.controller, IntegrationKind::Nats, id),
        Some(ParticipantDisposition::Named),
    )?;
    let entries = integration_entries(error, IntegrationKind::Nats, id);
    expect_eq("entries naming the integration", entries.len(), 1)?;
    expect_eq(
        "the named incomplete close",
        integration_failures(error, IntegrationKind::Nats, id)?,
        vec![(
            Some(id),
            IntegrationOperation::Close,
            IntegrationFailure::Timeout,
        )],
    )?;
    expect_settled_after_run(&observer)?;
    expect_one_aggregate_expiry(&run.controller, 1)?;
    witness.expect_saw_settled()
}

/// Dropping the last access handle requests close, and the entry settles
/// while the runtime is still running.
///
/// One clone still held keeps the entry open; the observer holds no access
/// and cannot keep it open.
fn last_handle_drop_requests_close() -> Row {
    let witness = OrderWitness::default();
    let mut published = Published::default();

    let run = run_row(&witness, || {
        let handle = admitted(IntegrationKind::Dns01)?;
        publish(&mut published, &handle);
        let observer = handle.observer();
        let clone = handle.clone();
        drop(handle);
        let still_open = observer.state();
        expect(
            &format!("dropping one of two handles closed the entry: {still_open:?}"),
            matches!(
                still_open,
                IntegrationEntryState::Admitted | IntegrationEntryState::Ready
            ),
        )?;
        drop(clone);
        row_bounded("the close the last drop requested", observer.closing())?;
        row_bounded("the entry settlement", observer.settled())?;
        expect_eq(
            "entry state before root return",
            observer.state(),
            IntegrationEntryState::Settled,
        )?;
        expect(
            "the runtime was stopping when the entry settled",
            !runtime::is_shutting_down(),
        )
    });

    let (id, _) = published_entry(published)?;
    closure_verdict(&run.outcome)?;
    expect_not_in_aggregate(&run.outcome, IntegrationKind::Dns01, id)?;
    expect(
        &format!("the run returned {:?}", run.outcome.as_ref().err()),
        run.outcome.is_ok(),
    )
}

/// Dropping an operation's waiter cancels that work, while the entry keeps
/// its own settlement and stays open for new work.
fn waiter_drop_cancels_and_the_entry_keeps_settlement() -> Row {
    let witness = OrderWitness::default();
    let mut published = Published::default();

    let run = run_row(&witness, || {
        let handle = admitted(IntegrationKind::Nats)?;
        publish(&mut published, &handle);
        let observer = handle.observer();
        witness.watch(&observer);
        let (dropped, work) = endless_work();
        let operation = started(&handle, "endless", work)?;
        drop(operation);
        let cancelled = crate::common::poll_until(ROW_BOUND, || dropped.load(Ordering::SeqCst));
        expect("the waiter drop never cancelled its work", cancelled)?;
        let state = observer.state();
        expect(
            &format!("a waiter drop closed its entry: {state:?}"),
            matches!(
                state,
                IntegrationEntryState::Admitted | IntegrationEntryState::Ready
            ),
        )?;
        let next = handle
            .run(instant_work())
            .map_err(|error| format!("new work after a waiter drop: {error:?}"))?;
        let finished = row_bounded("new work after a waiter drop", next.wait())?;
        expect_eq(
            "new work after a waiter drop",
            finished.map_err(|error| error.to_string()),
            Ok(()),
        )
    });

    closure_verdict(&run.outcome)?;
    let (id, observer) = published_entry(published)?;
    expect_settled_after_run(&observer)?;
    expect_settled_as(
        &run.controller,
        IntegrationKind::Nats,
        id,
        &[
            ParticipantDisposition::Completed,
            ParticipantDisposition::CancelledAndJoined,
        ],
    )?;
    witness.expect_saw_settled()
}

/// Work that panics unwinds into its waiter, not into the aggregate, and the
/// entry still settles at teardown without deadlock.
fn operation_panic_is_settled_by_the_entry() -> Row {
    let witness = OrderWitness::default();
    let mut published = Published::default();

    let run = run_row(&witness, || {
        let handle = admitted(IntegrationKind::Sqs)?;
        publish(&mut published, &handle);
        witness.watch(&handle.observer());
        let operation = started(&handle, "panicking", panicking_work())?;
        match row_bounded("the panicking operation", operation.wait())? {
            Err(RuntimeError::TaskPanicked(payload)) if payload.contains(PANIC_PAYLOAD) => Ok(()),
            other => Err(format!("the waiter read {other:?}, not the panic")),
        }
    });

    let (id, observer) = published_entry(published)?;
    closure_verdict(&run.outcome)?;
    expect_not_in_aggregate(&run.outcome, IntegrationKind::Sqs, id)?;
    expect_settled_after_run(&observer)?;
    witness.expect_saw_settled()
}

/// A failure its waiter never read leaves through the aggregate exactly once,
/// carrying its integration identity, ahead of every resource entry.
fn abandoned_failure_transfers_once_before_resources() -> Row {
    let witness = OrderWitness::failing();
    let mut published = Published::default();

    let run = run_row(&witness, || {
        let handle = admitted(IntegrationKind::Nats)?;
        publish(&mut published, &handle);
        witness.watch(&handle.observer());
        let (returning, returned) = std::sync::mpsc::sync_channel::<()>(1);
        let operation = started(&handle, "failing", async move {
            let _sent = returning.send(());
            Err::<(), IntegrationError>(charged_failure())
        })?;
        returned
            .recv_timeout(ROW_BOUND)
            .map_err(|_| "the failing operation never ran".to_owned())?;
        // Unread: the failure is published, never delivered.
        drop(operation);
        Ok(())
    });

    let (id, observer) = published_entry(published)?;
    let error = returned_aggregate(
        &run.outcome,
        "an abandoned failure and a failed resource left no aggregate",
    )?;
    let entries = integration_entries(error, IntegrationKind::Nats, id);
    let transferred = integration_failures(error, IntegrationKind::Nats, id)?;
    expect_eq("entries naming the integration", entries.len(), 1)?;
    expect_eq(
        "the transferred failure",
        transferred,
        vec![(
            Some(id),
            IntegrationOperation::Publish,
            IntegrationFailure::Unavailable,
        )],
    )?;

    let identities = lifecycle_kinds::aggregate_identities(error);
    let integration = format!(
        "{}|",
        lifecycle_kinds::participant_name(&LifecycleParticipant::Integration {
            kind: IntegrationKind::Nats,
            id,
        })
    );
    let integration_at = identities
        .iter()
        .position(|identity| identity.starts_with(&integration));
    let resource = format!("resource:{WITNESS}|");
    let resource_at = identities
        .iter()
        .position(|identity| identity.starts_with(&resource));
    match (integration_at, resource_at) {
        (Some(integration_at), Some(resource_at)) if integration_at < resource_at => {}
        _ => {
            return Err(format!(
                "the integration entry does not precede the resource entry: {identities:?}"
            ));
        }
    }
    expect_settled_after_run(&observer)?;
    witness.expect_saw_settled()
}
