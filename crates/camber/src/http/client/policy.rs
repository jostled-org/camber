//! The lifetime, retry, redirect, and trace settings one operation runs under.

use super::redirect::RedirectPolicy;
use super::replay::RetryPolicy;
use std::time::Duration;

/// One request's copy of its client's operation settings.
///
/// The copy is taken when the request is described, so a setter on the request
/// writes here and never to the client or to a sibling request. Every call has
/// one: a call with zero retries runs under the same total and the same
/// absolute deadline as a call with retries to spare.
#[derive(Clone)]
pub(super) struct OperationPolicy {
    /// The bound on the whole operation, measured from its first poll.
    pub(super) total: Duration,
    /// The caller's absolute ceiling. It can end the operation before the
    /// total does, and it cannot extend the total.
    pub(super) deadline: Option<std::time::Instant>,
    /// The bound on one attempt, from its send through the end of its body,
    /// or `None` when the response policy leaves that dimension unbounded.
    pub(super) attempt: Option<Duration>,
    /// How long one demand of a streamed body may wait for its chunk, or
    /// `None` when the response policy leaves that dimension unbounded. Only
    /// the streaming terminal reads it: a buffered call's transport owns its
    /// quiet interval.
    pub(super) idle: Option<Duration>,
    /// The most sends this operation may add to its first one.
    pub(super) retries: u32,
    pub(super) backoff: Duration,
    /// Whether this request's method may be sent again at all.
    pub(super) permission: RetryPolicy,
    /// Which redirect answers this operation follows.
    pub(super) redirect: RedirectPolicy,
    /// Whether the operation adds the caller's trace context to its request.
    pub(super) trace_context: bool,
}
