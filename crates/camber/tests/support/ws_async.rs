//! The async half of the raw transport helpers, over Tokio streams.
//!
//! `ws.rs` frames WebSocket traffic on a blocking `std::net::TcpStream`, which
//! is the wrong shape for a case that must hold a transport open across an
//! `await` — a lifecycle probe, a proxy bridge, a shutdown observed from the
//! peer's side. Those harnesses each grew their own copy of the same six
//! helpers, differing only in the word their panic messages used for the site.
//! That word is a parameter here, so the framing, the bounds, and the verdicts
//! are stated once.

use std::future::Future;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Bounds every async transport observation a harness waits on.
///
/// Deliberately generous: it is a hang guard, not a timing assertion. A wait
/// that reaches it reports failure instead of parking the test binary.
pub const ASYNC_EVENT_TIMEOUT: Duration = Duration::from_secs(5);

/// The bound a case that watches its server's own stop deadline expire runs
/// under.
///
/// One value for every harness that needs such a bound: the claim those cases
/// make is which multiple of the deadline a stop took, so a second value would
/// only be a second thing to keep in step with [`assert_within_one_deadline`].
///
/// Two seconds, and the tolerance is why it is not shorter. The elapsed time a
/// case measures spans its own scheduling as well as the deadline — a checkpoint
/// wait, a socket frame read, a whole server join — and the assertion allows one
/// further deadline for all of it. At two seconds that leaves the case's own work
/// more than two seconds of slack on the reference environment: a warm
/// `cargo test` on an 8-core developer machine, one case at a time under
/// `--test-threads=1`, where that work takes single-digit milliseconds. A
/// shorter bound spends the same slack on scheduling and fails a loaded runner
/// for the reason it was loaded rather than for the second armed deadline the
/// case is about.
pub const EXPIRING_STOP: Duration = Duration::from_secs(2);

/// A stop that ended within the one deadline its server was given.
///
/// The claim the two-stage abort has to keep. A bridge the abort cannot reach is
/// not a server that hangs — it is a server that ends one whole
/// `shutdown_timeout` late, because the escalation arms a second deadline of its
/// own. Elapsed time is the only thing that tells those two apart, so the
/// tolerance [`EXPIRING_STOP`] documents is what stands between the two answers.
///
/// `requested` is taken before the stop is asked for, which is the pessimistic
/// end of the measurement: everything the case does afterwards counts against
/// the same budget.
pub fn assert_within_one_deadline(requested: tokio::time::Instant, what: &str) {
    let elapsed = requested.elapsed();
    assert!(
        elapsed < EXPIRING_STOP * 2,
        "{what}: the stop took {elapsed:?}, which is past the one {EXPIRING_STOP:?} deadline it was given"
    );
}

/// The widest payload these helpers write in one frame.
///
/// A test frame that needed an extended length would be testing the length
/// encoding rather than the transport, and `ws.rs` already owns that.
const MAX_SHORT_PAYLOAD: usize = 125;

/// The mask key a test client frames with.
///
/// Any key satisfies the protocol's masking rule; a fixed one keeps a failing
/// frame readable in a capture.
const CLIENT_MASK: [u8; 4] = [0x12, 0x34, 0x56, 0x78];

/// Await `future` under `bound`, naming the site that was waiting.
async fn within<F: Future>(context: &str, bound: Duration, future: F) -> F::Output {
    tokio::time::timeout(bound, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {context}"))
}

/// Await one lifecycle observation, bounded by [`ASYNC_EVENT_TIMEOUT`].
///
/// Every async transport wait in the suite goes through this, so an observation
/// that never arrives fails its test at the bound rather than parking the
/// binary on it.
pub async fn lifecycle_event<F: Future>(context: &str, future: F) -> F::Output {
    within(context, ASYNC_EVENT_TIMEOUT, future).await
}

/// Read an HTTP head off any async stream: every byte through the blank line
/// that ends it.
///
/// Generic over the transport because a head arrives over TLS as readily as
/// over TCP, and a `TcpStream`-only reader would make the TLS caller write its
/// own. Sealed, because nothing appends to a head once it is framed.
pub async fn read_async_head<S>(stream: &mut S, context: &str, bound: Duration) -> Box<str>
where
    S: AsyncRead + Unpin,
{
    within(context, bound, frame_async_head(stream, context)).await
}

/// [`read_async_head`] with no bound of its own, for a caller that owns one.
///
/// A `#[tokio::test(start_paused = true)]` case cannot use the bounded form.
/// Paused time auto-advances whenever the runtime goes idle, and it goes idle
/// exactly while this read waits on a real socket — so a `tokio::time::timeout`
/// jumps straight to its own deadline and expires before the I/O can land. Such
/// a caller wraps this in a real-clock bound instead. The framing itself is
/// shared, so the two forms cannot drift on what ends a head.
pub async fn read_async_head_unbounded<S>(stream: &mut S, context: &str) -> Box<str>
where
    S: AsyncRead + Unpin,
{
    frame_async_head(stream, context).await
}

/// Read every byte through the blank line that ends a head.
async fn frame_async_head<S>(stream: &mut S, context: &str) -> Box<str>
where
    S: AsyncRead + Unpin,
{
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let count = stream
            .read(&mut byte)
            .await
            .unwrap_or_else(|error| panic!("{context}: failed reading the HTTP head: {error}"));
        assert_ne!(count, 0, "{context}: the peer closed mid-HTTP head");
        head.push(byte[0]);
    }
    String::from_utf8(head)
        .unwrap_or_else(|error| panic!("{context}: the HTTP head was not UTF-8: {error}"))
        .into_boxed_str()
}

/// [`read_async_head`] over TCP under the shared bound.
pub async fn read_async_http_head(stream: &mut TcpStream, context: &str) -> Box<str> {
    read_async_head(stream, context, ASYNC_EVENT_TIMEOUT).await
}

/// Read one server frame's opcode and payload, or `None` at end of stream.
///
/// End of stream is an answer, not a failure: these harnesses observe a
/// transport being given up, and the frame that never comes is what proves it.
/// The payload is sealed — nothing downstream of the read writes to it — which
/// matches the contract `ws.rs` states for its own frame reader.
pub async fn read_async_ws_frame_or_eof(
    stream: &mut TcpStream,
    context: &str,
) -> Option<(u8, Box<[u8]>)> {
    lifecycle_event(context, async {
        let mut header = [0_u8; 2];
        let first = stream
            .read(&mut header[..1])
            .await
            .unwrap_or_else(|error| panic!("{context}: failed reading a frame header: {error}"));
        match first {
            0 => None,
            _ => Some(read_async_frame_body(stream, context, header).await),
        }
    })
    .await
}

/// Read everything after a frame's first byte: the length, the payload, and the
/// opcode they belong to.
///
/// A server frame is never masked, so no mask key is read and one that arrived
/// would mean the payload offsets are wrong.
async fn read_async_frame_body(
    stream: &mut TcpStream,
    context: &str,
    mut header: [u8; 2],
) -> (u8, Box<[u8]>) {
    stream
        .read_exact(&mut header[1..])
        .await
        .unwrap_or_else(|error| panic!("{context}: failed reading a frame header: {error}"));
    assert_eq!(header[1] & 0x80, 0, "{context}: a server frame was masked");
    let length = read_async_frame_length(stream, context, header[1] & 0x7f).await;
    let mut payload = vec![0_u8; length];
    stream
        .read_exact(&mut payload)
        .await
        .unwrap_or_else(|error| panic!("{context}: failed reading a frame payload: {error}"));
    (header[0] & 0x0f, payload.into_boxed_slice())
}

/// Resolve a frame's payload length, reading an extended length when the short
/// field says one follows.
async fn read_async_frame_length(stream: &mut TcpStream, context: &str, short: u8) -> usize {
    match short {
        126 => {
            let mut extended = [0_u8; 2];
            stream
                .read_exact(&mut extended)
                .await
                .unwrap_or_else(|error| panic!("{context}: failed reading a length: {error}"));
            usize::from(u16::from_be_bytes(extended))
        }
        127 => {
            let mut extended = [0_u8; 8];
            stream
                .read_exact(&mut extended)
                .await
                .unwrap_or_else(|error| panic!("{context}: failed reading a length: {error}"));
            usize::try_from(u64::from_be_bytes(extended))
                .unwrap_or_else(|_| panic!("{context}: the frame length did not fit a usize"))
        }
        length => usize::from(length),
    }
}

/// Write one masked client frame.
pub async fn write_async_ws_frame(
    stream: &mut TcpStream,
    opcode: u8,
    payload: &[u8],
    context: &str,
) {
    assert!(
        payload.len() <= MAX_SHORT_PAYLOAD,
        "{context}: a test frame payload must fit one length byte"
    );
    let length = u8::try_from(payload.len()).expect("a short payload fits one length byte");
    let mut frame = Vec::with_capacity(payload.len() + 6);
    frame.extend_from_slice(&[0x80 | opcode, 0x80 | length]);
    frame.extend_from_slice(&CLIENT_MASK);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ CLIENT_MASK[index % CLIENT_MASK.len()]),
    );
    lifecycle_event(context, stream.write_all(&frame))
        .await
        .unwrap_or_else(|error| panic!("{context}: failed writing a frame: {error}"));
}

/// Require that the transport is closed.
///
/// An abortive close is a close: a peer that goes away reaches the reader as one
/// of the gone-peer kinds, which one depending on the platform, and the shared
/// predicate names that set once. A byte read here is the transport still being
/// held, which is what these cases exist to rule out.
pub async fn assert_transport_eof(stream: &mut TcpStream, context: &str) {
    lifecycle_event(context, async {
        let mut byte = [0_u8; 1];
        match stream.read(&mut byte).await {
            Ok(0) => {}
            Err(error) if super::http::is_closed_connection_error(&error) => {}
            Ok(count) => panic!("{context}: expected a closed transport, read {count} byte(s)"),
            Err(error) => panic!("{context}: failed observing the closed transport: {error}"),
        }
    })
    .await;
}

/// The most a refusal body is ever read before the reader gives up on it.
///
/// The expected body is one fixed sentence a category declared safe, so this is
/// a hang guard rather than a size claim: a peer that keeps writing is capped
/// here and fails the comparison below with what it did send, instead of filling
/// the test host's memory inside a bound that only limits time. Every sibling
/// reader in the suite carries such a cap; this one had only the clock.
const MAX_REFUSAL_BODY: u64 = 64 * 1024;

/// Require the client-safe body a refused upgrade carries, then a closed transport.
///
/// A refused upgrade is answered through rejection policy, so its head is
/// followed by the message that category declared safe. Reading to the peer's
/// end proves both at once: the body is exactly that message, and nothing
/// follows it but the close.
pub async fn assert_refusal_body_then_eof(stream: &mut TcpStream, expected: &str, context: &str) {
    lifecycle_event(context, async {
        let mut body = Vec::new();
        match stream.take(MAX_REFUSAL_BODY).read_to_end(&mut body).await {
            Ok(_) => {}
            Err(error) if super::http::is_closed_connection_error(&error) => {}
            Err(error) => panic!("{context}: failed reading the refusal body: {error}"),
        }
        assert_refusal_body(&body, expected, context);
    })
    .await;
}

/// Require that a refusal body is exactly the message its category declared safe.
///
/// One statement of the claim for every way of reading the body up to it. The
/// paused-clock cases cannot bound on the Tokio clock and read through a socket
/// deadline on a thread of their own, so they arrive here with bytes rather than
/// a stream — and a second spelling of the comparison is a second thing that can
/// come to disagree about what a refused peer is owed.
pub fn assert_refusal_body(body: &[u8], expected: &str, context: &str) {
    assert_eq!(
        String::from_utf8_lossy(body),
        expected,
        "{context}: the peer is told only what the refusal declared safe"
    );
}

/// Require the close handshake a graceful teardown owes its peer, then a
/// transport that is given up.
///
/// A graceful shutdown sends a close frame, takes the peer's reply, and lets the
/// socket end. Five harnesses claimed exactly that in five copies of the same
/// read, assertion, write, and end-of-stream check, differing only in the words
/// their failures used. `subject` supplies those words.
pub async fn assert_graceful_close_then_eof(stream: &mut TcpStream, subject: &str) {
    let (opcode, _) = read_async_ws_frame_or_eof(stream, &format!("the {subject} close"))
        .await
        .unwrap_or_else(|| panic!("{subject}: the transport ended without a close frame"));
    assert_eq!(
        opcode, 0x8,
        "{subject}: expected a close frame, got opcode {opcode:#x}"
    );
    write_async_ws_frame(stream, 0x8, &[], &format!("the {subject} close reply")).await;
    assert_transport_eof(stream, &format!("the {subject} transport")).await;
}

/// Require a close frame if one comes, then a transport that is given up.
///
/// [`assert_graceful_close_then_eof`] for a teardown entitled to skip the
/// courtesy: a forced abort or a supervisor unwind may drop the transport
/// outright, so end of stream answers the case as well as a close frame does.
/// Any other frame is neither, and fails.
pub async fn assert_optional_close_then_eof(stream: &mut TcpStream, subject: &str) {
    match read_async_ws_frame_or_eof(stream, &format!("the {subject} close")).await {
        None => {}
        Some((0x8, _)) => {
            write_async_ws_frame(stream, 0x8, &[], &format!("the {subject} close reply")).await;
            assert_transport_eof(stream, &format!("the {subject} transport")).await;
        }
        Some((opcode, payload)) => {
            panic!("{subject} emitted opcode {opcode:#x} with payload {payload:?}")
        }
    }
}

/// Require that a plain HTTP request to `path` still answers `200`.
///
/// The liveness half of a transport case: a listener that stopped serving
/// everything proves nothing about the one connection under test.
pub async fn assert_http_ok(addr: SocketAddr, path: &str, context: &str) {
    let mut stream = lifecycle_event(context, TcpStream::connect(addr))
        .await
        .unwrap_or_else(|error| panic!("{context}: failed connecting the HTTP probe: {error}"));
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    lifecycle_event(context, stream.write_all(request.as_bytes()))
        .await
        .unwrap_or_else(|error| panic!("{context}: failed writing the HTTP probe: {error}"));
    let response = read_async_http_head(&mut stream, context).await;
    assert_eq!(
        super::http::status_from_raw(&response),
        200,
        "{context}: the HTTP probe did not succeed: {response}"
    );
}
