//! The public client surface: its builder, its defaults, and the free
//! functions that use the shared default client.

use super::super::Response;
use super::super::policy_value::clamped_duration;
use super::super::transfer_budget::TransferBudget;
use super::dispatch::Dispatch;
use super::sequence::RetryPlan;
use crate::RuntimeError;
use reqwest::Method;
use reqwest::header::HeaderValue;
use std::sync::{Arc, LazyLock, OnceLock};
use std::time::Duration;

/// Hand every verb the client sends to `$generator`, so the builder methods and
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

/// Generate async HTTP method wrappers on `ClientBuilder`, one verb at a time.
macro_rules! http_methods {
    () => {};
    ($name:ident => $method:ident, no_body, $summary:literal; $($rest:tt)*) => {
        #[doc = concat!($summary, " using the configured timeouts.")]
        pub async fn $name(&self, url: &str) -> Result<Response, RuntimeError> {
            self.send(Method::$method, url, None).await
        }
        http_methods! { $($rest)* }
    };
    ($name:ident => $method:ident, $ct:literal, $summary:literal; $($rest:tt)*) => {
        #[doc = concat!($summary, " using the configured timeouts.")]
        pub async fn $name(&self, url: &str, body: &str) -> Result<Response, RuntimeError> {
            self.send(Method::$method, url, Some((const { HeaderValue::from_static($ct) }, body)))
                .await
        }
        http_methods! { $($rest)* }
    };
}

/// Generate free-standing async HTTP functions that delegate to
/// `default_dispatch`, one verb at a time.
macro_rules! http_free_functions {
    () => {};
    ($name:ident => $method:ident, no_body, $summary:literal; $($rest:tt)*) => {
        #[doc = concat!($summary, " using default 30s timeouts.")]
        pub async fn $name(url: &str) -> Result<Response, RuntimeError> {
            default_dispatch(Method::$method, url, None).await
        }
        http_free_functions! { $($rest)* }
    };
    ($name:ident => $method:ident, $ct:literal, $summary:literal; $($rest:tt)*) => {
        #[doc = concat!($summary, " using default 30s timeouts.")]
        pub async fn $name(url: &str, body: &str) -> Result<Response, RuntimeError> {
            default_dispatch(
                Method::$method,
                url,
                Some((const { HeaderValue::from_static($ct) }, body)),
            )
            .await
        }
        http_free_functions! { $($rest)* }
    };
}

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
const DEFAULT_BACKOFF: Duration = Duration::from_millis(100);
/// The bound on a whole retry sequence every client starts with.
const DEFAULT_RETRY_TIMEOUT: Duration = Duration::from_secs(30);
/// The buffered response maximum every client starts with, matching Camber's
/// ordinary request ceiling.
const DEFAULT_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;
/// The shortest duration any infallible setter will hold.
const MIN_SETTER_DURATION: Duration = Duration::from_millis(1);

/// The response policy a client starts with: eight MiB, thirty seconds of
/// quiet between body frames, and thirty seconds for one whole attempt.
const DEFAULT_RESPONSE_POLICY: TransferBudget = TransferBudget::of(
    Some(DEFAULT_RESPONSE_LIMIT),
    Some(DEFAULT_TIMEOUT),
    Some(DEFAULT_TIMEOUT),
);

/// A built client, or the message its build failed with.
///
/// The message is kept rather than the error because a cached failure is
/// handed to every later call, and `reqwest::Error` does not clone.
type BuiltClient = Result<reqwest::Client, Arc<str>>;

static CLIENT: LazyLock<BuiltClient> =
    LazyLock::new(|| build_client(DEFAULT_TIMEOUT, DEFAULT_RESPONSE_POLICY));

/// Read one cached build: the client, or its failure as this call's error.
fn cached_client(built: &BuiltClient) -> Result<&reqwest::Client, RuntimeError> {
    built
        .as_ref()
        .map_err(|e| RuntimeError::Http(Arc::clone(e)))
}

/// Build the Reqwest client one response policy describes.
///
/// The two deadlines are handed to the boundaries that actually enforce them:
/// the lifetime becomes Reqwest's whole-attempt timeout, and the quiet interval
/// becomes its per-read timeout. An unbounded dimension configures no timer at
/// all rather than a very long one.
///
/// Reqwest's own retry policy is turned off. Its default resends a request
/// after a low-level protocol NACK, which would put a second sender of the same
/// request beside this module's coordinator: an unsafe request refused a replay
/// here could still be replayed there, under a budget no Camber setting names.
/// Redirect handling is untouched.
fn build_client(connect_timeout: Duration, response: TransferBudget) -> BuiltClient {
    let mut builder = reqwest::Client::builder()
        .connect_timeout(connect_timeout)
        .retry(reqwest::retry::never());
    if let Some(total) = response.total() {
        builder = builder.timeout(total);
    }
    if let Some(idle) = response.idle() {
        builder = builder.read_timeout(idle);
    }
    builder.build().map_err(|e| e.to_string().into())
}

/// Create a client builder with custom timeout, retry, and response
/// configuration.
pub fn client() -> ClientBuilder {
    ClientBuilder {
        connect_timeout: DEFAULT_TIMEOUT,
        response: DEFAULT_RESPONSE_POLICY,
        retries: 0,
        backoff: DEFAULT_BACKOFF,
        retry_timeout: DEFAULT_RETRY_TIMEOUT,
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
            .field("retry_timeout", &self.retry_timeout)
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
/// | `retry_timeout` | 30 seconds |
///
/// `request_timeout`, `response_idle_timeout`, and the response maximum are
/// one stored [`TransferBudget`], which
/// [`response_budget`](Self::response_budget) replaces whole and the named
/// setters write one field of. `retry_timeout` is not part of it. Call order
/// is authoritative: the last write to a field is the one the client is built
/// from.
///
/// A retry is one more attempt under the same policy. Retry eligibility, count,
/// backoff, and the unsafe-method opt-in are unaffected by any of it; the
/// request-total deadline bounds each attempt rather than the sequence.
/// [`retry_timeout`](Self::retry_timeout) is the one bound on the whole
/// sequence of attempts and the delays between them.
///
/// Every replay decision belongs to this client. The underlying Reqwest client
/// is built with its own retry policy disabled, so no second sender resends a
/// request this one refused to repeat.
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
    retry_timeout: Duration,
    retry_unsafe_methods: bool,
    cached_client: OnceLock<BuiltClient>,
}

impl ClientBuilder {
    /// Set the connect timeout. Minimum: 1ms. Zero values are clamped.
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout =
            crate::time::clamp_duration(timeout, MIN_SETTER_DURATION, "connect_timeout");
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
                .with_clamped_total(timeout, MIN_SETTER_DURATION, "request_timeout");
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
        self.response =
            self.response
                .with_clamped_idle(timeout, MIN_SETTER_DURATION, "response_idle_timeout");
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
    /// Transient failures are request transport errors, response-header
    /// timeouts, and 429, 502, 503, or 504 responses. By default, configured
    /// retries apply only to GET, HEAD, and OPTIONS requests. See
    /// [`retry_unsafe_methods`](Self::retry_unsafe_methods) for what the opt-in
    /// does and does not authorize, and
    /// [`retry_timeout`](Self::retry_timeout) for the bound on the whole
    /// sequence.
    pub fn retries(mut self, n: u32) -> Self {
        self.retries = n;
        self
    }

    /// Allow configured retries for POST, PUT, PATCH, and DELETE requests.
    ///
    /// By default, only GET, HEAD, and OPTIONS requests are retried. Repeating
    /// one of those cannot duplicate server-visible work; repeating a POST,
    /// PUT, PATCH, or DELETE can, so the opt-in authorizes a replay only where
    /// there is evidence the first attempt did no work:
    ///
    /// - **A transient response authorizes a replay.** A 429, 502, 503, or 504
    ///   is the server's own statement that it did not act on the request, so
    ///   the request is sent again. Its body is sent again with it.
    /// - **An ambiguous transport failure does not.** Once a connection is
    ///   established, a failed send proves nothing: request headers or a body
    ///   prefix may already have reached the peer, and nothing on this side
    ///   distinguishes a peer that ignored them from one that acted on them.
    ///   The call returns that transport error rather than writing twice.
    /// - **A connect-stage failure does.** A failure while establishing the
    ///   transport means no server saw the request, so the attempt is repeated.
    ///
    /// A safe method is unaffected by this setting: any transient transport
    /// failure retries it.
    pub fn retry_unsafe_methods(mut self, enabled: bool) -> Self {
        self.retry_unsafe_methods = enabled;
        self
    }

    /// Set the base backoff duration between retries.
    /// Actual delay: `base * 2^attempt + jitter`, with jitter below `base` and
    /// every step saturating. Minimum: 1ms. Zero values are clamped.
    ///
    /// A transient response's valid `Retry-After` replaces this delay for that
    /// retry. Delta-seconds waits that many seconds. An HTTP-date waits until
    /// that date, and a date now or in the past retries at once. An invalid
    /// value is ignored, and this backoff applies. Every delay is clipped to
    /// the [`retry_timeout`](Self::retry_timeout) deadline.
    pub fn backoff(mut self, duration: Duration) -> Self {
        self.backoff = crate::time::clamp_duration(duration, MIN_SETTER_DURATION, "backoff");
        self
    }

    /// Set the bound on a whole retry sequence. Default: 30 seconds.
    /// Minimum: 1ms. Zero values are clamped.
    ///
    /// The deadline is fixed once, when a call begins, and no attempt or delay
    /// resets it. It encloses sending each attempt, waiting for each response
    /// head, every delay between attempts, and collecting the final response
    /// body. When it expires, the attempt in flight is dropped and the call
    /// returns `RuntimeError::DeadlineExceeded(DeadlineBoundary::ClientRetry)`
    /// without starting another attempt. A delay that would end past it ends
    /// at it instead.
    ///
    /// Runtime shutdown ends the same sequence: a pending attempt or delay
    /// returns `RuntimeError::Cancelled`, and no further attempt starts.
    ///
    /// Each attempt keeps its own boundaries inside it. A connect, request, or
    /// response-idle timeout that expires first returns its own result. Before
    /// the response head arrives, that result is still eligible for another
    /// attempt; a failure while the final response body is collected is not.
    ///
    /// With zero configured [`retries`](Self::retries) there is no sequence:
    /// this value is ignored, and the attempt's own boundaries are the whole
    /// lifetime of the call.
    #[must_use]
    pub fn retry_timeout(mut self, timeout: Duration) -> Self {
        self.retry_timeout = clamped_duration(timeout, MIN_SETTER_DURATION, "retry_timeout");
        self
    }

    fn get_client(&self) -> Result<&reqwest::Client, RuntimeError> {
        cached_client(
            self.cached_client
                .get_or_init(|| build_client(self.connect_timeout, self.response)),
        )
    }

    async fn send(
        &self,
        method: Method,
        url: &str,
        body: Option<(HeaderValue, &str)>,
    ) -> Result<Response, RuntimeError> {
        let retry = RetryPlan::configured(
            self.retries,
            self.backoff,
            self.retry_timeout,
            self.retry_unsafe_methods,
        );
        Dispatch::new(self.get_client()?, retry, self.response.max_bytes())
            .run(method, url, body)
            .await
    }

    with_http_verbs!(http_methods);
}

/// Async dispatch using the shared default client (no retries).
async fn default_dispatch(
    method: Method,
    url: &str,
    body: Option<(HeaderValue, &str)>,
) -> Result<Response, RuntimeError> {
    Dispatch::new(
        cached_client(&CLIENT)?,
        None,
        DEFAULT_RESPONSE_POLICY.max_bytes(),
    )
    .run(method, url, body)
    .await
}

with_http_verbs!(http_free_functions);
