use super::cookie::{CookieOptions, sanitize_cookie};
use super::rejection::MappedRefusal;
use crate::RuntimeError;
use bytes::Bytes;
use serde::Serialize;
use std::borrow::Cow;
use std::fmt;
use std::sync::OnceLock;

/// A single HTTP header as a name-value pair.
///
/// Uses `Cow<'static, str>` so that known-at-compile-time headers
/// (Content-Type, etc.) are `Cow::Borrowed` with zero heap allocation,
/// while dynamic headers use `Cow::Owned`.
pub type HeaderPair = (Cow<'static, str>, Cow<'static, str>);

/// Body storage that avoids double allocation.
///
/// Text and JSON responses store the string eagerly; bytes are computed
/// on demand in `into_wire()`. Binary responses store raw bytes; text
/// is decoded lazily on first `body()` call.
enum BodyStore {
    /// Source is text (text/json constructors). Bytes derived on demand.
    Text(Box<str>),
    /// Source is binary (bytes constructor, client responses).
    /// Text decoded lazily via `text_cache`.
    Raw {
        bytes: Bytes,
        text_cache: OnceLock<Box<str>>,
    },
    /// No body.
    Empty,
}

/// The outcome a handler produces.
///
/// Fallible, so a handler's error keeps its category and source chain until the
/// router's one rejection boundary decides what the peer is told.
///
/// Public because [`MiddlewareFuture`](super::MiddlewareFuture) resolves to it:
/// a name a published signature spells has to be a name a reader can follow.
/// It is transparently `Result<Response, RuntimeError>`, so this documents the
/// shape rather than adding one. It lives beside [`Response`] rather than beside
/// the route trie because it is what a response conversion answers with, and the
/// trie only stores the handlers that produce it.
pub type HandlerOutcome = Result<Response, RuntimeError>;

/// Trait for types that can be converted into an HTTP response outcome.
///
/// Implemented for `Response` (a deliberate application response) and
/// [`HandlerOutcome`] (that response, or the failure the router's rejection
/// boundary classifies). The conversion is fallible so a handler's error keeps
/// its category, source chain, and client-safe message until one boundary
/// decides what the peer is told.
pub trait IntoResponse {
    /// Convert this value into a response outcome.
    fn into_response(self) -> HandlerOutcome;
}

impl IntoResponse for Response {
    fn into_response(self) -> HandlerOutcome {
        Ok(self)
    }
}

impl IntoResponse for HandlerOutcome {
    fn into_response(self) -> HandlerOutcome {
        self
    }
}

/// Where a response came from, for the one decision that depends on it.
///
/// A conversion failure on an application response is a rejection the mapper
/// sees once. A conversion failure on a response the rejection boundary already
/// produced goes straight to the fixed fallback: mapping it again is how
/// response construction would recurse through policy.
pub(super) enum ResponseProvenance {
    /// Built by an application handler, by middleware, or by a framework path
    /// that has not reached rejection policy.
    Application,
    /// Produced by rejection policy — a mapper, the built-in mapper, or the
    /// fixed fallback — for the refusal the producing stage classified.
    ///
    /// The whole refusal rides here because the wire exit is where the status
    /// the peer was actually given becomes known, and that is the one status
    /// the operator's record and the operator's counter must both name.
    /// Conversion can still displace what policy settled on, so recording it
    /// any earlier would name a status no peer ever saw. Boxed, so a response
    /// nobody refused carries a pointer rather than a whole refusal.
    Mapped(Box<MappedRefusal>),
    /// Produced by the gate terminal, and untouched since.
    ///
    /// A specialized route runs its middleware as a gate rather than around a
    /// response, so the chain's answer means "the request may proceed" only
    /// while it is still the terminal's own value. A frame that refuses on the
    /// unwind replaces it, and this is what tells the two apart.
    Gate,
}

impl ResponseProvenance {
    /// Whether this is still the gate terminal's own passthrough answer.
    ///
    /// Listed rather than wildcarded, so a new variant is a compile error here
    /// instead of a silent pass through a gate that never approved it.
    pub(super) fn is_gate_passthrough(&self) -> bool {
        match self {
            Self::Gate => true,
            Self::Application | Self::Mapped(_) => false,
        }
    }

    /// Whether rejection policy built this response.
    ///
    /// Asked by the owners whose registered producer is only the producer while
    /// that producer answered for itself. A forwarded route names its upstream,
    /// and a mapped answer coming back from one means no upstream head ever
    /// reached it: the mapper built the head instead, and it is the mapper the
    /// operation's commitment has to name.
    ///
    /// Listed rather than wildcarded, for the reason
    /// [`Self::is_gate_passthrough`] is: a new variant is a compile error here
    /// instead of a head credited to an owner that produced nothing.
    pub(super) fn is_mapped(&self) -> bool {
        match self {
            Self::Mapped(_) => true,
            Self::Application | Self::Gate => false,
        }
    }

    /// The refusal this response answered, when rejection policy produced it.
    pub(super) fn into_refusal(self) -> Option<Box<MappedRefusal>> {
        match self {
            Self::Mapped(refusal) => Some(refusal),
            Self::Application | Self::Gate => None,
        }
    }

    /// The bounded name this origin is printed under.
    ///
    /// Listed rather than wildcarded, for the same reason
    /// [`Self::is_gate_passthrough`] is: a new variant is a compile error here
    /// instead of a response that prints as one it is not.
    fn label(&self) -> &'static str {
        match self {
            Self::Application => "application",
            Self::Mapped(_) => "mapped",
            Self::Gate => "gate",
        }
    }
}

/// The header a response states its representation through.
const CONTENT_TYPE: &str = "Content-Type";

/// A response Camber accepted that Hyper cannot put on the wire.
///
/// Carries the content type the accepted head established alongside the
/// builder's own error, because the rejection boundary needs both and the
/// response itself is gone by the time it answers. Built only on the failure
/// path, so a response that converts pays nothing for a value only a failure
/// reads.
pub(super) struct UnrepresentableResponse {
    error: hyper::http::Error,
    content_type: Option<Box<str>>,
}

impl UnrepresentableResponse {
    fn new(error: hyper::http::Error, headers: &[HeaderPair]) -> Self {
        Self {
            error,
            content_type: representable_content_type(headers),
        }
    }

    /// The content type the accepted head established.
    pub(super) fn content_type(&self) -> Option<&str> {
        self.content_type.as_deref()
    }

    /// The builder's own account of what it could not represent.
    pub(super) fn into_error(self) -> hyper::http::Error {
        self.error
    }
}

/// The declared content type, when the head stated one Hyper would accept.
///
/// A `Content-Type` that is itself the value Hyper refused established nothing:
/// naming it would report a representation that never validated.
fn representable_content_type(headers: &[HeaderPair]) -> Option<Box<str>> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(CONTENT_TYPE))
        .filter(|(_, value)| hyper::header::HeaderValue::from_str(value).is_ok())
        .map(|(_, value)| Box::from(value.as_ref()))
}

/// The bytes one stored body becomes on the wire.
fn wire_bytes(body: BodyStore) -> Bytes {
    match body {
        BodyStore::Text(text) => Bytes::from(String::from(text)),
        BodyStore::Raw { bytes, .. } => bytes,
        BodyStore::Empty => Bytes::new(),
    }
}

/// Validate that an HTTP status code is in the valid range (100-599).
pub(super) fn validate_status(status: u16) -> Result<(), RuntimeError> {
    match (100..=599).contains(&status) {
        true => Ok(()),
        false => Err(RuntimeError::InvalidArgument(
            format!("invalid HTTP status code: {status}").into_boxed_str(),
        )),
    }
}

impl fmt::Debug for Response {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (body_type, body_len) = match &self.body {
            BodyStore::Text(text) => ("text", text.len()),
            BodyStore::Raw { bytes, .. } => ("raw", bytes.len()),
            BodyStore::Empty => ("empty", 0),
        };
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("provenance", &self.provenance.label())
            .field("header_count", &self.headers.len())
            .field("body_type", &body_type)
            .field("body_length", &body_len)
            .finish()
    }
}

/// HTTP response with owned data.
///
/// Used for both outbound client responses (returned by `get`/`post`)
/// and server handler responses (constructed with `text`/`empty`).
pub struct Response {
    status: u16,
    body: BodyStore,
    headers: Vec<HeaderPair>,
    provenance: ResponseProvenance,
}

impl Response {
    pub(crate) fn new(status: u16, body: Bytes, headers: Vec<HeaderPair>) -> Self {
        Self {
            status,
            body: BodyStore::Raw {
                bytes: body,
                text_cache: OnceLock::new(),
            },
            headers,
            provenance: ResponseProvenance::Application,
        }
    }

    // -- Private construction helpers (shared by public and pub(crate) APIs) --

    fn build_text(status: u16, body: &str) -> Self {
        Self {
            status,
            body: BodyStore::Text(body.into()),
            headers: vec![(Cow::Borrowed("Content-Type"), Cow::Borrowed("text/plain"))],
            provenance: ResponseProvenance::Application,
        }
    }

    fn build_empty(status: u16) -> Self {
        Self {
            status,
            body: BodyStore::Empty,
            headers: Vec::new(),
            provenance: ResponseProvenance::Application,
        }
    }

    fn build_bytes(status: u16, data: impl Into<Bytes>) -> Self {
        Self {
            status,
            body: BodyStore::Raw {
                bytes: data.into(),
                text_cache: OnceLock::new(),
            },
            headers: vec![
                (
                    Cow::Borrowed("Content-Type"),
                    Cow::Borrowed("application/octet-stream"),
                ),
                (
                    Cow::Borrowed("X-Content-Type-Options"),
                    Cow::Borrowed("nosniff"),
                ),
            ],
            provenance: ResponseProvenance::Application,
        }
    }

    // -- Public API: validates status, returns Result --

    /// Construct a plain-text response with `Content-Type: text/plain`.
    ///
    /// Returns `Err(RuntimeError::InvalidArgument)` if status is outside 100-599.
    pub fn text(status: u16, body: &str) -> Result<Self, RuntimeError> {
        validate_status(status)?;
        Ok(Self::build_text(status, body))
    }

    /// Construct a response with no body.
    ///
    /// Returns `Err(RuntimeError::InvalidArgument)` if status is outside 100-599.
    pub fn empty(status: u16) -> Result<Self, RuntimeError> {
        validate_status(status)?;
        Ok(Self::build_empty(status))
    }

    /// Construct a JSON response with `Content-Type: application/json`.
    ///
    /// Returns `Err(RuntimeError::InvalidArgument)` if status is outside 100-599
    /// or if serialization fails.
    pub fn json(status: u16, value: &impl Serialize) -> Result<Self, RuntimeError> {
        validate_status(status)?;
        let body = serde_json::to_vec(value).map_err(|e| {
            RuntimeError::InvalidArgument(
                format!("json serialization failed: {e}").into_boxed_str(),
            )
        })?;
        Ok(Self {
            status,
            body: BodyStore::Raw {
                bytes: Bytes::from(body),
                text_cache: OnceLock::new(),
            },
            headers: vec![(
                Cow::Borrowed("Content-Type"),
                Cow::Borrowed("application/json"),
            )],
            provenance: ResponseProvenance::Application,
        })
    }

    /// Construct a binary response with `Content-Type: application/octet-stream`.
    ///
    /// Includes `X-Content-Type-Options: nosniff` to prevent browser MIME sniffing.
    /// Override the content type with [`with_content_type`](Self::with_content_type) if needed.
    ///
    /// Returns `Err(RuntimeError::InvalidArgument)` if status is outside 100-599.
    pub fn bytes(status: u16, data: impl Into<Bytes>) -> Result<Self, RuntimeError> {
        validate_status(status)?;
        Ok(Self::build_bytes(status, data))
    }

    // -- Internal API: no status validation, for known-valid status codes within the crate --

    /// Construct a plain-text response without status validation.
    /// Caller must ensure status is a valid HTTP status code (100-599).
    pub(crate) fn text_raw(status: u16, body: &str) -> Self {
        Self::build_text(status, body)
    }

    /// Construct an empty response without status validation.
    /// Caller must ensure status is a valid HTTP status code (100-599).
    pub(crate) fn empty_raw(status: u16) -> Self {
        Self::build_empty(status)
    }

    /// Construct a binary response without status validation.
    /// Caller must ensure status is a valid HTTP status code (100-599).
    pub(crate) fn bytes_raw(status: u16, data: impl Into<Bytes>) -> Self {
        Self::build_bytes(status, data)
    }

    /// Add a custom header to the response.
    pub fn with_header(self, name: &str, value: &str) -> Self {
        self.with_pair(Cow::Owned(name.to_owned()), Cow::Owned(value.to_owned()))
    }

    /// Add a header whose name Camber spells at compile time.
    ///
    /// The name is borrowed, never copied. Framework headers are literals, and
    /// a `String` copy of a constant on every response is exactly the
    /// allocation `HeaderPair`'s `Cow` shape exists to avoid. The value stays a
    /// `Cow` because some of these are constants too and some are built per
    /// response.
    pub(super) fn with_static_header(self, name: &'static str, value: Cow<'static, str>) -> Self {
        self.with_pair(Cow::Borrowed(name), value)
    }

    /// Append one header exactly as given.
    ///
    /// The one push site, so what a header costs is decided by the caller that
    /// knows whether its halves are constants, and nowhere else.
    pub(super) fn with_pair(mut self, name: Cow<'static, str>, value: Cow<'static, str>) -> Self {
        self.headers.push((name, value));
        self
    }

    /// Set the Content-Type header, replacing any existing one.
    pub fn with_content_type(self, content_type: &str) -> Self {
        self.with_replaced_header(CONTENT_TYPE, Cow::Owned(content_type.to_owned()))
    }

    /// Set one header to exactly this value, dropping every existing spelling.
    ///
    /// The framework's correction of protocol-owned output goes through here,
    /// so a mapper's conflicting value cannot survive alongside the required
    /// one as a second header line.
    ///
    /// The value is taken as a `Cow` rather than a `&str`: a correction whose
    /// value the framework spells at compile time, and one whose value the
    /// caller already owns, both reach the header list without a copy this
    /// function made.
    pub(super) fn with_replaced_header(self, name: &'static str, value: Cow<'static, str>) -> Self {
        self.without_header(name)
            .with_pair(Cow::Borrowed(name), value)
    }

    /// Drop every header with this name.
    pub(super) fn without_header(mut self, name: &str) -> Self {
        self.headers
            .retain(|(key, _)| !key.eq_ignore_ascii_case(name));
        self
    }

    /// Drop every header with any of these names.
    ///
    /// One pass over the list rather than one per name: the caller that removes
    /// a whole family — every header a version forbids — is removing them from
    /// the same response, and folding [`Self::without_header`] over the family
    /// walked the list once for each member to answer one question.
    pub(super) fn without_headers(mut self, names: &[&str]) -> Self {
        self.headers
            .retain(|(key, _)| !names.iter().any(|name| key.eq_ignore_ascii_case(name)));
        self
    }

    /// Append a `Set-Cookie` header with the given name and value.
    ///
    /// **Warning**: produces a cookie without `Secure`, `HttpOnly`, or `SameSite`
    /// attributes. Suitable for development or non-sensitive cookies only.
    /// For production use, prefer [`set_cookie_with`](Self::set_cookie_with) with
    /// explicit [`CookieOptions`] to set security attributes.
    pub fn set_cookie(self, name: &str, value: &str) -> Self {
        let header_value = format!("{}={}", sanitize_cookie(name), sanitize_cookie(value));
        self.with_static_header("Set-Cookie", Cow::Owned(header_value))
    }

    /// Append a `Set-Cookie` header with the given name, value, and options.
    pub fn set_cookie_with(self, name: &str, value: &str, options: &CookieOptions) -> Self {
        let header_value = options.format_header(name, value);
        self.with_static_header("Set-Cookie", Cow::Owned(header_value))
    }

    /// Strip the body, keeping status and headers. Used for HEAD auto-responses.
    pub(crate) fn strip_body(self) -> Self {
        Self {
            body: BodyStore::Empty,
            ..self
        }
    }

    /// Mark this response as the product of rejection policy for one refusal.
    ///
    /// Consuming rather than a setter, so a response is marked exactly where
    /// policy hands it back and nowhere else.
    #[must_use]
    pub(super) fn mark_mapped(self, refusal: MappedRefusal) -> Self {
        Self {
            provenance: ResponseProvenance::Mapped(Box::new(refusal)),
            ..self
        }
    }

    /// Mark this response as the gate terminal's own passthrough answer.
    #[must_use]
    pub(super) fn mark_gate(self) -> Self {
        Self {
            provenance: ResponseProvenance::Gate,
            ..self
        }
    }

    /// Where this response came from.
    pub(super) fn provenance(&self) -> &ResponseProvenance {
        &self.provenance
    }

    /// Return the HTTP status code.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// Return the response body as text.
    ///
    /// Invalid UTF-8 is decoded lossily on first access and cached.
    pub fn body(&self) -> &str {
        match &self.body {
            BodyStore::Text(text) => text,
            BodyStore::Raw { bytes, text_cache } => super::encoding::lossy_text(bytes, text_cache),
            BodyStore::Empty => "",
        }
    }

    /// Return the raw body bytes.
    pub fn body_bytes(&self) -> &[u8] {
        match &self.body {
            BodyStore::Text(text) => text.as_bytes(),
            BodyStore::Raw { bytes, .. } => bytes,
            BodyStore::Empty => &[],
        }
    }

    /// Return all response headers.
    pub fn headers(&self) -> &[HeaderPair] {
        &self.headers
    }

    /// Convert to a hyper Response with a full body, consuming self.
    ///
    /// Fallible, and it reports the builder's own error rather than
    /// substituting a response of its own: a header name `with_header` accepted
    /// but Hyper cannot represent is a rejection the router's one boundary owns,
    /// and a second fallback here is a second answer to the same condition.
    ///
    /// The failure carries the content type this head established. The headers
    /// are consumed here, and the boundary that maps the failure is asked what
    /// the response was going to be.
    ///
    /// The provenance leaves with the conversion rather than beside it. Hyper's
    /// value has nowhere to carry a refusal, so a conversion that discarded the
    /// provenance would drop a live [`MappedRefusal`] and have its `Drop` report
    /// a response that was in fact sent. Returning the two together means the
    /// caller cannot forget to take it first, and a caller that ignores the
    /// refusal has to say so.
    pub(super) fn into_wire(
        self,
    ) -> (
        ResponseProvenance,
        Result<hyper::Response<http_body_util::Full<Bytes>>, UnrepresentableResponse>,
    ) {
        let Self {
            status,
            body,
            headers,
            provenance,
        } = self;

        let mut builder = hyper::Response::builder().status(status);
        for (name, value) in &headers {
            builder = builder.header(name.as_ref(), value.as_ref());
        }
        let converted = builder
            .body(http_body_util::Full::new(wire_bytes(body)))
            .map_err(|error| UnrepresentableResponse::new(error, &headers));
        (provenance, converted)
    }
}
