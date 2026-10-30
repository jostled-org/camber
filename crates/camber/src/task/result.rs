//! One task result with blocking receipt and asynchronous notification.

use futures_util::task::AtomicWaker;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvError, SendError, SyncSender, TryRecvError, sync_channel};
use std::task::{Context, Poll};

/// The sole sender wakes the asynchronous receiver after sending or closing.
pub(crate) struct TaskResultSender<T> {
    sender: Option<SyncSender<T>>,
    wake: Arc<AtomicWaker>,
}

impl<T> TaskResultSender<T> {
    pub(crate) fn send(mut self, value: T) -> Result<(), SendError<T>> {
        match self.sender.take() {
            Some(sender) => sender.send(value),
            None => Err(SendError(value)),
        }
    }
}

impl<T> Drop for TaskResultSender<T> {
    fn drop(&mut self) {
        // Publish closure before waking, including a task discarded before it runs.
        drop(self.sender.take());
        self.wake.wake();
    }
}

/// The result stays in one channel whichever waiting mode the caller selects.
pub(crate) struct TaskResult<T> {
    receiver: Receiver<T>,
    wake: Arc<AtomicWaker>,
}

impl<T> Drop for TaskResult<T> {
    fn drop(&mut self) {
        // A continuing closure must not retain the executor task that stopped waiting.
        drop(self.wake.take());
    }
}

impl<T> TaskResult<T> {
    pub(crate) fn recv(self) -> Result<T, RecvError> {
        self.receiver.recv()
    }

    pub(crate) fn poll(&mut self, context: &mut Context<'_>) -> Poll<Result<T, RecvError>> {
        // Register first: a send between registration and receipt cannot lose a wake.
        self.wake.register(context.waker());
        match self.receiver.try_recv() {
            Ok(value) => Poll::Ready(Ok(value)),
            Err(TryRecvError::Empty) => Poll::Pending,
            Err(TryRecvError::Disconnected) => Poll::Ready(Err(RecvError)),
        }
    }
}

pub(crate) fn channel<T>() -> (TaskResultSender<T>, TaskResult<T>) {
    let (sender, receiver) = sync_channel(1);
    let wake = Arc::new(AtomicWaker::new());
    (
        TaskResultSender {
            sender: Some(sender),
            wake: Arc::clone(&wake),
        },
        TaskResult { receiver, wake },
    )
}
