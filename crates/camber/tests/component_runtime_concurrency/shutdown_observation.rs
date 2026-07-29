use crate::common::BOUND;
use crate::schedule_probes::{
    AsyncProbe, CALLBACK_INTERVAL, SyncProbe, async_deadline, deadline_left, sync_deadline,
};
use camber::runtime_test_support::{RuntimeCheckpoint, runtime_schedule};
use std::num::NonZeroUsize;
use std::time::Duration;

/// Bounds each leg of the registration-window rendezvous.
///
/// Short of the parent's ten-second isolation bound even when both the wait and
/// the release spend it: a run the parent kills reports nothing about which leg
/// failed.
const REGISTRATION_WINDOW_BOUND: Duration = Duration::from_secs(2);
const GENERATED_CASE_COUNT: u64 = 32;
const SHUTDOWN_WINDOW_MODE: &str = "runtime-shutdown-registration-window";
const SHUTDOWN_WINDOW_MARKER: &str = "runtime-shutdown-registration-window-complete";
const SHUTDOWN_WINDOW_TEST: &str =
    "shutdown_observation::shutdown_at_wait_registration_boundary_is_observed";

#[camber::test]
async fn on_shutdown_completes_after_request_shutdown() {
    let deadline = async_deadline();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (completed_tx, mut completed_rx) = tokio::sync::oneshot::channel();
    camber::spawn_async(async move {
        started_tx.send(()).unwrap();
        camber::task::on_shutdown().await;
        completed_tx.send(()).unwrap();
    });

    assert!(matches!(
        tokio::time::timeout_at(deadline, started_rx).await,
        Ok(Ok(()))
    ));
    assert!(matches!(
        completed_rx.try_recv(),
        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
    ));
    camber::runtime::request_shutdown();
    assert!(matches!(
        tokio::time::timeout_at(deadline, completed_rx).await,
        Ok(Ok(()))
    ));
}

#[camber::test]
async fn on_shutdown_completes_immediately_if_already_shutting_down() {
    let deadline = async_deadline();
    camber::runtime::request_shutdown();
    assert!(matches!(
        tokio::time::timeout_at(deadline, camber::task::on_shutdown()).await,
        Ok(())
    ));
}

#[camber::test]
async fn shutdown_before_wait_registration_is_observed() {
    // Admitted before the close transition, so the child exists; it
    // constructs and polls its observer only after notify_waiters has fired.
    let (observer, start) = spawn_deferred_shutdown_observer();
    camber::runtime::request_shutdown();
    start.send(()).unwrap();

    let result = tokio::time::timeout_at(async_deadline(), observer).await;
    assert!(
        matches!(result, Ok(Ok(()))),
        "an observer registered after shutdown did not observe the sticky state: {result:?}"
    );
}

#[test]
fn shutdown_at_wait_registration_boundary_is_observed() {
    crate::common::run_in_child(
        SHUTDOWN_WINDOW_TEST,
        SHUTDOWN_WINDOW_MODE,
        SHUTDOWN_WINDOW_MARKER,
        BOUND,
        run_shutdown_registration_window,
    );
}

fn run_shutdown_registration_window() {
    let checkpoint = RuntimeCheckpoint::ShutdownWaitRegistered;
    let controller = runtime_schedule();
    controller.pause_once(checkpoint).unwrap();

    camber::runtime::builder()
        .worker_threads(2)
        .with_test_schedule(&controller)
        .run(|| {
            let observer = camber::spawn_async(camber::task::on_shutdown());
            // Both waits are bounded and the release runs even if the probe
            // unwinds: the raw seam calls would park this closure forever on a
            // registration that never arrives, and only the parent process's
            // kill would end it — discarding which step failed.
            let inert_before_request = crate::common::probe_paused_window(
                &controller,
                checkpoint,
                REGISTRATION_WINDOW_BOUND,
                || {
                    let inert = !camber::runtime::is_shutting_down();
                    camber::runtime::request_shutdown();
                    inert
                },
            );
            assert_eq!(
                inert_before_request,
                Some(true),
                "the registration window was never observed with shutdown still unrequested"
            );

            let result = camber::runtime::block_on(async {
                tokio::time::timeout_at(async_deadline(), observer).await
            });
            assert!(
                matches!(result, Ok(Ok(()))),
                "shutdown notification was lost at registration: {result:?}"
            );
        })
        .unwrap();
}

#[test]
fn generated_shutdown_sequences_are_monotonic_and_observable() {
    let generator = crate::deterministic::DeterministicGenerator::stable();

    for index in 0..GENERATED_CASE_COUNT {
        let mut case = generator.case(index);
        let observer_count = case.bounded(NonZeroUsize::new(8).unwrap()) + 1;
        let before_shutdown = case.bounded(NonZeroUsize::new(observer_count + 1).unwrap());
        let repeated_requests = case.bounded(NonZeroUsize::new(4).unwrap()) + 1;
        // Sealed: the label is built once and only ever read, by the closure
        // below and by every assertion the case fails with.
        let case_id: Box<str> = case.to_string().into_boxed_str();

        let result = camber::runtime::builder().worker_threads(2).run(|| {
            camber::runtime::block_on(run_generated_shutdown_case(
                observer_count,
                before_shutdown,
                repeated_requests,
                &case_id,
            ));
        });
        assert!(result.is_ok(), "{case_id}: runtime failed: {result:?}");
    }
}

async fn run_generated_shutdown_case(
    observer_count: usize,
    before_shutdown: usize,
    repeated_requests: usize,
    case_id: &str,
) {
    assert!(
        !camber::runtime::is_shutting_down(),
        "{case_id}: fresh runtime started in shutdown state"
    );

    let mut observers = Vec::with_capacity(observer_count);
    for _ in 0..before_shutdown {
        observers.push(spawn_registered_shutdown_observer(case_id).await);
    }

    // Admitted before the close transition; each constructs its wait only
    // after shutdown fires, so the sticky state is what releases it.
    let deferred = (before_shutdown..observer_count)
        .map(|_| spawn_deferred_shutdown_observer())
        .collect::<Box<[_]>>();

    camber::runtime::request_shutdown();
    assert!(
        camber::runtime::is_shutting_down(),
        "{case_id}: shutdown state did not become observable"
    );

    for _ in 0..repeated_requests {
        camber::runtime::request_shutdown();
        assert!(
            camber::runtime::is_shutting_down(),
            "{case_id}: repeated request reversed shutdown state"
        );
    }

    for (observer, start) in deferred {
        start.send(()).unwrap();
        observers.push(observer);
    }

    for observer in observers {
        let result = tokio::time::timeout_at(async_deadline(), observer).await;
        assert!(
            matches!(result, Ok(Ok(()))),
            "{case_id}: shutdown observer was not released: {result:?}"
        );
    }
}

/// An observer admitted now that constructs its shutdown wait only when its
/// start trigger fires.
fn spawn_deferred_shutdown_observer() -> (
    camber::task::AsyncJoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (start_tx, start_rx) = tokio::sync::oneshot::channel();
    let observer = camber::spawn_async(async move {
        start_rx.await.unwrap();
        camber::task::on_shutdown().await;
    });
    (observer, start_tx)
}

async fn spawn_registered_shutdown_observer(case_id: &str) -> camber::task::AsyncJoinHandle<()> {
    let (poll_tx, poll_rx) = tokio::sync::oneshot::channel();
    let (registered_tx, registered_rx) = tokio::sync::oneshot::channel();
    let observer = camber::spawn_async(async move {
        let shutdown = camber::task::on_shutdown();
        tokio::pin!(shutdown);
        tokio::select! {
            biased;
            () = &mut shutdown => return,
            result = poll_rx => assert!(matches!(result, Ok(()))),
        }
        registered_tx.send(()).unwrap();
        shutdown.await;
    });

    poll_tx.send(()).unwrap();
    let registered = tokio::time::timeout_at(async_deadline(), registered_rx).await;
    assert!(
        matches!(registered, Ok(Ok(()))),
        "{case_id}: observer did not retain a pending shutdown wait: {registered:?}"
    );
    observer
}

#[camber::test]
async fn on_shutdown_stops_schedule_loop() {
    let (callback, mut probe) = AsyncProbe::new();
    let handle =
        camber::schedule::every_async(CALLBACK_INTERVAL, move || callback.clone().run()).unwrap();
    let observer_handle = handle.clone();
    let observer = camber::spawn_async(async move {
        camber::task::on_shutdown().await;
        observer_handle.cancel();
    });
    let deadline = async_deadline();

    probe.observe_and_release(deadline).await;
    probe.observe(deadline).await;
    camber::runtime::request_shutdown();
    probe.release();
    assert!(matches!(
        tokio::time::timeout_at(deadline, observer).await,
        Ok(Ok(()))
    ));
    probe.assert_disconnected(deadline).await;
}

#[camber::test]
async fn on_shutdown_works_in_select() {
    let trigger = tokio::sync::Notify::new();
    trigger.notify_one();
    let trigger_won = tokio::select! {
        () = camber::task::on_shutdown() => false,
        () = trigger.notified() => true,
    };
    assert!(trigger_won);

    camber::runtime::request_shutdown();
    let shutdown_won = tokio::select! {
        () = camber::task::on_shutdown() => true,
        () = trigger.notified() => false,
    };
    assert!(shutdown_won);
}

#[test]
fn on_cancel_triggers_shutdown() {
    const MODE: &str = "runtime-on-cancel";
    const MARKER: &str = "runtime-on-cancel-complete";
    const TEST_NAME: &str = "shutdown_observation::on_cancel_triggers_shutdown";

    crate::common::run_in_child(TEST_NAME, MODE, MARKER, BOUND, || {
        notification_cancel_stops_schedule();
        token_cancel_stops_schedule();
        delayed_cancel_bounds_schedule();
    });
}

fn notification_cancel_stops_schedule() {
    camber::runtime::test(|| {
        let cancel = std::sync::Arc::new(tokio::sync::Notify::new());
        camber::on_cancel(std::sync::Arc::clone(&cancel).notified_owned());
        assert_schedule_stops_after("notification cancellation", move || cancel.notify_one());
    })
    .unwrap();
}

fn token_cancel_stops_schedule() {
    camber::runtime::test(|| {
        let token = tokio_util::sync::CancellationToken::new();
        camber::on_cancel(token.clone().cancelled_owned());
        assert_schedule_stops_after("token cancellation", move || token.cancel());
    })
    .unwrap();
}

fn assert_schedule_stops_after(context: &str, cancel: impl FnOnce()) {
    let (callback, probe) = SyncProbe::new(());
    camber::schedule::every(CALLBACK_INTERVAL, move || callback.run()).unwrap();
    let deadline = sync_deadline();

    probe.observe_and_release(deadline);
    probe.observe(deadline);
    cancel();
    wait_for_shutdown(context, deadline);
    probe.release();
    probe.assert_disconnected(deadline);
}

fn delayed_cancel_bounds_schedule() {
    camber::runtime::test(|| {
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
        camber::on_cancel(async move {
            assert!(matches!(cancel_rx.await, Ok(())));
        });
        let (callback, probe) = SyncProbe::new(());
        camber::schedule::every(CALLBACK_INTERVAL, move || callback.run()).unwrap();
        let deadline = sync_deadline();

        probe.observe_and_release(deadline);
        probe.observe_and_release(deadline);
        probe.observe(deadline);
        cancel_tx.send(()).unwrap();
        wait_for_shutdown("delayed cancellation", deadline);
        probe.release();
        probe.assert_disconnected(deadline);
    })
    .unwrap();
}

fn wait_for_shutdown(context: &str, deadline: std::time::Instant) {
    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(camber::timeout(
            deadline_left(deadline),
            camber::task::on_shutdown(),
        ))
    });
    assert!(
        matches!(result, Ok(())),
        "{context} did not complete shutdown: {result:?}"
    );
}
