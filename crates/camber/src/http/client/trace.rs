//! The automatic trace context one operation sends.

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

/// The W3C trace context header.
pub(super) const TRACEPARENT: HeaderName = HeaderName::from_static("traceparent");

/// The `traceparent` an operation that starts now adds to its request, or
/// `None` when it adds none.
///
/// The task-local context is read here, at the operation's first poll. A
/// request that disabled the automatic context adds none, and a `traceparent`
/// the application wrote is kept as it is.
pub(super) fn entry_traceparent(enabled: bool, headers: &HeaderMap) -> Option<HeaderValue> {
    match enabled && !headers.contains_key(TRACEPARENT) {
        true => current_traceparent(),
        false => None,
    }
}

/// The calling task's trace context, rendered as a header value.
#[cfg(feature = "otel")]
fn current_traceparent() -> Option<HeaderValue> {
    // The rendered context is hexadecimal digits and hyphens, which is always
    // a header value.
    super::super::otel::current_context()
        .and_then(|ctx| HeaderValue::from_str(ctx.format_traceparent().as_str()).ok())
}

/// Without the `otel` feature no task holds a trace context, so no operation
/// adds one.
#[cfg(not(feature = "otel"))]
fn current_traceparent() -> Option<HeaderValue> {
    None
}
