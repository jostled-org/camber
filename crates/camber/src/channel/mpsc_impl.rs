use crate::RuntimeError;
use std::fmt;
use tokio::sync::mpsc as tokio_mpsc;

/// Create a bounded MPSC channel with the given capacity.
///
/// `MpscSender` uses blocking send (safe from sync camber tasks).
/// `MpscReceiver` uses async recv (designed for `spawn_async` / `every_async`).
pub fn mpsc<T>(cap: usize) -> Result<(MpscSender<T>, MpscReceiver<T>), RuntimeError> {
    match cap {
        0 => Err(RuntimeError::InvalidArgument(
            "mpsc channel capacity must be greater than zero".into(),
        )),
        _ => {
            let (tx, rx) = tokio_mpsc::channel(cap);
            Ok((MpscSender { inner: tx }, MpscReceiver { inner: rx }))
        }
    }
}

/// Sending half of an async MPSC channel.
pub struct MpscSender<T> {
    inner: tokio_mpsc::Sender<T>,
}

impl<T> MpscSender<T> {
    /// Blocking send. Safe from sync contexts (camber::spawn tasks).
    pub fn send(&self, value: T) -> Result<(), RuntimeError> {
        self.inner
            .blocking_send(value)
            .map_err(|_| RuntimeError::ChannelClosed)
    }

    /// Non-blocking send. Returns `Err(ChannelFull)` if the buffer is full.
    pub fn try_send(&self, value: T) -> Result<(), RuntimeError> {
        self.inner.try_send(value).map_err(|e| match e {
            tokio_mpsc::error::TrySendError::Full(_) => RuntimeError::ChannelFull,
            tokio_mpsc::error::TrySendError::Closed(_) => RuntimeError::ChannelClosed,
        })
    }
}

impl<T> Clone for MpscSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> fmt::Debug for MpscSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MpscSender").finish()
    }
}

/// Receiving half of an async MPSC channel.
pub struct MpscReceiver<T> {
    inner: tokio_mpsc::Receiver<T>,
}

impl<T> MpscReceiver<T> {
    /// Async receive. Returns `None` when all senders are dropped.
    pub async fn recv(&mut self) -> Option<T> {
        self.inner.recv().await
    }

    /// Close the receiver. Remaining buffered messages can still be received.
    pub fn close(&mut self) {
        self.inner.close();
    }
}

impl<T> fmt::Debug for MpscReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MpscReceiver").finish()
    }
}
