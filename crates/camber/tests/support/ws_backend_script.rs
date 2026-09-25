//! A scripted raw WebSocket backend for proxied negotiation rows.
//!
//! A Camber or Tungstenite backend answers every handshake the way a correct
//! server does, which is exactly what a negotiation row cannot use: the claims
//! are about a backend that refuses, answers with a broken head, selects a
//! protocol it was never offered, or never answers at all. This backend reads
//! the proxy's whole offer, reports it, and then writes whatever its row
//! scripted — byte for byte.
//!
//! Each accepted connection is one row, served by the script entry at its
//! accept index. Every phase the backend reaches is reported as a
//! [`BackendEvent`], and a fault on the backend's side is reported the same way
//! instead of panicking inside a task, so a row's own failure is never hidden
//! behind a fixture timeout. Every wait is bounded on a thread by
//! [`within_watchdog`], so the same fixture serves rows that hold the Tokio
//! clock still.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

use crate::http::header_values;
use crate::retry_upstream::within_watchdog;
use crate::tls::is_alert_received;
use crate::ws::RawFrame;
use crate::ws_async::{
    AsyncHeadFault, HaltableListener, send_report, try_frame_async_head, try_read_async_frame_rest,
    unless_halted, until_peer_closed,
};

/// The most bytes one offered head may take before the backend refuses it.
const HEAD_LIMIT: usize = 16 * 1024;

/// The first byte of every TLS handshake record.
const TLS_HANDSHAKE_RECORD: u8 = 0x16;

/// The text frame a backend sends once its upgrade is live.
pub const BACKEND_GREETING: &[u8] = b"from-backend";

/// The second text frame a read-ahead backend sends, after the client's own.
///
/// Different bytes from [`BACKEND_GREETING`] on purpose: a downstream peer that
/// read the coalesced first frame twice would read the greeting where this is
/// expected, so the two frames cannot be confused for one another.
pub const BACKEND_FOLLOWUP: &[u8] = b"after-read-ahead";

/// What one scripted connection does once it has read the proxy's offer.
#[derive(Clone)]
pub enum BackendScript {
    /// Write the bytes this builder makes from the offer's accept value, close
    /// the write half, and report the proxy's release of the transport.
    ///
    /// The accept value is derived from the key the proxy actually sent, so a
    /// row that is about a *wrong* accept states that and nothing else.
    Answer(fn(&str) -> String),
    /// Write the head this builder makes, keep both halves open, and report the
    /// proxy's release of the transport.
    ///
    /// Distinct from [`Self::Answer`], which closes its write half: a row about
    /// a backend the proxy connected and then had to let go of cannot be served
    /// by a backend that let go first.
    Negotiated(fn(&str) -> String),
    /// Write the head this builder makes, then exchange one real frame each
    /// way and answer the proxy's close.
    Upgrade(fn(&str) -> String),
    /// Write the head this builder makes and the first frame in the same write,
    /// then answer one client frame with a second frame before closing.
    ///
    /// The single write is the claim: a proxy that rebuilt the backend
    /// transport from a bare socket would drop the frame that arrived behind
    /// the head, and one that replayed its read-ahead would forward it twice.
    ReadAhead(fn(&str) -> String),
    /// Write nothing and hold the connection until the proxy releases it.
    Hold,
    /// Expect a TLS client hello under this configuration's untrusted
    /// certificate, and report whether the handshake was refused.
    UntrustedTls(Arc<rustls::ServerConfig>),
}

/// A phase one scripted connection reached, tagged with its accept index.
#[derive(Debug, Eq, PartialEq)]
pub enum BackendEvent {
    /// The whole offered head, as the proxy sent it.
    Offered(usize, Box<str>),
    /// The TLS client refused the untrusted certificate before any request.
    TlsRefused(usize),
    /// The payload of the one client frame the proxy forwarded.
    Exchanged(usize, Box<[u8]>),
    /// The proxy released the transport: end of stream or a reset.
    Released(usize),
    /// The backend could not serve its script, and why.
    Failed(usize, Box<str>),
}

/// The ordered subprotocol tokens every `Sec-WebSocket-Protocol` line offers.
///
/// Every line, in order, split on commas: the proxy may carry the offer in one
/// line or several, and either spelling is the same ordered offer.
pub fn offered_protocols(head: &str) -> Box<[Box<str>]> {
    header_values(head, "sec-websocket-protocol")
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(Box::from)
        .collect()
}

/// The accept value a correct backend answers `key` with.
pub fn accept_for(key: &str) -> String {
    tungstenite::handshake::derive_accept_key(key.as_bytes())
}

/// A correct `101` for `accept`, carrying `extra` header lines.
///
/// Every scripted answer starts from this, whether the row is about what the
/// extra lines say or about what happens after a wholly correct head: a row
/// that spelled its own valid head could drift from the one the rest of the
/// suite calls correct.
pub fn switching_protocols(accept: &str, extra: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\
         {extra}\r\n"
    )
}

/// The correct `101` a backend that selects no subprotocol answers with.
pub fn switching_without_selection(accept: &str) -> String {
    switching_protocols(accept, "")
}

/// A scripted backend that owns its listener, its sockets, and its accept loop.
pub struct ScriptedWsBackend {
    listener: HaltableListener<BackendEvent>,
}

impl ScriptedWsBackend {
    /// Bind a backend that serves accept index `n` with `script[n]`.
    ///
    /// The script is sealed: nothing appends a row once the backend listens.
    pub async fn bind(script: Box<[BackendScript]>) -> Self {
        let listener = HaltableListener::bind(
            "the scripted WebSocket backend",
            |_| {
                move |stream, row, events, halt| {
                    let entry = script.get(row).cloned().unwrap_or(BackendScript::Hold);
                    serve_row(stream, row, entry, events, halt)
                }
            },
            BackendEvent::Failed,
        )
        .await;
        Self { listener }
    }

    /// The backend as a proxy route names it over plain HTTP.
    pub fn http_url(&self) -> Box<str> {
        format!("http://{}", self.listener.addr()).into_boxed_str()
    }

    /// The backend as a proxy route names it over HTTPS.
    pub fn https_url(&self) -> Box<str> {
        format!("https://{}", self.listener.addr()).into_boxed_str()
    }

    /// The next phase the backend reports, or a failure naming the wait.
    pub async fn next(&mut self, context: &str) -> BackendEvent {
        match within_watchdog(self.listener.recv()).await {
            Some(Some(event)) => event,
            Some(None) => panic!("{context}: the scripted backend stopped reporting"),
            None => panic!("{context}: the scripted backend reported nothing"),
        }
    }

    /// Require that row `row`'s next report is its offer, and return the head.
    pub async fn offered(&mut self, row: usize, context: &str) -> Box<str> {
        self.next_offer(row, context).await.unwrap_or_else(|other| {
            panic!("{context}: expected row {row}'s offer, the backend reported {other:?}")
        })
    }

    /// [`Self::offered`] for a row whose offer carries a credential.
    ///
    /// A mismatch names the report's kind and row and never prints a head, so a
    /// fixture fault cannot leak a value the proxy was sent.
    pub async fn offered_unprinted(&mut self, row: usize, context: &str) -> Box<str> {
        self.next_offer(row, context)
            .await
            .unwrap_or_else(|other| match other {
                BackendEvent::Offered(index, _) => panic!(
                    "{context}: expected row {row}'s offer, the backend reported row {index}'s"
                ),
                other => {
                    panic!("{context}: expected row {row}'s offer, the backend reported {other:?}")
                }
            })
    }

    /// Row `row`'s offered head, or whatever the backend reported instead.
    async fn next_offer(&mut self, row: usize, context: &str) -> Result<Box<str>, BackendEvent> {
        match self.next(context).await {
            BackendEvent::Offered(index, head) if index == row => Ok(head),
            other => Err(other),
        }
    }

    /// Require that row `row`'s next report is `expected`.
    pub async fn expect(&mut self, expected: BackendEvent, context: &str) {
        let event = self.next(context).await;
        assert_eq!(event, expected, "{context}: the scripted backend reported");
    }

    /// Stop accepting, close every held connection, join the accept loop,
    /// prove the backend's address is free again, and require that nothing
    /// was left unreported.
    ///
    /// The join is bounded by the watchdog's wall clock, so a paused-clock row
    /// can finish its backend too.
    pub async fn finish(self, context: &str) {
        self.listener.finish(context, within_watchdog).await;
    }
}

async fn serve_row(
    mut stream: TcpStream,
    row: usize,
    script: BackendScript,
    events: mpsc::UnboundedSender<BackendEvent>,
    mut halt: watch::Receiver<bool>,
) {
    // A halted fixture closes every held connection whatever phase its row
    // reached, so a row that failed early cannot park the join.
    let outcome = unless_halted(&mut halt, run_script(&mut stream, row, script, &events)).await;
    if let Some(Err(fault)) = outcome {
        send_report(&events, BackendEvent::Failed(row, fault));
    }
}

async fn run_script(
    stream: &mut TcpStream,
    row: usize,
    script: BackendScript,
    events: &mpsc::UnboundedSender<BackendEvent>,
) -> Result<(), Box<str>> {
    match script {
        BackendScript::UntrustedTls(config) => refuse_tls(stream, row, config, events).await,
        BackendScript::Answer(answer) => {
            let key = read_offer(stream, row, events).await?;
            write_then_release(stream, row, &answer(&accept_for(&key)), events).await
        }
        BackendScript::Negotiated(answer) => {
            let key = read_offer(stream, row, events).await?;
            hold_after_answering(stream, row, &answer(&accept_for(&key)), events).await
        }
        BackendScript::Upgrade(answer) => {
            let key = read_offer(stream, row, events).await?;
            exchange(stream, row, &answer(&accept_for(&key)), events).await
        }
        BackendScript::ReadAhead(answer) => {
            let key = read_offer(stream, row, events).await?;
            exchange_behind_the_head(stream, row, &answer(&accept_for(&key)), events).await
        }
        BackendScript::Hold => {
            read_offer(stream, row, events).await?;
            await_release(stream, row, events).await
        }
    }
}

/// Read the proxy's whole offered head, report it, and return its key.
async fn read_offer(
    stream: &mut TcpStream,
    row: usize,
    events: &mpsc::UnboundedSender<BackendEvent>,
) -> Result<Box<str>, Box<str>> {
    let head = match try_frame_async_head(stream, HEAD_LIMIT).await {
        Ok(Some(head)) => head,
        Ok(None) => return Err("the proxy closed before its offer ended".into()),
        Err(AsyncHeadFault::Read(error)) => {
            return Err(format!("reading the offer failed: {error}").into());
        }
        Err(AsyncHeadFault::PastLimit) => return Err("the offered head passed its limit".into()),
        Err(AsyncHeadFault::NotUtf8(_)) => return Err("the offer was not UTF-8".into()),
    };
    let key = header_values(&head, "sec-websocket-key")
        .next()
        .map(Box::<str>::from)
        .ok_or_else(|| Box::<str>::from("the offer carried no Sec-WebSocket-Key"))?;
    send_report(events, BackendEvent::Offered(row, head));
    Ok(key)
}

/// Write one scripted answer, close the write half, and wait for release.
async fn write_then_release(
    stream: &mut TcpStream,
    row: usize,
    answer: &str,
    events: &mpsc::UnboundedSender<BackendEvent>,
) -> Result<(), Box<str>> {
    // A proxy that already released the transport is the outcome this row
    // waits for, so a write a gone peer refuses is not a fault. Any other
    // failure is.
    unless_released(
        stream.write_all(answer.as_bytes()).await,
        "writing the answer",
    )?;
    unless_released(stream.shutdown().await, "closing the write half")?;
    await_release(stream, row, events).await
}

/// Write one scripted answer, keep both halves open, and wait for release.
///
/// The write half stays open because the row's subject is the proxy letting the
/// transport go: a backend that shut its own half down first would supply the
/// end of stream the row is meant to observe the proxy cause.
async fn hold_after_answering(
    stream: &mut TcpStream,
    row: usize,
    answer: &str,
    events: &mpsc::UnboundedSender<BackendEvent>,
) -> Result<(), Box<str>> {
    stream
        .write_all(answer.as_bytes())
        .await
        .map_err(|error| format!("writing the held answer failed: {error}"))?;
    await_release(stream, row, events).await
}

/// Hold the transport until the proxy releases it, and report the release.
async fn await_release(
    stream: &mut TcpStream,
    row: usize,
    events: &mpsc::UnboundedSender<BackendEvent>,
) -> Result<(), Box<str>> {
    until_peer_closed(stream)
        .await
        .map_err(|error| format!("awaiting the release failed: {error}"))?;
    send_report(events, BackendEvent::Released(row));
    Ok(())
}

/// Complete the upgrade, send one frame, report the one frame forwarded back,
/// then answer the proxy's close and wait for release.
async fn exchange(
    stream: &mut TcpStream,
    row: usize,
    answer: &str,
    events: &mpsc::UnboundedSender<BackendEvent>,
) -> Result<(), Box<str>> {
    stream
        .write_all(answer.as_bytes())
        .await
        .map_err(|error| format!("writing the upgrade failed: {error}"))?;
    write_server_frame(stream, 0x1, BACKEND_GREETING).await?;
    carry_client_frame(stream, row, events).await?;
    answer_close(stream, row, events).await
}

/// Complete the upgrade with the first frame behind the head in one write,
/// answer the forwarded client frame with a second frame, then close.
///
/// The second frame is what makes the first one countable: a downstream peer
/// reads the greeting, its own reply crosses, and the next frame it reads is
/// this one — so a greeting delivered twice, or not at all, is visible.
async fn exchange_behind_the_head(
    stream: &mut TcpStream,
    row: usize,
    answer: &str,
    events: &mpsc::UnboundedSender<BackendEvent>,
) -> Result<(), Box<str>> {
    let mut opening = answer.as_bytes().to_vec();
    opening.extend_from_slice(&server_frame(0x1, BACKEND_GREETING));
    stream
        .write_all(&opening)
        .await
        .map_err(|error| format!("writing the coalesced upgrade failed: {error}"))?;
    carry_client_frame(stream, row, events).await?;
    write_server_frame(stream, 0x1, BACKEND_FOLLOWUP).await?;
    answer_close(stream, row, events).await
}

/// Read the one text frame the proxy forwarded, and report its payload.
async fn carry_client_frame(
    stream: &mut TcpStream,
    row: usize,
    events: &mpsc::UnboundedSender<BackendEvent>,
) -> Result<(), Box<str>> {
    let (opcode, payload) = read_client_frame(stream).await?;
    if opcode != 0x1 {
        return Err(format!("the proxy forwarded opcode {opcode:#x}, not text").into());
    }
    send_report(events, BackendEvent::Exchanged(row, payload));
    Ok(())
}

/// Drain until the proxy's close, answer it, and wait for the release.
async fn answer_close(
    stream: &mut TcpStream,
    row: usize,
    events: &mpsc::UnboundedSender<BackendEvent>,
) -> Result<(), Box<str>> {
    loop {
        let (opcode, _) = read_client_frame(stream).await?;
        if opcode == 0x8 {
            break;
        }
    }
    write_server_frame(stream, 0x8, &[0x03, 0xe8]).await?;
    unless_released(stream.shutdown().await, "closing the write half")?;
    await_release(stream, row, events).await
}

/// Accept `result` when it failed only because the proxy already released the
/// transport, and report every other failure as the backend's fault.
fn unless_released(result: std::io::Result<()>, operation: &str) -> Result<(), Box<str>> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if crate::http::is_closed_connection_error(&error) => Ok(()),
        Err(error) => Err(format!("{operation} failed: {error}").into()),
    }
}

/// Refuse to read anything before a TLS client hello, then let the client
/// refuse the untrusted certificate.
///
/// The first byte is peeked rather than read so the TLS acceptor sees the whole
/// hello. A plaintext request here is a fallback the proxy must never take.
async fn refuse_tls(
    stream: &mut TcpStream,
    row: usize,
    config: Arc<rustls::ServerConfig>,
    events: &mpsc::UnboundedSender<BackendEvent>,
) -> Result<(), Box<str>> {
    let mut first = [0_u8; 1];
    match stream.peek(&mut first).await {
        Ok(0) => return Err("the proxy closed before any TLS record".into()),
        Ok(_) => {}
        Err(error) => return Err(format!("peeking the first record failed: {error}").into()),
    }
    if first[0] != TLS_HANDSHAKE_RECORD {
        return Err(format!(
            "the proxy sent plaintext byte {:#04x} to a TLS backend",
            first[0]
        )
        .into());
    }
    // Only an alert is the proxy's refusal: a reset or an early end of stream
    // could come from a proxy that never checked the certificate.
    match tokio_rustls::TlsAcceptor::from(config).accept(stream).await {
        Ok(_) => Err("the proxy accepted an untrusted backend certificate".into()),
        Err(error) if is_alert_received(&error) => {
            send_report(events, BackendEvent::TlsRefused(row));
            Ok(())
        }
        Err(error) => Err(format!("the TLS accept failed without an alert: {error}").into()),
    }
}

/// One final unmasked server frame.
///
/// Separate from the write so a row that puts its head and its first frame in
/// one write frames that frame the same way every other row does.
fn server_frame(opcode: u8, payload: &[u8]) -> Box<[u8]> {
    RawFrame::complete(opcode, payload).encode()
}

/// Write one final unmasked server frame.
async fn write_server_frame(
    stream: &mut TcpStream,
    opcode: u8,
    payload: &[u8],
) -> Result<(), Box<str>> {
    stream
        .write_all(&server_frame(opcode, payload))
        .await
        .map_err(|error| format!("writing a frame failed: {error}").into())
}

/// Read one masked client frame and return its opcode and unmasked payload.
async fn read_client_frame(stream: &mut TcpStream) -> Result<(u8, Box<[u8]>), Box<str>> {
    let mut first = [0_u8; 1];
    let frame = match stream.read_exact(&mut first).await {
        Ok(_) => try_read_async_frame_rest(stream, first[0]).await,
        Err(error) => Err(error),
    }
    .map_err(|error| format!("reading a frame failed: {error}"))?;
    match frame.masked {
        true => Ok((frame.opcode, frame.payload)),
        false => Err("the proxy sent an unmasked client frame".into()),
    }
}
