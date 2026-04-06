use crate::RuntimeError;
use std::fmt::Write as FmtWrite;

impl std::fmt::Debug for SseWriter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SseWriter").finish_non_exhaustive()
    }
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
        let mut frame = String::new();
        let _ = writeln!(frame, "event: {event_type}");
        for line in event_data.split('\n') {
            let _ = writeln!(frame, "data: {line}");
        }
        frame.push('\n');
        self.send_raw(frame)
    }

    /// Write an SSE comment to detect client disconnect.
    ///
    /// Produces: `:\n\n` which SSE clients silently ignore.
    pub fn comment(&mut self) -> Result<(), RuntimeError> {
        self.send_raw(":\n\n".into())
    }

    fn send_raw(&self, payload: String) -> Result<(), RuntimeError> {
        self.tx
            .blocking_send(bytes::Bytes::from(payload))
            .map_err(|_| {
                RuntimeError::Io(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "SSE client disconnected",
                ))
            })
    }
}
