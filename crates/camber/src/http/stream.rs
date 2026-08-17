use super::response::HeaderPair;
use super::transfer_budget::TransferBudget;
use crate::RuntimeError;
use bytes::Bytes;
use std::borrow::Cow;
use tokio::sync::mpsc;

/// Default channel buffer capacity for streaming responses.
const DEFAULT_STREAM_BUFFER: usize = 32;

/// The transfer policy a streaming response that names none is bounded by.
///
/// Unbounded here is a statement, not an omission: the channel above keeps this
/// response's memory bounded whatever its policy says, and a generic streaming
/// handler is the shape a long-lived feed is written in. Under a router, host,
/// server, or runtime download policy this inherits that policy rather than
/// widening it, so a service that states a finite download bound gets one on
/// every stream it serves. [`StreamResponse::with_budget`] is how one stream
/// names its own.
const INHERITED_STREAM_POLICY: TransferBudget = TransferBudget::unbounded();

impl std::fmt::Debug for StreamSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamSender").finish_non_exhaustive()
    }
}

/// Sender half of a streaming HTTP response.
///
/// Push chunks to the connected client. Returns `RuntimeError::ChannelClosed`
/// when the client disconnects or the receiver is dropped.
pub struct StreamSender {
    tx: mpsc::Sender<Bytes>,
}

impl StreamSender {
    /// Send one response chunk to the client.
    pub async fn send(&self, data: impl Into<Bytes>) -> Result<(), RuntimeError> {
        self.tx
            .send(data.into())
            .await
            .map_err(|_| RuntimeError::ChannelClosed)
    }
}

impl std::fmt::Debug for StreamResponse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamResponse")
            .field("status", &self.status)
            .field("header_count", &self.headers.len())
            .finish()
    }
}

/// A streaming HTTP response returned from async handlers.
///
/// Created via `StreamResponse::new()`, which returns both the response
/// (to return from the handler) and a `StreamSender` (to push chunks).
pub struct StreamResponse {
    status: u16,
    headers: Vec<HeaderPair>,
    rx: mpsc::Receiver<Bytes>,
    /// The download policy this response states for itself, narrowed under the
    /// route's when the body is built.
    budget: TransferBudget,
}

impl StreamResponse {
    /// Create a streaming response with the given status code.
    ///
    /// Returns the response to return from the handler and a sender
    /// for pushing body chunks. Uses the default buffer capacity, and the
    /// download policy its route, host, server, or runtime selected.
    pub fn new(status: u16) -> (Self, StreamSender) {
        Self::over(status, DEFAULT_STREAM_BUFFER, INHERITED_STREAM_POLICY)
    }

    /// Create a streaming response with explicit buffer capacity.
    ///
    /// `cap` controls the channel depth for backpressure. Must be greater
    /// than zero.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when `cap` is zero.
    pub fn with_buffer(status: u16, cap: usize) -> Result<(Self, StreamSender), RuntimeError> {
        Self::with_budget(status, cap, INHERITED_STREAM_POLICY)
    }

    /// Create a streaming response under the transfer policy it names.
    ///
    /// `cap` is the channel depth this response's producer is held to, and
    /// `budget` is the payload maximum, quiet interval, and lifetime its body is
    /// bounded by. The policy is applied under whatever the route, host, server,
    /// or runtime already selected: an unbounded dimension here inherits the
    /// outer bound rather than removing it, and two finite values resolve to the
    /// smaller.
    ///
    /// The bounds are enforced on the body, after the head has committed, so a
    /// crossing ends the response rather than replacing its status. The frame
    /// that would cross the maximum is never written.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when `cap` is zero.
    pub fn with_budget(
        status: u16,
        cap: usize,
        budget: TransferBudget,
    ) -> Result<(Self, StreamSender), RuntimeError> {
        match cap {
            0 => Err(RuntimeError::InvalidArgument(
                "stream buffer capacity must be greater than zero".into(),
            )),
            cap => Ok(Self::over(status, cap, budget)),
        }
    }

    /// Build the response and its sender over a capacity already accepted.
    fn over(status: u16, cap: usize, budget: TransferBudget) -> (Self, StreamSender) {
        let (tx, rx) = mpsc::channel(cap);
        let resp = Self {
            status,
            headers: Vec::new(),
            rx,
            budget,
        };
        (resp, StreamSender { tx })
    }

    /// Add a custom header to the streaming response.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .push((Cow::Owned(name.to_owned()), Cow::Owned(value.to_owned())));
        self
    }

    /// The download policy this response states for itself.
    pub(crate) const fn budget(&self) -> TransferBudget {
        self.budget
    }

    pub(crate) fn into_parts(self) -> StreamParts {
        StreamParts {
            status: self.status,
            headers: self.headers.into_boxed_slice(),
            rx: self.rx,
        }
    }
}

pub(crate) struct StreamParts {
    pub(crate) status: u16,
    pub(crate) headers: Box<[HeaderPair]>,
    pub(crate) rx: mpsc::Receiver<Bytes>,
}
