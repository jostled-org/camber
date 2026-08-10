use super::cookie::{self, CookiePairs};
use super::disconnect::DisconnectSignal;
use super::encoding::decode_hex_pair;
use super::method::{Method, RequestMethod};
use super::multipart::{self, MultipartReader};
use super::rejection::RequestId;
use crate::RuntimeError;
use bytes::Bytes;
use serde::de::DeserializeOwned;
use std::fmt;
use std::sync::Arc;
use std::sync::OnceLock;

/// Boxed slice of (name, value) pairs for path parameters.
/// Keys are Arc<str> — shared with the frozen trie, cloned as refcount bumps.
pub(crate) type Params = Box<[(Arc<str>, Box<str>)]>;

/// Boxed slice of (name, value) pairs for parsed URL-encoded data.
type KvPairs = Box<[(Box<str>, Box<str>)]>;

/// The connection-level facts a request carries in from its transport: who the
/// peer is, whether the transport is TLS, the identity Camber minted for this
/// request, and the signal that resolves this response's lifetime.
///
/// [`RequestHead`] names exactly this bundle, so the dispatch paths thread one
/// borrowed value rather than four parallel parameters that can only agree by
/// construction. `Copy` because every field already is: a shared borrow, a
/// `bool`, an `Option<IpAddr>`, and fixed inline identifier storage. Passing it
/// by value therefore costs exactly what passing the fields separately cost,
/// which is the whole argument for `Copy` here.
#[derive(Clone, Copy)]
pub(super) struct RequestOrigin<'a> {
    pub(super) remote_addr: Option<std::net::IpAddr>,
    pub(super) is_tls: bool,
    pub(super) request_id: RequestId,
    /// The HTTP version this request arrived over.
    ///
    /// Read only where the protocol owns the answer — which connection headers
    /// a response may carry, and who enacts a close disposition.
    pub(super) version: hyper::Version,
    pub(super) disconnect: &'a DisconnectSignal,
}

/// Borrowed view of request metadata for pre-body classification and gate checks.
///
/// Used before body collection to determine route type (buffered vs streaming)
/// without consuming or buffering the incoming body.
pub(super) struct RequestHead<'a> {
    method: RequestMethod,
    uri: &'a hyper::Uri,
    headers: &'a hyper::HeaderMap,
    origin: RequestOrigin<'a>,
}

impl<'a> RequestHead<'a> {
    /// Borrow metadata from a hyper request without consuming it.
    ///
    /// Infallible: a method outside Camber's route enum is still a request that
    /// needs classifying, a record, and an answer through the same policy every
    /// other refusal reaches.
    pub(super) fn from_hyper_request(
        req: &'a hyper::Request<hyper::body::Incoming>,
        origin: RequestOrigin<'a>,
    ) -> Self {
        Self {
            method: RequestMethod::from_hyper(req.method()),
            uri: req.uri(),
            headers: req.headers(),
            origin,
        }
    }

    /// The method Camber can route on, when it can route on this one.
    pub(super) fn routable_method(&self) -> Option<Method> {
        self.method.routable()
    }

    pub(super) fn path(&self) -> &str {
        self.uri.path()
    }

    /// The identity Camber minted for this request.
    pub(super) fn request_id(&self) -> RequestId {
        self.origin.request_id
    }

    /// The method exactly as the peer sent it.
    pub(super) fn method_str(&self) -> &str {
        self.method.as_str()
    }

    /// The borrowed head's view of [`Request::header`].
    ///
    /// One lookup rule for the owned request and the borrowed head, so an
    /// admission policy and a handler reading the same name cannot disagree.
    pub(super) fn header(&self, name: &str) -> Option<&str> {
        header_str(self.headers, name)
    }

    /// The body length this request declared, after Hyper's normalization.
    ///
    /// Read from the framing Hyper accepted, never from the raw wire: a
    /// malformed, overflowing, or conflicting declaration is refused below this
    /// service, and an HTTP/1 request carrying both a length and a transfer
    /// coding reaches here with the length already removed. `None` is a body
    /// with no usable declaration, which the observed-byte limit bounds.
    ///
    /// The value stays a `u64`. Narrowing it to the platform word here is what
    /// would turn a declaration no machine can hold into a small admitted
    /// number.
    pub(super) fn declared_length(&self) -> Option<u64> {
        header_str(self.headers, hyper::header::CONTENT_LENGTH.as_str())
            .and_then(|declared| declared.trim().parse().ok())
    }

    /// The transfer coding this request declared, when Camber cannot undo it.
    ///
    /// Hyper accepts any coding chain that ends in `chunked` and decodes only
    /// the chunked framing, so `gzip, chunked` arrives here as chunked frames
    /// whose contents are still compressed. Camber undoes no content coding on
    /// the way in, so what a handler would be handed is not what the peer said
    /// it sent. A chain that does not end in `chunked` is refused below this
    /// service and never reaches here.
    ///
    /// The first value is the one read, under [`Self::header`]'s rule: a peer
    /// that sent `Transfer-Encoding: gzip` and `Transfer-Encoding: chunked` as
    /// two lines declared `gzip` first, and that is the coding this answers
    /// with.
    pub(super) fn undecodable_transfer_coding(&self) -> Option<&str> {
        let declared = header_str(self.headers, hyper::header::TRANSFER_ENCODING.as_str())?;
        match declared
            .split(',')
            .all(|coding| coding.trim().eq_ignore_ascii_case("chunked"))
        {
            true => None,
            false => Some(declared),
        }
    }

    /// The authority this request names, however its version states one.
    pub(super) fn authority(&self) -> &str {
        authority_of(self.headers, self.uri)
    }

    /// The borrowed head's view of [`Request::raw_query`].
    ///
    /// Named for what it returns, so the head and the owned request spell the
    /// raw query the same way. `query` belongs to the keyed decoded lookup on
    /// `Request`, and one concept per name is what keeps a reader from
    /// expecting a decoded value here.
    #[cfg(feature = "profiling")]
    pub(super) fn raw_query(&self) -> Option<&str> {
        self.uri.query()
    }

    /// Build a full owned Request from borrowed head metadata with an empty body.
    ///
    /// Clones the URI and HeaderMap from the borrowed references. Used for
    /// head-only dispatch paths (internal routes, WebSocket, SSE, middleware
    /// gate checks) where the request body is not needed. Method is already
    /// validated by `from_hyper_request`.
    pub(super) fn to_request(&self, params: Option<Params>) -> Request {
        Request {
            method: self.method.clone(),
            uri: self.uri.clone(),
            raw_headers: self.headers.clone(),
            body_raw: Bytes::new(),
            body_text: OnceLock::new(),
            params,
            query_params: OnceLock::new(),
            form_params: OnceLock::new(),
            cookie_params: OnceLock::new(),
            remote_addr: self.origin.remote_addr,
            is_tls: self.origin.is_tls,
            request_id: self.origin.request_id,
            version: self.origin.version,
            disconnect: self.origin.disconnect.clone(),
            _admission: None,
        }
    }
}

/// The authority a request names, however its HTTP version states one.
///
/// HTTP/1 states it in `Host`; HTTP/2 states it in `:authority`, which hyper
/// puts in the URI and writes no header for. One reader for both, so host
/// routing resolves the same service whichever version a peer speaks.
fn authority_of<'a>(headers: &'a hyper::HeaderMap, uri: &'a hyper::Uri) -> &'a str {
    header_str(headers, "host")
        .or_else(|| uri.authority().map(hyper::http::uri::Authority::as_str))
        .unwrap_or("")
}

/// Handler-facing HTTP request. Owns all data.
pub struct Request {
    method: RequestMethod,
    uri: hyper::Uri,
    raw_headers: hyper::HeaderMap,
    body_raw: Bytes,
    body_text: OnceLock<Box<str>>,
    params: Option<Params>,
    query_params: OnceLock<KvPairs>,
    form_params: OnceLock<KvPairs>,
    cookie_params: OnceLock<CookiePairs>,
    remote_addr: Option<std::net::IpAddr>,
    is_tls: bool,
    request_id: RequestId,
    version: hyper::Version,
    disconnect: DisconnectSignal,
    /// The admission permit this request was granted, held for exactly as long
    /// as the request is. Underscored because nothing reads it and nothing may:
    /// its whole contract is that dropping this request releases it once.
    _admission: Option<super::body_admission::BodyPermit>,
}

impl Request {
    /// Convert a hyper request with collected body bytes into a camber Request.
    ///
    /// Takes the whole [`RequestOrigin`], as [`RequestHead::to_request`] does:
    /// all three transport facts are set at construction, so a buffered request
    /// never exists holding a peer address or a TLS flag its caller still owes
    /// it.
    ///
    /// `method` arrives already parsed, from the classification that read this
    /// request's head. Parsing it again here produced a second `None` arm no
    /// request could reach — an unnameable method is answered before a body is
    /// ever collected — and forced every caller to carry a copy of the URI for
    /// a refusal that never fired.
    pub(super) fn from_hyper(
        parts: hyper::http::request::Parts,
        body_bytes: Bytes,
        origin: RequestOrigin<'_>,
        method: Method,
        admission: Option<super::body_admission::BodyPermit>,
    ) -> Self {
        Self {
            method: RequestMethod::Known(method),
            uri: parts.uri,
            raw_headers: parts.headers,
            body_raw: body_bytes,
            body_text: OnceLock::new(),
            params: None,
            query_params: OnceLock::new(),
            form_params: OnceLock::new(),
            cookie_params: OnceLock::new(),
            remote_addr: origin.remote_addr,
            is_tls: origin.is_tls,
            request_id: origin.request_id,
            version: origin.version,
            disconnect: origin.disconnect.clone(),
            _admission: admission,
        }
    }

    /// The identity Camber minted for this request.
    ///
    /// Generated after Hyper accepted the request head and before any method,
    /// host, route, body, middleware, or protocol classification ran, so every
    /// answer this request can get names the same value. An inbound
    /// `X-Request-Id` header is application data a handler may read, never this
    /// value.
    ///
    /// Returned by value, the way the rejection scope returns the same type:
    /// the identifier is `Copy` fixed-size storage, so a borrow buys nothing
    /// and only obliges every caller to dereference it.
    pub fn request_id(&self) -> RequestId {
        self.request_id
    }

    /// Observe this response's lifetime.
    ///
    /// The returned signal resolves once, to the terminal
    /// [`DisconnectCause`](super::DisconnectCause) of this request: the peer
    /// went away, this HTTP/2 stream was reset, the server began shutting
    /// down, or the response finished being produced. Clones share that one
    /// terminal cause, so a producer spawned alongside the handler can hold
    /// its own clone.
    ///
    /// A request built with [`Request::builder`] has no transport, so its
    /// signal never resolves.
    pub fn on_disconnect(&self) -> DisconnectSignal {
        self.disconnect.clone()
    }

    /// Return the HTTP method exactly as the peer sent it.
    ///
    /// Camber routes on a closed set of methods, but a peer may send any token.
    /// This reports what arrived, so a middleware or mapper reading it sees the
    /// request rather than a substitute for it.
    pub fn method(&self) -> &str {
        self.method.as_str()
    }

    /// True when the original request method is HEAD.
    ///
    /// Handlers can check this to skip expensive body construction.
    /// The runtime strips the body automatically for HEAD responses,
    /// but this method lets handlers avoid building it in the first place.
    pub fn is_head(&self) -> bool {
        self.method.routable().is_some_and(method_is_head)
    }

    /// Return the routable HTTP method enum, when this method is one.
    pub(super) fn method_enum(&self) -> Option<Method> {
        self.method.routable()
    }

    /// The method exactly as it arrived, in the form the identity carries.
    pub(super) fn request_method(&self) -> &RequestMethod {
        &self.method
    }

    /// The bounded label this request's tracing span is named by.
    ///
    /// Gated on the one reader that has it: the OpenTelemetry span is built
    /// before a rejection scope exists, so it names the method off the request.
    /// Every other record reads the label off the scope instead, so nothing
    /// outside that feature reaches for this.
    #[cfg(feature = "otel")]
    pub(super) fn method_label(&self) -> &'static str {
        self.method.label()
    }

    /// The HTTP version this request arrived over.
    pub(super) fn http_version(&self) -> hyper::Version {
        self.version
    }

    /// The authority this request names, however its version states one.
    pub(super) fn authority(&self) -> &str {
        authority_of(&self.raw_headers, &self.uri)
    }

    /// Return the request path without the query string.
    pub fn path(&self) -> &str {
        self.uri.path()
    }

    /// Take an owned handle to the request URI.
    ///
    /// `hyper::Uri` is `Bytes`-backed, so this is a refcount bump rather than a
    /// copy of the path. That is what makes it affordable to name a request
    /// before it moves into a producer, on paths where the name is only ever
    /// read if the spawn is refused.
    pub(super) fn uri_owned(&self) -> hyper::Uri {
        self.uri.clone()
    }

    /// Return the full path and query string from the original URI.
    ///
    /// Used by proxy forwarding to preserve query parameters.
    pub(super) fn raw_path_and_query(&self) -> &str {
        self.uri.path_and_query().map_or("/", |pq| pq.as_str())
    }

    /// Iterate over all request headers as `(name, value)` pairs.
    ///
    /// Invalid header values are yielded as empty strings.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.raw_headers
            .iter()
            .map(|(name, value)| (name.as_str(), header_value_str(value).unwrap_or("")))
    }

    /// Return the first header value matching the given name (case-insensitive).
    ///
    /// Uses hyper's O(1) hash lookup — no linear scan.
    pub fn header(&self, name: &str) -> Option<&str> {
        header_str(&self.raw_headers, name)
    }

    /// Return the remote IP address of the connecting client, if available.
    pub fn remote_addr(&self) -> Option<std::net::IpAddr> {
        self.remote_addr
    }

    /// Whether this request arrived over a TLS connection.
    pub(crate) fn is_tls(&self) -> bool {
        self.is_tls
    }

    /// Return the request body as text.
    ///
    /// Invalid UTF-8 is decoded lossily on first access and cached.
    pub fn body(&self) -> &str {
        super::encoding::lossy_text(&self.body_raw, &self.body_text)
    }

    /// Return the raw body bytes.
    pub fn body_bytes(&self) -> &[u8] {
        &self.body_raw
    }

    /// Return a ref-counted handle to the body bytes (zero-copy).
    pub(crate) fn body_raw(&self) -> Bytes {
        self.body_raw.clone()
    }

    /// Deserialize the request body as JSON.
    ///
    /// Fails with [`RuntimeError::MalformedBody`], which carries the parser's
    /// account for operators. Propagating it with `?` from a handler reaches
    /// the router's rejection boundary as a malformed-body refusal; the peer is
    /// answered with fixed safe text.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, RuntimeError> {
        serde_json::from_slice(&self.body_raw)
            .map_err(|e| RuntimeError::MalformedBody(e.to_string().into()))
    }

    /// Parse the request body as multipart/form-data.
    ///
    /// Returns a `MultipartReader` that provides access to all parts.
    /// Fails with [`RuntimeError::Multipart`] when the Content-Type is absent
    /// or the body is not a parseable multipart payload.
    pub fn multipart(&self) -> Result<MultipartReader, RuntimeError> {
        let content_type = self.header("content-type").ok_or_else(|| {
            RuntimeError::Multipart("missing Content-Type header for multipart".into())
        })?;
        multipart::parse(content_type, &self.body_raw)
    }

    /// Return the query string exactly as the peer sent it, without the `?`.
    ///
    /// `None` when the request target carried no `?`, and `Some("")` when it
    /// ended in one. Percent-escape letter case, `+`, `%20`, `=`, and `&` are
    /// all unchanged: this is the representation the URI parser accepted, which
    /// is what a signature check, a generic forwarder, or a protocol that
    /// distinguishes encoded spellings has to read.
    ///
    /// Borrowed from the URI this request already owns. Reading it decodes
    /// nothing and initializes no query state — [`Request::query_pairs`] is the
    /// decoded view.
    pub fn raw_query(&self) -> Option<&str> {
        self.uri.query()
    }

    /// Return the first query parameter value for the given key, or `None`.
    ///
    /// An empty name matches nothing, even where [`Request::query_pairs`]
    /// exposes a blank key the peer sent.
    pub fn query(&self, name: &str) -> Option<&str> {
        self.parsed_query()
            .iter()
            .find(|(key, _)| keyed_match(key, name))
            .map(|(_, value)| value.as_ref())
    }

    /// Return an iterator over all query parameter values for the given key.
    ///
    /// An empty name yields nothing, even where [`Request::query_pairs`]
    /// exposes a blank key the peer sent.
    pub fn query_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.parsed_query()
            .iter()
            .filter(move |(key, _)| keyed_match(key, name))
            .map(|(_, value)| value.as_ref())
    }

    /// Iterate over every decoded query pair in the order the peer sent them.
    ///
    /// Duplicate keys, dotted keys, blank keys, and blank values are all
    /// retained; an empty segment yields no pair. Decoding follows the same
    /// percent, plus, and lossy-UTF-8 rules the keyed helpers use, so a caller
    /// that must distinguish malformed escapes reads [`Request::raw_query`]
    /// instead.
    pub fn query_pairs(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.parsed_query()
            .iter()
            .map(|(key, value)| (key.as_ref(), value.as_ref()))
    }

    /// The decoded query, parsed once.
    ///
    /// Decodes what [`Request::raw_query`] returns, rather than reaching for the
    /// URI a second time: the raw view is what this request accepted, and the
    /// cache is that view decoded. An absent query decodes as an empty one, so
    /// the two targets `raw_query` separates yield the same empty sequence here.
    fn parsed_query(&self) -> &[(Box<str>, Box<str>)] {
        self.query_params.get_or_init(|| {
            parse_urlencoded(self.raw_query().unwrap_or(""), SegmentAdmission::AnyKey)
        })
    }

    /// Return the first form field value for the given key, or `None`.
    ///
    /// Lazily parses the request body as `application/x-www-form-urlencoded`.
    pub fn form(&self, name: &str) -> Option<&str> {
        find_in_pairs(self.parsed_form(), name)
    }

    fn parsed_form(&self) -> &[(Box<str>, Box<str>)] {
        self.form_params
            .get_or_init(|| parse_urlencoded(self.body(), SegmentAdmission::NamedKey))
    }

    /// Return the value of a cookie by name, or `None` if not present.
    ///
    /// Lazily parses the `Cookie` header on first access.
    pub fn cookie(&self, name: &str) -> Option<&str> {
        find_in_pairs(self.parsed_cookies(), name)
    }

    /// Iterate over all cookie name-value pairs.
    ///
    /// Lazily parses the `Cookie` header on first access.
    pub fn cookies(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.parsed_cookies()
            .iter()
            .map(|(k, v)| (k.as_ref(), v.as_ref()))
    }

    fn parsed_cookies(&self) -> &[(Box<str>, Box<str>)] {
        self.cookie_params.get_or_init(|| {
            let header_value = self.header("cookie").unwrap_or("");
            cookie::parse_cookies(header_value)
        })
    }

    /// Look up a path parameter by name.
    pub fn param(&self, name: &str) -> Option<&str> {
        find_in_pairs(self.params.as_ref()?, name)
    }

    pub(crate) fn set_params(&mut self, params: Params) {
        self.params = Some(params);
    }

    /// Create a `RequestBuilder` for constructing test requests.
    pub fn builder() -> RequestBuilder {
        RequestBuilder {
            method: Method::Get,
            path: "/".into(),
            headers: Vec::new(),
            body: Bytes::new(),
        }
    }
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method.as_str())
            .field("path", &self.uri.path())
            .field("header_count", &self.raw_headers.len())
            .field("body_length", &self.body_raw.len())
            .field("remote_addr", &self.remote_addr)
            .finish()
    }
}

/// Builder for constructing `Request` values in tests.
#[derive(Debug)]
pub struct RequestBuilder {
    method: Method,
    path: Box<str>,
    headers: Vec<(Box<str>, Box<str>)>,
    body: Bytes,
}

impl RequestBuilder {
    /// Set the HTTP method.
    ///
    /// Accepts the standard uppercase method names supported by Camber.
    pub fn method(mut self, method: &str) -> Result<Self, RuntimeError> {
        self.method = Method::parse(method).ok_or_else(|| {
            RuntimeError::InvalidArgument(format!("unknown HTTP method: {method}").into_boxed_str())
        })?;
        Ok(self)
    }

    /// Set the request path, including an optional query string.
    pub fn path(mut self, path: &str) -> Self {
        self.path = path.into();
        self
    }

    /// Append a header to the request.
    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Set a UTF-8 request body from text.
    pub fn body(mut self, body: &str) -> Self {
        self.body = Bytes::from(body.to_owned());
        self
    }

    /// Set the raw request body bytes.
    pub fn body_raw(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

    /// Serialize `value` as JSON and set `Content-Type: application/json`.
    ///
    /// The content type replaces any already set, rather than joining it: a
    /// body has one representation, and `finish` appends every header it is
    /// given, so a second line here would build a request stating two.
    pub fn json(mut self, value: &impl serde::Serialize) -> Result<Self, RuntimeError> {
        let serialized = serde_json::to_string(value).map_err(|e| {
            RuntimeError::InvalidArgument(
                format!("json serialization failed: {e}").into_boxed_str(),
            )
        })?;
        self.body = Bytes::from(serialized);
        self.headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
        self.headers
            .push(("Content-Type".into(), "application/json".into()));
        Ok(self)
    }

    /// Build the request value.
    ///
    /// Returns `RuntimeError::InvalidArgument` if the path, header names, or
    /// header values are invalid.
    pub fn finish(self) -> Result<Request, RuntimeError> {
        let uri: hyper::Uri = self
            .path
            .parse()
            .map_err(|e: hyper::http::uri::InvalidUri| {
                RuntimeError::InvalidArgument(format!("invalid path: {e}").into_boxed_str())
            })?;
        let mut header_map = hyper::HeaderMap::with_capacity(self.headers.len());
        for (name, value) in &self.headers {
            let n = hyper::header::HeaderName::from_bytes(name.as_bytes()).map_err(|e| {
                RuntimeError::InvalidArgument(
                    format!("invalid header name \"{name}\": {e}").into_boxed_str(),
                )
            })?;
            let v = hyper::header::HeaderValue::from_str(value).map_err(|e| {
                RuntimeError::InvalidArgument(
                    format!("invalid header value for \"{name}\": {e}").into_boxed_str(),
                )
            })?;
            header_map.append(n, v);
        }
        Ok(Request {
            method: RequestMethod::Known(self.method),
            uri,
            raw_headers: header_map,
            body_raw: self.body,
            body_text: OnceLock::new(),
            params: None,
            query_params: OnceLock::new(),
            form_params: OnceLock::new(),
            cookie_params: OnceLock::new(),
            remote_addr: None,
            is_tls: false,
            // A built request has no transport; the version it reports is the
            // one every other field it carries would have arrived over.
            version: hyper::Version::HTTP_11,
            // Minted through the production generator, so a built request
            // exercises the public accessor rather than a second identity
            // model that could drift from the served one.
            request_id: RequestId::generate(),
            disconnect: DisconnectSignal::detached(),
            // A built request was admitted by nothing, so it holds no permit.
            _admission: None,
        })
    }
}

/// Whether a method is `HEAD`.
///
/// One definition for [`Request::is_head`] and for the two dispatch paths that
/// decide body stripping from a bare [`Method`], before any `Request` exists:
/// internal-route dispatch and the streaming proxy. Three copies of the same
/// `matches!` are three places a HEAD can stop being recognised.
///
/// A free function rather than an inherent method because `method.rs` owns that
/// type's only `impl` block, and a second one elsewhere is the split inherent
/// impl the structural checks reject.
pub(super) fn method_is_head(method: Method) -> bool {
    matches!(method, Method::Head)
}

/// Decode one header value as UTF-8.
///
/// The single decode step behind every header accessor. Only the answer to an
/// undecodable value differs between them: a lookup reports absence, and the
/// iteration over every header yields `""` rather than dropping the name.
fn header_value_str(value: &hyper::header::HeaderValue) -> Option<&str> {
    std::str::from_utf8(value.as_bytes()).ok()
}

/// Look one header up by name, case-insensitively, and decode it.
///
/// One definition for the borrowed [`RequestHead`] and the owned [`Request`]:
/// they read the same map through the same lookup, and only differ in which
/// one of them owns it.
fn header_str<'a>(headers: &'a hyper::HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name).and_then(header_value_str)
}

/// Whether a decoded query pair answers a keyed query lookup for `name`.
///
/// An empty name matches nothing. The decoded query cache retains blank keys so
/// [`Request::query_pairs`] can expose the ones the peer actually sent, and
/// without this rule `query("")` would start answering with the first of them —
/// a value the released keyed helpers never returned.
///
/// The rule is stated once for [`Request::query`] and [`Request::query_all`],
/// and reaches no further. Form and cookie parsing drop blank keys before a
/// lookup can see one, so neither needs it. Route parameters are the one other
/// keyed surface that can hold an empty name — `parse_segments` reads a bare `:`
/// segment as a parameter named `""` — and changing what `param("")` answers is
/// public path access this contract leaves alone.
fn keyed_match(key: &str, name: &str) -> bool {
    !name.is_empty() && key == name
}

/// Find the first value in a slice of key-value pairs where the key matches `name`.
fn find_in_pairs<'a, K: AsRef<str>>(pairs: &'a [(K, Box<str>)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(key, _)| key.as_ref() == name)
        .map(|(_, value)| value.as_ref())
}

/// Which segments a URL-encoded parser keeps.
///
/// Query strings and form bodies decode identically and differ only here, so
/// the policy is a value the one parser reads rather than a second parser with
/// its own copy of the decoding rules. A query keeps `=value` and `=`, because
/// a blank name is part of the identity the peer sent and `query_pairs` has to
/// expose it. A form body names fields, and a field with no name is not one.
#[derive(Clone, Copy)]
enum SegmentAdmission {
    /// Keep every non-empty segment, blank key included.
    AnyKey,
    /// Keep only segments that name a field.
    NamedKey,
}

impl SegmentAdmission {
    fn admits(self, key: &str) -> bool {
        match self {
            Self::AnyKey => true,
            Self::NamedKey => !key.is_empty(),
        }
    }
}

/// Parse a URL-encoded string into key-value pairs with percent-decoding.
///
/// Segments are split structurally before anything is decoded, so an escaped
/// `%26` or `%3D` stays inside its component instead of opening a new pair or
/// key boundary. An empty segment — from an empty input, or from a leading,
/// consecutive, or trailing `&` — yields no pair under either admission policy.
/// A single scratch buffer serves every component, so decoding allocates per
/// retained component rather than per segment.
fn parse_urlencoded(input: &str, admission: SegmentAdmission) -> KvPairs {
    if input.is_empty() {
        return Box::new([]);
    }
    let mut scratch = Vec::with_capacity(input.len());
    input
        .split('&')
        .filter(|segment| !segment.is_empty())
        .filter_map(|segment| decode_segment(segment, admission, &mut scratch))
        .collect()
}

/// Decode one admitted segment into its key and value.
///
/// The segment splits at its first literal `=`; a segment without one has a
/// blank value.
fn decode_segment(
    segment: &str,
    admission: SegmentAdmission,
    scratch: &mut Vec<u8>,
) -> Option<(Box<str>, Box<str>)> {
    let (key, value) = segment.split_once('=').unwrap_or((segment, ""));
    match admission.admits(key) {
        false => None,
        true => {
            let decoded_key = percent_decode_into(key, scratch);
            let decoded_value = percent_decode_into(value, scratch);
            Some((decoded_key, decoded_value))
        }
    }
}

fn percent_decode_into(encoded: &str, scratch: &mut Vec<u8>) -> Box<str> {
    scratch.clear();
    let raw = encoded.as_bytes();
    let mut pos = 0;

    while pos < raw.len() {
        let (byte, advance) = decode_byte(raw, pos);
        scratch.push(byte);
        pos += advance;
    }

    match std::str::from_utf8(scratch) {
        Ok(valid) => Box::from(valid),
        Err(_) => String::from_utf8_lossy(scratch).into(),
    }
}

/// Decode one byte from a percent-encoded sequence. Returns (byte, chars consumed).
fn decode_byte(bytes: &[u8], i: usize) -> (u8, usize) {
    match bytes[i] {
        b'%' if i + 2 < bytes.len() => {
            let decoded = decode_hex_pair(bytes[i + 1], bytes[i + 2]);
            decoded.map_or((b'%', 1), |b| (b, 3))
        }
        b'+' => (b' ', 1),
        b => (b, 1),
    }
}
