//! The ordered, bounded resource coordinator: who runs when, what a wait that
//! expired leaves behind, and which failures reach the caller.

use crate::common::{
    Behavior, CallbackLog, OBSERVATION_BOUND, ScriptedResource, TICK, short_resource_budget,
    wait_until,
};
use crate::lifecycle_kinds;

use camber::runtime_test_support::{RuntimeController, runtime_schedule};
use camber::{LifecycleFailure, LifecycleFailureKind, LifecycleParticipant, RuntimeError, runtime};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

/// One list of owned names as the borrowed names an assertion is written in.
///
/// `Box<str>` and `&str` do not compare, and every claim here is about a
/// sequence of names: rendering once is what lets each row state its expected
/// order as a plain literal.
fn names(entries: &[Box<str>]) -> Box<[&str]> {
    entries.iter().map(Box::as_ref).collect()
}

/// Every resource failure the returned aggregate retained, in its own order.
fn aggregate_rows(error: &RuntimeError) -> Box<[Box<str>]> {
    lifecycle_failures(error).iter().map(resource_row).collect()
}

fn lifecycle_failures(error: &RuntimeError) -> &camber::LifecycleFailures {
    match error {
        RuntimeError::Lifecycle(failures) => failures,
        other => panic!("expected a lifecycle aggregate, got {other:?}"),
    }
}

/// One entry, read back through the public accessors alone.
fn resource_row(failure: &LifecycleFailure) -> Box<str> {
    let participant = failure.participant();
    let resource = match failure.kind() {
        LifecycleFailureKind::Resource(resource) => resource,
        other => panic!("expected a resource failure, got {other}"),
    };
    assert_eq!(
        lifecycle_kinds::participant_name(participant),
        format!("resource:{}", resource.name()),
        "the participant and the failure named different resources"
    );
    assert_eq!(
        lifecycle_kinds::phase_name(failure.phase()),
        format!(
            "resource:{}",
            lifecycle_kinds::resource_phase_name(resource.phase())
        ),
        "the lifecycle stage and the resource phase disagreed"
    );
    format!(
        "{}|{}|{}",
        resource.name(),
        lifecycle_kinds::resource_phase_name(resource.phase()),
        lifecycle_kinds::resource_kind_name(resource.kind())
    )
    .into_boxed_str()
}

// ---------------------------------------------------------------------------
// 17.T1
// ---------------------------------------------------------------------------

#[test]
fn resource_callbacks_begin_in_phase_order_and_never_overlap_per_resource() {
    let ordered = ordered_phase_begins();
    let health = ordered.phase("health");
    assert_eq!(
        &*names(&health[..3]),
        ["first", "second", "third"].as_slice(),
        "the readiness pass did not begin in registration order"
    );
    assert_eq!(
        &*names(&health[3..6]),
        ["first", "second", "third"].as_slice(),
        "a periodic pass did not begin in registration order"
    );
    assert_eq!(
        &*names(&ordered.phase("shutdown")),
        ["third", "second", "first"].as_slice(),
        "teardown did not begin in reverse registration order"
    );

    let (held, outcome) = held_callback_blocks_its_successors();
    assert_eq!(
        held.count("held", "health"),
        2,
        "a second callback entered a resource whose first one never returned"
    );
    assert!(
        held.count("steady", "health") >= 3,
        "the coordinator stopped ticking while one resource was held"
    );
    assert_eq!(
        &*names(&aggregate_rows(&outcome)),
        ["held|shutdown|blocked"].as_slice(),
        "a held resource was not reported as unable to enter shutdown"
    );
}

/// Run three ordinary resources through every phase, stopping once a second
/// periodic pass has visited all of them.
fn ordered_phase_begins() -> Arc<CallbackLog> {
    let log = Arc::new(CallbackLog::default());
    runtime::builder()
        .shutdown_timeout(Duration::from_secs(2))
        .health_interval(TICK)
        .resource(ScriptedResource::new("first", &log))
        .resource(ScriptedResource::new("second", &log))
        .resource(ScriptedResource::new("third", &log))
        .run(|| {
            wait_until("a full periodic pass", || log.phase("health").len() >= 6);
            runtime::request_shutdown();
        })
        .unwrap();
    log
}

/// Park one resource's periodic callback and hold it across another tick and
/// through teardown.
fn held_callback_blocks_its_successors() -> (Arc<CallbackLog>, RuntimeError) {
    let log = Arc::new(CallbackLog::default());
    let held = ScriptedResource::new("held", &log).health(Behavior::ParkFrom(2));
    let gate = held.gate();
    let witness = held.drop_witness();

    let outcome = runtime::builder()
        .shutdown_timeout(Duration::from_secs(2))
        .health_interval(TICK)
        .resource_budget(short_resource_budget())
        .resource(held)
        .resource(ScriptedResource::new("steady", &log))
        .run(|| {
            wait_until("two periodic passes past the held callback", || {
                log.count("steady", "health") >= 3
            });
            runtime::request_shutdown();
        })
        .unwrap_err();

    assert!(
        !witness.load(Ordering::Acquire),
        "a resource whose callback never returned was reported as released"
    );
    gate.open();
    wait_until("the released worker to let its resource go", || {
        witness.load(Ordering::Acquire)
    });
    (log, outcome)
}

// ---------------------------------------------------------------------------
// 17.T2
// ---------------------------------------------------------------------------

#[test]
fn timed_out_resource_worker_retains_shared_ownership_without_false_termination() {
    assert_eq!(
        &*names(&timed_out_startup()),
        ["parked|startup-health|deadline"].as_slice(),
        "an abandoned readiness probe was not reported as its own deadline"
    );
    let (log, rows) = timed_out_periodic();
    assert_eq!(
        &*names(&rows),
        ["parked|shutdown|blocked"].as_slice(),
        "teardown entered a resource whose probe still held it"
    );
    assert_eq!(
        log.count("parked", "health"),
        2,
        "more than one worker was abandoned on one resource"
    );
    assert_eq!(
        &*names(&timed_out_shutdown()),
        ["parked|shutdown|deadline"].as_slice(),
        "an abandoned teardown callback was not reported as its own deadline"
    );
}

/// Park the readiness probe, then prove the worker still owns the resource
/// after `run` has returned.
fn timed_out_startup() -> Box<[Box<str>]> {
    let log = Arc::new(CallbackLog::default());
    let parked = ScriptedResource::new("parked", &log).health(Behavior::ParkFrom(1));
    let gate = parked.gate();
    let witness = parked.drop_witness();

    let outcome = runtime::builder()
        .shutdown_timeout(Duration::from_secs(2))
        .resource_budget(short_resource_budget())
        .resource(parked)
        .run(|| ())
        .unwrap_err();

    assert!(
        !witness.load(Ordering::Acquire),
        "the runtime released a resource its worker had not finished with"
    );
    gate.open();
    wait_until("the released worker to let its resource go", || {
        witness.load(Ordering::Acquire)
    });
    aggregate_rows(&outcome)
}

/// Park a periodic probe and let teardown find the resource still held.
fn timed_out_periodic() -> (Arc<CallbackLog>, Box<[Box<str>]>) {
    let log = Arc::new(CallbackLog::default());
    let (parked_tx, parked_rx) = sync_channel(1);
    let parked = ScriptedResource::new("parked", &log)
        .health(Behavior::ParkFrom(2))
        .reports_parking(parked_tx);
    let gate = parked.gate();
    let witness = parked.drop_witness();

    let outcome = runtime::builder()
        .shutdown_timeout(Duration::from_secs(2))
        .health_interval(TICK)
        .resource_budget(short_resource_budget())
        .resource(parked)
        .run(|| {
            parked_rx.recv_timeout(OBSERVATION_BOUND).unwrap();
            runtime::request_shutdown();
        })
        .unwrap_err();

    assert!(
        !witness.load(Ordering::Acquire),
        "the runtime released a resource its worker had not finished with"
    );
    gate.open();
    wait_until("the released worker to let its resource go", || {
        witness.load(Ordering::Acquire)
    });
    (log, aggregate_rows(&outcome))
}

/// Park the teardown callback itself.
fn timed_out_shutdown() -> Box<[Box<str>]> {
    let log = Arc::new(CallbackLog::default());
    let parked = ScriptedResource::new("parked", &log).shutdown(Behavior::ParkFrom(1));
    let gate = parked.gate();
    let witness = parked.drop_witness();

    let outcome = runtime::builder()
        .shutdown_timeout(Duration::from_secs(2))
        .resource_budget(short_resource_budget())
        .resource(parked)
        .run(|| runtime::request_shutdown())
        .unwrap_err();

    assert!(
        !witness.load(Ordering::Acquire),
        "the runtime released a resource its worker had not finished with"
    );
    gate.open();
    wait_until("the released worker to let its resource go", || {
        witness.load(Ordering::Acquire)
    });
    aggregate_rows(&outcome)
}

// ---------------------------------------------------------------------------
// 17.T3
// ---------------------------------------------------------------------------

#[test]
fn startup_resource_failures_prevent_admission_and_aggregate_every_cause() {
    let log = Arc::new(CallbackLog::default());
    let served = Arc::new(AtomicBool::new(false));
    let controller = runtime_schedule();
    controller.refuse_resource_worker("lost");

    let parked = ScriptedResource::new("parked", &log).health(Behavior::ParkFrom(1));
    let gate = parked.gate();
    let witness = parked.drop_witness();

    let outcome = refused_startup(&log, &served, &controller, parked);

    assert!(
        !served.load(Ordering::Acquire),
        "the user closure served traffic behind a failed readiness pass"
    );
    assert_eq!(
        &*names(&log.phase("health")),
        ["returning", "panicking", "parked"].as_slice(),
        "the readiness pass skipped or reordered a resource after one failed"
    );
    assert_eq!(
        &*names(&aggregate_rows(&outcome)),
        [
            "returning|startup-health|returned",
            "panicking|startup-health|panicked",
            "parked|startup-health|deadline",
            "lost|startup-health|lost-worker",
        ]
        .as_slice(),
        "the readiness aggregate did not retain every cause in registration order"
    );
    assert_primary(&outcome, "returning");

    gate.open();
    wait_until("the released worker to let its resource go", || {
        witness.load(Ordering::Acquire)
    });
}

/// A resource whose worker was refused never begins, so its own record is the
/// one the coordinator writes; every other resource is visited around it.
fn refused_startup(
    log: &Arc<CallbackLog>,
    served: &Arc<AtomicBool>,
    controller: &RuntimeController,
    parked: ScriptedResource,
) -> RuntimeError {
    let served = Arc::clone(served);
    runtime::builder()
        .with_test_schedule(controller)
        .shutdown_timeout(Duration::from_secs(2))
        .resource_budget(short_resource_budget())
        .resource(ScriptedResource::new("returning", log).health(Behavior::FailFrom(1)))
        .resource(ScriptedResource::new("panicking", log).health(Behavior::PanicFrom(1)))
        .resource(parked)
        .resource(ScriptedResource::new("lost", log))
        .run(move || served.store(true, Ordering::Release))
        .unwrap_err()
}

/// The aggregate's primary names `resource`, read through the public accessor.
fn assert_primary(outcome: &RuntimeError, resource: &str) {
    let primary = lifecycle_failures(outcome).primary();
    assert_eq!(
        lifecycle_kinds::participant_name(primary.participant()),
        format!("resource:{resource}"),
        "the aggregate's primary named the wrong owner: {primary}"
    );
    assert!(
        matches!(primary.participant(), LifecycleParticipant::Resource(name) if &**name == resource),
        "the primary's own identity did not match its rendered name"
    );
}

// ---------------------------------------------------------------------------
// 17.T5
// ---------------------------------------------------------------------------

#[test]
fn shutdown_resource_failures_reach_the_lifecycle_aggregate() {
    let log = Arc::new(CallbackLog::default());
    let controller = runtime_schedule();

    let (probing_tx, probing_rx) = sync_channel(1);
    let blocked = ScriptedResource::new("blocked", &log)
        .health(Behavior::ParkFrom(2))
        .reports_parking(probing_tx);
    let parked = ScriptedResource::new("parked", &log).shutdown(Behavior::ParkFrom(1));
    let gates = [blocked.gate(), parked.gate()];
    let witnesses = [blocked.drop_witness(), parked.drop_witness()];

    let started = Instant::now();
    let outcome = failing_shutdown(&log, &controller, blocked, parked, probing_rx);
    let elapsed = started.elapsed();

    assert_eq!(
        &*names(&log.phase("shutdown")),
        ["neighbor", "parked", "panicking", "returning", "clean"].as_slice(),
        "teardown did not visit every eligible resource in reverse order"
    );
    assert_eq!(
        &*names(&aggregate_rows(&outcome)),
        [
            "blocked|shutdown|blocked",
            "lost|shutdown|lost-worker",
            "parked|shutdown|deadline",
            "panicking|shutdown|panicked",
            "returning|shutdown|returned",
        ]
        .as_slice(),
        "the teardown aggregate did not retain every failure in teardown order"
    );
    assert_primary(&outcome, "blocked");
    assert!(
        elapsed < OBSERVATION_BOUND,
        "the run took {elapsed:?}, so a teardown callback outlived its phase limit"
    );

    for gate in &gates {
        gate.open();
    }
    wait_until("every released worker to let its resource go", || {
        witnesses.iter().all(|w| w.load(Ordering::Acquire))
    });
}

/// Register one resource per teardown outcome and stop once the periodic probe
/// that will block teardown has parked.
///
/// `lost`'s worker is refused only once the run is already serving: a refusal
/// standing from the start would have taken the readiness pass down instead,
/// and this row is about teardown. `neighbor` sits between the refused resource
/// and the blocked one, so its begin proves the reverse-order visit continued
/// past a resource that never entered its callback at all.
fn failing_shutdown(
    log: &Arc<CallbackLog>,
    controller: &RuntimeController,
    blocked: ScriptedResource,
    parked: ScriptedResource,
    probing: Receiver<()>,
) -> RuntimeError {
    runtime::builder()
        .with_test_schedule(controller)
        .shutdown_timeout(Duration::from_secs(2))
        .health_interval(TICK)
        .resource_budget(short_resource_budget())
        .resource(ScriptedResource::new("clean", log))
        .resource(ScriptedResource::new("returning", log).shutdown(Behavior::FailFrom(1)))
        .resource(ScriptedResource::new("panicking", log).shutdown(Behavior::PanicFrom(1)))
        .resource(parked)
        .resource(ScriptedResource::new("lost", log))
        .resource(ScriptedResource::new("neighbor", log))
        .resource(blocked)
        .run(move || {
            probing.recv_timeout(OBSERVATION_BOUND).unwrap();
            controller.refuse_resource_worker("lost");
            runtime::request_shutdown();
        })
        .unwrap_err()
}
