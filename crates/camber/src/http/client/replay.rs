//! Whether one failed attempt may be sent again.
//!
//! Pure decisions over one outcome: status eligibility, method eligibility, and
//! transport evidence are answered here and nowhere else, so the sequence that
//! acts on them holds no second copy of the policy.

use reqwest::Method;

/// The statuses a peer uses to say it did not act on the request.
fn is_transient_status(status: u16) -> bool {
    matches!(status, 429 | 502..=504)
}

/// Whether a failed send reports a transport failure at all.
///
/// A builder or redirect failure is a fault in the call itself, so repeating it
/// repeats the fault.
fn is_transient_transport_error(error: &reqwest::Error) -> bool {
    !error.is_builder() && !error.is_redirect()
}

/// Whether a method asks a server to do nothing, so repeating it can duplicate
/// no server-visible work.
///
/// RFC 9110 safety, not idempotence: `PUT` and `DELETE` are idempotent, but a
/// duplicate of either still reaches an origin that may act on it.
fn is_safe_method(method: &Method) -> bool {
    matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS")
}

/// What a configured policy demands before this method's request is sent again.
///
/// Method eligibility on its own: it names the evidence a replay needs, and
/// says nothing about whether this attempt produced it.
enum ReplayEvidence {
    /// Repeating the method cannot duplicate server-visible work, so any
    /// transient failure is evidence enough.
    AnyTransient,
    /// The request may already have acted on the server, so only a failure
    /// proving no server connection was made permits a replay.
    ConnectStage,
    /// No configured policy replays this method.
    None,
}

fn replay_evidence(method: &Method, retry_unsafe_methods: bool) -> ReplayEvidence {
    match (is_safe_method(method), retry_unsafe_methods) {
        (true, _) => ReplayEvidence::AnyTransient,
        (false, true) => ReplayEvidence::ConnectStage,
        (false, false) => ReplayEvidence::None,
    }
}

/// Whether a received response authorizes another attempt.
///
/// A transient status is the server's own statement that it did not act on this
/// request, so the response answers the evidence question by itself and only
/// method eligibility remains.
pub(super) fn client_retryable_status(
    status: u16,
    method: &Method,
    retry_unsafe_methods: bool,
) -> bool {
    match replay_evidence(method, retry_unsafe_methods) {
        ReplayEvidence::None => false,
        ReplayEvidence::AnyTransient | ReplayEvidence::ConnectStage => is_transient_status(status),
    }
}

/// Whether a failed send left evidence that permits another attempt.
///
/// A connect-stage failure is the one transport classification that establishes
/// no server connection was made, so it is the only unsafe replay a policy can
/// authorize. Every other transport failure is ambiguous: request headers or a
/// body prefix may already have reached the peer, and nothing on this side
/// distinguishes a peer that ignored them from one that acted on them. The
/// caller receives the actual transport error instead of a second write.
pub fn client_retryable_transport(
    error: &reqwest::Error,
    method: &Method,
    retry_unsafe_methods: bool,
) -> bool {
    match (
        is_transient_transport_error(error),
        replay_evidence(method, retry_unsafe_methods),
    ) {
        (false, _) | (_, ReplayEvidence::None) => false,
        (true, ReplayEvidence::AnyTransient) => true,
        (true, ReplayEvidence::ConnectStage) => error.is_connect(),
    }
}
