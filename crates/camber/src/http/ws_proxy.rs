use super::Request;
use super::body::HyperResponseBody;
use super::disconnect::DisconnectSignal;
use super::rejection::Rejected;
use super::response::HeaderPair;
use super::router::WsHandler;
use super::server_lifecycle::{
    ConnectionLifecycle, ConnectionPermit, ServerControl, UpgradeRegistrar, UpgradeRegistration,
};
use super::websocket::WsConn;
use std::ops::ControlFlow;
use std::sync::Arc;

/// The client-side WebSocket transport both bridges take over after the `101`.
type ClientWs =
    tokio_tungstenite::WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;

/// A framed WebSocket message, in either direction.
///
/// Named for the framing layer it belongs to, and deliberately not `WsMessage`:
/// that name belongs to the public `WsMessage` enum in the sibling `websocket`
/// module, which is what handlers are given and carries a text or binary
/// payload and no control frames at all.
type WsFrameMessage = tokio_tungstenite::tungstenite::protocol::Message;

/// What a WebSocket stream yields for one frame.
type WsFrame = Option<Result<WsFrameMessage, tokio_tungstenite::tungstenite::Error>>;

/// The close frame a peer is given when Camber ends a bridge.
type WsClose = tokio_tungstenite::tungstenite::protocol::CloseFrame;

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
    // `&&` rather than a tuple of both scans: the `Upgrade` header is the
    // cheap lookup and the one an ordinary request fails, so the token scan
    // over `Connection` never runs for traffic that was never a handshake.
    let asks_to_upgrade =
        is_ws_upgrade_head(headers) && header_contains_token(headers, "connection", "upgrade");
    match asks_to_upgrade {
        true => {}
        false => return Err(WsHandshakeError::BadRequest),
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

/// Whether a request head asks to leave HTTP for the WebSocket protocol.
///
/// The two routing predicates and the handshake validator all ask through
/// here, so they read a repeated `Upgrade` header the same way: a request
/// cannot be routed as an upgrade and then refused `400` by the validator for
/// a header the router was happy with.
pub(super) fn is_ws_upgrade_head(headers: &hyper::HeaderMap) -> bool {
    single_header_equals(headers, "upgrade", "websocket")
}

/// The same question, of a request whose head has already been collected.
pub(super) fn is_ws_upgrade_request(req: &Request) -> bool {
    single_value_equals(named_request_headers(req, "upgrade"), "websocket")
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
    // An unreadable value keeps its place in the count rather than being
    // filtered out: two values, one of them invalid, is still a repeat.
    single_value_equals(
        headers
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap_or("")),
        expected,
    )
}

/// Whether a header carries exactly one value, and that value is `expected`.
///
/// A repeated header is not a match. Both header representations — the borrowed
/// hyper map and a collected request — answer through this one rule.
fn single_value_equals<'a>(mut values: impl Iterator<Item = &'a str>, expected: &str) -> bool {
    match (values.next(), values.next()) {
        (Some(value), None) => value.eq_ignore_ascii_case(expected),
        _ => false,
    }
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
/// Returns the refusal the handshake earned otherwise.
///
/// Each arm states its own reason. A handshake that repeats `Origin`, one that
/// states no single `Host` to compare against, and one whose authorities
/// genuinely differ are three faults, and one sentence for all three tells an
/// operator something false about two of them.
pub(super) fn check_ws_origin(req: &Request) -> Option<Rejected> {
    let origin = match unique_request_header(req, "origin") {
        HeaderPresence::Absent => return None,
        HeaderPresence::Unique(origin) => origin,
        HeaderPresence::Repeated => {
            return rejected_origin("handshake carries more than one Origin header");
        }
    };
    let host = match unique_request_header(req, "host") {
        HeaderPresence::Unique(host) => host,
        HeaderPresence::Absent | HeaderPresence::Repeated => {
            return rejected_origin("handshake states no single Host to match the Origin against");
        }
    };

    match origin_matches_host(origin, host) {
        true => None,
        false => rejected_origin("handshake Origin does not match the requested Host"),
    }
}

/// How many values a request carries under one header name.
///
/// Three named answers rather than a `Result` with an empty error: a repeated
/// header and an absent one are different facts about the handshake, and each
/// of them is refused with its own account.
enum HeaderPresence<'a> {
    /// No value under this name.
    Absent,
    /// Exactly one value.
    Unique(&'a str),
    /// More than one value, so no single one names the request.
    Repeated,
}

fn unique_request_header<'a>(req: &'a Request, name: &'static str) -> HeaderPresence<'a> {
    let mut values = named_request_headers(req, name);
    match (values.next(), values.next()) {
        (Some(value), None) => HeaderPresence::Unique(value),
        (None, _) => HeaderPresence::Absent,
        (Some(_), Some(_)) => HeaderPresence::Repeated,
    }
}

/// Every value a collected request carries under one header name.
///
/// The name is `'static`, the same way `single_header`'s is: every caller
/// passes a literal, and unifying it with the request borrow would cap the
/// returned values — which come from the request alone — at the shorter of the
/// two lifetimes.
fn named_request_headers<'a>(
    req: &'a Request,
    name: &'static str,
) -> impl Iterator<Item = &'a str> {
    req.headers()
        .filter_map(move |(candidate, value)| candidate.eq_ignore_ascii_case(name).then_some(value))
}

fn rejected_origin(detail: &'static str) -> Option<Rejected> {
    Some(Rejected::ws_origin_rejected(detail))
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

/// What a validated handshake hands the bridge that will serve it.
///
/// The ordering both upgrade kinds depend on lives in the one function that
/// builds this: the permit is taken only once the `101` exists, so the arm that
/// cannot build one never holds a connection slot for an upgrade that will not
/// happen, and the disconnect handoff is captured before the request can move
/// into a bridge.
struct WsHandoff<'a> {
    on_upgrade: hyper::upgrade::OnUpgrade,
    subprotocol: Option<&'a str>,
    response: hyper::Response<HyperResponseBody>,
    permit: Arc<ConnectionPermit>,
    handoff: DisconnectSignal,
}

/// One refused upgrade, and what negotiation had established when it failed.
///
/// The subprotocol travels with the refusal because the request it was read
/// from moves into the bridge: this is the last point that can say whether
/// negotiation had selected one, and rejection context reports presence exactly
/// where an owner established it.
pub(super) struct WsRefusal {
    pub(super) rejected: Rejected,
    pub(super) subprotocol: Option<Box<str>>,
}

impl WsRefusal {
    /// A refusal found before subprotocol negotiation ran.
    fn unnegotiated(rejected: Rejected) -> Self {
        Self {
            rejected,
            subprotocol: None,
        }
    }

    /// A refusal found after negotiation settled on what it settled on.
    fn negotiated(rejected: Rejected, subprotocol: Option<&str>) -> Self {
        Self {
            rejected,
            subprotocol: subprotocol.map(Box::from),
        }
    }
}

/// What a handshake attempt leaves the caller holding.
///
/// Both arms are what the peer gets, not success against error: a `101` whose
/// bridge is still to be built, or the refusal that replaces it. Written as
/// its own enum rather than a `Result` because that is what it means, and
/// because `clippy::result_large_err` does not apply to it.
enum WsHandoffOutcome<'a> {
    /// The handshake stands; here is everything the `101` handoff needs.
    Ready(WsHandoff<'a>),
    /// The peer gets this instead: a rejected handshake, or a `101` that could
    /// not be built.
    Refused(WsRefusal),
}

/// Validate the handshake and build everything the `101` handoff needs.
///
/// Both upgrade kinds enter here, so neither can restate that ordering or
/// answer a refused handshake differently.
fn prepare_ws_handoff<'a>(
    ws_upgrade: WsUpgrade,
    req: &'a Request,
    lifecycle: &ConnectionLifecycle,
) -> WsHandoffOutcome<'a> {
    let (on_upgrade, accept_key) = match ws_upgrade_pair(ws_upgrade) {
        Ok(pair) => pair,
        Err(error) => {
            return WsHandoffOutcome::Refused(WsRefusal::unnegotiated(ws_handshake_rejection(
                error,
            )));
        }
    };
    let subprotocol = extract_ws_subprotocol(req);
    let response = match ws_switching_protocols(accept_key.as_ref(), subprotocol) {
        Ok(response) => response,
        Err(error) => {
            return WsHandoffOutcome::Refused(WsRefusal::negotiated(
                Rejected::ws_upgrade_unbuildable(error),
                subprotocol,
            ));
        }
    };
    WsHandoffOutcome::Ready(WsHandoff {
        on_upgrade,
        subprotocol,
        response,
        permit: lifecycle.permit(),
        handoff: req.on_disconnect(),
    })
}

/// Validate the upgrade pair, spawn background work, return 101.
pub(super) async fn handle_ws_upgrade(
    ws_upgrade: WsUpgrade,
    handler: WsHandler,
    req: Request,
    buffer_size: usize,
    lifecycle: &ConnectionLifecycle,
) -> Result<hyper::Response<HyperResponseBody>, WsRefusal> {
    let prepared = match prepare_ws_handoff(ws_upgrade, &req, lifecycle) {
        WsHandoffOutcome::Ready(prepared) => prepared,
        WsHandoffOutcome::Refused(refusal) => return Err(refusal),
    };
    // Taken as an owned value before the request moves into the bridge: a
    // registration refusal past this point still has to say what negotiation
    // had selected.
    let selected: Option<Box<str>> = prepared.subprotocol.map(Box::from);
    let WsHandoff {
        on_upgrade,
        response,
        permit,
        handoff,
        ..
    } = prepared;
    // Present only on the owned path, which is the same path that produces a
    // registrar: a synchronous lifecycle carries neither.
    let script = lifecycle.script();
    own_upgrade_bridge(lifecycle, response, &handoff, move |attachment| {
        bridge_ws_handler(
            on_upgrade,
            handler,
            req,
            buffer_size,
            attachment,
            script,
            permit,
        )
    })
    .await
    .map_err(|rejected| WsRefusal {
        rejected,
        subprotocol: selected,
    })
}

/// What an owned server contributes to a bridge it is about to register.
///
/// Absent exactly on the detached path, where no registrar exists to take
/// ownership of the bridge.
struct BridgeAttachment {
    control: tokio::sync::watch::Receiver<ServerControl>,
    dispatch: super::server_lifecycle::UpgradeDispatchGate,
}

impl BridgeAttachment {
    /// Spread an optional attachment over the parts a bridge holds separately.
    ///
    /// Stated once, beside the struct, so a field added here reaches both
    /// bridges. Split at each bridge instead, a bridge that forgot the new
    /// field would still compile — every part is an `Option`.
    fn split(
        attachment: Option<Self>,
    ) -> (
        Option<tokio::sync::watch::Receiver<ServerControl>>,
        Option<super::server_lifecycle::UpgradeDispatchGate>,
    ) {
        match attachment {
            Some(Self { control, dispatch }) => (Some(control), Some(dispatch)),
            None => (None, None),
        }
    }
}

/// Choose who owns the bridge, then resolve the response lifetime to match.
///
/// An owned server registers the bridge and commits the `101` only once its
/// registrar has admitted it; a detached connection has no scope to be
/// admitted into, so it commits at once. Every upgrade kind routes through
/// here, so a new one inherits the choice instead of restating it.
async fn own_upgrade_bridge<F, Fut>(
    lifecycle: &ConnectionLifecycle,
    response: hyper::Response<HyperResponseBody>,
    handoff: &DisconnectSignal,
    build_bridge: F,
) -> Result<hyper::Response<HyperResponseBody>, Rejected>
where
    F: FnOnce(Option<BridgeAttachment>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let registrar = match lifecycle.upgrade_registrar() {
        Some(registrar) => registrar,
        None => {
            detach_bridge(build_bridge(None));
            return Ok(commit_upgrade(response, handoff));
        }
    };
    let attachment = BridgeAttachment {
        control: registrar.control(),
        dispatch: registrar.dispatch_gate(),
    };
    let (gate, start) = tokio::sync::oneshot::channel();
    let handle = spawn_gated_bridge(start, build_bridge(Some(attachment)));
    complete_upgrade_registration(registrar, handle, gate, response, handoff).await
}

/// Launch a synchronous-entry WebSocket bridge detached.
///
/// The synchronous connection path carries no Camber runtime context by
/// contract — the connection task that owns this upgrade is itself detached —
/// so there is no root scope for the bridge to be admitted into. It inherits
/// that connection's contract rather than becoming an orphaned scope child.
///
/// `own_upgrade_bridge` is its only caller. It stays a named function because
/// `docs/scripts/check_no_orphan_spawns.sh` allowlists spawns by
/// `file:function`, never by file: this is the site that anchors the detached
/// contract. The module's two other allowlisted spawns are per-connection
/// rather than background subsystems — `spawn_gated_bridge` hands its join
/// handle straight to the registrar that owns it, and `bridge_ws_handler` runs
/// the handler's blocking thread for the life of its own bridge. A spawn at any
/// fourth site in this file is reported.
fn detach_bridge<F>(bridge: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    drop(tokio::spawn(bridge));
}

/// Resolve the response lifetime at a successful `101` handoff.
///
/// Past this point the transport belongs to the WebSocket close contract, so
/// this is Camber's last observation of the HTTP response. A `101` is excluded
/// from the body's generic empty-response completion precisely so this handoff
/// — not a rule about body length — owns the transition.
fn commit_upgrade(
    response: hyper::Response<HyperResponseBody>,
    handoff: &DisconnectSignal,
) -> hyper::Response<HyperResponseBody> {
    handoff.complete();
    response
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

/// Take over the client transport as a server-role WebSocket stream.
///
/// Both bridges start here, so the handshake role and the framing
/// configuration are stated once rather than restated per bridge kind.
async fn upgrade_client_ws(
    on_upgrade: hyper::upgrade::OnUpgrade,
    context: &str,
) -> Option<ClientWs> {
    let upgraded = await_upgrade(on_upgrade, context).await?;
    Some(
        tokio_tungstenite::WebSocketStream::from_raw_socket(
            hyper_util::rt::TokioIo::new(upgraded),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await,
    )
}

/// Wait for the connection to report whether the peer ever saw this `101`.
///
/// An uncommitted dispatch means the response never reached the wire, so the
/// transport is shut down rather than spoken WebSocket over. Both bridges gate
/// on this answer, so neither can start framing against a peer that is still
/// waiting on an HTTP response. A connection with no gate — the detached
/// path — has no such handoff to wait on.
async fn commit_dispatch(
    gate: Option<super::server_lifecycle::UpgradeDispatchGate>,
    stream: &mut ClientWs,
) -> ControlFlow<()> {
    let committed = match gate {
        Some(gate) => gate.committed().await,
        None => true,
    };
    match committed {
        true => ControlFlow::Continue(()),
        false => {
            shutdown_client_transport(stream).await;
            ControlFlow::Break(())
        }
    }
}

/// What a bridge holds once it is open: the control watch it stops on, and the
/// client transport it frames over.
type OpenBridge = (
    Option<tokio::sync::watch::Receiver<ServerControl>>,
    ClientWs,
);

/// Open a bridge: spread the attachment, take over the client transport, and
/// wait for the `101` to reach the wire.
///
/// The sequence, not the steps, is what a third bridge would get wrong — every
/// step below is already shared — so the sequence is written once. `None` is
/// both ways it can fail to open: an upgrade Hyper never completed, and a
/// dispatch the connection never committed. Neither leaves anything for the
/// caller to do, because both have already logged or shut the transport down.
async fn open_bridge(
    on_upgrade: hyper::upgrade::OnUpgrade,
    attachment: Option<BridgeAttachment>,
    context: &str,
) -> Option<OpenBridge> {
    let (control, dispatch) = BridgeAttachment::split(attachment);
    let mut stream = upgrade_client_ws(on_upgrade, context).await?;
    match commit_dispatch(dispatch, &mut stream).await {
        ControlFlow::Break(()) => None,
        ControlFlow::Continue(()) => Some((control, stream)),
    }
}

async fn bridge_ws_handler(
    on_upgrade: hyper::upgrade::OnUpgrade,
    handler: WsHandler,
    req: Request,
    buffer_size: usize,
    attachment: Option<BridgeAttachment>,
    script: Option<Arc<super::mock::LifecycleScript>>,
    permit: Arc<ConnectionPermit>,
) {
    let opened = open_bridge(on_upgrade, attachment, "WebSocket client upgrade failed").await;
    let (mut control, mut ws_stream) = match opened {
        Some(opened) => opened,
        None => return,
    };

    super::mock::LifecycleScript::pause_at(
        script.as_deref(),
        super::mock::LifecycleCheckpoint::WebSocketOutgoingBufferConfigured(buffer_size),
    )
    .await;
    let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::channel::<WsFrameMessage>(buffer_size);
    super::mock::LifecycleScript::pause_at(
        script.as_deref(),
        super::mock::LifecycleCheckpoint::WebSocketIncomingBufferConfigured(buffer_size),
    )
    .await;
    let (incoming_tx, incoming_rx) = tokio::sync::mpsc::channel::<WsFrameMessage>(buffer_size);

    use futures_util::StreamExt;
    // Detached by contract — no handle exists to carry a panic back — so the
    // structured record is the only report there is.
    drop(tokio::task::spawn_blocking(move || {
        let conn = WsConn::new(outgoing_tx, incoming_rx);
        match crate::task::catch_panic(move || handler(&req, conn)) {
            Ok(Ok(())) => {}
            Ok(Err(e)) => tracing::warn!(error = %e, "WebSocket handler returned error"),
            Err(error) => tracing::error!(%error, "WebSocket handler panicked"),
        }
    }));

    loop {
        let flow = tokio::select! {
            biased;
            mode = next_control(&mut control) => stop_direct_bridge(mode, &mut ws_stream).await,
            outgoing = outgoing_rx.recv() => forward_outgoing(outgoing, &mut ws_stream).await,
            incoming = ws_stream.next() => {
                forward_incoming(incoming, &mut ws_stream, &incoming_tx).await
            }
        };
        match flow {
            ControlFlow::Break(()) => break,
            ControlFlow::Continue(()) => {}
        }
    }
    shutdown_client_transport(&mut ws_stream).await;
    drop(permit);
}

/// End the direct bridge on a server control transition.
///
/// A graceful stop owes the peer the close handshake; an abort takes the
/// transport away without one, and a `Running` reaching here means the control
/// sender is gone, which ends the bridge just the same.
async fn stop_direct_bridge(mode: ServerControl, stream: &mut ClientWs) -> ControlFlow<()> {
    match mode {
        ServerControl::Graceful => graceful_close_direct(stream).await,
        ServerControl::Abort | ServerControl::Running => {}
    }
    ControlFlow::Break(())
}

/// Close the client transport and wait for the peer's answering close.
async fn graceful_close_direct(stream: &mut ClientWs) {
    send_close(stream, None).await;
    drain_until_close(stream).await;
}

/// Write one handler-produced message to the client.
///
/// A closed outgoing channel means the handler returned, so the bridge closes
/// the transport it was framing for.
async fn forward_outgoing(
    outgoing: Option<WsFrameMessage>,
    stream: &mut ClientWs,
) -> ControlFlow<()> {
    use futures_util::SinkExt;
    let message = match outgoing {
        Some(message) => message,
        None => {
            close_transport(stream).await;
            return ControlFlow::Break(());
        }
    };
    match stream.send(message).await {
        Ok(()) => ControlFlow::Continue(()),
        Err(error) => {
            tracing::debug!(%error, "WebSocket client send failed");
            ControlFlow::Break(())
        }
    }
}

/// Hand one client frame to the handler.
///
/// A close frame ends the bridge after the queued writes are flushed, so the
/// peer sees everything the handler produced before the transport goes.
async fn forward_incoming(
    incoming: WsFrame,
    stream: &mut ClientWs,
    handler_tx: &tokio::sync::mpsc::Sender<WsFrameMessage>,
) -> ControlFlow<()> {
    let message = next_frame(incoming, "WebSocket client bridge closed")?;
    match message.is_close() {
        true => {
            flush_transport(stream).await;
            ControlFlow::Break(())
        }
        false => queue_for_handler(handler_tx, message).await,
    }
}

/// Queue one frame for the handler thread.
///
/// A dropped receiver means the handler returned, so there is nothing left to
/// deliver frames to.
async fn queue_for_handler(
    handler_tx: &tokio::sync::mpsc::Sender<WsFrameMessage>,
    message: WsFrameMessage,
) -> ControlFlow<()> {
    match handler_tx.send(message).await {
        Ok(()) => ControlFlow::Continue(()),
        Err(_) => ControlFlow::Break(()),
    }
}

/// Take the frame out of a stream's next item.
///
/// A transport error and an ended stream are the same answer to a bridge —
/// there is nothing further to forward — so both break; only the error has
/// anything to report.
fn next_frame(frame: WsFrame, context: &str) -> ControlFlow<(), WsFrameMessage> {
    match frame {
        Some(Ok(message)) => ControlFlow::Continue(message),
        Some(Err(error)) => {
            tracing::debug!(%error, "{context}");
            ControlFlow::Break(())
        }
        None => ControlFlow::Break(()),
    }
}

/// Validate the upgrade pair, build the backend URL, spawn the bridge, return 101.
pub(super) async fn handle_proxy_ws(
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

/// Hand the bridge to the owned server's registrar, committing the `101` only
/// once the bridge is registered and owned.
///
/// A registrar-produced `503` or `500` is an ordinary HTTP response whose body
/// owns its own completion, so only the admitted arm resolves the handoff.
async fn complete_upgrade_registration(
    registrar: UpgradeRegistrar,
    handle: tokio::task::JoinHandle<()>,
    gate: tokio::sync::oneshot::Sender<()>,
    response: hyper::Response<HyperResponseBody>,
    handoff: &DisconnectSignal,
) -> Result<hyper::Response<HyperResponseBody>, Rejected> {
    match registrar.submit(handle).await {
        UpgradeRegistration::Admitted => release_admitted_bridge(gate, response, handoff),
        UpgradeRegistration::Rejected => Err(Rejected::upgrade_registration_refused()),
        UpgradeRegistration::Unavailable => Err(Rejected::upgrade_registration_unavailable()),
    }
}

/// Release the admitted bridge from its gate, then commit its `101`.
///
/// The gate's receiver lives inside the registered task, so a send failure has
/// one meaning: the supervisor aborted that task between admitting it and this
/// release. The bridge will never run, and a `101` committed for it would hand
/// the peer a transport nothing serves and resolve the response lifetime as
/// `Completed`. That race reports what it is — the upgrade could not be taken
/// up — through the same response the registrar's own unavailability produces.
fn release_admitted_bridge(
    gate: tokio::sync::oneshot::Sender<()>,
    response: hyper::Response<HyperResponseBody>,
    handoff: &DisconnectSignal,
) -> Result<hyper::Response<HyperResponseBody>, Rejected> {
    match gate.send(()) {
        Ok(()) => Ok(commit_upgrade(response, handoff)),
        Err(()) => Err(Rejected::upgrade_registration_unavailable()),
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
fn build_backend_ws_url(path: &str, prefix: &str, backend: &str) -> Result<Box<str>, Rejected> {
    let remainder = match super::async_proxy::strip_prefix(path, prefix) {
        Some(remainder) => remainder,
        None => {
            // The one fault that check refuses. A path that simply does not
            // carry the prefix is returned whole, so it never arrives here.
            return Err(unbuildable_ws_target(super::async_proxy::TRAVERSAL_SEGMENT));
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
    Rejected::from_proxy_failure(super::async_proxy::ProxyFailure::UnbuildableTarget(detail))
}

/// Bridge frames bidirectionally between client and backend WebSocket connections.
async fn bridge_ws_proxy(
    on_upgrade: hyper::upgrade::OnUpgrade,
    backend_ws_url: Box<str>,
    forwarded_headers: Box<[HeaderPair]>,
    attachment: Option<BridgeAttachment>,
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

    use futures_util::StreamExt;
    let exit = loop {
        let flow = tokio::select! {
            biased;
            mode = next_control(&mut control) => {
                stop_proxy_bridge(mode, &mut client_ws, &mut backend_ws).await
            }
            message = client_ws.next() => {
                owes_close(forward_client_frame(message, &mut client_ws, &mut backend_ws).await)
            }
            message = backend_ws.next() => {
                owes_close(forward_backend_frame(message, &mut client_ws).await)
            }
        };
        match flow {
            ControlFlow::Break(exit) => break exit,
            ControlFlow::Continue(()) => {}
        }
    };
    match exit {
        // The control arm closed both transports and drained the answering
        // closes already. Closing either again is a write after the close
        // frame, which the transport reports as a failure that never happened.
        ProxyExit::Settled => shutdown_client_transport(&mut client_ws).await,
        ProxyExit::Owed => {
            close_transport(&mut backend_ws).await;
            end_client_transport(&mut client_ws, None).await;
        }
    }
    drop(permit);
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
/// The same shape the direct bridge's arm has, over both transports: a
/// graceful stop closes each side and waits for their answering closes, and
/// says so, so the teardown does not close them a second time. An abort takes
/// the transports away without a handshake, and a `Running` reaching here means
/// the control sender is gone; neither has closed anything.
async fn stop_proxy_bridge<C, B>(
    mode: ServerControl,
    client: &mut tokio_tungstenite::WebSocketStream<C>,
    backend: &mut tokio_tungstenite::WebSocketStream<B>,
) -> ControlFlow<ProxyExit>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match mode {
        ServerControl::Graceful => {
            graceful_close_proxy(client, backend).await;
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
async fn forward_backend_frame<C>(
    frame: WsFrame,
    client: &mut tokio_tungstenite::WebSocketStream<C>,
) -> ControlFlow<()>
where
    C: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
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

/// Send a close frame, reporting a failure the teardown cannot act on.
///
/// `None` is the ordinary end of a bridge, which carries no status code; a
/// frame is given only where the peer would otherwise have to guess at a fault
/// that was not its own.
async fn send_close<S>(stream: &mut tokio_tungstenite::WebSocketStream<S>, reason: Option<WsClose>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::SinkExt;
    match stream.send(WsFrameMessage::Close(reason)).await {
        Ok(()) => {}
        Err(error) => tracing::debug!(%error, "WebSocket close frame send failed"),
    }
}

/// Close a WebSocket transport, reporting a failure the teardown cannot act on.
async fn close_transport<S>(stream: &mut tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    match stream.close(None).await {
        Ok(()) => {}
        Err(error) => tracing::debug!(%error, "WebSocket close failed"),
    }
}

/// Flush queued writes, reporting a failure the teardown cannot act on.
async fn flush_transport<S>(stream: &mut tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::SinkExt;
    match stream.flush().await {
        Ok(()) => {}
        Err(error) => tracing::debug!(%error, "WebSocket flush failed"),
    }
}

async fn shutdown_client_transport<S>(stream: &mut tokio_tungstenite::WebSocketStream<S>)
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

/// Read one transport until its peer's close frame arrives.
///
/// A transport error and an ended stream stop the drain the same way a close
/// does: in all three there is no close frame still coming.
async fn drain_until_close<S>(stream: &mut tokio_tungstenite::WebSocketStream<S>)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use futures_util::StreamExt;
    while let Some(result) = stream.next().await {
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
    let host = match uri.authority() {
        Some(authority) => authority.as_str(),
        None => {
            tracing::warn!(url = %url, "WebSocket backend URL names no authority");
            return None;
        }
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

fn ws_handshake_rejection(error: WsHandshakeError) -> Rejected {
    match error {
        WsHandshakeError::BadRequest => Rejected::ws_bad_handshake(),
        WsHandshakeError::UnsupportedVersion => Rejected::ws_unsupported_version(),
    }
}

/// Build the `101` a validated handshake earns.
///
/// `Err` is a builder failure — unreachable while the accept key is derived
/// base64 and the subprotocol is token-validated, but a response that is not a
/// `101` must never be handed back as one: the caller would register a bridge
/// and resolve the handoff for an upgrade Hyper will never perform.
///
/// The builder's own error travels with the failure rather than being logged
/// here. It is the only account of what could not be represented, and it
/// belongs in the refusal record that already names the request, the route and
/// the subprotocol.
fn ws_switching_protocols(
    accept_key: &str,
    subprotocol: Option<&str>,
) -> Result<hyper::Response<HyperResponseBody>, hyper::http::Error> {
    let mut builder = hyper::Response::builder()
        .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Accept", accept_key);

    if let Some(proto) = subprotocol {
        builder = builder.header("Sec-WebSocket-Protocol", proto);
    }

    builder.body(HyperResponseBody::Full(http_body_util::Full::new(
        bytes::Bytes::new(),
    )))
}
