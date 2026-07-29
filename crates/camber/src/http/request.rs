use super::cookie::{self, CookiePairs};
use super::disconnect::DisconnectSignal;
use super::encoding::decode_hex_pair;
use super::method::Method;
use super::multipart::{self, MultipartReader};
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
/// peer is, whether the transport is TLS, and the signal that resolves this
/// response's lifetime.
///
/// [`RequestHead`] names exactly this triple, so the dispatch paths thread one
/// borrowed bundle rather than three parallel parameters that can only agree by
/// construction. `Copy` because every field already is: a shared borrow, a
/// `bool`, and an `Option<IpAddr>` — about 32 bytes once padded, not a pointer
/// and two flags. Passing it by value therefore costs exactly what passing the
/// three fields separately cost, which is the whole argument for `Copy` here.
#[derive(Clone, Copy)]
pub(super) struct RequestOrigin<'a> {
    pub(super) remote_addr: Option<std::net::IpAddr>,
    pub(super) is_tls: bool,
    pub(super) disconnect: &'a DisconnectSignal,
}

/// Borrowed view of request metadata for pre-body classification and gate checks.
///
/// Used before body collection to determine route type (buffered vs streaming)
/// without consuming or buffering the incoming body.
pub(super) struct RequestHead<'a> {
    method: Method,
    uri: &'a hyper::Uri,
    headers: &'a hyper::HeaderMap,
    origin: RequestOrigin<'a>,
}

impl<'a> RequestHead<'a> {
    /// Borrow metadata from a hyper request without consuming it.
    pub(super) fn from_hyper_request(
        req: &'a hyper::Request<hyper::body::Incoming>,
        origin: RequestOrigin<'a>,
    ) -> Option<Self> {
        let method = Method::from_hyper(req.method())?;
        Some(Self {
            method,
            uri: req.uri(),
            headers: req.headers(),
            origin,
        })
    }

    pub(super) fn method(&self) -> Method {
        self.method
    }

    pub(super) fn path(&self) -> &str {
        self.uri.path()
    }

    pub(super) fn header(&self, name: &str) -> Option<&str> {
        header_str(self.headers, name)
    }

    #[cfg(feature = "profiling")]
    pub(super) fn query(&self) -> Option<&str> {
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
            method: self.method,
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
            disconnect: self.origin.disconnect.clone(),
        }
    }
}

/// Handler-facing HTTP request. Owns all data.
pub struct Request {
    method: Method,
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
    disconnect: DisconnectSignal,
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
    ) -> Self {
        Self {
            method,
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
            disconnect: origin.disconnect.clone(),
        }
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

    /// Return the uppercase HTTP method string.
    pub fn method(&self) -> &'static str {
        self.method.as_str()
    }

    /// True when the original request method is HEAD.
    ///
    /// Handlers can check this to skip expensive body construction.
    /// The runtime strips the body automatically for HEAD responses,
    /// but this method lets handlers avoid building it in the first place.
    pub fn is_head(&self) -> bool {
        method_is_head(self.method)
    }

    /// Return the parsed HTTP method enum.
    pub(super) fn method_enum(&self) -> Method {
        self.method
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
        match std::str::from_utf8(&self.body_raw) {
            Ok(text) => text,
            Err(_) => self
                .body_text
                .get_or_init(|| String::from_utf8_lossy(&self.body_raw).into()),
        }
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
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, RuntimeError> {
        serde_json::from_slice(&self.body_raw)
            .map_err(|e| RuntimeError::BadRequest(e.to_string().into()))
    }

    /// Parse the request body as multipart/form-data.
    ///
    /// Returns a `MultipartReader` that provides access to all parts.
    /// Fails with `BadRequest` if the Content-Type is not multipart/form-data.
    pub fn multipart(&self) -> Result<MultipartReader, RuntimeError> {
        let content_type = self.header("content-type").ok_or_else(|| {
            RuntimeError::BadRequest("missing Content-Type header for multipart".into())
        })?;
        multipart::parse(content_type, &self.body_raw)
    }

    /// Return the first query parameter value for the given key, or `None`.
    pub fn query(&self, name: &str) -> Option<&str> {
        find_in_pairs(self.parsed_query(), name)
    }

    /// Return an iterator over all query parameter values for the given key.
    pub fn query_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.parsed_query()
            .iter()
            .filter(move |(k, _)| k.as_ref() == name)
            .map(|(_, v)| v.as_ref())
    }

    fn parsed_query(&self) -> &[(Box<str>, Box<str>)] {
        self.query_params
            .get_or_init(|| parse_urlencoded(self.uri.query().unwrap_or("")))
    }

    /// Return the first form field value for the given key, or `None`.
    ///
    /// Lazily parses the request body as `application/x-www-form-urlencoded`.
    pub fn form(&self, name: &str) -> Option<&str> {
        find_in_pairs(self.parsed_form(), name)
    }

    fn parsed_form(&self) -> &[(Box<str>, Box<str>)] {
        self.form_params
            .get_or_init(|| parse_urlencoded(self.body()))
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
    pub fn json(mut self, value: &impl serde::Serialize) -> Result<Self, RuntimeError> {
        let serialized = serde_json::to_string(value).map_err(|e| {
            RuntimeError::InvalidArgument(
                format!("json serialization failed: {e}").into_boxed_str(),
            )
        })?;
        self.body = Bytes::from(serialized);
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
            method: self.method,
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
            disconnect: DisconnectSignal::detached(),
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

/// Find the first value in a slice of key-value pairs where the key matches `name`.
fn find_in_pairs<'a, K: AsRef<str>>(pairs: &'a [(K, Box<str>)], name: &str) -> Option<&'a str> {
    pairs
        .iter()
        .find(|(k, _)| k.as_ref() == name)
        .map(|(_, v)| v.as_ref())
}

/// Parse a URL-encoded string into key-value pairs with percent-decoding.
/// Uses a single scratch buffer across all pairs to avoid per-pair allocation.
pub(crate) fn parse_urlencoded(input: &str) -> Box<[(Box<str>, Box<str>)]> {
    if input.is_empty() {
        return Box::new([]);
    }
    let mut scratch = Vec::with_capacity(input.len());
    input
        .split('&')
        .filter_map(|pair| parse_urlencoded_pair(pair, &mut scratch))
        .collect()
}

fn parse_urlencoded_pair(pair: &str, scratch: &mut Vec<u8>) -> Option<(Box<str>, Box<str>)> {
    let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
    match key.is_empty() {
        true => None,
        false => {
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
