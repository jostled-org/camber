use super::cookie::{self, CookiePairs};
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

/// Borrowed view of request metadata for pre-body classification and gate checks.
///
/// Used before body collection to determine route type (buffered vs streaming)
/// without consuming or buffering the incoming body.
pub(super) struct RequestHead<'a> {
    method: Method,
    uri: &'a hyper::Uri,
    headers: &'a hyper::HeaderMap,
    remote_addr: Option<std::net::IpAddr>,
    is_tls: bool,
}

impl<'a> RequestHead<'a> {
    /// Borrow metadata from a hyper request without consuming it.
    pub(super) fn from_hyper_request(
        req: &'a hyper::Request<hyper::body::Incoming>,
        remote_addr: Option<std::net::IpAddr>,
        is_tls: bool,
    ) -> Option<Self> {
        let method = Method::from_hyper(req.method())?;
        Some(Self {
            method,
            uri: req.uri(),
            headers: req.headers(),
            remote_addr,
            is_tls,
        })
    }

    pub(super) fn method(&self) -> Method {
        self.method
    }

    pub(super) fn path(&self) -> &str {
        self.uri.path()
    }

    pub(super) fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(name)
            .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
    }

    #[cfg(feature = "profiling")]
    pub(super) fn query(&self) -> Option<&str> {
        self.uri.query()
    }

    /// Build a lightweight owned Request for middleware gate checks.
    ///
    /// Clones the URI and HeaderMap from the borrowed references. Only call
    /// this when middleware actually needs to run — skip when no middleware
    /// is registered. Method is already validated by `from_hyper_request`.
    pub(super) fn to_gate_request(&self, params: Option<Params>) -> Request {
        self.to_request(params)
    }

    /// Build a full owned Request from borrowed head metadata with an empty body.
    ///
    /// Used for head-only dispatch paths (internal routes, WebSocket, SSE)
    /// where the request body is not needed.
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
            remote_addr: self.remote_addr,
            is_tls: self.is_tls,
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
}

impl Request {
    /// Convert a hyper request with collected body bytes into a camber Request.
    ///
    /// Returns `None` if the HTTP method is not supported (TRACE, CONNECT, etc.).
    pub(crate) fn from_hyper(
        parts: hyper::http::request::Parts,
        body_bytes: Bytes,
    ) -> Option<Self> {
        let method = Method::from_hyper(&parts.method)?;

        Some(Self {
            method,
            uri: parts.uri,
            raw_headers: parts.headers,
            body_raw: body_bytes,
            body_text: OnceLock::new(),
            params: None,
            query_params: OnceLock::new(),
            form_params: OnceLock::new(),
            cookie_params: OnceLock::new(),
            remote_addr: None,
            is_tls: false,
        })
    }

    pub fn method(&self) -> &'static str {
        self.method.as_str()
    }

    /// True when the original request method is HEAD.
    ///
    /// Handlers can check this to skip expensive body construction.
    /// The runtime strips the body automatically for HEAD responses,
    /// but this method lets handlers avoid building it in the first place.
    pub fn is_head(&self) -> bool {
        matches!(self.method, Method::Head)
    }

    /// Return the parsed HTTP method enum.
    pub(super) fn method_enum(&self) -> Method {
        self.method
    }

    pub fn path(&self) -> &str {
        self.uri.path()
    }

    /// Return the full path and query string from the original URI.
    ///
    /// Used by proxy forwarding to preserve query parameters.
    pub(super) fn raw_path_and_query(&self) -> &str {
        self.uri.path_and_query().map_or("/", |pq| pq.as_str())
    }

    pub fn headers(&self) -> impl Iterator<Item = (&str, &str)> + '_ {
        self.raw_headers.iter().map(|(k, v)| {
            let name = k.as_str();
            let value = std::str::from_utf8(v.as_bytes()).unwrap_or("");
            (name, value)
        })
    }

    /// Return the first header value matching the given name (case-insensitive).
    ///
    /// Uses hyper's O(1) hash lookup — no linear scan.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.raw_headers
            .get(name)
            .and_then(|v| std::str::from_utf8(v.as_bytes()).ok())
    }

    /// Return the remote IP address of the connecting client, if available.
    pub fn remote_addr(&self) -> Option<std::net::IpAddr> {
        self.remote_addr
    }

    pub(crate) fn set_remote_addr(&mut self, addr: std::net::IpAddr) {
        self.remote_addr = Some(addr);
    }

    /// Whether this request arrived over a TLS connection.
    pub(crate) fn is_tls(&self) -> bool {
        self.is_tls
    }

    pub(crate) fn set_tls(&mut self, tls: bool) {
        self.is_tls = tls;
    }

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
    pub fn method(mut self, method: &str) -> Result<Self, RuntimeError> {
        self.method = Method::parse(method).ok_or_else(|| {
            RuntimeError::InvalidArgument(format!("unknown HTTP method: {method}").into_boxed_str())
        })?;
        Ok(self)
    }

    pub fn path(mut self, path: &str) -> Self {
        self.path = path.into();
        self
    }

    pub fn header(mut self, name: &str, value: &str) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    pub fn body(mut self, body: &str) -> Self {
        self.body = Bytes::from(body.to_owned());
        self
    }

    pub fn body_raw(mut self, body: impl Into<Bytes>) -> Self {
        self.body = body.into();
        self
    }

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
        })
    }
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
        Err(_) => String::from_utf8_lossy(scratch).into_owned().into(),
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
