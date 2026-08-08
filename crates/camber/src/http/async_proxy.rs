use super::encoding::decode_hex_pair;
use super::map_reqwest_error;
use super::method::{Method, RequestMethod};
use super::rejection::{Diagnostic, proxy_failure_status};
use super::response::HeaderPair;
use crate::RuntimeError;
use arrayvec::ArrayString;
use std::borrow::Cow;
use std::fmt;
use std::sync::{Arc, LazyLock};

/// The one account of the only path [`strip_prefix`] refuses.
///
/// Shared with the proxied-WebSocket target builder, which raises the identical
/// fault from the identical check: one sentence for one fault, so an operator
/// reading two proxy classes reads the same reason for the same probe.
pub(super) const TRAVERSAL_SEGMENT: &str = "the request path contains a traversal segment";

static PROXY_CLIENT: LazyLock<Result<reqwest::Client, Arc<str>>> = LazyLock::new(|| {
    reqwest::Client::builder()
        .no_proxy()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| -> Arc<str> { e.to_string().into() })
});

pub(crate) fn proxy_client() -> Result<&'static reqwest::Client, RuntimeError> {
    PROXY_CLIENT
        .as_ref()
        .map_err(|e| RuntimeError::Http(Arc::clone(e)))
}

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

fn connection_header_tokens<'a>(
    headers: impl Iterator<Item = (&'a str, HeaderValueRef<'a>)>,
) -> Box<[&'a [u8]]> {
    headers
        .filter(|(name, _)| name.eq_ignore_ascii_case("connection"))
        .flat_map(|(_, value)| value.as_bytes().split(|byte| *byte == b','))
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
    headers: impl Iterator<Item = (&'a str, &'a str)>,
    connection_tokens: &[&[u8]],
) -> (reqwest::RequestBuilder, Option<&'a str>) {
    let mut original_host = None;
    for (name, value) in headers {
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
/// The source is taken as a way to start a pass rather than as a pass already
/// running: the set is read twice, once to collect the `Connection` tokens and
/// once to filter against them, and neither the collected slice's iterator nor
/// hyper's map iterator is `Clone`.
///
/// Values arrive as [`HeaderValueRef`] rather than as bytes or as text, so
/// neither caller pays for the form the other one stores.
fn forward_headers<'a, I>(
    builder: reqwest::RequestBuilder,
    headers: impl Fn() -> I,
    remote_addr: Option<std::net::IpAddr>,
    scheme: &str,
) -> reqwest::RequestBuilder
where
    I: Iterator<Item = (&'a str, HeaderValueRef<'a>)>,
{
    let connection_tokens = connection_header_tokens(headers());
    let readable = headers().map(|(name, value)| (name, value.as_str()));
    let (builder, original_host) = filter_request_headers(builder, readable, &connection_tokens);
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
    /// No usable upstream answer arrived.
    ///
    /// Either no response head arrived, or a head arrived whose body could not
    /// be read. Both leave the caller nothing to forward, and an expired
    /// deadline reads the same on either, so both answer as one fault.
    Upstream(RuntimeError),
}

impl From<RuntimeError> for ProxyFailure {
    fn from(error: RuntimeError) -> Self {
        Self::Upstream(error)
    }
}

impl ProxyFailure {
    /// A fault this proxy raised on itself, in the shape a refusal carries it.
    fn unsendable(error: RuntimeError) -> Self {
        Self::Unsendable(Arc::new(error))
    }
}

impl fmt::Display for ProxyFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnbuildableTarget(detail) => f.write_str(detail),
            Self::Unsendable(diagnostic) => fmt::Display::fmt(diagnostic, f),
            Self::Upstream(error) => fmt::Display::fmt(error, f),
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
    backend: &str,
    prefix: &str,
) -> Result<reqwest::RequestBuilder, ProxyFailure> {
    let remainder = match strip_prefix(path_and_query, prefix) {
        Some(remainder) => remainder,
        None => return Err(ProxyFailure::UnbuildableTarget(TRAVERSAL_SEGMENT)),
    };
    let url = format!("{backend}{remainder}");
    let client = proxy_client().map_err(ProxyFailure::unsendable)?;
    Ok(client.request(method, &url))
}

/// Build a reqwest builder for upstream forwarding with a buffered body.
fn build_upstream_request(
    req: &ProxyRequest,
    backend: &str,
    prefix: &str,
) -> Result<reqwest::RequestBuilder, ProxyFailure> {
    let method = to_reqwest_method(&req.method).map_err(ProxyFailure::unsendable)?;
    let builder = upstream_builder(method, &req.path, backend, prefix)?;
    let forwarded = forward_headers(
        builder,
        || {
            req.headers
                .iter()
                .map(|(name, value)| (name.as_ref(), HeaderValueRef::Text(value.as_ref())))
        },
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

/// Build a reqwest builder for upstream forwarding with a streaming incoming body.
fn build_upstream_request_streaming(
    parts: &IncomingProxyParts,
    incoming: hyper::body::Incoming,
    backend: &str,
    prefix: &str,
) -> Result<reqwest::RequestBuilder, ProxyFailure> {
    let builder = upstream_builder(
        routable_reqwest_method(parts.method),
        &parts.path_and_query,
        backend,
        prefix,
    )?;
    let forwarded = forward_headers(
        builder,
        || {
            parts
                .headers
                .iter()
                .map(|(name, value)| (name.as_str(), HeaderValueRef::Bytes(value.as_bytes())))
        },
        parts.remote_addr,
        parts.scheme,
    );

    use futures_util::StreamExt;
    let body_stream = http_body_util::BodyStream::new(incoming).filter_map(|result| async move {
        match result {
            Ok(frame) => frame.into_data().ok().map(Ok),
            Err(e) => Some(Err(e)),
        }
    });

    Ok(forwarded.body(reqwest::Body::wrap_stream(body_stream)))
}

/// Collect non-hop-by-hop headers from an upstream response.
fn collect_response_headers(resp: &reqwest::Response) -> Box<[HeaderPair]> {
    let connection_tokens = connection_header_tokens(
        resp.headers()
            .iter()
            .map(|(name, value)| (name.as_str(), HeaderValueRef::Bytes(value.as_bytes()))),
    );
    resp.headers()
        .iter()
        .filter(|(name, _)| {
            !is_hop_by_hop(name.as_str()) && !is_connection_named(name.as_str(), &connection_tokens)
        })
        .map(|(name, value)| {
            (
                Cow::Owned(name.as_str().to_owned()),
                Cow::Owned(value.to_str().unwrap_or_default().to_owned()),
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
async fn send_upstream(builder: reqwest::RequestBuilder) -> Result<UpstreamAnswer, ProxyFailure> {
    let response = builder.send().await.map_err(map_reqwest_error)?;
    let status = response.status().as_u16();
    let headers = collect_response_headers(&response);
    Ok(UpstreamAnswer {
        response,
        status,
        headers,
    })
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
pub(super) async fn forward_request_buffered(
    req: ProxyRequest,
    backend: &str,
    prefix: &str,
) -> Result<super::Response, ProxyFailure> {
    let builder = build_upstream_request(&req, backend, prefix)?;
    let UpstreamAnswer {
        response,
        status,
        headers,
    } = send_upstream(builder).await?;
    let body = response.bytes().await.map_err(map_reqwest_error)?;
    Ok(headers.iter().fold(
        super::Response::bytes_raw(status, body),
        |answered, (name, value)| answered.with_header(name, value),
    ))
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
pub fn proxy_forward(
    req: &super::Request,
    backend: &str,
    prefix: &str,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = super::Response> + Send>> {
    let proxy_req = ProxyRequest::from_request(req);
    let backend: Box<str> = backend.into();
    let prefix: Box<str> = prefix.into();
    Box::pin(async move {
        match forward_request_buffered(proxy_req, &backend, &prefix).await {
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
}

/// Spawn a task that streams response body chunks into an mpsc channel.
///
/// Shared between buffered-request and incoming-streaming proxy paths.
fn spawn_response_streamer(
    resp: reqwest::Response,
) -> tokio::sync::mpsc::Receiver<Result<bytes::Bytes, super::body::BodyError>> {
    let (tx, rx) = tokio::sync::mpsc::channel(super::DEFAULT_CHANNEL_BUFFER);
    tokio::spawn(async move {
        use futures_util::StreamExt;
        let mut stream = resp.bytes_stream();
        loop {
            let result = tokio::select! {
                biased;
                () = tx.closed() => break,
                result = stream.next() => result,
            };
            if !forward_stream_result(&tx, result).await {
                break;
            }
        }
    });
    rx
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
async fn stream_upstream(
    builder: reqwest::RequestBuilder,
) -> Result<StreamingProxyResponse, ProxyFailure> {
    let answer = send_upstream(builder).await?;
    Ok(StreamingProxyResponse {
        status: answer.status,
        headers: answer.headers,
        rx: spawn_response_streamer(answer.response),
    })
}

/// Forward a request to upstream and stream the response body via a channel.
pub(super) async fn forward_request_streaming(
    req: ProxyRequest,
    backend: &str,
    prefix: &str,
) -> Result<StreamingProxyResponse, ProxyFailure> {
    stream_upstream(build_upstream_request(&req, backend, prefix)?).await
}

/// Forward an incoming hyper body stream to upstream without buffering.
///
/// The request body is streamed directly from the client to upstream,
/// bypassing the router's max_request_body limit. The response body
/// is streamed back via an mpsc channel.
pub(super) async fn forward_incoming_streaming(
    parts: IncomingProxyParts,
    incoming: hyper::body::Incoming,
    backend: &str,
    prefix: &str,
) -> Result<StreamingProxyResponse, ProxyFailure> {
    stream_upstream(build_upstream_request_streaming(
        &parts, incoming, backend, prefix,
    )?)
    .await
}
