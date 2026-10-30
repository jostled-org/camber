use crate::common::BOUND;
use camber::{RuntimeError, channel, runtime, spawn, spawn_async};
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll, Wake, Waker};

struct WaiterProbe {
    dropped: Arc<AtomicBool>,
    wakes: AtomicUsize,
}

impl Wake for WaiterProbe {
    fn wake(self: Arc<Self>) {
        self.wakes.fetch_add(1, Ordering::Relaxed);
    }
}

impl Drop for WaiterProbe {
    fn drop(&mut self) {
        self.dropped.store(true, Ordering::Release);
    }
}

struct ResultProbe(Arc<AtomicUsize>);

impl Drop for ResultProbe {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Release);
    }
}

struct CompletionWake(std::sync::mpsc::SyncSender<()>);

impl Wake for CompletionWake {
    fn wake(self: Arc<Self>) {
        self.0.send(()).unwrap();
    }
}

#[test]
fn unread_blocking_result_is_destroyed_when_the_waiter_is_dropped() {
    let drops = Arc::new(AtomicUsize::new(0));
    let task_drops = Arc::clone(&drops);
    runtime::run(|| {
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(1);
        let (woken_tx, woken_rx) = std::sync::mpsc::sync_channel(1);
        let waker = Waker::from(Arc::new(CompletionWake(woken_tx)));
        let mut future = spawn(move || {
            release_rx.recv_timeout(BOUND).unwrap();
            ResultProbe(task_drops)
        })
        .into_future();
        let mut context = Context::from_waker(&waker);
        assert!(Pin::new(&mut future).poll(&mut context).is_pending());
        release_tx.send(()).unwrap();
        woken_rx
            .recv_timeout(BOUND)
            .expect("result delivery must wake the waiter");
        assert_eq!(drops.load(Ordering::Acquire), 0);
        drop(future);
        assert_eq!(drops.load(Ordering::Acquire), 1);
    })
    .unwrap();
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn dropped_blocking_waiter_releases_its_registered_waker() {
    runtime::run(|| {
        let dropped = Arc::new(AtomicBool::new(false));
        let waker = Waker::from(Arc::new(WaiterProbe {
            dropped: Arc::clone(&dropped),
            wakes: AtomicUsize::new(0),
        }));
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let mut future = spawn(move || {
            assert!(release_rx.recv_timeout(BOUND).is_err());
        })
        .into_future();
        {
            let mut context = Context::from_waker(&waker);
            assert!(Pin::new(&mut future).poll(&mut context).is_pending());
        }
        drop(waker);
        assert!(!dropped.load(Ordering::Acquire));
        drop(future);
        assert!(
            dropped.load(Ordering::Acquire),
            "the live closure retained its abandoned waiter"
        );
        drop(release_tx);
    })
    .unwrap();
}

#[test]
fn abandoned_blocking_result_is_destroyed_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let task_drops = Arc::clone(&drops);
    runtime::run(|| {
        let (release_tx, release_rx) = std::sync::mpsc::channel::<()>();
        let future = spawn(move || {
            assert!(release_rx.recv_timeout(BOUND).is_err());
            ResultProbe(task_drops)
        })
        .into_future();
        drop(future);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        drop(release_tx);
    })
    .unwrap();
    assert_eq!(drops.load(Ordering::Acquire), 1);
}

#[test]
fn awaiting_blocking_result_keeps_the_only_worker_available() {
    runtime::builder()
        .worker_threads(1)
        .run(|| {
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            let (release_tx, release_rx) = std::sync::mpsc::channel();
            let waiting = spawn_async(async move {
                spawn(move || {
                    started_tx.send(()).unwrap();
                    release_rx
                        .recv_timeout(BOUND)
                        .expect("async sibling must run");
                    42
                })
                .await
            });
            let sibling = spawn_async(async move {
                started_rx.await.unwrap();
                release_tx.send(()).unwrap();
            });
            runtime::block_on(async {
                assert_eq!(waiting.await.unwrap().unwrap(), 42);
                sibling.await.unwrap();
            });
        })
        .unwrap();
}

#[camber::test]
async fn awaited_blocking_task_returns_its_value() {
    assert_eq!(spawn(|| 17).await.unwrap(), 17);
}

#[camber::test]
async fn awaited_blocking_panic_maps_to_task_panicked() {
    let result = spawn(|| panic!("blocking result panic")).await;

    assert!(matches!(
        result,
        Err(RuntimeError::TaskPanicked(message)) if message.as_ref() == "blocking result panic"
    ));
}

#[camber::test]
async fn cancel_before_await_reaches_the_task_io_boundary() {
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = channel::bounded::<()>(1);
    let handle = spawn(move || {
        started_tx.send(()).unwrap();
        release_rx.recv()
    });
    started_rx.await.unwrap();
    handle.cancel();

    assert!(matches!(handle.await, Ok(Err(RuntimeError::Cancelled))));
    drop(release_tx);
}

#[test]
fn refused_blocking_future_needs_no_executor_and_delivers_once() {
    let mut future = spawn(|| 1).into_future();
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        Pin::new(&mut future).poll(&mut context),
        Poll::Ready(Err(RuntimeError::NoRuntime))
    ));
    assert!(Pin::new(&mut future).poll(&mut context).is_pending());
}

#[test]
fn delivered_blocking_future_parks_on_subsequent_polls() {
    runtime::run(|| {
        runtime::block_on(async {
            let mut future = spawn(|| 23).into_future();
            assert_eq!((&mut future).await.unwrap(), 23);
            let mut context = Context::from_waker(Waker::noop());
            assert!(Pin::new(&mut future).poll(&mut context).is_pending());
        })
    })
    .unwrap();
}

#[test]
fn dropped_blocking_waiter_keeps_its_task_in_the_scope() {
    let (outcome_tx, outcome_rx) = std::sync::mpsc::sync_channel(1);
    runtime::run(|| {
        let (release_tx, release_rx) = channel::bounded::<u8>(1);
        let mut future = spawn(move || outcome_tx.send(release_rx.recv()).unwrap()).into_future();
        let mut context = Context::from_waker(Waker::noop());
        assert!(Pin::new(&mut future).poll(&mut context).is_pending());
        drop(future);
        assert!(outcome_rx.try_recv().is_err());
        release_tx.send(7).unwrap();
    })
    .unwrap();

    let outcome = outcome_rx.try_recv().unwrap();
    assert!(
        matches!(outcome, Ok(7)),
        "the abandoned task observed {outcome:?} at its IO boundary"
    );
}
