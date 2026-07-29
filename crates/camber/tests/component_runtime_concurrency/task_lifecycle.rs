use crate::common::{BOUND, probe_paused_window, run_in_child, spawn_server_ready};
use camber::http::{self, Request, Response, Router};
use camber::runtime_test_support::{RuntimeCheckpoint, runtime_schedule};
use camber::{RuntimeError, runtime, spawn};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// Bounds each leg of this module's scope-checkpoint rendezvous.
///
/// Deliberately short of the parent's ten-second isolation bound even when
/// every leg spends it: a run the parent kills discards the structured tuple
/// that localizes which leg failed.
const SCOPE_EVENT_BOUND: Duration = Duration::from_secs(2);
const FINAL_TASK_MODE: &str = "runtime-final-task-scope-wait";
const FINAL_TASK_MARKER: &str = "runtime-final-task-scope-wait-complete";
const FINAL_TASK_TEST: &str = "task_lifecycle::final_task_completion_cannot_miss_scope_waiter";
const NESTED_RUNTIME_MODE: &str = "runtime-nested-rejection";
const NESTED_RUNTIME_MARKER: &str = "runtime-nested-rejection-complete";
const NESTED_RUNTIME_TEST: &str =
    "task_lifecycle::nested_runtime_is_rejected_without_corrupting_outer_context";

/// The drain awaits unjoined BLOCKING children: `run` returns only once every
/// `camber::spawn` body has finished, with no handle joined.
#[test]
fn spawned_tasks_complete_before_runtime_exits() {
    let counter = Arc::new(AtomicUsize::new(0));
    run_unjoined_blocking_tasks(Arc::clone(&counter));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        5,
        "the runtime returned before every unjoined blocking child completed"
    );
}

#[test]
fn join_handle_returns_task_result() {
    let result = runtime::run(|| spawn(|| 42).join()).unwrap();
    assert_eq!(result.unwrap(), 42);
}

#[test]
fn join_handle_returns_error_on_task_panic() {
    let result = runtime::run(|| {
        spawn(|| {
            assert_eq!(String::from("actual"), "expected", "intentional test panic");
        })
        .join()
    })
    .unwrap();

    assert!(matches!(result, Err(RuntimeError::TaskPanicked(_))));
}

#[camber::test]
async fn spawn_inside_handler_does_not_deadlock() {
    let mut router = Router::new();
    router.get("/compute", |_: &Request| async {
        let result = spawn(|| "computed").join().unwrap();
        Response::text(200, result)
    });

    let server = spawn_server_ready(router, Duration::from_secs(2)).unwrap();
    let counter = Arc::new(AtomicUsize::new(0));
    let handles = (0..4)
        .map(|_| {
            let request_counter = Arc::clone(&counter);
            let url = format!("http://{}/compute", server.local_addr());
            camber::spawn_async(async move {
                let response = http::get(&url).await.unwrap();
                assert_eq!(response.status(), 200);
                assert_eq!(response.body(), "computed");
                request_counter.fetch_add(1, Ordering::SeqCst);
            })
        })
        .collect::<Box<[_]>>();

    for handle in handles {
        handle.await.unwrap();
    }
    assert_eq!(counter.load(Ordering::SeqCst), 4);
    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}

/// The same structured-concurrency guarantee for unjoined ASYNC children,
/// which the blocking case above does not cover: `camber::spawn_async` bodies
/// still resolve before `run` returns even though nothing awaited their
/// handles.
#[test]
fn structured_concurrency_waits_for_spawned_tasks() {
    let counter = Arc::new(AtomicUsize::new(0));
    run_unjoined_async_tasks(Arc::clone(&counter));
    assert_eq!(
        counter.load(Ordering::SeqCst),
        5,
        "the runtime returned before every unjoined async child completed"
    );
}

fn run_unjoined_blocking_tasks(counter: Arc<AtomicUsize>) {
    runtime::run(move || {
        let (lifecycle_tx, lifecycle_rx) = camber::channel::bounded::<()>(1);
        (0..5).for_each(|_| {
            let task_counter = Arc::clone(&counter);
            let task_lifecycle = lifecycle_rx.clone();
            spawn(move || {
                assert!(matches!(
                    task_lifecycle.recv(),
                    Err(RuntimeError::ChannelClosed)
                ));
                task_counter.fetch_add(1, Ordering::SeqCst);
            });
        });
        drop(lifecycle_rx);
        drop(lifecycle_tx);
    })
    .unwrap();
}

fn run_unjoined_async_tasks(counter: Arc<AtomicUsize>) {
    runtime::run(move || {
        // Each child parks on its own lifecycle sender and wakes only when the
        // closure drops it, so no child can have finished before the closure
        // returned: whatever the counter reads afterwards, the drain awaited.
        let lifecycle = (0..5)
            .map(|_| {
                let (lifecycle_tx, lifecycle_rx) = tokio::sync::oneshot::channel::<()>();
                let task_counter = Arc::clone(&counter);
                camber::spawn_async(async move {
                    assert!(lifecycle_rx.await.is_err());
                    task_counter.fetch_add(1, Ordering::SeqCst);
                });
                lifecycle_tx
            })
            .collect::<Box<[_]>>();
        drop(lifecycle);
    })
    .unwrap();
}

#[test]
fn final_task_completion_cannot_miss_scope_waiter() {
    run_in_child(
        FINAL_TASK_TEST,
        FINAL_TASK_MODE,
        FINAL_TASK_MARKER,
        BOUND,
        run_final_task_scope,
    );
}

fn run_final_task_scope() {
    let completed = Arc::new(AtomicUsize::new(0));
    let task_completed = Arc::clone(&completed);
    let controller = runtime_schedule();
    let checkpoint = RuntimeCheckpoint::ScopeWaitObserved(1);
    controller.pause_once(checkpoint).unwrap();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let (finishing_tx, finishing_rx) = std::sync::mpsc::channel();

    let observations = std::thread::scope(|scope| {
        let checkpoint_controller = &controller;
        let checkpoint_driver = scope.spawn(move || {
            // The shared helper owns both bounded waits and releases the pause
            // even when the probe between them unwinds, so a stranded runtime
            // becomes an ordinary failure. `None` reports a window that was
            // never observed or never released.
            probe_paused_window(checkpoint_controller, checkpoint, SCOPE_EVENT_BOUND, || {
                let started = finish_tx.send(()).is_ok();
                let finished = finishing_rx.recv_timeout(SCOPE_EVENT_BOUND).is_ok();
                (started, finished)
            })
        });

        runtime::builder()
            .worker_threads(1)
            .with_test_schedule(&controller)
            .run(move || {
                let final_task = camber::spawn_async(async move {
                    assert!(matches!(finish_rx.await, Ok(())));
                    task_completed.fetch_add(1, Ordering::SeqCst);
                });
                // One Tokio worker makes this observer run only after the final
                // task's poll drops its task-tracker guard.
                tokio::spawn(async move {
                    assert!(matches!(final_task.await, Ok(())));
                    finishing_tx.send(()).unwrap();
                });
            })
            .unwrap();
        checkpoint_driver.join().unwrap()
    });

    assert_eq!(
        observations,
        Some((true, true)),
        "the scope-wait rendezvous did not complete (started, finished)"
    );
    assert_eq!(
        completed.load(Ordering::SeqCst),
        1,
        "runtime returned before its final tracked task completed"
    );
}

#[test]
fn nested_runtime_is_rejected_without_corrupting_outer_context() {
    run_in_child(
        NESTED_RUNTIME_TEST,
        NESTED_RUNTIME_MODE,
        NESTED_RUNTIME_MARKER,
        BOUND,
        assert_nested_runtime_rejection,
    );
}

fn assert_nested_runtime_rejection() {
    runtime::test(|| {
        let nested =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| runtime::test(|| ())));
        let nested = match nested {
            Ok(result) => result,
            Err(_) => panic!("nested runtime panicked instead of returning a rejection"),
        };
        match nested {
            Err(RuntimeError::InvalidArgument(message)) => assert_eq!(
                message.as_ref(),
                "nested runtime creation is not supported",
                "nested runtime rejection changed its public context"
            ),
            result => panic!("nested runtime was not rejected: {result:?}"),
        }

        assert!(
            !runtime::is_shutting_down(),
            "nested rejection corrupted the outer runtime state"
        );
        assert_eq!(
            spawn(|| 42).join().unwrap(),
            42,
            "outer runtime could not execute a task after nested rejection"
        );
        runtime::request_shutdown();
        assert!(
            runtime::is_shutting_down(),
            "outer runtime was unusable after nested rejection"
        );
        runtime::block_on(camber::task::on_shutdown());
    })
    .unwrap();
}
