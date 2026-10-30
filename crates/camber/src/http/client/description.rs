//! One validated request description: what a call sends, and the rules its
//! parts were admitted under.
//!
//! Every rule here reads a value and touches no network. The method, target,
//! and header grammars are the `http` and `url` crates' own parsers, so this
//! module names which parsed values Camber refuses and adds no grammar.

use super::error::{ClientError, ClientInput};
use super::trace::{TRACEPARENT, entry_traceparent};
use crate::RuntimeError;
use bytes::Bytes;
use reqwest::header::{
    AUTHORIZATION, CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, HeaderMap, HeaderName,
    HeaderValue, PROXY_AUTHORIZATION, TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use reqwest::{Method, Url};

/// The headers the transport writes. An application value for one would
/// disagree with the connection that carries it.
const TRANSPORT_OWNED: [HeaderName; 8] = [
    HOST,
    CONTENT_LENGTH,
    TRANSFER_ENCODING,
    CONNECTION,
    UPGRADE,
    TE,
    TRAILER,
    PROXY_AUTHORIZATION,
];

/// The headers that are credentials whichever setter wrote them.
const ALWAYS_SENSITIVE: [HeaderName; 2] = [AUTHORIZATION, COOKIE];

/// The refusal one invalid part of a description returns.
fn refused(part: ClientInput) -> RuntimeError {
    RuntimeError::Client(ClientError::InvalidRequest(part))
}

/// The method `token` names.
///
/// Any HTTP token is a method, in the case it was written. CONNECT is refused:
/// it asks for a tunnel, and this client returns a buffered response.
pub(super) fn request_method(token: &str) -> Result<Method, RuntimeError> {
    match Method::from_bytes(token.as_bytes()) {
        Ok(method) if method != Method::CONNECT => Ok(method),
        Ok(_) | Err(_) => Err(refused(ClientInput::Method)),
    }
}

/// Whether a parsed target is one this client sends to: HTTP or HTTPS, with a
/// host, and with no credentials in its authority.
fn is_sendable(target: &Url) -> bool {
    matches!(target.scheme(), "http" | "https")
        && target.has_host()
        && target.username().is_empty()
        && target.password().is_none()
}

/// The URL `text` names, resolved against `base` when it has one.
///
/// A control character is refused before parsing. The parser removes tabs and
/// line breaks, so admitting one would send to a target that was not written.
pub(super) fn parsed_url(text: &str, base: Option<&Url>) -> Option<Url> {
    match text.bytes().any(|byte| byte.is_ascii_control()) {
        true => None,
        false => Url::options().base_url(base).parse(text).ok(),
    }
}

/// One parsed HTTP(S) origin, with no credentials or resource-specific parts.
pub(super) fn bare_origin(text: &str) -> Option<Url> {
    let url = parsed_url(text, None)?;
    let bare = url.username().is_empty()
        && url.password().is_none()
        && url.query().is_none()
        && url.fragment().is_none()
        && url.path() == "/";
    match bare {
        true => sendable(url),
        false => None,
    }
}

/// An origin whose original spelling contains only scheme, authority, and optional `/`.
/// Check delimiters before URL normalization can discard userinfo or dot paths.
pub(super) fn exact_origin(text: &str) -> Option<Url> {
    let (_, authority_and_path) = text.split_once("://")?;
    let end = authority_and_path
        .find(['/', '?', '#'])
        .unwrap_or(authority_and_path.len());
    let (authority, path) = authority_and_path.split_at(end);
    let exact = text.trim() == text
        && !text.contains('\\')
        && !authority.is_empty()
        && !authority.contains('@')
        && matches!(path, "" | "/");
    match exact {
        true => bare_origin(text),
        false => None,
    }
}

/// Whether `a` and `b` share one origin: the same scheme, the same normalized
/// host, and the same effective port.
///
/// For HTTP(S) URLs this equals `Url::origin` equality, and it allocates
/// nothing. The host is compared as the URL parser serialized it, so case and
/// IDN spelling do not make another origin, and a root dot does. A URL with
/// no host or no known port shares an origin with no URL.
pub(super) fn same_origin(a: &Url, b: &Url) -> bool {
    match (a.host(), a.port_or_known_default()) {
        (Some(host), Some(port)) => {
            a.scheme() == b.scheme()
                && b.host() == Some(host)
                && b.port_or_known_default() == Some(port)
        }
        _ => false,
    }
}

/// A sendable HTTP(S) target with its fragment removed.
pub(super) fn sendable(mut target: Url) -> Option<Url> {
    match is_sendable(&target) {
        true => {
            target.set_fragment(None);
            Some(target)
        }
        false => None,
    }
}

/// The target `url` names, without its fragment.
pub(in crate::http) fn request_target(url: &str) -> Result<Url, RuntimeError> {
    parsed_url(url, None)
        .and_then(sendable)
        .ok_or_else(|| refused(ClientInput::Url))
}

/// One application header, parsed.
///
/// The name is read first, so a transport-owned name is refused as reserved
/// whatever its value is.
fn application_header(name: &str, value: &str) -> Result<(HeaderName, HeaderValue), RuntimeError> {
    let name = HeaderName::from_bytes(name.as_bytes()).map_err(|_| refused(ClientInput::Header))?;
    match TRANSPORT_OWNED.contains(&name) {
        true => Err(refused(ClientInput::ReservedHeader)),
        false => HeaderValue::from_bytes(value.as_bytes())
            .map(|value| (name, value))
            .map_err(|_| refused(ClientInput::Header)),
    }
}

/// One request as every attempt of it is sent.
///
/// The body is frozen `Bytes`, so each attempt shares one allocation. A header
/// value carries its own sensitivity bit, which is the only record of it. A
/// followed redirect changes this value in place, so a later attempt repeats
/// the hop the call is on and never the request it began with.
pub(super) struct Description {
    method: Method,
    url: Url,
    headers: HeaderMap,
    /// Empty when the request has no body.
    body: Bytes,
    /// The response ceiling, or `None` for the named opt-out.
    response_limit: Option<usize>,
}

impl Description {
    /// Describe a request to `url` with no header and no body.
    pub(super) fn new(
        method: Method,
        url: &str,
        response_limit: Option<usize>,
    ) -> Result<Self, RuntimeError> {
        Ok(Self {
            method,
            url: request_target(url)?,
            headers: HeaderMap::new(),
            body: Bytes::new(),
            response_limit,
        })
    }

    pub(super) fn method(&self) -> &Method {
        &self.method
    }

    pub(super) fn url(&self) -> &Url {
        &self.url
    }

    pub(super) fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub(super) fn body(&self) -> &Bytes {
        &self.body
    }

    pub(super) fn response_limit(&self) -> Option<usize> {
        self.response_limit
    }

    /// Validate one application header and replace any value its name holds.
    pub(super) fn write_header(
        &mut self,
        name: &str,
        value: &str,
        marked: bool,
    ) -> Result<(), RuntimeError> {
        let (name, value) = application_header(name, value)?;
        self.replace_header(name, value, marked);
        Ok(())
    }

    /// Replace the value `name` holds.
    ///
    /// Sensitivity is sticky: a name that was marked, or that is always a
    /// credential, stays sensitive under every later value.
    pub(super) fn replace_header(
        &mut self,
        name: HeaderName,
        mut value: HeaderValue,
        marked: bool,
    ) {
        let sensitive = marked
            || ALWAYS_SENSITIVE.contains(&name)
            || self
                .headers
                .get(&name)
                .is_some_and(HeaderValue::is_sensitive);
        value.set_sensitive(sensitive);
        self.headers.insert(name, value);
    }

    pub(super) fn replace_body(&mut self, body: Bytes) {
        self.body = body;
    }

    /// Add the automatic trace context an operation that starts now carries.
    ///
    /// The value becomes part of the description, so every retry and every
    /// followed redirect sends the context the operation began under.
    pub(super) fn carry_trace_context(&mut self, enabled: bool) {
        if let Some(value) = entry_traceparent(enabled, &self.headers) {
            self.replace_header(TRACEPARENT, value, false);
        }
    }

    /// Send to `url` from now on. The caller has admitted it, and has removed
    /// the credentials when `url` is at another origin.
    pub(super) fn retarget(&mut self, url: Url) {
        self.url = url;
    }

    /// Permanently remove `Authorization`, `Cookie`, and every header marked
    /// sensitive, before a change of origin.
    pub(super) fn strip_credentials(&mut self) {
        let sensitive: Box<[HeaderName]> = self
            .headers
            .iter()
            .filter(|(name, value)| ALWAYS_SENSITIVE.contains(name) || value.is_sensitive())
            .map(|(name, _)| name.clone())
            .collect();
        for name in sensitive {
            self.headers.remove(name);
        }
    }

    /// Send `method` from now on, with no body and no `Content-Type`.
    ///
    /// Every other header stays, with its sensitivity.
    pub(super) fn become_bodiless(&mut self, method: Method) {
        self.method = method;
        self.body = Bytes::new();
        self.headers.remove(CONTENT_TYPE);
    }

    /// Replace the response ceiling. Zero is refused: no answer fits it.
    pub(super) fn limit_response(&mut self, max_bytes: usize) -> Result<(), RuntimeError> {
        match max_bytes {
            0 => Err(refused(ClientInput::ResponseLimit)),
            limit => {
                self.response_limit = Some(limit);
                Ok(())
            }
        }
    }

    /// Finish the checks that need more than one field.
    ///
    /// A TRACE request carries no content, so one with a body is refused.
    pub(super) fn admit(&self) -> Result<(), RuntimeError> {
        match self.method == Method::TRACE && !self.body.is_empty() {
            true => Err(refused(ClientInput::Body)),
            false => Ok(()),
        }
    }

    /// Whether the header `name` is marked sensitive, or `None` when the
    /// description holds no such header.
    pub(super) fn header_sensitivity(&self, name: &str) -> Option<bool> {
        self.headers.get(name).map(HeaderValue::is_sensitive)
    }
}
