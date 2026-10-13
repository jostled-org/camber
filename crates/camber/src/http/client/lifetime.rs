//! The lifetime of one operation: its absolute deadline, the runtime's
//! stop request, and the attempt timer inside them.
//!
//! The deadline is fixed once, when the operation is first polled. No attempt
//! and no delay resets it. Every pending phase runs under [`Lifetime::within`],
//! so a call with zero retries has the same deadline and the same shutdown
//! owner as a call with retries to spare.

use super::error::{ClientError, ClientTimeout};
use crate::RuntimeError;
use crate::runtime_state::LifecycleSignals;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// The category of an operation that outlived its own deadline.
pub(super) const OPERATION_EXPIRED: ClientError = ClientError::Timeout(ClientTimeout::Operation);

/// How a lifetime ended a wait. These two ends are the only ones it has.
pub(super) enum LifetimeEnd {
    /// The operation deadline expired.
    Expired,
    /// The runtime asked the operation to stop.
    Stopped,
}

impl From<LifetimeEnd> for RuntimeError {
    fn from(end: LifetimeEnd) -> Self {
        match end {
            LifetimeEnd::Expired => Self::Client(OPERATION_EXPIRED),
            LifetimeEnd::Stopped => Self::Cancelled,
        }
    }
}

/// The category of an attempt whose timer expired, before its head or during
/// its body.
pub(super) const ATTEMPT_EXPIRED: ClientError = ClientError::Timeout(ClientTimeout::Attempt);

/// Where one operation reads its runtime's request to stop.
pub(super) enum Stop {
    /// The runtime of the task that polls the operation. An inline call
    /// reads it at each wait.
    Ambient,
    /// The runtime that admitted the operation's task, fixed at the first
    /// poll. Its shutdown and the close of its root scope both stop the
    /// operation.
    Captured(LifecycleSignals),
    /// No Camber runtime owns the operation, so nothing can ask it to stop.
    Never,
}

impl Stop {
    /// Wait until the runtime asks the operation to stop.
    ///
    /// The request is sticky: one made before this wait began is read at
    /// once.
    async fn requested(&self) {
        match self {
            Self::Ambient => crate::task::on_shutdown().await,
            Self::Captured(signals) => signals.wait().await,
            Self::Never => std::future::pending().await,
        }
    }
}

/// The absolute deadline of one operation, and its source of a stop request.
pub(super) struct Lifetime {
    deadline: Instant,
    stop: Stop,
}

impl Lifetime {
    /// Fix the deadline at `entry`: the earlier of `entry` plus `total` and
    /// the caller's absolute `ceiling`.
    ///
    /// `total` is clamped to the policy ceiling where it is set, so the
    /// addition stays inside the clock's range. The ceiling is an instant of
    /// the standard clock. It is converted once, here, to the distance left on
    /// that clock, and a ceiling too far away to add cannot be the earlier
    /// bound. A deadline that is not after `entry` has already expired, and
    /// the operation ends before it sends.
    pub(super) fn begin(
        total: Duration,
        ceiling: Option<std::time::Instant>,
    ) -> Result<Self, RuntimeError> {
        let entry = Instant::now();
        let total_end = entry + total;
        let supplied_end = ceiling
            .map(|at| at.saturating_duration_since(std::time::Instant::now()))
            .and_then(|left| entry.checked_add(left));
        match supplied_end.map_or(total_end, |supplied| supplied.min(total_end)) {
            deadline if deadline > entry => Ok(Self {
                deadline,
                stop: Stop::Ambient,
            }),
            _ => Err(RuntimeError::Client(OPERATION_EXPIRED)),
        }
    }

    /// Read the stop request from `stop` from now on. The deadline is kept.
    pub(super) fn stopped_by(self, stop: Stop) -> Self {
        Self { stop, ..self }
    }

    /// When an attempt that starts now and may last `attempt` expires.
    ///
    /// `None` arms no attempt timer. An attempt bound that would not expire
    /// before the operation deadline is one of those: the deadline ends that
    /// attempt, and a second timer on the same instant would compete with it
    /// to name the failure.
    pub(super) fn attempt_bound(&self, attempt: Option<Duration>) -> Option<Instant> {
        attempt
            .and_then(|limit| Instant::now().checked_add(limit))
            .filter(|expiry| *expiry < self.deadline)
    }

    /// Run one pending phase until it finishes or the operation ends.
    ///
    /// The end is polled before the work, so a deadline already committed or a
    /// shutdown already accepted never starts another attempt. That is the only
    /// order imposed: the deadline and shutdown are unordered between
    /// themselves, and either one ready beside a finished phase is a member of
    /// the closed result set the contract admits.
    pub(super) async fn within<T>(&self, work: impl Future<Output = T>) -> Result<T, LifetimeEnd> {
        tokio::select! {
            biased;
            cause = self.ended() => Err(cause),
            output = work => Ok(output),
        }
    }

    /// Run one phase of an attempt until it finishes, its attempt timer
    /// expires, or the operation ends.
    ///
    /// `None` is the attempt timer. The caller names that failure as
    /// [`ATTEMPT_EXPIRED`], because only the caller knows whether the phase
    /// may be sent again.
    pub(super) async fn within_attempt<T>(
        &self,
        bound: Option<Instant>,
        work: impl Future<Output = T>,
    ) -> Result<Option<T>, LifetimeEnd> {
        match bound {
            Some(expiry) => self
                .within(tokio::time::timeout_at(expiry, work))
                .await
                .map(Result::ok),
            None => self.within(work).await.map(Some),
        }
    }

    /// Wait out one delay, clipped to the operation deadline.
    ///
    /// A delay that would reach the deadline is the deadline: the wait ends
    /// there with the operation's own failure, and no attempt starts after it.
    pub(super) async fn wait(&self, delay: Duration) -> Result<(), LifetimeEnd> {
        match Instant::now()
            .checked_add(delay)
            .filter(|wake| *wake < self.deadline)
        {
            Some(wake) => self.within(tokio::time::sleep_until(wake)).await,
            None => self.within(std::future::pending::<()>()).await,
        }
    }

    /// The first of the operation deadline and the runtime's stop request.
    ///
    /// The request is sticky: one made before this wait began is observed at
    /// once. With no runtime established, nothing can make it, so only the
    /// deadline remains.
    async fn ended(&self) -> LifetimeEnd {
        tokio::select! {
            () = tokio::time::sleep_until(self.deadline) => LifetimeEnd::Expired,
            () = self.stop.requested() => LifetimeEnd::Stopped,
        }
    }
}
