use camber::{RuntimeError, Turn, TurnGuard, TurnRefused};
use std::future::{Future, pending, ready};
use std::pin::Pin;
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

fn poll<F: Future>(future: Pin<&mut F>) -> Poll<F::Output> {
    future.poll(&mut Context::from_waker(Waker::noop()))
}

fn granted(outcome: Poll<Result<TurnGuard, RuntimeError>>) -> TurnGuard {
    match outcome {
        Poll::Ready(Ok(guard)) => guard,
        other => panic!("expected a granted turn, got {other:?}"),
    }
}

fn refused(outcome: Poll<Result<TurnGuard, RuntimeError>>, expected: TurnRefused) {
    match outcome {
        Poll::Ready(Err(RuntimeError::Turn(actual))) => assert_eq!(actual, expected),
        other => panic!("expected {expected:?}, got {other:?}"),
    }
}

#[test]
fn turn_capacity_requires_a_nonzero_supported_count() {
    assert!(matches!(
        Turn::with_capacity(0),
        Err(RuntimeError::InvalidArgument(_))
    ));
    assert!(matches!(
        Turn::with_capacity(tokio::sync::Semaphore::MAX_PERMITS + 1),
        Err(RuntimeError::InvalidArgument(_))
    ));
    assert_eq!(Turn::default().queued(), 0);
}

#[test]
fn turn_capacity_accepts_the_semaphore_limit() {
    let widest = Turn::with_capacity(tokio::sync::Semaphore::MAX_PERMITS).unwrap();
    let first = granted(poll(Box::pin(widest.acquire()).as_mut()));
    let second = granted(poll(Box::pin(widest.acquire()).as_mut()));
    assert_eq!(widest.queued(), 0);
    drop((first, second));
}

#[test]
fn turn_fifo_counts_reserved_waiters_until_delivery() {
    let turn = Turn::new();
    let held = granted(poll(Box::pin(turn.acquire()).as_mut()));
    let mut first = Box::pin(turn.acquire());
    let mut second = Box::pin(turn.acquire());
    assert!(poll(first.as_mut()).is_pending());
    assert!(poll(second.as_mut()).is_pending());
    assert_eq!(turn.queued(), 2);
    drop(held);
    assert_eq!(turn.queued(), 2, "a reserved permit has not been delivered");
    assert!(
        poll(second.as_mut()).is_pending(),
        "later waiter cannot overtake"
    );
    let first = granted(poll(first.as_mut()));
    assert_eq!(turn.queued(), 1);
    drop(first);
    let second = granted(poll(second.as_mut()));
    assert_eq!(turn.queued(), 0);
    drop(second);
}

#[test]
fn turn_unpolled_and_immediate_acquisitions_are_not_queued() {
    let turn = Turn::with_capacity(2).unwrap();
    let unpolled = turn.acquire();
    assert_eq!(turn.queued(), 0);
    drop(unpolled);
    let first = granted(poll(Box::pin(turn.acquire()).as_mut()));
    let second = granted(poll(Box::pin(turn.acquire()).as_mut()));
    assert_eq!(turn.queued(), 0);
    let mut waiting = Box::pin(turn.acquire());
    assert!(poll(waiting.as_mut()).is_pending());
    assert_eq!(turn.queued(), 1);
    drop(waiting);
    assert_eq!(turn.queued(), 0);
    drop((first, second));
}

#[test]
fn turn_dropped_middle_and_reserved_waiters_restore_capacity() {
    let turn = Turn::new();
    let held = granted(poll(Box::pin(turn.acquire()).as_mut()));
    let mut first = Box::pin(turn.acquire());
    let mut middle = Box::pin(turn.acquire());
    let mut last = Box::pin(turn.acquire());
    assert!(poll(first.as_mut()).is_pending());
    assert!(poll(middle.as_mut()).is_pending());
    assert!(poll(last.as_mut()).is_pending());
    drop(middle);
    assert_eq!(turn.queued(), 2);
    drop(held);
    drop(first);
    assert_eq!(turn.queued(), 1);
    let last = granted(poll(last.as_mut()));
    assert_eq!(turn.queued(), 0);
    drop(last);
    drop(granted(poll(Box::pin(turn.acquire()).as_mut())));
}

#[test]
fn turn_close_is_shared_and_refuses_reserved_and_future_waiters() {
    let turn = Turn::new();
    let shared = turn.clone();
    let held = granted(poll(Box::pin(turn.acquire()).as_mut()));
    let mut waiting = Box::pin(turn.acquire());
    assert!(poll(waiting.as_mut()).is_pending());
    drop(held);
    shared.close();
    assert!(turn.is_closed());
    refused(poll(waiting.as_mut()), TurnRefused::Closed);
    assert_eq!(turn.queued(), 0);
    refused(poll(Box::pin(turn.acquire()).as_mut()), TurnRefused::Closed);
}

#[test]
fn turn_ready_refusals_precede_grants_without_requiring_tokio() {
    let turn = Turn::new();
    let expired = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
    let mut cancelled = Box::pin(turn.acquire_until(Some(expired), ready(42)));
    refused(poll(cancelled.as_mut()), TurnRefused::Cancelled);
    let mut elapsed = Box::pin(turn.acquire_until(Some(expired), pending::<()>()));
    refused(poll(elapsed.as_mut()), TurnRefused::Deadline);
    let future = Instant::now() + Duration::from_secs(10);
    let mut immediate = Box::pin(turn.acquire_until(Some(future), pending::<()>()));
    let guard = granted(poll(immediate.as_mut()));
    assert_eq!(turn.queued(), 0);
    drop(guard);
}

#[test]
fn turn_pending_timed_wait_without_tokio_has_no_queue_residue() {
    let turn = Turn::new();
    let held = granted(poll(Box::pin(turn.acquire()).as_mut()));
    let mut waiting = Box::pin(turn.acquire_until(
        Some(Instant::now() + Duration::from_secs(10)),
        pending::<()>(),
    ));
    assert!(matches!(
        poll(waiting.as_mut()),
        Poll::Ready(Err(RuntimeError::NoRuntime))
    ));
    assert_eq!(turn.queued(), 0);
    drop(held);
    drop(granted(poll(Box::pin(turn.acquire()).as_mut())));
}

#[test]
fn turn_ready_outcomes_work_with_a_disabled_timer_driver() {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(async {
            let turn = Turn::new();
            let expired = Instant::now().checked_sub(Duration::from_secs(1)).unwrap();
            refused(
                poll(Box::pin(turn.acquire_until(Some(expired), pending::<()>())).as_mut()),
                TurnRefused::Deadline,
            );
            let future = Instant::now() + Duration::from_secs(10);
            drop(granted(poll(
                Box::pin(turn.acquire_until(Some(future), pending::<()>())).as_mut(),
            )));
        });
}

/// A pending timed wait needs Tokio's timer, as `tokio::time::sleep` does.
///
/// Tokio reports a disabled driver only by panicking, so the wait panics too.
/// The unwind still drops the queued acquisition.
#[test]
fn turn_pending_wait_with_a_disabled_timer_driver_panics_like_tokio_time() {
    let turn = Turn::new();
    let timerless = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let held = timerless.block_on(turn.acquire()).unwrap();
    let future = Instant::now() + Duration::from_secs(10);
    let waited = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        timerless.block_on(turn.acquire_until(Some(future), pending::<()>()))
    }));
    let payload = waited.expect_err("a timed wait without a timer driver must panic");
    let message = payload
        .downcast_ref::<String>()
        .map(String::as_str)
        .or_else(|| payload.downcast_ref::<&str>().copied())
        .unwrap_or_default();
    assert!(message.contains("timers are disabled"), "{message}");
    assert_eq!(turn.queued(), 0);
    drop(held);
}

#[tokio::test(start_paused = true)]
async fn turn_expired_submillisecond_deadline_beats_a_reserved_grant() {
    let turn = Turn::new();
    let held = turn.acquire().await.unwrap();
    let deadline = (tokio::time::Instant::now() + Duration::from_micros(500)).into_std();
    let mut waiting = Box::pin(turn.acquire_until(Some(deadline), pending::<()>()));
    assert!(poll(waiting.as_mut()).is_pending());
    tokio::time::advance(Duration::from_micros(500)).await;
    drop(held);
    refused(poll(waiting.as_mut()), TurnRefused::Deadline);
    assert_eq!(turn.queued(), 0);
    drop(turn.acquire().await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn turn_expired_wait_removes_its_queue_position() {
    let turn = Turn::new();
    let held = turn.acquire().await.unwrap();
    let deadline = (tokio::time::Instant::now() + Duration::from_secs(5)).into_std();
    let mut waiting = Box::pin(turn.acquire_until(Some(deadline), pending::<()>()));
    assert!(poll(waiting.as_mut()).is_pending());
    assert_eq!(turn.queued(), 1);
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(matches!(
        waiting.await,
        Err(RuntimeError::Turn(TurnRefused::Deadline))
    ));
    assert_eq!(turn.queued(), 0);
    drop(held);
    drop(turn.acquire().await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn turn_cancellation_beats_simultaneous_deadline_and_reserved_grant() {
    let turn = Turn::new();
    let held = turn.acquire().await.unwrap();
    let deadline = (tokio::time::Instant::now() + Duration::from_secs(5)).into_std();
    let (cancel, cancelled) = tokio::sync::oneshot::channel::<()>();
    let mut waiting = Box::pin(turn.acquire_until(Some(deadline), cancelled));
    assert!(poll(waiting.as_mut()).is_pending());
    drop(held);
    cancel.send(()).unwrap();
    tokio::time::advance(Duration::from_secs(5)).await;
    assert!(matches!(
        waiting.await,
        Err(RuntimeError::Turn(TurnRefused::Cancelled))
    ));
    assert_eq!(turn.queued(), 0);
    drop(turn.acquire().await.unwrap());
}

#[tokio::test(start_paused = true)]
async fn turn_heartbeat_polls_keep_the_original_queue_position() {
    let turn = Turn::new();
    let held = turn.acquire().await.unwrap();
    let mut first = Box::pin(turn.acquire_until(None, pending::<()>()));
    assert!(poll(first.as_mut()).is_pending());
    let mut second = Box::pin(turn.acquire());
    assert!(poll(second.as_mut()).is_pending());
    for _ in 0..3 {
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            result = &mut first => panic!("turn was granted before release: {result:?}"),
        }
        assert_eq!(turn.queued(), 2);
    }
    drop(held);
    assert!(poll(second.as_mut()).is_pending());
    let guard = first.await.unwrap();
    assert_eq!(turn.queued(), 1);
    drop(guard);
    drop(second.await.unwrap());
    assert_eq!(turn.queued(), 0);
}

#[tokio::test]
async fn turn_close_wakes_queued_waiters_while_holders_remain_valid() {
    let turn = Turn::new();
    let held = turn.acquire().await.unwrap();
    let shared = turn.clone();
    let (registered, observed) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn(async move {
        let mut acquired = Box::pin(shared.acquire());
        assert!(poll(acquired.as_mut()).is_pending());
        registered.send(()).unwrap();
        acquired.await
    });
    observed.await.unwrap();
    turn.close();
    let result = tokio::time::timeout(Duration::from_secs(2), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(RuntimeError::Turn(TurnRefused::Closed))
    ));
    assert_eq!(turn.queued(), 0);
    assert_eq!(
        format!("{held:?}"),
        "TurnGuard { permits: 1 }",
        "closure does not revoke a delivered guard"
    );
    drop(held);
    refused(poll(Box::pin(turn.acquire()).as_mut()), TurnRefused::Closed);
    assert_eq!(turn.queued(), 0);
}

#[camber::test]
async fn turn_guard_in_blocking_work_outlives_its_result_waiter() {
    let turn = Turn::new();
    let guard = turn.acquire().await.unwrap();
    let (started, observed) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::channel();
    let handle = camber::spawn(move || {
        started.send(()).unwrap();
        released.recv().unwrap();
        drop(guard);
    });
    observed.await.unwrap();
    drop(handle);
    let mut waiting = Box::pin(turn.acquire());
    assert!(poll(waiting.as_mut()).is_pending());
    assert_eq!(turn.queued(), 1);
    release.send(()).unwrap();
    drop(
        tokio::time::timeout(Duration::from_secs(2), waiting)
            .await
            .unwrap()
            .unwrap(),
    );
    assert_eq!(turn.queued(), 0);
}
