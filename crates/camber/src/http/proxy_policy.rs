//! The bounds one proxy route applies to its upstream.

use super::policy_value::{finite_duration, positive_limit};
use super::transfer_budget::TransferBudget;
use crate::RuntimeError;
use std::time::Duration;

/// The default connect, request-total, and upstream-idle deadlines.
const DEFAULT_PROXY_TIMEOUT: Duration = Duration::from_secs(30);
/// The default buffered upstream response maximum, matching Camber's ordinary
/// request ceiling.
const DEFAULT_BUFFERED_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

/// Every bound one proxy route applies to the upstream it forwards to.
///
/// The three deadlines are independent phases, not one shared timeout:
///
/// - `connect_timeout` ends when TCP and TLS establishment complete;
/// - `request_timeout` covers request construction, upload, and acquiring a
///   usable upstream response head;
/// - `upstream_idle_timeout` covers each gap between upstream response body
///   frames.
///
/// Each retains its own failure provenance, so an operator can tell a dead
/// upstream from a slow one.
///
/// | Dimension | Default |
/// | --- | --- |
/// | `connect_timeout` | 30 seconds |
/// | `request_timeout` | 30 seconds |
/// | `upstream_idle_timeout` | 30 seconds |
/// | `buffered_response_limit` | eight MiB |
/// | `upload_budget` | [`TransferBudget::unbounded`] |
/// | `download_budget` | [`TransferBudget::unbounded`] |
///
/// Route-aware body admission still owns the request payload maximum: an
/// upload budget here can narrow it and never widen it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProxyPolicy {
    connect: Duration,
    request: Duration,
    upstream_idle: Duration,
    buffered_response: Option<usize>,
    upload: TransferBudget,
    download: TransferBudget,
}

impl Default for ProxyPolicy {
    fn default() -> Self {
        Self {
            connect: DEFAULT_PROXY_TIMEOUT,
            request: DEFAULT_PROXY_TIMEOUT,
            upstream_idle: DEFAULT_PROXY_TIMEOUT,
            buffered_response: Some(DEFAULT_BUFFERED_RESPONSE_LIMIT),
            upload: TransferBudget::unbounded(),
            download: TransferBudget::unbounded(),
        }
    }
}

impl ProxyPolicy {
    /// Set how long establishing the upstream transport may take.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero or
    /// longer than the thirty-year ceiling every Camber deadline shares.
    pub fn connect_timeout(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            connect: finite_duration(timeout, "proxy connect_timeout")?,
            ..self
        })
    }

    /// Set how long the upstream request may run before a usable head arrives.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero or
    /// longer than the thirty-year ceiling every Camber deadline shares.
    pub fn request_timeout(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            request: finite_duration(timeout, "proxy request_timeout")?,
            ..self
        })
    }

    /// Set the longest quiet interval allowed between upstream body frames.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero or
    /// longer than the thirty-year ceiling every Camber deadline shares.
    pub fn upstream_idle_timeout(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            upstream_idle: finite_duration(timeout, "proxy upstream_idle_timeout")?,
            ..self
        })
    }

    /// Set the maximum a buffered upstream response may retain.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when `max_bytes` is zero.
    pub fn buffered_response_limit(self, max_bytes: usize) -> Result<Self, RuntimeError> {
        Ok(Self {
            buffered_response: Some(positive_limit(max_bytes, "proxy buffered_response_limit")?),
            ..self
        })
    }

    /// Collect buffered upstream responses with no size ceiling.
    ///
    /// **Warning:** an upstream that answers with an unbounded or hostile body
    /// is then read entirely into this process's memory. Use it only for an
    /// upstream you control and trust, and prefer a streaming proxy route for
    /// large payloads. This is the only proxy configuration that removes the
    /// buffered ceiling, and it is deliberately named so its absence cannot be
    /// mistaken for a default.
    #[must_use]
    pub const fn unbounded_buffered_response(self) -> Self {
        Self {
            buffered_response: None,
            ..self
        }
    }

    /// Set the streaming-upload budget for this route.
    #[must_use]
    pub fn upload_budget(self, budget: TransferBudget) -> Self {
        Self {
            upload: budget,
            ..self
        }
    }

    /// Set the streaming-download budget for this route.
    #[must_use]
    pub fn download_budget(self, budget: TransferBudget) -> Self {
        Self {
            download: budget,
            ..self
        }
    }
}

/// The buffered maximum one proxy route freezes from `policy`.
///
/// The single reader of that dimension: a registered buffered route freezes
/// this value, the collection that route performs measures against it, and the
/// focused contract reads it here rather than restating the documented default
/// as a number of its own. `None` is the named opt-out, never a zero.
///
/// Pure: one field out, no allocation and no state.
#[doc(hidden)]
pub const fn frozen_buffered_response_limit(policy: &ProxyPolicy) -> Option<usize> {
    policy.buffered_response
}
