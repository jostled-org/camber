use camber::RuntimeError;
use std::time::Duration;

const MIGRATION_MODE: &str = "runtime-context-worker-migration";
const MIGRATION_MARKER: &str = "runtime-context-worker-migration-complete";
const MIGRATION_TEST: &str = "async_tasks::runtime_context_follows_task_after_worker_migration";

#[camber::test]
async fn spawn_async_returns_value() {
    assert_eq!(camber::spawn_async(async { 42 }).await.unwrap(), 42);
}

#[camber::test]
async fn spawn_async_cancel_returns_error() {
    let handle = camber::spawn_async(std::future::pending::<i32>());
    handle.cancel();

    assert!(matches!(handle.await, Err(RuntimeError::Cancelled)));
}

#[camber::test]
async fn spawn_async_participates_in_structured_concurrency() {
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    camber::spawn_async(async move {
        completed_tx.send(42).unwrap();
    });

    assert_eq!(completed_rx.await.unwrap(), 42);
}

#[camber::test]
async fn spawn_async_panic_returns_error() {
    let result = camber::spawn_async(async {
        assert_eq!(String::from("actual"), "expected", "intentional test panic");
    })
    .await;

    assert!(matches!(result, Err(RuntimeError::TaskPanicked(_))));
}

#[test]
fn runtime_context_follows_task_after_worker_migration() {
    match crate::process_support::is_private_child(MIGRATION_MODE) {
        true => {
            let observed_shutdown = force_runtime_task_migration();
            println!("{MIGRATION_MARKER}");
            assert!(
                observed_shutdown,
                "Camber runtime context was lost after Tokio moved the task"
            );
            return;
        }
        false => {}
    }

    let run = crate::process_support::run_isolated_exact(
        MIGRATION_TEST,
        MIGRATION_MODE,
        MIGRATION_MARKER,
        Duration::from_secs(10),
    )
    .unwrap();
    assert!(
        run.success(),
        "isolated worker-migration contract failed: {}",
        String::from_utf8_lossy(run.stderr())
    );
}

fn force_runtime_task_migration() -> bool {
    camber::runtime::builder()
        .worker_threads(2)
        .shutdown_timeout(Duration::from_millis(100))
        .run(|| {
            camber::runtime::block_on(async {
                let (blocker_entered_tx, blocker_entered_rx) = std::sync::mpsc::channel();
                let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
                let (release_blocker_tx, release_blocker_rx) = std::sync::mpsc::channel();
                let release_after_timeout = release_blocker_tx.clone();
                let resume_from_outside_runtime = std::thread::spawn(move || {
                    blocker_entered_rx.recv().unwrap();
                    resume_tx.send(()).unwrap();
                });

                let task = camber::spawn_async(async move {
                    camber::runtime::request_shutdown();
                    let initial_worker = std::thread::current().id();

                    // This task enters the current worker's local queue. Once it
                    // blocks that worker, Tokio must steal and resume us elsewhere.
                    tokio::spawn(async move {
                        blocker_entered_tx.send(()).unwrap();
                        release_blocker_rx.recv().unwrap();
                    });
                    // An external thread wakes this task into Tokio's global
                    // queue, making the free worker poll it.
                    resume_rx.await.unwrap();

                    let resumed_worker = std::thread::current().id();
                    assert_ne!(
                        initial_worker, resumed_worker,
                        "Tokio task did not migrate from its occupied worker"
                    );
                    let observed_shutdown = camber::runtime::is_shutting_down();
                    let _ = release_blocker_tx.send(());
                    observed_shutdown
                });

                let result = tokio::time::timeout(Duration::from_secs(3), task).await;
                // Ensure the occupied worker is recoverable even if migration broke.
                let _ = release_after_timeout.send(());
                resume_from_outside_runtime.join().unwrap();
                result.unwrap().unwrap()
            })
        })
        .unwrap()
}
