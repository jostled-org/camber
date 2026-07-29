use crate::common::remaining;
use camber::channel;
use std::fmt::Debug;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

const SAFETY_TIMEOUT: Duration = Duration::from_secs(3);

/// How often a scheduled callback under probe is asked to fire.
///
/// Short enough that a probe observes several ticks well inside
/// [`SAFETY_TIMEOUT`], and shared so a case's tick budget and its deadline
/// cannot drift apart.
pub const CALLBACK_INTERVAL: Duration = Duration::from_millis(25);

pub fn async_deadline() -> tokio::time::Instant {
    tokio::time::Instant::now() + SAFETY_TIMEOUT
}

pub fn sync_deadline() -> Instant {
    Instant::now() + SAFETY_TIMEOUT
}

#[derive(Clone)]
pub struct AsyncCallback {
    event_tx: UnboundedSender<()>,
    release: Arc<tokio::sync::Semaphore>,
}

impl AsyncCallback {
    pub async fn run(self) {
        self.event_tx.send(()).unwrap();
        self.release.acquire_owned().await.unwrap().forget();
    }
}

pub struct AsyncProbe {
    event_rx: UnboundedReceiver<()>,
    release: Arc<tokio::sync::Semaphore>,
}

impl AsyncProbe {
    pub fn new() -> (AsyncCallback, Self) {
        let (event_tx, event_rx) = tokio::sync::mpsc::unbounded_channel();
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let callback = AsyncCallback {
            event_tx,
            release: Arc::clone(&release),
        };
        (callback, Self { event_rx, release })
    }

    pub async fn observe(&mut self, deadline: tokio::time::Instant) {
        let result = tokio::time::timeout_at(deadline, self.event_rx.recv()).await;
        assert!(
            matches!(result, Ok(Some(()))),
            "callback did not arrive before the aggregate safety deadline: {result:?}"
        );
    }

    pub async fn observe_and_release(&mut self, deadline: tokio::time::Instant) {
        self.observe(deadline).await;
        self.release();
    }

    pub fn release(&self) {
        self.release.add_permits(1);
    }

    pub async fn assert_disconnected(&mut self, deadline: tokio::time::Instant) {
        let result = tokio::time::timeout_at(deadline, self.event_rx.recv()).await;
        assert!(
            matches!(result, Ok(None)),
            "callback channel did not disconnect before the aggregate safety deadline: {result:?}"
        );
    }
}

pub struct SyncCallback<T> {
    event_tx: std::sync::mpsc::Sender<T>,
    event: T,
    release_rx: channel::Receiver<()>,
}

impl<T> SyncCallback<T>
where
    T: Copy + Send + 'static,
{
    pub fn new(event_tx: std::sync::mpsc::Sender<T>, event: T) -> (Self, SyncRelease) {
        let (release_tx, release_rx) = channel::bounded(1);
        (
            Self {
                event_tx,
                event,
                release_rx,
            },
            SyncRelease { release_tx },
        )
    }

    pub fn run(&self) {
        self.event_tx.send(self.event).unwrap();
        self.release_rx.recv().unwrap();
    }
}

pub struct SyncRelease {
    release_tx: channel::Sender<()>,
}

impl SyncRelease {
    pub fn release(&self) {
        self.release_tx.send(()).unwrap();
    }
}

pub struct SyncProbe<T> {
    event_rx: std::sync::mpsc::Receiver<T>,
    release: SyncRelease,
}

impl<T> SyncProbe<T>
where
    T: Copy + Debug + Send + 'static,
{
    pub fn new(event: T) -> (SyncCallback<T>, Self) {
        let (event_tx, event_rx) = std::sync::mpsc::channel();
        let (callback, release) = SyncCallback::new(event_tx, event);
        (callback, Self { event_rx, release })
    }

    pub fn receive(receiver: &std::sync::mpsc::Receiver<T>, deadline: Instant) -> T {
        let result = receiver.recv_timeout(remaining(deadline));
        assert!(
            result.is_ok(),
            "callback missed aggregate safety deadline: {result:?}"
        );
        result.unwrap()
    }

    pub fn observe(&self, deadline: Instant) -> T {
        Self::receive(&self.event_rx, deadline)
    }

    pub fn observe_and_release(&self, deadline: Instant) -> T {
        let event = self.observe(deadline);
        self.release();
        event
    }

    pub fn release(&self) {
        self.release.release();
    }

    pub fn assert_disconnected(&self, deadline: Instant) {
        Self::assert_receiver_disconnected(&self.event_rx, deadline);
    }

    pub fn assert_receiver_disconnected(
        receiver: &std::sync::mpsc::Receiver<T>,
        deadline: Instant,
    ) {
        loop {
            match receiver.recv_timeout(remaining(deadline)) {
                Ok(_) => {}
                Err(error) => {
                    assert!(
                        matches!(error, std::sync::mpsc::RecvTimeoutError::Disconnected),
                        "callback channel did not disconnect before aggregate safety deadline"
                    );
                    return;
                }
            }
        }
    }
}

/// What is left of `deadline`, refusing a budget already spent.
///
/// [`crate::common::remaining`] saturates to zero, which is right for a wait
/// whose own expiry is the failure it reports: the wait runs, returns at once,
/// and its own message names what never arrived. A caller that has nothing to
/// say about a zero-length wait says so here instead, under a name that cannot
/// be confused with the shared helper's opposite semantics.
pub fn deadline_left(deadline: Instant) -> Duration {
    let left = remaining(deadline);
    assert!(
        !left.is_zero(),
        "the aggregate safety deadline expired before the wait it bounds began"
    );
    left
}
