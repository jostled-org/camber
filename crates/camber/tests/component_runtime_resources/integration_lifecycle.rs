//! 4.T1: runtime-owned integration admission, atomic with root-scope closure.
//!
//! The doc-hidden `IntegrationLifecycleProbe` calls the real runtime admission
//! path for one fixed integration kind, and the work it runs is a concrete
//! controlled future the row completes. The probe chooses no outcome: the
//! registry admits or refuses, commits closing, and settles each entry itself.
//! A row observes those commits through the entry's read-only observer, which
//! holds no access and so cannot keep an entry alive or close it.
//!
//! Each row owns its runtime, its threads, and its rendezvous, and returns its
//! own verdict, so one broken transition cannot hide the others.

use crate::common;
use crate::integration_accounts::expect_busy as expect_busy_refusal;
use crate::integration_rows::{
    LIVE_LIMIT, ROW_BOUND, Row, clean_run, expect, expect_eq, held_work, hold_live_slots,
    instant_work, row_bounded, run_rows,
};
use camber::runtime_test_support::{
    IntegrationEntryObserver, IntegrationEntryState, IntegrationLifecycleProbe,
    IntegrationProbeHandle, RuntimeCheckpoint, runtime_schedule, wait_scope_closing,
};
use camber::{
    IntegrationError, IntegrationFailure, IntegrationKind, Retryability, RuntimeError, runtime,
};
use std::fmt::Display;
use std::sync::mpsc;
use std::thread;

/// Admit one controlled integration, or fail the row naming the refusal.
fn admitted(what: &str, kind: IntegrationKind) -> Result<IntegrationProbeHandle, String> {
    IntegrationLifecycleProbe::admit(kind).map_err(|error| format!("{what} was refused: {error}"))
}

/// Fail the row unless a settled operation or close succeeded.
fn expect_success<E: Display>(what: &str, outcome: Result<(), E>) -> Row {
    outcome.map_err(|error| format!("{what}: expected success, got {error}"))
}

/// The typed integration refusal a runtime error carries, if it carries one.
fn integration_refusal(error: &RuntimeError) -> Option<&IntegrationError> {
    match error {
        RuntimeError::Integration(refusal) => Some(refusal),
        _ => None,
    }
}

/// Fail the row unless `outcome` is a typed `Busy` refusal for `kind`.
fn expect_busy<T>(what: &str, outcome: Result<T, RuntimeError>, kind: IntegrationKind) -> Row {
    let error = match outcome {
        Ok(_) => return Err(format!("{what} was admitted past the live limit")),
        Err(error) => error,
    };
    let Some(refusal) = integration_refusal(&error) else {
        return Err(format!(
            "{what} refused with {error:?}, not a typed integration refusal"
        ));
    };
    expect_busy_refusal(what, refusal, kind)
}

/// Whether `error` is a refusal of new work on an entry that has closed.
///
/// A permitted set rather than one answer: the entry refuses as closed, and a
/// runtime whose root admission has closed refuses first as `ScopeClosed`. Both
/// are committed refusals; neither submits the work.
fn refused_as_closed(error: &RuntimeError) -> bool {
    match error {
        RuntimeError::ScopeClosed => true,
        RuntimeError::Integration(refusal) => refusal.failure() == IntegrationFailure::Closed,
        _ => false,
    }
}

/// Fail the row unless running new work on `handle` is refused as closed.
fn expect_new_work_refused(what: &str, handle: &IntegrationProbeHandle) -> Row {
    match handle.run(instant_work()) {
        Ok(_) => Err(format!("{what}: a closed entry accepted new work")),
        Err(error) if refused_as_closed(&error) => Ok(()),
        Err(error) => Err(format!("{what}: refused with {error:?}, not as closed")),
    }
}

/// Fail the row unless the observed entry has committed closing or settled.
fn expect_closing_committed(what: &str, observer: &IntegrationEntryObserver) -> Row {
    let state = observer.state();
    expect(
        &format!("{what}: woke on close while the entry read {state:?}"),
        matches!(
            state,
            IntegrationEntryState::Closing | IntegrationEntryState::Settled
        ),
    )
}

/// Fail the row unless a whole runtime run tore down cleanly and its closure
/// returned a passing verdict, naming the run `what`.
fn expect_clean_run(what: &str, outcome: Result<Row, RuntimeError>) -> Row {
    clean_run(outcome).map_err(|reason| format!("{what}: {reason}"))
}

/// Fail the row unless a runtime thread returned, tore down cleanly, and its
/// closure passed, naming the run `what`.
fn joined_run(what: &str, joined: thread::Result<Result<Row, RuntimeError>>) -> Row {
    joined
        .map_err(|_| format!("{what} thread unwound"))
        .and_then(|outcome| expect_clean_run(what, outcome))
}

// ── 4.T1 ──────────────────────────────────────────────────────────────

#[test]
fn integration_admission_is_atomic_with_root_closure() {
    run_rows(&[
        (
            "no runtime refuses before admission",
            no_runtime_refuses_before_admission,
        ),
        (
            "closed root admission refuses",
            closed_root_admission_refuses,
        ),
        (
            "admission racing root closure is stopped with it",
            admission_racing_root_closure_is_stopped_with_it,
        ),
        (
            "live limit refuses the sixty-fifth instance",
            live_limit_refuses_the_sixty_fifth_instance,
        ),
        (
            "slot releases only after settlement",
            slot_releases_only_after_settlement,
        ),
        ("identity overflow is refused", identity_overflow_is_refused),
        (
            "close commits before observers wake",
            close_commits_before_observers_wake,
        ),
        (
            "concurrent close returns one fixed settlement",
            concurrent_close_returns_one_fixed_settlement,
        ),
        (
            "handle stays with its captured runtime",
            handle_stays_with_its_captured_runtime,
        ),
    ]);
}

/// Outside every runtime there is no owner to capture: admission and identity
/// seeding both answer `NoRuntime` rather than minting an orphan.
fn no_runtime_refuses_before_admission() -> Row {
    match IntegrationLifecycleProbe::admit(IntegrationKind::Nats) {
        Err(RuntimeError::NoRuntime) => {}
        Err(other) => return Err(format!("admission refused with {other:?}, not NoRuntime")),
        Ok(handle) => {
            return Err(format!(
                "instance {} was admitted with no runtime to own it",
                handle.id()
            ));
        }
    }
    match IntegrationLifecycleProbe::seed_next_identity(1) {
        Err(RuntimeError::NoRuntime) => Ok(()),
        other => Err(format!(
            "identity seeding answered {other:?}, not NoRuntime"
        )),
    }
}

/// A child still running after root admission closed cannot admit an
/// integration: the registry refuses as `ScopeClosed` once the root has.
fn closed_root_admission_refuses() -> Row {
    let (report, reported) = mpsc::sync_channel(1);
    let outcome = runtime::builder().run(move || {
        drop(camber::spawn_async(async move {
            wait_scope_closing().await;
            let refused = IntegrationLifecycleProbe::admit(IntegrationKind::Sqs).map(|h| h.id());
            let _sent = report.send(refused);
        }));
        Ok(())
    });
    expect_clean_run("closed-admission runtime", outcome)?;
    match reported.recv_timeout(ROW_BOUND) {
        Ok(Err(RuntimeError::ScopeClosed)) => Ok(()),
        Ok(Err(other)) => Err(format!("admission after closure refused with {other:?}")),
        Ok(Ok(id)) => Err(format!(
            "instance {id} was admitted after root admission closed"
        )),
        Err(_) => Err("the closing child never reported its admission".to_owned()),
    }
}

/// An admission taken while the root is held at its close transition is
/// admitted, and the registry stop that root closure starts reaches it
/// without its handle being dropped or closed.
///
/// No admitted entry can land on the far side of the transition unowned: the
/// only two outcomes are admitted-and-stopped or refused.
fn admission_racing_root_closure_is_stopped_with_it() -> Row {
    let controller = runtime_schedule();
    let (go, admit_now) = tokio::sync::oneshot::channel::<()>();
    let (report, reported) = mpsc::sync_channel::<Result<IntegrationEntryObserver, String>>(1);

    std::thread::scope(|scope| {
        let run = scope.spawn(|| {
            controller.pause_once(RuntimeCheckpoint::ScopeCloseTransition)?;
            runtime::builder()
                .with_test_schedule(&controller)
                .run(move || {
                    drop(camber::spawn_async(async move {
                        if admit_now.await.is_err() {
                            return;
                        }
                        let handle = match IntegrationLifecycleProbe::admit(IntegrationKind::Nats) {
                            Ok(handle) => handle,
                            Err(error) => {
                                let _sent = report
                                    .send(Err(format!("admission before closure: {error:?}")));
                                return;
                            }
                        };
                        let observer = handle.observer();
                        let _sent = report.send(Ok(observer.clone()));
                        // The handle is held, not dropped or closed: only the
                        // registry's own stop can close this entry.
                        let _stopped = tokio::time::timeout(ROW_BOUND, observer.closing()).await;
                        drop(handle);
                    }));
                })
        });

        let paused = common::poll_until(ROW_BOUND, || {
            controller.is_paused(RuntimeCheckpoint::ScopeCloseTransition)
        });
        let _sent = go.send(());
        let observed = reported.recv_timeout(ROW_BOUND);
        let released = controller.release(RuntimeCheckpoint::ScopeCloseTransition);
        controller.disarm();
        let torn_down = run.join();

        expect("the root never reached its close transition", paused)?;
        released.map_err(|error| format!("releasing the close transition: {error:?}"))?;
        let observer =
            observed.map_err(|_| "the racing child never reported its admission".to_owned())??;
        match torn_down {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(format!("the runtime tore down with {error:?}")),
            Err(_) => return Err("the runtime thread unwound".to_owned()),
        }
        expect_eq(
            "entry state after the run returned",
            observer.state(),
            IntegrationEntryState::Settled,
        )
    })
}

/// Sixty-four live instances fill the registry; the sixty-fifth is a typed
/// `Busy` for its kind, and identities rise across every admission.
fn live_limit_refuses_the_sixty_fifth_instance() -> Row {
    let outcome = runtime::builder().run(|| {
        let live = hold_live_slots(LIVE_LIMIT, IntegrationKind::Nats)?;
        let ids: Vec<u64> = live.iter().map(IntegrationProbeHandle::id).collect();
        expect(
            &format!("identities did not rise across admission: {ids:?}"),
            ids.windows(2).all(|pair| pair[0] < pair[1]),
        )?;
        expect_busy(
            "the sixty-fifth instance",
            IntegrationLifecycleProbe::admit(IntegrationKind::Sqs),
            IntegrationKind::Sqs,
        )
    });
    expect_clean_run("live-limit runtime", outcome)
}

/// A closing entry keeps its live slot until its retained work settles.
///
/// The close is committed while one operation is still held, so the next
/// admission stays `Busy`; once the row completes that work the close settles
/// and the freed slot admits again.
fn slot_releases_only_after_settlement() -> Row {
    let outcome = runtime::builder().run(|| {
        let mut live = hold_live_slots(LIVE_LIMIT, IntegrationKind::Nats)?.into_vec();
        let closing = live.pop().ok_or("no instance to close")?;
        let observer = closing.observer();
        let (release, work) = held_work();
        let operation = closing
            .run(work)
            .map_err(|error| format!("the held operation was refused: {error:?}"))?;
        let close = camber::spawn_async(closing.close());

        row_bounded("the close commit", observer.closing())?;
        expect_closing_committed("the committed close", &observer)?;
        expect_busy(
            "admission while the closing entry still holds work",
            IntegrationLifecycleProbe::admit(IntegrationKind::Nats),
            IntegrationKind::Nats,
        )?;

        let _released = release.send(());
        let finished = row_bounded("the held operation", operation.wait())?;
        expect_success("the held operation", finished)?;
        let closed = row_bounded("the close", close)?;
        expect(
            &format!("the close settled as {closed:?}"),
            matches!(closed, Ok(Ok(()))),
        )?;
        row_bounded("the entry settlement", observer.settled())?;
        expect_eq(
            "entry state after its close",
            observer.state(),
            IntegrationEntryState::Settled,
        )?;
        admitted("admission after settlement", IntegrationKind::Nats).map(drop)
    });
    expect_clean_run("slot-release runtime", outcome)
}

/// Identities are checked, not wrapped: after the last identity is handed out,
/// admission is refused as `LimitExceeded` even once a live slot is free.
fn identity_overflow_is_refused() -> Row {
    let outcome = runtime::builder().run(|| {
        IntegrationLifecycleProbe::seed_next_identity(u64::MAX)
            .map_err(|error| format!("seeding the last identity: {error:?}"))?;
        let last = admitted("the last identity", IntegrationKind::Dns01)?;
        expect_eq("the last identity", last.id(), u64::MAX)?;
        let observer = last.observer();
        row_bounded("closing the last identity", last.close())?
            .map_err(|error| format!("closing the last identity: {error:?}"))?;
        row_bounded("the last identity's settlement", observer.settled())?;
        expect(
            "exhausted identities were reset by the seed hook",
            IntegrationLifecycleProbe::seed_next_identity(u64::MAX).is_err(),
        )?;

        let refused = IntegrationLifecycleProbe::admit(IntegrationKind::Dns01);
        let error = match refused {
            Ok(handle) => {
                return Err(format!("identity {} was minted past the last", handle.id()));
            }
            Err(error) => error,
        };
        let refusal = integration_refusal(&error)
            .ok_or_else(|| format!("exhausted identities refused with {error:?}"))?;
        expect_eq(
            "exhausted identity failure",
            refusal.failure(),
            IntegrationFailure::LimitExceeded,
        )?;
        expect_eq(
            "exhausted identity retryability",
            refusal.retryability(),
            Retryability::Never,
        )
    });
    expect_clean_run("identity-overflow runtime", outcome)
}

#[test]
fn identity_seed_cannot_replace_a_live_entry() {
    let observer = runtime::builder()
        .run(|| {
            let first =
                IntegrationLifecycleProbe::admit(IntegrationKind::Nats).expect("first admission");
            first.ready().expect("first ready");
            let observer = first.observer();
            for id in [0, first.id()] {
                assert!(
                    matches!(
                        IntegrationLifecycleProbe::seed_next_identity(id),
                        Err(RuntimeError::Config(_))
                    ),
                    "seeding an issued identity must be refused"
                );
            }
            let next = IntegrationLifecycleProbe::admit(IntegrationKind::Sqs)
                .expect("admission after refused seed");
            assert_eq!(next.id(), first.id() + 1);
            (observer, first, next)
        })
        .expect("registry shutdown");
    assert_eq!(observer.0.state(), IntegrationEntryState::Settled);
}

/// An observer woken by the close sees the committed state, and every access
/// clone already refuses new work at the moment it wakes.
fn close_commits_before_observers_wake() -> Row {
    let outcome = runtime::builder().run(|| {
        let handle = admitted("the closing instance", IntegrationKind::Sqs)?;
        handle
            .ready()
            .map_err(|error| format!("marking the instance ready: {error:?}"))?;
        expect_eq(
            "state before close",
            handle.observer().state(),
            IntegrationEntryState::Ready,
        )?;
        let observer = handle.observer();
        let clone = handle.clone();
        let woken = camber::spawn_async(async move {
            observer.closing().await;
            let committed = expect_closing_committed("the woken observer", &observer);
            let refused = expect_new_work_refused("the clone at wake", &clone);
            committed.and(refused)
        });

        let closed = row_bounded("the close", handle.close())?;
        expect_success("the close", closed)?;
        match row_bounded("the woken observer", woken)? {
            Ok(verdict) => verdict?,
            Err(error) => return Err(format!("the observer child ended with {error:?}")),
        }
        expect_new_work_refused("the closing handle", &handle)
    });
    expect_clean_run("close-commit runtime", outcome)
}

/// Close is idempotent: two concurrent closers and a later one all read the
/// same fixed settlement.
fn concurrent_close_returns_one_fixed_settlement() -> Row {
    let outcome = runtime::builder().run(|| {
        let handle = admitted("the closing instance", IntegrationKind::Nats)?;
        let clone = handle.clone();
        let first = camber::spawn_async(handle.close());
        let second = camber::spawn_async(clone.close());
        let first = row_bounded("the first close", first)?;
        let second = row_bounded("the second close", second)?;
        expect(
            &format!("concurrent closes settled as {first:?} and {second:?}"),
            matches!((&first, &second), (Ok(Ok(())), Ok(Ok(())))),
        )?;
        let later = row_bounded("the later close", handle.close())?;
        expect_success("the later close", later)
    });
    expect_clean_run("concurrent-close runtime", outcome)
}

/// A handle carried into a second runtime still belongs to the first.
///
/// Runtime A admits the entry and hands it to runtime B, which runs work on
/// it. A's root return stops that entry while B is still open, so B's clone
/// refuses new work, and A's teardown waits for the work B submitted before it
/// returns. The handle never adopts the runtime of the thread using it.
fn handle_stays_with_its_captured_runtime() -> Row {
    let (handoff, handed) = mpsc::sync_channel::<IntegrationProbeHandle>(1);
    let (started, work_started) = mpsc::sync_channel::<()>(1);

    std::thread::scope(|scope| {
        let owner = scope.spawn(move || {
            runtime::builder().run(move || {
                let handle = admitted("the owning runtime's instance", IntegrationKind::Sqs)?;
                handoff
                    .send(handle)
                    .map_err(|_| "the second runtime was gone".to_owned())?;
                work_started
                    .recv_timeout(ROW_BOUND)
                    .map_err(|_| "the second runtime never submitted work".to_owned())
            })
        });

        let borrower = scope.spawn(move || {
            runtime::builder().run(move || {
                let handle = handed
                    .recv_timeout(ROW_BOUND)
                    .map_err(|_| "the owning runtime never handed its instance over".to_owned())?;
                let observer = handle.observer();
                let (release, work) = held_work();
                let operation = handle
                    .run(work)
                    .map_err(|error| format!("work from the second runtime: {error:?}"))?;
                let _sent = started.send(());

                row_bounded("the owning runtime's stop", observer.closing())?;
                expect_closing_committed("the stop the owner committed", &observer)?;
                expect(
                    "the second runtime was already stopping",
                    !runtime::is_shutting_down(),
                )?;
                expect_new_work_refused("the carried handle", &handle)?;

                let _released = release.send(());
                let finished = row_bounded("the submitted work", operation.wait())?;
                expect_success("the submitted work", finished)?;
                drop(handle);
                row_bounded("the owner's settlement", observer.settled())?;
                expect_eq(
                    "entry state after the owner settled it",
                    observer.state(),
                    IntegrationEntryState::Settled,
                )
            })
        });

        let owned = joined_run("the owning runtime", owner.join());
        let borrowed = joined_run("the borrowing runtime", borrower.join());
        owned.and(borrowed)
    })
}
