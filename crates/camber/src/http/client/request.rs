//! The public description of one outbound request.

use super::buffered::operate;
use super::description::{Description, request_method};
use super::error::ClientError;
use super::policy::OperationPolicy;
use super::redirect::RedirectPolicy;
use super::replay::RetryPolicy;
use super::response::ClientResponse;
use super::reusable::Client;
use super::stream::{ClientStreamResponse, operate as operate_streaming};
use crate::RuntimeError;
use bytes::Bytes;
use reqwest::Method;
use reqwest::header::{CONTENT_TYPE, HeaderValue};
use serde::Serialize;

/// The content type [`ClientRequest::json`] declares.
const APPLICATION_JSON: HeaderValue = HeaderValue::from_static("application/json");

/// One outbound request, described before it is sent.
///
/// [`Client::request`] validates the method and the target and returns this
/// description. Each setter consumes it and returns it, and
/// [`send`](Self::send) or [`send_streaming`](Self::send_streaming) runs it.
/// Nothing reaches the network before that.
///
/// The description owns its target, headers, and body, and holds its own copy
/// of the client's response ceiling, operation total, retry settings, and
/// redirect policy, and its own trace policy. A setter changes this request
/// only: another request through the same client, and the client itself,
/// keep their own values.
///
/// ```no_run
/// # async fn call(client: &camber::http::Client) -> Result<(), camber::RuntimeError> {
/// let response = client
///     .request("POST", "https://provider.example/v1/answers")?
///     .sensitive_header("authorization", "Bearer token")?
///     .json(&serde_json::json!({ "input": "summarize" }))?
///     .response_limit(1_048_576)?
///     .send()
///     .await?
///     .error_for_status()?;
/// # let _ = response;
/// # Ok(())
/// # }
/// ```
///
/// `Debug` prints the method, the header names, the body length, and the
/// response ceiling. It never prints the target, a header value, or the body.
pub struct ClientRequest<'a> {
    client: &'a Client,
    description: Description,
    /// The client's operation settings when this request was described, and
    /// this request's own deadline.
    policy: OperationPolicy,
}

impl<'a> ClientRequest<'a> {
    /// Describe a request whose method is already a valid token.
    pub(super) fn described(
        client: &'a Client,
        method: Method,
        url: &str,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            client,
            description: Description::new(method, url, client.response_limit())?,
            policy: client.operation_policy(),
        })
    }

    /// Describe a request from a method string.
    pub(super) fn parsed(
        client: &'a Client,
        method: &str,
        url: &str,
    ) -> Result<Self, RuntimeError> {
        Self::described(client, request_method(method)?, url)
    }

    /// Give the request the content a verb convenience was called with.
    pub(super) fn content(mut self, content: Option<(HeaderValue, &str)>) -> Self {
        if let Some((content_type, payload)) = content {
            self.description
                .replace_header(CONTENT_TYPE, content_type, false);
            self.description
                .replace_body(Bytes::copy_from_slice(payload.as_bytes()));
        }
        self
    }

    /// Set the application header `name`, replacing any value it holds.
    ///
    /// The name is matched without regard to case. A name or value outside the
    /// HTTP grammar, including one with a CR or LF, returns
    /// `ClientInput::Header`. Host, Content-Length, Transfer-Encoding,
    /// Connection, Upgrade, TE, Trailer, and Proxy-Authorization belong to the
    /// transport and return `ClientInput::ReservedHeader`.
    ///
    /// Authorization and Cookie are always sensitive. A name set through
    /// [`sensitive_header`](Self::sensitive_header) stays sensitive when this
    /// method replaces its value.
    pub fn header(mut self, name: &str, value: &str) -> Result<Self, RuntimeError> {
        self.description.write_header(name, value, false)?;
        Ok(self)
    }

    /// Set the application header `name` and mark it sensitive.
    ///
    /// The rules of [`header`](Self::header) apply. The mark is also set on
    /// the transport's header value, so the transport does not log it or index
    /// it for HTTP/2 header compression.
    pub fn sensitive_header(mut self, name: &str, value: &str) -> Result<Self, RuntimeError> {
        self.description.write_header(name, value, true)?;
        Ok(self)
    }

    /// Replace the body with these bytes.
    ///
    /// The bytes are sent as they are. This method does not set or remove
    /// `Content-Type`.
    #[must_use]
    pub fn body(mut self, value: impl Into<Bytes>) -> Self {
        self.description.replace_body(value.into());
        self
    }

    /// Serialize `value` as the JSON body and set `Content-Type` to
    /// `application/json`.
    ///
    /// The value is serialized once, here. A value that does not serialize
    /// returns `ClientError::Encode`.
    pub fn json<T: Serialize + ?Sized>(mut self, value: &T) -> Result<Self, RuntimeError> {
        let encoded =
            serde_json::to_vec(value).map_err(|_| RuntimeError::Client(ClientError::Encode))?;
        self.description.replace_body(Bytes::from(encoded));
        self.description
            .replace_header(CONTENT_TYPE, APPLICATION_JSON, false);
        Ok(self)
    }

    /// Replace the largest response body this request retains, or the most
    /// body bytes it delivers as a stream.
    ///
    /// Zero returns `ClientInput::ResponseLimit`.
    pub fn response_limit(mut self, max_bytes: usize) -> Result<Self, RuntimeError> {
        self.description.limit_response(max_bytes)?;
        Ok(self)
    }

    /// Set an absolute deadline for the whole operation.
    ///
    /// The operation ends at the earlier of this instant and the client's
    /// [`total_timeout`](Client::total_timeout), measured from the first poll
    /// of [`send`](Self::send). A deadline cannot extend the total. A deadline
    /// already in the past sends nothing: `send` returns
    /// `ClientTimeout::Operation`.
    #[must_use]
    pub fn deadline(mut self, deadline: std::time::Instant) -> Self {
        self.policy.deadline = Some(deadline);
        self
    }

    /// Replace this request's redirect policy.
    ///
    /// The request starts with the policy of its client's
    /// [`redirect_policy`](Client::redirect_policy), which is
    /// [`RedirectPolicy::None`] unless it was set.
    #[must_use]
    pub fn redirect_policy(mut self, policy: RedirectPolicy) -> Self {
        self.policy.redirect = policy;
        self
    }

    /// Replace this request's replay permission. Default:
    /// [`RetryPolicy::Idempotent`].
    ///
    /// The permission says whether this request's method may be sent again
    /// after a transient outcome. It does not send again without a retry
    /// count: see [`retries`](Self::retries).
    #[must_use]
    pub fn retry_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy.permission = policy;
        self
    }

    /// Replace the most sends this request may add to its first one.
    ///
    /// The request starts with the count of its client's
    /// [`retries`](Client::retries). Zero sends once.
    #[must_use]
    pub fn retries(mut self, count: u32) -> Self {
        self.policy.retries = count;
        self
    }

    /// Say whether this request carries the caller's trace context. Default:
    /// `true`.
    ///
    /// With the `otel` feature, a request sent inside the tracing middleware
    /// adds the `traceparent` of the task that first polls
    /// [`send`](Self::send). Every retry and every followed redirect of the
    /// call carries that one value. `false` adds none. A `traceparent` set
    /// through [`header`](Self::header) is sent as it is under either
    /// setting. A sensitive trace header is removed on an origin change.
    /// Without the `otel` feature no request adds one.
    #[must_use]
    pub fn trace_context(mut self, enabled: bool) -> Self {
        self.policy.trace_context = enabled;
        self
    }

    /// Send the request and collect its complete response.
    ///
    /// The checks that need more than one field run first, and a refused
    /// description sends nothing. A TRACE request with a body returns
    /// `ClientInput::Body`. Every status is a response: use
    /// [`ClientResponse::error_for_status`] to refuse a 400–599 status.
    ///
    /// The operation begins when this future is first polled. Every attempt,
    /// every followed redirect, every delay between attempts, and the final
    /// body run under one deadline, with or without retries. Past it, `send` returns
    /// `ClientTimeout::Operation`. Runtime shutdown returns
    /// `RuntimeError::Cancelled`. Dropping the future drops the attempt in
    /// flight, and nothing is sent afterwards. Polled outside a Tokio
    /// executor, it returns `RuntimeError::NoRuntime`.
    ///
    /// A started operation records one `http.client.completed` tracing event,
    /// when it returns or when its pending future is dropped. The event holds
    /// the method label, the host the call began at, the final status, the
    /// elapsed milliseconds, the sends, the followed redirects, and the
    /// outcome. It holds no path, query, header value, or body. A future that
    /// is never polled records nothing.
    pub async fn send(self) -> Result<ClientResponse, RuntimeError> {
        operate(self.client, self.description, self.policy).await
    }

    /// Send the request and return the response head with its body unread.
    ///
    /// The returned [`ClientStreamResponse`] holds the final status and
    /// headers. Its body is read one chunk at a time through
    /// [`ClientBody`](crate::http::ClientBody), and Camber reads the upstream
    /// only when a chunk is asked for. Every status is a response.
    ///
    /// ```no_run
    /// # use camber::http::{Bytes, Client};
    /// # async fn write_chunk(chunk: Bytes) -> Result<(), camber::RuntimeError> {
    /// #     drop(chunk);
    /// #     Ok(())
    /// # }
    /// # async fn fetch(client: &Client, source_url: &str) -> Result<(), camber::RuntimeError> {
    /// let response = client
    ///     .request("GET", source_url)?
    ///     .response_limit(64 * 1024 * 1024)?
    ///     .send_streaming()
    ///     .await?
    ///     .error_for_status()?;
    /// let mut body = response.into_body();
    /// while let Some(chunk) = body.next().await? {
    ///     write_chunk(chunk).await?;
    /// }
    /// body.close().await?;
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// The checks of [`send`](Self::send) apply, and so do its retries and
    /// redirects, up to the final head. After the head is returned nothing is
    /// sent again: not for a body failure, and not for a status conversion.
    ///
    /// A stream needs a finite response maximum and a finite response idle
    /// interval. A client whose policy removed either returns
    /// `ClientInput::StreamBudget` before anything is resolved or dialed.
    /// [`response_limit`](Self::response_limit) supplies the maximum for one
    /// request. A head that declares more than the maximum returns
    /// `ClientError::ResponseTooLarge`, and no body byte is read.
    ///
    /// The operation begins when this future is first polled, and that poll
    /// fixes its deadline, its trace context, and the runtime that owns it.
    /// The deadline covers the head, the body, and the time the caller holds
    /// the response between reads. The attempt timeout continues through the
    /// body the same way. The idle interval measures only a pending
    /// [`ClientBody::next`](crate::http::ClientBody::next).
    ///
    /// One task owns the response. Under a Camber runtime it belongs to the
    /// runtime that first polled this future, and a runtime whose scope is
    /// closed returns `RuntimeError::ScopeClosed` and sends nothing. Runtime
    /// shutdown ends the response and returns `RuntimeError::Cancelled`.
    /// Outside a Camber runtime the returned handle owns the task. Polled
    /// outside a Tokio executor, this future returns
    /// `RuntimeError::NoRuntime`.
    ///
    /// Dropping this future ends the operation. A returned error means the
    /// upstream is already released.
    ///
    /// A started operation records one `http.client.completed` tracing event,
    /// after the upstream is released. A body read to its clean end records
    /// `response`. A response or body that is dropped or closed first records
    /// `abandoned`. A failure after the head keeps the status of that head.
    pub async fn send_streaming(self) -> Result<ClientStreamResponse, RuntimeError> {
        operate_streaming(self.client, self.description, self.policy).await
    }
}

/// Whether the header `name` of `request` is marked sensitive on the value the
/// transport is given, or `None` when the request holds no such header.
///
/// Semver-unsupported. It reads the request and changes nothing.
#[doc(hidden)]
pub fn client_header_sensitivity(request: &ClientRequest<'_>, name: &str) -> Option<bool> {
    request.description.header_sensitivity(name)
}

/// The header names a `Debug` rendering lists.
struct HeaderNames<'a>(&'a Description);

impl std::fmt::Debug for HeaderNames<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(
                self.0
                    .headers()
                    .keys()
                    .map(reqwest::header::HeaderName::as_str),
            )
            .finish()
    }
}

impl std::fmt::Debug for ClientRequest<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientRequest")
            .field("method", &self.description.method().as_str())
            .field("header_names", &HeaderNames(&self.description))
            .field("body_len", &self.description.body().len())
            .field("response_limit", &self.description.response_limit())
            .finish()
    }
}
