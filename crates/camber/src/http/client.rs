use super::Response;
use super::boundary::ByteBoundary;
use super::checked_collect::collect_response;
use super::map_reqwest_error;
use super::method::Method as LocalMethod;
use super::mock;
use super::transfer_budget::TransferBudget;
use crate::RuntimeError;
use crate::runtime;
use reqwest::Method;
use std::borrow::Cow;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

/// Generate async HTTP method wrappers on `ClientBuilder`.
/// Two arms: `no_body` for methods without a request body,
/// and a content-type literal for methods that take `body: &str`.
macro_rules! http_methods {
    ($($(#[$meta:meta])* $name:ident => $method:ident, no_body;)*) => {
        $(
            $(#[$meta])*
            pub async fn $name(&self, url: &str) -> Result<Response, RuntimeError> {
                self.send(Method::$method, url, None).await
            }
        )*
    };
    ($($(#[$meta:meta])* $name:ident => $method:ident, $ct:literal;)*) => {
        $(
            $(#[$meta])*
            pub async fn $name(&self, url: &str, body: &str) -> Result<Response, RuntimeError> {
                self.send(Method::$method, url, Some(($ct, body))).await
            }
        )*
    };
    ($($(#[$meta:meta])* $name:ident => $method:ident, $kind:tt;)*) => {
        http_methods! { @split [] [] [$($(#[$meta])* $name => $method, $kind;)*] }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] []) => {
        http_methods! { $($nb)* }
        http_methods! { $($wb)* }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] [$(#[$meta:meta])* $name:ident => $method:ident, no_body; $($rest:tt)*]) => {
        http_methods! { @split [$($nb)* $(#[$meta])* $name => $method, no_body;] [$($wb)*] [$($rest)*] }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] [$(#[$meta:meta])* $name:ident => $method:ident, $ct:literal; $($rest:tt)*]) => {
        http_methods! { @split [$($nb)*] [$($wb)* $(#[$meta])* $name => $method, $ct;] [$($rest)*] }
    };
}

/// Generate free-standing async HTTP functions that delegate to `default_dispatch`.
macro_rules! http_free_functions {
    ($($(#[$meta:meta])* $name:ident => $method:ident, no_body;)*) => {
        $(
            $(#[$meta])*
            pub async fn $name(url: &str) -> Result<Response, RuntimeError> {
                default_dispatch(Method::$method, url, None).await
            }
        )*
    };
    ($($(#[$meta:meta])* $name:ident => $method:ident, $ct:literal;)*) => {
        $(
            $(#[$meta])*
            pub async fn $name(url: &str, body: &str) -> Result<Response, RuntimeError> {
                default_dispatch(Method::$method, url, Some(($ct, body))).await
            }
        )*
    };
    ($($(#[$meta:meta])* $name:ident => $method:ident, $kind:tt;)*) => {
        http_free_functions! { @split [] [] [$($(#[$meta])* $name => $method, $kind;)*] }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] []) => {
        http_free_functions! { $($nb)* }
        http_free_functions! { $($wb)* }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] [$(#[$meta:meta])* $name:ident => $method:ident, no_body; $($rest:tt)*]) => {
        http_free_functions! { @split [$($nb)* $(#[$meta])* $name => $method, no_body;] [$($wb)*] [$($rest)*] }
    };
    (@split [$($nb:tt)*] [$($wb:tt)*] [$(#[$meta:meta])* $name:ident => $method:ident, $ct:literal; $($rest:tt)*]) => {
        http_free_functions! { @split [$($nb)*] [$($wb)* $(#[$meta])* $name => $method, $ct;] [$($rest)*] }
    };
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_BACKOFF: Duration = Duration::from_millis(100);
/// The buffered response maximum every client starts with, matching Camber's
/// ordinary request ceiling.
const DEFAULT_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
/// The shortest deadline the infallible response setters will hold.
const MIN_RESPONSE_DEADLINE: Duration = Duration::from_millis(1);

/// The response policy a client starts with: eight MiB, thirty seconds of
/// quiet between body frames, and thirty seconds for one whole attempt.
const DEFAULT_RESPONSE_POLICY: TransferBudget = TransferBudget::of(
    Some(DEFAULT_RESPONSE_LIMIT),
    Some(DEFAULT_TIMEOUT),
    Some(DEFAULT_TIMEOUT),
);

static CLIENT: LazyLock<Result<reqwest::Client, Arc<str>>> = LazyLock::new(|| {
    build_client(DEFAULT_TIMEOUT, DEFAULT_RESPONSE_POLICY)
        .map_err(|e| -> Arc<str> { e.to_string().into() })
});

fn default_client() -> Result<&'static reqwest::Client, RuntimeError> {
    CLIENT
        .as_ref()
        .map_err(|e| RuntimeError::Http(Arc::clone(e)))
}

/// Build the Reqwest client one response policy describes.
///
/// The two deadlines are handed to the boundaries that actually enforce them:
/// the lifetime becomes Reqwest's whole-attempt timeout, and the quiet interval
/// becomes its per-read timeout. An unbounded dimension configures no timer at
/// all rather than a very long one.
fn build_client(
    connect_timeout: Duration,
    response: TransferBudget,
) -> Result<reqwest::Client, RuntimeError> {
    let mut builder = reqwest::Client::builder().connect_timeout(connect_timeout);
    if let Some(total) = response.total() {
        builder = builder.timeout(total);
    }
    if let Some(idle) = response.idle() {
        builder = builder.read_timeout(idle);
    }
    builder
        .build()
        .map_err(|e| RuntimeError::Http(e.to_string().into()))
}

fn client_build_error(err: RuntimeError) -> Arc<str> {
    match err {
        RuntimeError::Http(msg) => msg,
        other => other.to_string().into(),
    }
}

async fn build_and_send(
    client: &reqwest::Client,
    method: &Method,
    url: &str,
    body: Option<(&str, &str)>,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut builder = client.request(method.clone(), url);
    if let Some((content_type, payload)) = body {
        // reqwest::Body only implements From<&'static str> and From<String>,
        // so non-static &str requires an owned copy per attempt.
        builder = builder
            .header("content-type", content_type)
            .body(String::from(payload));
    }
    #[cfg(feature = "otel")]
    if let Some(ctx) = super::otel::current_context() {
        builder = builder.header("traceparent", ctx.format_traceparent().as_str());
    }
    builder.send().await
}

fn is_transient_status(status: u16) -> bool {
    matches!(status, 429 | 502..=504)
}

fn is_transient_transport_error(error: &reqwest::Error) -> bool {
    !error.is_builder() && !error.is_redirect()
}

fn is_retry_eligible(method: &Method, retry_unsafe_methods: bool) -> bool {
    retry_unsafe_methods || matches!(method.as_str(), "GET" | "HEAD" | "OPTIONS")
}

fn jitter_nanos() -> u64 {
    crate::prng::next_u64()
}

fn exponential_jitter(base: Duration, attempt: u32) -> Duration {
    let multiplier = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
    let exp = base.saturating_mul(multiplier);
    let jitter_bound = base.as_nanos().min(u128::from(u64::MAX)) as u64;
    let jitter = match jitter_bound {
        0 => Duration::ZERO,
        n => Duration::from_nanos(jitter_nanos() % n),
    };
    exp.saturating_add(jitter)
}

async fn sleep_backoff(base: Duration, attempt: u32, override_secs: Option<u64>) {
    let delay = match override_secs {
        Some(secs) => Duration::from_secs(secs),
        None => exponential_jitter(base, attempt),
    };
    tokio::time::sleep(delay).await;
}

/// Parse a `Retry-After` header value as whole seconds.
fn parse_retry_after(resp: &reqwest::Response) -> Option<u64> {
    resp.headers()
        .get("retry-after")?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()
}

/// What one dispatch runs under, once eligibility has been decided.
///
/// The transport, the retry schedule, and the ceiling the answer is collected
/// under travel together because they are one configuration: a retry that
/// reused a different client, or an answer collected under a different maximum,
/// would be a second policy for the same call.
#[derive(Clone, Copy)]
struct Dispatch<'a> {
    client: &'a reqwest::Client,
    retries: u32,
    backoff: Duration,
    /// The response ceiling, or `None` for the named opt-out.
    response_limit: Option<usize>,
}

async fn do_request_with_retry(
    dispatch: Dispatch<'_>,
    method: Method,
    url: &str,
    body: Option<(&str, &str)>,
) -> Result<Response, RuntimeError> {
    let Dispatch {
        client,
        retries,
        backoff,
        response_limit,
    } = dispatch;
    let mut remaining = retries;
    loop {
        let result = build_and_send(client, &method, url, body).await;
        let attempt = retries - remaining;
        match result {
            Ok(resp) if remaining > 0 && is_transient_status(resp.status().as_u16()) => {
                let status = resp.status().as_u16();
                let retry_after = parse_retry_after(&resp);
                drop(resp);
                tracing::debug!(
                    method = %method,
                    url = url,
                    status,
                    attempt = attempt + 1,
                    "retrying transient HTTP status"
                );
                sleep_backoff(backoff, attempt, retry_after).await;
                remaining -= 1;
            }
            Err(error) if remaining > 0 && is_transient_transport_error(&error) => {
                tracing::debug!(
                    method = %method,
                    url = url,
                    error = %error,
                    attempt = attempt + 1,
                    "retrying transient HTTP error"
                );
                sleep_backoff(backoff, attempt, None).await;
                remaining -= 1;
            }
            Ok(resp) => return read_response(resp, response_limit).await,
            Err(error) => return Err(map_reqwest_error(error)),
        }
    }
}

fn try_mock(method: &Method, url: &str) -> Option<Response> {
    let local_method = LocalMethod::from_reqwest(method);
    local_method.and_then(|m| mock::try_intercept(m, url))
}

async fn retry_dispatch(
    dispatch: Dispatch<'_>,
    method: Method,
    url: &str,
    body: Option<(&str, &str)>,
    retry_unsafe_methods: bool,
) -> Result<Response, RuntimeError> {
    runtime::check_cancel()?;
    let eligible = Dispatch {
        retries: match is_retry_eligible(&method, retry_unsafe_methods) {
            true => dispatch.retries,
            false => 0,
        },
        ..dispatch
    };
    match try_mock(&method, url) {
        Some(resp) => Ok(resp),
        None => do_request_with_retry(eligible, method, url, body).await,
    }
}

/// Create a client builder with custom timeout, retry, and response
/// configuration.
pub fn client() -> ClientBuilder {
    ClientBuilder {
        connect_timeout: DEFAULT_TIMEOUT,
        response: DEFAULT_RESPONSE_POLICY,
        retries: 0,
        backoff: DEFAULT_BACKOFF,
        retry_unsafe_methods: false,
        cached_client: OnceLock::new(),
    }
}

impl std::fmt::Debug for ClientBuilder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientBuilder")
            .field("connect_timeout", &self.connect_timeout)
            .field("response", &self.response)
            .field("retries", &self.retries)
            .field("backoff", &self.backoff)
            .field("retry_unsafe_methods", &self.retry_unsafe_methods)
            .finish()
    }
}

/// Builder for configuring outbound HTTP client deadlines, retries, and the
/// maximum one response may retain.
///
/// The three deadlines are independent boundaries, not one shared timeout:
///
/// - `connect_timeout` ends when the transport is established;
/// - `request_timeout` covers one whole attempt, from connect through the end
///   of the response body;
/// - `response_idle_timeout` covers each gap between response body reads.
///
/// | Dimension | Default |
/// | --- | --- |
/// | `connect_timeout` | 30 seconds |
/// | `request_timeout` | 30 seconds |
/// | `response_idle_timeout` | 30 seconds |
/// | response maximum | eight MiB |
///
/// The last three are one stored [`TransferBudget`], which
/// [`response_budget`](Self::response_budget) replaces whole and the named
/// setters write one field of. Call order is authoritative: the last write to a
/// field is the one the client is built from.
///
/// A retry is one more attempt under the same policy. Retry eligibility, count,
/// backoff, and the unsafe-method opt-in are unaffected by any of it; the
/// request-total deadline bounds each attempt rather than the sequence.
///
/// The underlying `reqwest::Client` is built lazily on first request
/// and cached for subsequent calls.
pub struct ClientBuilder {
    connect_timeout: Duration,
    /// The one response policy: byte maximum, quiet interval, and attempt
    /// lifetime. One store, so a caller reading it back sees what the client
    /// will be built from rather than three fields that can disagree.
    response: TransferBudget,
    retries: u32,
    backoff: Duration,
    retry_unsafe_methods: bool,
    cached_client: OnceLock<Result<reqwest::Client, Arc<str>>>,
}

impl ClientBuilder {
    /// Set the connect timeout. Minimum: 1ms. Zero values are clamped.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        const MIN: Duration = Duration::from_millis(1);
        self.connect_timeout = crate::time::clamp_duration(timeout, MIN, "connect_timeout");
        self
    }

    /// Set how long one whole attempt may take, from connect through the end
    /// of the response body. Minimum: 1ms. Zero values are clamped.
    ///
    /// This replaces the former `read_timeout`, which never owned a read-level
    /// boundary: the value has always bounded the complete attempt, and the
    /// name now says which deadline it is. The per-read boundary it was
    /// mistaken for is [`response_idle_timeout`](Self::response_idle_timeout).
    ///
    /// It writes the lifetime dimension of this client's one response policy,
    /// so a later [`response_budget`](Self::response_budget) replaces it.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.response =
            self.response
                .with_clamped_total(timeout, MIN_RESPONSE_DEADLINE, "request_timeout");
        self
    }

    /// Set the longest quiet interval allowed between response body reads.
    /// Minimum: 1ms. Zero values are clamped.
    ///
    /// The interval resets on each successful read, so it detects a peer that
    /// stopped sending without bounding a large body that keeps arriving. It
    /// writes the quiet-interval dimension of this client's one response
    /// policy, so a later [`response_budget`](Self::response_budget) replaces
    /// it.
    #[must_use]
    pub fn response_idle_timeout(mut self, timeout: Duration) -> Self {
        self.response = self.response.with_clamped_idle(
            timeout,
            MIN_RESPONSE_DEADLINE,
            "response_idle_timeout",
        );
        self
    }

    /// Replace this client's whole response policy.
    ///
    /// All three dimensions at once: the buffered maximum, the quiet interval,
    /// and the attempt lifetime. A [`TransferBudget`] validates every finite
    /// value it holds, so a zero maximum or deadline is refused where it is
    /// written and no client is ever built from one.
    #[must_use]
    pub fn response_budget(mut self, budget: TransferBudget) -> Self {
        self.response = budget;
        self
    }

    /// Collect responses with no size ceiling.
    ///
    /// **Warning:** a peer that answers with an unbounded or hostile body is
    /// then read entirely into this process's memory. Use it only for a peer
    /// you control and trust. This is the only client configuration that
    /// removes the response maximum, and it is deliberately named so its
    /// absence cannot be mistaken for a default. Both deadlines survive it.
    #[must_use]
    pub fn unbounded_response(mut self) -> Self {
        self.response = self.response.without_max_bytes();
        self
    }

    /// The one response policy this client will be built from.
    ///
    /// What the last write to each dimension left, which is what a call reads
    /// its maximum and its two deadlines from.
    #[must_use]
    pub fn response_policy(&self) -> TransferBudget {
        self.response
    }

    /// Set the maximum number of retries for transient failures.
    ///
    /// Transient failures are request transport errors, response-header timeouts, and
    /// 429, 502, 503, or 504 responses. By default, configured retries apply
    /// only to GET, HEAD, and OPTIONS requests.
    pub fn retries(mut self, n: u32) -> Self {
        self.retries = n;
        self
    }

    /// Allow configured retries for POST, PUT, PATCH, and DELETE requests.
    ///
    /// This opt-in applies to both transient transport failures and transient
    /// response statuses. Request bodies are sent again on each retry.
    /// By default, only GET, HEAD, and OPTIONS requests are retried.
    pub fn retry_unsafe_methods(mut self, enabled: bool) -> Self {
        self.retry_unsafe_methods = enabled;
        self
    }

    /// Set the base backoff duration between retries.
    /// Actual delay: `base * 2^attempt + jitter`. Minimum: 1ms. Zero values are clamped.
    pub fn backoff(mut self, duration: Duration) -> Self {
        const MIN: Duration = Duration::from_millis(1);
        self.backoff = crate::time::clamp_duration(duration, MIN, "backoff");
        self
    }

    fn get_client(&self) -> Result<&reqwest::Client, RuntimeError> {
        self.cached_client
            .get_or_init(|| {
                build_client(self.connect_timeout, self.response).map_err(client_build_error)
            })
            .as_ref()
            .map_err(|e| RuntimeError::Http(Arc::clone(e)))
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<(&str, &str)>,
    ) -> Result<Response, RuntimeError> {
        let dispatch = Dispatch {
            client: self.get_client()?,
            retries: self.retries,
            backoff: self.backoff,
            response_limit: self.response.max_bytes(),
        };
        retry_dispatch(dispatch, method, url, body, self.retry_unsafe_methods).await
    }

    http_methods! {
        /// Send an HTTP GET request with the configured timeouts.
        get => GET, no_body;
        /// Send an HTTP POST request with a text body using the configured timeouts.
        post => POST, "text/plain";
        /// Send an HTTP POST request with a JSON body using the configured timeouts.
        post_json => POST, "application/json";
        /// Send an HTTP POST request with a URL-encoded form body using the configured timeouts.
        post_form => POST, "application/x-www-form-urlencoded";
        /// Send an HTTP PUT request with a text body using the configured timeouts.
        put => PUT, "text/plain";
        /// Send an HTTP PUT request with a JSON body using the configured timeouts.
        put_json => PUT, "application/json";
        /// Send an HTTP PUT request with a URL-encoded form body using the configured timeouts.
        put_form => PUT, "application/x-www-form-urlencoded";
        /// Send an HTTP DELETE request with the configured timeouts.
        delete => DELETE, no_body;
        /// Send an HTTP DELETE request with a text body using the configured timeouts.
        delete_with_body => DELETE, "text/plain";
        /// Send an HTTP PATCH request with a text body using the configured timeouts.
        patch => PATCH, "text/plain";
        /// Send an HTTP PATCH request with a JSON body using the configured timeouts.
        patch_json => PATCH, "application/json";
        /// Send an HTTP PATCH request with a URL-encoded form body using the configured timeouts.
        patch_form => PATCH, "application/x-www-form-urlencoded";
        /// Send an HTTP HEAD request with the configured timeouts.
        head => HEAD, no_body;
        /// Send an HTTP OPTIONS request with the configured timeouts.
        options => OPTIONS, no_body;
    }
}

/// Async dispatch using the shared default client (no retries).
async fn default_dispatch(
    method: Method,
    url: &str,
    body: Option<(&str, &str)>,
) -> Result<Response, RuntimeError> {
    let dispatch = Dispatch {
        client: default_client()?,
        retries: 0,
        backoff: Duration::ZERO,
        response_limit: DEFAULT_RESPONSE_POLICY.max_bytes(),
    };
    retry_dispatch(dispatch, method, url, body, false).await
}

http_free_functions! {
    /// Send an HTTP GET request with default 30s timeouts.
    get => GET, no_body;
    /// Send an HTTP POST request with a text body and default 30s timeouts.
    post => POST, "text/plain";
    /// Send an HTTP POST request with a JSON body and default 30s timeouts.
    post_json => POST, "application/json";
    /// Send an HTTP POST request with a URL-encoded form body and default 30s timeouts.
    post_form => POST, "application/x-www-form-urlencoded";
    /// Send an HTTP PUT request with a text body and default 30s timeouts.
    put => PUT, "text/plain";
    /// Send an HTTP PUT request with a JSON body and default 30s timeouts.
    put_json => PUT, "application/json";
    /// Send an HTTP PUT request with a URL-encoded form body and default 30s timeouts.
    put_form => PUT, "application/x-www-form-urlencoded";
    /// Send an HTTP DELETE request with default 30s timeouts.
    delete => DELETE, no_body;
    /// Send an HTTP DELETE request with a text body and default 30s timeouts.
    delete_with_body => DELETE, "text/plain";
    /// Send an HTTP PATCH request with a text body and default 30s timeouts.
    patch => PATCH, "text/plain";
    /// Send an HTTP PATCH request with a JSON body and default 30s timeouts.
    patch_json => PATCH, "application/json";
    /// Send an HTTP PATCH request with a URL-encoded form body and default 30s timeouts.
    patch_form => PATCH, "application/x-www-form-urlencoded";
    /// Send an HTTP HEAD request with default 30s timeouts.
    head => HEAD, no_body;
    /// Send an HTTP OPTIONS request with default 30s timeouts.
    options => OPTIONS, no_body;
}

/// Read one answer's head, then collect its body under the configured maximum.
///
/// The head is taken first because the ceiling refuses bodies: a caller told
/// that its peer answered too large still learns the status and headers it
/// answered with from the typed cause's own boundary, and nothing here has to
/// re-read a response the collector consumed.
async fn read_response(
    resp: reqwest::Response,
    response_limit: Option<usize>,
) -> Result<Response, RuntimeError> {
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

    let body_bytes = collect_response(resp, ByteBoundary::ClientResponse, response_limit).await?;

    Ok(Response::new(status, body_bytes, headers))
}
