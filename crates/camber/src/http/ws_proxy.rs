use super::body::HyperResponseBody;
use super::response::HeaderPair;
use super::router::WsHandler;
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
pub(super) fn handle_ws_upgrade(
    ws_upgrade: WsUpgrade,
    handler: WsHandler,
    req: Request,
    buffer_size: usize,
) -> Result<hyper::Response<HyperResponseBody>, std::convert::Infallible> {
    let (on_upgrade, accept_key) = match ws_upgrade_pair(ws_upgrade) {
        Some(pair) => pair,
        None => return ws_missing_upgrade(),
    };

    let subprotocol = extract_ws_subprotocol(&req);
    let response = ws_switching_protocols(accept_key.as_ref(), subprotocol);
    let _task = crate::task::spawn_async(bridge_ws_handler(on_upgrade, handler, req, buffer_size));
    Ok(response)
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

    let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::channel::<
        tokio_tungstenite::tungstenite::protocol::Message,
    >(buffer_size);
    let (incoming_tx, incoming_rx) = tokio::sync::mpsc::channel::<
        tokio_tungstenite::tungstenite::protocol::Message,
    >(buffer_size);

    use futures_util::{SinkExt, StreamExt};
    let (mut ws_sink, mut ws_source) = ws_stream.split();

    let read_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_source.next().await {
            if incoming_tx.send(msg).await.is_err() {
                break;
            }
        }
    });

    let write_task = tokio::spawn(async move {
        while let Some(msg) = outgoing_rx.recv().await {
            if ws_sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = ws_sink.close().await;
    });

    let join_result = tokio::task::spawn_blocking(move || {
        let conn = WsConn::new(outgoing_tx, incoming_rx);
        if let Err(e) = handler(&req, conn) {
            tracing::warn!(error = %e, "WebSocket handler returned error");
        }
    })
    .await;

    if let Err(e) = join_result {
        tracing::warn!(error = %e, "WebSocket handler task panicked");
    }

    read_task.abort();
    // Let the write task drain remaining queued messages before closing.
    let _ = write_task.await;
}

/// Validate the upgrade pair, build the backend URL, spawn the bridge, return 101.
pub(super) fn handle_proxy_ws(
    ws_upgrade: WsUpgrade,
    req: Request,
    backend: Arc<str>,
    prefix: Arc<str>,
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
    let _task = crate::task::spawn_async(bridge_ws_proxy(
        on_upgrade,
        backend_ws_url,
        forwarded_headers,
    ));
    Ok(ws_switching_protocols(accept_key.as_ref(), subprotocol))
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
    let (mut client_sink, mut client_source) = client_ws.split();
    let (mut backend_sink, mut backend_source) = backend_ws.split();

    let c2b = tokio::spawn(async move {
        while let Some(Ok(msg)) = client_source.next().await {
            if backend_sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = backend_sink.close().await;
    });
    let c2b_abort = c2b.abort_handle();

    let b2c = tokio::spawn(async move {
        while let Some(Ok(msg)) = backend_source.next().await {
            if client_sink.send(msg).await.is_err() {
                break;
            }
        }
        let _ = client_sink.close().await;
    });
    let b2c_abort = b2c.abort_handle();

    tokio::select! {
        _ = c2b => { b2c_abort.abort(); }
        _ = b2c => { c2b_abort.abort(); }
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
