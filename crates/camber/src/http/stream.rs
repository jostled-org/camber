use super::response::HeaderPair;
use crate::RuntimeError;
use bytes::Bytes;
use std::borrow::Cow;
use tokio::sync::mpsc;

/// Default channel buffer capacity for streaming responses.
const DEFAULT_STREAM_BUFFER: usize = 32;

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
}

impl StreamResponse {
    /// Create a streaming response with the given status code.
    ///
    /// Returns the response to return from the handler and a sender
    /// for pushing body chunks. Uses the default buffer capacity.
    pub fn new(status: u16) -> (Self, StreamSender) {
        let (tx, rx) = mpsc::channel(DEFAULT_STREAM_BUFFER);
        let resp = Self {
            status,
            headers: Vec::new(),
            rx,
        };
        (resp, StreamSender { tx })
    }

    /// Create a streaming response with explicit buffer capacity.
    ///
    /// `cap` controls the channel depth for backpressure. Must be greater
    /// than zero.
    pub fn with_buffer(status: u16, cap: usize) -> Result<(Self, StreamSender), RuntimeError> {
        match cap {
            0 => Err(RuntimeError::InvalidArgument(
                "stream buffer capacity must be greater than zero".into(),
            )),
            _ => {
                let (tx, rx) = mpsc::channel(cap);
                let resp = Self {
                    status,
                    headers: Vec::new(),
                    rx,
                };
                Ok((resp, StreamSender { tx }))
            }
        }
    }

    /// Add a custom header to the streaming response.
    pub fn with_header(mut self, name: &str, value: &str) -> Self {
        self.headers
            .push((Cow::Owned(name.to_owned()), Cow::Owned(value.to_owned())));
        self
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
