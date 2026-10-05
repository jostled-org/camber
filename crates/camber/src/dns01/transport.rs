//! The HTTP bounds every DNS-01 request shares.
//!
//! One provider or directory request waits at most [`REQUEST_CAP`], and never
//! past the enclosing order's deadline. A response body is collected to at
//! most [`BODY_LIMIT`] bytes before anything parses it. Neither client follows
//! a redirect, so a credential sent to one origin never reaches another.

use std::time::Duration;

use bytes::{Bytes, BytesMut};
use tokio::time::Instant;

/// The longest one provider or directory request may wait.
pub(super) const REQUEST_CAP: Duration = Duration::from_secs(10);

/// The largest response body read before it is refused.
pub(super) const BODY_LIMIT: usize = 1024 * 1024;

/// How long one request may wait: [`REQUEST_CAP`], narrowed by what remains
/// before `deadline`. `None` once the deadline has passed.
pub(super) fn request_bound(deadline: Instant) -> Option<Duration> {
    match deadline.saturating_duration_since(Instant::now()) {
        Duration::ZERO => None,
        remaining => Some(remaining.min(REQUEST_CAP)),
    }
}

/// Whether `url` carries no user name, password, or fragment, so it names an
/// endpoint and nothing else.
pub(super) fn anonymous(url: &reqwest::Url) -> bool {
    url.username().is_empty() && url.password().is_none() && url.fragment().is_none()
}

/// A client builder that follows no redirect and waits at most
/// [`REQUEST_CAP`] per request, trusting the platform roots plus `roots`.
///
/// # Errors
///
/// The certificate parser's refusal of a root.
pub(super) fn client_builder(
    roots: &[rustls::pki_types::CertificateDer<'static>],
) -> Result<reqwest::ClientBuilder, reqwest::Error> {
    let roots = roots
        .iter()
        .map(|root| reqwest::Certificate::from_der(root.as_ref()))
        .collect::<Result<Box<[_]>, _>>()?;
    Ok(reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(REQUEST_CAP)
        .tls_certs_merge(roots))
}

/// Why a response body could not be collected.
#[derive(Debug, thiserror::Error)]
pub(super) enum BodyFailure {
    /// The body passed [`BODY_LIMIT`]; nothing past the limit was kept.
    #[error("response body exceeds {BODY_LIMIT} bytes")]
    TooLarge,
    /// The transport failed while the body was read.
    #[error("response body transport failed")]
    Transport(#[source] reqwest::Error),
}

/// Collect `response`'s body, refusing it once it passes [`BODY_LIMIT`].
///
/// A declared length over the limit is refused before any chunk is read.
pub(super) async fn read_bounded(mut response: reqwest::Response) -> Result<Bytes, BodyFailure> {
    let declared = response
        .content_length()
        .and_then(|length| usize::try_from(length).ok());
    match declared {
        Some(length) if length > BODY_LIMIT => return Err(BodyFailure::TooLarge),
        _ => {}
    }
    let mut body = BytesMut::with_capacity(declared.unwrap_or(0));
    while let Some(chunk) = response.chunk().await.map_err(BodyFailure::Transport)? {
        match body.len().checked_add(chunk.len()) {
            Some(total) if total <= BODY_LIMIT => body.extend_from_slice(&chunk),
            _ => return Err(BodyFailure::TooLarge),
        }
    }
    Ok(body.freeze())
}
