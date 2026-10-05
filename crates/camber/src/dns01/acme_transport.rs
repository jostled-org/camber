//! The HTTP client every ACME request of one order travels through.
//!
//! instant-acme sends through this client, not its own: the instance's
//! configured roots join the platform roots, only HTTPS is spoken, and every
//! request waits at most the shared request cap narrowed by the order's
//! remaining time. A request the order has no time left for is not sent.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::http;
use instant_acme::{BodyWrapper, BytesResponse, HttpClient};
use rustls::pki_types::CertificateDer;
use tokio::time::Instant;

use super::acme_progress::AcmeProgress;
use super::transport::{BodyFailure, read_bounded, request_bound};

/// One order's ACME client: its trust and its deadline.
pub(super) struct AcmeTransport {
    client: reqwest::Client,
    deadline: Instant,
    progress: AcmeProgress,
}

impl AcmeTransport {
    /// A client trusting the platform roots plus `roots`, bounded by
    /// `deadline`.
    ///
    /// # Errors
    ///
    /// The builder's own failure, when the TLS backend refuses a root.
    pub(super) fn new(
        roots: &[CertificateDer<'static>],
        deadline: Instant,
        progress: AcmeProgress,
    ) -> Result<Self, reqwest::Error> {
        let client = super::transport::client_builder(roots)?
            .https_only(true)
            .build()?;
        Ok(Self {
            client,
            deadline,
            progress,
        })
    }
}

impl HttpClient for AcmeTransport {
    fn request(
        &self,
        request: http::Request<BodyWrapper<Bytes>>,
    ) -> Pin<Box<dyn Future<Output = Result<BytesResponse, instant_acme::Error>> + Send>> {
        let client = self.client.clone();
        let deadline = self.deadline;
        let progress = self.progress.clone();
        Box::pin(async move {
            send(client, deadline, request, progress)
                .await
                .map_err(|failure| instant_acme::Error::Other(Box::new(failure)))
        })
    }
}

/// Why one ACME request failed in Camber's transport.
#[derive(Debug)]
pub(super) enum AcmeTransportFailure {
    /// The request's bound passed before an answer arrived, or the order had
    /// no time left to send it.
    Timeout,
    /// The request could not be built or delivered, or its answer was lost.
    Unavailable(reqwest::Error),
    /// The answer arrived but could not be rebuilt for instant-acme.
    Unrebuilt(http::Error),
    /// The answer's body passed the shared body limit.
    TooLarge,
}

impl std::fmt::Display for AcmeTransportFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Timeout => f.write_str("ACME request passed its deadline"),
            Self::Unavailable(_) => f.write_str("ACME request could not be delivered"),
            Self::Unrebuilt(_) => f.write_str("ACME response could not be rebuilt"),
            Self::TooLarge => f.write_str("ACME response body passed its limit"),
        }
    }
}

impl std::error::Error for AcmeTransportFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Unavailable(error) => Some(error),
            Self::Unrebuilt(error) => Some(error),
            Self::Timeout | Self::TooLarge => None,
        }
    }
}

impl From<reqwest::Error> for AcmeTransportFailure {
    fn from(error: reqwest::Error) -> Self {
        match error.is_timeout() {
            true => Self::Timeout,
            false => Self::Unavailable(error),
        }
    }
}

impl From<BodyFailure> for AcmeTransportFailure {
    fn from(failure: BodyFailure) -> Self {
        match failure {
            BodyFailure::TooLarge => Self::TooLarge,
            BodyFailure::Transport(error) => error.into(),
        }
    }
}

/// Send one request within its bound and collect its bounded answer.
async fn send(
    client: reqwest::Client,
    deadline: Instant,
    request: http::Request<BodyWrapper<Bytes>>,
    progress: AcmeProgress,
) -> Result<BytesResponse, AcmeTransportFailure> {
    // The sequential SDK has consumed the prior response before requesting again.
    progress.settled();
    let bound = request_bound(deadline).ok_or(AcmeTransportFailure::Timeout)?;
    let request = outgoing(request, bound).await?;
    progress.submitting(&request);
    let mut response = client.execute(request).await.map_err(|error| {
        if error.is_connect() {
            progress.settled();
        }
        AcmeTransportFailure::from(error)
    })?;
    let mut answer = http::Response::builder()
        .status(response.status())
        .version(response.version());
    // The body is read through its own size hint, never these headers, so
    // they move into the rebuilt answer instead of being copied.
    if let Some(headers) = answer.headers_mut() {
        *headers = std::mem::take(response.headers_mut());
    }
    let body = read_bounded(response).await?;
    answer
        .body(Full::new(body))
        .map(BytesResponse::from)
        .map_err(AcmeTransportFailure::Unrebuilt)
}

/// Convert instant-acme's request into reqwest's, bounded by `bound`.
async fn outgoing(
    request: http::Request<BodyWrapper<Bytes>>,
    bound: Duration,
) -> Result<reqwest::Request, AcmeTransportFailure> {
    let (parts, body) = request.into_parts();
    let Ok(collected) = body.collect().await;
    let body = collected.to_bytes();
    let mut request = reqwest::Request::try_from(http::Request::from_parts(parts, body))?;
    *request.timeout_mut() = Some(bound);
    Ok(request)
}
