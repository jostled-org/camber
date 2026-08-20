//! One typed boundary for every Camber-controlled HTTP refusal.
//!
//! A failure found during routing, admission, parsing, middleware, protocol
//! validation, proxying, or response construction becomes a [`Rejected`]
//! value: a client-safe [`Rejection`], the private cause an operator needs,
//! and the identity that names the request. Exactly one boundary turns that
//! value into a response, so application policy runs once and internal causes
//! never leave through it.
//!
//! The public half — [`RequestId`], [`RejectionKind`], [`Rejection`],
//! [`RejectionContext`], [`RejectionProtocol`], and
//! [`NegotiatedResponseMetadata`] — is re-exported from [`super`]. Everything
//! below that is private to the crate.

use super::async_proxy::ProxyFailure;
use super::boundary::{ByteBoundary, CrossedBound, DeadlineBoundary};
use super::method::RequestMethod;
use super::request::Request;
use super::response::{
    HeaderPair, Response, ResponseProvenance, UnrepresentableResponse, validate_status,
};
use crate::RuntimeError;
use arrayvec::ArrayString;
use bytes::Bytes;
use http_body_util::Full;
use std::borrow::Cow;
use std::error::Error;
use std::fmt;
use std::net::IpAddr;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};

// ── Request identity ───────────────────────────────────────────────

/// How many hexadecimal digits one identifier renders as.
const REQUEST_ID_DIGITS: usize = 32;

/// The digits one nibble renders as, lowercase.
const HEX_DIGITS: [char; 16] = [
    '0', '1', '2', '3', '4', '5', '6', '7', '8', '9', 'a', 'b', 'c', 'd', 'e', 'f',
];

/// The nonce every identifier minted by this process is built from.
///
/// One draw, so two processes writing to the same log cannot mint the same
/// identifier for two different requests while the counter alone stays cheap.
static PROCESS_NONCE: LazyLock<u64> = LazyLock::new(crate::prng::next_u64);

/// The monotonic counter that separates two identifiers in this process.
static REQUEST_COUNTER: AtomicU64 = AtomicU64::new(0);

/// The header a built-in rejection response names its request under.
const REQUEST_ID_HEADER: &str = "X-Request-Id";

/// The body every redacted internal failure answers with, exactly.
const INTERNAL_ERROR_BODY: &str = "internal server error";

/// The status the one fixed fallback answers with.
///
/// Held as the Hyper value, because that is what the response the peer is given
/// carries. The record an operator correlates against it reads the same value
/// through [`fallback_status`], so the two cannot be spelled apart.
const FALLBACK_STATUS_CODE: hyper::StatusCode = hyper::StatusCode::INTERNAL_SERVER_ERROR;

/// The fallback status as the number a record and a Camber response state it by.
fn fallback_status() -> u16 {
    FALLBACK_STATUS_CODE.as_u16()
}

/// The body a refusal that is about service state answers with.
const SERVICE_UNAVAILABLE_BODY: &str = "service unavailable";

/// The body a request whose payload could not be parsed answers with.
const UNPARSEABLE_BODY: &str = "malformed request body";

/// The body a request whose multipart payload could not be parsed answers with.
const INVALID_MULTIPART_BODY: &str = "invalid multipart body";

/// The body a request larger than its effective limit answers with.
///
/// One spelling for both producers: the buffered collector's own refusal and the
/// crossing a streaming consumer reports as a typed error are the same answer to
/// the peer, so a second literal would be a second thing it could be told.
const BODY_TOO_LARGE_BODY: &str = "request body too large";

/// The body a request Camber could not read answers with, for the same reason.
const BODY_UNREADABLE_BODY: &str = "request body could not be read";

/// The header a method refusal names its allowed set under.
const ALLOW_HEADER: &str = "Allow";

/// The header an unsupported-version handshake refusal advertises through.
#[cfg(feature = "ws")]
const WS_VERSION_HEADER: &str = "Sec-WebSocket-Version";

/// The only WebSocket version Camber speaks.
#[cfg(feature = "ws")]
const WS_VERSION: &str = "13";

/// The body a proxied request answers with when no upstream could be reached.
const BAD_GATEWAY_BODY: &str = "bad gateway";

/// The body a proxied request answers with when its upstream ran out of time.
const GATEWAY_TIMEOUT_BODY: &str = "gateway timeout";

/// The header an HTTP/1 transport disposition is stated through.
const CONNECTION_HEADER: &str = "Connection";

/// The `Connection` value that ends an HTTP/1 transport with its answer.
const CLOSE_DISPOSITION: &str = "close";

/// The header pair that ends an HTTP/1 transport with the answer carrying it.
///
/// The disposition as a value, for the one stage that appends it: the streaming
/// commitment that carries an upstream head out over payload a peer still owed.
/// The mapper's own rule below states the same disposition by replacing the
/// header by name instead, so it takes the two constants directly. Both read
/// those same constants, and neither can tell a peer what the other would not.
pub(super) fn close_header() -> HeaderPair {
    (
        Cow::Borrowed(CONNECTION_HEADER),
        Cow::Borrowed(CLOSE_DISPOSITION),
    )
}

/// The headers HTTP/2 forbids on a response.
///
/// A mapper writing any of these is describing an HTTP/1 connection it does not
/// own. Removing them is the h2 half of the same disposition rule that
/// overwrites `Connection` on HTTP/1.
const CONNECTION_SPECIFIC_HEADERS: [&str; 5] = [
    CONNECTION_HEADER,
    "Keep-Alive",
    "Proxy-Connection",
    "Transfer-Encoding",
    "Upgrade",
];

/// An opaque identifier Camber mints for one accepted request head.
///
/// Built from the process nonce and a monotonic counter, rendered once into
/// fixed inline storage. It is never read from an inbound header: a peer that
/// sends `X-Request-Id` is sending application data, not Camber authority.
#[derive(Clone, Copy, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct RequestId {
    /// The rendered digits, UTF-8 by construction rather than by decode.
    digits: ArrayString<REQUEST_ID_DIGITS>,
}

impl RequestId {
    /// Mint the identifier for one accepted request head.
    pub(super) fn generate() -> Self {
        let nonce = u128::from(*PROCESS_NONCE) << 64;
        let counter = u128::from(REQUEST_COUNTER.fetch_add(1, Ordering::Relaxed));
        Self {
            digits: render_hex(nonce | counter),
        }
    }

    /// Borrow the 32 lowercase hexadecimal digits this identifier renders as.
    pub fn as_str(&self) -> &str {
        self.digits.as_str()
    }
}

impl fmt::Display for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl fmt::Debug for RequestId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("RequestId").field(&self.as_str()).finish()
    }
}

/// The digit count is the value's width and the storage's capacity at once.
///
/// Stated to the compiler rather than to the reader: a wider count would shift
/// past the end of a `u128`, and either direction would push a digit the
/// storage has no room for. Hold this and the loop below cannot overflow.
const _: () = assert!(REQUEST_ID_DIGITS * 4 == u128::BITS as usize);

/// Render a 128-bit value as its fixed-width lowercase hexadecimal digits.
fn render_hex(value: u128) -> ArrayString<REQUEST_ID_DIGITS> {
    let mut digits = ArrayString::new();
    for position in (0..REQUEST_ID_DIGITS).rev() {
        let nibble = (value >> (position * 4)) & 0xf;
        digits.push(HEX_DIGITS[nibble as usize]);
    }
    digits
}

// ── Public taxonomy ────────────────────────────────────────────────

/// The category of a Camber-controlled HTTP refusal.
///
/// Fixed at the first boundary that still knows which producer found the
/// failure. No later layer infers a category from a status code or an error
/// string, so two categories a mapper gives the same status remain distinct.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum RejectionKind {
    /// No route or host claims the request target.
    Routing,
    /// A route claims the path, but not with the received method.
    MethodSelection,
    /// The request body exceeds the effective limit.
    BodyLimit,
    /// Body admission refused the work.
    ///
    /// Raised by a route's configured body-admission policy, before a payload
    /// frame is polled. A body Camber failed to read is
    /// [`RejectionKind::BodyUnreadable`]: a transport account of a request
    /// nothing refused, not a refusal.
    BodyAdmission,
    /// The body stopped arriving for a reason that is not the limit.
    BodyUnreadable,
    /// The body collection deadline expired.
    BodyTimeout,
    /// The admitted request outlived its total deadline before a response head.
    ///
    /// Held apart from [`RejectionKind::BodyTimeout`] because the two name
    /// different configured bounds: a body that stopped arriving is the peer's
    /// upload, while a request that ran out of total time may have had no body
    /// at all. Answering both as one told an operator to widen the wrong
    /// deadline.
    RequestTimeout,
    /// A request body could not be parsed as its declared representation.
    MalformedBody,
    /// A `multipart/form-data` body could not be parsed.
    Multipart,
    /// A response cannot represent one of its headers or its status.
    InvalidHeader,
    /// The application declared the failure through its handler.
    Application,
    /// The application declared the failure through middleware.
    Middleware,
    /// A WebSocket handshake failed before `101`.
    WebSocketHandshake,
    /// A proxied request failed before an upstream response head.
    Proxy,
    /// Camber itself could not complete the request.
    InternalService,
}

impl RejectionKind {
    /// Every category the taxonomy admits, in declaration order.
    ///
    /// A test seam, not API. Hidden from the documentation and outside the
    /// semver promise this crate makes about its public surface, on the same
    /// footing as `runtime_test_support`. Reach it from a test root and
    /// nowhere else.
    ///
    /// It exists because Rust cannot enumerate variants and the integration
    /// suite has to. The suite kept its own copy of this list, a copy nothing
    /// tied to the enum: a fifteenth variant compiled clean against fourteen
    /// entries, and every assertion built on them went on passing over a
    /// vocabulary that no longer covered the taxonomy. Declared here, the list
    /// sits against the variants it names, so adding one is a single edit in a
    /// single place.
    #[doc(hidden)]
    pub const ALL: [Self; 15] = [
        Self::Routing,
        Self::MethodSelection,
        Self::BodyLimit,
        Self::BodyAdmission,
        Self::BodyUnreadable,
        Self::BodyTimeout,
        Self::RequestTimeout,
        Self::MalformedBody,
        Self::Multipart,
        Self::InvalidHeader,
        Self::Application,
        Self::Middleware,
        Self::WebSocketHandshake,
        Self::Proxy,
        Self::InternalService,
    ];

    /// The bounded name this category is recorded and counted under.
    ///
    /// A closed vocabulary of static text: an operator's counter is labelled
    /// with it, so it cannot become a value derived from a request.
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Routing => "routing",
            Self::MethodSelection => "method_selection",
            Self::BodyLimit => "body_limit",
            Self::BodyAdmission => "body_admission",
            Self::BodyUnreadable => "body_unreadable",
            Self::BodyTimeout => "body_timeout",
            Self::RequestTimeout => "request_timeout",
            Self::MalformedBody => "malformed_body",
            Self::Multipart => "multipart",
            Self::InvalidHeader => "invalid_header",
            Self::Application => "application",
            Self::Middleware => "middleware",
            Self::WebSocketHandshake => "websocket_handshake",
            Self::Proxy => "proxy",
            Self::InternalService => "internal_service",
        }
    }
}

/// The client-safe projection of one refusal.
///
/// Owns its category, default status, safe message, and safe default headers,
/// and nothing else: a mapper reading this value cannot reach a diagnostic, a
/// source error, a panic payload, an upstream address, or a filesystem path.
///
/// Status is data rather than a function of category. One handshake category
/// produces `400`, `403`, or `426`; one internal-service category produces a
/// redacted `500` or an availability `503`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Rejection {
    kind: RejectionKind,
    status: u16,
    message: Cow<'static, str>,
    headers: Box<[HeaderPair]>,
}

impl Rejection {
    /// Build a rejection with a validated status.
    ///
    /// Returns `RuntimeError::InvalidArgument` when `status` is outside
    /// 100–599. Applications use this to exercise their own rejection mapper
    /// without booting a server.
    pub fn new(
        kind: RejectionKind,
        status: u16,
        message: impl Into<Cow<'static, str>>,
    ) -> Result<Self, RuntimeError> {
        validate_status(status)?;
        Ok(Self::raw(kind, status, message))
    }

    /// Build a rejection from a status Camber already knows is valid.
    pub(super) fn raw(
        kind: RejectionKind,
        status: u16,
        message: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            kind,
            status,
            message: message.into(),
            headers: Box::new([]),
        }
    }

    /// What a peer is told when the service cannot take the work.
    ///
    /// Four categories answer this way: a closed scope, a policy that declined
    /// the work, an upgrade no supervisor would own, and a route with no
    /// admissible backend. They differ by category alone, so the status and the
    /// safe body are settled here — a second spelling of the pair is a second
    /// place it can drift.
    fn service_unavailable(kind: RejectionKind) -> Self {
        Self::raw(kind, 503, SERVICE_UNAVAILABLE_BODY)
    }

    /// What a peer is told about a fault nothing safe can be said of.
    ///
    /// Settled here for the reason [`Self::service_unavailable`] is: every
    /// redacting producer answers with one status and one body, so a peer that
    /// could tell two of them apart would be reading what was redacted.
    fn redacted_fault(kind: RejectionKind) -> Self {
        Self::raw(kind, 500, INTERNAL_ERROR_BODY)
    }

    /// What a peer is told when its body outran the limit it was admitted under.
    ///
    /// Settled here for the reason [`Self::service_unavailable`] is, one step
    /// further: the buffered collector and the streaming consumer answer the
    /// same condition, so the category and the status travel with the safe body
    /// rather than beside it. A second spelling of the triple is a place where
    /// one of the two producers could start answering under a status the other
    /// does not, reported under the same metric label.
    fn body_limit() -> Self {
        Self::raw(RejectionKind::BodyLimit, 413, BODY_TOO_LARGE_BODY)
    }

    /// What a peer is told when Camber could not read the body it sent.
    ///
    /// Settled here for the same reason [`Self::body_limit`] is: a mid-body
    /// transport failure and a streaming consumer's stalled read are one answer
    /// under one category.
    fn body_unreadable() -> Self {
        Self::raw(RejectionKind::BodyUnreadable, 400, BODY_UNREADABLE_BODY)
    }

    /// Add one safe default header, keeping registration order.
    #[must_use]
    pub fn with_header(self, name: &str, value: &str) -> Self {
        self.pushing((Cow::Owned(name.to_owned()), Cow::Owned(value.to_owned())))
    }

    /// Add one safe default header whose name Camber spells at compile time.
    ///
    /// The name is borrowed, never copied. `HeaderPair` is a pair of `Cow`s so a
    /// header the framework already owns as a constant costs no heap at all;
    /// copying `Allow` or `Sec-WebSocket-Version` into an allocation per refusal
    /// is exactly what that shape exists to avoid. The value stays a `Cow`
    /// because some of these are constants too and some are settled per route.
    #[must_use]
    pub(super) fn with_static_header(
        self,
        name: &'static str,
        value: impl Into<Cow<'static, str>>,
    ) -> Self {
        self.pushing((Cow::Borrowed(name), value.into()))
    }

    /// Append one header, keeping registration order.
    ///
    /// The one place the boxed slice is reshaped, so what a header costs is
    /// decided by the caller that knows whether its halves are constants.
    fn pushing(self, header: HeaderPair) -> Self {
        let mut headers = self.headers.into_vec();
        headers.push(header);
        Self {
            headers: headers.into_boxed_slice(),
            ..self
        }
    }

    /// The category this refusal was classified as.
    pub fn kind(&self) -> RejectionKind {
        self.kind
    }

    /// The status a mapper is offered as the default answer.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The client-safe message for this refusal.
    pub fn message(&self) -> &str {
        &self.message
    }

    /// Iterate the safe default headers, in registration order.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.headers
            .iter()
            .map(|(name, value)| (name.as_ref(), value.as_ref()))
    }

    /// Borrow the safe default headers as the pairs they are stored as.
    ///
    /// The built-in mapper reads them through here rather than through
    /// [`Self::headers`]: it is putting them straight onto a response whose
    /// header list is the same `Cow` pair, so a `&str` view would force it to
    /// re-copy every constant name and every settled value it was handed.
    pub(super) fn header_pairs(&self) -> &[HeaderPair] {
        &self.headers
    }
}

/// The name a head that established no dispatch class is recorded under.
const UNCLASSIFIED_PROTOCOL: &str = "unclassified";

/// Which dispatch class had been selected when a refusal was found.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub enum RejectionProtocol {
    /// A buffered HTTP handler.
    OrdinaryHttp,
    /// A streaming HTTP response.
    StreamingHttp,
    /// A server-sent events stream.
    ServerSentEvents,
    /// A WebSocket upgrade.
    WebSocket,
    /// A gRPC call.
    Grpc,
    /// A proxied request.
    Proxy,
}

impl RejectionProtocol {
    /// Every dispatch class this enum admits, in declaration order.
    ///
    /// Named exhaustively here, against the variants it lists, for the reason
    /// [`RejectionKind::ALL`] exists: a seventh class forces an edit to the
    /// exhaustive [`Self::label`] match, and a list kept anywhere else compiles
    /// clean at six while every assertion built on it goes on passing over a
    /// vocabulary that no longer covers the labels production writes.
    pub(super) const ALL: [Self; 6] = [
        Self::OrdinaryHttp,
        Self::StreamingHttp,
        Self::ServerSentEvents,
        Self::WebSocket,
        Self::Grpc,
        Self::Proxy,
    ];

    /// Every dispatch class, plus the name a head that established none is
    /// recorded under.
    ///
    /// Published from the enum rather than transcribed beside it, so a class
    /// added here reaches the vocabulary a completion label is checked against.
    pub(super) fn vocabulary() -> Box<[&'static str]> {
        Self::ALL
            .map(Self::label)
            .into_iter()
            .chain([UNCLASSIFIED_PROTOCOL])
            .collect()
    }

    /// The bounded name this dispatch class is recorded under.
    fn label(self) -> &'static str {
        match self {
            Self::OrdinaryHttp => "ordinary_http",
            Self::StreamingHttp => "streaming_http",
            Self::ServerSentEvents => "server_sent_events",
            Self::WebSocket => "websocket",
            Self::Grpc => "grpc",
            Self::Proxy => "proxy",
        }
    }
}

/// What a selected protocol had already negotiated when a refusal was found.
///
/// Present only once an owner established it. Absence is an `Option`, never an
/// empty sentinel.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct NegotiatedResponseMetadata {
    protocol: RejectionProtocol,
    content_type: Option<Box<str>>,
    subprotocol: Option<Box<str>>,
}

impl NegotiatedResponseMetadata {
    /// Name the selected protocol, with nothing else negotiated yet.
    pub fn new(protocol: RejectionProtocol) -> Self {
        Self {
            protocol,
            content_type: None,
            subprotocol: None,
        }
    }

    /// Record a content type that validated before the failure.
    #[must_use]
    pub fn with_content_type(self, content_type: &str) -> Self {
        Self {
            content_type: Some(content_type.into()),
            ..self
        }
    }

    /// Record a WebSocket subprotocol that negotiation selected.
    #[must_use]
    pub fn with_subprotocol(self, subprotocol: &str) -> Self {
        Self {
            subprotocol: Some(subprotocol.into()),
            ..self
        }
    }

    /// The dispatch class this request had selected.
    pub fn protocol(&self) -> RejectionProtocol {
        self.protocol
    }

    /// The validated content type, when one was established.
    pub fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The selected WebSocket subprotocol, when negotiation established one.
    pub fn subprotocol(&self) -> Option<&str> {
        self.subprotocol.as_deref()
    }
}

/// The owned request snapshot a mapper reads.
///
/// Materialized only when a rejection is converted, so the success path pays
/// nothing for it. Method, raw path, and request identity are present for
/// every request Hyper hands to Camber; every other value is present exactly
/// when its owner had established it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RejectionContext {
    request_id: RequestId,
    method: Cow<'static, str>,
    raw_path: Cow<'static, str>,
    remote_addr: Option<IpAddr>,
    route: Option<Box<str>>,
    negotiated: Option<NegotiatedResponseMetadata>,
}

impl RejectionContext {
    /// Build the context every mapped request has.
    pub fn new(
        request_id: RequestId,
        method: impl Into<Cow<'static, str>>,
        raw_path: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            request_id,
            method: method.into(),
            raw_path: raw_path.into(),
            remote_addr: None,
            route: None,
            negotiated: None,
        }
    }

    /// Record the peer address the transport reported.
    #[must_use]
    pub fn with_remote_addr(self, remote_addr: IpAddr) -> Self {
        Self {
            remote_addr: Some(remote_addr),
            ..self
        }
    }

    /// Record the registered route pattern path matching established.
    #[must_use]
    pub fn with_route(self, route: &str) -> Self {
        Self {
            route: Some(route.into()),
            ..self
        }
    }

    /// Record what the selected protocol had negotiated.
    #[must_use]
    pub fn with_negotiated(self, negotiated: NegotiatedResponseMetadata) -> Self {
        Self {
            negotiated: Some(negotiated),
            ..self
        }
    }

    /// The identifier Camber minted for this request.
    pub fn request_id(&self) -> &RequestId {
        &self.request_id
    }

    /// The method exactly as the peer sent it.
    pub fn method(&self) -> &str {
        &self.method
    }

    /// The accepted URI path, without the query string.
    pub fn raw_path(&self) -> &str {
        &self.raw_path
    }

    /// The peer address, when the transport reported one.
    pub fn remote_addr(&self) -> Option<IpAddr> {
        self.remote_addr
    }

    /// The pattern the selected route is named by, once selection established it.
    ///
    /// A trie handler is named by its normalized pattern. A route selected
    /// outside the trie — an internal route, the gRPC dispatch class — is named
    /// by the fixed identity its registration carries.
    pub fn route(&self) -> Option<&str> {
        self.route.as_deref()
    }

    /// What the selected protocol negotiated, once an owner established it.
    pub fn negotiated(&self) -> Option<&NegotiatedResponseMetadata> {
        self.negotiated.as_ref()
    }
}

// ── Private rejection flow ─────────────────────────────────────────

/// The response policy a router was configured with.
///
/// Synchronous: it runs at failure, admission, and shutdown boundaries where
/// awaiting application work would create a second cancellable lifecycle.
pub(super) type RejectionMapper =
    dyn Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync;

/// Take one configured policy into the shape every stage reads it through.
///
/// Both routers offer this setting, and both hand the closure here, so what a
/// mapper is stays one contract. Two copies of these bounds are two places the
/// contract can be changed and only one of them updated.
pub(super) fn shared_mapper<F>(mapper: F) -> Arc<RejectionMapper>
where
    F: Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static,
{
    Arc::new(mapper)
}

/// The private cause behind one refusal, held only for operators.
pub(super) type Diagnostic = Arc<dyn Error + Send + Sync>;

/// The private detail behind a refusal that has no source error of its own.
///
/// A routing decision is not a failed operation, so there is no error to
/// borrow. This carries what an operator needs to read — the observed limit,
/// the received method, the frozen allowed set — without that text ever
/// reaching the client-safe projection.
///
/// Held as a `Cow` rather than a `Box<str>`: most producers state a fixed
/// sentence the framework already owns as a constant, and copying it onto the
/// heap for every `404` buys nothing. A producer that formats its observed
/// values lands in the owned half and pays for exactly the string it built.
#[derive(Debug)]
struct RefusalDetail(Cow<'static, str>);

impl RefusalDetail {
    /// One stated reason, in the shape a refusal carries its cause in.
    fn diagnostic(detail: impl Into<Cow<'static, str>>) -> Diagnostic {
        Arc::new(Self(detail.into()))
    }
}

/// The operator's account of one crossed service deadline.
///
/// It names the closed boundary the policy configured and the value configured
/// for it, so an operator reads which bound to widen rather than that
/// "something timed out". Its source is the typed
/// [`RuntimeError::DeadlineExceeded`] naming the same boundary, so a caller
/// walking the chain reaches the vocabulary rather than the sentence.
#[derive(Debug)]
struct CrossedDeadline {
    cause: RuntimeError,
    configured: std::time::Duration,
}

impl CrossedDeadline {
    fn new(boundary: DeadlineBoundary, configured: std::time::Duration) -> Self {
        Self {
            cause: RuntimeError::DeadlineExceeded(boundary),
            configured,
        }
    }
}

impl fmt::Display for CrossedDeadline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} after {:?}", self.cause, self.configured)
    }
}

impl Error for CrossedDeadline {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(&self.cause)
    }
}

impl fmt::Display for RefusalDetail {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl Error for RefusalDetail {}

/// What the transport must do once a refusal has been answered.
///
/// Held on the refusal rather than decided at the wire, because the stage that
/// found the fault is the one that knows whether the connection is still safe
/// to reuse. Response policy may choose the status and the body; it may not
/// weaken this.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Disposition {
    /// The connection may carry another request.
    Reusable,
    /// The connection must not carry another request.
    Close,
}

/// One header a protocol requires on the final response, whatever a mapper chose.
///
/// Required only at one status: a `405` has to carry `Allow`, but a mapper that
/// answers the same refusal with another non-informational status is describing
/// a different response, and the framework requires nothing of it.
struct ProtectedHeader {
    status: u16,
    name: &'static str,
    /// Borrowed when the protocol fixes the value, owned when the route does.
    ///
    /// A version this server speaks is the same text for every request, so
    /// nothing is copied for it. A frozen allowed set belongs to the trie that
    /// rendered it, so that one is copied here and nowhere else.
    value: Cow<'static, str>,
}

/// One refusal, before it becomes a response.
///
/// The safe projection and the private cause are separate fields rather than
/// one value with a redacting accessor: a mapper is handed the projection, and
/// there is no path from it back to the cause.
pub(super) struct Rejected {
    rejection: Rejection,
    diagnostic: Diagnostic,
    disposition: Disposition,
    protected: Option<ProtectedHeader>,
    /// The configured bound whose crossing produced this refusal.
    ///
    /// Carried as the closed vocabulary rather than left inside the diagnostic
    /// sentence, because the completion record has to name it as a label: text
    /// an operator reads and text a counter is keyed by are not the same thing,
    /// and parsing the first to produce the second is how a diagnostic becomes
    /// unbounded cardinality.
    crossed: CrossedBound,
}

impl Rejected {
    /// A refusal that answers with fixed text and keeps the connection.
    ///
    /// Every constructor funnels through here, so every refusal states a cause:
    /// "a refusal an operator is told nothing about" is not a state this type
    /// can hold.
    fn plain(rejection: Rejection, diagnostic: Diagnostic) -> Self {
        Self {
            rejection,
            diagnostic,
            disposition: Disposition::Reusable,
            protected: None,
            crossed: CrossedBound::None,
        }
    }

    /// The configured bound whose crossing produced this refusal.
    pub(super) const fn crossed(&self) -> CrossedBound {
        self.crossed
    }

    /// The same refusal, naming the bound its producer measured.
    #[must_use]
    fn over(self, crossed: CrossedBound) -> Self {
        Self { crossed, ..self }
    }

    /// A refusal whose cause is a decision rather than a failed operation.
    ///
    /// The detail is taken as anything that can become one immutable string, so
    /// a producer that formats its observed values and one that states a fixed
    /// sentence both land here without an intermediate copy.
    fn decided(rejection: Rejection, detail: impl Into<Cow<'static, str>>) -> Self {
        Self::plain(rejection, RefusalDetail::diagnostic(detail))
    }

    /// A refusal after which this connection cannot frame another request.
    fn closing(rejection: Rejection, detail: impl Into<Cow<'static, str>>) -> Self {
        Self::decided(rejection, detail).forcing_close()
    }

    /// The same, for a refusal whose cause is a failed operation.
    ///
    /// [`Self::closing`] covers the stages that state a reason; this covers the
    /// stages holding a source error. Both exist so a producer that leaves
    /// payload unread says so by picking a constructor, rather than by
    /// remembering a modifier after building a reusable refusal.
    fn plain_closing(rejection: Rejection, diagnostic: Diagnostic) -> Self {
        Self::plain(rejection, diagnostic).forcing_close()
    }

    /// The producer declared this message client-safe.
    ///
    /// The declared text moves into the safe projection, and the operator's
    /// account states what the safe projection cannot: that a producer, not the
    /// framework, chose this wording. A `RuntimeError::BadRequest` here read
    /// back as `bad request: {message}` — one restatement of the safe message,
    /// under a typed error nothing else looked at.
    fn declared(kind: RejectionKind, message: Box<str>) -> Self {
        let detail = format!("the producer declared this message client-safe: {message}");
        Self::decided(Rejection::raw(kind, 400, String::from(message)), detail)
    }

    /// The service cannot take the work right now.
    fn unavailable(error: RuntimeError) -> Self {
        Self::plain(
            Rejection::service_unavailable(RejectionKind::InternalService),
            Arc::new(error),
        )
    }

    /// The producer could not complete, and the peer learns nothing more.
    fn faulted(kind: RejectionKind, diagnostic: Diagnostic) -> Self {
        Self::plain(Rejection::redacted_fault(kind), diagnostic)
    }

    /// A body could not be parsed as the representation it declared.
    ///
    /// The parser's own account is the diagnostic; the peer is told only that
    /// the body was not the representation it claimed to be.
    fn unparseable(kind: RejectionKind, safe: &'static str, error: RuntimeError) -> Self {
        Self::plain(Rejection::raw(kind, 400, safe), Arc::new(error))
    }

    /// A response Camber accepted cannot be represented on the wire.
    pub(super) fn unrepresentable(error: hyper::http::Error) -> Self {
        Self::faulted(RejectionKind::InvalidHeader, Arc::new(error))
    }

    /// The body is larger than this request's effective limit will collect.
    ///
    /// Forces close: the bytes past the limit are still framed on the
    /// connection and will never be read, so nothing establishes where the next
    /// request on it would begin.
    pub(super) fn body_too_large(limit: usize) -> Self {
        Self::closing(
            Rejection::body_limit(),
            format!("body exceeds the {limit}-byte limit"),
        )
        .over(CrossedBound::Bytes(ByteBoundary::RequestBody))
    }

    /// A streaming consumer measured its payload past the maximum it was
    /// admitted under.
    ///
    /// The same category and the same safe answer as the buffered refusal above;
    /// what differs is only who counted. The consumer's own account is the
    /// diagnostic, because it names the bound that was crossed — the request's
    /// total or one field's — and the peer is told neither.
    fn body_limit_crossed(error: RuntimeError) -> Self {
        Self::plain_closing(Rejection::body_limit(), Arc::new(error))
            .over(CrossedBound::Bytes(ByteBoundary::RequestBody))
    }

    /// A streaming consumer's transport stopped delivering the payload.
    fn body_read_failed(error: RuntimeError) -> Self {
        Self::plain_closing(Rejection::body_unreadable(), Arc::new(error))
    }

    /// A streaming multipart route reached buffered dispatch.
    ///
    /// Camber's own fault and nothing the peer did, so the answer is the fixed
    /// redacted one. It exists because the alternative is a silent fall-through
    /// that would run a multipart handler with no session behind it.
    pub(super) fn multipart_misdispatched() -> Self {
        Self::decided(
            Rejection::redacted_fault(RejectionKind::InternalService),
            "a streaming multipart route reached buffered dispatch",
        )
    }

    /// The peer framed its body under a coding Camber cannot undo.
    ///
    /// Hyper accepted the chain because it ends in `chunked` and decoded that
    /// much; the remaining coding is one this service does not undo on the way
    /// in. Refused before a data frame is polled, because the bytes underneath
    /// are not the representation the request claims and no handler could read
    /// them as one.
    ///
    /// Forces close for the reason every pre-read refusal does: the framed body
    /// is left unread, so nothing establishes where the next request on this
    /// connection would begin.
    pub(super) fn undecodable_transfer_coding(coding: &str) -> Self {
        Self::closing(
            Rejection::raw(RejectionKind::MalformedBody, 400, UNPARSEABLE_BODY),
            format!("transfer coding {coding} cannot be decoded"),
        )
    }

    /// The application's body-admission policy refused this request's work.
    ///
    /// The policy's own error is the diagnostic and reaches neither the mapper
    /// nor the peer: an admission refusal states that the service declined the
    /// work, and the reason it declined is the operator's.
    ///
    /// Forces close for the reason every pre-read refusal does: the body was
    /// never read, so nothing establishes where the next request on this
    /// connection would begin.
    pub(super) fn body_admission_refused(error: RuntimeError) -> Self {
        Self::plain_closing(
            Rejection::service_unavailable(RejectionKind::BodyAdmission),
            Arc::new(error),
        )
    }

    /// The application's body-admission policy panicked.
    ///
    /// Camber itself could not complete the request, so the category is
    /// [`RejectionKind::InternalService`] and the peer is told nothing beyond
    /// the fixed redacted answer. The unwind is captured here rather than
    /// allowed to leave the request future.
    pub(super) fn body_admission_panicked(payload: Box<dyn std::any::Any + Send>) -> Self {
        Self::faulted(
            RejectionKind::InternalService,
            Arc::new(crate::task::panic_to_error(payload)),
        )
        .forcing_close()
    }

    /// The body stopped arriving for a reason that is not the limit.
    ///
    /// A mid-body transport failure or a peer reset is not an oversized body,
    /// and answering it as one tells the peer to shrink a request that was the
    /// right size. It is not an admission refusal either: nothing decided
    /// against the work, so the category is the transport's own. The
    /// transport's account is the diagnostic; the peer is told only that Camber
    /// could not read what it sent.
    ///
    /// Forces close for the same reason the limit does: what the peer still
    /// owed is unread, so nothing establishes where the next request would
    /// begin.
    pub(super) fn body_unreadable(error: Box<dyn Error + Send + Sync>) -> Self {
        Self::plain_closing(Rejection::body_unreadable(), Arc::from(error))
    }

    /// One streaming direction carried more payload than its maximum allows.
    ///
    /// The same category and safe answer the buffered and route-aware refusals
    /// give, because a peer or a producer that crossed a byte maximum is one
    /// condition however it was counted. What differs is the boundary the
    /// operator reads: the direction names itself, so an upload maximum is never
    /// reported as the download's.
    ///
    /// Forces close for the reason every byte refusal does: the payload past the
    /// maximum is unread, so nothing establishes where the next request on this
    /// connection would begin.
    pub(super) fn transfer_too_large(boundary: ByteBoundary, limit: usize) -> Self {
        Self::closing(
            Rejection::body_limit(),
            format!("{boundary} exceeds the {limit}-byte maximum"),
        )
        .over(CrossedBound::Bytes(boundary))
    }

    /// One streaming direction crossed a configured transfer deadline.
    ///
    /// The peer is told what it is told for every request-body deadline; the
    /// operator reads which of the two transfer bounds ended the work.
    pub(super) fn transfer_timeout(
        boundary: DeadlineBoundary,
        configured: std::time::Duration,
    ) -> Self {
        Self::crossed_deadline(
            Rejection::raw(RejectionKind::BodyTimeout, 408, "request body timed out"),
            boundary,
            configured,
        )
    }

    /// One streaming direction's source stopped delivering its payload.
    ///
    /// The source's own account is the diagnostic, for the reason
    /// [`Self::body_unreadable`] keeps the transport's: the stage that raised the
    /// fault is the one that can name it, and the peer is told only that Camber
    /// could not read what it sent.
    pub(super) fn transfer_source_failed(account: Box<str>) -> Self {
        Self::body_read_failed(RuntimeError::RequestBodyUnreadable(account))
    }

    /// The body stayed quiet longer than its configured idle interval.
    ///
    /// Forces close for the same reason a body limit does: what the peer still
    /// owed is unread, so the connection can frame nothing after it.
    pub(super) fn body_timeout(idle: std::time::Duration) -> Self {
        Self::crossed_deadline(
            Rejection::raw(RejectionKind::BodyTimeout, 408, "request body timed out"),
            DeadlineBoundary::RequestBodyIdle,
            idle,
        )
    }

    /// The admitted request outlived its total before a response head committed.
    ///
    /// Forces close for the same reason: the peer may still owe payload this
    /// request will never read, so nothing establishes where a next request
    /// framed on this connection would begin.
    pub(super) fn request_timeout(total: std::time::Duration) -> Self {
        Self::crossed_deadline(
            Rejection::raw(RejectionKind::RequestTimeout, 408, "request timed out"),
            DeadlineBoundary::RequestTotal,
            total,
        )
    }

    /// One refusal a configured deadline produced, under the boundary it names.
    ///
    /// The operator's account is the typed error itself rather than a sentence
    /// restating it: an operator filtering on a crossed bound reads the same
    /// closed [`DeadlineBoundary`] the policy that configured it is written in,
    /// and the client-safe projection still says only that the request timed
    /// out.
    fn crossed_deadline(
        rejection: Rejection,
        boundary: DeadlineBoundary,
        configured: std::time::Duration,
    ) -> Self {
        Self::plain_closing(
            rejection,
            Arc::new(CrossedDeadline::new(boundary, configured)),
        )
        .over(CrossedBound::Deadline(boundary))
    }

    /// No host and no route claims this request target.
    ///
    /// The detail is a constant its caller spells, because the two producers
    /// state which of the two lookups came up empty and neither observes
    /// anything a peer sent. Taken as `&'static str` so the most common refusal
    /// this server gives copies nothing onto the heap to say why.
    pub(super) fn not_found(detail: &'static str) -> Self {
        Self::decided(
            Rejection::raw(RejectionKind::Routing, 404, "not found"),
            detail,
        )
    }

    /// A router claims this authority, but no route claims the path.
    ///
    /// The one spelling of the refusal every unmatched path is answered with.
    /// Two dispatch sites reach it — the host-routed lookup and the plain one —
    /// and stated at each, the sentence an operator filters this server's most
    /// common refusal on was one edit away from being two sentences.
    pub(super) fn no_route() -> Self {
        Self::not_found("no route claims this path")
    }

    /// The `Host` value is not an authority Camber can resolve a router from.
    ///
    /// Forces close: the authority the peer named could not be parsed, so
    /// nothing establishes that the next request framed on this connection was
    /// meant for this service.
    pub(super) fn invalid_host(received: &str) -> Self {
        Self::closing(
            Rejection::raw(RejectionKind::Routing, 400, "invalid host header"),
            format!("unparseable Host value {received:?}"),
        )
    }

    /// The request target nests deeper than the router will match.
    pub(super) fn uri_too_deep(limit: usize) -> Self {
        Self::decided(
            Rejection::raw(RejectionKind::Routing, 414, "URI path too deep"),
            format!("path exceeds the {limit}-segment match limit"),
        )
    }

    /// A route claims this path, but not with the received method.
    ///
    /// The allowed set is borrowed from the trie, which settled it while
    /// resolving the path. Nothing here re-derives it from the route.
    ///
    /// The set is taken into an owned value once and then shared between the
    /// safe default header and the protected one, so the advertised value and
    /// the required value are the same text by construction. The header name is
    /// a constant this file spells, so it is borrowed rather than copied.
    pub(super) fn method_not_allowed(received: &str, allow: &str) -> Self {
        let detail = format!("received {received}, frozen allowed set is {allow}");
        let allowed = allow.to_owned();
        Self::decided(
            Rejection::raw(RejectionKind::MethodSelection, 405, "method not allowed")
                .with_static_header(ALLOW_HEADER, allowed.clone()),
            detail,
        )
        .protecting(ALLOW_HEADER, allowed)
    }

    /// The upgrade request is not a WebSocket handshake Camber can complete.
    #[cfg(feature = "ws")]
    pub(super) fn ws_bad_handshake() -> Self {
        Self::decided(
            Rejection::raw(
                RejectionKind::WebSocketHandshake,
                400,
                "invalid WebSocket upgrade headers",
            ),
            "handshake method, version, key, or subprotocol syntax is not acceptable",
        )
    }

    /// The handshake names a WebSocket version Camber does not speak.
    ///
    /// The required version is both a safe default header and a protected one:
    /// a mapper answering `426` is describing the same refusal, and a `426` that
    /// advertised another version would tell the peer to retry with a version
    /// this server will refuse again.
    #[cfg(feature = "ws")]
    pub(super) fn ws_unsupported_version() -> Self {
        Self::decided(
            Rejection::raw(
                RejectionKind::WebSocketHandshake,
                426,
                "unsupported WebSocket version",
            )
            .with_static_header(WS_VERSION_HEADER, WS_VERSION),
            format!("handshake requested a version other than {WS_VERSION}"),
        )
        .protecting(WS_VERSION_HEADER, WS_VERSION)
    }

    /// The handshake's `Origin` is not one this service accepts.
    ///
    /// The detail arrives from the caller because one check finds several
    /// distinct faults — a repeated `Origin`, no single `Host` to compare
    /// against, or authorities that genuinely differ — and a fixed sentence
    /// here would describe two of them wrongly.
    #[cfg(feature = "ws")]
    pub(super) fn ws_origin_rejected(detail: &'static str) -> Self {
        Self::decided(
            Rejection::raw(
                RejectionKind::WebSocketHandshake,
                403,
                "WebSocket origin rejected",
            ),
            detail,
        )
    }

    /// Camber accepted the handshake but could not build the `101` it earned.
    ///
    /// The builder's own error is the diagnostic: it is the only account of
    /// what could not be represented, and it reaches the operator joined to the
    /// request, route and subprotocol this refusal already names.
    #[cfg(feature = "ws")]
    pub(super) fn ws_upgrade_unbuildable(error: hyper::http::Error) -> Self {
        Self::uncommitted_upgrade(
            Rejection::redacted_fault(RejectionKind::InternalService),
            Arc::new(error),
        )
    }

    /// The supervisor declined to take ownership of this upgrade.
    #[cfg(feature = "ws")]
    pub(super) fn upgrade_registration_refused() -> Self {
        Self::uncommitted_upgrade(
            Rejection::service_unavailable(RejectionKind::InternalService),
            RefusalDetail::diagnostic("the supervisor refused to own this upgrade"),
        )
    }

    /// No supervisor was able to take ownership of this upgrade.
    #[cfg(feature = "ws")]
    pub(super) fn upgrade_registration_unavailable() -> Self {
        Self::uncommitted_upgrade(
            Rejection::redacted_fault(RejectionKind::InternalService),
            RefusalDetail::diagnostic("no supervisor could take ownership of this upgrade"),
        )
    }

    /// An upgrade refused before commitment, on a transport that must close.
    ///
    /// Forces close: the peer asked to leave HTTP and is being told it may not,
    /// on a connection whose upgrade intent nothing else can unwind. Every
    /// refusal in that state comes through here — the `101` that could not be
    /// built and the two the supervisor would not own — so one place decides
    /// what such a connection may do next.
    ///
    /// The answer arrives already built. Each of the three is one of the two
    /// answers this file spells for every producer that gives them, so taking a
    /// status and a body here would be a third place either could be written.
    #[cfg(feature = "ws")]
    fn uncommitted_upgrade(rejection: Rejection, diagnostic: Diagnostic) -> Self {
        Self::plain_closing(rejection, diagnostic)
    }

    /// The refusal one proxy fault is answered through.
    ///
    /// Every proxy class classifies here, so one fault reaches a route's own
    /// policy under the same category, status and body whichever class found
    /// it. The three faults stay separate accounts under that one category: a
    /// target this proxy could not build names either the peer's path or the
    /// configured backend, a request it could not send is Camber's own, and an
    /// upstream that gave no usable answer — no head, or a head whose body could
    /// not be read — is the backend's. Recorded as one, a traversal probe and a
    /// Camber-side fault would both read to an operator as a backend outage.
    pub(super) fn from_proxy_failure(failure: ProxyFailure) -> Self {
        let (status, safe) = proxy_failure_status(&failure);
        let rejection = Rejection::raw(RejectionKind::Proxy, status, safe);
        match failure {
            ProxyFailure::UnbuildableTarget(detail) => Self::decided(rejection, detail),
            ProxyFailure::Unsendable(diagnostic) => Self::plain(rejection, diagnostic),
            ProxyFailure::Phase(failure) => Self::plain(rejection, Arc::new(failure)),
        }
    }

    /// No upstream is admissible for a proxied route.
    pub(super) fn no_admissible_backend() -> Self {
        Self::decided(
            Rejection::service_unavailable(RejectionKind::Proxy),
            "every backend for this route is failing its health check",
        )
    }

    /// A class that owes a middleware gate resolved no chain to run it.
    ///
    /// Refused rather than passed. A stream, an SSE, a WebSocket, or a proxied
    /// stream reaches its upstream on a pass, so reading "no chain" as "the
    /// chain agreed" would forward a request whose middleware never ran.
    pub(super) fn gate_unrunnable() -> Self {
        Self::decided(
            Rejection::redacted_fault(RejectionKind::InternalService),
            "a dispatch class owing a middleware gate resolved no router to run it",
        )
    }

    /// Require one header on the final response, at this refusal's own status.
    ///
    /// The status is read off the refusal rather than restated by the caller.
    /// Spelled twice, a constructor could advertise `Allow` on a `405` and
    /// protect it at some other number, and the mismatch would silently stop
    /// protecting anything: protocol enforcement applies the header only where
    /// the status it names is the status answered.
    #[must_use]
    fn protecting(self, name: &'static str, value: impl Into<Cow<'static, str>>) -> Self {
        let status = self.rejection.status();
        Self {
            protected: Some(ProtectedHeader {
                status,
                name,
                value: value.into(),
            }),
            ..self
        }
    }

    /// Require the transport to close once this refusal has been answered.
    #[must_use]
    fn forcing_close(self) -> Self {
        Self {
            disposition: Disposition::Close,
            ..self
        }
    }
}

/// The status and safe body one proxy fault is answered with.
///
/// An upstream deadline is a gateway timeout; every other proxy fault is a bad
/// gateway, including the two that stopped before an upstream was reached — a
/// target this proxy could not build and a request it could not send waited on
/// nothing, so neither expired.
///
/// Named here rather than inlined, because the buffered proxy entry point
/// answers this same fault with a status alone and builds no [`Rejected`] at
/// all: a second spelling of the pair is a second place it can drift from this
/// one.
pub(super) fn proxy_failure_status(failure: &ProxyFailure) -> (u16, &'static str) {
    match expired_upstream(failure) {
        true => (504, GATEWAY_TIMEOUT_BODY),
        false => (502, BAD_GATEWAY_BODY),
    }
}

/// Whether one proxy fault is a deadline this route configured.
///
/// Only a phase that reached an upstream can expire, and it names which of its
/// deadlines it crossed. The two faults that stopped before an upstream was
/// reached waited on nothing, so neither can be one.
fn expired_upstream(failure: &ProxyFailure) -> bool {
    match failure {
        ProxyFailure::Phase(failure) => matches!(
            failure.cause(),
            RuntimeError::Timeout | RuntimeError::DeadlineExceeded(_)
        ),
        ProxyFailure::UnbuildableTarget(_) | ProxyFailure::Unsendable(_) => false,
    }
}

/// The categories one producing stage classifies its own failures under.
///
/// Only two rows differ by producer: the message the producer declared safe,
/// and a fault it can say nothing safe about. Every other row belongs to the
/// framework producer that raised it, so it classifies the same however far it
/// was propagated.
#[derive(Clone, Copy)]
pub(super) struct ProducerKinds {
    /// The category for a failure whose message the producer declared safe.
    declared: RejectionKind,
    /// The category for a failure with no safe detail at all.
    faulted: RejectionKind,
}

/// How a buffered handler's own failures are classified.
pub(super) const HANDLER: ProducerKinds = ProducerKinds {
    declared: RejectionKind::Application,
    faulted: RejectionKind::InternalService,
};

/// How a middleware frame's own failures are classified.
pub(super) const MIDDLEWARE: ProducerKinds = ProducerKinds {
    declared: RejectionKind::Middleware,
    faulted: RejectionKind::Middleware,
};

/// How one streaming multipart session's own failures are classified.
///
/// Every failure it actually raises names its own provenance below — a byte
/// crossing, a transport read, or the grammar — so these two rows cover only
/// what the session never produces: it declares no client-safe message of its
/// own, and a fault it can say nothing about is Camber's.
pub(super) const MULTIPART_SESSION: ProducerKinds = ProducerKinds {
    declared: RejectionKind::Multipart,
    faulted: RejectionKind::InternalService,
};

/// Classify one failure at the stage that raised it.
fn classify(error: RuntimeError, kinds: ProducerKinds) -> Rejected {
    match error {
        RuntimeError::BadRequest(message) => Rejected::declared(kinds.declared, message),
        RuntimeError::ScopeClosed => Rejected::unavailable(RuntimeError::ScopeClosed),
        parsed @ RuntimeError::MalformedBody(_) => {
            Rejected::unparseable(RejectionKind::MalformedBody, UNPARSEABLE_BODY, parsed)
        }
        parsed @ RuntimeError::Multipart(_) => {
            Rejected::unparseable(RejectionKind::Multipart, INVALID_MULTIPART_BODY, parsed)
        }
        // Classified by the error's own provenance rather than by the producer's
        // rows: a handler that propagated one of these with `?` is reporting the
        // framework's measurement, not declaring a failure of its own.
        crossed @ RuntimeError::RequestBodyLimit(_) => Rejected::body_limit_crossed(crossed),
        unread @ RuntimeError::RequestBodyUnreadable(_) => Rejected::body_read_failed(unread),
        other => Rejected::faulted(kinds.faulted, Arc::new(other)),
    }
}

/// The request facts a context is materialized from.
///
/// Owned because the mapping point outlives the borrow of the request: the
/// `Uri` is `Bytes`-backed, so holding one is a refcount bump rather than a
/// copy of the path.
#[derive(Clone)]
pub(super) struct RequestIdentity {
    request_id: RequestId,
    method: RequestMethod,
    uri: hyper::Uri,
    remote_addr: Option<IpAddr>,
    version: hyper::Version,
    /// The registered pattern, once path matching established it.
    route: Option<Arc<str>>,
    /// The dispatch class, once method selection established it.
    protocol: Option<RejectionProtocol>,
    /// The representation, once an accepted response head established it.
    content_type: Option<Box<str>>,
    /// The WebSocket subprotocol, once negotiation selected one.
    subprotocol: Option<Box<str>>,
}

impl RequestIdentity {
    /// Name a request from the head that has not been built into one yet.
    pub(super) fn from_head(
        origin: &super::request::RequestOrigin<'_>,
        method: &hyper::Method,
        uri: &hyper::Uri,
    ) -> Self {
        Self {
            request_id: origin.request_id,
            method: RequestMethod::from_hyper(method),
            uri: uri.clone(),
            remote_addr: origin.remote_addr,
            version: origin.version,
            route: None,
            protocol: None,
            content_type: None,
            subprotocol: None,
        }
    }

    /// Name a request that has already been built.
    pub(super) fn from_request(req: &Request) -> Self {
        Self {
            request_id: req.request_id(),
            method: req.request_method().clone(),
            uri: req.uri_owned(),
            remote_addr: req.remote_addr(),
            version: req.http_version(),
            route: None,
            protocol: None,
            content_type: None,
            subprotocol: None,
        }
    }

    /// Record the registered pattern path matching established.
    #[must_use]
    pub(super) fn with_route(self, route: Arc<str>) -> Self {
        Self {
            route: Some(route),
            ..self
        }
    }

    /// Record the dispatch class method selection established.
    #[must_use]
    pub(super) fn with_protocol(self, protocol: RejectionProtocol) -> Self {
        Self {
            protocol: Some(protocol),
            ..self
        }
    }

    /// Record the representation an accepted response head established.
    #[must_use]
    fn with_content_type(self, content_type: &str) -> Self {
        Self {
            content_type: Some(content_type.into()),
            ..self
        }
    }

    /// Record the subprotocol WebSocket negotiation selected.
    #[must_use]
    #[cfg(feature = "ws")]
    fn with_subprotocol(self, subprotocol: &str) -> Self {
        Self {
            subprotocol: Some(subprotocol.into()),
            ..self
        }
    }

    /// The bounded label this request is recorded under.
    fn method_label(&self) -> &'static str {
        self.method.label()
    }

    /// The bounded dispatch class this request is recorded under.
    ///
    /// A head answered before any owner established a class has none, and it is
    /// named rather than left empty: a completion counter whose class label is
    /// sometimes absent splits one time series into two.
    fn protocol_label(&self) -> &'static str {
        match self.protocol {
            Some(protocol) => protocol.label(),
            None => UNCLASSIFIED_PROTOCOL,
        }
    }

    /// Whether the wire answer to this request carries no body.
    fn is_head(&self) -> bool {
        self.method
            .routable()
            .is_some_and(super::request::method_is_head)
    }

    /// Build the owned snapshot one refusal hands to policy.
    fn materialize(&self) -> RejectionContext {
        let base = RejectionContext::new(
            self.request_id,
            self.method.to_cow(),
            self.uri.path().to_owned(),
        );
        let addressed = match self.remote_addr {
            Some(remote_addr) => base.with_remote_addr(remote_addr),
            None => base,
        };
        let routed = match &self.route {
            Some(route) => addressed.with_route(route),
            None => addressed,
        };
        match self.protocol {
            Some(protocol) => routed.with_negotiated(self.negotiated(protocol)),
            None => routed,
        }
    }

    /// What the selected dispatch class had negotiated by this point.
    fn negotiated(&self, protocol: RejectionProtocol) -> NegotiatedResponseMetadata {
        let base = NegotiatedResponseMetadata::new(protocol);
        let typed = match &self.content_type {
            Some(content_type) => base.with_content_type(content_type),
            None => base,
        };
        match &self.subprotocol {
            Some(subprotocol) => typed.with_subprotocol(subprotocol),
            None => typed,
        }
    }
}

/// One response as the wire will carry it, and the refusal it answered.
///
/// The category is carried beside the response rather than read off it: the
/// wire value is Hyper's, which has no room for Camber's taxonomy, and the last
/// stage that could raise a refusal is the conversion that produces it.
pub(super) struct Finalized {
    pub(super) response: hyper::Response<Full<Bytes>>,
    pub(super) refused: Option<RejectionKind>,
}

/// The rejection policy and identity one request resolved to.
///
/// Cloned once per middleware frame per request, into a terminal that must own
/// everything it reads. Both fields are shared handles, so entering a frame is
/// two refcount bumps and no allocation however deep the chain is — the
/// identity carries a `Uri`, a method, a route, and a rendered identifier, and
/// copying all of that per frame is what this shape exists to avoid.
///
/// The identity is rebuilt, not mutated, at the few transitions that establish
/// something new. Each of those runs once per request, before any chain is
/// entered.
#[derive(Clone)]
pub(super) struct RejectionScope {
    mapper: Option<Arc<RejectionMapper>>,
    identity: Arc<RequestIdentity>,
}

impl RejectionScope {
    /// The policy a resolved router selected.
    pub(super) fn new(mapper: Option<Arc<RejectionMapper>>, identity: RequestIdentity) -> Self {
        Self {
            mapper,
            identity: Arc::new(identity),
        }
    }

    /// The same policy, over an identity one transition has added to.
    fn transitioned(self, established: impl FnOnce(RequestIdentity) -> RequestIdentity) -> Self {
        Self {
            mapper: self.mapper,
            identity: Arc::new(established(own_identity(self.identity))),
        }
    }

    /// The label this request is recorded under.
    pub(super) fn method_label(&self) -> &'static str {
        self.identity.method_label()
    }

    /// The bounded dispatch class this request is recorded under.
    pub(super) fn protocol_label(&self) -> &'static str {
        self.identity.protocol_label()
    }

    /// The accepted URI path this request is recorded under.
    pub(super) fn path(&self) -> &str {
        self.identity.uri.path()
    }

    /// The identifier Camber minted for this request.
    pub(super) fn request_id(&self) -> RequestId {
        self.identity.request_id
    }

    /// Whether this request asked for a head and no body.
    ///
    /// [`convert`](Self::convert) strips the body of a refusal from the same
    /// answer. A stage that re-derived the method would be a second place the
    /// rule could be decided, and the copy that drifted would strip a body this
    /// one kept.
    pub(super) fn is_head(&self) -> bool {
        self.identity.is_head()
    }

    /// The same policy, named by what the stage that resolved it established.
    #[must_use]
    pub(super) fn established(self, route: Arc<str>, protocol: RejectionProtocol) -> Self {
        self.transitioned(|identity| identity.with_route(route).with_protocol(protocol))
    }

    /// The same policy, under the dispatch class this request actually took.
    ///
    /// A proxied route that carries an upgrade leaves the trie as proxy
    /// dispatch and is answered as a WebSocket handshake. The class a refusal
    /// reports is the one it was answered under, not the one its handler was
    /// registered as.
    #[must_use]
    #[cfg(feature = "ws")]
    pub(super) fn reclassified(self, protocol: RejectionProtocol) -> Self {
        self.transitioned(|identity| identity.with_protocol(protocol))
    }

    /// The same policy, named by the subprotocol negotiation just selected.
    ///
    /// Called only after a subprotocol has been chosen, so a handshake that
    /// fails before that point reports none — absence here is the transition
    /// not having happened, never an empty value.
    #[must_use]
    #[cfg(feature = "ws")]
    pub(super) fn negotiated_subprotocol(self, subprotocol: &str) -> Self {
        self.transitioned(|identity| identity.with_subprotocol(subprotocol))
    }

    /// Resolve one outcome into the response the peer is given.
    ///
    /// Called at the stage that produced the outcome, so a failure is
    /// classified where its producer is still known and the mapped response
    /// unwinds only through the frames entered outside that point.
    pub(super) fn resolve(
        &self,
        outcome: Result<Response, RuntimeError>,
        kinds: ProducerKinds,
    ) -> Response {
        match outcome {
            Ok(response) => response,
            Err(error) => self.map(classify(error, kinds)),
        }
    }

    /// Convert one buffered response for the wire.
    ///
    /// The `HEAD` body strip happens here rather than at each caller, so no
    /// exit can strip a body another one keeps. A conversion failure on an
    /// application response is `InvalidHeader` and reaches policy once; a
    /// conversion failure on a response policy already produced goes straight
    /// to the fixed fallback, so response construction cannot recurse.
    ///
    /// The category travels out with the response because this is the last
    /// stage that can raise one: a refusal found during conversion is still a
    /// refusal the operator's counters have to see.
    pub(super) fn finalize(&self, response: Response) -> Finalized {
        let (provenance, converted) = self.convert(response);
        match converted {
            Ok(converted) => sent(converted, provenance),
            Err(refused) => self.recover(provenance, refused),
        }
    }

    /// Convert one response, stripping the body a `HEAD` never receives.
    fn convert(
        &self,
        response: Response,
    ) -> (
        ResponseProvenance,
        Result<hyper::Response<Full<Bytes>>, UnrepresentableResponse>,
    ) {
        match self.identity.is_head() {
            true => response.strip_body().into_wire(),
            false => response.into_wire(),
        }
    }

    /// Answer a response Camber accepted but cannot put on the wire.
    ///
    /// A response policy already produced keeps the category it was answering:
    /// the fixed fallback replaces what the peer is given, not what the refusal
    /// was.
    fn recover(
        &self,
        provenance: ResponseProvenance,
        refused: UnrepresentableResponse,
    ) -> Finalized {
        match provenance.into_refusal() {
            Some(mut mapped) => self.displace_mapped(&mut mapped),
            None => self.map_unrepresentable(refused),
        }
    }

    /// Answer a mapped response the wire cannot carry, and say so.
    ///
    /// The refusal is recorded here, under the fixed fallback's status, because
    /// that is the status the peer is given: an operator correlating the record
    /// against the wire has to read the same value in both. What policy had
    /// settled on is named by its own event instead of being lost. Reported as
    /// its own condition rather than as a second refusal: nothing was
    /// reclassified, the peer was simply given the fixed answer instead of the
    /// mapped one.
    fn displace_mapped(&self, mapped: &mut MappedRefusal) -> Finalized {
        mapped.record(fallback_status());
        tracing::error!(
            request_id = self.identity.request_id.as_str(),
            kind = mapped.kind().label(),
            mapped_status = mapped.mapped_status(),
            status = fallback_status(),
            "mapped rejection response could not be represented"
        );
        Finalized {
            response: fallback_hyper(self.identity.request_id),
            refused: Some(mapped.kind()),
        }
    }

    /// Map an application response the wire cannot carry.
    ///
    /// Mapped under a scope that knows the content type the refused head had
    /// established: the head was accepted here, so this is the last point that
    /// can tell policy what the response was going to be.
    ///
    /// Policy's answer goes back through finalization so it is recorded under
    /// the status it actually leaves with. That re-entry is bounded at one
    /// step: what `map` returns always carries a refusal, so the second pass
    /// reaches [`Self::displace_mapped`] and never arrives back here.
    fn map_unrepresentable(&self, refused: UnrepresentableResponse) -> Finalized {
        let scope = self.after_head(refused.content_type());
        let mapped = scope.map(Rejected::unrepresentable(refused.into_error()));
        scope.finalize(mapped)
    }

    /// The same policy and identity, with what an accepted response head established.
    fn after_head(&self, content_type: Option<&str>) -> Self {
        match content_type {
            Some(content_type) => self
                .clone()
                .transitioned(|identity| identity.with_content_type(content_type)),
            None => self.clone(),
        }
    }

    /// Turn one refusal into the response the peer is given.
    ///
    /// One boundary: policy runs once, protocol-owned output is corrected after
    /// it, and the result is marked as policy's so a later conversion failure
    /// cannot re-enter here.
    pub(super) fn map(&self, mut rejected: Rejected) -> Response {
        let context = self.identity.materialize();
        // Taken, not borrowed: enforcement is the last reader of the protected
        // header, and the refusal that rides out on the response carries only
        // its category and its cause. Left in place, every `405` copied its own
        // frozen allowed set onto the heap for a value nothing read again.
        let protected = rejected.protected.take();
        let answered = self
            .invoke(&rejected.rejection, &context)
            .and_then(|response| {
                without_informational_status(response, &rejected.rejection, &context)
            })
            .unwrap_or_else(|| fallback_response(*context.request_id()));
        let final_response = self.enforce_protocol(answered, protected, rejected.disposition);
        // Not recorded here. Policy has settled on a status — after the mapper,
        // after the fixed fallback, and after protocol correction — but the
        // wire has not accepted it yet, and a response conversion that fails
        // sends the peer a different one. The refusal rides out with the
        // response and is recorded once, where the sent status is known. The
        // fixed fallback is one of those outcomes, so it is recorded by that
        // same event and never by a second one.
        let mapped_status = final_response.status();
        final_response.mark_mapped(MappedRefusal::new(rejected, context, mapped_status))
    }

    /// Run the selected policy once for one refusal.
    fn invoke(&self, rejection: &Rejection, context: &RejectionContext) -> Option<Response> {
        match &self.mapper {
            Some(mapper) => catch_mapper(mapper, rejection, context),
            None => Some(built_in_response(rejection, context)),
        }
    }

    /// Apply the output the protocol owns, over whatever policy chose.
    ///
    /// A mapper controls the status, the body, the content type, and every
    /// application header. It does not control the values that make a response
    /// mean what its status says, or the transport disposition the producing
    /// stage decided. Both corrections are static validated data, so neither
    /// can fail the way the response they correct might.
    fn enforce_protocol(
        &self,
        response: Response,
        protected: Option<ProtectedHeader>,
        disposition: Disposition,
    ) -> Response {
        let required = match protected {
            Some(header) if header.status == response.status() => {
                response.with_replaced_header(header.name, header.value)
            }
            Some(_) | None => response,
        };
        self.enforce_disposition(required, disposition)
    }

    /// Apply the transport disposition this refusal decided.
    fn enforce_disposition(&self, response: Response, disposition: Disposition) -> Response {
        match (self.closes_transport(), disposition) {
            // HTTP/2 frames every request on its own stream, so neither an
            // unread body nor an unusable authority can desynchronize the
            // connection: there is nothing here for a disposition to enact.
            // The header is illegal on this version besides, so a mapper's copy
            // of it leaves here.
            (false, _) => response.without_headers(&CONNECTION_SPECIFIC_HEADERS),
            (true, Disposition::Close) => {
                response.with_replaced_header(CONNECTION_HEADER, Cow::Borrowed(CLOSE_DISPOSITION))
            }
            (true, Disposition::Reusable) => response,
        }
    }

    /// Whether ending this request's transport is what stops it being reused.
    ///
    /// The one place the version question is answered for an unread body. An
    /// HTTP/1 connection frames its next request immediately behind this one,
    /// so payload left unread has to end the connection; an HTTP/2 stream is
    /// its own frame sequence, so ending that stream is already the whole
    /// answer and the connection carries on.
    pub(super) fn closes_transport(&self) -> bool {
        !matches!(self.identity.version, hyper::Version::HTTP_2)
    }

    /// Leave this transport unable to frame a request behind an unread body.
    ///
    /// The disposition rule above, applied to an answer that is not a refusal:
    /// a specialized gate's own response, produced before a payload frame was
    /// ever polled. A mapped refusal states its disposition on itself; these
    /// have none to state, so the stage that leaves payload unread says it here.
    pub(super) fn closing(&self, response: Response) -> Response {
        self.enforce_disposition(response, Disposition::Close)
    }
}

/// Run one configured mapper without letting its failure escape the request.
///
/// A panic is isolated here because a mapper runs during failure teardown: an
/// unwind escaping into the request task would take the connection with it and
/// leave the peer with no answer at all.
///
/// Both failures name the request and the category they displaced. Without
/// those an operator reads that a mapper broke but not which refusal lost its
/// answer, and the fixed fallback the peer then receives says nothing either.
fn catch_mapper(
    mapper: &Arc<RejectionMapper>,
    rejection: &Rejection,
    context: &RejectionContext,
) -> Option<Response> {
    match std::panic::catch_unwind(AssertUnwindSafe(|| mapper(rejection, context))) {
        Ok(Ok(response)) => Some(response),
        Ok(Err(error)) => {
            tracing::error!(
                request_id = context.request_id().as_str(),
                kind = rejection.kind().label(),
                cause = %SourceChain(&error),
                "rejection mapper failed"
            );
            None
        }
        Err(payload) => {
            tracing::error!(
                request_id = context.request_id().as_str(),
                kind = rejection.kind().label(),
                panic = crate::task::panic_message(payload.as_ref()),
                "rejection mapper panicked"
            );
            None
        }
    }
}

/// Refuse a mapped response that would falsely claim a protocol handoff.
///
/// The third failure that displaces a mapped answer with the fixed fallback, so
/// it names the request and the category the same way the other two do: without
/// them an operator reads that some mapper answered `1xx` and not which refusal
/// lost its answer.
fn without_informational_status(
    response: Response,
    rejection: &Rejection,
    context: &RejectionContext,
) -> Option<Response> {
    match response.status() < 200 {
        true => {
            tracing::error!(
                request_id = context.request_id().as_str(),
                kind = rejection.kind().label(),
                status = response.status(),
                "rejection mapper returned an informational status"
            );
            None
        }
        false => Some(response),
    }
}

/// The answer the built-in mapper gives.
///
/// Provenance is not marked here: `map` marks whatever policy returned, so one
/// site owns that invariant for the configured and built-in mappers alike.
fn built_in_response(rejection: &Rejection, context: &RejectionContext) -> Response {
    let base = Response::text_raw(rejection.status(), rejection.message());
    let with_defaults = rejection
        .header_pairs()
        .iter()
        .fold(base, |response, (name, value)| {
            response.with_pair(name.clone(), value.clone())
        });
    with_request_id(with_defaults, *context.request_id())
}

/// Name the request a built-in answer belongs to.
///
/// One site for both answers Camber builds itself, so the header the built-in
/// mapper attaches and the header the fixed fallback attaches cannot be spelled
/// apart.
fn with_request_id(response: Response, request_id: RequestId) -> Response {
    response.with_static_header(
        REQUEST_ID_HEADER,
        Cow::Owned(request_id.as_str().to_owned()),
    )
}

/// The one answer to a policy that could not supply a usable response.
///
/// Built from fixed text and the request's own identifier, and never through a
/// mapper: reaching for policy again is how a failing mapper would recurse.
///
/// Takes the identifier rather than the whole context, because that is all it
/// reads and the hyper form below has nothing else to hand it.
fn fallback_response(request_id: RequestId) -> Response {
    with_request_id(
        Response::text_raw(fallback_status(), INTERNAL_ERROR_BODY),
        request_id,
    )
}

/// The redacted answer to a response Camber cannot represent at all.
///
/// The same fallback as [`fallback_response`], carried through the ordinary
/// conversion: the fixed answer is spelled once, and the hyper form cannot drift
/// from the Camber form in status, body, or headers.
fn fallback_hyper(request_id: RequestId) -> hyper::Response<Full<Bytes>> {
    let (_, converted) = fallback_response(request_id).into_wire();
    match converted {
        Ok(response) => response,
        Err(refused) => {
            tracing::error!(
                request_id = request_id.as_str(),
                cause = %SourceChain(&refused.into_error()),
                "fixed fallback response could not be represented"
            );
            unnamed_fallback()
        }
    }
}

/// The same redacted status and body, naming no request.
///
/// The last step of [`fallback_hyper`], which reaches it only if a 32-digit
/// hexadecimal identifier stops being a valid header value.
fn unnamed_fallback() -> hyper::Response<Full<Bytes>> {
    let body = Full::new(Bytes::from_static(INTERNAL_ERROR_BODY.as_bytes()));
    let mut response = hyper::Response::new(body);
    *response.status_mut() = FALLBACK_STATUS_CODE;
    response
}

/// One refusal, and the answer policy settled on for it.
///
/// Travels on the response policy produced, as far as the wire exit, because
/// only the exit knows the status the peer was actually given — conversion can
/// still displace what policy chose. The private diagnostic travels inside it
/// rather than beside it, so the recording stays here and `http/record.rs` is
/// never given access to a cause the peer must not see.
pub(super) struct MappedRefusal {
    rejected: Rejected,
    context: RejectionContext,
    mapped_status: u16,
    /// Whether an exit has already recorded this refusal.
    ///
    /// Read by [`Drop`], which is what answers for a refusal no exit reached. A
    /// plain flag rather than a shared cell: the two exits own the value while
    /// they record it, so nothing else can be looking.
    recorded: bool,
}

/// What became of the answer policy produced for one refusal.
///
/// The distinction the record is written under: a refusal the peer was answered
/// with has a sent status, and a refusal that was thrown away has none.
enum Delivery {
    /// The peer received this status.
    Sent(u16),
    /// The peer received something else, or nothing at all.
    ///
    /// An outer middleware or gate frame answered in place of the mapped
    /// response, or the request ended before the answer could be sent. Either
    /// way no status on the wire belongs to this refusal, so the record names
    /// what policy had chosen instead and the request's completion event names
    /// what the peer actually got.
    Discarded,
}

impl Delivery {
    /// The status the peer was given for this refusal, when it was given one.
    fn sent_status(&self) -> Option<u16> {
        match self {
            Self::Sent(status) => Some(*status),
            Self::Discarded => None,
        }
    }

    /// The status policy chose, reported only when nothing sent it.
    ///
    /// Off the answered record entirely: there it would restate the status
    /// beside itself, and an operator filtering on one refusal would read the
    /// same number twice.
    fn unsent_status(&self, mapped_status: u16) -> Option<u16> {
        match self {
            Self::Sent(_) => None,
            Self::Discarded => Some(mapped_status),
        }
    }

    /// The fixed sentence this disposition is recorded under.
    fn condition(&self) -> &'static str {
        match self {
            Self::Sent(_) => "request rejected",
            Self::Discarded => "rejection response was not sent",
        }
    }
}

impl MappedRefusal {
    /// Pair one refusal with the answer and the status policy gave it.
    fn new(rejected: Rejected, context: RejectionContext, mapped_status: u16) -> Self {
        Self {
            rejected,
            context,
            mapped_status,
            recorded: false,
        }
    }

    /// The category this refusal is recorded and counted under.
    pub(super) fn kind(&self) -> RejectionKind {
        self.rejected.rejection.kind()
    }

    /// The status policy settled on, before the wire had its say.
    fn mapped_status(&self) -> u16 {
        self.mapped_status
    }

    /// Record this refusal under the status the peer was given.
    ///
    /// The status is taken rather than read off this value: it is the one the
    /// peer received, which the wire exit owns and this refusal cannot know.
    /// Taking `&mut` is what marks the refusal answered, so the drop that
    /// follows knows it has nothing left to report.
    fn record(&mut self, status: u16) {
        self.recorded = true;
        self.emit(Delivery::Sent(status));
    }

    /// Record this refusal for operators, with the cause the peer never sees.
    ///
    /// Every value is its own field, and the message is one fixed sentence: an
    /// operator filters on the category, the status, or the request, and a
    /// cause spliced into the message would make each of those a different
    /// string. Optional context is recorded exactly when its owner established
    /// it — a `None` field records nothing rather than a sentinel.
    ///
    /// One site for both dispositions, so a refusal the peer never received is
    /// described by the same fields as one it did, and only the sentence and
    /// the status field say which happened.
    fn emit(&self, delivery: Delivery) {
        let context = &self.context;
        let negotiated = context.negotiated();
        tracing::error!(
            request_id = context.request_id().as_str(),
            kind = self.kind().label(),
            status = delivery.sent_status(),
            mapped_status = delivery.unsent_status(self.mapped_status),
            default_status = self.rejected.rejection.status(),
            method = context.method(),
            raw_path = context.raw_path(),
            route = context.route(),
            protocol = negotiated.map(|negotiated| negotiated.protocol().label()),
            content_type = negotiated.and_then(NegotiatedResponseMetadata::content_type),
            subprotocol = negotiated.and_then(NegotiatedResponseMetadata::subprotocol),
            remote_addr = context.remote_addr().map(tracing::field::display),
            cause = %SourceChain(self.rejected.diagnostic.as_ref()),
            "{}",
            delivery.condition()
        );
    }
}

/// A refusal no exit recorded is still an operator's to see.
///
/// The response policy produced can be replaced on the unwind — an outer
/// middleware or gate frame may answer with its own value — and it can be
/// dropped outright when a request ends before its answer is sent. Both leave
/// the refusal with no wire status and no exit to report it, which is exactly
/// the shape "a refusal disposed of silently" names: a dropped value that
/// produces no diagnostic and no failing test. The disposition is stated here
/// instead of inherited, so a refusal that reaches no peer still reaches an
/// operator, with its category, its identity, and its private cause.
///
/// The counter is not reached from here and does not move for these: it is
/// labelled by the status the peer received, and this refusal was not the
/// answer the peer received. The request's own completion event and counter
/// name what was sent instead.
impl Drop for MappedRefusal {
    fn drop(&mut self) {
        match self.recorded {
            true => {}
            false => self.emit(Delivery::Discarded),
        }
    }
}

/// Record what one converted response answered, under the status it carries.
///
/// The single point where a refusal becomes an operator's record: every
/// buffered answer Camber gives is converted here, so a refusal that reaches
/// the wire is recorded exactly once and always with the status sent.
///
/// A refusal that never reaches here is a refusal no peer was answered with —
/// a middleware or gate frame replaced its response on the unwind, or the
/// request ended before it could be sent. It has no sent status to record and
/// no answer to count, so [`MappedRefusal`]'s own drop records it as discarded
/// instead of leaving it silent.
fn sent(response: hyper::Response<Full<Bytes>>, provenance: ResponseProvenance) -> Finalized {
    let refused = provenance.into_refusal().map(|mut mapped| {
        mapped.record(response.status().as_u16());
        mapped.kind()
    });
    Finalized { response, refused }
}

/// Take the identity out of a scope nothing else holds, or copy it when it does.
///
/// A transition runs once per request, before any middleware frame has cloned
/// the scope, so the common case moves the value out and allocates one new
/// share for it. The copy is the honest answer for the rare scope a frame is
/// already holding: the identity it holds names the same request it was given.
fn own_identity(identity: Arc<RequestIdentity>) -> RequestIdentity {
    Arc::try_unwrap(identity).unwrap_or_else(|shared| (*shared).clone())
}

/// One error and everything it was caused by, written as a single line.
///
/// A borrowed view rather than a built string: the chain is walked while the
/// subscriber writes it, so a refusal nobody is recording pays nothing to
/// format one.
///
/// Visible to the rest of the HTTP module because it is the one spelling of
/// this walk. A second copy renders `error: cause: cause` the same way until
/// one of them is corrected, and then two callers disagree about what a cause
/// chain reads as. A caller that needs the text owned calls `to_string` on it;
/// the walk itself is still written once.
pub(super) struct SourceChain<'a>(pub(super) &'a (dyn Error + 'static));

impl fmt::Display for SourceChain<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)?;
        let mut source = self.0.source();
        while let Some(cause) = source {
            write!(f, ": {cause}")?;
            source = cause.source();
        }
        Ok(())
    }
}
