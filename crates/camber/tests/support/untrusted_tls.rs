//! A loopback TLS peer whose certificate no trust store holds.
//!
//! A client that validates its peer refuses the handshake, so the peer reads
//! no request. The peer reports what each connection reached, so a row can
//! require the refusal and not only the client's error.
//!
//! A second peer here presents a certificate the row chose. It reports the
//! server name each client asked for, and it answers a client that trusted it.
//!
//! Every mount sits beside a `halt` module and a `tls_support` module, which
//! this file reaches through `super`.

use super::halt::{HaltableListener, send_report, unless_halted};
use super::tls_support;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch as halt_signal};
use tokio_rustls::TlsAcceptor;
use tokio_rustls::server::TlsStream;

/// The most application data the TLS peer keeps as evidence.
const APPLICATION_CAP: usize = 16 * 1024;

/// What the untrusted TLS peer saw of one connection.
#[derive(Debug, Eq, PartialEq)]
pub enum TlsPeerEvent {
    /// The client refused the handshake with an alert, so it sent no
    /// application data.
    HandshakeRefused,
    /// The row halted the peer before the handshake ended. The dial is still
    /// reported, so a connection no row expected is never lost to the halt.
    Unfinished,
    /// The handshake completed, and the peer read these application bytes.
    Completed(Box<[u8]>),
    /// The fixture itself failed.
    Fault(Box<str>),
}

fn tls_peer_fault(connection: usize, detail: Box<str>) -> TlsPeerEvent {
    TlsPeerEvent::Fault(format!("connection {connection}: {detail}").into_boxed_str())
}

/// Bind a TLS peer that presents a certificate no trust store holds.
///
/// The certificate names the loopback address the client dials, so the name
/// matches and only the issuer is refused.
pub async fn untrusted_tls_peer() -> HaltableListener<TlsPeerEvent> {
    let (cert_pem, key_pem) = tls_support::generate_cert_with_san("127.0.0.1");
    let acceptor = TlsAcceptor::from(tls_support::build_server_config(&cert_pem, &key_pem));
    HaltableListener::bind(
        "the untrusted TLS peer",
        move |_: &mpsc::UnboundedSender<TlsPeerEvent>| {
            move |stream, _, events, halt| serve_untrusted(acceptor.clone(), stream, events, halt)
        },
        tls_peer_fault,
    )
    .await
}

/// Finish one handshake, or name what stopped it.
async fn handshaken(
    halt: &mut halt_signal::Receiver<bool>,
    accept: impl Future<Output = std::io::Result<TlsStream<TcpStream>>>,
) -> Result<TlsStream<TcpStream>, TlsPeerEvent> {
    match unless_halted(halt, accept).await {
        None => Err(TlsPeerEvent::Unfinished),
        Some(Err(error)) if tls_support::is_alert_received(&error) => {
            Err(TlsPeerEvent::HandshakeRefused)
        }
        Some(Err(error)) => Err(TlsPeerEvent::Fault(
            format!("the TLS accept failed without an alert: {error}").into_boxed_str(),
        )),
        Some(Ok(established)) => Ok(established),
    }
}

/// Run one handshake, and report whether it completed and what followed it.
async fn serve_untrusted(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    events: mpsc::UnboundedSender<TlsPeerEvent>,
    mut halt: halt_signal::Receiver<bool>,
) {
    let seen = match handshaken(&mut halt, acceptor.accept(stream)).await {
        Err(unfinished) => unfinished,
        Ok(mut established) => {
            TlsPeerEvent::Completed(application_data(&mut established, &mut halt).await)
        }
    };
    send_report(&events, seen);
}

/// Read what a client sent after a completed handshake, up to one request head.
///
/// The read ends at the head's end, the client's close, the halt, or the cap.
/// The peer then closes, so a client that did trust it fails its row instead
/// of parking it.
async fn application_data(
    established: &mut TlsStream<TcpStream>,
    halt: &mut halt_signal::Receiver<bool>,
) -> Box<[u8]> {
    let mut seen = Vec::new();
    let mut chunk = [0_u8; 1024];
    while seen.len() < APPLICATION_CAP && !seen.ends_with(b"\r\n\r\n") {
        match unless_halted(halt, established.read(&mut chunk)).await {
            Some(Ok(read)) if read > 0 => seen.extend_from_slice(&chunk[..read]),
            Some(Ok(_) | Err(_)) | None => break,
        }
    }
    seen.into_boxed_slice()
}

/// What a named TLS peer saw of one connection.
#[derive(Debug, Eq, PartialEq)]
pub struct NamedTlsEvent {
    /// The server name the client asked for. `None` means it sent none, or the
    /// connection ended before its first handshake message was read.
    pub server_name: Option<Box<str>>,
    /// How far the connection got.
    pub reached: TlsPeerEvent,
}

impl NamedTlsEvent {
    /// A connection that ended before the client's server name was read.
    fn unnamed(reached: TlsPeerEvent) -> Self {
        Self {
            server_name: None,
            reached,
        }
    }
}

fn named_peer_fault(connection: usize, detail: Box<str>) -> NamedTlsEvent {
    NamedTlsEvent::unnamed(tls_peer_fault(connection, detail))
}

/// Bind a TLS peer that presents this certificate, reports the server name
/// each client asked for, and answers one request head with `reply`.
///
/// The name is read from the client's first handshake message, so a client
/// that then refuses the certificate has still reported the name it wanted.
/// The row writes `reply` whole. The peer closes after it.
pub async fn named_tls_peer(
    cert_pem: &[u8],
    key_pem: &[u8],
    reply: Vec<u8>,
) -> HaltableListener<NamedTlsEvent> {
    let config = tls_support::build_server_config(cert_pem, key_pem);
    let reply: std::sync::Arc<[u8]> = reply.into();
    HaltableListener::bind(
        "the named TLS peer",
        move |_: &mpsc::UnboundedSender<NamedTlsEvent>| {
            move |stream, _, events, halt| {
                serve_named(config.clone(), reply.clone(), stream, events, halt)
            }
        },
        named_peer_fault,
    )
    .await
}

/// Read the client's first handshake message, then run the handshake.
async fn serve_named(
    config: std::sync::Arc<rustls::ServerConfig>,
    reply: std::sync::Arc<[u8]>,
    stream: TcpStream,
    events: mpsc::UnboundedSender<NamedTlsEvent>,
    mut halt: halt_signal::Receiver<bool>,
) {
    let hello = tokio_rustls::LazyConfigAcceptor::new(rustls::server::Acceptor::default(), stream);
    let seen = match unless_halted(&mut halt, hello).await {
        None => NamedTlsEvent::unnamed(TlsPeerEvent::Unfinished),
        Some(Err(error)) => NamedTlsEvent::unnamed(TlsPeerEvent::Fault(
            format!("the first handshake message was unreadable: {error}").into_boxed_str(),
        )),
        Some(Ok(start)) => named_handshake(start, config, &reply, &mut halt).await,
    };
    send_report(&events, seen);
}

/// Keep the name the client asked for, and report how its handshake ended.
async fn named_handshake(
    start: tokio_rustls::StartHandshake<TcpStream>,
    config: std::sync::Arc<rustls::ServerConfig>,
    reply: &[u8],
    halt: &mut halt_signal::Receiver<bool>,
) -> NamedTlsEvent {
    let server_name = start.client_hello().server_name().map(Box::from);
    let reached = match handshaken(halt, start.into_stream(config)).await {
        Err(unfinished) => unfinished,
        Ok(mut established) => answer_named(&mut established, reply, halt).await,
    };
    NamedTlsEvent {
        server_name,
        reached,
    }
}

/// Read one request head, answer it, and close.
///
/// The report is made after the answer is written, so a row that holds the
/// client's result can wait for the report without a second edge.
async fn answer_named(
    established: &mut TlsStream<TcpStream>,
    reply: &[u8],
    halt: &mut halt_signal::Receiver<bool>,
) -> TlsPeerEvent {
    let seen = application_data(established, halt).await;
    let written = unless_halted(halt, async {
        established.write_all(reply).await?;
        established.shutdown().await
    })
    .await;
    match written {
        Some(Err(error)) => {
            TlsPeerEvent::Fault(format!("writing the answer failed: {error}").into_boxed_str())
        }
        Some(Ok(())) | None => TlsPeerEvent::Completed(seen),
    }
}
