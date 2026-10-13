//! The streamed answer an outbound call returns: its head, and the one handle
//! to its body.

use super::super::body::ClientBody;
use super::super::head::ResponseHead;
use crate::RuntimeError;

/// The head of a streamed outbound HTTP response, with its unread body.
///
/// [`ClientRequest::send_streaming`](crate::http::ClientRequest::send_streaming)
/// returns it as soon as the final status and headers arrive. No body byte
/// has been read. [`into_body`](Self::into_body) gives the one handle that
/// reads the body, one chunk at a time.
///
/// Every status is a response. A 404 or a 503 arrives here like a 200;
/// [`error_for_status`](Self::error_for_status) is the explicit conversion to
/// a failure.
///
/// The response stays open while this value lives. The operation total and
/// the attempt timeout keep running, and either one ends the response with no
/// call from the caller. Dropping the value cancels the response. The drop
/// does not wait for the upstream to be released: use
/// [`ClientBody::close`] for that.
///
/// The value is `Send`. It has one owner: it is not `Clone`.
///
/// ```compile_fail
/// fn duplicate(response: camber::http::ClientStreamResponse) {
///     let _copy = response.clone();
/// }
/// ```
///
/// It is not an inbound [`Response`](crate::http::Response) and does not
/// convert into one. A handler that forwards an upstream body chooses the
/// status and headers it sends, and reads each chunk itself.
///
/// ```compile_fail
/// fn forward(response: camber::http::ClientStreamResponse) -> camber::http::HandlerOutcome {
///     camber::http::IntoResponse::into_response(response)
/// }
/// ```
///
/// `Debug` prints the status and the header names. It never prints a header
/// value.
pub struct ClientStreamResponse {
    /// The status and the headers, stored and read by the rules every
    /// outbound answer shares.
    head: ResponseHead,
    /// The one handle to the body. Its drop cancels the response.
    body: ClientBody,
}

impl ClientStreamResponse {
    pub(super) fn new(head: ResponseHead, body: ClientBody) -> Self {
        Self { head, body }
    }

    /// The HTTP status code the peer answered with.
    #[must_use]
    pub fn status(&self) -> u16 {
        self.head.status()
    }

    /// The first value of the header `name`, matched without regard to case.
    ///
    /// The value is the raw bytes the peer sent. Use [`headers`](Self::headers)
    /// to read every value of a repeated name.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.head.header(name)
    }

    /// Every header entry, as a lowercase name and the value's raw bytes.
    ///
    /// A repeated name appears once per value, in the order the peer sent
    /// those values. The order between different names is not the wire order.
    pub fn headers(&self) -> impl Iterator<Item = (&str, &[u8])> {
        self.head.headers()
    }

    /// Return this response, or `ClientError::Status` for a 400–599 status.
    ///
    /// A returned response is the same live response. A refusal carries the
    /// status number only, drops the headers, and cancels the unread body.
    ///
    /// The refusal does not wait. It requests the release of the upstream,
    /// and the task that owns the response releases it afterwards. It sends
    /// nothing again, and it does not change the cause of a response that
    /// already ended.
    pub fn error_for_status(self) -> Result<Self, RuntimeError> {
        self.head.refuse_error_status().map(|()| self)
    }

    /// Take the one handle to the body.
    ///
    /// The status and the headers are dropped. Read them first.
    #[must_use]
    pub fn into_body(self) -> ClientBody {
        self.body
    }
}

impl std::fmt::Debug for ClientStreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClientStreamResponse")
            .field("status", &self.status())
            .field("header_names", &self.head.names())
            .finish_non_exhaustive()
    }
}
