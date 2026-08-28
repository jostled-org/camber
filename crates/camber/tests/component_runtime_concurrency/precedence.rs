use crate::common::{
    ArmedWatch, BOUND, NamedSubject, RecordingResource, SHORT_DRAIN, WedgedHandle,
    assert_forced_abort, block_on_detached, ignore_hook, join_bounded, observe_armed_sequence,
    observe_armed_window, reached,
};
use crate::lifecycle_kinds;
use crate::scope_builders::{probed_runtime, scope_runtime};
use camber::RuntimeError;
use camber::runtime_test_support::{
    AdmittedScope, RuntimeCheckpoint, RuntimeController, wait_scope_closing,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Slack over the one drain boundary this file still measures, covering
/// timer-wheel granularity and scheduling jitter on the reference environment
/// (macOS/Linux CI runners, debug profile, tests running in parallel).
///
/// It never approaches the alternative that assertion discriminates against: a
/// drain that waited out the parked blocking child would take `BLOCKING_PARK`
/// (4 s).
const SLACK: Duration = Duration::from_millis(1500);
/// A blocking park far longer than the drain window, so the drain provably
/// proceeded past it rather than outlasting it.
const BLOCKING_PARK: Duration = Duration::from_secs(4);
/// The drain window in which exactly one child — the case's own — is still
/// outstanding.
const AWAITING_ONE: RuntimeCheckpoint = RuntimeCheckpoint::ScopeWaitObserved(1);
/// The drain's terminal observation, taken after the forced join.
const DRAINED: RuntimeCheckpoint = RuntimeCheckpoint::ScopeWaitObserved(0);

/// One named child's join acknowledgment and whether its owner still holds it,
/// read as one observation so the pair is always taken at the same instant.
///
/// Both readings take the sentinel [`reached`] carries: a failed probe must
/// still release the window it paused, or the runtime never returns.
fn read_child(child: &AdmittedScope) -> (bool, bool) {
    (
        reached(child, AdmittedScope::joined),
        reached(child, AdmittedScope::retained),
    )
}

/// Run a runtime, sampling the named child at the drain's terminal
/// observation.
///
/// That checkpoint is the last point at which the runtime is still alive to
/// read: once `run` returns, the scope is gone and a probe has nothing to
/// name. `None` reports a drain that never reached that observation.
fn observe_drain_end<F, T>(
    shutdown_timeout: Duration,
    body: F,
) -> (Result<T, RuntimeError>, Option<(bool, bool)>)
where
    F: FnOnce() -> T,
{
    let named = NamedSubject::new();
    let body_named = named.clone();
    observe_armed_window(
        |builder| builder.shutdown_timeout(shutdown_timeout),
        DRAINED,
        move |controller: &RuntimeController| {
            // Named before the body admits anything, so the reading below is
            // about the child this case started.
            body_named.name_next(controller);
            body()
        },
        move |_| read_child(named.get("the drain-end probe")),
    )
}

/// A child that observes `ScopeClosing` and exits is awaited to completion by
/// the drain, and the drain leaves the runtime result alone.
#[test]
fn cooperative_child_is_awaited_within_bounded_drain() {
    const VALUE: u32 = 7;

    let completed = Arc::new(AtomicBool::new(false));
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

    let child_completed = Arc::clone(&completed);
    let observer_completed = Arc::clone(&completed);

    let (value, (awaited, released, completed_at_zero)) = observe_armed_sequence(
        |builder| builder.shutdown_timeout(BOUND),
        |gate| {
            camber::spawn_async(async move {
                wait_scope_closing().await;
                // Records the release it actually received: a child that ran
                // on because its sender was dropped would report a completion
                // the drain never awaited.
                child_completed.store(release_rx.await.is_ok(), Ordering::SeqCst);
            });
            gate.arm(AWAITING_ONE);
            VALUE
        },
        |watch| observe_cooperative_drain(watch, release_tx, observer_completed),
    );

    assert!(
        awaited,
        "the drain never paused while awaiting its cooperative child"
    );
    assert!(released, "the cooperative child's release was never sent");
    assert_eq!(
        value.expect("the cooperative drain failed the runtime"),
        VALUE,
        "the drain displaced the closure's value"
    );
    assert!(
        completed.load(Ordering::SeqCst),
        "the cooperative child never completed"
    );
    assert_eq!(
        completed_at_zero,
        Some(true),
        "the drain reached zero before the child it was awaiting completed"
    );
}

/// The observer half of [`cooperative_child_is_awaited_within_bounded_drain`]:
/// hold the drain on its one child, release that child, then read the terminal
/// observation.
fn observe_cooperative_drain(
    watch: &ArmedWatch<'_>,
    release: tokio::sync::oneshot::Sender<()>,
    completed: Arc<AtomicBool>,
) -> (bool, bool, Option<bool>) {
    watch.wait_armed();
    // The drain is waiting on exactly this child, which cannot have completed:
    // nothing has released it yet.
    let awaited = watch.probe(AWAITING_ONE, |_| ()).is_some();
    // Arm the terminal observation before the child can finish, so a zero
    // count cannot be reached before it is observable. The seam holds one
    // checkpoint at a time, so this arms only once the window above has been
    // released.
    watch.controller().pause_once(DRAINED).unwrap();
    let released = release.send(()).is_ok();
    let completed_at_zero = watch.probe(DRAINED, |_| completed.load(Ordering::SeqCst));
    (awaited, released, completed_at_zero)
}

/// A yielding async child that ignores `ScopeClosing` is aborted, joined, and
/// reported — the runtime returns instead of hanging on it.
#[test]
fn wedged_async_child_is_aborted_joined_and_reported() {
    let wedged = WedgedHandle::new();
    let closure_wedged = wedged.clone();

    // No wall-clock bound here: an unbounded drain would never reach the
    // terminal observation at all, so the assertion guarding it would never
    // run. What discriminates is the observation itself — reported outstanding,
    // joined, unregistered, and the documented forced-abort handle result.
    let (result, probe) = observe_drain_end(SHORT_DRAIN, move || {
        // Pending at an await it never leaves: abortable, but never
        // cooperative.
        let handle = camber::spawn_async(async { std::future::pending::<()>().await });
        closure_wedged.record(handle);
    });

    lifecycle_kinds::assert_scope_drain(
        result
            .as_ref()
            .expect_err("the drain reported no outstanding child"),
        1,
        "the drain did not report one outstanding child",
    );
    let (joined, retained) = probe.expect("the drain never paused at its terminal observation");
    assert!(
        joined,
        "the owner never awaited the aborted child's Tokio handle"
    );
    assert!(!retained, "the aborted child left a handle behind");

    let outcome = block_on_detached(join_bounded(wedged.take(), BOUND));
    assert_forced_abort(&outcome);
}

/// The drain's report counts every outstanding child, whatever kind they are.
///
/// Every other case here wedges one child and reads `ScopeDrainTimeout(1)`, a
/// payload an implementation that hardcoded one — or that reported "at least
/// one" — would also produce. Two children of the two kinds the drain treats
/// differently, one abortable and one it cannot preempt, are what separate a
/// count from a constant. The count is read off the returned error, so nothing
/// here depends on the clock.
#[test]
fn drain_timeout_counts_every_outstanding_child() {
    // Parks the blocking child on a channel rather than the clock, for the
    // reason `nonpreemptible_blocking_child_still_yields_bounded_return` gives:
    // executor shutdown cannot join a blocking thread, so dropping this sender
    // after the assertion is what ends the park.
    let (park_tx, park_rx) = std::sync::mpsc::channel::<()>();

    let result = scope_runtime(SHORT_DRAIN).run(move || {
        // Pending at an await it never leaves: abortable, and joined.
        camber::spawn_async(async { std::future::pending::<()>().await });
        // Parked in a blocking body: outstanding for the same drain, and
        // beyond its reach.
        camber::spawn(move || {
            let _ = park_rx.recv_timeout(BLOCKING_PARK);
        });
    });

    lifecycle_kinds::assert_scope_drain(
        result
            .as_ref()
            .expect_err("the drain reported no outstanding children"),
        2,
        "the drain did not report both outstanding children",
    );
    drop(park_tx);
}

/// A blocking child abort cannot preempt does not stall the scope drain: the
/// drain reports it and proceeds to resource shutdown within one window.
#[test]
fn nonpreemptible_blocking_child_still_yields_bounded_return() {
    // The two readings this case takes out of runtime-owned closures. Both are
    // handed out through the shared slot, which owns the diagnosis for a
    // closure that never filled one.
    let observed_at_shutdown = WedgedHandle::new();
    let teardown_at = WedgedHandle::new();
    let closure_teardown = teardown_at.clone();
    let resource_observation = observed_at_shutdown.clone();
    // Raised when the blocking child leaves its park, so resource shutdown can
    // read whether the child it cannot stop was still parked as it ran. That
    // flag is the ordering fact itself; the duration below corroborates it.
    let park_ended = Arc::new(AtomicBool::new(false));
    let child_park_ended = Arc::clone(&park_ended);
    let resource_park_ended = Arc::clone(&park_ended);
    // Parks the blocking child on a channel rather than the clock: the
    // runtime's own executor shutdown cannot join a `spawn_blocking` thread,
    // so a slept park would outlive the whole test. Dropping this sender after
    // the assertions ends it deterministically instead.
    let (park_tx, park_rx) = std::sync::mpsc::channel::<()>();

    let result = scope_runtime(SHORT_DRAIN)
        .resource(RecordingResource::new(
            "drain-marker",
            ignore_hook,
            move || {
                // Both facts at one observation point: when resource shutdown
                // ran, and whether the blocking child had left its park by
                // then.
                resource_observation
                    .record((Instant::now(), resource_park_ended.load(Ordering::SeqCst)));
                Ok(())
            },
        ))
        .run(move || {
            camber::spawn(move || {
                let _ = park_rx.recv_timeout(BLOCKING_PARK);
                child_park_ended.store(true, Ordering::SeqCst);
            });
            closure_teardown.record(Instant::now());
        });

    lifecycle_kinds::assert_scope_drain(
        result
            .as_ref()
            .expect_err("the drain reported no outstanding child"),
        1,
        "the drain did not report the non-preemptible child outstanding",
    );

    let (shutdown, park_ended_at_shutdown) = observed_at_shutdown
        .take_expecting("the resource shutdown hook never recorded the parked child");
    assert!(
        !park_ended_at_shutdown,
        "resource shutdown ran only after the blocking child left its park"
    );
    let waited = shutdown.saturating_duration_since(
        teardown_at.take_expecting("the closure never recorded its teardown start"),
    );
    assert!(
        waited <= SHORT_DRAIN + SLACK,
        "resource shutdown waited {waited:?} on a child the drain cannot stop"
    );
    drop(park_tx);
}

/// Resource shutdown begins only after the scope has aborted and JOINED every
/// stoppable child — not merely after the count reached zero.
#[test]
fn resource_shutdown_runs_only_after_stoppable_children_are_drained_or_aborted() {
    let (controller, builder) = probed_runtime(SHORT_DRAIN);
    let joined_at_shutdown = WedgedHandle::new();
    let resource_joined = joined_at_shutdown.clone();
    // Named from inside the closure, immediately before the wedged child is
    // admitted: the runtime admits its own children during setup, so a claim
    // taken out here would name one of those instead. The hook then reads this
    // child's own join acknowledgment rather than a runtime-wide total.
    let subject = NamedSubject::new();
    let closure_subject = subject.clone();

    let result = builder
        .resource(RecordingResource::new(
            "join-order",
            ignore_hook,
            move || {
                // Reads the join acknowledgment as shutdown runs, so the ordering
                // between the two is observed rather than inferred.
                resource_joined.record(reached(
                    subject.get("the join-order hook"),
                    AdmittedScope::joined,
                ));
                Ok(())
            },
        ))
        .run(move || {
            closure_subject.name_next(&controller);
            camber::spawn_async(async { std::future::pending::<()>().await });
        });

    lifecycle_kinds::assert_scope_drain(
        result
            .as_ref()
            .expect_err("the wedged child was not reported at all"),
        1,
        "the wedged child was not reported outstanding",
    );
    assert!(
        joined_at_shutdown
            .take_expecting("the resource shutdown hook never recorded the join acknowledgment"),
        "resource shutdown began before the aborted child's handle was joined"
    );
}

/// At the escalation boundary the scope acts on the retained handle: the same
/// child is registered and unjoined on the near side of the boundary, and
/// joined with its entry gone on the far side.
///
/// The proof is that strict before/after ordering across the boundary, not a
/// duration: the child is `pending` forever, so only the forced abort-and-join
/// can move the pair from `(0, 1)` to `(1, 0)`.
#[test]
fn deadline_drains_registered_async_child_to_zero() {
    let named = NamedSubject::new();
    let body_named = named.clone();

    let (result, (awaiting, drained)) = observe_armed_sequence(
        |builder| builder.shutdown_timeout(SHORT_DRAIN),
        move |gate| {
            // Named before the admission, so both windows below read this one
            // child rather than whatever the scope was holding.
            body_named.name_next(gate.controller());
            camber::spawn_async(async { std::future::pending::<()>().await });
            gate.arm(AWAITING_ONE);
        },
        move |watch| observe_escalation_boundary(watch, &named),
    );

    lifecycle_kinds::assert_scope_drain(
        result
            .as_ref()
            .expect_err("the deadline reported no registered child"),
        1,
        "the deadline did not report the registered child",
    );
    assert_eq!(
        awaiting,
        Some((false, true)),
        "before escalation the child was not registered and unjoined"
    );
    assert_eq!(
        drained,
        Some((true, false)),
        "after escalation the child was not joined with its handle removed"
    );
}

/// The observer half of [`deadline_drains_registered_async_child_to_zero`]:
/// read the named child on each side of the escalation boundary.
fn observe_escalation_boundary(
    watch: &ArmedWatch<'_>,
    named: &NamedSubject,
) -> (Option<(bool, bool)>, Option<(bool, bool)>) {
    let read = || read_child(named.get("the escalation-boundary probe"));
    watch.wait_armed();
    // The drain is waiting on exactly this child, and holding it here spends
    // none of the escalation budget: the seam credits the paused time back.
    let awaiting = watch.probe(AWAITING_ONE, |_| read());
    // The seam holds one checkpoint at a time, so the terminal observation is
    // armed only once the window above has been released. The child never
    // completes, so no state between the two windows is reachable.
    watch.controller().pause_once(DRAINED).unwrap();
    let drained = watch.probe(DRAINED, |_| read());
    (awaiting, drained)
}
