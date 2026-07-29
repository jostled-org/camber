use bytes::Bytes;
use std::ops::ControlFlow;
use std::time::Duration;
use tokio_tungstenite::tungstenite::protocol::Message;

use crate::RuntimeError;

/// A WebSocket message — either UTF-8 text or raw binary.
#[derive(Debug, Clone)]
pub enum WsMessage {
    Text(Box<str>),
    Binary(Bytes),
}

impl std::fmt::Debug for WsConn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WsConn").finish_non_exhaustive()
    }
}

/// Bidirectional WebSocket connection for sync handler code.
///
/// Wraps a tokio-tungstenite `WebSocketStream`, bridging async
/// send/recv to blocking calls via `block_in_place`.
pub struct WsConn {
    tx: tokio::sync::mpsc::Sender<Message>,
    rx: tokio::sync::mpsc::Receiver<Message>,
}

impl WsConn {
    pub(crate) fn new(
        tx: tokio::sync::mpsc::Sender<Message>,
        rx: tokio::sync::mpsc::Receiver<Message>,
    ) -> Self {
        Self { tx, rx }
    }

    /// Receive the next text message. Returns `None` when the peer closes.
    /// Skips binary, ping, and pong frames.
    pub fn recv(&mut self) -> Option<Box<str>> {
        self.recv_classified(classify_text)
    }

    /// Receive the next text message within `timeout`.
    ///
    /// Returns `Ok(None)` when the peer closes and skips binary, ping, and pong
    /// frames like [`Self::recv`].
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::Timeout`] when no text message or close frame is
    /// received before the deadline.
    pub fn recv_timeout(&mut self, timeout: Duration) -> Result<Option<Box<str>>, RuntimeError> {
        crate::runtime::block_on(async {
            tokio::time::timeout(timeout, async {
                loop {
                    match classify_text(self.rx.recv().await?) {
                        ControlFlow::Break(received) => return received,
                        ControlFlow::Continue(()) => {}
                    }
                }
            })
            .await
            .map_err(|_| RuntimeError::Timeout)
        })
    }

    /// Receive the next binary message. Returns `None` when the peer closes.
    /// Skips text, ping, and pong frames.
    pub fn recv_binary(&mut self) -> Option<Bytes> {
        self.recv_classified(classify_binary)
    }

    /// Receive the next text or binary message. Returns `None` when the
    /// peer closes. Skips ping and pong frames.
    pub fn recv_message(&mut self) -> Option<WsMessage> {
        self.recv_classified(classify_any)
    }

    /// Block until one classifier settles on an answer.
    ///
    /// The loop every blocking receiver runs, written once: a classifier that
    /// skips a frame keeps the loop going, and one that settles ends it with
    /// what it decided.
    fn recv_classified<T>(&mut self, classify: fn(Message) -> ControlFlow<Option<T>>) -> Option<T> {
        loop {
            match classify(self.rx.blocking_recv()?) {
                ControlFlow::Break(received) => return received,
                ControlFlow::Continue(()) => {}
            }
        }
    }

    /// Send a text message to the peer.
    ///
    /// Takes `&self`: the send half is a channel sender, which needs no
    /// exclusive access. A `&WsConn` can therefore go to a send-only helper, or
    /// be shared across threads to fan out, while the receive half above keeps
    /// the `&mut self` it genuinely needs.
    pub fn send(&self, text: &str) -> Result<(), RuntimeError> {
        self.send_message(Message::Text(text.into()))
    }

    /// Send a binary message to the peer.
    pub fn send_binary(&self, data: &[u8]) -> Result<(), RuntimeError> {
        self.send_message(Message::Binary(bytes::Bytes::copy_from_slice(data)))
    }

    fn send_message(&self, msg: Message) -> Result<(), RuntimeError> {
        self.tx.blocking_send(msg).map_err(|_| {
            RuntimeError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "WebSocket client disconnected",
            ))
        })
    }
}

/// One frame's answer for a caller that asked for text.
///
/// `Break` settles the receive — with the payload, or with `None` for the
/// peer's close. `Continue` is the skip every receiver applies to a frame it
/// did not ask for, ping and pong included. Stating it once per payload kind is
/// what keeps the blocking and the timed receiver from drifting apart, and what
/// gives a change of Ping/Pong policy a single site to land on.
fn classify_text(message: Message) -> ControlFlow<Option<Box<str>>> {
    match message {
        Message::Text(text) => ControlFlow::Break(Some(Box::from(text.as_ref()))),
        Message::Close(_) => ControlFlow::Break(None),
        _ => ControlFlow::Continue(()),
    }
}

/// The same three decisions for a caller that asked for binary.
fn classify_binary(message: Message) -> ControlFlow<Option<Bytes>> {
    match message {
        Message::Binary(data) => ControlFlow::Break(Some(data)),
        Message::Close(_) => ControlFlow::Break(None),
        _ => ControlFlow::Continue(()),
    }
}

/// The same three decisions for a caller that takes either payload kind.
fn classify_any(message: Message) -> ControlFlow<Option<WsMessage>> {
    match message {
        Message::Text(text) => ControlFlow::Break(Some(WsMessage::Text(Box::from(text.as_ref())))),
        Message::Binary(data) => ControlFlow::Break(Some(WsMessage::Binary(data))),
        Message::Close(_) => ControlFlow::Break(None),
        _ => ControlFlow::Continue(()),
    }
}
