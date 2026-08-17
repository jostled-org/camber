use super::body_admission::{AdmittedBody, BodyBudget, BodyPermit};
use super::boundary::DeadlineBoundary;
use super::encoding::decode_hex_pair;
use super::map_reqwest_error;
use super::method::{Method, RequestMethod};
use super::mock::{LifecycleCheckpoint, LifecycleScript};
use super::proxy_upstream::ProxyUpstream;
use super::rejection::{Diagnostic, Rejected, proxy_failure_status};
use super::response::HeaderPair;
use super::transfer::{IncomingSource, Transfer, TransferFailure, TransferOwner};
use crate::RuntimeError;
use arrayvec::ArrayString;
use std::borrow::Cow;
use std::fmt;
use std::future::Future;
use std::sync::Arc;
use tokio::sync::oneshot;

/// The one account of the only path [`strip_prefix`] refuses.
///
/// Shared with the proxied-WebSocket target builder, which raises the identical
/// fault from the identical check: one sentence for one fault, so an operator
/// reading two proxy classes reads the same reason for the same probe.
pub(super) const TRAVERSAL_SEGMENT: &str = "the request path contains a traversal segment";

/// How long an upload gets to confirm it stopped before its answer goes out.
///
/// The acknowledgement arrives only on another poll of the outbound request
/// body, and an upstream that answered without draining makes none — so this
/// bounds scheduling time, not transfer time, and seconds of it are already
/// generous. Unbounded, a head in hand waits for as long as that upstream
/// connection lives.
const UPLOAD_QUIESCE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Check whether a header must not be forwarded between hops.
fn is_hop_by_hop(name: &str) -> bool {
    name.eq_ignore_ascii_case("connection")
        || name.eq_ignore_ascii_case("keep-alive")
        || name.eq_ignore_ascii_case("proxy-authenticate")
        || name.eq_ignore_ascii_case("proxy-authorization")
        || name.eq_ignore_ascii_case("proxy-connection")
        || name.eq_ignore_ascii_case("te")
        || name.eq_ignore_ascii_case("trailer")
        || name.eq_ignore_ascii_case("transfer-encoding")
        || name.eq_ignore_ascii_case("upgrade")
        || name.eq_ignore_ascii_case("host")
}

fn is_rfc_token(value: &[u8]) -> bool {
    !value.is_empty()
        && value.iter().copied().all(|byte| {
            matches!(
                byte,
                b'!' | b'#'
                    | b'$'
                    | b'%'
                    | b'&'
                    | b'\''
                    | b'*'
                    | b'+'
                    | b'-'
                    | b'.'
                    | b'^'
                    | b'_'
                    | b'`'
                    | b'|'
                    | b'~'
                    | b'0'..=b'9'
                    | b'A'..=b'Z'
                    | b'a'..=b'z'
            )
        })
}

/// One header value, in the form the set it came from already holds it.
///
/// Every pass over a header set here needs both forms: the `Connection` token
/// scan reads bytes, and the filter that follows reads text. The sets do not
/// agree on which they hold — a collected request holds text, a hyper map holds
/// bytes — so a single form in the item type charges one of them a conversion
/// it does not owe. Named, each set hands over what it has, and only the byte
/// set converts, once.
#[derive(Clone, Copy)]
enum HeaderValueRef<'a> {
    Text(&'a str),
    Bytes(&'a [u8]),
}

impl<'a> HeaderValueRef<'a> {
    /// The value as bytes. Free from either set.
    fn as_bytes(self) -> &'a [u8] {
        match self {
            Self::Text(text) => text.as_bytes(),
            Self::Bytes(bytes) => bytes,
        }
    }

    /// The value as text, empty where the bytes are not UTF-8.
    ///
    /// An unreadable value reads as empty rather than being dropped, the same
    /// rule `Request::headers` follows: HTTP header values are ASCII, so this
    /// answer is for a value no peer should have sent.
    fn as_str(self) -> &'a str {
        match self {
            Self::Text(text) => text,
            Self::Bytes(bytes) => std::str::from_utf8(bytes).unwrap_or_default(),
        }
    }
}

/// One header on its way between two hops, named and in its own form.
///
/// Materialized rather than iterated twice from its source, because every set
/// here is read twice — once for the `Connection` tokens it delegates, once to
/// filter against them — and neither a collected slice's iterator nor hyper's
/// map iterator is `Clone`.
#[derive(Clone, Copy)]
struct ForwardedHeader<'a> {
    name: &'a str,
    value: HeaderValueRef<'a>,
}

/// The `Connection` tokens one header set delegates to other header names.
fn connection_header_tokens<'a>(headers: &[ForwardedHeader<'a>]) -> Box<[&'a [u8]]> {
    headers
        .iter()
        .filter(|header| header.name.eq_ignore_ascii_case("connection"))
        .flat_map(|header| header.value.as_bytes().split(|byte| *byte == b','))
        .map(|token| token.trim_ascii())
        .filter(|token| is_rfc_token(token))
        .collect()
}

fn is_connection_named(name: &str, connection_tokens: &[&[u8]]) -> bool {
    connection_tokens
        .iter()
        .any(|token| token.eq_ignore_ascii_case(name.as_bytes()))
}

/// Check whether a header is a forwarded-metadata header that Camber sets itself.
/// Client-supplied values must be stripped before Camber adds its own to prevent
/// spoofing (e.g. a client injecting `X-Forwarded-For: 10.0.0.1`).
pub(super) fn is_forwarded_metadata(name: &str) -> bool {
    name.eq_ignore_ascii_case("x-forwarded-for")
        || name.eq_ignore_ascii_case("x-forwarded-host")
        || name.eq_ignore_ascii_case("x-forwarded-proto")
        || name.eq_ignore_ascii_case("x-real-ip")
        || name.eq_ignore_ascii_case("forwarded")
}

pub(super) fn strip_prefix<'a>(path_and_query: &'a str, prefix: &str) -> Option<Cow<'a, str>> {
    let (path, query) = match path_and_query.split_once('?') {
        Some((p, q)) => (p, Some(q)),
        None => (path_and_query, None),
    };
    let remainder = match path.strip_prefix(prefix) {
        Some("") => "/",
        Some(rest) => rest,
        None => path,
    };
    let has_traversal = remainder.split('/').any(is_dot_dot);
    match (has_traversal, query) {
        (true, _) => None,
        (false, Some(q)) => Some(Cow::Owned(format!("{remainder}?{q}"))),
        (false, None) => Some(Cow::Borrowed(remainder)),
    }
}

/// Check whether a path segment is `..` after percent-decoding.
///
/// Catches raw `..`, single-encoded `%2e%2e`, mixed `%2e.`, `.%2e`,
/// and double-encoded variants like `%252e%252e`.
fn is_dot_dot(segment: &str) -> bool {
    let decoded = percent_decode_segment(segment);
    decoded == ".."
}

/// Percent-decode a single path segment. Handles one level of encoding
/// then recurses once to catch double-encoding (`%252e` -> `%2e` -> `.`).
fn percent_decode_segment(input: &str) -> Cow<'_, str> {
    let first_pass = percent_decode_once(input);
    match matches!(first_pass, Cow::Borrowed(_)) {
        true => first_pass,
        false => Cow::Owned(percent_decode_once(first_pass.as_ref()).into_owned()),
    }
}

/// Single pass of percent-decoding over a string.
/// Returns `Cow::Borrowed` when no percent-encoding is present.
fn percent_decode_once(input: &str) -> Cow<'_, str> {
    match input.contains('%') {
        true => {}
        false => return Cow::Borrowed(input),
    }
    let mut result = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let decoded = match bytes[i] {
            b'%' if i + 2 < bytes.len() => decode_hex_pair(bytes[i + 1], bytes[i + 2]),
            _ => None,
        };
        match decoded {
            Some(ch) => {
                result.push(ch as char);
                i += 3;
            }
            None => {
                result.push(bytes[i] as char);
                i += 1;
            }
        }
    }
    Cow::Owned(result)
}

/// Owned data extracted from Request for async forwarding.
/// Owns its data to avoid holding a &Request borrow across .await points.
pub(super) struct ProxyRequest {
    pub(super) method: RequestMethod,
    pub(super) path: Box<str>,
    pub(super) headers: Box<[HeaderPair]>,
    pub(super) body: bytes::Bytes,
    pub(super) remote_addr: Option<std::net::IpAddr>,
    pub(super) scheme: &'static str,
}

/// Every header one collected request forwards.
fn collected_headers(req: &ProxyRequest) -> Box<[ForwardedHeader<'_>]> {
    req.headers
        .iter()
        .map(|(name, value)| ForwardedHeader {
            name: name.as_ref(),
            value: HeaderValueRef::Text(value.as_ref()),
        })
        .collect()
}

impl ProxyRequest {
    pub(super) fn from_request(req: &super::Request) -> Self {
        Self {
            method: req.request_method().clone(),
            path: req.raw_path_and_query().into(),
            headers: req
                .headers()
                .map(|(k, v)| (Cow::Owned(k.to_owned()), Cow::Owned(v.to_owned())))
                .collect(),
            body: req.body_raw(),
            remote_addr: req.remote_addr(),
            scheme: match req.is_tls() {
                true => "https",
                false => "http",
            },
        }
    }
}

/// The reqwest method a forwarded request travels under.
///
/// A method outside Camber's route enum is still the one the peer sent, and a
/// proxy that substituted another would forward a different request. Every
/// unnameable value came from a `hyper::Method`, which is already a validated
/// token, so this conversion does not fail in practice — and where it somehow
/// did, the refusal is reported rather than the method silently rewritten.
fn to_reqwest_method(method: &RequestMethod) -> Result<reqwest::Method, RuntimeError> {
    match method {
        RequestMethod::Known(known) => Ok(routable_reqwest_method(*known)),
        RequestMethod::Unnameable(text) => reqwest::Method::from_bytes(text.as_bytes())
            .map_err(|error| RuntimeError::Http(format!("unforwardable method: {error}").into())),
    }
}

fn routable_reqwest_method(method: Method) -> reqwest::Method {
    match method {
        Method::Get => reqwest::Method::GET,
        Method::Post => reqwest::Method::POST,
        Method::Put => reqwest::Method::PUT,
        Method::Delete => reqwest::Method::DELETE,
        Method::Patch => reqwest::Method::PATCH,
        Method::Head => reqwest::Method::HEAD,
        Method::Options => reqwest::Method::OPTIONS,
    }
}

/// Filter request headers onto a reqwest builder, returning the original Host value if present.
fn filter_request_headers<'a>(
    mut builder: reqwest::RequestBuilder,
    headers: &[ForwardedHeader<'a>],
    connection_tokens: &[&[u8]],
) -> (reqwest::RequestBuilder, Option<&'a str>) {
    let mut original_host = None;
    for header in headers {
        let (name, value) = (header.name, header.value.as_str());
        match (
            is_hop_by_hop(name),
            name.eq_ignore_ascii_case("host"),
            is_forwarded_metadata(name),
            is_connection_named(name, connection_tokens),
        ) {
            (_, true, _, _) => original_host = Some(value),
            (true, _, _, _) | (_, _, true, _) | (_, _, _, true) => {}
            _ => builder = builder.header(name, value),
        }
    }
    (builder, original_host)
}

/// The widest text any `IpAddr` renders as.
///
/// The IPv4-mapped IPv6 form is the longest either version produces: five
/// zero groups and the mapped marker, then a dotted-quad tail. Stated to the
/// compiler below rather than to the reader, so the inline storage the peer
/// address is rendered into cannot be narrowed without the claim failing.
const MAX_IP_TEXT_LEN: usize = 45;

const _: () = assert!(MAX_IP_TEXT_LEN == "0000:0000:0000:0000:0000:ffff:255.255.255.255".len());

/// Attach X-Forwarded-* headers and remote address to a reqwest builder.
fn attach_forwarding_metadata(
    builder: reqwest::RequestBuilder,
    original_host: Option<&str>,
    remote_addr: Option<std::net::IpAddr>,
    scheme: &str,
) -> reqwest::RequestBuilder {
    let hosted = match original_host {
        Some(host) => builder.header("x-forwarded-host", host),
        None => builder,
    };
    let schemed = hosted.header("x-forwarded-proto", scheme);
    match remote_addr {
        Some(addr) => attach_peer_address(schemed, addr),
        None => schemed,
    }
}

/// Name the peer the request arrived from, or name it not at all.
///
/// The storage is exactly [`MAX_IP_TEXT_LEN`] wide and the constant above
/// proves that is every address's width, so the refusal arm is unreachable. It
/// answers by sending neither header rather than by sending both empty: an
/// upstream reading `x-forwarded-for: ` is told this request came from a peer
/// with no address, which is a worse answer than being told nothing.
fn attach_peer_address(
    builder: reqwest::RequestBuilder,
    addr: std::net::IpAddr,
) -> reqwest::RequestBuilder {
    use std::fmt::Write;
    let mut rendered = ArrayString::<MAX_IP_TEXT_LEN>::new();
    match write!(rendered, "{addr}") {
        Ok(()) => builder
            .header("x-forwarded-for", rendered.as_str())
            .header("x-real-ip", rendered.as_str()),
        Err(_) => builder,
    }
}

/// Filter one request's headers onto a builder and add Camber's own metadata.
///
/// Both upstream builders run these three steps in this order — scan
/// `Connection` for the names it delegates, strip what must not travel between
/// hops, then state what Camber knows about the peer — and differ only in where
/// their headers come from. Written twice, one copy can drop a step and forward
/// a header the other strips.
///
/// Values arrive as [`HeaderValueRef`] rather than as bytes or as text, so
/// neither caller pays for the form the other one stores.
fn forward_headers(
    builder: reqwest::RequestBuilder,
    headers: &[ForwardedHeader<'_>],
    remote_addr: Option<std::net::IpAddr>,
    scheme: &str,
) -> reqwest::RequestBuilder {
    let connection_tokens = connection_header_tokens(headers);
    let (builder, original_host) = filter_request_headers(builder, headers, &connection_tokens);
    attach_forwarding_metadata(builder, original_host, remote_addr, scheme)
}

/// Why a proxied request produced no usable upstream answer.
///
/// Three faults with one category and, outside a deadline, one status — kept
/// apart because they are not the same fault. A target this proxy could not
/// build was never reachable to begin with. A request this proxy could not send
/// is Camber's own fault: the shared client would not build, or the method the
/// peer sent could not be carried onto the outbound request. Only the third
/// reached an upstream. Recorded as one, a path-traversal probe and a broken
/// client are indistinguishable from a backend outage.
pub(super) enum ProxyFailure {
    /// This proxy could not build a target to send to.
    ///
    /// A target is built from the peer's path and from the configured backend,
    /// so either can be the fault: a path Camber refuses to forward, or a
    /// backend naming no scheme the proxy class can reach. Both are refused
    /// before anything was sent, which is what they share.
    UnbuildableTarget(&'static str),
    /// Camber could not send the request at all.
    ///
    /// Carries a diagnostic rather than a fixed sentence: both faults in this
    /// class are reported by a library, so neither states its reason in advance.
    Unsendable(Diagnostic),
    /// One phase of the forward failed, naming the phase it failed in.
    ///
    /// The phase is the provenance. Establishing the transport, taking a head,
    /// and waiting for the next body frame are three configured bounds on one
    /// route, and an operator reading one failure has to learn which of them
    /// ended the work — a dead upstream and a slow one are the same status and
    /// nothing else.
    Phase(PhaseFailure),
}

/// The phase of one proxied request a failure belongs to.
///
/// Closed and exhaustively matched: each value is a dimension
/// [`ProxyPolicy`](super::ProxyPolicy) configures separately, so a new phase is
/// a deliberate addition here rather than a string a caller invents.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProxyPhase {
    /// Establishing the upstream transport.
    Connect,
    /// Building and sending the request, up to a usable upstream head.
    Request,
    /// Waiting for the next frame of the upstream answer's body.
    UpstreamIdle,
    /// Carrying this request's own payload to the upstream.
    Upload,
    /// Carrying the upstream answer's payload to the peer.
    Download,
}

impl ProxyPhase {
    /// The bounded name this phase is reported under.
    ///
    /// A closed vocabulary of static text, for the reason
    /// [`DeadlineBoundary`] has one: an operator filters on it, so it can never
    /// become a value derived from a request.
    const fn label(self) -> &'static str {
        match self {
            Self::Connect => "connect",
            Self::Request => "request",
            Self::UpstreamIdle => "upstream idle",
            Self::Upload => "upload",
            Self::Download => "download",
        }
    }
}

impl fmt::Display for ProxyPhase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.label())
    }
}

/// One phase's failure, and the typed cause the phase's own owner minted.
///
/// Both halves are needed. The cause names the bound — a crossed deadline names
/// its [`DeadlineBoundary`], a crossed maximum its
/// [`ByteBoundary`](super::ByteBoundary) — and the phase names the leg of the
/// forward that was running, which is what a transport fault carrying only a
/// library's own account cannot say.
#[derive(Debug)]
pub(super) struct PhaseFailure {
    phase: ProxyPhase,
    cause: RuntimeError,
}

impl PhaseFailure {
    /// The failure one phase reports from the cause its own owner minted.
    const fn new(phase: ProxyPhase, cause: RuntimeError) -> Self {
        Self { phase, cause }
    }

    /// The typed cause this phase failed with.
    pub(super) const fn cause(&self) -> &RuntimeError {
        &self.cause
    }
}

impl fmt::Display for PhaseFailure {
    /// The cause, under the phase that raised it — unless the cause already
    /// names its own bound.
    ///
    /// A crossed deadline or maximum states the boundary it crossed, and every
    /// proxy boundary is named for its phase already: "proxy download failed:
    /// byte limit exceeded: proxy_buffered_response" tells an operator the same
    /// thing twice. A transport fault carries a library's own account and names
    /// nothing, so there the phase is the whole of the provenance.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match names_its_bound(&self.cause) {
            true => fmt::Display::fmt(&self.cause, f),
            false => write!(f, "proxy {} failed: {}", self.phase, self.cause),
        }
    }
}

/// Whether one cause already names the bound it crossed.
fn names_its_bound(cause: &RuntimeError) -> bool {
    matches!(
        cause,
        RuntimeError::DeadlineExceeded(_) | RuntimeError::LimitExceeded(_)
    )
}

// No `source`: the cause is already rendered above, and the recorded diagnostic
// walks the chain — implementing it would print the same sentence twice.
impl std::error::Error for PhaseFailure {}

impl ProxyFailure {
    /// A fault this proxy raised on itself, in the shape a refusal carries it.
    fn unsendable(error: RuntimeError) -> Self {
        Self::Unsendable(Arc::new(error))
    }

    /// One phase's failure, carrying the cause that phase's owner minted.
    const fn phase(phase: ProxyPhase, cause: RuntimeError) -> Self {
        Self::Phase(PhaseFailure::new(phase, cause))
    }

    /// One phase's failure from a deadline that phase configured.
    fn expired(phase: ProxyPhase, boundary: DeadlineBoundary) -> Self {
        Self::phase(phase, RuntimeError::DeadlineExceeded(boundary))
    }
}

impl fmt::Display for ProxyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnbuildableTarget(detail) => f.write_str(detail),
            Self::Unsendable(diagnostic) => fmt::Display::fmt(diagnostic, f),
            Self::Phase(failure) => fmt::Display::fmt(failure, f),
        }
    }
}

/// Create a reqwest builder with URL resolved from path and prefix.
///
/// Shared setup for both buffered and streaming upstream builders:
/// strip prefix, format URL, acquire client, create builder.
///
/// The target is built per request from the peer's own path, so the refusal it
/// raises is about peer input and is classified as such — not as an upstream
/// that failed to answer a request this proxy never sent.
fn upstream_builder(
    method: reqwest::Method,
    path_and_query: &str,
    target: &ProxyTarget<'_>,
) -> Result<reqwest::RequestBuilder, ProxyFailure> {
    let remainder = match strip_prefix(path_and_query, target.prefix) {
        Some(remainder) => remainder,
        None => return Err(ProxyFailure::UnbuildableTarget(TRAVERSAL_SEGMENT)),
    };
    let url = format!("{}{remainder}", target.backend);
    let client = target.upstream.client().map_err(ProxyFailure::unsendable)?;
    Ok(client.request(method, &url))
}

/// Where one forward sends, and the frozen owner it sends through.
///
/// The three travel together because a route froze them together: the backend
/// it forwards to, the prefix it rewrites, and the client and phase deadlines
/// its policy produced. Passed as one value so no caller can pair one route's
/// backend with another route's client.
#[derive(Clone, Copy)]
pub(super) struct ProxyTarget<'a> {
    pub(super) backend: &'a str,
    pub(super) prefix: &'a str,
    pub(super) upstream: &'a ProxyUpstream,
}

/// Build a reqwest builder for upstream forwarding with a buffered body.
fn build_upstream_request(
    req: &ProxyRequest,
    target: &ProxyTarget<'_>,
) -> Result<reqwest::RequestBuilder, ProxyFailure> {
    let method = to_reqwest_method(&req.method).map_err(ProxyFailure::unsendable)?;
    let builder = upstream_builder(method, &req.path, target)?;
    let forwarded = forward_headers(
        builder,
        &collected_headers(req),
        req.remote_addr,
        req.scheme,
    );
    Ok(forwarded.body(req.body.clone()))
}

/// Metadata extracted from a hyper request for streaming proxy forwarding.
pub(super) struct IncomingProxyParts {
    pub(super) method: super::method::Method,
    pub(super) path_and_query: Box<str>,
    pub(super) headers: hyper::HeaderMap,
    pub(super) remote_addr: Option<std::net::IpAddr>,
    pub(super) scheme: &'static str,
}

/// Every header one incoming request forwards.
fn incoming_headers(parts: &IncomingProxyParts) -> Box<[ForwardedHeader<'_>]> {
    parts
        .headers
        .iter()
        .map(|(name, value)| ForwardedHeader {
            name: name.as_str(),
            value: HeaderValueRef::Bytes(value.as_bytes()),
        })
        .collect()
}

/// Build a reqwest builder for upstream forwarding with a streaming incoming body.
fn build_upstream_request_streaming(
    parts: &IncomingProxyParts,
    upload: reqwest::Body,
    target: &ProxyTarget<'_>,
) -> Result<reqwest::RequestBuilder, ProxyFailure> {
    let builder = upstream_builder(
        routable_reqwest_method(parts.method),
        &parts.path_and_query,
        target,
    )?;
    let forwarded = forward_headers(
        builder,
        &incoming_headers(parts),
        parts.remote_addr,
        parts.scheme,
    );

    Ok(forwarded.body(upload))
}

/// Every header one upstream answer arrived with.
fn answered_headers(resp: &reqwest::Response) -> Box<[ForwardedHeader<'_>]> {
    resp.headers()
        .iter()
        .map(|(name, value)| ForwardedHeader {
            name: name.as_str(),
            value: HeaderValueRef::Bytes(value.as_bytes()),
        })
        .collect()
}

/// Collect non-hop-by-hop headers from an upstream response.
fn collect_response_headers(resp: &reqwest::Response) -> Box<[HeaderPair]> {
    let headers = answered_headers(resp);
    let connection_tokens = connection_header_tokens(&headers);
    headers
        .iter()
        .filter(|header| {
            !is_hop_by_hop(header.name) && !is_connection_named(header.name, &connection_tokens)
        })
        .map(|header| {
            (
                Cow::Owned(header.name.to_owned()),
                Cow::Owned(header.value.as_str().to_owned()),
            )
        })
        .collect()
}

/// One upstream answer's head, and the response its body is still inside.
///
/// Taken by the one send every forwarding entry point makes: the three of them
/// differ in how the request was built and what they do with the body, never in
/// how an answer's status and headers are read off it.
struct UpstreamAnswer {
    response: reqwest::Response,
    status: u16,
    headers: Box<[HeaderPair]>,
}

/// Send one built request and take the head of the answer it earns.
///
/// Two phases meet here and are held apart. The request total bounds everything
/// up to a usable head — construction, upload, and the head's arrival — and is
/// enforced here because this is the last owner that can name it. What the send
/// itself raises belongs to whichever phase it happened in: a transport that was
/// never established is the connect phase's, whatever else Reqwest reports is
/// the request phase's.
async fn send_upstream(
    builder: reqwest::RequestBuilder,
    upstream: &ProxyUpstream,
) -> Result<UpstreamAnswer, ProxyFailure> {
    let sending = builder.send();
    let sent = tokio::time::timeout(upstream.request_timeout(), sending)
        .await
        .map_err(|_| ProxyFailure::expired(ProxyPhase::Request, DeadlineBoundary::ProxyRequest))?;
    let response = sent.map_err(sent_failure)?;
    let status = response.status().as_u16();
    let headers = collect_response_headers(&response);
    Ok(UpstreamAnswer {
        response,
        status,
        headers,
    })
}

/// Name the phase one Reqwest send failed in.
///
/// A connect that expired is the one deadline Reqwest owns, so it is reported
/// under the boundary the policy configured rather than as an untyped timeout.
/// Every other fault keeps the phase it happened in.
fn sent_failure(error: reqwest::Error) -> ProxyFailure {
    match (sent_phase(&error), error.is_timeout()) {
        (ProxyPhase::Connect, true) => {
            ProxyFailure::expired(ProxyPhase::Connect, DeadlineBoundary::ProxyConnect)
        }
        (phase, _) => ProxyFailure::phase(phase, map_reqwest_error(error)),
    }
}

/// The phase one Reqwest fault happened in.
///
/// Reqwest states whether a fault reached the transport and whether it came from
/// the request body, which is the whole distinction: a transport that was never
/// established is the connect phase's, a payload that stopped arriving is this
/// request's own upload, and what is left belongs to the request itself.
fn sent_phase(error: &reqwest::Error) -> ProxyPhase {
    match (error.is_connect(), error.is_body()) {
        (true, _) => ProxyPhase::Connect,
        (false, true) => ProxyPhase::Upload,
        (false, false) => ProxyPhase::Request,
    }
}

/// Forward a request to upstream and return a buffered camber Response.
///
/// Proxy routes go through the middleware chain, so middleware can inspect
/// and modify the upstream response (status, headers). The body is fully
/// buffered into the Response.
///
/// The failure keeps its class, because both callers still need it: the
/// middleware terminal maps it through the rejection boundary, where a
/// traversal probe must not be recorded as a backend outage, and
/// [`proxy_forward`] settles a status from the same class alone.
///
/// The buffered maximum this route froze is the frozen owner's, and `None` is
/// its named opt-out. Collection is the shared checked one every buffered
/// Camber consumer performs, so an upstream body this route would refuse is
/// refused before the crossing frame is retained — and the answer the peer gets
/// carries no part of the payload that crossed it. Each of its frames is read
/// under the quiet interval this route configured, so an upstream that commits a
/// head and then stops sending is named as the idle phase rather than waited on.
pub(super) async fn forward_request_buffered(
    req: ProxyRequest,
    target: &ProxyTarget<'_>,
) -> Result<super::Response, ProxyFailure> {
    let builder = build_upstream_request(&req, target)?;
    let UpstreamAnswer {
        response,
        status,
        headers,
    } = send_upstream(builder, target.upstream).await?;
    let body = super::checked_collect::collect_response(
        response,
        super::boundary::ByteBoundary::ProxyBufferedResponse,
        target.upstream.buffered_limit(),
        Some(super::checked_collect::CollectionIdle::new(
            target.upstream.upstream_idle(),
            DeadlineBoundary::ProxyUpstreamIdle,
        )),
    )
    .await
    .map_err(collected_failure)?;
    Ok(headers.iter().fold(
        super::Response::bytes_raw(status, body),
        |answered, (name, value)| answered.with_header(name, value),
    ))
}

/// Name the phase one buffered collection failed in.
///
/// The quiet interval and everything else the answer's body did are two phases
/// on one route: an upstream that stopped sending is the idle phase's, and a
/// body too large or unreadable is the download the route could not carry.
fn collected_failure(error: RuntimeError) -> ProxyFailure {
    match error {
        RuntimeError::DeadlineExceeded(DeadlineBoundary::ProxyUpstreamIdle) => {
            ProxyFailure::phase(ProxyPhase::UpstreamIdle, error)
        }
        error => ProxyFailure::phase(ProxyPhase::Download, error),
    }
}

/// Forward a request to a backend service and return a buffered response.
///
/// Extracts owned data from the request (method, path, headers, body),
/// strips `prefix` from the path, forwards to `backend`, and returns
/// the upstream response with hop-by-hop headers removed.
///
/// The failure is recorded here rather than classified. This entry point
/// answers its caller with a response and holds no rejection scope, so there is
/// nothing to map the failure through: the routed proxy terminal is the path
/// that reaches the boundary. Recording the cause is what keeps it from being
/// dropped with the response an operator never sees a reason for.
///
/// The status is read from the one function that owns the pair, not spelled
/// here: an upstream deadline is a gateway timeout and everything else is a bad
/// gateway, and the CLI's `proxy` plus `root` overlay routes real GET and HEAD
/// traffic through this path, so a third spelling of that rule is user-visible
/// the moment it drifts.
///
/// The upstream is reached under [`ProxyPolicy`]'s documented defaults, which
/// are the same ones a route registered with [`Router::proxy`] freezes. This
/// entry point takes no policy — it is handed a backend and a prefix and nothing
/// else — so the defaults are the only bounds it can name, and a caller who
/// needs others registers the route instead. Because those defaults are the
/// only bounds reachable here, one owner is the whole population: every call in
/// a process shares one default-policy client and the connection pool under it.
/// A route's own owner is frozen with its graph and is never this one.
///
/// [`ProxyPolicy`]: super::ProxyPolicy
/// [`Router::proxy`]: super::Router::proxy
pub fn proxy_forward(
    req: &super::Request,
    backend: &str,
    prefix: &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = super::Response> + Send>> {
    let proxy_req = ProxyRequest::from_request(req);
    let backend: Box<str> = backend.into();
    let prefix: Box<str> = prefix.into();
    let upstream = ProxyUpstream::defaults();
    Box::pin(async move {
        let target = ProxyTarget {
            backend: &backend,
            prefix: &prefix,
            upstream,
        };
        match forward_request_buffered(proxy_req, &target).await {
            Ok(resp) => resp,
            Err(failure) => {
                tracing::warn!(error = %failure, "proxy forward failed");
                let (status, safe) = proxy_failure_status(&failure);
                super::Response::text_raw(status, safe)
            }
        }
    })
}

/// Result of initiating a streaming proxy request.
/// Status and headers are buffered; the body streams via an mpsc channel.
pub(super) struct StreamingProxyResponse {
    pub(super) status: u16,
    pub(super) headers: Box<[HeaderPair]>,
    pub(super) rx: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, super::body::BodyError>>,
    /// What this request's own payload did while the answer was being taken.
    pub(super) upload: UploadDisposition,
}

/// What one request's upload left behind when its answer was settled.
///
/// Read by the stage that puts the answer on the wire, because the two states
/// leave the downstream transport in different conditions: a drained upload
/// owes nothing, and a stopped one leaves framed payload no stage will read.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum UploadDisposition {
    /// The peer's payload ended inside its bound, or there was none to read.
    Drained,
    /// Polling stopped with payload the peer still owed left unread.
    Stopped,
}

/// Spawn a task that streams response body chunks into an mpsc channel.
///
/// Shared between buffered-request and incoming-streaming proxy paths.
///
/// Each frame is read under the quiet interval this route configured, and only
/// the read is: time spent waiting for the downstream peer to take a frame is
/// backpressure, not an upstream that went quiet, and charging it here would end
/// healthy transfers whose reader is simply slower than their producer.
fn spawn_response_streamer(
    resp: reqwest::Response,
    idle: std::time::Duration,
) -> tokio::sync::mpsc::Receiver<Result<bytes::Bytes, super::body::BodyError>> {
    let (tx, rx) = tokio::sync::mpsc::channel(super::DEFAULT_CHANNEL_BUFFER);
    tokio::spawn(async move {
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        loop {
            let read = tokio::select! {
                biased;
                () = tx.closed() => break,
                read = tokio::time::timeout(idle, stream.next()) => read,
            };
            if !forward_stream_read(&tx, read).await {
                break;
            }
        }
    });
    rx
}

/// Hand one read of the upstream body on, or end the transfer with its cause.
async fn forward_stream_read(
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, super::body::BodyError>>,
    read: Result<Option<Result<bytes::Bytes, reqwest::Error>>, tokio::time::error::Elapsed>,
) -> bool {
    match read {
        Ok(result) => forward_stream_result(tx, result).await,
        Err(_) => {
            let expired = RuntimeError::DeadlineExceeded(DeadlineBoundary::ProxyUpstreamIdle);
            tracing::warn!(error = %expired, "proxy upstream body went quiet");
            let _ = tx
                .send(Err(super::body::BodyError::UpstreamProxy(
                    expired.to_string().into(),
                )))
                .await;
            false
        }
    }
}

async fn forward_stream_result(
    tx: &tokio::sync::mpsc::Sender<Result<bytes::Bytes, super::body::BodyError>>,
    result: Option<Result<bytes::Bytes, reqwest::Error>>,
) -> bool {
    match result {
        Some(Ok(bytes)) => tx.send(Ok(bytes)).await.is_ok(),
        Some(Err(error)) => {
            tracing::warn!(error = %error, "proxy upstream body read failed");
            let body_error = super::body::BodyError::UpstreamProxy(error.to_string().into());
            let _ = tx.send(Err(body_error)).await;
            false
        }
        None => false,
    }
}

/// Send one built request and stream its answer back through a channel.
///
/// Both streaming entry points end here: the head is buffered, the body is
/// forwarded chunk by chunk with backpressure through the returned receiver.
/// Stating it once is what keeps a request built from a collected body and one
/// built from a live hyper stream answering the same way.
///
/// The head's arrival is a scheduling checkpoint of its own, taken before the
/// answer's body streamer exists: a case that has to make an upstream answer
/// and an upload's own verdict ready in the same turn needs the head held at
/// the moment it became ready, not after this call has already returned it.
async fn stream_upstream(
    builder: reqwest::RequestBuilder,
    upstream: &ProxyUpstream,
    observer: Option<&Arc<LifecycleScript>>,
) -> Result<StreamingProxyResponse, ProxyFailure> {
    let answer = send_upstream(builder, upstream).await?;
    LifecycleScript::pause_at(
        observer.map(Arc::as_ref),
        LifecycleCheckpoint::StreamingUpstreamHeadReady,
    )
    .await;
    Ok(StreamingProxyResponse {
        status: answer.status,
        headers: answer.headers,
        rx: spawn_response_streamer(answer.response, upstream.upstream_idle()),
        upload: UploadDisposition::Drained,
    })
}

/// Forward a request to upstream and stream the response body via a channel.
///
/// The payload was collected and bounded before this call, so its upload owes
/// nothing to settle: only the answer is still outstanding.
pub(super) async fn forward_request_streaming(
    req: ProxyRequest,
    target: &ProxyTarget<'_>,
) -> Result<StreamingProxyResponse, UploadFailure> {
    let builder = build_upstream_request(&req, target)?;
    Ok(stream_upstream(builder, target.upstream, None).await?)
}

/// Forward an incoming hyper body stream to upstream under its admission.
///
/// The payload is never collected: it is measured frame by frame against the
/// bound this request's own route admitted and forwarded as it arrives. What
/// settles the request is the race between that bound and the upstream's
/// answer, which [`settle_upload`] owns.
///
/// `upload` is this direction's own transfer owner, already narrowed to the
/// deadlines it may enforce: route-aware admission stays the single authority
/// over payload bytes, and transfer policy contributes the quiet interval and
/// the lifetime that authority does not carry.
pub(super) async fn forward_incoming_streaming(
    parts: IncomingProxyParts,
    incoming: hyper::body::Incoming,
    admitted: AdmittedBody,
    upload: TransferOwner,
    target: &ProxyTarget<'_>,
    observer: Option<&Arc<LifecycleScript>>,
) -> Result<StreamingProxyResponse, UploadFailure> {
    let (body, coordinator) = bounded_upload(incoming, admitted, upload, observer);
    let builder = build_upstream_request_streaming(&parts, body, target)?;
    settle_upload(
        coordinator,
        stream_upstream(builder, target.upstream, observer),
    )
    .await
}

/// Why one streaming forward committed no upstream answer.
///
/// Two producers, kept apart because a peer is told different things by each: a
/// proxy fault is the upstream leg's, and a bounded upload's refusal is this
/// request's own — its limit, or the transport it arrived over. Collapsed into
/// one, an oversized upload would be reported to the operator as a backend
/// outage and to the peer as a bad gateway.
pub(super) enum UploadFailure {
    /// The proxy produced no usable upstream answer.
    Proxy(ProxyFailure),
    /// The bounded upload refused this request before anything was committed.
    Body(Rejected),
}

impl From<ProxyFailure> for UploadFailure {
    fn from(failure: ProxyFailure) -> Self {
        Self::Proxy(failure)
    }
}

/// The item a bounded upload hands its upstream request.
type UploadItem = Result<bytes::Bytes, std::io::Error>;

/// The fault an upload reports when it will send no more of a request's payload.
///
/// It carries no reason. The reason travels to the coordinator on its own
/// channel, which is the only stage that answers with it; what the outbound
/// request needs to know is that this body ends without the bytes it declared,
/// so that the upstream sees an aborted request rather than a complete short
/// one.
fn upload_aborted() -> std::io::Error {
    std::io::Error::other("the request body was refused before it finished arriving")
}

/// How one bounded upload ended.
enum UploadEnd {
    /// The peer's payload ended inside its bound.
    Drained,
    /// The coordinator asked this upload to stop.
    Stopped,
    /// This upload refused the request, and this is the refusal.
    Refused(Rejected),
}

/// What one poll of a bounded upload produced.
///
/// No payload-free step: the transfer owner skips trailers and empty frames
/// before this upload sees them, so every step here is a frame to measure or an
/// end to report.
enum UploadStep {
    /// One data frame the bound admitted.
    Forward(bytes::Bytes),
    /// The upload ends here.
    End(UploadEnd),
}

/// One streaming route's incoming payload, measured before it is forwarded.
///
/// The sole owner of the admitted permit from the moment the specialized gate
/// passes the request: every way this value goes away — a drained body, a
/// refusal, a stop, a dropped upstream request, a cancelled request future —
/// releases that permit exactly once.
struct BoundedUpload {
    /// The peer's payload under this direction's own transfer owner.
    ///
    /// The owner carries the quiet interval, the lifetime, and the operation's
    /// cancellation, shutdown, and peer-lifetime authority. It carries no byte
    /// maximum: [`BodyBudget`] below is this request's single payload-byte
    /// authority, and a second counter over the same frames would be a second
    /// authority over the same bytes.
    transfer: Transfer<IncomingSource<hyper::body::Incoming>>,
    budget: BodyBudget,
    permit: Option<BodyPermit>,
    /// The channel a refusal travels on, and the one the coordinator closes to
    /// ask this upload to stop. One handle for both, because they are the same
    /// question from opposite ends: whether anything this upload could still
    /// say would change the answer.
    refusal: Option<oneshot::Sender<Rejected>>,
    /// The acknowledgement this upload owes once it has stopped polling.
    finished: Option<oneshot::Sender<UploadDisposition>>,
    observer: Option<Arc<LifecycleScript>>,
}

/// The half of one bounded upload the request's own coordinator holds.
struct UploadCoordinator {
    refusal: oneshot::Receiver<Rejected>,
    finished: oneshot::Receiver<UploadDisposition>,
    observer: Option<Arc<LifecycleScript>>,
}

/// The senders one upload uses to report its terminal state.
struct UploadSignals {
    refusal: oneshot::Sender<Rejected>,
    finished: oneshot::Sender<UploadDisposition>,
}

/// Split one admitted incoming body into the upload and its coordinator.
fn bounded_upload(
    incoming: hyper::body::Incoming,
    admitted: AdmittedBody,
    upload: TransferOwner,
    observer: Option<&Arc<LifecycleScript>>,
) -> (reqwest::Body, UploadCoordinator) {
    let (signals, coordinator) = upload_coordination(observer);
    match hyper::body::Body::is_end_stream(&incoming) {
        true => (drained_upload(admitted.permit, signals), coordinator),
        false => live_upload(incoming, admitted, upload, signals, coordinator, observer),
    }
}

/// Build the reporting channel pair shared by live and already-ended uploads.
fn upload_coordination(
    observer: Option<&Arc<LifecycleScript>>,
) -> (UploadSignals, UploadCoordinator) {
    let (refusal_tx, refusal_rx) = oneshot::channel();
    let (finished_tx, finished_rx) = oneshot::channel();
    (
        UploadSignals {
            refusal: refusal_tx,
            finished: finished_tx,
        },
        UploadCoordinator {
            refusal: refusal_rx,
            finished: finished_rx,
            observer: observer.map(Arc::clone),
        },
    )
}

/// Complete an upload whose incoming body had already reached end-of-stream.
fn drained_upload(permit: Option<BodyPermit>, signals: UploadSignals) -> reqwest::Body {
    drop(permit);
    drop(signals.refusal);
    acknowledge_end(signals.finished, UploadDisposition::Drained);
    reqwest::Body::from(bytes::Bytes::new())
}

/// Wrap an incoming body that still has frames to poll.
fn live_upload(
    incoming: hyper::body::Incoming,
    admitted: AdmittedBody,
    upload: TransferOwner,
    signals: UploadSignals,
    coordinator: UploadCoordinator,
    observer: Option<&Arc<LifecycleScript>>,
) -> (reqwest::Body, UploadCoordinator) {
    let upload = BoundedUpload {
        budget: BodyBudget::new(&admitted),
        transfer: upload.deadlines_only().over(IncomingSource::new(incoming)),
        permit: admitted.permit,
        refusal: Some(signals.refusal),
        finished: Some(signals.finished),
        observer: observer.map(Arc::clone),
    };
    (
        reqwest::Body::wrap_stream(futures_util::stream::unfold(
            upload,
            BoundedUpload::next_data,
        )),
        coordinator,
    )
}

impl BoundedUpload {
    /// The next payload frame this upload forwards, or the end of it.
    ///
    /// An upload that has already reported its end yields nothing more: the one
    /// acknowledgement it owes has been sent, and a second pass over a body
    /// whose owner has stopped listening would be work with no reader.
    async fn next_data(self) -> Option<(UploadItem, Self)> {
        match self.finished.is_some() {
            false => None,
            true => self.forward_next().await,
        }
    }

    /// Poll until this upload has a frame to forward or an end to report.
    async fn forward_next(mut self) -> Option<(UploadItem, Self)> {
        match self.step().await {
            UploadStep::Forward(data) => Some((Ok(data), self)),
            UploadStep::End(end) => self.ended(end),
        }
    }

    /// Take one frame from the peer, unless the coordinator asks to stop first.
    ///
    /// Biased on the stop: an answer the coordinator has already taken settles
    /// this request, so a frame ready in the same turn is one no stage will
    /// ever look at.
    async fn step(&mut self) -> UploadStep {
        let Self {
            transfer, refusal, ..
        } = self;
        let read = tokio::select! {
            biased;
            () = stop_requested(refusal.as_mut()) => return UploadStep::End(UploadEnd::Stopped),
            read = transfer.frame() => read,
        };
        self.measure(read).await
    }

    /// Measure one delivered frame against the bound this request was admitted
    /// under.
    ///
    /// Only payload arrives here: the transfer owner skips trailers and empty
    /// frames, so neither is counted and neither restarts a quiet interval.
    async fn measure(&mut self, read: Result<Option<bytes::Bytes>, TransferFailure>) -> UploadStep {
        let data = match read {
            Ok(None) => return UploadStep::End(UploadEnd::Drained),
            Err(failure) => return UploadStep::End(transfer_end(&failure)),
            Ok(Some(data)) => data,
        };
        LifecycleScript::count_body_frame(self.observer.as_deref());
        self.admit(data).await
    }

    /// Forward one data frame, or refuse the frame that would cross the bound.
    async fn admit(&mut self, data: bytes::Bytes) -> UploadStep {
        match self.budget.admit_frame(data.len()) {
            Ok(_) => UploadStep::Forward(data),
            Err(rejected) => self.crossed(rejected).await,
        }
    }

    /// Hold one crossing at its checkpoint, then end this upload with it.
    ///
    /// Ordered here rather than where the refusal is sent: a case whose claim is
    /// how one coordinator turn settles this request's own bound against an
    /// upstream answer needs the crossing observed and not yet reported, so both
    /// results can be staged before either is said.
    async fn crossed(&self, rejected: Rejected) -> UploadStep {
        LifecycleScript::pause_at(
            self.observer.as_deref(),
            LifecycleCheckpoint::RequestBodyLimitObserved,
        )
        .await;
        UploadStep::End(UploadEnd::Refused(rejected))
    }

    /// Report how this upload ended, and tell its consumer what to do next.
    ///
    /// A refusal leaves as a stream error rather than as an end of body: an
    /// upstream given a clean end would take a truncated request for a complete
    /// one, which is the request Camber refused to forward.
    fn ended(mut self, end: UploadEnd) -> Option<(UploadItem, Self)> {
        let aborted = matches!(end, UploadEnd::Refused(_));
        self.report(end);
        match aborted {
            true => Some((Err(upload_aborted()), self)),
            false => None,
        }
    }

    /// End this upload once: release the permit, then answer the coordinator.
    ///
    /// Both handles are taken whatever the ending was. A refusal travels out on
    /// the first; an upload that raised none closes it, which is the same
    /// answer to the coordinator's only question about it.
    fn report(&mut self, end: UploadEnd) {
        drop(self.permit.take());
        let disposition = match &end {
            UploadEnd::Drained => UploadDisposition::Drained,
            UploadEnd::Stopped | UploadEnd::Refused(_) => UploadDisposition::Stopped,
        };
        match self.refusal.take().zip(refusal_of(end)) {
            Some((sender, rejected)) => raise_refusal(sender, rejected),
            None => {}
        }
        match self.finished.take() {
            Some(sender) => acknowledge_end(sender, disposition),
            None => {}
        }
    }
}

/// Hand one upload's refusal to the coordinator, or say that nothing heard it.
///
/// A closed channel means the upstream's answer already won the tie: the peer
/// is told the upstream's status and never learns its upload crossed the bound.
/// The log is the operator's only record that the limit fired at all.
fn raise_refusal(sender: oneshot::Sender<Rejected>, rejected: Rejected) {
    match sender.send(rejected) {
        Ok(()) => {}
        Err(_) => tracing::warn!(
            "request body limit refused an upload after its answer was already taken"
        ),
    }
}

/// Tell the coordinator this upload has stopped, if it is still listening.
fn acknowledge_end(sender: oneshot::Sender<UploadDisposition>, disposition: UploadDisposition) {
    match sender.send(disposition) {
        // Both outcomes leave nothing owed: a coordinator that is gone settled
        // this request without waiting, and it already assumes what an unheard
        // disposition would have told it.
        Ok(()) | Err(_) => {}
    }
}

/// The refusal one ending carries, if it carries one.
fn refusal_of(end: UploadEnd) -> Option<Rejected> {
    match end {
        UploadEnd::Refused(rejected) => Some(rejected),
        UploadEnd::Drained | UploadEnd::Stopped => None,
    }
}

/// How one upload ends when its transfer owner fixed a terminal.
///
/// The owner that measured the bound is the one that mints the refusal naming
/// it, so nothing is restated here. A terminal the declared table gives no
/// refusal — a shutdown, a forced cancellation, a peer whose lifetime ended, or
/// a request deadline the coordinator answers — stops this upload without one:
/// its answer belongs to the stage that selected it, not to the body.
fn transfer_end(failure: &TransferFailure) -> UploadEnd {
    match failure.refusal() {
        Some(rejected) => UploadEnd::Refused(rejected),
        None => UploadEnd::Stopped,
    }
}

/// Whether the coordinator has asked this upload to stop.
///
/// It asks by closing the channel a refusal would travel on. Once an answer has
/// been taken, nothing this upload could still say would change it, so the way
/// to stop it is to stop listening. An upload that has already reported waits
/// here forever, which is the poll its caller never makes.
async fn stop_requested(refusal: Option<&mut oneshot::Sender<Rejected>>) {
    match refusal {
        Some(sender) => sender.closed().await,
        None => std::future::pending().await,
    }
}

/// The refusal one upload raises, if it raises one.
///
/// A channel that closes with nothing on it is an upload that ended inside its
/// bound: nothing is owed, so this never resolves, and the answer the
/// coordinator is still waiting for is the upstream's.
async fn upload_refusal(refusal: &mut oneshot::Receiver<Rejected>) -> Rejected {
    match refusal.await {
        Ok(rejected) => rejected,
        Err(_) => std::future::pending().await,
    }
}

/// Settle one streaming request between its own bound and the upstream's answer.
///
/// Biased on the bound. Both results can become ready in the same turn — a
/// crossing frame and a response head — and the tie is decided for the refusal,
/// because the alternative commits an answer to a request this route already
/// refused to forward in full.
async fn settle_upload(
    mut coordinator: UploadCoordinator,
    upstream: impl Future<Output = Result<StreamingProxyResponse, ProxyFailure>>,
) -> Result<StreamingProxyResponse, UploadFailure> {
    tokio::pin!(upstream);
    let answered = tokio::select! {
        biased;
        rejected = upload_refusal(&mut coordinator.refusal) => {
            return Err(UploadFailure::Body(rejected));
        }
        answered = &mut upstream => answered,
    };
    let answered = resolve_upstream_result(&mut coordinator, answered)?;
    coordinator.commit(answered).await
}

/// Prefer a refusal that caused the upstream request itself to fail.
fn resolve_upstream_result(
    coordinator: &mut UploadCoordinator,
    answered: Result<StreamingProxyResponse, ProxyFailure>,
) -> Result<StreamingProxyResponse, UploadFailure> {
    answered.map_err(|failure| match coordinator.refusal.try_recv() {
        Ok(rejected) => UploadFailure::Body(rejected),
        Err(_) => UploadFailure::Proxy(failure),
    })
}

impl UploadCoordinator {
    /// Stop this request's upload, then let the upstream's answer through.
    async fn commit(
        mut self,
        answered: StreamingProxyResponse,
    ) -> Result<StreamingProxyResponse, UploadFailure> {
        let upload = self.quiesce().await;
        LifecycleScript::pause_at(
            self.observer.as_deref(),
            LifecycleCheckpoint::BeforeStreamingResponseCommit,
        )
        .await;
        Ok(StreamingProxyResponse { upload, ..answered })
    }

    /// Ask the upload to stop, and wait until it says it has.
    ///
    /// Waited for rather than assumed. The answer is already decided by this
    /// point, so an upload still polling frames behind it could raise a limit
    /// nothing would act on; stopping it before the head reaches the peer is
    /// what makes that state unreachable rather than merely unlikely.
    ///
    /// The wait is bounded by [`UPLOAD_QUIESCE_TIMEOUT`], because an upstream
    /// that answers and then stops reading never polls the body that owes the
    /// acknowledgement: the answer would be held undelivered for the life of
    /// that upstream connection. An upload that did not confirm reads as
    /// [`UploadDisposition::Stopped`] — the conservative answer, which forces
    /// the close unread payload requires.
    async fn quiesce(&mut self) -> UploadDisposition {
        self.refusal.close();
        let disposition =
            match tokio::time::timeout(UPLOAD_QUIESCE_TIMEOUT, &mut self.finished).await {
                Ok(Ok(disposition)) => disposition,
                Ok(Err(_)) | Err(_) => UploadDisposition::Stopped,
            };
        LifecycleScript::pause_at(
            self.observer.as_deref(),
            LifecycleCheckpoint::StreamingUploadQuiesced,
        )
        .await;
        disposition
    }
}
