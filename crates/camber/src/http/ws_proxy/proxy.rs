//! The proxied WebSocket bridge: two transports, forwarded frames, one owner.
//!
//! Separate from the direct bridge on purpose. A proxied connection has no
//! application queues, no terminal cause an application reads, and no receive
//! owner — it has a second WebSocket, and its lifecycle is what the two peers
//! do to each other. The two bridges share framing and handshake substrate and
//! nothing else.

use super::super::Request;
use super::super::body::HyperResponseBody;
use super::super::rejection::Rejected;
use super::super::response::HeaderPair;
use super::super::server_lifecycle::{ConnectionLifecycle, ConnectionPermit, ServerControl};
use super::framing::{
    WsClose, WsError, WsFrame, WsFrameMessage, close_transport, drain_until_close, flush_transport,
    next_control, next_frame, send_close, shutdown_client_transport, until_abort,
};
use super::handoff::{WsHandoff, WsHandoffOutcome, WsRefusal, prepare_ws_handoff};
use super::handshake::WsUpgrade;
use super::ownership::{ClientWs, open_bridge, own_upgrade_bridge};
use std::ops::ControlFlow;
use std::sync::Arc;

/// Validate the upgrade pair, build the backend URL, spawn the bridge, return 101.
pub(in crate::http) async fn handle_proxy_ws(
    ws_upgrade: WsUpgrade,
    req: Request,
    backend: Arc<str>,
    prefix: Arc<str>,
    lifecycle: &ConnectionLifecycle,
) -> Result<hyper::Response<HyperResponseBody>, WsRefusal> {
    let prepared = match prepare_ws_handoff(ws_upgrade, &req, lifecycle) {
        WsHandoffOutcome::Ready(prepared) => prepared,
        WsHandoffOutcome::Refused(refusal) => return Err(refusal),
    };
    // Borrowed rather than owned: this bridge never takes the request, so the
    // selected protocol stays readable for as long as a refusal could name it.
    // Only a refusal allocates it, and an upgrade takes at most one of them.
    let subprotocol = prepared.subprotocol;

    let backend_ws_url = match build_backend_ws_url(req.raw_path_and_query(), &prefix, &backend) {
        Ok(url) => url,
        Err(rejected) => return Err(WsRefusal::negotiated(rejected, subprotocol)),
    };

    // The backend is offered the protocol the client was already promised, so
    // it cannot select a different one.
    let forwarded_headers = collect_forwardable_ws_headers(&req, subprotocol);
    let WsHandoff {
        on_upgrade,
        response,
        permit,
        handoff,
        ..
    } = prepared;
    own_upgrade_bridge(lifecycle, response, &handoff, move |attachment| {
        bridge_ws_proxy(
            on_upgrade,
            backend_ws_url,
            forwarded_headers,
            attachment,
            permit,
        )
    })
    .await
    .map_err(|rejected| WsRefusal::negotiated(rejected, subprotocol))
}

/// Collect headers safe to forward on a proxied WebSocket connection.
///
/// Forwards Authorization, Cookie, and non-forwarded X-* headers. The selected
/// subprotocol is appended separately so the backend cannot select a protocol
/// different from the client-facing commitment.
fn collect_forwardable_ws_headers(req: &Request, subprotocol: Option<&str>) -> Box<[HeaderPair]> {
    let headers = req
        .headers()
        .filter(|(name, _)| is_forwardable_ws_header(name))
        .map(|(name, value)| {
            (
                std::borrow::Cow::Owned(name.to_owned()),
                std::borrow::Cow::Owned(value.to_owned()),
            )
        });
    let selected = subprotocol.into_iter().map(|protocol| {
        (
            std::borrow::Cow::Borrowed("Sec-WebSocket-Protocol"),
            std::borrow::Cow::Owned(protocol.to_owned()),
        )
    });
    headers.chain(selected).collect()
}

/// A WS proxy header is forwardable if it is Authorization, Cookie,
/// a non-forwarded X-* header.
/// Other WebSocket handshake headers (sec-websocket-key, sec-websocket-version, etc.)
/// are excluded — the proxy generates its own.
fn is_forwardable_ws_header(name: &str) -> bool {
    match name {
        n if n.eq_ignore_ascii_case("authorization") => true,
        n if n.eq_ignore_ascii_case("cookie") => true,
        n if n.eq_ignore_ascii_case("sec-websocket-protocol") => false,
        n if n
            .get(..2)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("x-"))
            && !super::super::async_proxy::is_forwarded_metadata(n) =>
        {
            true
        }
        _ => false,
    }
}

/// Convert an HTTP backend URL + request path into a WebSocket URL.
fn build_backend_ws_url(path: &str, prefix: &str, backend: &str) -> Result<Box<str>, Rejected> {
    let remainder = match super::super::async_proxy::strip_prefix(path, prefix) {
        Some(remainder) => remainder,
        None => {
            // The one fault that check refuses. A path that simply does not
            // carry the prefix is returned whole, so it never arrives here.
            return Err(unbuildable_ws_target(
                super::super::async_proxy::TRAVERSAL_SEGMENT,
            ));
        }
    };
    match backend {
        s if s.starts_with("http://") => {
            Ok(format!("ws://{}{remainder}", &s["http://".len()..]).into_boxed_str())
        }
        s if s.starts_with("https://") => {
            Ok(format!("wss://{}{remainder}", &s["https://".len()..]).into_boxed_str())
        }
        _ => Err(unbuildable_ws_target(
            "the configured backend names no scheme this proxy can upgrade over",
        )),
    }
}

/// Refuse a proxied upgrade whose target this proxy cannot build.
///
/// Classified as the same proxy fault the buffered and streaming classes raise
/// on the same peer input, so a traversal probe reads one way across all three
/// and never as a backend outage.
fn unbuildable_ws_target(detail: &'static str) -> Rejected {
    Rejected::from_proxy_failure(super::super::async_proxy::ProxyFailure::UnbuildableTarget(
        detail,
    ))
}

/// Bridge frames bidirectionally between client and backend WebSocket connections.
async fn bridge_ws_proxy(
    on_upgrade: hyper::upgrade::OnUpgrade,
    backend_ws_url: Box<str>,
    forwarded_headers: Box<[HeaderPair]>,
    attachment: Option<super::ownership::BridgeAttachment>,
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

    // Past the dispatch commitment the peer holds an upgraded transport, so
    // every exit from here owes it the same close the framing loop's exits
    // give it — a backend that never answers is not a reason to drop the
    // client socket without one.
    let backend_request = match build_ws_backend_request(&backend_ws_url, &forwarded_headers) {
        Some(req) => req,
        None => {
            end_client_transport(&mut client_ws, Some(backend_fault_close())).await;
            return;
        }
    };

    let (mut backend_ws, _) = match tokio_tungstenite::connect_async(backend_request).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(url = %backend_ws_url, error = %e, "WebSocket proxy backend connection failed");
            end_client_transport(&mut client_ws, Some(backend_fault_close())).await;
            return;
        }
    };

    let exit = forward_proxy_frames(&mut control, &mut client_ws, &mut backend_ws).await;
    settle_proxy_transports(exit, &mut client_ws, &mut backend_ws).await;
    drop(permit);
}

/// Forward frames in both directions until one side ends the bridge.
async fn forward_proxy_frames<B>(
    control: &mut Option<tokio::sync::watch::Receiver<ServerControl>>,
    client_ws: &mut ClientWs,
    backend_ws: &mut tokio_tungstenite::WebSocketStream<B>,
) -> ProxyExit
where
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
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
async fn settle_proxy_transports<B>(
    exit: ProxyExit,
    client_ws: &mut ClientWs,
    backend_ws: &mut tokio_tungstenite::WebSocketStream<B>,
) where
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match exit {
        // The control arm closed both transports and drained the answering
        // closes already. Closing either again is a write after the close
        // frame, which the transport reports as a failure that never happened.
        ProxyExit::Settled => shutdown_client_transport(client_ws).await,
        ProxyExit::Owed => {
            close_transport(backend_ws).await;
            end_client_transport(client_ws, None).await;
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
/// instead, so the peer is told the transport closed; `reason` is what
/// distinguishes a backend fault from the ordinary end of frame flow.
async fn end_client_transport(stream: &mut ClientWs, reason: Option<WsClose>) {
    send_close(stream, reason).await;
    shutdown_client_transport(stream).await;
}

/// The close a peer is given when the backend, not the peer, ended the bridge.
///
/// `1011` is the server-side internal-error code: the handshake succeeded and
/// the fault is on Camber's side of the bridge, which is exactly what a peer
/// reading `1006` cannot tell from its own connection dropping.
fn backend_fault_close() -> WsClose {
    WsClose {
        code: tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode::Error,
        reason: tokio_tungstenite::tungstenite::Utf8Bytes::from_static(
            "WebSocket proxy backend unavailable",
        ),
    }
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
    control: &mut Option<tokio::sync::watch::Receiver<ServerControl>>,
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
    send_close(client, None).await;
    send_close(backend, None).await;
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

/// Build an HTTP request for the backend WebSocket connection with forwarded headers.
fn build_ws_backend_request(url: &str, headers: &[HeaderPair]) -> Option<hyper::Request<()>> {
    let uri: hyper::Uri = match url.parse() {
        Ok(u) => u,
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "WebSocket backend URI parse failed");
            return None;
        }
    };
    // A backend configured as an `http://` URL with no authority builds a
    // `ws:///…` this request can never be sent to. The peer is already past the
    // `101` by then, so it is answered with a `1011` close and nothing else:
    // named here, or an operator reads that close with no account of it at all.
    let host = match backend_host_header(&uri) {
        Some(host) => host,
        None => {
            tracing::warn!(url = %url, "WebSocket backend URL names no authority");
            return None;
        }
    };

    let mut builder = hyper::Request::builder()
        .uri(uri)
        .header("Host", &*host)
        .header("Connection", "Upgrade")
        .header("Upgrade", "websocket")
        .header("Sec-WebSocket-Version", "13")
        .header(
            "Sec-WebSocket-Key",
            tokio_tungstenite::tungstenite::handshake::client::generate_key(),
        );

    for (name, value) in headers {
        builder = builder.header(name.as_ref(), value.as_ref());
    }

    match builder.body(()) {
        Ok(req) => Some(req),
        Err(e) => {
            tracing::warn!(url = %url, error = %e, "WebSocket backend request build failed");
            None
        }
    }
}

/// The `Host` header a backend upgrade carries, or `None` for a URL that names
/// no authority.
///
/// Built from the host and the port rather than from the whole authority. An
/// authority also carries userinfo, so a backend configured as
/// `http://user:secret@internal:8080` would otherwise send its credentials in a
/// `Host` header — one strict backends reject, and one every intermediary and
/// access log downstream reads.
fn backend_host_header(uri: &hyper::Uri) -> Option<Box<str>> {
    let host = uri.host()?;
    match uri.port_u16() {
        Some(port) => Some(format!("{host}:{port}").into_boxed_str()),
        None => Some(Box::from(host)),
    }
}
