//! The buffered answer an outbound call returns.
//!
//! A response is complete when a caller receives it: the body was collected
//! under the call's ceiling, and the headers are the bytes the peer sent.
//! Every conversion here reads that stored answer. None of them sends.

use super::super::response::HeaderPair;
use super::error::{ClientDecode, ClientError};
use crate::RuntimeError;
use bytes::Bytes;
use reqwest::header::HeaderMap;
use serde::de::DeserializeOwned;

/// One response header as it is stored: its name, and its value's raw bytes.
pub(in crate::http) type HeaderEntry = (Box<str>, Box<[u8]>);

/// The statuses [`ClientResponse::error_for_status`] refuses.
const ERROR_STATUSES: std::ops::RangeInclusive<u16> = 400..=599;

/// Copy a peer's headers out of the transport's map.
///
/// The map yields a repeated name once per value, in the order the values
/// arrived, so the copy keeps both the multiplicity and that order. A value
/// is copied as bytes: one that is not text stays what the peer sent.
pub(super) fn wire_headers(headers: &HeaderMap) -> Box<[HeaderEntry]> {
    headers
        .iter()
        .map(|(name, value)| (Box::from(name.as_str()), Box::from(value.as_bytes())))
        .collect()
}

/// Copy a canned answer's text headers into the stored shape.
pub(in crate::http) fn text_headers(headers: &[HeaderPair]) -> Box<[HeaderEntry]> {
    headers
        .iter()
        .map(|(name, value)| (Box::from(name.as_ref()), Box::from(value.as_bytes())))
        .collect()
}

/// A completed outbound HTTP response.
///
/// The body is the bytes the peer sent, collected whole under the client's
/// response maximum. Nothing decodes it: a compressed answer stays compressed,
/// and [`text`](Self::text) and [`json`](Self::json) read the stored bytes
/// only when a caller asks.
///
/// Every status is a response. A 404 or a 503 arrives here like a 200;
/// [`error_for_status`](Self::error_for_status) is the explicit conversion to
/// a failure.
///
/// This is not an inbound [`Response`](crate::http::Response) and does not
/// convert into one. A handler that forwards an upstream answer chooses the
/// status, body, and headers it sends.
///
/// `Debug` prints the status, the header names, and the body length. It never
/// prints a header value or body content.
pub struct ClientResponse {
    status: u16,
    /// Never mutated after collection, so the entries are a boxed slice.
    headers: Box<[HeaderEntry]>,
    /// Shared with the collector that produced it, so handing the body to a
    /// caller copies nothing.
    body: Bytes,
}

impl ClientResponse {
    pub(in crate::http) fn new(status: u16, headers: Box<[HeaderEntry]>, body: Bytes) -> Self {
        Self {
            status,
            headers,
            body,
        }
    }

    /// The HTTP status code the peer answered with.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The first value of the header `name`, matched without regard to case.
    ///
    /// The value is the raw bytes the peer sent. Use [`headers`](Self::headers)
    /// to read every value of a repeated name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(found, _)| found.eq_ignore_ascii_case(name))
            .map(|(_, value)| &**value)
    }

    /// Every header entry, as a lowercase name and the value's raw bytes.
    ///
    /// A repeated name appears once per value, in the order the peer sent
    /// those values. The order between different names is not the wire order.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.headers.iter().map(|(name, value)| (&**name, &**value))
    }

    /// The body exactly as the peer sent it.
    #[must_use]
    pub fn body_bytes(&self) -> &[u8] {
        &self.body
    }

    /// Take the body without copying it.
    #[must_use]
    pub fn into_body(self) -> Bytes {
        self.body
    }

    /// The body as UTF-8 text.
    ///
    /// Decoding is strict. A body that is not UTF-8 returns
    /// `ClientError::Decode(ClientDecode::Utf8)`; no byte is replaced.
    pub fn text(&self) -> Result<&str, RuntimeError> {
        std::str::from_utf8(&self.body).map_err(|_| decode_failure(ClientDecode::Utf8))
    }

    /// Deserialize the body as JSON.
    ///
    /// The `Content-Type` header is not read. A body that is not the requested
    /// JSON returns `ClientError::Decode(ClientDecode::Json)`.
    pub fn json<T: DeserializeOwned>(&self) -> Result<T, RuntimeError> {
        serde_json::from_slice(&self.body).map_err(|_| decode_failure(ClientDecode::Json))
    }

    /// Return this response, or `ClientError::Status` for a 400–599 status.
    ///
    /// The refusal carries the status number only. The refused headers and
    /// body are dropped.
    pub fn error_for_status(self) -> Result<Self, RuntimeError> {
        match ERROR_STATUSES.contains(&self.status) {
            true => Err(RuntimeError::Client(ClientError::Status(self.status))),
            false => Ok(self),
        }
    }
}

/// The failure one local decoding returns.
///
/// The decoder's own error is dropped: its text can quote the body.
fn decode_failure(kind: ClientDecode) -> RuntimeError {
    RuntimeError::Client(ClientError::Decode(kind))
}

/// The header names a `Debug` rendering lists.
struct HeaderNames<'a>(&'a [HeaderEntry]);

impl std::fmt::Debug for HeaderNames<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_list()
            .entries(self.0.iter().map(|(name, _)| name))
            .finish()
    }
}

impl std::fmt::Debug for ClientResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientResponse")
            .field("status", &self.status)
            .field("header_names", &HeaderNames(&self.headers))
            .field("body_len", &self.body.len())
            .finish()
    }
}
