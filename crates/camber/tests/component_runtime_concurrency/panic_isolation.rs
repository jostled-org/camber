use crate::common::{BOUND, PERPETUAL, SHORT_DRAIN, join_bounded, wait_registry_at_most};
use crate::scope_builders::{probed_runtime, scope_runtime};
use camber::{RuntimeError, runtime, schedule};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

const USER_PANIC: &str = "user-owned child panic";
const INTERNAL_PANIC: &str = "internally-owned child panic";
const CLOSURE_VALUE: u32 = 11;
const SIBLING_VALUE: u32 = 23;

/// Fire one schedule tick that panics, and return once its body has entered.
///
/// The callback rendezvouses before it panics, so the panic is guaranteed to
/// have been raised on a child the scope owns rather than raced against the
/// closure's return.
fn panic_from_a_schedule_child() {
    let (entered_tx, mut entered_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
    let handle = schedule::every_async(PERPETUAL, move || {
        let entered = entered_tx.clone();
        async move {
            entered
                .send(())
                .expect("the panic rendezvous receiver was dropped");
            panic!("{}", INTERNAL_PANIC);
        }
    })
    .unwrap();
    handle.trigger();
    // Bounded: the sender lives as long as the schedule handle, so a tick that
    // never runs would never disconnect the receiver and an unbounded wait
    // would park every case in this file with no failure reported.
    runtime::block_on(tokio::time::timeout(BOUND, entered_rx.recv()))
        .expect("the panicking schedule callback never entered")
        .expect("the panic rendezvous sender was dropped");
}

/// A user-owned child's panic is delivered on its own handle and leaves the
/// runtime result alone. Paired negative of the internal-panic routing.
#[test]
fn user_owned_panic_stays_on_its_handle_and_does_not_displace_run_result() {
    let sibling_ran = Arc::new(AtomicBool::new(false));
    let child_sibling = Arc::clone(&sibling_ran);

    let outcome = scope_runtime(SHORT_DRAIN).run(move || {
        let panicker = camber::spawn_async(async { panic!("{}", USER_PANIC) });
        let sibling = camber::spawn_async(async move {
            child_sibling.store(true, Ordering::SeqCst);
            SIBLING_VALUE
        });
        // Bounded: both joins run before the closure returns, so the
        // drain's own timeout bounds neither. A child that never resolved
        // would park the closure instead of failing the case.
        let panicked = runtime::block_on(join_bounded(panicker, BOUND));
        let survived = runtime::block_on(join_bounded(sibling, BOUND));
        (panicked, survived, CLOSURE_VALUE)
    });

    match outcome {
        Err(error) => panic!("a user-owned panic displaced the runtime result: {error}"),
        Ok((panicked, survived, value)) => {
            assert!(
                matches!(&panicked, Err(RuntimeError::TaskPanicked(message)) if &**message == USER_PANIC),
                "the panic was not delivered on its own handle: {panicked:?}"
            );
            assert!(
                matches!(survived, Ok(SIBLING_VALUE)),
                "the panicking child cancelled its sibling: {survived:?}"
            );
            assert_eq!(value, CLOSURE_VALUE);
        }
    }
    assert!(
        sibling_ran.load(Ordering::SeqCst),
        "the sibling never ran alongside the panicking child"
    );
}

/// An internally-owned child has no handle to carry its panic, so the scope
/// records it and it displaces the closure's value — which is never inspected.
#[test]
fn internal_panic_displaces_runtime_result_without_inspecting_closure_value() {
    let sibling_ran = Arc::new(AtomicBool::new(false));
    let child_sibling = Arc::clone(&sibling_ran);

    let outcome = scope_runtime(SHORT_DRAIN).run(move || {
        let sibling = camber::spawn_async(async move {
            child_sibling.store(true, Ordering::SeqCst);
        });
        panic_from_a_schedule_child();
        // Bounded for the same reason as the paired user-panic case: this
        // join happens before the drain exists.
        runtime::block_on(join_bounded(sibling, BOUND)).unwrap();
        // A plain value, not a Result: displacement cannot come from the
        // runtime reading it.
        CLOSURE_VALUE
    });

    assert!(
        matches!(&outcome, Err(RuntimeError::TaskPanicked(message)) if &**message == INTERNAL_PANIC),
        "the internal panic did not displace the closure's value: {outcome:?}"
    );
    assert!(
        sibling_ran.load(Ordering::SeqCst),
        "the internal panic cancelled a sibling"
    );
}

/// A panic localizes the fault, so it is reported in preference to the drain
/// timeout it often causes.
#[test]
fn internal_panic_outranks_drain_timeout_when_both_occur() {
    let (controller, builder) = probed_runtime(SHORT_DRAIN);

    let outcome = builder.run(|| {
        // Never observes ScopeClosing, so the drain must escalate.
        camber::spawn_async(async { std::future::pending::<()>().await });
        // Read once that child is registered and before the schedule is
        // admitted. Neither it nor the runtime's own children can exit
        // while the closure runs — only `ScopeClosing` ends them, and that
        // has not fired — so the schedule child is the only one that can
        // take the registry back to this length.
        let before = controller.scope_registry_len().unwrap();
        panic_from_a_schedule_child();
        // Ordering, not budget: without this the recorded panic would be
        // read against the drain window rather than against the child's
        // own exit, which is the wall-clock race this suite forbids.
        assert!(
            wait_registry_at_most(&controller, before, BOUND),
            "the panicking schedule child never left the root scope, so the \
             drain timeout would have been read against an unrecorded panic"
        );
    });

    assert!(
        matches!(&outcome, Err(RuntimeError::TaskPanicked(message)) if &**message == INTERNAL_PANIC),
        "the drain timeout outranked the internal panic: {outcome:?}"
    );
}
