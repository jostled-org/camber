#![cfg(feature = "ws")]

//! A `wss` backend is authenticated before the proxy speaks WebSocket to it.
//!
//! The rows drive the proxy's own backend handshake — target, offer, dial, TLS
//! and answer validation — against a local certificate authority, through the
//! adapter that swaps the trust anchors and nothing else. A certificate for
//! another name and one no root signed are both refused at the connect leg, so
//! no offer follows them; a trusted one under its own name upgrades, and the
//! connection it yields carries a real frame. Every client hello names HTTP/1.1
//! and nothing else: the upgrade is an HTTP/1.1 exchange, and a backend that
//! negotiated anything else could not answer it.
//!
//! The last row swaps nothing. It dials under production's own trust — the
//! branch a `wss` route takes — where the public roots refuse a locally signed
//! certificate, and reads the offer out of the hello that refusal was answered
//! with. The offer a production route makes is proven there rather than
//! inferred from the configured rows.

use crate::common::{
    HaltableListener, generate_cert_with_san, is_alert_received, lifecycle_event,
    root_store_trusting, send_report, unless_halted,
};
use camber::RuntimeError;
use camber::http::mock::{BackendWsConnection, backend_ws_handshake};
use futures_util::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

/// The name a correctly certified backend answers under.
///
/// An address rather than a hostname: the fixture binds the loopback address,
/// and a name would make the row depend on what a resolver returns for it.
const BACKEND_SAN: &str = "127.0.0.1";

/// A name no row connects to.
const WRONG_SAN: &str = "not-this-backend.example";

/// The origin-form target every row offers.
const BACKEND_PATH: &str = "/echo";

/// The subprotocol every row offers, and a correct backend selects.
const OFFERED_PROTOCOL: &str = "chat";

/// The only application protocol a backend upgrade may be offered under.
const HTTP1_ALPN: &str = "http/1.1";

/// The text frame a verified backend sends once its upgrade is live.
const BACKEND_GREETING: &str = "from-tls-backend";

/// The account every refused connect leg is reported under.
const CONNECT_FAILED: &str = "proxy connect failed:";

// ── The scripted TLS backend ───────────────────────────────────────

/// What one TLS backend connection reached.
#[derive(Debug, Eq, PartialEq)]
enum TlsBackendEvent {
    /// The application protocols one client hello offered, in order.
    Offered(Box<[Box<str>]>),
    /// The WebSocket handshake completed over the verified transport.
    Upgraded,
    /// The client refused this backend's certificate with an alert, before any
    /// request.
    Refused,
    /// The backend could not serve its connection, and why.
    Failed(Box<str>),
}

/// A TLS backend that owns its listener, its sockets, and its accept loop.
struct TlsWsBackend {
    listener: HaltableListener<TlsBackendEvent>,
}

impl TlsWsBackend {
    async fn bind(cert_pem: &[u8], key_pem: &[u8]) -> Self {
        let listener = HaltableListener::bind(
            "the TLS WebSocket backend",
            |events| {
                let acceptor = tokio_rustls::TlsAcceptor::from(recording_server_config(
                    cert_pem,
                    key_pem,
                    events.clone(),
                ));
                move |stream, _, events, halt| {
                    serve_tls_peer(acceptor.clone(), stream, events, halt)
                }
            },
            |_, fault| TlsBackendEvent::Failed(fault),
        )
        .await;
        Self { listener }
    }

    /// The backend as a proxy route names it.
    fn url(&self) -> Box<str> {
        format!("https://{}", self.listener.addr()).into_boxed_str()
    }

    /// The next phase the backend reports, or a failure naming the wait.
    async fn next(&mut self, context: &str) -> TlsBackendEvent {
        lifecycle_event(context, self.listener.recv())
            .await
            .unwrap_or_else(|| panic!("{context}: the TLS backend stopped reporting"))
    }

    /// Require that the backend's next report is `expected`.
    async fn expect(&mut self, expected: TlsBackendEvent, context: &str) {
        let event = self.next(context).await;
        assert_eq!(event, expected, "{context}: the TLS backend reported");
    }

    /// Stop accepting, close every held connection, join the accept loop,
    /// prove the address is free again, and require that nothing was left
    /// unreported.
    async fn finish(self, context: &str) {
        self.listener
            .finish(context, |accepts| async move {
                Some(lifecycle_event(context, accepts).await)
            })
            .await;
    }
}

/// A server configuration that reports every client hello's ALPN offer.
///
/// The offer is read where it actually arrives rather than inferred from what
/// was negotiated: a server that offered both protocols and answered HTTP/1.1
/// would look the same as a client that never named the other one.
fn recording_server_config(
    cert_pem: &[u8],
    key_pem: &[u8],
    events: mpsc::UnboundedSender<TlsBackendEvent>,
) -> Arc<rustls::ServerConfig> {
    let resolver = OfferRecorder {
        key: Arc::new(crate::common::certified_key_from_pem(cert_pem, key_pem)),
        events,
    };
    let mut config = rustls::ServerConfig::builder_with_provider(Arc::new(
        rustls::crypto::aws_lc_rs::default_provider(),
    ))
    .with_safe_default_protocol_versions()
    .expect("the backend's protocol versions")
    .with_no_client_auth()
    .with_cert_resolver(Arc::new(resolver));
    // Both protocols, so a client that offered HTTP/2 would have had it
    // accepted. What the rows read is the offer itself, and this is what makes
    // an unoffered protocol a choice rather than an absent option.
    config.alpn_protocols = vec![b"h2".to_vec(), HTTP1_ALPN.as_bytes().to_vec()];
    Arc::new(config)
}

/// The resolver that records each hello's offer and answers with one key.
#[derive(Debug)]
struct OfferRecorder {
    key: Arc<rustls::sign::CertifiedKey>,
    events: mpsc::UnboundedSender<TlsBackendEvent>,
}

impl rustls::server::ResolvesServerCert for OfferRecorder {
    fn resolve(
        &self,
        hello: rustls::server::ClientHello<'_>,
    ) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let offered = hello
            .alpn()
            .map(|protocols| {
                protocols
                    .map(|protocol| String::from_utf8_lossy(protocol).into())
                    .collect()
            })
            .unwrap_or_default();
        send_report(&self.events, TlsBackendEvent::Offered(offered));
        Some(Arc::clone(&self.key))
    }
}

/// Authenticate one peer, upgrade it, greet it, and answer its close.
///
/// A halted fixture gives up whatever phase its peer reached, so a connection
/// a row never finished cannot park the join.
async fn serve_tls_peer(
    acceptor: tokio_rustls::TlsAcceptor,
    stream: TcpStream,
    events: mpsc::UnboundedSender<TlsBackendEvent>,
    mut halt: watch::Receiver<bool>,
) {
    if let Err(ending) = serve_verified_peer(acceptor, stream, &events, &mut halt).await {
        send_report(&events, ending);
    }
}

/// Serve one peer to its end.
///
/// `Ok` is an end no row reads: the peer's close, or the fixture's halt. `Err`
/// is the report the connection ended on.
async fn serve_verified_peer(
    acceptor: tokio_rustls::TlsAcceptor,
    stream: TcpStream,
    events: &mpsc::UnboundedSender<TlsBackendEvent>,
    halt: &mut watch::Receiver<bool>,
) -> Result<(), TlsBackendEvent> {
    let Some(accepted) = unless_halted(halt, acceptor.accept(stream)).await else {
        return Ok(());
    };
    let authenticated = accepted.map_err(|error| accept_failure(&error))?;
    let handshake = tokio_tungstenite::accept_hdr_async(authenticated, select_the_first_offer);
    let Some(upgraded) = unless_halted(halt, handshake).await else {
        return Ok(());
    };
    let mut websocket =
        upgraded.map_err(|error| failed("the WebSocket handshake failed", error))?;
    send_report(events, TlsBackendEvent::Upgraded);
    let greeting = websocket.send(tungstenite::Message::text(BACKEND_GREETING));
    let Some(greeted) = unless_halted(halt, greeting).await else {
        return Ok(());
    };
    greeted.map_err(|error| failed("the greeting could not be sent", error))?;
    while let Some(Some(message)) = unless_halted(halt, websocket.next()).await {
        let message = message.map_err(|error| failed("the verified transport failed", error))?;
        if message.is_close() {
            return Ok(());
        }
    }
    Ok(())
}

/// The fixture's own failure in `phase`.
fn failed(phase: &str, error: impl std::fmt::Display) -> TlsBackendEvent {
    TlsBackendEvent::Failed(format!("{phase}: {error}").into())
}

/// Read a failed TLS accept as the client's refusal only when the client said
/// so with an alert.
///
/// Any other failure is the fixture's own, for the reason [`is_alert_received`]
/// gives.
fn accept_failure(error: &std::io::Error) -> TlsBackendEvent {
    match is_alert_received(error) {
        true => TlsBackendEvent::Refused,
        false => failed("the TLS accept failed", error),
    }
}

/// Select the first protocol the client offered, the way a backend does.
fn select_the_first_offer(
    request: &tungstenite::handshake::server::Request,
    mut response: tungstenite::handshake::server::Response,
) -> Result<tungstenite::handshake::server::Response, tungstenite::handshake::server::ErrorResponse>
{
    let selected = request
        .headers()
        .get("sec-websocket-protocol")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(',').next())
        .map(str::trim)
        .and_then(|token| http::HeaderValue::from_str(token).ok());
    if let Some(selected) = selected {
        response
            .headers_mut()
            .insert("sec-websocket-protocol", selected);
    }
    Ok(response)
}

// ── Rows ───────────────────────────────────────────────────────────

/// Require that the backend's next report is a hello naming HTTP/1.1 alone.
async fn assert_only_http1_offered(backend: &mut TlsWsBackend, label: &str) {
    match backend.next(label).await {
        TlsBackendEvent::Offered(protocols) => assert_eq!(
            *protocols
                .iter()
                .map(|name| &**name)
                .collect::<Box<[&str]>>(),
            [HTTP1_ALPN],
            "{label}: the backend upgrade offered application protocols"
        ),
        other => panic!("{label}: expected the client hello, the backend reported {other:?}"),
    }
}

/// Read the greeting off a validated backend connection, then close it.
async fn assert_verified_connection_carries_a_frame(connection: BackendWsConnection, label: &str) {
    let mut websocket = connection.into_websocket();
    let message = lifecycle_event(label, websocket.next())
        .await
        .unwrap_or_else(|| panic!("{label}: the verified transport ended before its frame"))
        .unwrap_or_else(|error| panic!("{label}: the verified transport failed: {error}"));
    assert_eq!(
        message.into_text().ok().as_deref(),
        Some(BACKEND_GREETING),
        "{label}: the frame the verified transport carried"
    );
    lifecycle_event(label, websocket.close(None))
        .await
        .unwrap_or_else(|error| panic!("{label}: closing the verified transport failed: {error}"));
}

/// A correct name under a trusted root upgrades, and the connection it yields
/// names the backend's own selection and carries a real frame.
async fn assert_verified_backend_upgrades() {
    let label = "a trusted backend under its own name";
    let (certificate, key) = generate_cert_with_san(BACKEND_SAN);
    let mut backend = TlsWsBackend::bind(&certificate, &key).await;

    let connection = lifecycle_event(
        label,
        backend_ws_handshake(
            &backend.url(),
            BACKEND_PATH,
            &[OFFERED_PROTOCOL],
            Some(root_store_trusting(&[&certificate])),
        ),
    )
    .await
    .unwrap_or_else(|error| panic!("{label}: the verified handshake failed: {error:?}"));
    assert_eq!(
        connection.selected_protocol(),
        Some(OFFERED_PROTOCOL),
        "{label}: the protocol the backend selected"
    );
    assert_only_http1_offered(&mut backend, label).await;
    backend.expect(TlsBackendEvent::Upgraded, label).await;
    assert_verified_connection_carries_a_frame(connection, label).await;
    backend.finish(label).await;
}

/// The trust one refusal row hands the production handshake.
enum RefusedTrust {
    /// The backend's own certificate, so only the name it carries can refuse
    /// it.
    OwnCertificate,
    /// A root that signed nothing this row serves.
    UnrelatedRoot,
    /// Production's own anchors, reached by handing the adapter no trust at
    /// all. No locally generated certificate chains to the public roots, so
    /// this row ends at the same refusal — but it ends there through the
    /// config a `wss` route builds, which is what makes the hello it was
    /// refused with production's own offer.
    PublicRoots,
}

impl RefusedTrust {
    /// The anchors to hand the adapter for a backend certified by `certificate`.
    fn roots(&self, certificate: &[u8]) -> Option<rustls::RootCertStore> {
        match self {
            Self::OwnCertificate => Some(root_store_trusting(&[certificate])),
            Self::UnrelatedRoot => {
                Some(root_store_trusting(&[&generate_cert_with_san(WRONG_SAN).0]))
            }
            Self::PublicRoots => None,
        }
    }
}

/// A certificate this row's roots cannot verify is refused at the connect leg,
/// and no offer follows it.
async fn assert_unverified_backend_is_refused(label: &str, san: &str, trust: &RefusedTrust) {
    let (certificate, key) = generate_cert_with_san(san);
    let mut backend = TlsWsBackend::bind(&certificate, &key).await;
    let roots = trust.roots(&certificate);

    let refusal = lifecycle_event(
        label,
        backend_ws_handshake(&backend.url(), BACKEND_PATH, &[OFFERED_PROTOCOL], roots),
    )
    .await
    .err()
    .unwrap_or_else(|| panic!("{label}: the handshake accepted an unverified backend"));
    match refusal {
        RuntimeError::Http(account) => assert!(
            account.starts_with(CONNECT_FAILED),
            "{label}: the refusal was reported as {account}"
        ),
        other => panic!("{label}: the refusal was {other:?}"),
    }
    assert_only_http1_offered(&mut backend, label).await;
    backend.expect(TlsBackendEvent::Refused, label).await;
    backend.finish(label).await;
}

/// 7.T3
///
/// The proxy's own backend handshake owner, under trust a row supplies. A
/// verified backend upgrades over HTTP/1.1 and carries a frame; a certificate
/// for another name and one no root signed are both refused before any offer;
/// and every client hello names HTTP/1.1 alone, the last one under production's
/// own roots.
#[camber::test]
async fn backend_tls_uses_verified_http1_upgrade() {
    assert_verified_backend_upgrades().await;
    assert_unverified_backend_is_refused(
        "a certificate for another name",
        WRONG_SAN,
        &RefusedTrust::OwnCertificate,
    )
    .await;
    assert_unverified_backend_is_refused(
        "a certificate no trusted root signed",
        BACKEND_SAN,
        &RefusedTrust::UnrelatedRoot,
    )
    .await;
    assert_unverified_backend_is_refused(
        "production's own public roots",
        BACKEND_SAN,
        &RefusedTrust::PublicRoots,
    )
    .await;
}
