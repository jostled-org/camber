//! One request and the collection of its answer.
//!
//! An exchange is what every attempt repeats: the same client, method, target,
//! and body, collected under the same response maximum. It holds no retry
//! policy and no deadline of its own beyond the ones its Reqwest client
//! enforces per attempt.

use super::super::Response;
use super::super::boundary::ByteBoundary;
use super::super::checked_collect::collect_response;
use super::super::map_reqwest_error;
use crate::RuntimeError;
use bytes::Bytes;
use reqwest::Method;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use std::borrow::Cow;

/// The W3C trace context header, validated at compile time rather than parsed
/// on every attempt.
#[cfg(feature = "otel")]
const TRACEPARENT: reqwest::header::HeaderName =
    reqwest::header::HeaderName::from_static("traceparent");

/// One request as every attempt of it is sent.
pub(super) struct Exchange<'a> {
    client: &'a reqwest::Client,
    method: &'a Method,
    url: &'a str,
    /// The content type and payload. The payload is copied once, here, and
    /// each attempt shares that copy. The content type is a static
    /// `HeaderValue`, so no attempt parses or copies it.
    body: Option<(HeaderValue, Bytes)>,
    /// The response ceiling, or `None` for the named opt-out.
    response_limit: Option<usize>,
}

impl<'a> Exchange<'a> {
    pub(super) fn new(
        client: &'a reqwest::Client,
        method: &'a Method,
        url: &'a str,
        body: Option<(HeaderValue, &str)>,
        response_limit: Option<usize>,
    ) -> Self {
        Self {
            client,
            method,
            url,
            body: body.map(|(content_type, payload)| {
                (content_type, Bytes::copy_from_slice(payload.as_bytes()))
            }),
            response_limit,
        }
    }

    pub(super) fn method(&self) -> &'a Method {
        self.method
    }

    pub(super) fn url(&self) -> &'a str {
        self.url
    }

    /// Send one attempt and wait for its response head.
    pub(super) async fn send(&self) -> Result<reqwest::Response, reqwest::Error> {
        let mut builder = self.client.request(self.method.clone(), self.url);
        if let Some((content_type, payload)) = &self.body {
            builder = builder
                .header(CONTENT_TYPE, content_type.clone())
                .body(payload.clone());
        }
        #[cfg(feature = "otel")]
        if let Some(ctx) = super::super::otel::current_context() {
            builder = builder.header(TRACEPARENT, ctx.format_traceparent().as_str());
        }
        builder.send().await
    }

    /// Read one answer's head, then collect its body under the configured
    /// maximum.
    ///
    /// The head is taken first because the ceiling refuses bodies: a caller told
    /// that its peer answered too large still learns the status and headers it
    /// answered with from the typed cause's own boundary, and nothing here has
    /// to re-read a response the collector consumed.
    pub(super) async fn collect(&self, resp: reqwest::Response) -> Result<Response, RuntimeError> {
        let status = resp.status().as_u16();

        let headers: Vec<_> = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                let name: Cow<'static, str> = Cow::Owned(k.as_str().to_owned());
                let value: Cow<'static, str> = Cow::Owned(v.to_str().unwrap_or("").to_owned());
                (name, value)
            })
            .collect();

        // No quiet interval of this collection's own: a client's response idle
        // deadline is configured on its Reqwest client and enforced per read
        // there, so arming a second timer here would be a second owner of one
        // dimension.
        let body_bytes = collect_response(
            resp,
            ByteBoundary::ClientResponse,
            self.response_limit,
            None,
        )
        .await?;

        Ok(Response::new(status, body_bytes, headers))
    }

    /// Run the exchange once, with no retry sequence around it.
    ///
    /// The attempt's own boundaries are the whole lifetime authority.
    pub(super) async fn once(&self) -> Result<Response, RuntimeError> {
        let resp = self.send().await.map_err(map_reqwest_error)?;
        self.collect(resp).await
    }
}
