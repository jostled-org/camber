//! The one deadline a receive may wait under.

use std::pin::pin;
use std::task::{Context, Poll, Waker};
use std::time::Duration;
use tokio::time::Instant;

use crate::RuntimeError;

/// How long one receive may wait.
///
/// Fixed by the caller before the first wait and wrapped around the whole
/// receive, so a loop that skips messages spends one deadline rather than
/// starting a new one per message.
#[derive(Clone, Copy)]
pub(super) enum ReceiveDeadline {
    /// Wait for as long as the connection is live. Needs no clock.
    Untimed,
    /// Give up at this instant on the entered runtime's clock.
    At(Instant),
    /// A bounded wait was asked for with no runtime to take a clock from.
    ///
    /// Not a refusal yet: a receive that can answer without waiting still
    /// answers. Only one that would have to wait reports `NoRuntime`.
    Unclocked,
}

impl ReceiveDeadline {
    /// The deadline `timeout` from now on the entered runtime's clock.
    ///
    /// A timeout too large to represent waits untimed, which is the only
    /// instant such a timeout could name.
    pub(super) fn after(timeout: Duration) -> Self {
        match tokio::runtime::Handle::try_current() {
            Ok(_) => Instant::now()
                .checked_add(timeout)
                .map_or(Self::Untimed, Self::At),
            Err(_) => Self::Unclocked,
        }
    }

    /// Run `receive` under this deadline.
    ///
    /// The receive is always polled before the clock is read. Tokio's timeout
    /// polls its inner future first, so an answer that is ready when the
    /// deadline has passed is still the answer, and an unclocked receive gets
    /// exactly one poll: what is queued or already closed answers, and only a
    /// receive that would have to wait is refused.
    pub(super) async fn bound<T>(
        self,
        receive: impl Future<Output = T>,
    ) -> Result<T, RuntimeError> {
        match self {
            Self::Untimed => Ok(receive.await),
            Self::At(at) => tokio::time::timeout_at(at, receive)
                .await
                .map_err(|_| RuntimeError::Timeout),
            Self::Unclocked => ready_now(receive).ok_or(RuntimeError::NoRuntime),
        }
    }
}

/// The answer `receive` gives in one poll, if it gives one without waiting.
///
/// No waker is kept: a receive that is pending here is dropped, and dropping a
/// pending receive takes nothing from the queue.
fn ready_now<T>(receive: impl Future<Output = T>) -> Option<T> {
    let mut context = Context::from_waker(Waker::noop());
    match pin!(receive).poll(&mut context) {
        Poll::Ready(answer) => Some(answer),
        Poll::Pending => None,
    }
}
