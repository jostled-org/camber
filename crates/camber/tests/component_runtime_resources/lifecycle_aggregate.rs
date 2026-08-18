//! 16.T2: the closed lifecycle-failure vocabulary and its deterministic
//! aggregate.
//!
//! Every aggregate here is built through the production log a coordinator
//! records into and read back through the public `RuntimeError::Lifecycle` a
//! caller matches on. A test that assembled its own `LifecycleFailures` would
//! prove its own ordering, not the one teardown returns.

use camber::__private::LifecycleFailureLog;
use camber::http::DeadlineBoundary;
use camber::{
    LifecycleFailure, LifecycleFailureKind, LifecycleFailures, LifecycleParticipant,
    LifecyclePhase, ResourceFailure, ResourceFailureKind, ResourcePhase, RuntimeError,
};
use std::sync::Arc;

/// One recorded lifecycle failure, in the order a coordinator hands it over.
type Row = (LifecycleParticipant, LifecyclePhase, LifecycleFailureKind);

fn assert_send_sync<T: Send + Sync + 'static>() {}

/// Freeze rows through the production log and read the aggregate back off the
/// public error a caller receives.
fn aggregate_of(rows: impl IntoIterator<Item = Row>) -> Arc<LifecycleFailures> {
    let mut log = LifecycleFailureLog::new();
    for (participant, phase, kind) in rows {
        log.record(participant, phase, kind);
    }
    match log.into_error() {
        Some(RuntimeError::Lifecycle(failures)) => failures,
        other => panic!("expected RuntimeError::Lifecycle, got {other:?}"),
    }
}

/// Every closed participant, matched without a wildcard.
fn participant_name(participant: &LifecycleParticipant) -> String {
    match participant {
        LifecycleParticipant::RootScope => "root-scope".to_owned(),
        LifecycleParticipant::Server => "server".to_owned(),
        LifecycleParticipant::Connection => "connection".to_owned(),
        LifecycleParticipant::Upgrade => "upgrade".to_owned(),
        LifecycleParticipant::BackgroundTask => "background-task".to_owned(),
        LifecycleParticipant::Resource(name) => format!("resource:{name}"),
        LifecycleParticipant::Exporter => "exporter".to_owned(),
        LifecycleParticipant::Executor => "executor".to_owned(),
    }
}

/// Every closed lifecycle phase, matched without a wildcard.
fn phase_name(phase: LifecyclePhase) -> String {
    match phase {
        LifecyclePhase::Startup => "startup".to_owned(),
        LifecyclePhase::GracefulDrain => "graceful-drain".to_owned(),
        LifecyclePhase::ForcedJoin => "forced-join".to_owned(),
        LifecyclePhase::Resource(resource) => format!("resource:{}", resource_phase_name(resource)),
        LifecyclePhase::Finalize => "finalize".to_owned(),
    }
}

/// Every closed resource phase, matched without a wildcard.
fn resource_phase_name(phase: ResourcePhase) -> &'static str {
    match phase {
        ResourcePhase::StartupHealth => "startup-health",
        ResourcePhase::PeriodicHealth => "periodic-health",
        ResourcePhase::Shutdown => "shutdown",
    }
}

/// Every closed lifecycle failure kind, matched without a wildcard.
fn kind_name(kind: &LifecycleFailureKind) -> &'static str {
    match kind {
        LifecycleFailureKind::DeadlineExceeded(_) => "deadline",
        LifecycleFailureKind::Cancelled => "cancelled",
        LifecycleFailureKind::TaskPanicked(_) => "panicked",
        LifecycleFailureKind::ScopeDrainTimeout { .. } => "scope-drain",
        LifecycleFailureKind::Resource(_) => "resource",
        LifecycleFailureKind::JoinLost(_) => "join-lost",
        LifecycleFailureKind::Operation(_) => "operation",
    }
}

/// Every closed resource failure kind, matched without a wildcard.
fn resource_kind_name(kind: &ResourceFailureKind) -> &'static str {
    match kind {
        ResourceFailureKind::Returned(_) => "returned",
        ResourceFailureKind::Panicked(_) => "panicked",
        ResourceFailureKind::DeadlineExceeded => "deadline",
        ResourceFailureKind::LostWorker => "lost-worker",
        ResourceFailureKind::BlockedByActiveCallback => "blocked",
    }
}

/// How one entry reads for an order assertion: who, in which phase, over what.
fn entry_identity(failure: &LifecycleFailure) -> String {
    format!(
        "{}|{}|{}",
        participant_name(failure.participant()),
        phase_name(failure.phase()),
        kind_name(failure.kind())
    )
}

fn resource_row(name: &str, phase: ResourcePhase, kind: ResourceFailureKind) -> Row {
    (
        LifecycleParticipant::Resource(Arc::from(name)),
        LifecyclePhase::Resource(phase),
        LifecycleFailureKind::Resource(ResourceFailure::new(Arc::from(name), phase, kind)),
    )
}

fn drain_row(participant: LifecycleParticipant, kind: LifecycleFailureKind) -> Row {
    (participant, LifecyclePhase::GracefulDrain, kind)
}

/// The aggregate orders entries by owner, whatever order teardown recorded
/// them in, and keeps recording order inside one owner class.
fn assert_owner_order_is_independent_of_recording_order() {
    let failures = aggregate_of([
        drain_row(
            LifecycleParticipant::Executor,
            LifecycleFailureKind::JoinLost(Arc::from("executor")),
        ),
        resource_row(
            "second",
            ResourcePhase::Shutdown,
            ResourceFailureKind::LostWorker,
        ),
        drain_row(
            LifecycleParticipant::Server,
            LifecycleFailureKind::JoinLost(Arc::from("server-late")),
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
            LifecycleParticipant::Connection,
            LifecycleFailureKind::Cancelled,
        ),
        drain_row(
            LifecycleParticipant::BackgroundTask,
            LifecycleFailureKind::TaskPanicked(Arc::from("child")),
        ),
        drain_row(
            LifecycleParticipant::Upgrade,
            LifecycleFailureKind::Cancelled,
        ),
        drain_row(
            LifecycleParticipant::Server,
            LifecycleFailureKind::JoinLost(Arc::from("server-later")),
        ),
    ]);

    let ordered: Vec<String> = failures.iter().map(entry_identity).collect();
    assert_eq!(
        ordered,
        [
            "root-scope|graceful-drain|scope-drain",
            "server|graceful-drain|join-lost",
            "server|graceful-drain|join-lost",
            "connection|graceful-drain|cancelled",
            "upgrade|graceful-drain|cancelled",
            "background-task|graceful-drain|panicked",
            "resource:second|resource:shutdown|resource",
            "resource:first|resource:startup-health|resource",
            "exporter|graceful-drain|join-lost",
            "executor|graceful-drain|join-lost",
        ]
    );
    assert_eq!(failures.len(), 10);
    assert_eq!(failures.iter().len(), failures.len());

    // Recording order survives inside one owner class: the two servers keep the
    // sequence teardown admitted them in, and the two resources keep the order
    // their phase invoked them in.
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
    assert_eq!(
        join_lost,
        ["server-late", "server-later", "exporter", "executor"]
    );
}

/// Every row of the primary precedence, read off aggregates that each drop the
/// class above.
fn assert_primary_precedence() {
    let deadline = drain_row(
        LifecycleParticipant::Server,
        LifecycleFailureKind::DeadlineExceeded(DeadlineBoundary::AggregateShutdown),
    );
    let cancelled = drain_row(
        LifecycleParticipant::Connection,
        LifecycleFailureKind::Cancelled,
    );
    let panicked = drain_row(
        LifecycleParticipant::BackgroundTask,
        LifecycleFailureKind::TaskPanicked(Arc::from("child unwound")),
    );
    let drained = drain_row(
        LifecycleParticipant::RootScope,
        LifecycleFailureKind::ScopeDrainTimeout { outstanding: 4 },
    );
    let resource = || {
        resource_row(
            "db",
            ResourcePhase::Shutdown,
            ResourceFailureKind::BlockedByActiveCallback,
        )
    };
    let exporter = || {
        drain_row(
            LifecycleParticipant::Exporter,
            LifecycleFailureKind::Operation(Arc::new(RuntimeError::Timeout)),
        )
    };
    let executor = || {
        drain_row(
            LifecycleParticipant::Executor,
            LifecycleFailureKind::Operation(Arc::new(RuntimeError::Cancelled)),
        )
    };

    let every = || {
        [
            executor(),
            exporter(),
            resource(),
            drained.clone(),
            panicked.clone(),
            cancelled.clone(),
            deadline.clone(),
        ]
    };

    // Each row drops the class above it, so the next one down must win.
    let ladder: [(usize, &str); 7] = [
        (0, "deadline"),
        (1, "cancelled"),
        (2, "panicked"),
        (3, "scope-drain"),
        (4, "resource"),
        (5, "operation"),
        (6, "operation"),
    ];
    for (dropped, expected) in ladder {
        let rows: Vec<Row> = every().into_iter().take(7 - dropped).collect();
        let failures = aggregate_of(rows);
        assert_eq!(
            kind_name(failures.primary().kind()),
            expected,
            "primary after dropping {dropped} class(es)"
        );
    }

    // The exporter and the executor are told apart by participant, not kind:
    // both carry `Operation`, and the exporter outranks the executor.
    let both = aggregate_of([executor(), exporter()]);
    assert_eq!(
        participant_name(both.primary().participant()),
        "exporter",
        "exporter outranks executor"
    );
}

/// Within one class the first failure in owner order wins, whatever order it
/// was recorded in.
fn assert_owner_order_breaks_class_ties() {
    let failures = aggregate_of([
        drain_row(
            LifecycleParticipant::Connection,
            LifecycleFailureKind::Cancelled,
        ),
        drain_row(
            LifecycleParticipant::RootScope,
            LifecycleFailureKind::Cancelled,
        ),
    ]);
    assert_eq!(
        participant_name(failures.primary().participant()),
        "root-scope"
    );
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
            LifecycleParticipant::Upgrade,
            LifecycleFailureKind::JoinLost(Arc::from("upgrade")),
        ),
        drain_row(
            LifecycleParticipant::RootScope,
            LifecycleFailureKind::ScopeDrainTimeout { outstanding: 1 },
        ),
    ]);

    // Owner order first: root scope, upgrade, the two resources, exporter. Only
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
        LifecycleParticipant::Server,
        LifecycleFailureKind::DeadlineExceeded(DeadlineBoundary::AggregateShutdown),
    )]);
    let moved = Arc::clone(&failures);
    let joined = std::thread::spawn(move || entry_identity(moved.primary()))
        .join()
        .expect("aggregate reader thread");
    assert_eq!(joined, entry_identity(failures.primary()));

    // The rendered aggregate leads with its primary and states how many owners
    // failed, so one operator line answers both questions.
    let rendered = RuntimeError::Lifecycle(failures).to_string();
    assert!(
        rendered.contains("aggregate_shutdown") && rendered.contains("[1 recorded]"),
        "aggregate display lost its primary or its count: {rendered}"
    );
}

/// 16.T2: the aggregate's shape, order, primary, causes, and ownership are all
/// deterministic and closed to wildcard matching.
#[test]
fn lifecycle_aggregate_accessors_order_and_primary_are_deterministic() {
    assert_owner_order_is_independent_of_recording_order();
    assert_primary_precedence();
    assert_owner_order_breaks_class_ties();
    assert_causes_are_typed_and_closed();
    assert_clean_teardown_has_no_aggregate();
    assert_flat_variants_remain();
    assert_aggregate_ownership();
}
