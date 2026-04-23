use crate::RuntimeError;
use std::fmt;
use std::ops::Deref;
use tokio::sync::watch as tokio_watch;

/// Create a watch channel with an initial value.
///
/// A watch channel holds a single value. Receivers always see the latest
/// value — intermediate writes are skipped. Use this for configuration,
/// state snapshots, or shutdown signals where only the current value matters.
///
/// The sender can update the value; all receivers see the change.
/// Receivers can be cloned cheaply.
pub fn watch<T>(initial: T) -> (WatchSender<T>, WatchReceiver<T>) {
    let (tx, rx) = tokio_watch::channel(initial);
    (WatchSender { inner: tx }, WatchReceiver { inner: rx })
}

/// Sending half of a watch channel.
///
/// Holds the current value. Dropping the sender closes the channel —
/// receivers' `changed()` futures will resolve with `ChannelClosed`.
pub struct WatchSender<T> {
    inner: tokio_watch::Sender<T>,
}

impl<T> WatchSender<T> {
    /// Replace the current value. All receivers see the update.
    ///
    /// Returns `ChannelClosed` if all receivers have been dropped.
    pub fn send(&self, value: T) -> Result<(), RuntimeError> {
        self.inner
            .send(value)
            .map_err(|_| RuntimeError::ChannelClosed)
    }

    /// Modify the current value in place. Notifies receivers even if the
    /// value is unchanged.
    ///
    /// Unlike `send()`, this always succeeds — the sender owns the stored
    /// value regardless of whether receivers exist. The mutation is applied
    /// and visible via `borrow()` even if all receivers have been dropped.
    pub fn send_modify<F: FnOnce(&mut T)>(&self, modify: F) {
        self.inner.send_modify(modify);
    }

    /// Read the current value.
    pub fn borrow(&self) -> WatchRef<'_, T> {
        WatchRef {
            inner: self.inner.borrow(),
        }
    }
}

impl<T> Clone for WatchSender<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for WatchSender<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatchSender").finish()
    }
}

/// Receiving half of a watch channel. Cloneable — each clone tracks its own
/// "seen" state independently.
pub struct WatchReceiver<T> {
    inner: tokio_watch::Receiver<T>,
}

impl<T> WatchReceiver<T> {
    /// Read the current value without marking it as seen. Use
    /// `borrow_and_update()` to also mark the value as seen for
    /// `has_changed()` tracking.
    pub fn borrow(&self) -> WatchRef<'_, T> {
        WatchRef {
            inner: self.inner.borrow(),
        }
    }

    /// Read the current value and mark it as seen. After this call,
    /// `has_changed()` returns `false` until the sender writes again.
    pub fn borrow_and_update(&mut self) -> WatchRef<'_, T> {
        WatchRef {
            inner: self.inner.borrow_and_update(),
        }
    }

    /// Wait until the value changes. Marks the new value as seen.
    /// Returns `ChannelClosed` when the sender is dropped.
    pub async fn changed(&mut self) -> Result<(), RuntimeError> {
        self.inner
            .changed()
            .await
            .map_err(|_| RuntimeError::ChannelClosed)
    }

    /// Check if the value has changed since the receiver was created, or
    /// since the last call to `changed()` or `borrow_and_update()`.
    pub fn has_changed(&self) -> bool {
        self.inner.has_changed().unwrap_or(false)
    }
}

impl<T> Clone for WatchReceiver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: fmt::Debug> fmt::Debug for WatchReceiver<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WatchReceiver").finish()
    }
}

/// RAII borrow of the current watch value. Dereferences to `T`.
pub struct WatchRef<'a, T> {
    inner: tokio_watch::Ref<'a, T>,
}

impl<T> Deref for WatchRef<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.inner
    }
}

impl<T: fmt::Debug> fmt::Debug for WatchRef<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.fmt(f)
    }
}
