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
///
/// A waiting send is a future, and admission is its only effect. A send that
/// is still pending has admitted nothing: dropping it drops the payload it was
/// offered and frees its place in line, and nothing enqueues that payload
/// later. Once admitted, the frame belongs to the bridge, and the terminal
/// cause alone decides what happens to it.
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
    /// The wait suspends the calling task and holds no thread, so it makes
    /// progress on a current-thread runtime as well as a multi-thread one.
    /// Dropping the future before it completes admits nothing.
    ///
    /// # Errors
    ///
    /// [`RuntimeError::WebSocketClosed`] once the connection has ended.
    pub async fn send(&self, text: &str) -> Result<(), RuntimeError> {
        self.live()?;
        self.admit(WsMessage::Text(Box::from(text))).await
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
    /// The borrowed payload is copied once per send, when the future is first
    /// polled, because the frame outlives this call by exactly as long as it
    /// sits in the queue. A producer that already owns immutable storage can
    /// give that storage to [`Self::send_shared_binary`] instead and pay no
    /// copy at all.
    ///
    /// # Errors
    ///
    /// The same as [`Self::send`].
    pub async fn send_binary(&self, data: &[u8]) -> Result<(), RuntimeError> {
        self.live()?;
        self.admit(WsMessage::Binary(Bytes::copy_from_slice(data)))
            .await
    }

    /// Send a binary message only if the outbound queue has a free slot now.
    ///
    /// Copies the borrowed payload once, exactly as [`Self::send_binary`] does.
    ///
    /// # Errors
    ///
    /// The same two as [`Self::try_send`].
    pub fn try_send_binary(&self, data: &[u8]) -> Result<(), RuntimeError> {
        self.live()?;
        self.try_admit(WsMessage::Binary(Bytes::copy_from_slice(data)))
    }

    /// Send shared immutable binary storage, waiting for outbound queue
    /// capacity.
    ///
    /// Taken by value, because success is this connection taking the handle:
    /// the frame it becomes outlives the call, and nothing here may copy the
    /// bytes to make that true. Cloning a [`Bytes`] changes a reference count,
    /// so one payload offered to many connections is one allocation however
    /// many recipients there are — and dropping the caller's own handle after
    /// admission leaves every queued clone valid.
    ///
    /// ```rust,no_run
    /// # use camber::http::{Bytes, WsSender};
    /// # async fn fan_out(payload: Bytes, recipients: &[WsSender]) -> Result<(), camber::RuntimeError> {
    /// for recipient in recipients {
    ///     recipient.send_shared_binary(payload.clone()).await?;
    /// }
    /// # Ok(())
    /// # }
    /// ```
    ///
    /// Success means the same thing every other send means: the frame entered
    /// this connection's bounded queue. Whether it is then written or dropped
    /// is the connection's terminal cause to decide, and either way the handle
    /// is released when the bridge is done with it. A send dropped while it
    /// waits releases the handle it holds, exactly as a refusal does.
    ///
    /// # Errors
    ///
    /// The same as [`Self::send`]. A refused call drops only the handle it was
    /// given; every other clone of that payload is untouched.
    pub async fn send_shared_binary(&self, data: Bytes) -> Result<(), RuntimeError> {
        self.live()?;
        self.admit(WsMessage::Binary(data)).await
    }

    /// Send shared immutable binary storage only if the outbound queue has a
    /// free slot now.
    ///
    /// Takes the handle by value and copies nothing, exactly as
    /// [`Self::send_shared_binary`] does.
    ///
    /// # Errors
    ///
    /// The same two as [`Self::try_send`]. A refused call drops only the handle
    /// it was given.
    pub fn try_send_shared_binary(&self, data: Bytes) -> Result<(), RuntimeError> {
        self.live()?;
        self.try_admit(WsMessage::Binary(data))
    }

    /// Whether this connection is still worth building a frame for.
    ///
    /// Asked by every public send before it takes the caller's payload. A
    /// borrowed payload becomes an owned frame only to be admitted, so a
    /// connection that has already ended would otherwise pay a heap copy per
    /// attempt for a frame nothing can write — the cost a fan-out producer that
    /// has not yet noticed the close pays on every send. A shared payload pays
    /// no copy, but the same check is what lets its refusal answer with this
    /// connection's cause before the handle is spent.
    ///
    /// The admissions below do not repeat it: nothing is awaited between this
    /// check and the queue operation. A closure that lands in that window is
    /// read from the queue's own refusal, or, once admitted, the frame belongs
    /// to the bridge and the terminal cause decides it.
    fn live(&self) -> Result<(), RuntimeError> {
        match self.terminal.cause() {
            Some(cause) => Err(RuntimeError::WebSocketClosed(cause)),
            None => Ok(()),
        }
    }

    /// Wait for a slot in the outbound queue and take it.
    ///
    /// The one bounded admission every waiting send goes through, after the
    /// caller's [`Self::live`] check. A closure that lands after that check is
    /// caught by the send's own failure, which reads the committed cause.
    ///
    /// The queue's own send is the whole wait. It holds the message in this
    /// future until a slot is free and then enqueues it in the same poll, so a
    /// future dropped while pending drops the message with it and leaves no
    /// reservation behind. No task is spawned to finish it.
    async fn admit(&self, message: WsMessage) -> Result<(), RuntimeError> {
        self.frames
            .send(message)
            .await
            .map_err(|_| RuntimeError::WebSocketClosed(self.terminal.committed()))
    }

    /// Take a slot in the outbound queue if one is free right now, after the
    /// caller's [`Self::live`] check. A closure that lands after that check is
    /// read by [`Self::refusal`].
    fn try_admit(&self, message: WsMessage) -> Result<(), RuntimeError> {
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
