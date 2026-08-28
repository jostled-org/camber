//! 11.T1 and 11.T3: the direct runtime lifecycle account, and the executor
//! fact it refuses to manufacture.
//!
//! 11.T1 owns the collection: a real `RuntimeBuilder::run` fails one owner of
//! every direct class, and both the iteration a caller takes and the rendering
//! an operator reads have to carry all of them. Nothing is elected. The closed
//! vocabulary is exercised beside it through the production log a coordinator
//! records into and read back through the public `RuntimeError::Lifecycle` a
//! caller matches on — a test that assembled its own `LifecycleFailures` would
//! prove its own ordering, not the one teardown returns.
//!
//! 11.T3 owns the executor: Tokio's bounded shutdown is a safety action, so a
//! window that ran out states nothing about an owner. No aggregate entry, no
//! settlement, and no deadline reading may name an executor at all — and the
//! window is still a bound, so the run it ends returns while the child it could
//! not stop is still parked.

use crate::common;
use crate::lifecycle_kinds::{
    aggregate_identities, aggregate_rendering, entry_identity, participant_name, phase_name,
    resource_kind_name, resource_phase_name,
};
use camber::__private::LifecycleFailureLog;
use camber::http::DeadlineBoundary;
use camber::runtime_test_support::{RuntimeController, runtime_schedule};
use camber::{
    LifecycleFailure, LifecycleFailureKind, LifecycleFailures, LifecycleParticipant,
    LifecyclePhase, ResourceFailure, ResourceFailureKind, ResourcePhase, RuntimeError, runtime,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The drain window every live row here configures.
///
/// Short enough that a wedged child reaches it well inside the suite's own
/// bound, and long enough that a cooperating owner is never cut off by it.
const DRAIN_WINDOW: Duration = Duration::from_millis(500);

/// The bound a live row's own rendezvous runs under.
///
/// A hang guard, not a timing assertion: a wait that reaches it fails the row
/// instead of parking the binary.
const ROW_BOUND: Duration = Duration::from_secs(10);

/// The ceiling `run` must return under while a non-preemptible child is still
/// held.
///
/// The other half of Invariant 20: the executor window is a BOUND, so the run it
/// ends has to come back on its own. 11.T3 parks its blocking child for
/// [`ROW_BOUND`], and a runtime that dropped its executor rather than bounding it
/// would wait that park out in full. Half the park sits five seconds clear of
/// both outcomes, so the row states the window ran without turning suite load
/// into a timing failure.
const BOUNDED_RETURN: Duration = Duration::from_secs(5);

/// Every owner name this plan removed from the runtime aggregate contract.
///
/// Read as a forbidden set rather than as an expectation: each of these settles
/// inside the flat server tree, or — for the executor — is an owner Camber gets
/// no acknowledgement from at all.
const OWNERS_OUTSIDE_THE_RUNTIME_AGGREGATE: [&str; 4] =
    ["server", "connection", "upgrade", "executor"];

/// One recorded lifecycle failure, in the order a coordinator hands it over.
type Row = (LifecycleParticipant, LifecyclePhase, LifecycleFailureKind);

fn assert_send_sync<T: Send + Sync + 'static>() {}

/// Freeze rows through the production log and read the aggregate back off the
/// public error a caller receives.
fn aggregate_of(rows: impl IntoIterator<Item = Row>) -> LifecycleFailures {
    let mut log = LifecycleFailureLog::new();
    for (participant, phase, kind) in rows {
        log.record(participant, phase, kind);
    }
    match log.into_error() {
        Some(RuntimeError::Lifecycle(failures)) => failures,
        other => panic!("expected RuntimeError::Lifecycle, got {other:?}"),
    }
}

fn resource_row(name: &str, phase: ResourcePhase, kind: ResourceFailureKind) -> Row {
    (
        LifecycleParticipant::Resource(Arc::from(name)),
        LifecyclePhase::Resource(phase),
        LifecycleFailureKind::Resource(ResourceFailure::new(Arc::from(name), phase, kind)),
    )
}

fn phase_row(
    participant: LifecycleParticipant,
    phase: LifecyclePhase,
    kind: LifecycleFailureKind,
) -> Row {
    (participant, phase, kind)
}

fn drain_row(participant: LifecycleParticipant, kind: LifecycleFailureKind) -> Row {
    phase_row(participant, LifecyclePhase::GracefulDrain, kind)
}

/// One row per owner class, recorded in an order no rendering order would
/// produce, with two background children and two resources to hold recording
/// order inside a class.
fn rows_recorded_out_of_rendering_order() -> [Row; 6] {
    [
        resource_row(
            "second",
            ResourcePhase::Shutdown,
            ResourceFailureKind::LostWorker,
        ),
        drain_row(
            LifecycleParticipant::BackgroundTask,
            LifecycleFailureKind::JoinLost(Arc::from("child-late")),
        ),
        drain_row(
            LifecycleParticipant::RootScope,
            LifecycleFailureKind::ScopeDrainTimeout { outstanding: 2 },
        ),
        resource_row(
            "first",
            ResourcePhase::StartupHealth,
            ResourceFailureKind::DeadlineExceeded,
        ),
        drain_row(
            LifecycleParticipant::Exporter,
            LifecycleFailureKind::JoinLost(Arc::from("exporter")),
        ),
        drain_row(
            LifecycleParticipant::BackgroundTask,
            LifecycleFailureKind::JoinLost(Arc::from("child-later")),
        ),
    ]
}

/// Recording order survives inside one owner class: the two background children
/// keep the sequence teardown admitted them in, and the two resources keep the
/// order their phase invoked them in.
fn assert_recording_order_survives_within_one_class(failures: &LifecycleFailures) {
    let join_lost: Vec<&str> = failures
        .iter()
        .filter_map(|failure| match failure.kind() {
            LifecycleFailureKind::JoinLost(name) => Some(name.as_ref()),
            LifecycleFailureKind::DeadlineExceeded(_)
            | LifecycleFailureKind::Cancelled
            | LifecycleFailureKind::TaskPanicked(_)
            | LifecycleFailureKind::ScopeDrainTimeout { .. }
            | LifecycleFailureKind::Resource(_)
            | LifecycleFailureKind::Operation(_) => None,
        })
        .collect();
    assert_eq!(join_lost, ["child-late", "child-later", "exporter"]);
}

/// The aggregate renders entries in one stable order, whatever order teardown
/// recorded them in, and keeps recording order inside one owner class.
///
/// Reproducible output, not precedence: what this pins is that two identical
/// runs render identically, and every row below reads the whole collection
/// rather than the entry that happens to render first.
fn assert_rendering_order_is_independent_of_recording_order() {
    let failures = aggregate_of(rows_recorded_out_of_rendering_order());

    let ordered: Vec<String> = failures.iter().map(entry_identity).collect();
    assert_eq!(
        ordered,
        [
            "root-scope|graceful-drain|scope-drain",
            "background-task|graceful-drain|join-lost",
            "background-task|graceful-drain|join-lost",
            "resource:second|resource:shutdown|resource",
            "resource:first|resource:startup-health|resource",
            "exporter|graceful-drain|join-lost",
        ]
    );
    assert_eq!(failures.len(), 6);
    assert_eq!(failures.iter().len(), failures.len());

    assert_recording_order_survives_within_one_class(&failures);
}

/// One row for every stage the closed vocabulary spells, recorded in an order
/// no stage rank would produce.
fn rows_covering_every_stage() -> [Row; 7] {
    [
        phase_row(
            LifecycleParticipant::Exporter,
            LifecyclePhase::Finalize,
            LifecycleFailureKind::JoinLost(Arc::from("exporter-finalize")),
        ),
        resource_row(
            "db",
            ResourcePhase::Shutdown,
            ResourceFailureKind::LostWorker,
        ),
        phase_row(
            LifecycleParticipant::RootScope,
            LifecyclePhase::Startup,
            LifecycleFailureKind::Operation(Arc::new(RuntimeError::Database(Box::from(
                "startup probe refused",
            )))),
        ),
        phase_row(
            LifecycleParticipant::BackgroundTask,
            LifecyclePhase::ForcedJoin,
            LifecycleFailureKind::TaskPanicked(Arc::from("child unwound past the deadline")),
        ),
        phase_row(
            LifecycleParticipant::RootScope,
            LifecyclePhase::GracefulDrain,
            LifecycleFailureKind::DeadlineExceeded(DeadlineBoundary::AggregateShutdown),
        ),
        resource_row(
            "cache",
            ResourcePhase::PeriodicHealth,
            ResourceFailureKind::DeadlineExceeded,
        ),
        resource_row(
            "db",
            ResourcePhase::StartupHealth,
            ResourceFailureKind::LostWorker,
        ),
    ]
}

/// The stable rendering order first, and beside each entry the stage text
/// production itself renders — the name an operator reads the failure under.
fn assert_every_stage_is_named_in_rendering_order(failures: &LifecycleFailures) {
    let staged: Vec<(String, String)> = failures
        .iter()
        .map(|failure| (entry_identity(failure), failure.phase().to_string()))
        .collect();
    assert_eq!(
        staged,
        [
            ("root-scope|startup|operation", "startup"),
            ("root-scope|graceful-drain|deadline", "graceful-drain"),
            ("background-task|forced-join|panicked", "forced-join"),
            ("resource:db|resource:shutdown|resource", "shutdown"),
            (
                "resource:cache|resource:periodic-health|resource",
                "periodic-health"
            ),
            (
                "resource:db|resource:startup-health|resource",
                "startup-health"
            ),
            ("exporter|finalize|join-lost", "finalize"),
        ]
        .map(|(identity, stage)| (identity.to_owned(), stage.to_owned()))
    );
}

/// Every stage the closed vocabulary spells is one of the recorded rows, so a
/// stage that stopped being carried is caught here and not by the step that
/// first records it.
fn assert_no_stage_stopped_being_carried(failures: &LifecycleFailures) {
    let mut stages: Vec<String> = failures
        .iter()
        .map(|failure| phase_name(failure.phase()))
        .collect();
    stages.sort();
    stages.dedup();
    assert_eq!(
        stages,
        [
            "finalize",
            "forced-join",
            "graceful-drain",
            "resource:periodic-health",
            "resource:shutdown",
            "resource:startup-health",
            "startup",
        ]
    );
}

/// The rendered failure carries the stage too: an operator line that named no
/// stage would leave the reader unable to tell a refused startup from a
/// teardown that ran out of time.
fn assert_rendered_failures_state_their_stage(failures: &LifecycleFailures) {
    for failure in failures.iter() {
        let rendered = failure.to_string();
        assert!(
            rendered.contains(&failure.phase().to_string()),
            "a rendered failure dropped its stage: {rendered}"
        );
    }
}

/// Every stage a runtime can fail in reaches the aggregate as a recorded row,
/// and each row states the stage under the name production renders it as.
///
/// Steps 17 and 18 record startup refusals, forced joins, and finalize failures
/// against this vocabulary and add no stage of their own, so every stage is
/// carried and named here rather than the first time a coordinator emits one.
fn assert_every_phase_is_recorded_and_rendered() {
    let failures = aggregate_of(rows_covering_every_stage());

    assert_every_stage_is_named_in_rendering_order(&failures);
    assert_no_stage_stopped_being_carried(&failures);
    assert_rendered_failures_state_their_stage(&failures);
}

/// Fail one owner of every direct class inside one real `RuntimeBuilder::run`,
/// and hand back the run's own schedule beside what it returned.
///
/// Three classes, because three are what production can record against: the
/// root scope names the child its drain could not stop, that child names the
/// panic it unwound with, and the registered resource names the callback that
/// refused. The exporter is the fourth direct participant and has no failure
/// path at all — teardown visits and releases it — so the vocabulary rows above
/// are where its name is held rather than here.
///
/// The schedule comes back with the error because the aggregate alone cannot
/// answer every claim about it: its participants are a closed enum, and the
/// owners this contract excludes are ones production only ever names in free
/// form.
fn run_failing_every_direct_owner() -> (RuntimeController, RuntimeError) {
    let log = Arc::new(common::CallbackLog::default());
    let refusing =
        common::ScriptedResource::new("refusing", &log).shutdown(common::Behavior::FailFrom(1));
    let panicked = Arc::new(AtomicBool::new(false));
    let entered = Arc::clone(&panicked);
    let controller = runtime_schedule();

    let failure = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(DRAIN_WINDOW)
        .resource_budget(common::short_resource_budget())
        .resource(refusing)
        .run(move || {
            // Never observes `ScopeClosing`, so the drain has a child it cannot
            // stop and the root scope has something to name.
            drop(camber::spawn_async(async {
                std::future::pending::<()>().await;
            }));
            camber::schedule::every(Duration::from_millis(1), move || {
                entered.store(true, Ordering::SeqCst);
                panic!("direct owner child panic");
            })
            .expect("the panicking child was admitted");
            common::wait_until("the scheduled child to unwind", || {
                panicked.load(Ordering::SeqCst)
            });
        })
        .expect_err("a run that failed three direct owners reported a clean teardown");
    (controller, failure)
}

/// Every direct owner that failed reaches the caller through iteration, and no
/// entry is elected the one to act on.
fn assert_iteration_holds_every_direct_owner(failure: &RuntimeError) {
    let identities = aggregate_identities(failure);
    for expected in [
        "root-scope|graceful-drain|scope-drain",
        "background-task|graceful-drain|panicked",
        "resource:refusing|resource:shutdown|resource",
    ] {
        assert!(
            identities.iter().any(|identity| identity == expected),
            "the account dropped {expected}: {identities:?}"
        );
    }
}

/// The rendering carries every direct owner too, so an operator reading one
/// line is not shown a chosen failure and a count.
fn assert_rendering_holds_every_direct_owner(failure: &RuntimeError) {
    let rendered = aggregate_rendering(failure);
    let identities = aggregate_identities(failure);
    assert!(
        rendered.contains(&format!("[{} recorded]", identities.len())),
        "the rendered account lost its count: {rendered}"
    );
    for entry in ["root-scope", "background-task", "resource refusing"] {
        assert!(
            rendered.contains(entry),
            "the rendered account dropped {entry}: {rendered}"
        );
    }
}

/// Every owner name this run settled, in the free form production wrote it in.
fn settled_owners(controller: &RuntimeController) -> Box<[String]> {
    controller
        .participant_settlements()
        .iter()
        .map(|settlement| settlement.participant().to_owned())
        .collect()
}

/// No owner that settles inside the flat server tree took part in the runtime's
/// own teardown, and neither did the executor.
///
/// Read off the settlement inventory rather than off the account, because the
/// account cannot answer it: a `LifecycleParticipant` is a closed enum of four
/// direct classes, so no aggregate entry could ever spell one of these names.
/// The inventory records whatever name an owner settled under, which is the one
/// place a server-tree owner or the executor would appear if it had joined.
fn assert_no_indirect_owner_reaches_the_aggregate(controller: &RuntimeController) {
    let settled = settled_owners(controller);
    for forbidden in OWNERS_OUTSIDE_THE_RUNTIME_AGGREGATE {
        assert!(
            !settled.iter().any(|owner| owner.starts_with(forbidden)),
            "{forbidden} reached the runtime aggregate: {settled:?}"
        );
    }
}

/// Within one class the entries keep their recording order, whatever order they
/// were handed over in.
fn assert_recording_order_decides_ties() {
    let failures = aggregate_of([
        drain_row(
            LifecycleParticipant::BackgroundTask,
            LifecycleFailureKind::Cancelled,
        ),
        drain_row(
            LifecycleParticipant::RootScope,
            LifecycleFailureKind::Cancelled,
        ),
    ]);
    let ordered: Vec<String> = failures
        .iter()
        .map(|failure| participant_name(failure.participant()))
        .collect();
    assert_eq!(ordered, ["root-scope", "background-task"]);
    assert_eq!(failures.len(), 2);
}

/// A nested typed cause reaches the caller for the two kinds that carry one,
/// and nothing else invents one.
fn assert_causes_are_typed_and_closed() {
    let returned = Arc::new(RuntimeError::Database(Box::from("pool exhausted")));
    let operation = Arc::new(RuntimeError::Timeout);

    let failures = aggregate_of([
        resource_row(
            "db",
            ResourcePhase::StartupHealth,
            ResourceFailureKind::Returned(Arc::clone(&returned)),
        ),
        resource_row(
            "cache",
            ResourcePhase::PeriodicHealth,
            ResourceFailureKind::Panicked(Arc::from("index out of bounds")),
        ),
        drain_row(
            LifecycleParticipant::Exporter,
            LifecycleFailureKind::Operation(Arc::clone(&operation)),
        ),
        drain_row(
            LifecycleParticipant::BackgroundTask,
            LifecycleFailureKind::JoinLost(Arc::from("child")),
        ),
        drain_row(
            LifecycleParticipant::RootScope,
            LifecycleFailureKind::ScopeDrainTimeout { outstanding: 1 },
        ),
    ]);

    // Rendering order first: root scope, the child, the two resources, exporter.
    // Only
    // the nested `Returned` and the `Operation` carry a typed cause; the closed
    // kinds carry their whole account directly and invent nothing.
    let causes: Vec<Option<String>> = failures
        .iter()
        .map(|failure| failure.cause().map(ToString::to_string))
        .collect();
    assert_eq!(
        causes,
        vec![
            None,
            None,
            Some(returned.to_string()),
            None,
            Some(operation.to_string()),
        ]
    );

    // The nested resource failure keeps its own name, phase, kind, and cause.
    let nested: Vec<(String, &str, &str, bool)> = failures
        .iter()
        .filter_map(|failure| match failure.kind() {
            LifecycleFailureKind::Resource(resource) => Some((
                resource.name().to_owned(),
                resource_phase_name(resource.phase()),
                resource_kind_name(resource.kind()),
                resource.cause().is_some(),
            )),
            LifecycleFailureKind::DeadlineExceeded(_)
            | LifecycleFailureKind::Cancelled
            | LifecycleFailureKind::TaskPanicked(_)
            | LifecycleFailureKind::ScopeDrainTimeout { .. }
            | LifecycleFailureKind::JoinLost(_)
            | LifecycleFailureKind::Operation(_) => None,
        })
        .collect();
    assert_eq!(
        nested,
        [
            ("db".to_owned(), "startup-health", "returned", true),
            ("cache".to_owned(), "periodic-health", "panicked", false),
        ]
    );
}

/// A teardown that failed at nothing produces no aggregate at all, so a caller
/// never receives an empty one.
fn assert_clean_teardown_has_no_aggregate() {
    assert!(
        LifecycleFailureLog::new().into_error().is_none(),
        "an empty log must not mint an aggregate"
    );
}

/// The flat variants stay for operations outside aggregate teardown.
fn assert_flat_variants_remain() {
    let flat = [
        RuntimeError::Timeout,
        RuntimeError::Cancelled,
        RuntimeError::TaskPanicked(Box::from("worker")),
        RuntimeError::ScopeDrainTimeout(3),
    ];
    for error in flat {
        let named = match error {
            RuntimeError::Timeout => "timeout",
            RuntimeError::Cancelled => "cancelled",
            RuntimeError::TaskPanicked(_) => "panicked",
            RuntimeError::ScopeDrainTimeout(_) => "scope-drain",
            RuntimeError::Lifecycle(_) => "lifecycle",
            _ => "other",
        };
        assert_ne!(named, "lifecycle", "a flat variant became the aggregate");
        assert_ne!(named, "other", "a flat variant disappeared");
    }
}

/// Every value a caller can retain outlives the teardown that produced it and
/// crosses threads with it.
fn assert_aggregate_ownership() {
    assert_send_sync::<LifecycleFailures>();
    assert_send_sync::<LifecycleFailure>();
    assert_send_sync::<LifecycleFailureKind>();
    assert_send_sync::<LifecycleParticipant>();
    assert_send_sync::<LifecyclePhase>();
    assert_send_sync::<ResourceFailure>();
    assert_send_sync::<ResourceFailureKind>();
    assert_send_sync::<ResourcePhase>();
    assert_send_sync::<RuntimeError>();

    let failures = aggregate_of([drain_row(
        LifecycleParticipant::RootScope,
        LifecycleFailureKind::DeadlineExceeded(DeadlineBoundary::AggregateShutdown),
    )]);
    let moved = failures.clone();
    let joined = std::thread::spawn(move || moved.iter().map(entry_identity).collect::<Vec<_>>())
        .join()
        .expect("aggregate reader thread");
    assert_eq!(
        joined,
        failures.iter().map(entry_identity).collect::<Vec<_>>()
    );

    // The rendered aggregate states how many owners failed and then names every
    // one of them, so one operator line answers both questions without electing
    // an entry.
    let rendered = RuntimeError::Lifecycle(failures).to_string();
    assert!(
        rendered.contains("aggregate_shutdown") && rendered.contains("[1 recorded]"),
        "aggregate display lost its entry or its count: {rendered}"
    );
}

/// 11.T1: a real run reports every direct owner that failed, through iteration
/// and through rendering, and elects none of them.
#[test]
fn runtime_run_reports_every_direct_owner_without_primary() {
    let (controller, failure) = run_failing_every_direct_owner();
    assert_iteration_holds_every_direct_owner(&failure);
    assert_rendering_holds_every_direct_owner(&failure);
    assert_no_indirect_owner_reaches_the_aggregate(&controller);

    assert_rendering_order_is_independent_of_recording_order();
    assert_every_phase_is_recorded_and_rendered();
    assert_recording_order_decides_ties();
    assert_causes_are_typed_and_closed();
    assert_clean_teardown_has_no_aggregate();
    assert_flat_variants_remain();
    assert_aggregate_ownership();
}

/// 11.T3: Tokio's bounded shutdown is a safety action, so the time it took is
/// not a fact about an owner.
///
/// The row wedges a blocking child abort cannot preempt. The drain names it
/// honestly — the root scope could not stop it and the child is outstanding —
/// and then the executor window runs out with that child still held. Nothing
/// about an executor may appear anywhere as a result: not in the account the
/// caller reads, not in the settlement inventory, and not among the owners that
/// read the shared expiry.
///
/// Both halves of the invariant are asserted here. The window still has to be a
/// bound, so the run comes back while the child is parked rather than waiting the
/// park out: an unbounded executor stop would produce the same three absences and
/// prove nothing.
#[test]
fn executor_shutdown_elapsed_time_creates_no_participant_fact() {
    let controller = runtime_schedule();
    let (park_tx, park_rx) = std::sync::mpsc::channel::<()>();

    let started = std::time::Instant::now();
    let outcome = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(DRAIN_WINDOW)
        .run(move || {
            // Blocking and parked: abort cannot preempt it, so it is still held
            // when the executor's own window opens and runs out.
            drop(camber::spawn(move || {
                let _parked = park_rx.recv_timeout(ROW_BOUND);
            }));
        });
    let elapsed = started.elapsed();

    // Read before the aggregate, because this is the half that decides whether
    // the safety window exists at all: the run left the still-parked child
    // behind on a bound rather than joining it.
    assert!(
        elapsed < BOUNDED_RETURN,
        "the run waited the parked child out instead of bounding the executor \
         stop: {elapsed:?}"
    );

    let failure = outcome.expect_err("the wedged blocking child was not reported at all");
    let identities = aggregate_identities(&failure);
    assert!(
        identities
            .iter()
            .any(|identity| identity == "root-scope|graceful-drain|scope-drain"),
        "the drain did not name the owner that could not stop its child: {identities:?}"
    );
    assert!(
        !identities
            .iter()
            .any(|identity| identity.starts_with("executor")),
        "an expired safety window was reported as an executor failure: {identities:?}"
    );

    let settled = settled_owners(&controller);
    assert!(
        !settled.iter().any(|owner| owner == "executor"),
        "an expired safety window settled an owner: {settled:?}"
    );

    let readers: Box<[String]> = controller
        .shutdown_deadline_readings()
        .iter()
        .map(|reading| reading.participant().to_owned())
        .collect();
    assert!(
        !readers.iter().any(|owner| owner == "executor"),
        "the safety window was published as an owner's reading of the shared \
         expiry: {readers:?}"
    );

    drop(park_tx);
}
