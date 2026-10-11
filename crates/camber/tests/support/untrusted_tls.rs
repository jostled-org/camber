//! A loopback TLS peer whose certificate no trust store holds.
//!
//! A client that validates its peer refuses the handshake, so the peer reads
//! no request. The peer reports what each connection reached, so a row can
//! require the refusal and not only the client's error.
//!
//! Every mount sits beside a `halt` module and a `tls_support` module, which
//! this file reaches through `super`.

use super::halt::{HaltableListener, send_report, unless_halted};
use super::tls_support;

use tokio::io::AsyncReadExt;
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
            move |stream, _connection, events, halt| {
                serve_untrusted(acceptor.clone(), stream, events, halt)
            }
        },
        tls_peer_fault,
    )
    .await
}

/// Run one handshake, and report whether it completed and what followed it.
async fn serve_untrusted(
    acceptor: TlsAcceptor,
    stream: TcpStream,
    events: mpsc::UnboundedSender<TlsPeerEvent>,
    mut halt: halt_signal::Receiver<bool>,
) {
    let seen = match unless_halted(&mut halt, acceptor.accept(stream)).await {
        None => TlsPeerEvent::Unfinished,
        Some(Err(error)) if tls_support::is_alert_received(&error) => {
            TlsPeerEvent::HandshakeRefused
        }
        Some(Err(error)) => TlsPeerEvent::Fault(
            format!("the TLS accept failed without an alert: {error}").into_boxed_str(),
        ),
        Some(Ok(mut established)) => {
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
