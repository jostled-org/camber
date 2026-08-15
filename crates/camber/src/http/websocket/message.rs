use bytes::Bytes;
use tokio_tungstenite::tungstenite::protocol::Message;

use super::terminal::WsCloseCause;

/// A WebSocket message — either UTF-8 text or raw binary.
///
/// The binary variant is one shared immutable buffer, both for what a peer sent
/// and for what a send admitted. It is moved from the caller through the
/// outbound queue into the transport frame and never copied along the way, so
/// one payload offered to many connections stays one allocation. The copy a
/// borrowed payload needs is taken at the public admission boundary, where the
/// borrow ends.
#[derive(Debug, Clone)]
pub enum WsMessage {
    Text(Box<str>),
    Binary(Bytes),
}

/// What one receive on a direct WebSocket answered.
///
/// Either the next application message, or the single reason this connection
/// ended. There is no third answer: a receive that finds the connection over
/// reports the cause every other half reads, and never an absence a caller has
/// to interpret.
#[derive(Debug, Clone)]
pub enum WsReceive {
    Message(WsMessage),
    Closed(WsCloseCause),
}

/// Turn one application message into the frame the transport writes.
///
/// Both arms reuse the caller's allocation: a `Box<str>` is already the buffer
/// `Utf8Bytes` wants, and `Bytes` is already the buffer a binary frame wants.
/// The copy a borrowed payload needs is taken once at the public admission
/// boundary instead, where the borrow actually ends.
///
/// The binary arm moves the same handle the caller gave the sender, so the
/// frame this hands back is backed by the caller's own allocation and not by a
/// copy of it. A conversion that copied here would undo the whole shared path,
/// one recipient at a time.
pub(crate) fn into_frame(message: WsMessage) -> Message {
    match message {
        WsMessage::Text(text) => Message::Text(String::from(text).into()),
        WsMessage::Binary(data) => Message::Binary(data),
    }
}

/// Take the application message out of one received frame.
///
/// `None` for every frame the application never sees. Ping, pong, and raw
/// frames belong to the transport, which answers them itself; a close frame is
/// the bridge's own signal and reaches the receive owner as a cause rather than
/// as a message.
pub(crate) fn from_frame(frame: Message) -> Option<WsMessage> {
    match frame {
        Message::Text(text) => Some(WsMessage::Text(Box::from(text.as_ref()))),
        Message::Binary(data) => Some(WsMessage::Binary(data)),
        Message::Ping(_) | Message::Pong(_) | Message::Close(_) | Message::Frame(_) => None,
    }
}
