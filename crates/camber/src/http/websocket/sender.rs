use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::TrySendError;

use super::message::WsMessage;
use super::terminal::TerminalState;
use crate::RuntimeError;

/// One send capability on a direct WebSocket.
///
/// Cloneable, `Send`, and `Sync`: sending needs no exclusive access, so any
/// number of these can move into independent work and enqueue through the one
/// bounded outbound queue the bridge drains. A clone copies this handle and
/// nothing else — no payload is shared, and no clone owns the transport, the
/// bridge, or the connection's permit.
///
/// Success means the frame entered that queue. Whether the bridge then writes
/// it or cancels it is decided by the connection's terminal cause, not by the
/// send that admitted it.
#[derive(Clone)]
pub struct WsSender {
    frames: Sender<WsMessage>,
    terminal: Arc<TerminalState>,
}

impl std::fmt::Debug for WsSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsSender").finish_non_exhaustive()
    }
}

impl WsSender {
    pub(crate) fn new(frames: Sender<WsMessage>, terminal: Arc<TerminalState>) -> Self {
        Self { frames, terminal }
    }

    /// Send a text message, waiting for outbound queue capacity.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::WebSocketClosed`] once the connection has ended, and
    /// [`RuntimeError::BlockingInAsyncContext`] when the caller is on a
    /// current-thread Tokio runtime, where waiting would stop the only thread
    /// that could free a slot.
    pub fn send(&self, text: &str) -> Result<(), RuntimeError> {
        self.live()?;
        self.admit(WsMessage::Text(Box::from(text)))
    }

    /// Send a text message only if the outbound queue has a free slot now.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::ChannelFull`] while the connection is live and its queue
    /// is full, and [`RuntimeError::WebSocketClosed`] once it has ended.
    pub fn try_send(&self, text: &str) -> Result<(), RuntimeError> {
        self.live()?;
        self.try_admit(WsMessage::Text(Box::from(text)))
    }

    /// Send a binary message, waiting for outbound queue capacity.
    ///
    /// The borrowed payload is copied once, here, because the frame outlives
    /// this call by exactly as long as it sits in the queue.
    ///
    /// # Errors
    ///
    /// The same two as [`Self::send`].
    pub fn send_binary(&self, data: &[u8]) -> Result<(), RuntimeError> {
        self.live()?;
        self.admit(WsMessage::Binary(Bytes::copy_from_slice(data)))
    }

    /// Send a binary message only if the outbound queue has a free slot now.
    ///
    /// # Errors
    ///
    /// The same two as [`Self::try_send`].
    pub fn try_send_binary(&self, data: &[u8]) -> Result<(), RuntimeError> {
        self.live()?;
        self.try_admit(WsMessage::Binary(Bytes::copy_from_slice(data)))
    }

    /// Whether this connection is still worth building a frame for.
    ///
    /// Asked by every public send before it copies the caller's payload. A
    /// borrowed payload becomes an owned frame only to be admitted, so a
    /// connection that has already ended would otherwise pay a heap copy per
    /// attempt for a frame nothing can write — the cost a fan-out producer that
    /// has not yet noticed the close pays on every send.
    ///
    /// A fast path only. The admissions below keep their own check, which is
    /// what closes the race with a closure that lands between the two.
    fn live(&self) -> Result<(), RuntimeError> {
        match self.terminal.cause() {
            Some(cause) => Err(RuntimeError::WebSocketClosed(cause)),
            None => Ok(()),
        }
    }

    /// Wait for a slot in the outbound queue and take it.
    ///
    /// The terminal check comes first so a connection that has already ended
    /// answers with its cause rather than with a channel result the caller
    /// would have to interpret. A closure that lands after that check is caught
    /// by the send's own failure, which reads the same committed cause.
    fn admit(&self, message: WsMessage) -> Result<(), RuntimeError> {
        self.live()?;
        crate::task::wait_blocking(|| self.frames.blocking_send(message))?
            .map_err(|_| RuntimeError::WebSocketClosed(self.terminal.committed()))
    }

    /// Take a slot in the outbound queue if one is free right now.
    fn try_admit(&self, message: WsMessage) -> Result<(), RuntimeError> {
        self.live()?;
        self.frames
            .try_send(message)
            .map_err(|refused| self.refusal(refused))
    }

    /// Why one non-blocking admission was refused.
    ///
    /// A full queue means backpressure only while the connection is live. The
    /// bridge commits its cause before it lets go of either queue end, so a
    /// refusal that finds a committed cause raced a closure rather than a
    /// consumer that is merely behind — and reporting that as fullness would
    /// invite a caller to retry a connection that is over.
    fn refusal(&self, refused: TrySendError<WsMessage>) -> RuntimeError {
        match (self.terminal.cause(), refused) {
            (Some(cause), _) => RuntimeError::WebSocketClosed(cause),
            (None, TrySendError::Full(_)) => RuntimeError::ChannelFull,
            (None, TrySendError::Closed(_)) => {
                RuntimeError::WebSocketClosed(self.terminal.committed())
            }
        }
    }
}
