//! The reusable client: its configuration, its defaults, and the verb
//! conveniences on it and on the shared default client.

use super::super::policy_value::clamped_duration;
use super::super::transfer_budget::TransferBudget;
use super::error::ClientError;
use super::policy::OperationPolicy;
use super::projection::client_error_projection;
use super::redirect::RedirectPolicy;
use super::replay::RetryPolicy;
use super::request::ClientRequest;
use super::response::ClientResponse;
use crate::RuntimeError;
use reqwest::Method;
use reqwest::header::HeaderValue;
use std::sync::{LazyLock, OnceLock};
use std::time::Duration;

/// Hand every verb the client sends to `$generator`, so the client methods and
/// the free functions cannot drift apart.
///
/// Each item is `name => METHOD, body, "summary"`. A `no_body` item makes a
/// call without a request body; a content-type literal makes one that takes
/// `body: &str`, and that literal becomes a `HeaderValue` at compile time. The
/// generator appends its own timeout clause to the summary.
macro_rules! with_http_verbs {
    ($generator:ident) => {
        $generator! {
            get => GET, no_body, "Send an HTTP GET request";
            post => POST, "text/plain", "Send an HTTP POST request with a text body";
            post_json => POST, "application/json", "Send an HTTP POST request with a JSON body";
            post_form => POST, "application/x-www-form-urlencoded",
                "Send an HTTP POST request with a URL-encoded form body";
            put => PUT, "text/plain", "Send an HTTP PUT request with a text body";
            put_json => PUT, "application/json", "Send an HTTP PUT request with a JSON body";
            put_form => PUT, "application/x-www-form-urlencoded",
                "Send an HTTP PUT request with a URL-encoded form body";
            delete => DELETE, no_body, "Send an HTTP DELETE request";
            delete_with_body => DELETE, "text/plain",
                "Send an HTTP DELETE request with a text body";
            patch => PATCH, "text/plain", "Send an HTTP PATCH request with a text body";
            patch_json => PATCH, "application/json", "Send an HTTP PATCH request with a JSON body";
            patch_form => PATCH, "application/x-www-form-urlencoded",
                "Send an HTTP PATCH request with a URL-encoded form body";
            head => HEAD, no_body, "Send an HTTP HEAD request";
            options => OPTIONS, no_body, "Send an HTTP OPTIONS request";
        }
    };
}

/// Generate async HTTP method wrappers on `Client`, one verb at a time.
macro_rules! http_methods {
    () => {};
    ($name:ident => $method:ident, no_body, $summary:literal; $($rest:tt)*) => {
        #[doc = concat!($summary, " using the configured timeouts.")]
        pub async fn $name(&self, url: &str) -> Result<ClientResponse, RuntimeError> {
            self.convenience(Method::$method, url, None).await
        }
        http_methods! { $($rest)* }
    };
    ($name:ident => $method:ident, $ct:literal, $summary:literal; $($rest:tt)*) => {
        #[doc = concat!($summary, " using the configured timeouts.")]
        pub async fn $name(&self, url: &str, body: &str) -> Result<ClientResponse, RuntimeError> {
            self.convenience(
                Method::$method,
                url,
                Some((const { HeaderValue::from_static($ct) }, body)),
            )
            .await
        }
        http_methods! { $($rest)* }
    };
}

/// Generate free-standing async HTTP functions that delegate to the shared
/// default client, one verb at a time.
macro_rules! http_free_functions {
    () => {};
    ($name:ident => $method:ident, no_body, $summary:literal; $($rest:tt)*) => {
        #[doc = concat!($summary, " using default 30s timeouts.")]
        pub async fn $name(url: &str) -> Result<ClientResponse, RuntimeError> {
            SHARED.$name(url).await
        }
        http_free_functions! { $($rest)* }
    };
    ($name:ident => $method:ident, $ct:literal, $summary:literal; $($rest:tt)*) => {
        #[doc = concat!($summary, " using default 30s timeouts.")]
        pub async fn $name(url: &str, body: &str) -> Result<ClientResponse, RuntimeError> {
            SHARED.$name(url, body).await
        }
        http_free_functions! { $($rest)* }
    };
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_BACKOFF: Duration = Duration::from_millis(100);
/// The bound on a whole operation every client starts with.
const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
/// The buffered response maximum every client starts with, matching Camber's
/// ordinary request ceiling.
const DEFAULT_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
/// The shortest duration any infallible setter will hold.
const MIN_SETTER_DURATION: Duration = Duration::from_millis(1);

/// The response policy a client starts with: eight MiB, thirty seconds of
/// quiet between reads, and thirty seconds for one whole attempt.
const DEFAULT_RESPONSE_POLICY: TransferBudget = TransferBudget::of(
    Some(DEFAULT_RESPONSE_LIMIT),
    Some(DEFAULT_TIMEOUT),
    Some(DEFAULT_TIMEOUT),
);

/// A built client, or the category its build failed with.
///
/// The category is kept rather than the error because a cached failure is
/// handed to every later call. `reqwest::Error` does not clone, and its text
/// can repeat what the builder was given.
type BuiltClient = Result<reqwest::Client, ClientError>;

/// The client every free function sends through: the defaults, and one
/// transport shared by all of them.
static SHARED: LazyLock<Client> = LazyLock::new(client);

/// Read one cached build: the client, or its failure as this call's error.
fn cached_client(built: &BuiltClient) -> Result<&reqwest::Client, RuntimeError> {
    built
        .as_ref()
        .map_err(|failure| RuntimeError::Client(*failure))
}

/// Whether `client` holds a transport it has already built.
///
/// A client builds its transport on first use and discards it when a
/// transport setting is written, so this is `false` before the first call and
/// after such a write.
///
/// Semver-unsupported. It reads the client and changes nothing.
#[doc(hidden)]
pub fn client_transport_is_built(client: &Client) -> bool {
    client.cached_client.get().is_some()
}

/// Build the Reqwest client one connect bound and one quiet interval describe.
///
/// Reqwest enforces those two timers and no other. Its whole-request timeout
/// is never set: the operation owns the attempt timer, and a second timer on
/// the same boundary would compete with it to name the failure. An unbounded
/// quiet interval configures no timer at all rather than a very long one.
///
/// Reqwest's own retry policy is turned off. Its default resends a request
/// after a low-level protocol NACK, which would put a second sender of the same
/// request beside this module's coordinator: a request refused a replay
/// here could still be replayed there, under a budget no Camber setting names.
///
/// Reqwest's redirect following is turned off for the same reason. A followed
/// redirect is a second request, to a target the caller did not name, carrying
/// the first request's headers. The transport returns every redirect as the
/// answer it is, and the operation follows one only under the request's
/// `RedirectPolicy`. The automatic `Referer` goes with it, and no cookie store
/// is configured, so the transport adds no header that repeats where a request
/// came from or what an earlier answer set.
///
/// Automatic decoding is turned off for every codec Reqwest supports. Each one
/// is a Cargo feature another crate in the build can enable, and an enabled
/// codec would decode the body and remove the headers that describe it. The
/// explicit settings keep the returned bytes the bytes the peer sent.
fn build_client(connect_timeout: Duration, idle: Option<Duration>) -> BuiltClient {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .retry(reqwest::retry::never())
        .redirect(reqwest::redirect::Policy::none())
        .referer(false)
        .no_gzip()
        .no_brotli()
        .no_deflate()
        .no_zstd();
    if let Some(idle) = idle {
        builder = builder.read_timeout(idle);
    }
    builder
        .build()
        .map_err(|error| client_error_projection(&error))
}

/// Create a reusable client with the default timeout, retry, and response
/// configuration.
///
/// The value is ready to send. Each configuration method consumes it and
/// returns the changed client; there is no build step.
pub fn client() -> Client {
    Client {
        connect_timeout: DEFAULT_TIMEOUT,
        response: DEFAULT_RESPONSE_POLICY,
        retries: 0,
        backoff: DEFAULT_BACKOFF,
        redirect: RedirectPolicy::None,
        total_timeout: DEFAULT_TOTAL_TIMEOUT,
        cached_client: OnceLock::new(),
    }
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("total_timeout", &self.total_timeout)
            .field("connect_timeout", &self.connect_timeout)
            .field("response", &self.response)
            .field("retries", &self.retries)
            .field("backoff", &self.backoff)
            .field("redirect", &self.redirect)
            .finish()
    }
}

/// A reusable outbound HTTP client: its deadlines, its retries, and the
/// maximum one response may retain.
///
/// [`request`](Self::request) describes one request with any method, its own
/// headers, and its own response maximum. The verb methods are conveniences
/// over the same description. Requests share this client's connection pool
/// and nothing else: one request's headers, body, and limits never reach
/// another.
///
/// The four deadlines are independent boundaries, not one shared timeout:
///
/// - `total_timeout` covers one whole operation: every attempt, every followed
///   redirect, every delay between attempts, and the final response body;
/// - `request_timeout` covers one attempt, from its send through the end of
///   its response body;
/// - `connect_timeout` ends when the transport is established;
/// - `response_idle_timeout` covers each wait for response bytes.
///
/// | Dimension | Default |
/// | --- | --- |
/// | `total_timeout` | 30 seconds |
/// | `request_timeout` | 30 seconds |
/// | `connect_timeout` | 30 seconds |
/// | `response_idle_timeout` | 30 seconds |
/// | response maximum | eight MiB |
///
/// `request_timeout`, `response_idle_timeout`, and the response maximum are
/// one stored [`TransferBudget`], which
/// [`response_budget`](Self::response_budget) replaces whole and the named
/// setters write one field of. `total_timeout` is not part of it, so no write
/// to the budget removes it. Call order is authoritative: the last write to a
/// field is the one a request runs under.
///
/// Each timer names itself. An operation past `total_timeout` returns
/// `ClientTimeout::Operation`, and an attempt past `request_timeout` returns
/// `ClientTimeout::Attempt`. The transport owns the connect and quiet-interval
/// timers, and either one returns `ClientTimeout::Transport`.
///
/// A retry is one more attempt under the same policy. The retry count and the
/// backoff are unaffected by any of it. Whether a request may be sent again is
/// that request's own [`RetryPolicy`], not a setting of this client.
/// A call with zero retries runs under the same `total_timeout` as a call
/// with retries.
///
/// Every replay decision belongs to this client. The underlying Reqwest client
/// is built with its own retry policy disabled, so no second sender resends a
/// request this one refused to repeat.
///
/// Redirects are not followed by default. A 3xx answer is returned as the
/// response, with its `Location` header, and nothing is sent to the place it
/// names. [`redirect_policy`](Self::redirect_policy) lets requests follow a
/// redirect to the origin they began at, and to no other. The client sends no
/// `Referer`, keeps no cookies, and adds no credentials.
///
/// Every verb returns a [`ClientResponse`]. A 4xx or 5xx status is a response,
/// not a failure, and the body is returned as the peer sent it: automatic
/// decompression is disabled.
///
/// Each started call records one `http.client.completed` tracing event: the
/// method label, the host the call began at, the final status, the elapsed
/// milliseconds, the sends, the followed redirects, and a closed outcome. The
/// event holds no path, query, header value, or body. With the `otel` feature,
/// a call inside the tracing middleware sends that request's `traceparent`.
/// Use [`ClientRequest::trace_context`] to withhold it from one request.
///
/// Camber does not check the address a host resolves to. Send only to URLs
/// you trust.
///
/// The underlying `reqwest::Client` is built lazily on first request
/// and cached for subsequent calls. A setter that changes a transport setting
/// discards the cached client, so the next request builds one from the
/// current values.
pub struct Client {
    /// The bound on one whole operation. Outside the response policy, so no
    /// write to that policy can remove it.
    total_timeout: Duration,
    connect_timeout: Duration,
    /// The one response policy: byte maximum, quiet interval, and attempt
    /// lifetime. One store, so a caller reading it back sees what the client
    /// will be built from rather than three fields that can disagree.
    response: TransferBudget,
    retries: u32,
    backoff: Duration,
    /// The redirect policy a request described now starts with.
    redirect: RedirectPolicy,
    cached_client: OnceLock<BuiltClient>,
}

impl Client {
    /// Set the bound on one whole operation. Default: 30 seconds.
    /// Minimum: 1ms. Maximum: thirty years. Values outside are clamped.
    ///
    /// The deadline is fixed once, when a call is first polled, and no attempt
    /// or delay resets it. It encloses sending each attempt, waiting for each
    /// response head, every delay between attempts, and collecting the final
    /// response body. When it expires, the attempt in flight is dropped and
    /// the call returns `ClientTimeout::Operation` without starting another
    /// attempt. A delay that would end past it ends at it instead.
    ///
    /// It applies to every call, with zero [`retries`](Self::retries) or with
    /// more. No write to the response policy removes it, including
    /// `TransferBudget::unbounded()`. A request's own
    /// [`deadline`](ClientRequest::deadline) can end the operation sooner and
    /// cannot extend it.
    ///
    /// Runtime shutdown ends the same operation: a pending attempt or delay
    /// returns `RuntimeError::Cancelled`, and no further attempt starts.
    ///
    /// Each attempt keeps its own boundaries inside it. A connect, request, or
    /// response-idle timeout that expires first returns its own result. Before
    /// the response head arrives, that result is still eligible for another
    /// attempt; a failure while the final response body is collected is not.
    #[must_use]
    pub fn total_timeout(mut self, timeout: Duration) -> Self {
        self.total_timeout = clamped_duration(timeout, MIN_SETTER_DURATION, "total_timeout");
        self
    }

    /// Set the connect timeout. Minimum: 1ms. Maximum: thirty years. Values
    /// outside are clamped.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = clamped_duration(timeout, MIN_SETTER_DURATION, "connect_timeout");
        self.discard_transport();
        self
    }

    /// Set how long one attempt may take, from its send through the end of
    /// the response body. Minimum: 1ms. Zero values are clamped.
    ///
    /// The client owns this timer. An attempt past it returns
    /// `ClientTimeout::Attempt`. The per-read boundary is
    /// [`response_idle_timeout`](Self::response_idle_timeout).
    ///
    /// It writes the lifetime dimension of this client's one response policy,
    /// so a later [`response_budget`](Self::response_budget) replaces it.
    #[must_use]
    pub fn request_timeout(mut self, timeout: Duration) -> Self {
        self.response =
            self.response
                .with_clamped_total(timeout, MIN_SETTER_DURATION, "request_timeout");
        self
    }

    /// Set the longest quiet interval allowed while waiting for response
    /// bytes. Minimum: 1ms. Zero values are clamped.
    ///
    /// The interval resets on each successful read, so it detects a peer that
    /// stopped sending without bounding a large body that keeps arriving. The
    /// transport owns this timer, and a wait past it returns
    /// `ClientTimeout::Transport`. It
    /// writes the quiet-interval dimension of this client's one response
    /// policy, so a later [`response_budget`](Self::response_budget) replaces
    /// it.
    #[must_use]
    pub fn response_idle_timeout(mut self, timeout: Duration) -> Self {
        self.response =
            self.response
                .with_clamped_idle(timeout, MIN_SETTER_DURATION, "response_idle_timeout");
        self.discard_transport();
        self
    }

    /// Replace this client's whole response policy.
    ///
    /// All three dimensions at once: the buffered maximum, the quiet interval,
    /// and the attempt lifetime. A [`TransferBudget`] validates every finite
    /// value it holds, so a zero maximum or deadline is refused where it is
    /// written and no client is ever built from one.
    ///
    /// The budget can leave the attempt lifetime and the quiet interval
    /// unbounded. It cannot remove [`total_timeout`](Self::total_timeout),
    /// which is not one of its dimensions.
    #[must_use]
    pub fn response_budget(mut self, budget: TransferBudget) -> Self {
        self.response = budget;
        self.discard_transport();
        self
    }

    /// Collect responses with no size ceiling.
    ///
    /// **Warning:** a peer that answers with an unbounded or hostile body is
    /// then read entirely into this process's memory. Use it only for a peer
    /// you control and trust. It is deliberately named so its absence cannot
    /// be mistaken for a default. A budget with no maximum, given to
    /// [`Client::response_budget`], removes the maximum too. Every deadline
    /// survives it.
    #[must_use]
    pub fn unbounded_response(mut self) -> Self {
        self.response = self.response.without_max_bytes();
        self
    }

    /// The one response policy this client's requests run under.
    ///
    /// What the last write to each dimension left, which is what a call reads
    /// its maximum and its two deadlines from.
    #[must_use]
    pub fn response_policy(&self) -> TransferBudget {
        self.response
    }

    /// Set the most sends one call may add to its first one. Default: zero.
    ///
    /// A retry is earned by a 429, 502, 503, or 504 response, or by an attempt
    /// that fails before its response head: a refused or timed-out connect, a
    /// connection closed without an answer, or an expired
    /// [`request_timeout`](Self::request_timeout). A DNS failure and a TLS
    /// failure are returned at once. So is every failure after the response
    /// head.
    ///
    /// The count says how many times. A request's
    /// [`RetryPolicy`] says whether its method may be repeated at all. The
    /// default policy repeats only GET, HEAD, OPTIONS, PUT, and DELETE, so a
    /// POST through the verb conveniences is sent once whatever this count is.
    /// Use [`ClientRequest::retry_policy`] to permit one request's replay, and
    /// [`ClientRequest::retries`] to give one request its own count.
    ///
    /// Every send and every delay runs under
    /// [`total_timeout`](Self::total_timeout).
    pub fn retries(mut self, n: u32) -> Self {
        self.retries = n;
        self
    }

    /// Set the base backoff duration between retries.
    /// Actual delay: `base * 2^attempt + jitter`, with jitter below `base` and
    /// every step saturating. Minimum: 1ms. Maximum: thirty years. Values
    /// outside are clamped.
    ///
    /// A transient response's valid `Retry-After` replaces this delay for that
    /// retry. Delta-seconds waits that many seconds. An HTTP-date waits until
    /// that date, and a date now or in the past retries at once. An invalid
    /// value is ignored, and this backoff applies. Every delay is clipped to
    /// the [`total_timeout`](Self::total_timeout) deadline.
    pub fn backoff(mut self, duration: Duration) -> Self {
        self.backoff = clamped_duration(duration, MIN_SETTER_DURATION, "backoff");
        self
    }

    /// Set the redirect policy this client's requests start with. Default:
    /// [`RedirectPolicy::None`].
    ///
    /// Under [`RedirectPolicy::SameOrigin`], a request follows a redirect only
    /// to the scheme, host, and port it began at, and at most `max_hops` times
    /// in one call. Any other destination ends the call with a
    /// `ClientError::Redirect` before anything is sent to it. A followed
    /// redirect carries the request's headers, credentials included. Camber
    /// does not check the address a permitted host resolves to.
    ///
    /// A followed redirect is one more send under
    /// [`total_timeout`](Self::total_timeout), with its own
    /// [`request_timeout`](Self::request_timeout). It spends one hop and no
    /// retry. A retry after it repeats the request the redirect led to, not
    /// the first one. The verb conveniences run under this policy. Use
    /// [`ClientRequest::redirect_policy`] to give one request its own.
    #[must_use]
    pub fn redirect_policy(mut self, policy: RedirectPolicy) -> Self {
        self.redirect = policy;
        self
    }

    /// Describe one request with any HTTP method.
    ///
    /// Nothing is sent. The method and the target are validated here, and the
    /// returned [`ClientRequest`] takes headers, a body, a response maximum,
    /// its redirect and retry policies, and a deadline before
    /// [`send`](ClientRequest::send) runs it.
    ///
    /// `method` is any HTTP token, in the case it is written: `"PURGE"` is a
    /// method, and `"get"` is not `"GET"`. CONNECT is refused, because this
    /// client returns a buffered response and not a tunnel. An invalid method
    /// returns `ClientInput::Method`.
    ///
    /// `url` is an absolute `http` or `https` URL with a host and without a
    /// user name or password. Its fragment is removed. Any other target
    /// returns `ClientInput::Url`.
    pub fn request(&self, method: &str, url: &str) -> Result<ClientRequest<'_>, RuntimeError> {
        ClientRequest::parsed(self, method, url)
    }

    /// The transport every request through this client shares, built on first
    /// use.
    pub(super) fn transport(&self) -> Result<&reqwest::Client, RuntimeError> {
        cached_client(
            self.cached_client
                .get_or_init(|| build_client(self.connect_timeout, self.response.idle())),
        )
    }

    /// Forget the built transport after a transport setting changed.
    ///
    /// The next request builds one from the current settings. A client that
    /// kept the old transport would enforce the timers it was built with.
    fn discard_transport(&mut self) {
        self.cached_client = OnceLock::new();
    }

    /// The response ceiling a request described now starts with.
    pub(super) fn response_limit(&self) -> Option<usize> {
        self.response.max_bytes()
    }

    /// The operation settings a request described now runs under.
    pub(super) fn operation_policy(&self) -> OperationPolicy {
        OperationPolicy {
            total: self.total_timeout,
            deadline: None,
            attempt: self.response.total(),
            retries: self.retries,
            backoff: self.backoff,
            permission: RetryPolicy::default(),
            redirect: self.redirect,
            trace_context: true,
        }
    }

    /// Send one verb convenience through the general request description.
    async fn convenience(
        &self,
        method: Method,
        url: &str,
        content: Option<(HeaderValue, &str)>,
    ) -> Result<ClientResponse, RuntimeError> {
        ClientRequest::described(self, method, url)?
            .content(content)
            .send()
            .await
    }

    with_http_verbs!(http_methods);
}

with_http_verbs!(http_free_functions);
