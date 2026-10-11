//! The lifetime of one operation: its absolute deadline, the runtime's
//! shutdown, and the attempt timer inside them.
//!
//! The deadline is fixed once, when the operation is first polled. No attempt
//! and no delay resets it. Every pending phase runs under [`Lifetime::within`],
//! so a call with zero retries has the same deadline and the same shutdown
//! owner as a call with retries to spare.

use super::error::{ClientError, ClientTimeout};
use crate::RuntimeError;
use std::future::Future;
use std::time::Duration;
use tokio::time::Instant;

/// The failure of an operation that outlived its own deadline.
const OPERATION_EXPIRED: RuntimeError =
    RuntimeError::Client(ClientError::Timeout(ClientTimeout::Operation));

/// The absolute deadline of one operation.
pub(super) struct Lifetime {
    deadline: Instant,
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
            deadline if deadline > entry => Ok(Self { deadline }),
            _ => Err(OPERATION_EXPIRED),
        }
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
    pub(super) async fn within<T>(&self, work: impl Future<Output = T>) -> Result<T, RuntimeError> {
        tokio::select! {
            biased;
            cause = self.ended() => Err(cause),
            output = work => Ok(output),
        }
    }

    /// Run one phase of an attempt until it finishes, its attempt timer
    /// expires, or the operation ends.
    ///
    /// `None` is the attempt timer. The caller names that failure, because
    /// only the caller knows whether the phase may be sent again.
    pub(super) async fn within_attempt<T>(
        &self,
        bound: Option<Instant>,
        work: impl Future<Output = T>,
    ) -> Result<Option<T>, RuntimeError> {
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
    pub(super) async fn wait(&self, delay: Duration) -> Result<(), RuntimeError> {
        match Instant::now()
            .checked_add(delay)
            .filter(|wake| *wake < self.deadline)
        {
            Some(wake) => self.within(tokio::time::sleep_until(wake)).await,
            None => self.within(std::future::pending::<()>()).await,
        }
    }

    /// The first of the operation deadline and runtime shutdown.
    ///
    /// Shutdown is sticky: one requested before this wait began is observed at
    /// once. With no runtime established, nothing can request it, so only the
    /// deadline remains.
    async fn ended(&self) -> RuntimeError {
        tokio::select! {
            () = tokio::time::sleep_until(self.deadline) => OPERATION_EXPIRED,
            () = crate::task::on_shutdown() => RuntimeError::Cancelled,
        }
    }
}
