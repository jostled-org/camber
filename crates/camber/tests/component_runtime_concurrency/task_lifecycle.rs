use crate::http_support;
use camber::http::{self, Request, Response, Router};
use camber::runtime_test_support::{RuntimeCheckpoint, runtime_schedule};
use camber::{RuntimeError, runtime, spawn};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const FINAL_TASK_MODE: &str = "runtime-final-task-scope-wait";
const FINAL_TASK_MARKER: &str = "runtime-final-task-scope-wait-complete";
const FINAL_TASK_TEST: &str = "task_lifecycle::final_task_completion_cannot_miss_scope_waiter";
const NESTED_RUNTIME_MODE: &str = "runtime-nested-rejection";
const NESTED_RUNTIME_MARKER: &str = "runtime-nested-rejection-complete";
const NESTED_RUNTIME_TEST: &str =
    "task_lifecycle::nested_runtime_is_rejected_without_corrupting_outer_context";

#[test]
fn spawned_tasks_complete_before_runtime_exits() {
    let counter = Arc::new(AtomicUsize::new(0));
    run_unjoined_tasks(Arc::clone(&counter));
    assert_eq!(counter.load(Ordering::SeqCst), 5);
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

#[test]
fn spawn_join_returns_result() {
    runtime::run(|| {
        assert_eq!(spawn(|| 42).join().unwrap(), 42);

        let result = spawn(|| {
            assert_eq!(String::from("actual"), "expected", "intentional test panic");
        })
        .join();
        assert!(matches!(result, Err(RuntimeError::TaskPanicked(_))));
    })
    .unwrap();
}

#[camber::test]
async fn spawn_inside_handler_does_not_deadlock() {
    let mut router = Router::new();
    router.get("/compute", |_: &Request| async {
        let result = spawn(|| "computed").join().unwrap();
        Response::text(200, result)
    });

    let server = http_support::spawn_server_ready(router, Duration::from_secs(2)).unwrap();
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
        .collect::<Vec<_>>();

    for handle in handles {
        handle.await.unwrap();
    }
    assert_eq!(counter.load(Ordering::SeqCst), 4);
    server.shutdown_bounded(Duration::from_secs(2)).unwrap();
}

#[test]
fn structured_concurrency_waits_for_spawned_tasks() {
    let counter = Arc::new(AtomicUsize::new(0));
    run_unjoined_tasks(Arc::clone(&counter));
    assert_eq!(counter.load(Ordering::SeqCst), 5);
}

fn run_unjoined_tasks(counter: Arc<AtomicUsize>) {
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

#[test]
fn final_task_completion_cannot_miss_scope_waiter() {
    match crate::process_support::is_private_child(FINAL_TASK_MODE) {
        true => {
            run_final_task_scope();
            println!("{FINAL_TASK_MARKER}");
            return;
        }
        false => {}
    }

    let run = crate::process_support::run_isolated_exact(
        FINAL_TASK_TEST,
        FINAL_TASK_MODE,
        FINAL_TASK_MARKER,
        Duration::from_secs(10),
    )
    .unwrap();
    assert!(
        run.success(),
        "isolated final-task scope contract failed: {}",
        String::from_utf8_lossy(run.stderr())
    );
}

fn run_final_task_scope() {
    let completed = Arc::new(AtomicUsize::new(0));
    let task_completed = Arc::clone(&completed);
    let controller = runtime_schedule();
    let checkpoint = RuntimeCheckpoint::TaskWaitPredicateObserved(1);
    controller.pause_once(checkpoint).unwrap();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let (finishing_tx, finishing_rx) = std::sync::mpsc::channel();

    std::thread::scope(|scope| {
        let checkpoint_controller = &controller;
        let checkpoint_driver = scope.spawn(move || {
            checkpoint_controller.wait_until_paused(checkpoint).unwrap();
            finish_tx.send(()).unwrap();
            finishing_rx.recv().unwrap();
            checkpoint_controller.release(checkpoint).unwrap();
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
        checkpoint_driver.join().unwrap();
    });

    assert_eq!(
        completed.load(Ordering::SeqCst),
        1,
        "runtime returned before its final tracked task completed"
    );
}

#[test]
fn nested_runtime_is_rejected_without_corrupting_outer_context() {
    match crate::process_support::is_private_child(NESTED_RUNTIME_MODE) {
        true => {
            assert_nested_runtime_rejection();
            println!("{NESTED_RUNTIME_MARKER}");
            return;
        }
        false => {}
    }

    let run = crate::process_support::run_isolated_exact(
        NESTED_RUNTIME_TEST,
        NESTED_RUNTIME_MODE,
        NESTED_RUNTIME_MARKER,
        Duration::from_secs(10),
    )
    .unwrap();
    assert!(
        run.success(),
        "isolated nested-runtime contract failed: {}",
        String::from_utf8_lossy(run.stderr())
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
