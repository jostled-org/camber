use crate::common::{
    BOUND, RUNTIME_OWNED_CHILDREN, join_bounded, observe_armed_sequence, observe_armed_window,
    registry_len, wait_paused_bounded, wait_registry_at_most,
};
use crate::scope_builders::{probed_runtime, scope_runtime};
use camber::runtime_test_support::{RuntimeCheckpoint, RuntimeController};
use camber::{RuntimeError, runtime};
use std::future::IntoFuture;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Poll the root scope's registry to empty within `bound`, reporting whether it
/// got there.
///
/// `Closed` is admission-refused AND count zero, so a closure that only
/// requested shutdown leaves the scope in `Closing`: the runtime's own children
/// are still counted at the close transition. The scope reaches `Closed` when
/// the last of them exits on `ScopeClosing` and the 1 -> 0 edge runs
/// `on_drained`, which is exactly what an empty registry reports — the zero
/// case of the shared registry wait.
fn wait_scope_drained(controller: &RuntimeController, bound: Duration) -> bool {
    wait_registry_at_most(controller, 0, bound)
}

/// A child admitted before the close transition is counted, so the drain
/// awaits it to completion — even though the transition was already pending
/// when it was admitted.
///
/// Limitation: the 0 -> 1 admission edge named by the invariant is not
/// reachable in-process on unix. The runtime admits its own signal watcher
/// before the closure runs, and that watcher can only exit AFTER `ScopeClosing`
/// — which is the transition under test — so the count is never momentarily
/// zero while admission is still Open. The case therefore proves the ordering
/// the edge exists for: the subject's admission completes on the near side of
/// the close transition, raising the count by exactly one, and the drain awaits
/// that subject to completion rather than returning on the count it read
/// before. No production seam is added to manufacture the zero-count start.
#[test]
fn admission_counted_before_close_is_awaited_by_the_drain() {
    let close_transition = RuntimeCheckpoint::ScopeCloseTransition;
    let drain_observed = RuntimeCheckpoint::ScopeWaitObserved(1);

    let child_ran = Arc::new(AtomicBool::new(false));
    let closure_child = Arc::clone(&child_ran);

    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();

    let (result, drain_window) = observe_armed_window(
        |builder| builder.shutdown_timeout(BOUND),
        drain_observed,
        move |controller| {
            // An uncounted watcher: on_cancel's task requests shutdown,
            // closing admission from outside the scope.
            runtime::on_cancel(async move {
                let _ = shutdown_rx.await;
            });
            controller.pause_once(close_transition).unwrap();
            shutdown_tx.send(()).unwrap();

            // The close is pending at its checkpoint: admission is still Open,
            // and the only children counted are the ones the runtime admits
            // for itself.
            assert!(
                wait_paused_bounded(controller, close_transition, BOUND),
                "the close transition never paused"
            );
            let before = registry_len(controller);

            camber::spawn_async(async move {
                // Records the release it actually received: a child that
                // proceeded because its sender was dropped would report a run
                // the drain never awaited.
                closure_child.store(release_rx.await.is_ok(), Ordering::SeqCst);
            });

            // Admission (RUNTIME_OWNED_CHILDREN -> +1) completed before the
            // transition runs.
            let after = registry_len(controller);
            controller.release(close_transition).unwrap();
            (before, after)
        },
        // Releases the child only once the drain is provably running with that
        // child still counted, and reads which child that is: the runtime's own
        // children have exited by this window, so the one retained entry is the
        // subject admitted at the close boundary.
        |controller| (release_tx.send(()).is_ok(), registry_len(controller)),
    );

    let (before, after) = result.expect("the admission runtime failed");
    assert_eq!(
        before, RUNTIME_OWNED_CHILDREN,
        "a child was already counted before the subject's admission"
    );
    assert_eq!(
        after,
        RUNTIME_OWNED_CHILDREN + 1,
        "the subject was not counted before the close transition ran"
    );
    assert_eq!(
        drain_window,
        Some((true, 1)),
        "the drain never paused holding exactly the child admitted before the close"
    );
    assert!(
        child_ran.load(Ordering::SeqCst),
        "the drain returned without awaiting a child admitted before the close"
    );
}

/// Admission counts a child before its body runs, so no observer can see the
/// count trail an admitted child.
#[test]
fn admission_counts_the_child_before_its_body_runs() {
    let counted = RuntimeCheckpoint::AdmissionCounted;

    let body_ran = Arc::new(AtomicBool::new(false));
    let closure_body = Arc::clone(&body_ran);
    let observer_body = Arc::clone(&body_ran);

    let (result, observed) = observe_armed_sequence(
        |builder| builder.shutdown_timeout(BOUND),
        move |gate| {
            // Armed from inside the closure, not before the runtime starts:
            // the runtime admits its own children first, and one of those
            // would take the single pause this case needs to land on the child
            // below.
            gate.arm(counted);
            camber::spawn_async(async move {
                closure_body.store(true, Ordering::SeqCst);
            });
        },
        |watch| {
            watch.wait_armed();
            watch.probe(counted, |controller| {
                (
                    observer_body.load(Ordering::SeqCst),
                    // Pins WHICH admission is paused. `AdmissionCounted` fires
                    // on every admission, so a probe that read only the body
                    // flag would report `false` for the runtime's own children
                    // too. The registry reports children whose joinable handle
                    // is filled, and the subject's is not yet — it is seeded at
                    // this checkpoint and registered at the next — so its own
                    // admission reads exactly the runtime-owned children. An
                    // earlier admission reports fewer, a later one more.
                    registry_len(controller),
                )
            })
        },
    );

    result.expect("the admission runtime failed");
    assert_eq!(
        observed,
        Some((false, RUNTIME_OWNED_CHILDREN)),
        "the pause was not the subject's admission with its body still unrun"
    );
    assert!(
        body_ran.load(Ordering::SeqCst),
        "the admitted child never ran"
    );
}

/// A child's joinable handle is registered before its body runs, and the
/// entry is gone once the child completes — the scope never counts a child it
/// has no way to join, and a finished child leaves no handle behind.
#[test]
fn gated_admission_registers_before_run_and_removes_on_completion() {
    let registered = RuntimeCheckpoint::AdmissionRegistered;
    let holder_only = RuntimeCheckpoint::ScopeWaitObserved(1);

    let body_ran = Arc::new(AtomicBool::new(false));
    let hold = Arc::new(tokio::sync::Notify::new());

    let closure_body = Arc::clone(&body_ran);
    let closure_hold = Arc::clone(&hold);
    let observer_body = Arc::clone(&body_ran);
    let observer_hold = Arc::clone(&hold);

    let (result, (registered_window, holder_window)) = observe_armed_sequence(
        |builder| builder.shutdown_timeout(BOUND),
        move |gate| {
            // The holder keeps exactly one child outstanding, so the drain
            // reports a count of one once the subject is gone.
            camber::spawn_async(async move { closure_hold.notified().await });

            gate.arm(registered);
            let subject = camber::spawn_async(async move {
                closure_body.store(true, Ordering::SeqCst);
            });
            // Bounded: the drain has not started yet, so `shutdown_timeout`
            // bounds nothing here — a subject that never resolved would park
            // the closure and hang the binary with no failure reported.
            runtime::block_on(join_bounded(subject, BOUND)).unwrap();

            gate.arm(holder_only);
            // Bounds the drain at shutdown_timeout if any assertion wedges.
            runtime::request_shutdown();
        },
        |watch| {
            // The subject child is registered and still gated: its handle is
            // already retained and its body has not run.
            watch.wait_armed();
            let registered_window = watch.probe(registered, |controller| {
                (
                    observer_body.load(Ordering::SeqCst),
                    registry_len(controller),
                )
            });

            // The drain observes one remaining child: the subject completed,
            // so only the holder's entry may remain.
            watch.wait_armed();
            let holder_window = watch.probe(holder_only, |controller| {
                let entries = registry_len(controller);
                observer_hold.notify_one();
                entries
            });
            (registered_window, holder_window)
        },
    );

    result.expect("the gated admission runtime failed");
    let (body_ran_while_gated, entries_when_registered) =
        registered_window.expect("the admission never paused at its registration window");
    assert!(
        !body_ran_while_gated,
        "the child's body ran before its joinable handle was registered"
    );
    assert_eq!(
        entries_when_registered,
        RUNTIME_OWNED_CHILDREN + 2,
        "the registry did not retain the gated child's handle alongside the holder's"
    );
    assert_eq!(
        holder_window,
        Some(1),
        "the completed child left its handle behind in the registry"
    );
}

/// Row A: an admission attempted from the closure thread with the scope in
/// `Closed` — admission refused and the count already at zero — is refused, and
/// no task body runs.
#[test]
fn admission_after_close_from_the_closure_thread_is_refused() {
    let blocking_ran = Arc::new(AtomicBool::new(false));
    let async_ran = Arc::new(AtomicBool::new(false));
    let closure_blocking = Arc::clone(&blocking_ran);
    let closure_async = Arc::clone(&async_ran);
    let (controller, builder) = probed_runtime(BOUND);

    let (drained, blocking, asynchronous) = builder
        .run(|| {
            runtime::request_shutdown();
            // `Closed`, not `Closing`: waiting for the runtime's own children
            // to exit is what separates this row from Row B, which attempts its
            // admissions while one child is still live.
            let drained = wait_scope_drained(&controller, BOUND);

            let blocking = camber::spawn(move || closure_blocking.store(true, Ordering::SeqCst));
            let asynchronous = camber::spawn_async(async move {
                closure_async.store(true, Ordering::SeqCst);
            });
            (
                drained,
                blocking.join(),
                runtime::block_on(asynchronous.into_future()),
            )
        })
        .unwrap();

    assert!(
        drained,
        "the scope never reached Closed: its count was still above zero"
    );
    assert!(
        matches!(blocking, Err(RuntimeError::ScopeClosed)),
        "a blocking admission after the close was not refused: {blocking:?}"
    );
    assert!(
        matches!(asynchronous, Err(RuntimeError::ScopeClosed)),
        "an async admission after the close was not refused: {asynchronous:?}"
    );
    assert!(
        !blocking_ran.load(Ordering::SeqCst),
        "the refused blocking task ran its body"
    );
    assert!(
        !async_ran.load(Ordering::SeqCst),
        "the refused async task ran its body"
    );
}

/// Row B: a descendant admission from inside an already-admitted child, with
/// the scope in Closing (one live child, count >= 1), is refused too.
#[test]
fn descendant_admission_while_closing_is_refused() {
    let blocking_ran = Arc::new(AtomicBool::new(false));
    let async_ran = Arc::new(AtomicBool::new(false));
    let child_blocking = Arc::clone(&blocking_ran);
    let child_async = Arc::clone(&async_ran);

    let (closed_tx, closed_rx) = tokio::sync::oneshot::channel::<()>();

    let outcomes = scope_runtime(BOUND)
        .run(move || {
            let child = camber::spawn_async(async move {
                // Records the rendezvous it actually received: a child that
                // proceeded because its sender was dropped would attempt its
                // descendant admissions before the close, proving nothing.
                let observed_close = closed_rx.await.is_ok();
                let blocking = camber::spawn(move || child_blocking.store(true, Ordering::SeqCst));
                let asynchronous = camber::spawn_async(async move {
                    child_async.store(true, Ordering::SeqCst);
                });
                (
                    observed_close,
                    blocking.join(),
                    asynchronous.into_future().await,
                )
            });

            runtime::request_shutdown();
            closed_tx.send(()).unwrap();
            // Bounded: the drain cannot start until this closure returns, so a
            // child whose descendant admissions never resolve would park the
            // closure rather than fail the case.
            runtime::block_on(join_bounded(child, BOUND)).unwrap()
        })
        .unwrap();

    assert!(
        outcomes.0,
        "the descendant ran before the close was observed"
    );
    assert!(
        matches!(outcomes.1, Err(RuntimeError::ScopeClosed)),
        "a descendant blocking admission while closing was not refused: {:?}",
        outcomes.1
    );
    assert!(
        matches!(outcomes.2, Err(RuntimeError::ScopeClosed)),
        "a descendant async admission while closing was not refused: {:?}",
        outcomes.2
    );
    assert!(
        !blocking_ran.load(Ordering::SeqCst),
        "the refused descendant blocking task ran its body"
    );
    assert!(
        !async_ran.load(Ordering::SeqCst),
        "the refused descendant async task ran its body"
    );
}
