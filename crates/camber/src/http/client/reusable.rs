//! The reusable client: its configuration, its defaults, and the verb
//! conveniences on it and on the shared default client.

use super::super::policy_value::clamped_duration;
use super::super::transfer_budget::TransferBudget;
use super::dns_edge::ClientDnsEdge;
use super::egress::{EgressPolicy, egress_refused};
use super::policy::OperationPolicy;
use super::projection::client_error_projection;
use super::redirect::RedirectPolicy;
use super::replay::RetryPolicy;
use super::request::ClientRequest;
use super::response::ClientResponse;
use super::transport::{
    BuiltClient, Dialer, Profile, TransportCache, TransportSettings, build_client,
};
use crate::RuntimeError;
use reqwest::header::HeaderValue;
use reqwest::{Method, Url};
use std::sync::{Arc, LazyLock};
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
/// A client builds one transport for each terminal on first use and discards
/// both when a transport setting is written. This is `true` while either is
/// built, so it is `false` before the first call and after such a write.
///
/// Semver-unsupported. It reads the client and changes nothing.
#[doc(hidden)]
pub fn client_transport_is_built(client: &Client) -> bool {
    client.cached.is_built()
}

/// Give `client` the lookup answers of `edge` in place of the system
/// resolver's.
///
/// The edge answers only the lookups of a client whose policy restricts
/// addresses: no other client resolves a name itself. Every address it returns
/// is read by the production visit bound and the production policy decision,
/// and is dialed through the production pinned transport.
///
/// Semver-unsupported. It supplies an input and no verdict.
#[doc(hidden)]
#[must_use]
pub fn client_dns_edge(mut client: Client, edge: &ClientDnsEdge) -> Client {
    client.dns_edge = Some(edge.clone());
    client
}

/// Add one local trust root, in PEM, to every transport `client` builds.
///
/// The root is merged with the roots the transport already trusts, through the
/// factory every transport is built by. It adds trust and removes no check:
/// the certificate chain, its validity, and the requested name are still
/// verified. A second call replaces the first root. A root the TLS backend
/// cannot read fails the build of the transport, and each call through the
/// client then returns `ClientError::Configuration`.
///
/// Semver-unsupported.
#[doc(hidden)]
pub fn client_test_root(mut client: Client, pem: &[u8]) -> Result<Client, RuntimeError> {
    let root = reqwest::Certificate::from_pem(pem)
        .map_err(|error| RuntimeError::Client(client_error_projection(&error)))?;
    client.test_root = Some(Arc::new(root));
    client.discard_transport();
    Ok(client)
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
        egress: EgressPolicy::unrestricted(),
        dns_edge: None,
        test_root: None,
        cached: TransportCache::default(),
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
            .field("egress", &self.egress)
            .finish()
    }
}

/// A reusable outbound HTTP client: its deadlines, its retries, and the
/// maximum one response may retain.
///
/// [`request`](Self::request) describes one request with any method, its own
/// headers, and its own response maximum. The verb methods are conveniences
/// over the same description. Requests share this client's egress policy and
/// its connection pools, and nothing else: one request's headers, body, and
/// limits never reach another. Buffered calls share one pool and streamed
/// calls another. A call that [`EgressPolicy::public_internet`] restricts
/// uses neither. It builds a transport of its own and closes it when the call
/// ends.
///
/// The four deadlines are independent boundaries, not one shared timeout:
///
/// - `total_timeout` covers one whole operation: every attempt, every followed
///   redirect, every delay between attempts, and the final response body;
/// - `request_timeout` covers one attempt, from its send through the end of
///   its response body;
/// - `connect_timeout` ends when the transport is established;
/// - `response_idle_timeout` covers each wait for response bytes. For a
///   streamed response it covers one pending read of the body.
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
/// `ClientTimeout::Attempt`. The transport owns the connect timer and the
/// quiet-interval timer of a buffered call, and either one returns
/// `ClientTimeout::Transport`. Camber owns the idle interval of a streamed
/// body, and it returns `ClientTimeout::ResponseIdle`.
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
/// [`ClientRequest::send_streaming`] returns the response head and reads the
/// body one chunk at a time. A stream needs a finite response maximum and a
/// finite idle interval. Buffered and streamed calls use separate connection
/// pools.
///
/// Each started call records one `http.client.completed` tracing event: the
/// method label, the host the call began at, the final status, the elapsed
/// milliseconds, the sends, the followed redirects, and a closed outcome. The
/// event holds no path, query, header value, or body. With the `otel` feature,
/// a call inside the tracing middleware sends that request's `traceparent`.
/// Use [`ClientRequest::trace_context`] to withhold it from one request.
///
/// By default Camber does not check the address a host resolves to, so a
/// default client reaches private services. Send only to URLs you trust, or
/// give the client an [`EgressPolicy`] with
/// [`egress_policy`](Self::egress_policy) before it fetches a URL you do not.
///
/// The underlying `reqwest::Client` is built lazily on first request
/// and cached for subsequent calls: one for buffered calls and one for
/// streamed calls. A setter that changes a transport setting discards both,
/// so the next request builds one from the current values.
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
    /// Which destinations this client sends to. Immutable once set: no
    /// request reads a wider one.
    egress: EgressPolicy,
    /// The lookup answers a test chose, in place of the system resolver's.
    dns_edge: Option<ClientDnsEdge>,
    /// The one local trust root a test added to every transport. Shared, so
    /// an operation's settings copy no certificate.
    test_root: Option<Arc<reqwest::Certificate>>,
    /// The reusable transports, one for each terminal. Only a call whose
    /// policy pins no address sends through one.
    cached: TransportCache,
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
    /// stopped sending without bounding a large body that keeps arriving. For
    /// a buffered call the transport owns this timer, and a wait past it
    /// returns `ClientTimeout::Transport`.
    ///
    /// For a streamed call Camber owns it. It measures one pending
    /// [`ClientBody::next`](crate::http::ClientBody::next) and returns
    /// `ClientTimeout::ResponseIdle`. Time the caller spends between reads is
    /// not idle time.
    ///
    /// It writes the quiet-interval dimension of this client's one response
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
    ///
    /// A budget with no maximum or no quiet interval still serves buffered
    /// calls. [`ClientRequest::send_streaming`] refuses it with
    /// `ClientInput::StreamBudget`.
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
    ///
    /// A streamed call does not run without a maximum. Give that request one
    /// with [`ClientRequest::response_limit`].
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
    /// checks the address a permitted host resolves to only under an
    /// [`egress_policy`](Self::egress_policy) that restricts addresses.
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

    /// Set the destinations this client may send to. Default:
    /// [`EgressPolicy::unrestricted`].
    ///
    /// The policy governs every call through this client: the verb
    /// conveniences and every described request, buffered or streamed. No
    /// request setter widens it.
    /// A refused call returns `ClientError::Egress`, sends nothing, and is
    /// never retried.
    ///
    /// Under [`EgressPolicy::https_only`], an HTTP target is refused before
    /// its host is resolved. A policy that restricts only the scheme
    /// restricts no address, and its calls keep this client's reusable
    /// connection pool.
    ///
    /// Under [`EgressPolicy::public_internet`], each call resolves its host
    /// once, through the system resolver, under the call's own deadline. An
    /// address literal is checked as its own answer. An answer with more than
    /// 64 addresses returns `ClientEgress::ResolutionLimit`. One refused
    /// address refuses the whole answer with `ClientEgress::Address`, and an
    /// empty or failed lookup returns `ClientError::Dns`. The call then dials
    /// only the checked addresses. Retries and same-origin hops reuse that
    /// pin. Each origin change requires fresh admission, including return hops. The
    /// request keeps its host name, so the `Host` header, the TLS server
    /// name, and certificate verification read the name and not the address.
    ///
    /// A call under such a policy uses no proxy, from the environment or from
    /// the system, and builds a transport of its own. Its connections are
    /// closed when the call ends, and the next call resolves, checks, and
    /// connects again. A restricted client therefore pays a lookup, a
    /// connect, and a TLS handshake for each call.
    ///
    /// Setting a policy discards the transport this client built, so no
    /// connection pooled under an earlier policy serves a later call.
    #[must_use]
    pub fn egress_policy(mut self, policy: EgressPolicy) -> Self {
        self.egress = policy;
        self.discard_transport();
        self
    }

    /// Describe one request with any HTTP method.
    ///
    /// Nothing is sent. The method and the target are validated here, and the
    /// returned [`ClientRequest`] takes headers, a body, a response maximum,
    /// its redirect and retry policies, and a deadline before
    /// [`send`](ClientRequest::send) or
    /// [`send_streaming`](ClientRequest::send_streaming) runs it.
    ///
    /// `method` is any HTTP token, in the case it is written: `"PURGE"` is a
    /// method, and `"get"` is not `"GET"`. CONNECT is refused, because this
    /// client returns a response and not a tunnel. An invalid method
    /// returns `ClientInput::Method`.
    ///
    /// `url` is an absolute `http` or `https` URL with a host and without a
    /// user name or password. Its fragment is removed. Any other target
    /// returns `ClientInput::Url`.
    pub fn request(&self, method: &str, url: &str) -> Result<ClientRequest<'_>, RuntimeError> {
        ClientRequest::parsed(self, method, url)
    }

    /// The reusable transport of `profile`, built on first use.
    fn transport(&self, profile: Profile) -> Result<&reqwest::Client, RuntimeError> {
        cached_client(self.cached.get_or_build(profile, || {
            build_client(&self.transport_settings(profile), None)
        }))
    }

    /// The settings a transport of `profile` is built from.
    ///
    /// Only the buffered profile arms the quiet interval on its transport.
    fn transport_settings(&self, profile: Profile) -> TransportSettings {
        TransportSettings {
            connect_timeout: self.connect_timeout,
            idle: match profile {
                Profile::Buffered => self.response.idle(),
                Profile::Streaming => None,
            },
            test_root: self.test_root.clone(),
        }
    }

    /// Refuse a target whose scheme this client's policy does not send to.
    ///
    /// No address is read, so nothing is resolved.
    pub(super) fn admit_scheme(&self, target: &Url) -> Result<(), RuntimeError> {
        self.egress.admit_scheme(target).map_err(egress_refused)
    }

    /// How one operation of `profile` admits and reaches its destinations.
    ///
    /// The dialer borrows this client's policy. A policy that pins no address
    /// shares the reusable transport of `profile` and its pool. Any other
    /// builds a checked transport for each origin under the timer profile and
    /// TLS trust fixed here. Nothing is resolved here.
    fn dialer(&self, profile: Profile) -> Result<Dialer<'_>, RuntimeError> {
        match self.egress.pins_addresses() {
            false => self
                .transport(profile)
                .map(|shared| Dialer::reusable(&self.egress, shared)),
            true => Ok(Dialer::pinned(
                &self.egress,
                self.dns_edge.clone(),
                self.transport_settings(profile),
            )),
        }
    }

    /// The dialer of one buffered operation, which borrows this client.
    pub(super) fn operation_dialer(&self) -> Result<Dialer<'_>, RuntimeError> {
        self.dialer(Profile::Buffered)
    }

    /// The dialer of one streamed operation.
    ///
    /// The operation's task outlives this borrow, so the dialer holds shared
    /// handles to the policy, the reusable transport, and the trust root.
    pub(super) fn stream_dialer(&self) -> Result<Dialer<'static>, RuntimeError> {
        self.dialer(Profile::Streaming).map(Dialer::into_owned)
    }

    /// Forget both built transports after a transport setting changed.
    ///
    /// The next request of each terminal builds one from the current
    /// settings. A client that kept an old transport would enforce the timers
    /// it was built with.
    fn discard_transport(&mut self) {
        self.cached = TransportCache::default();
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
            idle: self.response.idle(),
            retries: self.retries,
            backoff: self.backoff,
            permission: RetryPolicy::default(),
            redirect: self.redirect.clone(),
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
