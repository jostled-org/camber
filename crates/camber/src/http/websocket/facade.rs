use bytes::Bytes;
use std::ops::ControlFlow;
use std::time::Duration;

use super::deadline::ReceiveDeadline;
use super::message::{WsMessage, WsReceive};
use super::receiver::WsReceiver;
use super::sender::WsSender;
use crate::RuntimeError;

/// Bidirectional WebSocket connection for async callback code.
///
/// The convenience facade over the connection's two real owners. Its receive
/// methods answer `None` for every way a connection can end, and its sends
/// report a closed connection as a broken pipe. Callers that need the cause
/// use the typed endpoints from [`Self::split`].
///
/// Every waiting method is a future. The text and binary receives discard the
/// opposite message kind on purpose, and a discarded message stays discarded
/// when the receive is cancelled afterwards. [`Self::recv_message`] discards
/// nothing, so cancelling it loses nothing.
///
/// [`Self::sender`] hands out an independent send capability without giving up
/// the receive owner. [`Self::split`] gives up the facade and makes both owners
/// explicit, which is what work that must outlive the callback needs.
pub struct WsConn {
    sender: WsSender,
    receiver: WsReceiver,
}

impl std::fmt::Debug for WsConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsConn").finish_non_exhaustive()
    }
}

impl WsConn {
    pub(crate) fn new(sender: WsSender, receiver: WsReceiver) -> Self {
        Self { sender, receiver }
    }

    /// A send capability that does not consume this connection.
    ///
    /// The returned handle enqueues through the same bounded outbound queue
    /// this facade's own sends use, so a caller can hand sending to independent
    /// work and keep receiving here.
    pub fn sender(&self) -> WsSender {
        self.sender.clone()
    }

    /// Give up the facade for the two owners underneath it.
    ///
    /// Both halves keep the connection live for as long as they exist, so a
    /// callback that moves them into owned work may return without ending the
    /// connection.
    pub fn split(self) -> (WsSender, WsReceiver) {
        (self.sender, self.receiver)
    }

    /// Receive the next text message. Returns `None` when the connection ends.
    /// Skips binary messages.
    pub async fn recv(&mut self) -> Option<Box<str>> {
        self.classified(text_answer).await
    }

    /// Receive the next text message within `timeout`.
    ///
    /// Returns `Ok(None)` when the connection ends and skips binary messages
    /// like [`Self::recv`]. The deadline is fixed once, when the future is first
    /// polled, and covers the whole call: a skipped message does not extend it.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Timeout`] when no text message arrives and the
    /// connection has not ended before the deadline, and
    /// [`RuntimeError::NoRuntime`] when the call has to wait with no Tokio
    /// runtime entered.
    pub async fn recv_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<Box<str>>, RuntimeError> {
        ReceiveDeadline::after(timeout)
            .bound(self.classified(text_answer))
            .await
    }

    /// Receive the next binary message. Returns `None` when the connection
    /// ends. Skips text messages.
    pub async fn recv_binary(&mut self) -> Option<Bytes> {
        self.classified(binary_answer).await
    }

    /// Receive the next text or binary message. Returns `None` when the
    /// connection ends.
    pub async fn recv_message(&mut self) -> Option<WsMessage> {
        self.classified(any_answer).await
    }

    /// Take receives until one classifier settles on an answer.
    ///
    /// The loop every receiver on this facade runs, written once: a classifier
    /// that skips a message keeps the loop going, and one that settles ends it
    /// with what it decided. A timed caller wraps this whole loop in its one
    /// deadline, so skipping cannot restart it.
    async fn classified<T>(
        &mut self,
        answer: fn(WsReceive) -> ControlFlow<Option<T>>,
    ) -> Option<T> {
        loop {
            match answer(self.receiver.next_receive().await) {
                ControlFlow::Break(received) => return received,
                ControlFlow::Continue(()) => {}
            }
        }
    }

    /// Send a text message to the peer.
    ///
    /// Takes `&self`: the send half needs no exclusive access, so a `&WsConn`
    /// can go to a send-only helper while the receive half above keeps the
    /// `&mut self` it genuinely needs.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Io`] with [`std::io::ErrorKind::BrokenPipe`]
    /// once the connection has ended. Use [`Self::sender`] and
    /// [`WsSender::send`] when the cause matters.
    pub async fn send(&self, text: &str) -> Result<(), RuntimeError> {
        closed_as_broken_pipe(self.sender.send(text).await)
    }

    /// Send a binary message to the peer.
    ///
    /// # Errors
    ///
    /// The same as [`Self::send`].
    pub async fn send_binary(&self, data: &[u8]) -> Result<(), RuntimeError> {
        closed_as_broken_pipe(self.sender.send_binary(data).await)
    }
}

/// One receive's answer to a caller that asked for text.
///
/// `Break` settles the receive — with the payload, or with `None` for the end
/// of the connection. `Continue` is the skip a caller applies to a payload kind
/// it did not ask for. Stating it once per payload kind is what keeps the
/// untimed and the timed receiver from drifting apart.
fn text_answer(received: WsReceive) -> ControlFlow<Option<Box<str>>> {
    match received {
        WsReceive::Message(WsMessage::Text(text)) => ControlFlow::Break(Some(text)),
        WsReceive::Message(WsMessage::Binary(_)) => ControlFlow::Continue(()),
        WsReceive::Closed(_) => ControlFlow::Break(None),
    }
}

/// The same three decisions for a caller that asked for binary.
fn binary_answer(received: WsReceive) -> ControlFlow<Option<Bytes>> {
    match received {
        WsReceive::Message(WsMessage::Binary(data)) => ControlFlow::Break(Some(data)),
        WsReceive::Message(WsMessage::Text(_)) => ControlFlow::Continue(()),
        WsReceive::Closed(_) => ControlFlow::Break(None),
    }
}

/// The same three decisions for a caller that takes either payload kind.
fn any_answer(received: WsReceive) -> ControlFlow<Option<WsMessage>> {
    match received {
        WsReceive::Message(message) => ControlFlow::Break(Some(message)),
        WsReceive::Closed(_) => ControlFlow::Break(None),
    }
}

/// Map a typed closure back to the broken pipe this facade has always reported.
///
/// Only the closure is remapped. Any other error is not a connection that
/// ended, and reporting it as a broken pipe would hide it.
fn closed_as_broken_pipe(outcome: Result<(), RuntimeError>) -> Result<(), RuntimeError> {
    match outcome {
        Err(RuntimeError::WebSocketClosed(_)) => Err(RuntimeError::Io(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "WebSocket client disconnected",
        ))),
        other => other,
    }
}
