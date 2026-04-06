use camber::{runtime, spawn};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::Duration;

#[test]
fn spawned_tasks_complete_before_runtime_exits() {
    let counter = Arc::new(AtomicUsize::new(0));

    runtime::run(|| {
        for _ in 0..5 {
            let counter = Arc::clone(&counter);
            spawn(move || {
                thread::sleep(Duration::from_millis(50));
                counter.fetch_add(1, Ordering::SeqCst);
            });
        }
    })
    .unwrap();

    assert_eq!(counter.load(Ordering::SeqCst), 5);
}

#[test]
fn join_handle_returns_task_result() {
    let result = runtime::run(|| {
        let handle = spawn(|| 42);
        handle.join()
    })
    .unwrap();
    assert_eq!(result.unwrap(), 42);
}

#[test]
fn join_handle_returns_error_on_task_panic() {
    let result = runtime::run(|| {
        let handle = spawn(|| {
            #[allow(clippy::panic)]
            {
                panic!("intentional test panic");
            }
        });
        handle.join()
    })
    .unwrap();
    assert!(result.is_err());
    assert!(matches!(
        result.unwrap_err(),
        camber::RuntimeError::TaskPanicked(_)
    ));
}
