//! The proxied WebSocket bridge: two transports, forwarded frames, one owner.
//!
//! Separate from the direct bridge on purpose. A proxied connection has no
//! application queues, no terminal cause an application reads, and no receive
//! owner — it has a second WebSocket, and its lifecycle is what the two peers
//! do to each other. The two bridges share framing and handshake substrate and
//! nothing else.

use super::super::Request;
use super::super::body::HyperResponseBody;
use super::super::proxy_upstream::ProxyUpstream;
use super::super::rejection::Rejected;
use super::super::server_lifecycle::{ConnectionLifecycle, ConnectionPermit, ServerControl};
use super::backend::{
    BackendHandshake, BackendTarget, BackendTrust, BackendWs, NegotiatedBackend,
    ValidatedBackendUpgrade, forwarded_offer_headers,
};
use super::framing::{
    WsError, WsFrame, WsFrameMessage, close_transport, drain_until_close, flush_transport,
    next_control, next_frame, send_close, shutdown_client_transport, until_abort,
};
use super::handoff::{WsHandoffOutcome, WsRefusal, prepare_ws_handoff};
use super::handshake::WsUpgrade;
use super::ownership::{BridgeAttachment, ClientWs, open_bridge};
use std::ops::ControlFlow;
use std::sync::Arc;

/// Admit the client's offer, negotiate it with the backend, and only then
/// build the `101`, register the bridge, and return it.
///
/// The backend is reached and validated on the request task, inside the
/// request total, and under the deadlines the route froze. Backend negotiation
/// failures become mapped Proxy refusals. Inbound validation, request-total
/// expiry, and bridge registration retain their own rejection categories.
/// The peer never sees a `101` for a backend that refused, broke its handshake,
/// or never answered. The `101` names only what the backend selected, and the
/// registered bridge frames over the transport negotiated here — it never
/// dials a second time. Any exit before the bridge takes that transport drops
/// it, which releases the backend.
pub(in crate::http) async fn handle_proxy_ws(
    ws_upgrade: WsUpgrade,
    req: Request,
    backend: Arc<str>,
    prefix: Arc<str>,
    upstream: &ProxyUpstream,
    lifecycle: &ConnectionLifecycle,
) -> Result<hyper::Response<HyperResponseBody>, WsRefusal> {
    let offer = ws_upgrade.admit(&req).map_err(WsRefusal::unnegotiated)?;
    let target = BackendTarget::resolve(req.raw_path_and_query(), &prefix, &backend)
        .map_err(backend_refusal)?;
    let NegotiatedBackend { upgrade, selection } =
        BackendHandshake::new(target, forwarded_offer_headers(&req), offer.protocols())
            .map_err(backend_refusal)?
            .negotiate(upstream, &BackendTrust::Public)
            .await
            .map_err(backend_refusal)?;
    let prepared = match prepare_ws_handoff(offer, selection, &req, lifecycle) {
        WsHandoffOutcome::Ready(prepared) => prepared,
        WsHandoffOutcome::Refused(refusal) => return Err(refusal),
    };
    prepared
        .register(lifecycle, move |on_upgrade, permit, attachment| {
            bridge_ws_proxy(on_upgrade, upgrade, attachment, permit)
        })
        .await
}

/// A backend that could not be targeted, offered, reached, or validated.
///
/// Nothing was selected: a protocol the backend never validly chose is not one
/// the refusal may report.
fn backend_refusal(failure: super::super::async_proxy::ProxyFailure) -> WsRefusal {
    WsRefusal::unnegotiated(Rejected::from_proxy_failure(failure))
}

/// Bridge frames bidirectionally between the client and the negotiated backend.
async fn bridge_ws_proxy(
    on_upgrade: hyper::upgrade::OnUpgrade,
    backend: ValidatedBackendUpgrade,
    attachment: BridgeAttachment,
    permit: Arc<ConnectionPermit>,
) {
    let opened = open_bridge(
        on_upgrade,
        attachment,
        "WebSocket proxy client upgrade failed",
    )
    .await;
    let (mut control, mut client_ws) = match opened {
        Some(opened) => opened,
        None => return,
    };
    let mut backend_ws = backend.into_websocket().await;
    let exit = forward_proxy_frames(&mut control, &mut client_ws, &mut backend_ws).await;
    settle_proxy_transports(exit, &mut client_ws, &mut backend_ws).await;
    drop(permit);
}

/// Forward frames in both directions until one side ends the bridge.
async fn forward_proxy_frames(
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    client_ws: &mut ClientWs,
    backend_ws: &mut BackendWs,
) -> ProxyExit {
    use futures_util::StreamExt;
    loop {
        let flow = tokio::select! {
            biased;
            mode = next_control(control) => {
                stop_proxy_bridge(mode, control, client_ws, backend_ws).await
            }
            message = client_ws.next() => {
                owes_close(forward_client_frame(message, client_ws, backend_ws).await)
            }
            message = backend_ws.next() => {
                owes_close(forward_backend_frame(message, client_ws).await)
            }
        };
        match flow {
            ControlFlow::Break(exit) => break exit,
            ControlFlow::Continue(()) => {}
        }
    }
}

/// End both transports according to what the framing loop left them owed.
async fn settle_proxy_transports(
    exit: ProxyExit,
    client_ws: &mut ClientWs,
    backend_ws: &mut BackendWs,
) {
    match exit {
        // The control arm closed both transports and drained the answering
        // closes already. Closing either again is a write after the close
        // frame, which the transport reports as a failure that never happened.
        ProxyExit::Settled => shutdown_client_transport(client_ws).await,
        ProxyExit::Owed => {
            close_transport(backend_ws).await;
            end_client_transport(client_ws).await;
        }
    }
}

/// What the proxy bridge's transports are still owed when its loop ends.
///
/// Only the graceful control arm performs the close handshake itself, and only
/// that arm knows it did; the teardown reads this answer rather than trying to
/// re-derive it from the transports.
enum ProxyExit {
    /// The control arm closed both sides and drained their answering closes.
    Settled,
    /// No close handshake was performed, so the teardown still owes both.
    Owed,
}

/// Label a frame-flow arm's answer: no arm but the graceful stop closes.
fn owes_close(flow: ControlFlow<()>) -> ControlFlow<ProxyExit> {
    match flow {
        ControlFlow::Break(()) => ControlFlow::Break(ProxyExit::Owed),
        ControlFlow::Continue(()) => ControlFlow::Continue(()),
    }
}

/// End the client transport the peer took over at the `101`.
///
/// A raw shutdown alone leaves the peer reading `1006`, the code for a
/// connection that simply dropped. Every post-commitment exit ends here
/// instead, so the peer is told the transport closed. The backend was
/// validated before the `101`, so no exit here is a backend that never
/// answered.
async fn end_client_transport(stream: &mut ClientWs) {
    send_close(stream).await;
    shutdown_client_transport(stream).await;
}

/// End the proxy bridge on a server control transition.
///
/// A graceful stop closes each side and waits for their answering closes, and
/// says so, so the teardown does not close them a second time. An abort takes
/// the transports away without a handshake, and a `Running` reaching here means
/// the control sender is gone; neither has closed anything.
///
/// The graceful handshake is bounded by the same control watch that asked for
/// it: a peer that answers nothing would otherwise hold this bridge past the
/// abort that the server's next transition — a cancellation, or its graceful
/// deadline expiring — published to it.
async fn stop_proxy_bridge<C, B>(
    mode: ServerControl,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    client: &mut tokio_tungstenite::WebSocketStream<C>,
    backend: &mut tokio_tungstenite::WebSocketStream<B>,
) -> ControlFlow<ProxyExit>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match mode {
        ServerControl::Graceful => {
            until_abort(control, graceful_close_proxy(client, backend)).await;
            ControlFlow::Break(ProxyExit::Settled)
        }
        ServerControl::Abort | ServerControl::Running => ControlFlow::Break(ProxyExit::Owed),
    }
}

/// Close both transports and wait for each peer's answering close.
async fn graceful_close_proxy<C, B>(
    client: &mut tokio_tungstenite::WebSocketStream<C>,
    backend: &mut tokio_tungstenite::WebSocketStream<B>,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    send_close(client).await;
    send_close(backend).await;
    drain_proxy_close(client, backend).await;
}

/// Forward one client frame to the backend.
///
/// A client close is forwarded and then answered: the bridge waits for the
/// backend's own close so both halves finish the handshake before the
/// transports go.
async fn forward_client_frame<C, B>(
    frame: WsFrame,
    client: &mut tokio_tungstenite::WebSocketStream<C>,
    backend: &mut tokio_tungstenite::WebSocketStream<B>,
) -> ControlFlow<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::SinkExt;
    let message = next_frame(frame, "WebSocket proxy client closed")?;
    let closes = message.is_close();
    match (backend.send(message).await, closes) {
        (Ok(()), false) => ControlFlow::Continue(()),
        (Ok(()), true) => {
            forward_backend_close(client, backend).await;
            ControlFlow::Break(())
        }
        (Err(error), _) => {
            tracing::debug!(%error, "WebSocket proxy backend send failed");
            ControlFlow::Break(())
        }
    }
}

/// Forward one backend frame to the client.
///
/// A backend close is forwarded and ends the bridge — the client half has
/// nothing further to carry once the origin has closed.
async fn forward_backend_frame<S>(frame: WsFrame, client: &mut S) -> ControlFlow<()>
where
    S: futures_util::Sink<WsFrameMessage, Error = WsError> + Unpin,
{
    use futures_util::SinkExt;
    let message = next_frame(frame, "WebSocket proxy backend closed")?;
    let closes = message.is_close();
    match (client.send(message).await, closes) {
        (Ok(()), false) => ControlFlow::Continue(()),
        (Ok(()), true) => ControlFlow::Break(()),
        (Err(error), _) => {
            tracing::debug!(%error, "WebSocket proxy client send failed");
            ControlFlow::Break(())
        }
    }
}

/// Wait for the backend's answering close, then flush what the client is owed.
///
/// One deliberate difference from the shape this replaces: the flush now also
/// runs when the backend stream errors or ends without a close. That is
/// harmless — the flush only pushes tungstenite's queued close reply, and the
/// `ProxyExit::Owed` teardown flushes the same transport again through
/// `send_close`.
async fn forward_backend_close<C, B>(
    client: &mut tokio_tungstenite::WebSocketStream<C>,
    backend: &mut tokio_tungstenite::WebSocketStream<B>,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    drain_until_close(backend).await;
    flush_transport(client).await;
}

async fn drain_proxy_close<C, B>(
    client: &mut tokio_tungstenite::WebSocketStream<C>,
    backend: &mut tokio_tungstenite::WebSocketStream<B>,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let ((), ()) = tokio::join!(drain_until_close(client), drain_until_close(backend));
}
