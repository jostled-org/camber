use crate::RuntimeError;
use std::fmt::Write as FmtWrite;

impl std::fmt::Debug for SseWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseWriter").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for AsyncSseWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AsyncSseWriter").finish_non_exhaustive()
    }
}

/// The comment frame both writers send: `:\n\n`, which SSE clients ignore.
const COMMENT_FRAME: &[u8] = b":\n\n";

/// Frame one SSE event, with an `event:` line when it has a type.
///
/// Multi-line data is split into separate `data:` lines per the SSE spec.
fn event_frame(event_type: Option<&str>, event_data: &str) -> bytes::Bytes {
    let mut frame = String::new();
    if let Some(event_type) = event_type {
        let _ = writeln!(frame, "event: {event_type}");
    }
    for line in event_data.split('\n') {
        let _ = writeln!(frame, "data: {line}");
    }
    frame.push('\n');
    bytes::Bytes::from(frame)
}

/// The error a writer returns once the peer has left.
fn disconnected() -> RuntimeError {
    RuntimeError::Io(std::io::Error::new(
        std::io::ErrorKind::BrokenPipe,
        "SSE client disconnected",
    ))
}

/// Writer for Server-Sent Events over a long-lived HTTP connection.
///
/// Each call to `event()` sends an SSE-formatted message through an mpsc
/// channel feeding a streaming hyper response body. Returns an error when
/// the client disconnects (receiver dropped).
pub struct SseWriter {
    tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
}

impl SseWriter {
    pub(crate) fn new(tx: tokio::sync::mpsc::Sender<bytes::Bytes>) -> Self {
        Self { tx }
    }

    /// Write an SSE event with the given type and data.
    ///
    /// Multi-line data is split into separate `data:` lines per the SSE spec.
    /// Produces: `event: {event_type}\ndata: {line1}\ndata: {line2}\n\n`
    pub fn event(&mut self, event_type: &str, event_data: &str) -> Result<(), RuntimeError> {
        self.send_raw(event_frame(Some(event_type), event_data))
    }

    /// Write an SSE event with data only, which clients dispatch as `message`.
    ///
    /// Produces: `data: {line1}\ndata: {line2}\n\n`
    pub fn data(&mut self, event_data: &str) -> Result<(), RuntimeError> {
        self.send_raw(event_frame(None, event_data))
    }

    /// Write an SSE comment to detect client disconnect.
    ///
    /// Produces: `:\n\n` which SSE clients silently ignore.
    pub fn comment(&mut self) -> Result<(), RuntimeError> {
        self.send_raw(bytes::Bytes::from_static(COMMENT_FRAME))
    }

    fn send_raw(&self, payload: bytes::Bytes) -> Result<(), RuntimeError> {
        self.tx.blocking_send(payload).map_err(|_| disconnected())
    }
}

/// Async writer for Server-Sent Events, owned by the handler's future.
///
/// The async twin of [`SseWriter`]: the same frames, sent without blocking a
/// thread. Each call waits for room in the feed's bounded event channel and
/// returns an error once the client has disconnected.
pub struct AsyncSseWriter {
    tx: tokio::sync::mpsc::Sender<bytes::Bytes>,
}

impl AsyncSseWriter {
    pub(crate) fn new(tx: tokio::sync::mpsc::Sender<bytes::Bytes>) -> Self {
        Self { tx }
    }

    /// Write an SSE event with the given type and data.
    ///
    /// Produces: `event: {event_type}\ndata: {line1}\ndata: {line2}\n\n`
    pub async fn event(&self, event_type: &str, event_data: &str) -> Result<(), RuntimeError> {
        self.send_raw(event_frame(Some(event_type), event_data))
            .await
    }

    /// Write an SSE event with data only, which clients dispatch as `message`.
    ///
    /// Produces: `data: {line1}\ndata: {line2}\n\n`
    pub async fn data(&self, event_data: &str) -> Result<(), RuntimeError> {
        self.send_raw(event_frame(None, event_data)).await
    }

    /// Write an SSE comment, as a keepalive or to detect client disconnect.
    ///
    /// Produces: `:\n\n` which SSE clients silently ignore.
    pub async fn comment(&self) -> Result<(), RuntimeError> {
        self.send_raw(bytes::Bytes::from_static(COMMENT_FRAME))
            .await
    }

    async fn send_raw(&self, payload: bytes::Bytes) -> Result<(), RuntimeError> {
        self.tx.send(payload).await.map_err(|_| disconnected())
    }
}
