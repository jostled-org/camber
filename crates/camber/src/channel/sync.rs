use crate::RuntimeError;
use crate::runtime;
use crossbeam_channel as cb;

const DEFAULT_CAPACITY: usize = 128;

/// Create an unbounded-style channel with default capacity (128).
pub fn new<T>() -> (Sender<T>, Receiver<T>) {
    bounded(DEFAULT_CAPACITY)
}

/// Create a bounded channel with explicit capacity.
pub fn bounded<T>(cap: usize) -> (Sender<T>, Receiver<T>) {
    let (tx, rx) = cb::bounded(cap);
    (Sender { inner: tx }, Receiver { inner: rx })
}

pub struct Sender<T> {
    inner: cb::Sender<T>,
}

impl<T> Sender<T> {
    pub fn send(&self, value: T) -> Result<(), RuntimeError> {
        runtime::check_cancel()?;
        self.inner
            .send(value)
            .map_err(|_| RuntimeError::ChannelClosed)
    }

    #[doc(hidden)]
    pub fn as_crossbeam(&self) -> &cb::Sender<T> {
        &self.inner
    }
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

pub struct Receiver<T> {
    inner: cb::Receiver<T>,
}

impl<T> Receiver<T> {
    pub fn recv(&self) -> Result<T, RuntimeError> {
        runtime::check_cancel()?;
        match runtime::cancel_channel() {
            Some(cancel) => {
                cb::select! {
                    recv(self.inner) -> res => match res {
                        Ok(val) => Ok(val),
                        Err(_) => Err(RuntimeError::ChannelClosed),
                    },
                    recv(cancel) -> res => match res {
                        Ok(()) => Err(RuntimeError::Cancelled),
                        // Cancel channel sender dropped (JoinHandle dropped without cancel).
                        // Fall back to blocking recv — no cancellation active.
                        Err(_) => self.inner.recv().map_err(|_| RuntimeError::ChannelClosed),
                    },
                }
            }
            None => {
                let val = self.inner.recv().map_err(|_| RuntimeError::ChannelClosed)?;
                runtime::check_cancel()?;
                Ok(val)
            }
        }
    }

    /// Cancel-aware iterator that yields values until the sender is dropped
    /// or the task is cancelled.
    pub fn iter(&self) -> CancelIter<'_, T> {
        CancelIter { receiver: self }
    }

    #[doc(hidden)]
    pub fn as_crossbeam(&self) -> &cb::Receiver<T> {
        &self.inner
    }
}

impl<T> Clone for Receiver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

/// Iterator that checks cancellation between items.
pub struct CancelIter<'a, T> {
    receiver: &'a Receiver<T>,
}

impl<T> Iterator for CancelIter<'_, T> {
    type Item = T;

    fn next(&mut self) -> Option<T> {
        self.receiver.recv().ok()
    }
}
