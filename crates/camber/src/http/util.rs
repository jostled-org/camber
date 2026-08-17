use super::rejection::SourceChain;

/// Strip surrounding double quotes from a header value (RFC 6265 / RFC 2616).
pub(crate) fn strip_quotes(v: &str) -> &str {
    match v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        true => &v[1..v.len() - 1],
        false => v,
    }
}

/// Why a blocking worker never handed back an answer.
///
/// One rule for every offloaded owner Camber awaits — the static-file reader and
/// the profiling sampler alike. A panic and a cancellation are different
/// failures: one is a fault inside the work, and the other is Camber's own
/// executor taking the work away. The panic keeps its payload, because the
/// payload is the only thing that says which work faulted.
pub(crate) fn blocking_worker_failed(joined: tokio::task::JoinError) -> crate::RuntimeError {
    match joined.is_panic() {
        true => crate::task::panic_to_error(joined.into_panic()),
        false => crate::RuntimeError::Cancelled,
    }
}

/// Map reqwest errors to RuntimeError, detecting timeouts.
///
/// The whole cause chain is rendered into the message. `reqwest`'s own
/// `Display` states the kind and the URL and nothing else, and
/// `RuntimeError::Http` carries an `Arc<str>` with no source for a recorder to
/// walk — so a record built from `to_string` alone told an operator that a
/// request failed and never that the connection was refused, the name did not
/// resolve, the handshake was rejected, or the peer reset mid-head.
///
/// The walk itself is `SourceChain`, the one place this crate spells it.
/// Rendered eagerly here, unlike at the rejection record, because the value it
/// becomes is an owned `Arc<str>` on the error itself: the chain is walked
/// once, where the source is still reachable, and every later reader gets the
/// same text.
pub(crate) fn map_reqwest_error(e: reqwest::Error) -> crate::RuntimeError {
    match e.is_timeout() {
        true => crate::RuntimeError::Timeout,
        false => crate::RuntimeError::Http(SourceChain(&e).to_string().into()),
    }
}
