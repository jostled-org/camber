//! Runtime absence is a typed outcome. No entry point fills it by minting a
//! default runtime, so every case here runs on a plain test thread that holds
//! neither the async task-local nor the synchronous thread-local context.

use crate::common::{block_on_detached, poll_until};
use camber::{RuntimeError, runtime, schedule};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long an inert observer is watched before it counts as never firing.
const INERT_BOUND: Duration = Duration::from_millis(250);

/// A spawn from a thread with no runtime context reports the absence through
/// the handle it already has, and runs no task body.
#[test]
fn spawns_without_runtime_context_resolve_no_runtime() {
    let blocking_ran = Arc::new(AtomicBool::new(false));
    let blocking_body = Arc::clone(&blocking_ran);
    let blocking = camber::spawn(move || blocking_body.store(true, Ordering::SeqCst)).join();

    let async_ran = Arc::new(AtomicBool::new(false));
    let async_body = Arc::clone(&async_ran);
    let spawned = camber::spawn_async(async move { async_body.store(true, Ordering::SeqCst) });

    assert!(
        matches!(blocking, Err(RuntimeError::NoRuntime)),
        "camber::spawn without a runtime did not resolve NoRuntime: {blocking:?}"
    );
    let spawned = block_on_detached(async { spawned.await });
    assert!(
        matches!(spawned, Err(RuntimeError::NoRuntime)),
        "camber::spawn_async without a runtime did not resolve NoRuntime: {spawned:?}"
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

/// The schedule constructors return `Result`, so they propagate the absence
/// before building any schedule rather than handing back an inert handle.
#[test]
fn schedule_constructors_return_no_runtime_error_without_a_runtime() {
    const INTERVAL: Duration = Duration::from_millis(5);
    let fired = Arc::new(AtomicBool::new(false));

    let every_fired = Arc::clone(&fired);
    let every = schedule::every(INTERVAL, move || every_fired.store(true, Ordering::SeqCst));

    let async_fired = Arc::clone(&fired);
    let every_async = schedule::every_async(INTERVAL, move || {
        let fired = Arc::clone(&async_fired);
        async move { fired.store(true, Ordering::SeqCst) }
    });

    let notified_fired = Arc::clone(&fired);
    let every_notified =
        schedule::every_async_notified(INTERVAL, Arc::new(tokio::sync::Notify::new()), move || {
            let fired = Arc::clone(&notified_fired);
            async move { fired.store(true, Ordering::SeqCst) }
        });

    let cron_fired = Arc::clone(&fired);
    let cron = schedule::cron("* * * * * *", move || {
        cron_fired.store(true, Ordering::SeqCst)
    });

    assert!(
        matches!(every, Err(RuntimeError::NoRuntime)),
        "schedule::every without a runtime did not report the absence: {every:?}"
    );
    assert!(
        matches!(every_async, Err(RuntimeError::NoRuntime)),
        "schedule::every_async without a runtime did not report the absence: {every_async:?}"
    );
    assert!(
        matches!(every_notified, Err(RuntimeError::NoRuntime)),
        "schedule::every_async_notified without a runtime did not report the absence: {every_notified:?}"
    );
    assert!(
        matches!(cron, Err(RuntimeError::NoRuntime)),
        "schedule::cron without a runtime did not report the absence: {cron:?}"
    );
    assert!(
        !fired_within(&fired, INERT_BOUND),
        "a refused schedule ran its callback"
    );
    let spawned = camber::spawn(|| ()).join();
    assert!(
        matches!(spawned, Err(RuntimeError::NoRuntime)),
        "a refused schedule minted a runtime on this thread: {spawned:?}"
    );
}

/// Watch `flag` for `window`, reporting whether anything ever set it.
///
/// Reading a refusal's flag at zero elapsed time cannot fail: it is the window
/// that discriminates, because a constructor which minted its own runtime and
/// started the `INTERVAL` loop before returning `Err` would fire dozens of
/// times inside one.
fn fired_within(flag: &AtomicBool, window: Duration) -> bool {
    poll_until(window, || flag.load(Ordering::SeqCst))
}

/// The fire-and-forget lifecycle surfaces stay total outside a runtime: they
/// are no-ops, mint nothing, and the shutdown observer watches an inert signal
/// that nothing can fire.
#[test]
fn public_lifecycle_calls_are_inert_without_a_runtime() {
    runtime::on_cancel(std::future::pending());
    runtime::request_shutdown();

    assert!(
        matches!(camber::spawn(|| ()).join(), Err(RuntimeError::NoRuntime)),
        "a lifecycle call minted a runtime on this thread"
    );
    assert!(
        !runtime::is_shutting_down(),
        "request_shutdown with no runtime reported a shutdown"
    );

    let observed =
        block_on_detached(async { tokio::time::timeout(INERT_BOUND, camber::on_shutdown()).await });
    assert!(
        observed.is_err(),
        "on_shutdown completed with no runtime to request shutdown"
    );
}
