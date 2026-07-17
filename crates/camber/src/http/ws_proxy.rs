use super::body::HyperResponseBody;
use super::response::HeaderPair;
use super::router::WsHandler;
use super::server_lifecycle::{
    ConnectionLifecycle, ConnectionPermit, ServerControl, UpgradeRegistrar, UpgradeRegistration,
};
use super::websocket::WsConn;
use super::{Request, Response};
use std::sync::Arc;

pub(super) type WsUpgrade = Option<(hyper::upgrade::OnUpgrade, Box<str>)>;

/// Extract the WebSocket upgrade future and accept key before consuming the request.
pub(super) fn extract_ws_upgrade(
    req: &mut hyper::Request<hyper::body::Incoming>,
) -> Option<(hyper::upgrade::OnUpgrade, Box<str>)> {
    let is_upgrade = req
        .headers()
        .get("upgrade")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    match is_upgrade {
        true => {
            let accept_key = req.headers().get("sec-websocket-key").map(|k| {
                tokio_tungstenite::tungstenite::handshake::derive_accept_key(k.as_bytes())
            });
            let on_upgrade = hyper::upgrade::on(req);
            accept_key.map(|key| (on_upgrade, key.into()))
        }
        false => None,
    }
}

/// Check the WebSocket Origin header against the request Host.
///
/// Returns `None` if the origin is acceptable (missing or same-host).
/// Returns `Some(403 response)` if the origin is null, malformed, or cross-host.
pub(super) fn check_ws_origin(req: &Request) -> Option<Response> {
    let origin = req.header("origin")?;

    let origin_authority = match origin {
        "null" => None,
        _ => parse_origin_authority(origin),
    };

    let accepted = match origin_authority {
        None => false,
        Some(auth) => auth == normalize_authority(req.header("host").unwrap_or("")),
    };

    match accepted {
        true => None,
        false => Some(Response::text_raw(403, "WebSocket origin rejected")),
    }
}

/// Parse the authority (host[:port]) from an Origin header value.
///
/// Origin format: `scheme://host[:port]`
/// Returns the normalized authority, or `None` if malformed.
fn parse_origin_authority(origin: &str) -> Option<std::borrow::Cow<'_, str>> {
    let sep = origin.find("://")?;
    let scheme = &origin[..sep];
    let after_scheme = &origin[sep + 3..];

    // Authority ends at the first `/` or end of string
    let authority = match after_scheme.find('/') {
        Some(pos) => &after_scheme[..pos],
        None => after_scheme,
    };

    match authority.is_empty() {
        true => None,
        false => Some(strip_default_port(authority, scheme)),
    }
}

/// Normalize an authority by stripping default ports.
///
/// Port 80 is default for http origins, port 443 for https.
/// Host headers have no scheme context, so only strip port if it matches
/// both common defaults (covers the typical case where the Host header
/// port matches the Origin's default port).
fn normalize_authority(authority: &str) -> std::borrow::Cow<'_, str> {
    // For Host headers (no scheme), strip port 80 and 443 as defaults
    strip_default_port(authority, "")
}

/// Strip the port from an authority if it is the default for the given scheme.
///
/// `http` default: 80. `https` default: 443.
/// Empty scheme strips both (used for Host header normalization).
fn strip_default_port<'a>(authority: &'a str, scheme: &str) -> std::borrow::Cow<'a, str> {
    // Handle IPv6 bracketed addresses: [::1]:port
    let (host_part, port_part) = match (
        authority.starts_with('['),
        authority.find("]:"),
        authority.rsplit_once(':'),
    ) {
        (true, Some(pos), _) => (&authority[..=pos], Some(&authority[pos + 2..])),
        (true, None, _) | (false, _, None) => (authority, None),
        (false, _, Some((h, p))) => (h, Some(p)),
    };

    let is_default = matches!(
        (port_part, scheme),
        (Some("80"), "http" | "") | (Some("443"), "https" | "")
    );

    match is_default {
        true => std::borrow::Cow::Borrowed(host_part),
        false => std::borrow::Cow::Borrowed(authority),
    }
}

/// Extract the upgrade pair when the request contained valid WS upgrade headers.
fn ws_upgrade_pair(ws_upgrade: WsUpgrade) -> Option<(hyper::upgrade::OnUpgrade, Box<str>)> {
    ws_upgrade
}

/// Validate the upgrade pair, spawn background work, return 101.
pub(super) async fn handle_ws_upgrade(
    ws_upgrade: WsUpgrade,
    handler: WsHandler,
    req: Request,
    buffer_size: usize,
    lifecycle: &ConnectionLifecycle,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let (on_upgrade, accept_key) = match ws_upgrade_pair(ws_upgrade) {
        Some(pair) => pair,
        None => return ws_missing_upgrade(),
    };

    let subprotocol = extract_ws_subprotocol(&req);
    let response = ws_switching_protocols(accept_key.as_ref(), subprotocol);
    let permit = lifecycle.permit();
    match lifecycle.upgrade_registrar() {
        Some(registrar) => {
            let control = registrar.control();
            let script = lifecycle.script();
            let (gate, start) = tokio::sync::oneshot::channel();
            let handle = spawn_gated_bridge(
                start,
                bridge_ws_handler(
                    on_upgrade,
                    handler,
                    req,
                    buffer_size,
                    Some(control),
                    script,
                    permit,
                ),
            );
            complete_upgrade_registration(registrar, handle, gate, response).await
        }
        None => {
            drop(crate::task::spawn_async(bridge_ws_handler(
                on_upgrade,
                handler,
                req,
                buffer_size,
                None,
                None,
                permit,
            )));
            Ok(response)
        }
    }
}

fn spawn_gated_bridge<F>(
    start: tokio::sync::oneshot::Receiver<()>,
    bridge: F,
) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        match start.await {
            Ok(()) => bridge.await,
            Err(_) => {}
        }
    })
}

/// Await the hyper upgrade, logging on failure.
async fn await_upgrade(
    on_upgrade: hyper::upgrade::OnUpgrade,
    context: &str,
) -> Option<hyper::upgrade::Upgraded> {
    match on_upgrade.await {
        Ok(u) => Some(u),
        Err(e) => {
            tracing::warn!(error = %e, "{context}");
            None
        }
    }
}

/// Await the upgrade then bridge async WS frames to a sync handler via channels.
async fn bridge_ws_handler(
    on_upgrade: hyper::upgrade::OnUpgrade,
    handler: WsHandler,
    req: Request,
    buffer_size: usize,
    mut control: Option<tokio::sync::watch::Receiver<ServerControl>>,
    script: Option<Arc<super::mock::LifecycleScript>>,
    permit: Arc<ConnectionPermit>,
) {
    let upgraded = match await_upgrade(on_upgrade, "WebSocket client upgrade failed").await {
        Some(u) => u,
        None => return,
    };

    let ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;

    if let Some(script) = &script {
        script
            .pause(super::mock::LifecycleCheckpoint::WebSocketOutgoingBufferConfigured(buffer_size))
            .await;
    }
    let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::channel::<
        tokio_tungstenite::tungstenite::protocol::Message,
    >(buffer_size);
    if let Some(script) = &script {
        script
            .pause(super::mock::LifecycleCheckpoint::WebSocketIncomingBufferConfigured(buffer_size))
            .await;
    }
    let (incoming_tx, incoming_rx) = tokio::sync::mpsc::channel::<
        tokio_tungstenite::tungstenite::protocol::Message,
    >(buffer_size);

    use futures_util::{SinkExt, StreamExt};
    let mut ws_stream = ws_stream;
    drop(tokio::task::spawn_blocking(move || {
        let conn = WsConn::new(outgoing_tx, incoming_rx);
        if let Err(e) = handler(&req, conn) {
            tracing::warn!(error = %e, "WebSocket handler returned error");
        }
    }));

    loop {
        tokio::select! {
            biased;
            mode = next_control(&mut control), if control.is_some() => {
                match mode {
                    ServerControl::Graceful => {
                        let _ = ws_stream
                            .send(tokio_tungstenite::tungstenite::Message::Close(None))
                            .await;
                        drain_direct_close(&mut ws_stream).await;
                    }
                    ServerControl::Abort | ServerControl::Running => {}
                }
                break;
            }
            outgoing = outgoing_rx.recv() => match outgoing {
                Some(message) => {
                    if ws_stream.send(message).await.is_err() {
                        break;
                    }
                }
                None => {
                    let _ = ws_stream.close(None).await;
                    break;
                }
            },
            incoming = ws_stream.next() => match incoming {
                Some(Ok(message)) if message.is_close() => {
                    let _ = ws_stream.flush().await;
                    break;
                }
                Some(Ok(message)) => {
                    if incoming_tx.send(message).await.is_err() {
                        break;
                    }
                }
                Some(Err(error)) => {
                    tracing::debug!(%error, "WebSocket client bridge closed");
                    break;
                }
                None => break,
            }
        }
    }
    shutdown_client_transport(&mut ws_stream).await;
    drop(permit);
}

/// Validate the upgrade pair, build the backend URL, spawn the bridge, return 101.
pub(super) async fn handle_proxy_ws(
    ws_upgrade: WsUpgrade,
    req: Request,
    backend: Arc<str>,
    prefix: Arc<str>,
    lifecycle: &ConnectionLifecycle,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let (on_upgrade, accept_key) = match ws_upgrade_pair(ws_upgrade) {
        Some(pair) => pair,
        None => return ws_missing_upgrade(),
    };

    let backend_ws_url = match build_backend_ws_url(req.raw_path_and_query(), &prefix, &backend) {
        Ok(url) => url,
        Err(resp) => return Ok(*resp),
    };

    let subprotocol = extract_ws_subprotocol(&req);
    let forwarded_headers = collect_forwardable_ws_headers(&req);
    let response = ws_switching_protocols(accept_key.as_ref(), subprotocol);
    let permit = lifecycle.permit();
    match lifecycle.upgrade_registrar() {
        Some(registrar) => {
            let control = registrar.control();
            let (gate, start) = tokio::sync::oneshot::channel();
            let handle = spawn_gated_bridge(
                start,
                bridge_ws_proxy(
                    on_upgrade,
                    backend_ws_url,
                    forwarded_headers,
                    Some(control),
                    permit,
                ),
            );
            complete_upgrade_registration(registrar, handle, gate, response).await
        }
        None => {
            drop(crate::task::spawn_async(bridge_ws_proxy(
                on_upgrade,
                backend_ws_url,
                forwarded_headers,
                None,
                permit,
            )));
            Ok(response)
        }
    }
}

async fn complete_upgrade_registration(
    registrar: UpgradeRegistrar,
    handle: tokio::task::JoinHandle<()>,
    gate: tokio::sync::oneshot::Sender<()>,
    response: hyper::Response<HyperResponseBody>,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    match registrar.submit(handle).await {
        UpgradeRegistration::Admitted => {
            let _ = gate.send(());
            Ok(response)
        }
        UpgradeRegistration::Rejected => Ok(super::server_lifecycle::rejected_response()),
        UpgradeRegistration::Unavailable => Ok(super::server_lifecycle::unavailable_response()),
    }
}

/// Extract the client's Sec-WebSocket-Protocol header for inclusion in the 101 response.
fn extract_ws_subprotocol(req: &Request) -> Option<&str> {
    req.headers()
        .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-protocol"))
        .map(|(_, v)| v)
}

/// Collect headers safe to forward on a proxied WebSocket connection.
///
/// Forwards Authorization, Cookie, Sec-WebSocket-Protocol, and non-forwarded
/// X-* headers. Excludes spoofable forwarding metadata and handshake headers
/// that the proxy regenerates itself.
fn collect_forwardable_ws_headers(req: &Request) -> Box<[HeaderPair]> {
    req.headers()
        .filter(|(name, _)| is_forwardable_ws_header(name))
        .map(|(name, value)| {
            (
                std::borrow::Cow::Owned(name.to_owned()),
                std::borrow::Cow::Owned(value.to_owned()),
            )
        })
        .collect()
}

/// A WS proxy header is forwardable if it is Authorization, Cookie,
/// Sec-WebSocket-Protocol (subprotocol negotiation), or a non-forwarded
/// X-* header.
/// Other WebSocket handshake headers (sec-websocket-key, sec-websocket-version, etc.)
/// are excluded — the proxy generates its own.
fn is_forwardable_ws_header(name: &str) -> bool {
    match name {
        n if n.eq_ignore_ascii_case("authorization") => true,
        n if n.eq_ignore_ascii_case("cookie") => true,
        n if n.eq_ignore_ascii_case("sec-websocket-protocol") => true,
        n if n
            .get(..2)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("x-"))
            && !super::async_proxy::is_forwarded_metadata(n) =>
        {
            true
        }
        _ => false,
    }
}

/// Convert an HTTP backend URL + request path into a WebSocket URL.
fn build_backend_ws_url(
    path: &str,
    prefix: &str,
    backend: &str,
) -> Result<Box<str>, Box<hyper::Response<HyperResponseBody>>> {
    let remainder = match super::async_proxy::strip_prefix(path, prefix) {
        Some(r) => r,
        None => {
            return Err(Box::new(super::handle::to_hyper_full(Response::text_raw(
                400,
                "invalid proxy path",
            ))));
        }
    };
    match backend {
        s if s.starts_with("http://") => {
            Ok(format!("ws://{}{remainder}", &s["http://".len()..]).into_boxed_str())
        }
        s if s.starts_with("https://") => {
            Ok(format!("wss://{}{remainder}", &s["https://".len()..]).into_boxed_str())
        }
        _ => Err(Box::new(super::handle::to_hyper_full(Response::text_raw(
            502,
            "unsupported backend scheme for WebSocket proxy",
        )))),
    }
}

/// Bridge frames bidirectionally between client and backend WebSocket connections.
async fn bridge_ws_proxy(
    on_upgrade: hyper::upgrade::OnUpgrade,
    backend_ws_url: Box<str>,
    forwarded_headers: Box<[HeaderPair]>,
    mut control: Option<tokio::sync::watch::Receiver<ServerControl>>,
    permit: Arc<ConnectionPermit>,
) {
    let upgraded = match await_upgrade(on_upgrade, "WebSocket proxy client upgrade failed").await {
        Some(u) => u,
        None => return,
    };

    let client_ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;

    let backend_request = match build_ws_backend_request(&backend_ws_url, &forwarded_headers) {
        Some(req) => req,
        None => return,
    };

    let (backend_ws, _) = match tokio_tungstenite::connect_async(backend_request).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::warn!(url = %backend_ws_url, error = %e, "WebSocket proxy backend connection failed");
            return;
        }
    };

    use futures_util::{SinkExt, StreamExt};
    let mut client_ws = client_ws;
    let mut backend_ws = backend_ws;
    loop {
        tokio::select! {
            biased;
            mode = next_control(&mut control), if control.is_some() => {
                match mode {
                    ServerControl::Graceful => {
                        let close = tokio_tungstenite::tungstenite::Message::Close(None);
                        let _ = client_ws.send(close.clone()).await;
                        let _ = backend_ws.send(close).await;
                        drain_proxy_close(&mut client_ws, &mut backend_ws).await;
                    }
                    ServerControl::Abort | ServerControl::Running => {}
                }
                break;
            }
            message = client_ws.next() => match message {
                Some(Ok(message)) => {
                    let closes = message.is_close();
                    if backend_ws.send(message).await.is_err() {
                        break;
                    }
                    if closes {
                        forward_backend_close(&mut client_ws, &mut backend_ws).await;
                        break;
                    }
                }
                Some(Err(error)) => {
                    tracing::debug!(%error, "WebSocket proxy client closed");
                    break;
                }
                None => break,
            },
            message = backend_ws.next() => match message {
                Some(Ok(message)) => {
                    let closes = message.is_close();
                    if client_ws.send(message).await.is_err() || closes {
                        break;
                    }
                }
                Some(Err(error)) => {
                    tracing::debug!(%error, "WebSocket proxy backend closed");
                    break;
                }
                None => break,
            }
        }
    }
    let _ = client_ws.close(None).await;
    let _ = backend_ws.close(None).await;
    shutdown_client_transport(&mut client_ws).await;
    drop(permit);
}

async fn forward_backend_close<C, B>(
    client: &mut tokio_tungstenite::WebSocketStream<C>,
    backend: &mut tokio_tungstenite::WebSocketStream<B>,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::{SinkExt, StreamExt};
    while let Some(result) = backend.next().await {
        match result {
            Ok(message) if message.is_close() => {
                let _ = client.flush().await;
                return;
            }
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

async fn shutdown_client_transport<S>(stream: &mut tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio::io::AsyncWriteExt;
    let _ = stream.get_mut().shutdown().await;
    tokio::task::yield_now().await;
}

async fn next_control(
    control: &mut Option<tokio::sync::watch::Receiver<ServerControl>>,
) -> ServerControl {
    let receiver = match control {
        Some(receiver) => receiver,
        None => return std::future::pending().await,
    };
    loop {
        let current = *receiver.borrow_and_update();
        if current != ServerControl::Running {
            return current;
        }
        match receiver.changed().await {
            Ok(()) => {}
            Err(_) => return current,
        }
    }
}

async fn drain_direct_close<S>(stream: &mut tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::StreamExt;
    while let Some(result) = stream.next().await {
        match result {
            Ok(message) if message.is_close() => return,
            Ok(_) => {}
            Err(_) => return,
        }
    }
}

async fn drain_proxy_close<C, B>(
    client: &mut tokio_tungstenite::WebSocketStream<C>,
    backend: &mut tokio_tungstenite::WebSocketStream<B>,
) where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::StreamExt;
    loop {
        tokio::select! {
            client_message = client.next() => match client_message {
                Some(Ok(message)) if !message.is_close() => {}
                _ => return,
            },
            backend_message = backend.next() => match backend_message {
                Some(Ok(message)) if !message.is_close() => {}
                _ => return,
            },
        }
    }
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
    let host = match uri.authority() {
        Some(auth) => auth.as_str(),
        None => return None,
    };

    let mut builder = hyper::Request::builder()
        .uri(url)
        .header("Host", host)
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

fn ws_missing_upgrade() -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    Ok(super::handle::to_hyper_full(Response::text_raw(
        400,
        "missing WebSocket upgrade headers",
    )))
}

fn ws_switching_protocols(
    accept_key: &str,
    subprotocol: Option<&str>,
) -> hyper::Response<HyperResponseBody> {
    let mut builder = hyper::Response::builder()
        .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Accept", accept_key);

    if let Some(proto) = subprotocol {
        builder = builder.header("Sec-WebSocket-Protocol", proto);
    }

    builder
        .body(HyperResponseBody::Full(http_body_util::Full::new(
            bytes::Bytes::new(),
        )))
        .unwrap_or_else(|err| {
            tracing::error!("failed to build WebSocket 101 response: {err}");
            hyper::Response::new(HyperResponseBody::Full(http_body_util::Full::new(
                bytes::Bytes::new(),
            )))
        })
}
