use super::body::HyperResponseBody;
use super::response::HeaderPair;
use super::router::WsHandler;
use super::server_lifecycle::{
    ConnectionLifecycle, ConnectionPermit, ServerControl, UpgradeRegistrar, UpgradeRegistration,
};
use super::websocket::WsConn;
use super::{Request, Response};
use std::sync::Arc;

pub(super) enum WsUpgrade {
    Ready(hyper::upgrade::OnUpgrade, Box<str>),
    Rejected(WsHandshakeError),
}

pub(super) enum WsHandshakeError {
    BadRequest,
    UnsupportedVersion,
}

/// Extract the WebSocket upgrade future and accept key before consuming the request.
pub(super) fn extract_ws_upgrade(req: &mut hyper::Request<hyper::body::Incoming>) -> WsUpgrade {
    let accept_key = match validate_ws_handshake(req) {
        Ok(key) => tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes()),
        Err(error) => return WsUpgrade::Rejected(error),
    };
    WsUpgrade::Ready(hyper::upgrade::on(req), accept_key.into())
}

fn validate_ws_handshake(
    request: &hyper::Request<hyper::body::Incoming>,
) -> Result<&hyper::header::HeaderValue, WsHandshakeError> {
    if request.method() != hyper::Method::GET || request.version() != hyper::Version::HTTP_11 {
        return Err(WsHandshakeError::BadRequest);
    }
    let headers = request.headers();
    match (
        header_contains_token(headers, "connection", "upgrade"),
        single_header_equals(headers, "upgrade", "websocket"),
    ) {
        (true, true) => {}
        _ => return Err(WsHandshakeError::BadRequest),
    }
    validate_ws_version(headers)?;
    validate_ws_subprotocols(headers)?;

    let key = match single_header(headers, "sec-websocket-key") {
        Some(key) if valid_ws_key(key.as_bytes()) => key,
        _ => return Err(WsHandshakeError::BadRequest),
    };
    Ok(key)
}

fn validate_ws_subprotocols(headers: &hyper::HeaderMap) -> Result<(), WsHandshakeError> {
    for value in headers.get_all("sec-websocket-protocol") {
        let value = value.to_str().map_err(|_| WsHandshakeError::BadRequest)?;
        if !value.split(',').map(str::trim).all(is_http_token) {
            return Err(WsHandshakeError::BadRequest);
        }
    }
    Ok(())
}

fn validate_ws_version(headers: &hyper::HeaderMap) -> Result<(), WsHandshakeError> {
    let mut versions = headers.get_all("sec-websocket-version").iter();
    let version = match versions.next() {
        Some(version) => version,
        None => return Err(WsHandshakeError::BadRequest),
    };
    match (version == "13", versions.next()) {
        (true, None) => Ok(()),
        _ => Err(WsHandshakeError::UnsupportedVersion),
    }
}

fn single_header<'a>(
    headers: &'a hyper::HeaderMap,
    name: &'static str,
) -> Option<&'a hyper::header::HeaderValue> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => Some(value),
        _ => None,
    }
}

fn single_header_equals(headers: &hyper::HeaderMap, name: &'static str, expected: &str) -> bool {
    single_header(headers, name)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.eq_ignore_ascii_case(expected))
}

fn header_contains_token(headers: &hyper::HeaderMap, name: &'static str, expected: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .try_fold(false, |found, value| {
            value
                .to_str()
                .ok()?
                .split(',')
                .try_fold(found, |seen, token| {
                    let token = token.trim_matches([' ', '\t']);
                    is_http_token(token).then_some(seen || token.eq_ignore_ascii_case(expected))
                })
        })
        .is_some_and(|found| found)
}

fn valid_ws_key(key: &[u8]) -> bool {
    match key {
        [symbols @ .., b'=', b'='] if symbols.len() == 22 => {
            symbols.iter().copied().all(is_base64_symbol)
                && symbols
                    .last()
                    .copied()
                    .and_then(base64_value)
                    .is_some_and(|value| value & 0x0f == 0)
        }
        _ => false,
    }
}

const fn is_base64_symbol(byte: u8) -> bool {
    base64_value(byte).is_some()
}

const fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Check the WebSocket Origin header against the request Host.
///
/// Returns `None` if the origin is acceptable (missing or same-host).
/// Returns `Some(403 response)` if the origin is null, malformed, or cross-host.
pub(super) fn check_ws_origin(req: &Request) -> Option<Response> {
    let origin = match unique_request_header(req, "origin") {
        Ok(Some(origin)) => origin,
        Ok(None) => return None,
        Err(()) => return rejected_origin(),
    };
    let host = match unique_request_header(req, "host") {
        Ok(Some(host)) => host,
        Ok(None) | Err(()) => return rejected_origin(),
    };

    match origin_matches_host(origin, host) {
        true => None,
        false => rejected_origin(),
    }
}

fn unique_request_header<'a>(req: &'a Request, name: &str) -> Result<Option<&'a str>, ()> {
    let mut values = req
        .headers()
        .filter_map(|(candidate, value)| candidate.eq_ignore_ascii_case(name).then_some(value));
    match (values.next(), values.next()) {
        (None, None) => Ok(None),
        (Some(value), None) => Ok(Some(value)),
        _ => Err(()),
    }
}

fn rejected_origin() -> Option<Response> {
    Some(Response::text_raw(403, "WebSocket origin rejected"))
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let (scheme, origin_authority) = match origin.split_once("://") {
        Some(parts) => parts,
        None => return false,
    };
    let default_port = match scheme {
        value if value.eq_ignore_ascii_case("http") => 80,
        value if value.eq_ignore_ascii_case("https") => 443,
        _ => return false,
    };
    let origin = match parse_authority(origin_authority) {
        Some(authority) => authority,
        None => return false,
    };
    let host = match parse_authority(host) {
        Some(authority) => authority,
        None => return false,
    };
    let host_matches = origin
        .authority
        .host()
        .eq_ignore_ascii_case(host.authority.host());
    let port_matches = match host.port {
        Some(host_port) => host_port == origin.port.unwrap_or(default_port),
        None => origin
            .port
            .is_none_or(|origin_port| origin_port == default_port),
    };
    host_matches && port_matches
}

struct ParsedAuthority {
    authority: hyper::http::uri::Authority,
    port: Option<u16>,
}

fn parse_authority(value: &str) -> Option<ParsedAuthority> {
    match value
        .bytes()
        .any(|byte| matches!(byte, b'@' | b'/' | b'?' | b'#' | b',' | b' ' | b'\t'))
    {
        true => return None,
        false => {}
    }
    let authority: hyper::http::uri::Authority = value.parse().ok()?;
    match authority.host().is_empty() {
        true => return None,
        false => {}
    }
    let port = explicit_authority_port(&authority)?;
    Some(ParsedAuthority { authority, port })
}

fn explicit_authority_port(authority: &hyper::http::uri::Authority) -> Option<Option<u16>> {
    let value = authority.as_str();
    let has_separator = match value.starts_with('[') {
        true => value.contains("]:"),
        false => value.contains(':'),
    };
    match (has_separator, authority.port_u16()) {
        (false, None) => Some(None),
        (true, Some(port)) => Some(Some(port)),
        _ => None,
    }
}

/// Extract the upgrade pair when the request contained valid WS upgrade headers.
fn ws_upgrade_pair(
    ws_upgrade: WsUpgrade,
) -> Result<(hyper::upgrade::OnUpgrade, Box<str>), WsHandshakeError> {
    match ws_upgrade {
        WsUpgrade::Ready(on_upgrade, accept_key) => Ok((on_upgrade, accept_key)),
        WsUpgrade::Rejected(error) => Err(error),
    }
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
        Ok(pair) => pair,
        Err(error) => return Ok(ws_handshake_rejection(error)),
    };

    let subprotocol = extract_ws_subprotocol(&req);
    let response = ws_switching_protocols(accept_key.as_ref(), subprotocol);
    let permit = lifecycle.permit();
    match lifecycle.upgrade_registrar() {
        Some(registrar) => {
            let control = registrar.control();
            let dispatch_gate = registrar.dispatch_gate();
            let script = lifecycle.script();
            let (gate, start) = tokio::sync::oneshot::channel();
            let handle = spawn_gated_bridge(
                start,
                bridge_ws_handler(
                    on_upgrade,
                    handler,
                    req,
                    buffer_size,
                    DirectBridgeControl {
                        server: Some(control),
                        dispatch: Some(dispatch_gate),
                    },
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
                DirectBridgeControl {
                    server: None,
                    dispatch: None,
                },
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
struct DirectBridgeControl {
    server: Option<tokio::sync::watch::Receiver<ServerControl>>,
    dispatch: Option<super::server_lifecycle::UpgradeDispatchGate>,
}

async fn bridge_ws_handler(
    on_upgrade: hyper::upgrade::OnUpgrade,
    handler: WsHandler,
    req: Request,
    buffer_size: usize,
    mut control: DirectBridgeControl,
    script: Option<Arc<super::mock::LifecycleScript>>,
    permit: Arc<ConnectionPermit>,
) {
    let upgraded = match await_upgrade(on_upgrade, "WebSocket client upgrade failed").await {
        Some(u) => u,
        None => return,
    };

    let mut ws_stream = tokio_tungstenite::WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;

    let dispatch_committed = match control.dispatch {
        Some(gate) => gate.committed().await,
        None => true,
    };
    if !dispatch_committed {
        shutdown_client_transport(&mut ws_stream).await;
        return;
    }

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
    drop(tokio::task::spawn_blocking(move || {
        let conn = WsConn::new(outgoing_tx, incoming_rx);
        if let Err(e) = handler(&req, conn) {
            tracing::warn!(error = %e, "WebSocket handler returned error");
        }
    }));

    loop {
        tokio::select! {
            biased;
            mode = next_control(&mut control.server), if control.server.is_some() => {
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
        Ok(pair) => pair,
        Err(error) => return Ok(ws_handshake_rejection(error)),
    };

    let backend_ws_url = match build_backend_ws_url(req.raw_path_and_query(), &prefix, &backend) {
        Ok(url) => url,
        Err(resp) => return Ok(*resp),
    };

    let subprotocol = extract_ws_subprotocol(&req);
    let forwarded_headers = collect_forwardable_ws_headers(&req, subprotocol);
    let response = ws_switching_protocols(accept_key.as_ref(), subprotocol);
    let permit = lifecycle.permit();
    match lifecycle.upgrade_registrar() {
        Some(registrar) => {
            let control = registrar.control();
            let dispatch_gate = registrar.dispatch_gate();
            let (gate, start) = tokio::sync::oneshot::channel();
            let handle = spawn_gated_bridge(
                start,
                bridge_ws_proxy(
                    on_upgrade,
                    backend_ws_url,
                    forwarded_headers,
                    Some(control),
                    Some(dispatch_gate),
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

/// Select the first syntactically valid protocol offered by the client.
fn extract_ws_subprotocol(req: &Request) -> Option<&str> {
    req.headers()
        .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-protocol"))
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .find(|protocol| is_http_token(protocol))
}

fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
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
    dispatch_gate: Option<super::server_lifecycle::UpgradeDispatchGate>,
    permit: Arc<ConnectionPermit>,
) {
    let upgraded = match await_upgrade(on_upgrade, "WebSocket proxy client upgrade failed").await {
        Some(u) => u,
        None => return,
    };

    let mut client_ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
        hyper_util::rt::TokioIo::new(upgraded),
        tokio_tungstenite::tungstenite::protocol::Role::Server,
        None,
    )
    .await;

    let dispatch_committed = match dispatch_gate {
        Some(gate) => gate.committed().await,
        None => true,
    };
    if !dispatch_committed {
        shutdown_client_transport(&mut client_ws).await;
        return;
    }

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
    let host = uri.authority()?.as_str();

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

fn ws_handshake_rejection(error: WsHandshakeError) -> hyper::Response<HyperResponseBody> {
    let response = match error {
        WsHandshakeError::BadRequest => {
            Response::text_raw(400, "invalid WebSocket upgrade headers")
        }
        WsHandshakeError::UnsupportedVersion => {
            Response::text_raw(426, "unsupported WebSocket version")
                .with_header("Sec-WebSocket-Version", "13")
        }
    };
    super::handle::to_hyper_full(response)
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

    match builder.body(HyperResponseBody::Full(http_body_util::Full::new(
        bytes::Bytes::new(),
    ))) {
        Ok(response) => response,
        Err(err) => {
            tracing::error!("failed to build WebSocket 101 response: {err}");
            hyper::Response::new(HyperResponseBody::Full(http_body_util::Full::new(
                bytes::Bytes::new(),
            )))
        }
    }
}
