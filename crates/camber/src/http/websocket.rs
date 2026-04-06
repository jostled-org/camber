use bytes::Bytes;
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
        loop {
            let msg = self.rx.blocking_recv()?;
            match msg {
                Message::Text(text) => return Some(Box::from(text.as_ref())),
                Message::Close(_) => return None,
                _ => continue,
            }
        }
    }

    /// Receive the next binary message. Returns `None` when the peer closes.
    /// Skips text, ping, and pong frames.
    pub fn recv_binary(&mut self) -> Option<Bytes> {
        loop {
            let msg = self.rx.blocking_recv()?;
            match msg {
                Message::Binary(data) => return Some(Bytes::from(data)),
                Message::Close(_) => return None,
                _ => continue,
            }
        }
    }

    /// Receive the next text or binary message. Returns `None` when the
    /// peer closes. Skips ping and pong frames.
    pub fn recv_message(&mut self) -> Option<WsMessage> {
        loop {
            let msg = self.rx.blocking_recv()?;
            match msg {
                Message::Text(text) => {
                    return Some(WsMessage::Text(Box::from(text.as_ref())));
                }
                Message::Binary(data) => return Some(WsMessage::Binary(Bytes::from(data))),
                Message::Close(_) => return None,
                _ => continue,
            }
        }
    }

    /// Send a text message to the peer.
    pub fn send(&mut self, text: &str) -> Result<(), RuntimeError> {
        self.send_message(Message::Text(text.into()))
    }

    /// Send a binary message to the peer.
    pub fn send_binary(&mut self, data: &[u8]) -> Result<(), RuntimeError> {
        self.send_message(Message::Binary(data.to_vec()))
    }

    fn send_message(&mut self, msg: Message) -> Result<(), RuntimeError> {
        self.tx.blocking_send(msg).map_err(|_| {
            RuntimeError::Io(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "WebSocket client disconnected",
            ))
        })
    }
}
