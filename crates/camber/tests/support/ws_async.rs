//! The async half of the raw transport helpers, over Tokio streams.
//!
//! `ws.rs` frames WebSocket traffic on a blocking `std::net::TcpStream`, which
//! is the wrong shape for a case that must hold a transport open across an
//! `await` — a lifecycle probe, a proxy bridge, a shutdown observed from the
//! peer's side. Those harnesses each grew their own copy of the same six
//! helpers, differing only in the word their panic messages used for the site.
//! That word is a parameter here, so the framing, the bounds, and the verdicts
//! are stated once.

use std::future::{Future, IntoFuture};
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::{Router, ServerHandle};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub use super::halt::{HaltableListener, send_report, unless_halted, until_peer_closed};

/// Bounds every async transport observation a harness waits on.
///
/// Deliberately generous: it is a hang guard, not a timing assertion. A wait
/// that reaches it reports failure instead of parking the test binary.
pub const ASYNC_EVENT_TIMEOUT: Duration = Duration::from_secs(5);

pub use super::ws::{BINARY, CLOSE, PING, PONG, TEXT};

/// The granularity the runtime's timer rounds a deadline up to.
///
/// A sleep does not fire at its own instant but at the first tick at or after
/// it, so a row that advanced exactly onto a deadline would wait on a timer
/// that has not been reached. The margin is what crosses that rounding, and it
/// is why the exactness of a deadline is asserted on the instant production
/// published rather than on the tick a row happened to stop at.
pub const TIMER_TICK: Duration = Duration::from_millis(1);

/// The runtime clock, frozen for as long as this lives.
///
/// Resumed by `Drop` rather than by the row, so a row that fails while frozen
/// still hands its teardown a running clock: the teardown's bounds come off
/// that clock, and a frozen one would advance straight through them.
pub struct FrozenClock;

impl FrozenClock {
    pub fn freeze() -> Self {
        tokio::time::pause();
        Self
    }
}

impl Drop for FrozenClock {
    fn drop(&mut self) {
        tokio::time::resume();
    }
}

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
    frame_async_head_or_eof(stream, context)
        .await
        .unwrap_or_else(|| panic!("{context}: the peer closed mid-HTTP head"))
}

/// Read every byte through the blank line that ends a head, or `None` if the
/// peer closed before one arrived.
///
/// End of stream is an answer for a row that admits a transport which simply
/// went away, and a failure for every other row. Both forms read the same
/// bytes, so they cannot drift on what ends a head or on what a head may
/// contain.
async fn frame_async_head_or_eof<S>(stream: &mut S, context: &str) -> Option<Box<str>>
where
    S: AsyncRead + Unpin,
{
    try_frame_async_head(stream, usize::MAX)
        .await
        .unwrap_or_else(|fault| match fault {
            AsyncHeadFault::Read(error) => {
                panic!("{context}: failed reading the HTTP head: {error}")
            }
            AsyncHeadFault::NotUtf8(error) => {
                panic!("{context}: the HTTP head was not UTF-8: {error}")
            }
            AsyncHeadFault::PastLimit => {
                panic!("{context}: the HTTP head passed {} bytes", usize::MAX)
            }
        })
}

/// Why [`try_frame_async_head`] produced no head.
#[derive(Debug)]
pub enum AsyncHeadFault {
    /// The transport failed before the blank line arrived.
    Read(std::io::Error),
    /// The head grew past the caller's limit before its blank line.
    PastLimit,
    /// The head was complete but was not UTF-8.
    NotUtf8(std::string::FromUtf8Error),
}

/// Read every byte through the blank line that ends a head, `None` if the
/// peer closed first, or the fault that stopped the read.
///
/// The one framing every head reader shares. A fixture that must report a fault
/// as an event, rather than panic inside a task, calls this directly; the
/// panicking readers above turn each fault into their own verdict.
pub async fn try_frame_async_head<S>(
    stream: &mut S,
    limit: usize,
) -> Result<Option<Box<str>>, AsyncHeadFault>
where
    S: AsyncRead + Unpin,
{
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await.map_err(AsyncHeadFault::Read)? {
            0 => return Ok(None),
            _ => head.push(byte[0]),
        }
        if head.len() > limit {
            return Err(AsyncHeadFault::PastLimit);
        }
    }
    String::from_utf8(head)
        .map(|head| Some(head.into_boxed_str()))
        .map_err(AsyncHeadFault::NotUtf8)
}

/// [`read_async_head`] over TCP under the shared bound.
pub async fn read_async_http_head(stream: &mut TcpStream, context: &str) -> Box<str> {
    read_async_head(stream, context, ASYNC_EVENT_TIMEOUT).await
}

/// [`read_async_http_head`] for a row whose peer may have closed instead.
pub async fn read_async_http_head_or_eof(
    stream: &mut TcpStream,
    context: &str,
) -> Option<Box<str>> {
    within(
        context,
        ASYNC_EVENT_TIMEOUT,
        frame_async_head_or_eof(stream, context),
    )
    .await
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
    lifecycle_event(context, try_read_async_ws_frame_or_eof(stream))
        .await
        .unwrap_or_else(|error| panic!("{context}: failed reading a frame: {error}"))
}

pub async fn try_read_async_ws_frame_or_eof(
    stream: &mut TcpStream,
) -> io::Result<Option<(u8, Box<[u8]>)>> {
    let mut first = [0_u8; 1];
    if stream.read(&mut first).await? == 0 {
        return Ok(None);
    }
    let frame = try_read_async_frame_rest(stream, first[0]).await?;
    match frame.masked {
        true => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "a server frame was masked",
        )),
        false => Ok(Some((frame.opcode, frame.payload))),
    }
}

/// Read a required server frame without blocking its executor.
pub async fn read_async_ws_frame(stream: &mut TcpStream) -> (u8, Box<[u8]>) {
    read_async_ws_frame_or_eof(stream, "the direction peer's next frame")
        .await
        .expect("the direction peer closed before its next frame")
}

pub async fn read_async_ws_text_frame(stream: &mut TcpStream) -> Box<str> {
    let (opcode, payload) = read_async_ws_frame(stream).await;
    assert_eq!(opcode, TEXT, "the peer received a non-text frame");
    String::from_utf8(payload.into_vec())
        .expect("the peer received invalid UTF-8")
        .into_boxed_str()
}

pub async fn read_async_ws_binary_frame(stream: &mut TcpStream) -> Box<[u8]> {
    let (opcode, payload) = read_async_ws_frame(stream).await;
    assert_eq!(opcode, BINARY, "the peer received a non-binary frame");
    payload
}

pub async fn write_async_ws_text_frame(stream: &mut TcpStream, text: &str) {
    write_async_ws_frame(stream, TEXT, text.as_bytes(), "the peer's text").await;
}

/// Require the next frame an async peer takes is `expected` as text.
pub async fn expect_async_text(peer: &mut TcpStream, expected: &str, context: &str) {
    let frame = read_async_ws_frame_or_eof(peer, context).await;
    assert_eq!(
        frame,
        Some((TEXT, Box::from(expected.as_bytes()))),
        "{context}: the peer took another frame"
    );
}

/// Require the next frame an async peer takes is a close.
pub async fn expect_async_close(peer: &mut TcpStream, context: &str) {
    let (opcode, _) = read_async_ws_frame_or_eof(peer, context)
        .await
        .unwrap_or_else(|| panic!("{context}: the transport ended without a close frame"));
    assert_eq!(
        opcode, CLOSE,
        "{context}: expected a close frame, got opcode {opcode:#x}"
    );
}

/// Read exactly `expected` text frames off the peer, in order.
pub async fn assert_async_texts_in_order(stream: &mut TcpStream, expected: &[&str]) {
    for text in expected {
        expect_async_text(stream, text, "the peer's next admitted text").await;
    }
}

/// One frame read off an async transport.
pub struct AsyncFrame {
    pub opcode: u8,
    /// Whether the mask bit was set. The payload is unmasked either way.
    pub masked: bool,
    pub payload: Box<[u8]>,
}

/// Read everything after a frame's first byte: the length, any mask key, and
/// the payload.
///
/// The one async frame decoder. A server-frame reader and a client-frame reader
/// differ only in the mask bit they require, so each checks
/// [`AsyncFrame::masked`] itself. The payload is capped at the sync reader's
/// limit, so a corrupt length fails the read instead of the allocation.
pub async fn try_read_async_frame_rest(
    stream: &mut TcpStream,
    first: u8,
) -> io::Result<AsyncFrame> {
    let mut second = [0_u8; 1];
    stream.read_exact(&mut second).await?;
    let masked = second[0] & 0x80 != 0;
    let length = match second[0] & 0x7f {
        126 => {
            let mut extended = [0_u8; 2];
            stream.read_exact(&mut extended).await?;
            usize::from(u16::from_be_bytes(extended))
        }
        127 => {
            let mut extended = [0_u8; 8];
            stream.read_exact(&mut extended).await?;
            usize::try_from(u64::from_be_bytes(extended)).unwrap_or(usize::MAX)
        }
        short => usize::from(short),
    };
    if length > super::ws::MAX_FRAME_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("a {length}-byte frame exceeded the fixture limit"),
        ));
    }
    let mut mask = [0_u8; 4];
    if masked {
        stream.read_exact(&mut mask).await?;
    }
    let mut payload = vec![0_u8; length];
    stream.read_exact(&mut payload).await?;
    payload
        .iter_mut()
        .zip(mask.iter().cycle())
        .for_each(|(byte, key)| *byte ^= key);
    Ok(AsyncFrame {
        opcode: first & 0x0f,
        masked,
        payload: payload.into_boxed_slice(),
    })
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
    let frame = super::ws::RawFrame {
        mask: Some(CLIENT_MASK),
        ..super::ws::RawFrame::complete(opcode, payload)
    }
    .encode();
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
    expect_async_close(stream, &format!("the {subject} close")).await;
    write_async_ws_frame(stream, CLOSE, &[], &format!("the {subject} close reply")).await;
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
        Some((CLOSE, _)) => {
            write_async_ws_frame(stream, CLOSE, &[], &format!("the {subject} close reply")).await;
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
    let mut stream = lifecycle_event(
        context,
        super::http::request_on_new_peer(addr, path, "close"),
    )
    .await;
    let response = read_async_http_head(&mut stream, context).await;
    assert_eq!(
        super::http::status_from_raw(&response),
        200,
        "{context}: the HTTP probe did not succeed: {response}"
    );
}

/// One served router on a loopback address, and the owner it is stopped and
/// joined through.
pub struct OwnedServer {
    addr: SocketAddr,
    handle: ServerHandle,
}

impl OwnedServer {
    /// Bind a loopback listener and serve `router` on it.
    ///
    /// The listener is bound here rather than inside the server so the address
    /// is known before anything is served: a proxy route has to name its
    /// upstream before that upstream can answer.
    pub async fn bind(router: Router, context: &str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap_or_else(|error| panic!("{context}: binding the server failed: {error}"));
        Self::serve_on(listener, router, context)
    }

    /// Serve `router` on a listener the caller already bound.
    pub fn serve_on(listener: TcpListener, router: Router, context: &str) -> Self {
        let addr = listener
            .local_addr()
            .unwrap_or_else(|error| panic!("{context}: the served address is unreadable: {error}"));
        let handle = camber::http::serve_background(listener, router)
            .unwrap_or_else(|error| panic!("{context}: serving requires a Tokio runtime: {error}"));
        Self { addr, handle }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// The URL a proxy route names this server by.
    pub fn url(&self) -> Box<str> {
        format!("http://{}", self.addr).into_boxed_str()
    }

    /// The owner this server is stopped and joined through.
    pub fn handle(&self) -> &ServerHandle {
        &self.handle
    }

    /// Join the owner, whichever stop the caller asked for.
    pub async fn join(self) -> Result<(), RuntimeError> {
        self.handle.into_future().await
    }

    /// Stop the server gracefully and join its owner.
    pub async fn stop(self) -> Result<(), RuntimeError> {
        self.handle.shutdown();
        self.join().await
    }

    /// Stop the server gracefully, require a clean join, and prove its address
    /// is free again.
    ///
    /// The join is bounded by [`lifecycle_event`], a Tokio timeout. A paused
    /// clock auto-advances onto that timeout while the join waits on real I/O,
    /// so a paused-clock case must bound [`Self::stop`] on a real clock instead.
    pub async fn stop_cleanly(self, context: &str) {
        let addr = self.addr;
        lifecycle_event(context, self.stop())
            .await
            .unwrap_or_else(|error| panic!("{context}: the server did not stop cleanly: {error}"));
        super::http::assert_address_reused(addr, context).await;
    }
}
