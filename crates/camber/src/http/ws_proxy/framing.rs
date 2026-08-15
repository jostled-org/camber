//! The framing substrate both bridges frame over.
//!
//! Nothing here decides a lifecycle. These are the transport operations a
//! direct bridge and a proxied bridge both perform on a WebSocket — send a
//! close, flush what is queued, read until the answering close, take the frame
//! out of one stream item — written once so the two bridges cannot drift in how
//! they end a transport while keeping their own separate contracts for when.
//!
//! Each operation is bounded by the half it needs rather than by a whole
//! transport, because the direct bridge's two directions own opposite halves of
//! one split stream while the proxied bridge owns two whole ones.

use super::super::server_lifecycle::ServerControl;
use std::ops::ControlFlow;

/// A framed WebSocket message, in either direction.
///
/// Named for the framing layer it belongs to, and deliberately not `WsMessage`:
/// that name belongs to the public `WsMessage` enum in the sibling `websocket`
/// module, which is what handlers are given and carries a text or binary
/// payload and no control frames at all.
pub(super) type WsFrameMessage = tokio_tungstenite::tungstenite::protocol::Message;

/// What framing reports when it cannot carry or parse a frame.
pub(super) type WsError = tokio_tungstenite::tungstenite::Error;

/// What a WebSocket stream yields for one frame.
pub(super) type WsFrame = Option<Result<WsFrameMessage, WsError>>;

/// The close frame a peer is given when Camber ends a bridge.
pub(super) type WsClose = tokio_tungstenite::tungstenite::protocol::CloseFrame;

/// Build the close frame a peer is given.
///
/// One place turns a reason into a frame, so a reason and the frame it becomes
/// stay stated together and the two bridges cannot hand a peer differently
/// shaped closes. This owns the shape of a close, not every close sent:
/// `close_transport` ends a transport through the sink's own close, which
/// writes an empty close frame this never sees.
fn close_frame(reason: Option<WsClose>) -> WsFrameMessage {
    WsFrameMessage::Close(reason)
}

/// Send a close frame, reporting a failure the teardown cannot act on.
///
/// `None` is the ordinary end of a bridge, which carries no status code; a
/// frame is given only where the peer would otherwise have to guess at a fault
/// that was not its own.
pub(super) async fn send_close<S>(sink: &mut S, reason: Option<WsClose>)
where
    S: futures_util::Sink<WsFrameMessage, Error = WsError> + Unpin,
{
    use futures_util::SinkExt;
    match sink.send(close_frame(reason)).await {
        Ok(()) => {}
        Err(error) => tracing::debug!(%error, "WebSocket close frame send failed"),
    }
}

/// Close a WebSocket transport, reporting a failure the teardown cannot act on.
///
/// Not a spelling of `send_close(sink, None)`. The sink's own close writes the
/// same empty close frame, then keeps flushing until the handshake finishes,
/// and it counts a transport the peer already closed as success where
/// `send_close` reports that transport as a failure. A teardown with no reason
/// to give and no answering close to wait on ends here; a teardown that carries
/// a reason, or that goes on to drain the answer, ends at `send_close`.
pub(super) async fn close_transport<S>(sink: &mut S)
where
    S: futures_util::Sink<WsFrameMessage, Error = WsError> + Unpin,
{
    use futures_util::SinkExt;
    match sink.close().await {
        Ok(()) => {}
        Err(error) => tracing::debug!(%error, "WebSocket close failed"),
    }
}

/// Flush queued writes, reporting a failure the teardown cannot act on.
pub(super) async fn flush_transport<S>(sink: &mut S)
where
    S: futures_util::Sink<WsFrameMessage, Error = WsError> + Unpin,
{
    use futures_util::SinkExt;
    match sink.flush().await {
        Ok(()) => {}
        Err(error) => tracing::debug!(%error, "WebSocket flush failed"),
    }
}

/// Shut the socket under one WebSocket transport down.
///
/// Takes the whole transport rather than a half: this is the only operation
/// here that reaches past framing to the socket itself, which no split half
/// owns.
pub(super) async fn shutdown_client_transport<S>(stream: &mut tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    match stream.get_mut().shutdown().await {
        Ok(()) => {}
        Err(error) => tracing::debug!(%error, "WebSocket transport shutdown failed"),
    }
    tokio::task::yield_now().await;
}

/// Read one transport until its peer's close frame arrives.
///
/// A transport error and an ended stream stop the drain the same way a close
/// does: in all three there is no close frame still coming.
pub(super) async fn drain_until_close<S>(source: &mut S)
where
    S: futures_util::Stream<Item = Result<WsFrameMessage, WsError>> + Unpin,
{
    use futures_util::StreamExt;
    while let Some(result) = source.next().await {
        match result {
            Ok(message) if message.is_close() => return,
            Ok(_) => {}
            Err(error) => {
                tracing::debug!(%error, "WebSocket close drain failed");
                return;
            }
        }
    }
}

/// Take the frame out of a stream's next item.
///
/// A transport error and an ended stream are the same answer to a bridge —
/// there is nothing further to forward — so both break; only the error has
/// anything to report.
pub(super) fn next_frame(frame: WsFrame, context: &str) -> ControlFlow<(), WsFrameMessage> {
    match frame {
        Some(Ok(message)) => ControlFlow::Continue(message),
        Some(Err(error)) => {
            tracing::debug!(%error, "{context}");
            ControlFlow::Break(())
        }
        None => ControlFlow::Break(()),
    }
}

/// Wait for this server's next control transition away from `Running`.
pub(super) async fn next_control(
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
) -> ServerControl {
    awaited_control(control, |mode| mode != ServerControl::Running).await
}

/// Perform one teardown step for only as long as this server is still asking
/// for an orderly end.
///
/// Every step a bridge owes its peer ends when the peer says so: a close frame
/// is written when the peer reads it, and the answering close arrives when the
/// peer sends one. A peer that does neither is exactly what an abort — a
/// caller cancelling, or a graceful deadline that expired into one — is the
/// answer to, and a bridge the server is waiting on has to hear that answer
/// where it is parked. Without this bound the step outlasts the abort
/// published to it, and the connection ends only when the supervisor takes it
/// away a second whole deadline later.
///
/// What the abort interrupts is owed to nobody afterwards. An aborted server
/// owes its peers no close, so a step abandoned here is one the abort has
/// already made pointless.
pub(super) async fn until_abort<F>(
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    step: F,
) where
    F: std::future::Future<Output = ()>,
{
    tokio::select! {
        biased;
        _ = awaited_control(control, |mode| mode == ServerControl::Abort) => {}
        () = step => {}
    }
}

/// Wait for a control mode that answers `reached`.
///
/// A sender that is gone is the last answer there will ever be — the server it
/// belonged to is no longer there to ask for anything — so whatever it last
/// published is what this returns.
async fn awaited_control(
    receiver: &mut tokio::sync::watch::Receiver<ServerControl>,
    reached: fn(ServerControl) -> bool,
) -> ServerControl {
    loop {
        let current = *receiver.borrow_and_update();
        if reached(current) {
            return current;
        }
        match receiver.changed().await {
            Ok(()) => {}
            Err(_) => return current,
        }
    }
}
