use super::cookie::{CookieOptions, sanitize_cookie};
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

fn raw_body_text<'a>(bytes: &'a Bytes, text_cache: &'a OnceLock<Box<str>>) -> &'a str {
    match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => text_cache.get_or_init(|| String::from_utf8_lossy(bytes).into()),
    }
}

/// Body storage that avoids double allocation.
///
/// Text and JSON responses store the string eagerly; bytes are computed
/// on demand in `into_hyper()`. Binary responses store raw bytes; text
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

/// Trait for types that can be converted into an HTTP response.
///
/// Implemented for `Response` (passthrough) and `Result<Response, RuntimeError>`
/// (maps `BadRequest` to 400, other errors to 500).
pub trait IntoResponse {
    /// Convert this value into a concrete [`Response`].
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for Result<Response, RuntimeError> {
    fn into_response(self) -> Response {
        match self {
            Ok(resp) => resp,
            Err(RuntimeError::BadRequest(msg)) => Response::build_text(400, &msg),
            Err(e) => Response::build_text(500, &e.to_string()),
        }
    }
}

/// Validate that an HTTP status code is in the valid range (100-599).
fn validate_status(status: u16) -> Result<(), RuntimeError> {
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
        }
    }

    // -- Private construction helpers (shared by public and pub(crate) APIs) --

    fn build_text(status: u16, body: &str) -> Self {
        Self {
            status,
            body: BodyStore::Text(body.into()),
            headers: vec![(Cow::Borrowed("Content-Type"), Cow::Borrowed("text/plain"))],
        }
    }

    fn build_empty(status: u16) -> Self {
        Self {
            status,
            body: BodyStore::Empty,
            headers: Vec::new(),
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
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .push((Cow::Owned(name.to_owned()), Cow::Owned(value.to_owned())));
        self
    }

    /// Set the Content-Type header, replacing any existing one.
    pub fn with_content_type(mut self, content_type: &str) -> Self {
        self.headers
            .retain(|(k, _)| !k.eq_ignore_ascii_case("Content-Type"));
        self.headers.push((
            Cow::Borrowed("Content-Type"),
            Cow::Owned(content_type.to_owned()),
        ));
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
        self.with_header("Set-Cookie", &header_value)
    }

    /// Append a `Set-Cookie` header with the given name, value, and options.
    pub fn set_cookie_with(self, name: &str, value: &str, options: &CookieOptions) -> Self {
        let header_value = options.format_header(name, value);
        self.with_header("Set-Cookie", &header_value)
    }

    /// Strip the body, keeping status and headers. Used for HEAD auto-responses.
    pub(crate) fn strip_body(self) -> Self {
        Self {
            status: self.status,
            body: BodyStore::Empty,
            headers: self.headers,
        }
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
            BodyStore::Raw { bytes, text_cache } => raw_body_text(bytes, text_cache),
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
    pub(crate) fn into_hyper(self) -> hyper::Response<http_body_util::Full<bytes::Bytes>> {
        let status = self.status;
        let body_bytes = match self.body {
            BodyStore::Text(text) => Bytes::from(String::from(text)),
            BodyStore::Raw { bytes, .. } => bytes,
            BodyStore::Empty => Bytes::new(),
        };

        let mut builder = hyper::Response::builder().status(status);
        for (name, value) in &self.headers {
            builder = builder.header(name.as_ref(), value.as_ref());
        }
        builder
            .body(http_body_util::Full::new(body_bytes))
            .unwrap_or_else(|err| {
                tracing::error!(%err, status, "hyper response builder failed");
                {
                    let mut fallback = hyper::Response::new(http_body_util::Full::new(
                        bytes::Bytes::from_static(b"internal error"),
                    ));
                    *fallback.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
                    fallback
                }
            })
    }
}
