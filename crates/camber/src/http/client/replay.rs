//! Whether one failed attempt may be sent again.
//!
//! Two independent questions are answered here and nowhere else. The request's
//! permission answers whether its method may be repeated at all. The outcome
//! answers whether another send could end differently. The operation that acts
//! on them holds no second copy of either.
//!
//! No outcome is evidence that the peer did no work. A gateway status, a
//! closed connection, and an expired timer all leave that unknown, so the
//! permission is the only authority for a repeat.

use super::error::{ClientError, ClientTimeout};
use super::projection::client_error_projection;
use reqwest::Method;

/// Which requests may be sent again after a transient outcome.
///
/// The permission belongs to one request: set it with
/// [`ClientRequest::retry_policy`](super::ClientRequest::retry_policy). It
/// decides whether a method may be repeated. The retry count decides how many
/// times, and the default count is zero, so no permission sends twice until
/// retries are configured.
///
/// Camber cannot learn from an outcome whether the peer acted on a request. A
/// 503, a connection closed before the response head, and a timeout all leave
/// that open. The permission is therefore a statement about the request, not
/// about the failure.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum RetryPolicy {
    /// No outcome earns a retry.
    ///
    /// A redirect the request's
    /// [`RedirectPolicy`](super::RedirectPolicy) follows is still sent.
    Never,
    /// Repeat only GET, HEAD, OPTIONS, PUT, and DELETE: the methods whose
    /// repeat leaves the same state as one send. This is the default.
    ///
    /// A method token is case-sensitive, so `get` is an extension method and
    /// is not repeated.
    #[default]
    Idempotent,
    /// Repeat any method.
    ///
    /// This is the caller's assertion that repeating this operation is
    /// acceptable, for example because the provider honors an idempotency key
    /// the request carries. Camber adds no such key and does not promise that
    /// the operation runs once.
    Replayable,
}

/// Whether repeating `method` leaves the state one send of it leaves.
///
/// RFC 9110 idempotence, read from the exact token: an extension method states
/// nothing about itself, and a token in another case is an extension method.
fn is_idempotent_method(method: &Method) -> bool {
    matches!(
        method.as_str(),
        "GET" | "HEAD" | "OPTIONS" | "PUT" | "DELETE"
    )
}

/// Whether `policy` lets a request with `method` be sent again.
///
/// Method permission on its own: it says nothing about whether this attempt's
/// outcome is one a repeat could change.
fn permits_replay(policy: RetryPolicy, method: &Method) -> bool {
    match policy {
        RetryPolicy::Never => false,
        RetryPolicy::Idempotent => is_idempotent_method(method),
        RetryPolicy::Replayable => true,
    }
}

/// Whether `policy` lets a redirect repeat a request with `method`, body
/// included.
///
/// A redirect the peer asked for is not a retry, so `Never` does not withhold
/// it. It still repeats the request, so a method outside the idempotent ones
/// needs the caller's `Replayable`.
pub(super) fn client_resendable(method: &Method, policy: RetryPolicy) -> bool {
    match policy {
        RetryPolicy::Never | RetryPolicy::Idempotent => is_idempotent_method(method),
        RetryPolicy::Replayable => true,
    }
}

/// The statuses a peer uses for a condition that can pass.
fn is_transient_status(status: u16) -> bool {
    matches!(status, 429 | 502..=504)
}

/// Whether another send could end differently from the one that failed with
/// `category` before its response head.
///
/// A refused or timed-out dial, a connection closed without an answer, and an
/// expired attempt timer can each pass. A name that does not resolve and a
/// peer whose TLS is refused fail the same way again, and a fault in the call
/// itself repeats the fault. Every category that is not a pre-head transport
/// outcome is terminal, so a new category is terminal until it is named here.
fn is_transient_failure(category: ClientError) -> bool {
    match category {
        ClientError::Connect
        | ClientError::Transport
        | ClientError::Timeout(ClientTimeout::Transport | ClientTimeout::Attempt) => true,
        ClientError::Configuration
        | ClientError::InvalidRequest(_)
        | ClientError::Encode
        | ClientError::Dns
        | ClientError::Tls
        | ClientError::Timeout(ClientTimeout::Operation)
        | ClientError::Redirect(_)
        | ClientError::Status(_)
        | ClientError::Decode(_)
        | ClientError::ResponseTooLarge { .. } => false,
    }
}

/// Whether a received response head earns another attempt.
pub(super) fn client_retryable_status(status: u16, method: &Method, policy: RetryPolicy) -> bool {
    permits_replay(policy, method) && is_transient_status(status)
}

/// Whether an attempt that failed with `category` before its response head
/// earns another attempt.
pub(super) fn client_retryable_failure(
    category: ClientError,
    method: &Method,
    policy: RetryPolicy,
) -> bool {
    permits_replay(policy, method) && is_transient_failure(category)
}

/// Whether a failed send earns another attempt.
///
/// The failure is read through the one client projection, so the category the
/// caller would receive is the category this decision is made on.
///
/// Semver-unsupported. It reads its arguments and changes nothing.
#[doc(hidden)]
pub fn client_retryable_transport(
    error: &reqwest::Error,
    method: &Method,
    policy: RetryPolicy,
) -> bool {
    client_retryable_failure(client_error_projection(error), method, policy)
}
