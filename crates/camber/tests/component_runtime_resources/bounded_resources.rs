//! The ordered, bounded resource coordinator: who runs when, what a wait that
//! expired leaves behind, and which failures reach the caller.

use crate::common::{
    Behavior, CallbackLog, OBSERVATION_BOUND, ScriptedResource, TICK, short_resource_budget,
    wait_until,
};
use crate::lifecycle_kinds;

use camber::runtime_test_support::{RuntimeController, runtime_schedule};
use camber::{LifecycleFailure, LifecycleFailureKind, LifecycleParticipant, RuntimeError, runtime};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
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
    lifecycle_kinds::aggregate(error)
        .iter()
        .map(resource_row)
        .collect()
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
        .expect_err("a resource that could not enter teardown was not reported");

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
        .expect_err("an abandoned readiness probe was not reported");

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
        .expect_err("teardown found an abandoned probe's resource free to enter");

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
        .expect_err("an abandoned teardown callback was not reported");

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
    let listener = ListenerWitness::bound();

    let result = refused_startup(&log, &served, &controller, parked, listener.share());

    // Admission first: it is the half of this row's claim a run that answered
    // its peer has already broken, whatever it went on to return.
    listener.assert_unadmitted();
    assert!(
        !served.load(Ordering::Acquire),
        "the user closure served traffic behind a failed readiness pass"
    );
    let outcome = result.expect_err("a failed readiness pass let the run report success");
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
    listener: Arc<TcpListener>,
) -> Result<(), RuntimeError> {
    let served = Arc::clone(served);
    runtime::builder()
        .with_test_schedule(controller)
        .shutdown_timeout(Duration::from_secs(2))
        .resource_budget(short_resource_budget())
        .resource(ScriptedResource::new("returning", log).health(Behavior::FailFrom(1)))
        .resource(ScriptedResource::new("panicking", log).health(Behavior::PanicFrom(1)))
        .resource(parked)
        .resource(ScriptedResource::new("lost", log))
        .run(move || {
            served.store(true, Ordering::Release);
            admit_and_answer(&listener);
        })
}

/// Accept the peer already queued on the witness listener and answer it.
///
/// The one thing in this fixture that can admit traffic. It returns at once
/// either way: the peer connected before the run, so a closure that reaches
/// this has a connection waiting rather than a wait with no end.
fn admit_and_answer(listener: &TcpListener) {
    if let Ok((mut peer, _)) = listener.accept() {
        drop(peer.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n"));
    }
}

/// How long the queued peer is given to be answered before the row calls it
/// unanswered.
const UNSERVED_BOUND: Duration = Duration::from_millis(250);

/// The listener the user closure would have served on, with one real peer
/// already queued on it.
///
/// The peer connects BEFORE the run, so admission is observed rather than
/// inferred: a closure that gets to serve accepts a connection that is already
/// waiting and answers it. Nothing here reserves a port by binding and
/// rebinding — one listener is bound once, shared with the closure, and
/// outlived by the row that owns it.
struct ListenerWitness {
    listener: Arc<TcpListener>,
    peer: TcpStream,
}

impl ListenerWitness {
    fn bound() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind the witness listener");
        let addr = listener
            .local_addr()
            .expect("the witness listener's address");
        let peer = TcpStream::connect(addr).expect("queue a peer on the witness listener");
        Self {
            listener: Arc::new(listener),
            peer,
        }
    }

    /// The handle the user closure would serve through.
    fn share(&self) -> Arc<TcpListener> {
        Arc::clone(&self.listener)
    }

    /// The queued peer was never answered, and its connection is still waiting
    /// to be accepted.
    ///
    /// Two readings, because either alone is weaker than the claim: an
    /// unanswered peer could have been accepted and abandoned, and a connection
    /// still in the queue says nothing about what a peer received.
    fn assert_unadmitted(mut self) {
        self.peer
            .set_read_timeout(Some(UNSERVED_BOUND))
            .expect("bound the witness peer's read");
        let mut answer = [0_u8; 1];
        let read = self.peer.read(&mut answer);
        assert!(
            !matches!(read, Ok(1..)),
            "a peer queued on the witness listener was answered behind a failed \
             readiness pass: {read:?}"
        );
        self.listener
            .set_nonblocking(true)
            .expect("read the witness listener's accept queue");
        assert!(
            self.listener.accept().is_ok(),
            "the queued connection was admitted behind a failed readiness pass"
        );
    }
}

/// The aggregate's primary names `resource`, read through the public accessor.
fn assert_primary(outcome: &RuntimeError, resource: &str) {
    let primary = lifecycle_kinds::aggregate_primary(outcome);
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
        .expect_err("a failing teardown pass reported the run as clean")
}
