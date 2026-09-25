//! One retry sequence: every attempt, every delay, and the final collection
//! under one absolute deadline and one runtime shutdown.
//!
//! The deadline is fixed once, at call entry. No attempt and no delay resets
//! it, so the configured retry timeout bounds the whole sequence rather than
//! any one part of it. A per-attempt boundary that commits first keeps its own
//! result and enters the replay decision; the sequence deadline and runtime
//! shutdown end the sequence and drop whatever was in flight.

use super::super::boundary::DeadlineBoundary;
use super::super::map_reqwest_error;
use super::delay::{retry_delay, stated_retry_after};
use super::exchange::Exchange;
use super::replay::{client_retryable_status, client_retryable_transport};
use crate::RuntimeError;
use crate::http::Response;
use std::future::Future;
use std::num::NonZeroU32;
use std::time::Duration;
use tokio::time::Instant;

/// The retry configuration one call runs under.
///
/// It exists only when retries are configured: a client with zero retries has
/// no sequence, and its attempt boundaries are the complete lifetime authority.
/// The count is the configured one, not a count already filtered by method:
/// eligibility is decided per outcome, where the evidence is.
#[derive(Clone, Copy)]
pub(super) struct RetryPlan {
    retries: NonZeroU32,
    backoff: Duration,
    timeout: Duration,
    retry_unsafe_methods: bool,
}

impl RetryPlan {
    /// The plan a client's settings describe, or `None` for zero retries.
    pub(super) fn configured(
        retries: u32,
        backoff: Duration,
        timeout: Duration,
        retry_unsafe_methods: bool,
    ) -> Option<Self> {
        NonZeroU32::new(retries).map(|retries| Self {
            retries,
            backoff,
            timeout,
            retry_unsafe_methods,
        })
    }
}

/// The uniquely owned coordinator of one call's attempts.
pub(super) struct RetrySequence<'a> {
    exchange: Exchange<'a>,
    plan: RetryPlan,
    /// Call entry plus the retry timeout, fixed once.
    deadline: Instant,
    /// The zero-based index of the attempt now running or just finished.
    attempt: u32,
}

impl<'a> RetrySequence<'a> {
    /// Begin a sequence at call entry, fixing its absolute deadline.
    ///
    /// The retry timeout is clamped to the policy ceiling where it is set, so
    /// the addition stays inside the clock's range.
    pub(super) fn begin(plan: RetryPlan, exchange: Exchange<'a>) -> Self {
        Self {
            exchange,
            plan,
            deadline: Instant::now() + plan.timeout,
            attempt: 0,
        }
    }

    /// Run attempts until one answer is final, a failure is not replayable, or
    /// the sequence ends.
    pub(super) async fn run(mut self) -> Result<Response, RuntimeError> {
        loop {
            let sent = self.within(self.exchange.send()).await?;
            let delay = match sent {
                Ok(resp) if self.replays_status(&resp) => self.dispose_transient(resp),
                Ok(resp) => return self.within(self.exchange.collect(resp)).await?,
                Err(error) if self.replays_transport(&error) => self.note_transport(&error),
                Err(error) => return Err(map_reqwest_error(error)),
            };
            self.back_off(delay).await?;
            self.attempt += 1;
        }
    }

    /// Whether any configured retry is left after the current attempt.
    ///
    /// `attempt` never exceeds the configured count, so the next attempt's
    /// index cannot overflow.
    fn has_retry_left(&self) -> bool {
        self.attempt < self.plan.retries.get()
    }

    fn replays_status(&self, resp: &reqwest::Response) -> bool {
        self.has_retry_left()
            && client_retryable_status(
                resp.status().as_u16(),
                self.exchange.method(),
                self.plan.retry_unsafe_methods,
            )
    }

    fn replays_transport(&self, error: &reqwest::Error) -> bool {
        self.has_retry_left()
            && client_retryable_transport(
                error,
                self.exchange.method(),
                self.plan.retry_unsafe_methods,
            )
    }

    /// Release a transient answer before the delay begins, and choose that
    /// delay.
    ///
    /// Dropping the unread response closes its connection now, so the peer is
    /// not held for the length of the backoff.
    fn dispose_transient(&self, resp: reqwest::Response) -> Duration {
        let status = resp.status().as_u16();
        let stated = stated_retry_after(&resp);
        drop(resp);
        tracing::debug!(
            method = %self.exchange.method(),
            url = self.exchange.url(),
            status,
            attempt = self.attempt + 1,
            "retrying transient HTTP status"
        );
        retry_delay(self.plan.backoff, self.attempt, stated)
    }

    /// Record a replayable transport failure, and choose the delay after it.
    fn note_transport(&self, error: &reqwest::Error) -> Duration {
        tracing::debug!(
            method = %self.exchange.method(),
            url = self.exchange.url(),
            error = %error,
            attempt = self.attempt + 1,
            "retrying transient HTTP error"
        );
        retry_delay(self.plan.backoff, self.attempt, None)
    }

    /// Wait out one delay, clipped to the sequence deadline.
    ///
    /// A delay that would reach the deadline is the deadline: the wait ends
    /// there with the sequence's own failure, and no attempt starts after it.
    async fn back_off(&self, delay: Duration) -> Result<(), RuntimeError> {
        match Instant::now()
            .checked_add(delay)
            .filter(|wake| *wake < self.deadline)
        {
            Some(wake) => self.within(tokio::time::sleep_until(wake)).await,
            None => self.within(std::future::pending::<()>()).await,
        }
    }

    /// Run one pending phase until it finishes or the sequence ends.
    ///
    /// The end is polled before the work, so a deadline already committed or a
    /// shutdown already accepted never starts another attempt. That is the only
    /// order imposed: the deadline and shutdown are unordered between
    /// themselves, and either one ready beside a finished phase is a member of
    /// the closed result set the contract admits.
    async fn within<T>(&self, work: impl Future<Output = T>) -> Result<T, RuntimeError> {
        tokio::select! {
            biased;
            cause = self.ended() => Err(cause),
            output = work => Ok(output),
        }
    }

    /// The first of the sequence deadline and runtime shutdown.
    ///
    /// Shutdown is sticky: one requested before this wait began is observed at
    /// once. With no runtime established, nothing can request it, so only the
    /// deadline remains.
    async fn ended(&self) -> RuntimeError {
        tokio::select! {
            () = tokio::time::sleep_until(self.deadline) => {
                RuntimeError::DeadlineExceeded(DeadlineBoundary::ClientRetry)
            }
            () = crate::task::on_shutdown() => RuntimeError::Cancelled,
        }
    }
}
