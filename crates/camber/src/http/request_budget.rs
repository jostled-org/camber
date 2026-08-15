//! The two deadlines an admitted request runs under.

use super::policy_value::{finite_duration, narrow};
use crate::RuntimeError;
use std::time::Duration;

/// The time an admitted request may spend before its response head commits.
///
/// Two independent deadlines, both optional, neither derived from the other:
///
/// - `body_idle` is the longest quiet interval allowed between request body
///   data frames while a body consumer is active;
/// - `total` is the longest lifetime from admitted request head through
///   committed response head or successful WebSocket handoff.
///
/// Whichever expires first at the boundary it owns ends the request. Response
/// body time is not request time: it belongs to the selected download
/// [`TransferBudget`](super::TransferBudget).
///
/// There is deliberately no `Default`. A caller states
/// [`bounded`](Self::bounded) or [`unbounded`](Self::unbounded) rather than
/// inheriting a value it never read. The server's own default request budget is
/// part of [`ServerPolicy`](super::ServerPolicy).
///
/// The value is small, immutable, and `Copy`: it is read on every admitted
/// request and shares no allocation.
///
/// ```rust
/// use camber::http::RequestBudget;
/// use std::time::Duration;
///
/// # fn main() -> Result<(), camber::RuntimeError> {
/// let budget = RequestBudget::bounded(Duration::from_secs(5), Duration::from_secs(20))?;
/// assert_eq!(budget.body_idle(), Some(Duration::from_secs(5)));
///
/// // Long-poll routes can drop the total while keeping idle detection.
/// let long_poll = budget.without_total();
/// assert_eq!(long_poll.total(), None);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RequestBudget {
    body_idle: Option<Duration>,
    total: Option<Duration>,
}

impl RequestBudget {
    /// Bound both request dimensions.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when either duration is zero.
    /// Zero is not a spelling of "unbounded" — [`unbounded`](Self::unbounded)
    /// is.
    pub fn bounded(body_idle: Duration, total: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            body_idle: Some(finite_duration(body_idle, "request body_idle")?),
            total: Some(finite_duration(total, "request total")?),
        })
    }

    /// Disable both request deadlines.
    ///
    /// The only way to remove both at once, and the only value that reads as an
    /// explicit choice rather than an omission. Under an outer finite policy it
    /// inherits that policy: an inner unbounded value never widens an outer
    /// bound.
    #[must_use]
    pub const fn unbounded() -> Self {
        Self {
            body_idle: None,
            total: None,
        }
    }

    /// Replace the body-idle deadline with a finite duration.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero.
    pub fn with_body_idle(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            body_idle: Some(finite_duration(timeout, "request body_idle")?),
            ..self
        })
    }

    /// Remove the body-idle deadline, keeping the total deadline.
    #[must_use]
    pub const fn without_body_idle(self) -> Self {
        Self {
            body_idle: None,
            ..self
        }
    }

    /// Replace the request-total deadline with a finite duration.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero.
    pub fn with_total(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            total: Some(finite_duration(timeout, "request total")?),
            ..self
        })
    }

    /// Remove the request-total deadline, keeping the body-idle deadline.
    #[must_use]
    pub const fn without_total(self) -> Self {
        Self {
            total: None,
            ..self
        }
    }

    /// The configured quiet interval between request body data frames.
    #[must_use]
    pub const fn body_idle(&self) -> Option<Duration> {
        self.body_idle
    }

    /// The configured lifetime from admitted head to committed response head.
    #[must_use]
    pub const fn total(&self) -> Option<Duration> {
        self.total
    }

    /// A budget over two values a `const` already fixed.
    ///
    /// Camber's own documented defaults are non-zero literals, so the validated
    /// constructor's `Result` would only add a branch no default can reach.
    /// Nothing outside this crate's defaults may use it.
    pub(super) const fn from_constants(body_idle: Duration, total: Duration) -> Self {
        Self {
            body_idle: Some(body_idle),
            total: Some(total),
        }
    }

    /// This budget applied under one that contains it.
    pub(super) fn narrowed_by(self, outer: Self) -> Self {
        Self {
            body_idle: narrow(self.body_idle, outer.body_idle),
            total: narrow(self.total, outer.total),
        }
    }
}
