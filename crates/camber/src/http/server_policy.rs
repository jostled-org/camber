//! The complete operating envelope one HTTP server serves under.

#[cfg(feature = "profiling")]
use super::policy_value::positive_limit;
use super::policy_value::{finite_duration, limit_within, narrow};
use super::request_budget::RequestBudget;
use super::server::MAX_CONNECTION_LIMIT;
use super::transfer_budget::TransferBudget;
use crate::RuntimeError;
use std::time::Duration;

/// The default pre-head wait for a complete request head.
pub(crate) const DEFAULT_HEADER_TIMEOUT: Duration = Duration::from_secs(60);
/// The default deadline every graceful shutdown participant shares.
pub(crate) const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// The default body-idle and request-total deadlines.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The default maximum a rendered profiling response is retained under: eight
/// MiB, the same ceiling Camber's ordinary buffered collections use.
#[cfg(feature = "profiling")]
pub const DEFAULT_PROFILING_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

/// Every bound one server applies to the work it admits.
///
/// A server policy is the sole authority under bare Tokio. Inside a Camber
/// runtime the runtime's own policy contains it: a server may narrow an outer
/// bound and can never widen one, per dimension.
///
/// | Dimension | Default |
/// | --- | --- |
/// | `header_timeout` | 60 seconds |
/// | request `body_idle` | 30 seconds |
/// | request `total` | 30 seconds |
/// | `upload_budget` | [`TransferBudget::unbounded`] |
/// | `download_budget` | [`TransferBudget::unbounded`] |
/// | `shutdown_timeout` | 30 seconds |
/// | `profiling_response_limit` | eight MiB |
/// | `connection_limit` | none — unbounded |
///
/// The rendered-profile maximum is finite by default and is removed only by
/// [`unbounded_profiling_response`](Self::unbounded_profiling_response), which
/// is deliberately named so its absence cannot be mistaken for a default.
///
/// The two streaming defaults are unbounded on purpose: a long-lived stream
/// that its registration did not bound stays open. Their channels remain
/// memory-bounded by the configured buffer depth. A production service that
/// serves untrusted peers should select finite streaming budgets and a finite
/// [`connection_limit`](Self::connection_limit); omitting the connection limit
/// is intended for development, tests, or a service behind an admission
/// boundary that already enforces one.
///
/// ```rust
/// use camber::http::{RequestBudget, ServerPolicy};
/// use std::time::Duration;
///
/// # fn main() -> Result<(), camber::RuntimeError> {
/// let policy = ServerPolicy::default()
///     .header_timeout(Duration::from_secs(10))?
///     .request_budget(RequestBudget::bounded(
///         Duration::from_secs(5),
///         Duration::from_secs(15),
///     )?)
///     .connection_limit(1024)?;
/// assert_ne!(policy, ServerPolicy::default());
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerPolicy {
    header_timeout: Duration,
    request: RequestBudget,
    upload: TransferBudget,
    download: TransferBudget,
    shutdown_timeout: Duration,
    /// The maximum a rendered profile is retained under, or `None` for the
    /// explicit opt-out.
    #[cfg(feature = "profiling")]
    profiling_response: Option<usize>,
    connection_limit: Option<usize>,
}

impl Default for ServerPolicy {
    fn default() -> Self {
        Self {
            header_timeout: DEFAULT_HEADER_TIMEOUT,
            request: RequestBudget::from_constants(
                DEFAULT_REQUEST_TIMEOUT,
                DEFAULT_REQUEST_TIMEOUT,
            ),
            upload: TransferBudget::unbounded(),
            download: TransferBudget::unbounded(),
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            #[cfg(feature = "profiling")]
            profiling_response: Some(DEFAULT_PROFILING_RESPONSE_LIMIT),
            connection_limit: None,
        }
    }
}

impl ServerPolicy {
    /// Set how long Hyper may wait for a complete HTTP/1 request head.
    ///
    /// This is a pre-head bound and nothing else. It is independent of request
    /// body idle time, request total time, connection admission, and shutdown:
    /// a peer that never finishes its head has no request, no request ID, and
    /// no mapped rejection — its transport is closed. Hyper exposes no
    /// equivalent HTTP/2 per-stream partial-HEADERS timer; HTTP/2 request
    /// budgets begin after Hyper delivers a complete head.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero or
    /// longer than the thirty-year ceiling every Camber deadline shares.
    pub fn header_timeout(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            header_timeout: finite_duration(timeout, "header_timeout")?,
            ..self
        })
    }

    /// Set the default deadlines for every request this server admits.
    #[must_use]
    pub fn request_budget(self, budget: RequestBudget) -> Self {
        Self {
            request: budget,
            ..self
        }
    }

    /// Set the default budget for streaming uploads.
    #[must_use]
    pub fn upload_budget(self, budget: TransferBudget) -> Self {
        Self {
            upload: budget,
            ..self
        }
    }

    /// Set the default budget for streaming downloads.
    #[must_use]
    pub fn download_budget(self, budget: TransferBudget) -> Self {
        Self {
            download: budget,
            ..self
        }
    }

    /// Set the one deadline every graceful shutdown participant shares.
    ///
    /// Cooperative work may finish until it expires. It bounds Camber's own
    /// waiting and escalation; it cannot stop an async task that never yields
    /// or an application callback running on a blocking thread.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero or
    /// longer than the thirty-year ceiling every Camber deadline shares. The
    /// first graceful transition adds this value to the clock, which panics on
    /// a deadline it cannot represent rather than saturating.
    pub fn shutdown_timeout(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            shutdown_timeout: finite_duration(timeout, "shutdown_timeout")?,
            ..self
        })
    }

    /// Limit how many connections this listener admits at once.
    ///
    /// The permit is taken before TLS or HTTP work and released once, when the
    /// last owner of the transport — HTTP connection task, SSE response, gRPC
    /// stream, or direct/proxied WebSocket bridge — lets it go.
    ///
    /// Omitting the limit is unbounded. Production services should set one.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when `limit` is zero, which
    /// would admit nothing at all, or when it exceeds the largest limit the
    /// admission semaphore can hold. The refusal names that ceiling, so it is
    /// read from the semaphore rather than restated here. A limit that large
    /// bounds nothing a listener can reach; unbounded admission is spelled by
    /// omitting the limit.
    pub fn connection_limit(self, limit: usize) -> Result<Self, RuntimeError> {
        Ok(Self {
            connection_limit: Some(limit_within(
                limit,
                MAX_CONNECTION_LIMIT,
                "connection_limit",
            )?),
            ..self
        })
    }

    /// Cap the rendered profiling response this server retains.
    ///
    /// Sampling and rendering run on a blocking thread, and the renderer's
    /// output is accounted before it is retained: the write that would carry the
    /// answer past this maximum is dropped, and the request is refused rather
    /// than answered with a partial profile.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when `max_bytes` is zero, which
    /// would retain nothing at all. Unbounded output is spelled by
    /// [`unbounded_profiling_response`](Self::unbounded_profiling_response).
    #[cfg(feature = "profiling")]
    pub fn profiling_response_limit(self, max_bytes: usize) -> Result<Self, RuntimeError> {
        Ok(Self {
            profiling_response: Some(positive_limit(max_bytes, "profiling_response_limit")?),
            ..self
        })
    }

    /// Retain a rendered profiling response with no size ceiling.
    ///
    /// **Warning:** a flamegraph grows with the number of distinct stacks the
    /// sampler found, so a process with deep or wide concurrency renders an
    /// answer this server then holds entirely in memory. Use it when a profile
    /// this service must produce is being truncated by the default, and prefer
    /// naming a larger [`profiling_response_limit`](Self::profiling_response_limit)
    /// where a number is known. This is the only spelling that removes the
    /// maximum.
    ///
    /// A server inside a Camber runtime is still contained by that runtime's
    /// policy: opting out here inherits an outer finite maximum rather than
    /// erasing it.
    #[must_use]
    #[cfg(feature = "profiling")]
    pub const fn unbounded_profiling_response(self) -> Self {
        Self {
            profiling_response: None,
            ..self
        }
    }

    // The readers below carry a `_value` suffix because each dimension's
    // natural name is already the setter this immutable value is built with.
    // They are crate-internal: a caller configures a policy and compares
    // policies, and the fields stay private so a validated value cannot be
    // edited past its constructor.

    /// The configured pre-head wait for a complete request head.
    pub(crate) const fn header_timeout_value(&self) -> Duration {
        self.header_timeout
    }

    /// The configured default request deadlines.
    pub(crate) const fn request_budget_value(&self) -> RequestBudget {
        self.request
    }

    /// The configured default streaming-upload budget.
    pub(crate) const fn upload_budget_value(&self) -> TransferBudget {
        self.upload
    }

    /// The configured default streaming-download budget.
    pub(crate) const fn download_budget_value(&self) -> TransferBudget {
        self.download
    }

    /// The configured aggregate shutdown deadline.
    pub(crate) const fn shutdown_timeout_value(&self) -> Duration {
        self.shutdown_timeout
    }

    /// The configured connection limit, or `None` for unbounded admission.
    pub(crate) const fn connection_limit_value(&self) -> Option<usize> {
        self.connection_limit
    }

    /// This policy applied under the one that contains it.
    ///
    /// Every dimension narrows independently through the shared rule, so a
    /// server started inside a Camber runtime can tighten that runtime's
    /// envelope and can never escape it.
    pub(super) fn narrowed_by(self, outer: Self) -> Self {
        Self {
            header_timeout: self.header_timeout.min(outer.header_timeout),
            request: self.request.narrowed_by(outer.request),
            upload: self.upload.narrowed_by(outer.upload),
            download: self.download.narrowed_by(outer.download),
            shutdown_timeout: self.shutdown_timeout.min(outer.shutdown_timeout),
            #[cfg(feature = "profiling")]
            profiling_response: narrow(self.profiling_response, outer.profiling_response),
            connection_limit: narrow(self.connection_limit, outer.connection_limit),
        }
    }
}

/// The rendered-profile maximum one served profiling route freezes from `policy`.
///
/// The single reader of that dimension: the built-in profiling route freezes this
/// value at match time, the render it performs measures against it, and the
/// focused contract reads it here rather than restating the documented default as
/// a number of its own. `None` is the named opt-out, never a zero.
///
/// Pure: one field out, no allocation and no state.
#[cfg(feature = "profiling")]
#[doc(hidden)]
pub const fn frozen_profiling_response_limit(policy: &ServerPolicy) -> Option<usize> {
    policy.profiling_response
}
