//! The public async WebSocket surface, entered through the crate exports.
//!
//! Type only: every waiting endpoint operation returns a `Send` future with
//! its exact result type, and every immediate operation stays synchronous.
//! No probe future is polled and no endpoint value is built, so
//! no runtime, socket, or peer exists. Progress, cancellation, timeout, and
//! ownership belong to the component and acceptance roots.

use std::future::IntoFuture;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::{Bytes, WsConn, WsMessage, WsReceive, WsReceiver, WsSender};

use crate::probes::{assert_send, require_send};

/// Compiles only when `probe` builds a future that can move across Tokio
/// workers from owned endpoint arguments.
fn require_send_probe<A, F>(probe: fn(A) -> F) -> fn(A) -> F
where
    F: Future<Output = Result<(), RuntimeError>> + Send,
{
    probe
}

fn require_shared_sender<T: Clone + Send + Sync>() {}

/// Waits on every sender operation; the immediate ones never wait.
async fn sender_operations(sender: WsSender) -> Result<(), RuntimeError> {
    let payload = Bytes::from_static(b"shared");
    let text: Result<(), RuntimeError> = require_send(sender.send("text").into_future()).await;
    let binary: Result<(), RuntimeError> =
        require_send(sender.send_binary(b"borrowed").into_future()).await;
    let shared: Result<(), RuntimeError> =
        require_send(sender.send_shared_binary(payload.clone()).into_future()).await;

    let immediate_text: Result<(), RuntimeError> = sender.try_send("text");
    let immediate_binary: Result<(), RuntimeError> = sender.try_send_binary(b"borrowed");
    let immediate_shared: Result<(), RuntimeError> = sender.try_send_shared_binary(payload);

    text.and(binary)
        .and(shared)
        .and(immediate_text)
        .and(immediate_binary)
        .and(immediate_shared)
}

/// Waits on both typed receives through the unique `&mut` receiver.
async fn receiver_operations(mut receiver: WsReceiver) -> Result<(), RuntimeError> {
    let untimed: Result<WsReceive, RuntimeError> =
        require_send(receiver.recv().into_future()).await;
    let timed: Result<WsReceive, RuntimeError> =
        require_send(receiver.recv_timeout(Duration::ZERO).into_future()).await;
    untimed.and(timed).map(drop)
}

/// Waits on every facade operation; `sender` and `split` never wait.
async fn facade_operations(mut conn: WsConn) -> Result<(), RuntimeError> {
    let text: Option<Box<str>> = require_send(conn.recv().into_future()).await;
    let binary: Option<Bytes> = require_send(conn.recv_binary().into_future()).await;
    let message: Option<WsMessage> = require_send(conn.recv_message().into_future()).await;
    drop((text, binary, message));
    let timed: Result<Option<Box<str>>, RuntimeError> =
        require_send(conn.recv_timeout(Duration::ZERO).into_future()).await;
    let text: Result<(), RuntimeError> = require_send(conn.send("text").into_future()).await;
    let binary: Result<(), RuntimeError> =
        require_send(conn.send_binary(b"borrowed").into_future()).await;

    let sibling: WsSender = conn.sender();
    let endpoints: (WsSender, WsReceiver) = conn.split();
    drop((sibling, endpoints));

    timed.map(drop).and(text).and(binary)
}

#[test]
fn async_websocket_public_operations_are_futures() {
    require_send_probe(sender_operations);
    require_send_probe(receiver_operations);
    require_send_probe(facade_operations);
    require_shared_sender::<WsSender>();
    assert_send::<WsReceiver>();
}
