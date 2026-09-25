//! The backend half of a proxied WebSocket, settled before the peer hears
//! anything.
//!
//! A proxied `101` promises the peer a WebSocket the backend has already
//! agreed to. So the backend is reached, offered everything the peer offered,
//! and its answer validated — status, accept key, upgrade tokens, extensions,
//! and the protocol it selected — before the downstream response exists. Every
//! future this takes runs on the request task that awaits it: the Hyper
//! connection is polled here, beside the response it produces, and no driver
//! outlives a refusal. Only a complete validation hands the upgraded transport
//! on, and it hands over Hyper's own upgraded stream, so bytes the backend sent
//! behind its head are framed rather than lost.

use super::super::Request;
use super::super::async_proxy::{
    ProxyFailure, ProxyPhase, connection_tokens, has_name_prefix, is_connection_named,
    is_forwarded_metadata, target_url,
};
use super::super::proxy_upstream::ProxyUpstream;
use super::handshake::{
    WsProtocolOffers, WsSelection, header_contains_token, is_ws_upgrade_head, single_header,
};
use crate::RuntimeError;
use crate::http::DeadlineBoundary;
use std::sync::Arc;
use std::time::Duration;

/// The request body a WebSocket offer carries: none.
type OfferBody = http_body_util::Empty<bytes::Bytes>;

/// The backend transport once its upgrade is validated.
pub type BackendWs =
    tokio_tungstenite::WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;

/// How a backend is reached: in the clear, or under verified TLS.
#[derive(Clone, Copy)]
enum BackendScheme {
    Plain,
    Tls,
}

/// Where one proxied offer goes, built from the route's backend and the
/// peer's own path.
pub(super) struct BackendTarget {
    scheme: BackendScheme,
    /// The host as a socket address names it: an IPv6 literal without brackets.
    host: Box<str>,
    port: u16,
    /// The `Host` header the offer carries.
    ///
    /// Built from the host and the port rather than from the whole authority.
    /// An authority also carries userinfo, so a backend configured as
    /// `http://user:secret@internal:8080` would otherwise send its credentials
    /// in a `Host` header — one strict backends reject, and one every
    /// intermediary and access log downstream reads.
    host_header: Box<str>,
    uri: hyper::Uri,
}

impl BackendTarget {
    /// Resolve the backend URL one offer is sent to.
    ///
    /// A refusal here is the same unbuildable-target proxy fault the buffered
    /// and streaming classes raise on the same input, so a traversal probe
    /// reads one way across all three and never as a backend outage.
    pub(super) fn resolve(path: &str, prefix: &str, backend: &str) -> Result<Self, ProxyFailure> {
        let url = target_url(backend, prefix, path)?;
        let uri = hyper::Uri::try_from(url.into_string())
            .map_err(|_| unbuildable("the configured backend and request path form no URL"))?;
        let scheme = match uri.scheme_str() {
            Some("http") => BackendScheme::Plain,
            Some("https") => BackendScheme::Tls,
            _ => {
                return Err(unbuildable(
                    "the configured backend names no scheme this proxy can upgrade over",
                ));
            }
        };
        let host = uri
            .host()
            .filter(|host| !host.is_empty())
            .ok_or_else(|| unbuildable("the configured backend names no authority"))?;
        let explicit_port = uri.port_u16();
        let host_header = match explicit_port {
            Some(port) => format!("{host}:{port}").into_boxed_str(),
            None => Box::from(host),
        };
        let port = explicit_port.unwrap_or(match scheme {
            BackendScheme::Plain => 80,
            BackendScheme::Tls => 443,
        });
        Ok(Self {
            scheme,
            host: host.trim_start_matches('[').trim_end_matches(']').into(),
            port,
            host_header,
            uri,
        })
    }

    /// The origin-form target the offer's request line names.
    fn origin_form(&self) -> &str {
        self.uri
            .path_and_query()
            .map_or("/", |target| target.as_str())
    }
}

/// Refuse a proxied upgrade whose target this proxy cannot build.
const fn unbuildable(detail: &'static str) -> ProxyFailure {
    ProxyFailure::UnbuildableTarget(detail)
}

/// Which roots a `wss` backend's certificate must chain to.
pub(super) enum BackendTrust {
    /// The public WebPKI roots every production route verifies against.
    Public,
    /// A caller-supplied config, for the test adapter that exercises this
    /// exact owner against a local certificate authority.
    Configured(Arc<rustls::ClientConfig>),
}

impl BackendTrust {
    fn client_config(&self) -> Result<Arc<rustls::ClientConfig>, RuntimeError> {
        match self {
            Self::Public => crate::tls::http1_client_config(),
            Self::Configured(config) => Ok(Arc::clone(config)),
        }
    }
}

/// One offer ready to send, and what a valid answer to it must carry.
pub(super) struct BackendHandshake<'a> {
    target: BackendTarget,
    request: hyper::Request<OfferBody>,
    accept: Box<str>,
    offers: &'a WsProtocolOffers,
}

/// A backend whose `101` passed every check, and the offer it selected.
pub(super) struct NegotiatedBackend {
    pub(super) upgrade: ValidatedBackendUpgrade,
    pub(super) selection: WsSelection,
}

/// The upgraded backend transport, before it is framed.
pub(super) struct ValidatedBackendUpgrade(hyper::upgrade::Upgraded);

impl ValidatedBackendUpgrade {
    /// Frame the validated transport as the client side of a WebSocket.
    ///
    /// Hyper's upgraded stream replays whatever it read behind the `101`
    /// before it reads the socket again, so a frame the backend sent in the
    /// same write as its head reaches the bridge exactly once.
    pub(super) async fn into_websocket(self) -> BackendWs {
        tokio_tungstenite::WebSocketStream::from_raw_socket(
            hyper_util::rt::TokioIo::new(self.0),
            tokio_tungstenite::tungstenite::protocol::Role::Client,
            None,
        )
        .await
    }
}

/// The backend's answer to one offer, body unread.
type BackendAnswer = hyper::Response<hyper::body::Incoming>;

/// Whether the Hyper connection is still owed polling after the answer.
enum ConnectionState {
    /// The answer arrived first; the connection still holds the transport.
    Driving,
    /// The connection finished while producing the answer, so any upgrade it
    /// owed is already handed over.
    Finished,
}

impl<'a> BackendHandshake<'a> {
    /// Build the offer `forwarded` headers and every client offer produce.
    ///
    /// The backend is offered every protocol the client offered, in order, so
    /// the selection is the backend's to make. A header this proxy cannot carry
    /// is Camber's own fault, reported before anything is dialled.
    pub(super) fn new<'h>(
        target: BackendTarget,
        forwarded: impl Iterator<Item = (&'h str, &'h str)>,
        offers: &'a WsProtocolOffers,
    ) -> Result<Self, ProxyFailure> {
        let key = tokio_tungstenite::tungstenite::handshake::client::generate_key();
        let accept =
            tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes()).into();
        let builder = hyper::Request::get(target.origin_form())
            .header(hyper::header::HOST, &*target.host_header)
            .header(hyper::header::CONNECTION, "Upgrade")
            .header(hyper::header::UPGRADE, "websocket")
            .header(hyper::header::SEC_WEBSOCKET_VERSION, "13")
            .header(hyper::header::SEC_WEBSOCKET_KEY, key);
        let builder = forwarded.fold(builder, |builder, (name, value)| {
            builder.header(name, value)
        });
        let builder = match offers.joined() {
            Some(offered) => builder.header(hyper::header::SEC_WEBSOCKET_PROTOCOL, offered),
            None => builder,
        };
        let request = builder.body(OfferBody::new()).map_err(|error| {
            ProxyFailure::unsendable(RuntimeError::Http(
                format!("the WebSocket backend offer could not be built: {error}").into(),
            ))
        })?;
        Ok(Self {
            target,
            request,
            accept,
            offers,
        })
    }

    /// Reach the backend, send the offer, and validate its answer, all under
    /// the deadline the route froze for a usable upstream head.
    ///
    /// Establishing the transport — TLS included — also answers to the route's
    /// connect deadline. Dropping this future at any point, by the request
    /// total or a cancelled request, drops the transport with it.
    pub(super) async fn negotiate(
        self,
        upstream: &ProxyUpstream,
        trust: &BackendTrust,
    ) -> Result<NegotiatedBackend, ProxyFailure> {
        tokio::time::timeout(
            upstream.request_timeout(),
            self.establish_and_exchange(upstream.connect_timeout(), trust),
        )
        .await
        .map_err(|_| ProxyFailure::expired(ProxyPhase::Request, DeadlineBoundary::ProxyRequest))?
    }

    async fn establish_and_exchange(
        self,
        connect_deadline: Duration,
        trust: &BackendTrust,
    ) -> Result<NegotiatedBackend, ProxyFailure> {
        match self.target.scheme {
            BackendScheme::Plain => {
                let transport = within_connect(connect_deadline, dial(&self.target)).await?;
                self.exchange(transport).await
            }
            BackendScheme::Tls => {
                let transport =
                    within_connect(connect_deadline, dial_tls(&self.target, trust)).await?;
                self.exchange(transport).await
            }
        }
    }

    /// Send the offer over one established transport and take its upgrade.
    async fn exchange<T>(self, transport: T) -> Result<NegotiatedBackend, ProxyFailure>
    where
        T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (mut sender, connection) =
            hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(transport))
                .await
                .map_err(request_failure)?;
        let connection = connection.with_upgrades();
        tokio::pin!(connection);
        let (mut response, state) =
            answer_to(sender.send_request(self.request), connection.as_mut()).await?;
        let selection =
            validate_answer(&response, &self.accept, self.offers).map_err(refused_handshake)?;
        let on_upgrade = hyper::upgrade::on(&mut response);
        match state {
            ConnectionState::Driving => connection.as_mut().await.map_err(request_failure)?,
            ConnectionState::Finished => {}
        }
        let upgraded = on_upgrade.await.map_err(request_failure)?;
        // Held until the upgrade is taken, so the connection never reads a
        // dropped sender as the end of its only request.
        drop(sender);
        Ok(NegotiatedBackend {
            upgrade: ValidatedBackendUpgrade(upgraded),
            selection,
        })
    }
}

/// Await the backend's answer while polling the connection that produces it.
///
/// The connection is what reads the answer, so it has to run while the
/// response is awaited. It can also finish in the same poll that delivers a
/// `101`, having handed its transport to the upgrade: a finished connection is
/// therefore not a failure by itself, and the answer it already delivered is
/// taken. The answer is read first only because it is the value wanted; a
/// connection that ended without one leaves the send to fail on its own.
async fn answer_to(
    sending: impl std::future::Future<Output = Result<BackendAnswer, hyper::Error>>,
    mut connection: std::pin::Pin<&mut impl std::future::Future<Output = Result<(), hyper::Error>>>,
) -> Result<(BackendAnswer, ConnectionState), ProxyFailure> {
    tokio::pin!(sending);
    tokio::select! {
        biased;
        answered = &mut sending => {
            answered.map(|response| (response, ConnectionState::Driving)).map_err(request_failure)
        }
        ended = connection.as_mut() => {
            ended.map_err(request_failure)?;
            let response = sending.await.map_err(request_failure)?;
            Ok((response, ConnectionState::Finished))
        }
    }
}

/// Establish one transport under the route's connect deadline.
async fn within_connect<T>(
    deadline: Duration,
    connecting: impl std::future::Future<Output = Result<T, ProxyFailure>>,
) -> Result<T, ProxyFailure> {
    tokio::time::timeout(deadline, connecting)
        .await
        .map_err(|_| ProxyFailure::expired(ProxyPhase::Connect, DeadlineBoundary::ProxyConnect))?
}

async fn dial(target: &BackendTarget) -> Result<tokio::net::TcpStream, ProxyFailure> {
    tokio::net::TcpStream::connect((&*target.host, target.port))
        .await
        .map_err(|error| ProxyFailure::phase(ProxyPhase::Connect, RuntimeError::Io(error)))
}

/// Dial and authenticate a `wss` backend.
///
/// The certificate must chain to `trust` and name the configured host. A
/// failure is a connect failure and nothing else: no plaintext offer follows.
async fn dial_tls(
    target: &BackendTarget,
    trust: &BackendTrust,
) -> Result<tokio_rustls::client::TlsStream<tokio::net::TcpStream>, ProxyFailure> {
    let connect_failure = |cause| ProxyFailure::phase(ProxyPhase::Connect, cause);
    let config = trust.client_config().map_err(connect_failure)?;
    let name = crate::tls::client_server_name(&target.host).map_err(connect_failure)?;
    let transport = dial(target).await?;
    crate::tls::client_handshake(config, name, transport)
        .await
        .map_err(connect_failure)
}

/// Check every field the downstream `101` relies on, and return the offer the
/// backend selected.
///
/// An extension is refused whatever its value: none is offered, so any the
/// backend names is unsolicited, and the bridge could not honour it.
fn validate_answer(
    response: &BackendAnswer,
    accept: &str,
    offers: &WsProtocolOffers,
) -> Result<WsSelection, &'static str> {
    let headers = response.headers();
    require(
        response.status() == hyper::StatusCode::SWITCHING_PROTOCOLS,
        "the backend did not answer 101",
    )?;
    require(
        is_ws_upgrade_head(headers),
        "the backend did not upgrade to websocket exactly once",
    )?;
    require(
        header_contains_token(headers, "connection", "upgrade"),
        "the backend's Connection field carries no valid upgrade token",
    )?;
    require(
        single_header(headers, "sec-websocket-accept").is_some_and(|value| value == accept),
        "the backend's Sec-WebSocket-Accept does not answer the offered key",
    )?;
    require(
        !headers.contains_key(hyper::header::SEC_WEBSOCKET_EXTENSIONS),
        "the backend named an extension nobody offered",
    )?;
    backend_selection(headers, offers)
}

/// The offer one backend answer selected: none, or exactly one offered token.
fn backend_selection(
    headers: &hyper::HeaderMap,
    offers: &WsProtocolOffers,
) -> Result<WsSelection, &'static str> {
    let mut selected = headers
        .get_all(hyper::header::SEC_WEBSOCKET_PROTOCOL)
        .iter();
    match (selected.next(), selected.next()) {
        (None, _) => Ok(WsSelection::NONE),
        (Some(value), None) => value
            .to_str()
            .ok()
            .and_then(|token| offers.select(token))
            .ok_or("the backend selected a protocol the client did not offer"),
        (Some(_), Some(_)) => Err("the backend selected more than one protocol"),
    }
}

fn require(holds: bool, reason: &'static str) -> Result<(), &'static str> {
    match holds {
        true => Ok(()),
        false => Err(reason),
    }
}

/// A backend that answered the offer with anything but a valid `101`.
fn refused_handshake(reason: &'static str) -> ProxyFailure {
    ProxyFailure::phase(
        ProxyPhase::Request,
        RuntimeError::Http(format!("backend refused the WebSocket handshake: {reason}").into()),
    )
}

/// A fault the HTTP/1 exchange itself raised.
fn request_failure(error: hyper::Error) -> ProxyFailure {
    ProxyFailure::phase(
        ProxyPhase::Request,
        RuntimeError::Http(error.to_string().into()),
    )
}

/// The headers of a peer's request a proxied offer carries to its backend.
///
/// The peer's own `Connection` is read first and every field it names is
/// dropped, whatever the allowlist below would have said about it. A named
/// field is one the peer asked this hop to consume, and an allowlist consulted
/// on its own forwards a credential the peer marked hop-by-hop — the narrower
/// rule is not the stricter one here.
pub(super) fn forwarded_offer_headers(req: &Request) -> impl Iterator<Item = (&str, &str)> {
    let delegated = connection_tokens(req.headers().map(|(name, value)| (name, value.as_bytes())));
    req.headers().filter(move |(name, _)| {
        !is_connection_named(name, &delegated) && is_forwardable_ws_header(name)
    })
}

/// A WS proxy header is forwardable if it is Authorization, Cookie, or a
/// non-forwarded X-* header.
///
/// Other WebSocket handshake headers (`Sec-WebSocket-Key`,
/// `Sec-WebSocket-Version`, and the rest) are excluded — the proxy generates
/// its own, and forwards the client's protocol offers itself.
fn is_forwardable_ws_header(name: &str) -> bool {
    match name {
        n if n.eq_ignore_ascii_case("authorization") => true,
        n if n.eq_ignore_ascii_case("cookie") => true,
        n if has_name_prefix(n, "x-") && !is_forwarded_metadata(n) => true,
        _ => false,
    }
}

/// One backend the test adapter negotiated: what it selected, and the framed
/// transport the bridge would have carried.
#[doc(hidden)]
pub struct BackendWsConnection {
    selected: Option<Box<str>>,
    websocket: BackendWs,
}

impl BackendWsConnection {
    /// The protocol the backend selected from the offers, if any.
    pub fn selected_protocol(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    /// The validated transport, framed as the proxy's client side.
    pub fn into_websocket(self) -> BackendWs {
        self.websocket
    }
}

/// Negotiate one backend WebSocket through the proxy's own handshake owner,
/// trusting `roots` instead of the public WebPKI roots, or the public roots
/// themselves when `roots` is `None`.
///
/// Exists so a test can prove authenticated `wss` success against a local
/// certificate authority without a public custom-trust API. Everything past
/// the trust anchors — target, offer, dial, TLS with HTTP/1.1 ALPN, answer
/// validation, and client framing — is the production owner, under the
/// documented default proxy deadlines; `None` is production's own trust, the
/// branch a `wss` route takes, and leaves nothing swapped at all. `backend` is
/// an `http` or `https` base URL and `path` the origin-form target appended to
/// it.
///
/// # Errors
///
/// Returns [`RuntimeError::Tls`] when `roots` form no client config,
/// [`RuntimeError::InvalidArgument`] for a protocol that is not a token, and
/// [`RuntimeError::Http`] carrying the proxy's own account of any target,
/// connect, TLS, or handshake refusal.
#[doc(hidden)]
pub async fn backend_ws_handshake(
    backend: &str,
    path: &str,
    protocols: &[&str],
    roots: Option<rustls::RootCertStore>,
) -> Result<BackendWsConnection, RuntimeError> {
    let trust = match roots {
        None => BackendTrust::Public,
        Some(roots) => BackendTrust::Configured(Arc::new(
            crate::tls::backend_client_config(roots).map_err(RuntimeError::Tls)?,
        )),
    };
    let offers = WsProtocolOffers::from_tokens(protocols).ok_or_else(|| {
        RuntimeError::InvalidArgument("every offered protocol must be a token".into())
    })?;
    let failed = |failure: ProxyFailure| RuntimeError::Http(failure.to_string().into());
    let target = BackendTarget::resolve(path, "", backend).map_err(failed)?;
    let NegotiatedBackend { upgrade, selection } =
        BackendHandshake::new(target, std::iter::empty(), &offers)
            .map_err(failed)?
            .negotiate(ProxyUpstream::defaults(), &trust)
            .await
            .map_err(failed)?;
    Ok(BackendWsConnection {
        selected: offers.named(selection).map(Box::from),
        websocket: upgrade.into_websocket().await,
    })
}
