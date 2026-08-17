//! The three independent bounds one streaming transfer runs under.

use super::policy_value::{clamped_duration, finite_duration, narrow, positive_limit};
use crate::RuntimeError;
use std::time::Duration;

/// The bytes and time one streaming upload or download may spend.
///
/// Three independent dimensions, each optional:
///
/// - `max_bytes` is the payload maximum, counted with checked addition on each
///   data frame before that frame is retained or forwarded;
/// - `idle` is the longest quiet interval allowed between data frames;
/// - `total` is the transfer's whole lifetime, starting when its consumer first
///   becomes eligible to poll.
///
/// Trailers cost no payload bytes and do not reset idle time.
///
/// There is deliberately no `Default`: a streaming caller states
/// [`bounded`](Self::bounded) or [`unbounded`](Self::unbounded). Camber's own
/// long-lived streaming registrations document which of the two they select.
///
/// ```rust
/// use camber::http::TransferBudget;
/// use std::time::Duration;
///
/// # fn main() -> Result<(), camber::RuntimeError> {
/// let upload = TransferBudget::bounded(
///     8 * 1024 * 1024,
///     Duration::from_secs(10),
///     Duration::from_secs(60),
/// )?;
/// assert_eq!(upload.max_bytes(), Some(8 * 1024 * 1024));
///
/// // A live feed keeps its byte and idle bounds and drops only the lifetime.
/// let feed = upload.without_total();
/// assert_eq!(feed.total(), None);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransferBudget {
    max_bytes: Option<usize>,
    idle: Option<Duration>,
    total: Option<Duration>,
}

impl TransferBudget {
    /// Bound every transfer dimension.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when `max_bytes` is zero, or
    /// when either duration is zero or longer than the thirty-year ceiling
    /// every Camber deadline shares. A route that accepts no payload is a
    /// body-mode decision, not a zero transfer maximum.
    pub fn bounded(
        max_bytes: usize,
        idle: Duration,
        total: Duration,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            max_bytes: Some(positive_limit(max_bytes, "transfer max_bytes")?),
            idle: Some(finite_duration(idle, "transfer idle")?),
            total: Some(finite_duration(total, "transfer total")?),
        })
    }

    /// Disable every transfer bound.
    ///
    /// The explicit opt-out for long-lived streams. Channel memory stays
    /// bounded by its configured depth; this removes byte and time bounds only.
    /// Under an outer finite policy it inherits that policy rather than
    /// widening it.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            max_bytes: None,
            idle: None,
            total: None,
        }
    }

    /// One budget built from values this crate states as constants.
    ///
    /// The validating constructors above are for a caller's values, and they
    /// refuse by returning. A framework default has no caller to return to, so
    /// it is written here the way [`ProxyPolicy`](super::ProxyPolicy) writes its
    /// own: as literals this crate owns, which the same rules already accept.
    /// Nothing outside `crate::http` can reach it, and no caller's value may.
    pub(super) const fn of(
        max_bytes: Option<usize>,
        idle: Option<Duration>,
        total: Option<Duration>,
    ) -> Self {
        Self {
            max_bytes,
            idle,
            total,
        }
    }

    /// Replace the quiet-interval deadline with `timeout` brought inside the
    /// range a policy owner can enforce.
    ///
    /// For the infallible builder setters, whose signature cannot report a
    /// refusal. [`Self::with_idle`] is what a caller stating a bound gets.
    pub(super) fn with_clamped_idle(self, timeout: Duration, min: Duration, name: &str) -> Self {
        Self {
            idle: Some(clamped_duration(timeout, min, name)),
            ..self
        }
    }

    /// Replace the lifetime deadline with `timeout` brought inside the range a
    /// policy owner can enforce.
    ///
    /// For the infallible builder setters, whose signature cannot report a
    /// refusal. [`Self::with_total`] is what a caller stating a bound gets.
    pub(super) fn with_clamped_total(self, timeout: Duration, min: Duration, name: &str) -> Self {
        Self {
            total: Some(clamped_duration(timeout, min, name)),
            ..self
        }
    }

    /// Replace the payload maximum.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when `max_bytes` is zero.
    pub fn with_max_bytes(self, max_bytes: usize) -> Result<Self, RuntimeError> {
        Ok(Self {
            max_bytes: Some(positive_limit(max_bytes, "transfer max_bytes")?),
            ..self
        })
    }

    /// Remove the payload maximum, keeping both time bounds.
    #[must_use]
    pub const fn without_max_bytes(self) -> Self {
        Self {
            max_bytes: None,
            ..self
        }
    }

    /// Replace the quiet-interval deadline.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero or
    /// longer than the thirty-year ceiling every Camber deadline shares.
    pub fn with_idle(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            idle: Some(finite_duration(timeout, "transfer idle")?),
            ..self
        })
    }

    /// Remove the quiet-interval deadline.
    #[must_use]
    pub const fn without_idle(self) -> Self {
        Self { idle: None, ..self }
    }

    /// Replace the transfer lifetime deadline.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero or
    /// longer than the thirty-year ceiling every Camber deadline shares.
    pub fn with_total(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            total: Some(finite_duration(timeout, "transfer total")?),
            ..self
        })
    }

    /// Remove the transfer lifetime deadline.
    #[must_use]
    pub const fn without_total(self) -> Self {
        Self {
            total: None,
            ..self
        }
    }

    /// The configured payload maximum.
    #[must_use]
    pub const fn max_bytes(&self) -> Option<usize> {
        self.max_bytes
    }

    /// The configured quiet interval between data frames.
    #[must_use]
    pub const fn idle(&self) -> Option<Duration> {
        self.idle
    }

    /// The configured transfer lifetime.
    #[must_use]
    pub const fn total(&self) -> Option<Duration> {
        self.total
    }

    /// This budget applied under one that contains it.
    pub(super) fn narrowed_by(self, outer: Self) -> Self {
        Self {
            max_bytes: narrow(self.max_bytes, outer.max_bytes),
            idle: narrow(self.idle, outer.idle),
            total: narrow(self.total, outer.total),
        }
    }
}
