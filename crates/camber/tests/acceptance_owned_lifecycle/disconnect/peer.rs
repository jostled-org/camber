//! What the test-side peer does on the wire, and what it observes there.
//!
//! Every read is framed and bounded: a single socket read returns whatever one
//! syscall produced, so a case that samples the socket once can pass on a
//! partial frame and then fail on the remainder of it.

use super::fixture::{BOUND, QUIET};
use super::probes::CauseProbe;
use super::routes::READY_PATH;
use crate::common::HttpResponse;
use camber::http::DisconnectCause;
use std::net::{Shutdown, SocketAddr};
use std::time::Instant;

/// Ceiling on one framed read, well above any frame these cases produce.
const FRAMED_READ_LIMIT: usize = 64 * 1024;

/// Send one whole request and require a response to it.
///
/// The empty headers, the empty body, and [`BOUND`] are the same at every call
/// site in this tree, so the bound is stated once here and each caller names
/// only what it asked for. `subject` is that name, and an unanswered request is
/// reported against it rather than against the transport.
pub(crate) fn send(addr: SocketAddr, method: &str, path: &str, subject: &str) -> HttpResponse {
    crate::common::request(addr, method, path, &[], &[], BOUND)
        .unwrap_or_else(|error| panic!("{subject} never arrived within the bound: {error}"))
}

/// Assert that `head` is the start of a produced `200` response.
fn assert_produced_head(head: &[u8], subject: &str) {
    assert!(
        head.starts_with(b"HTTP/1.1 200"),
        "{subject} never reached the peer: {:?}",
        String::from_utf8_lossy(head)
    );
}

/// Prove a completed connection frees its permit while the listener is live.
///
/// Under `connection_limit(1)` a retained permit makes the next request never
/// serve, so this is the leak probe the plan names — and it runs before
/// teardown, where a closed listener could mask the leak.
///
/// The first probe retries, because the connection whose permit this measures
/// is released asynchronously once its peer goes away; the second gets one
/// bounded attempt, so a permit the first probe itself retained fails here.
pub(super) fn assert_permit_released(addr: SocketAddr) {
    crate::common::wait_for_http_response(addr, BOUND)
        .expect("no connection was accepted after the measured one closed");
    let response = send(
        addr,
        "GET",
        READY_PATH,
        "the request after the permit probe",
    );
    assert_eq!(
        response.status, 200,
        "the request after the permit probe did not reach the router"
    );
}

/// Read a streaming response's head, framed by the blank line that ends it.
pub(super) fn read_head(client: &mut std::net::TcpStream) -> Box<[u8]> {
    crate::common::read_head(client, BOUND)
        .expect("the response head never arrived within the bound")
}

/// Read through `delimiter`, bounded.
///
/// Framing every read is what keeps the assertion about the bytes rather than
/// about how TCP split them.
pub(super) fn read_until(client: &mut std::net::TcpStream, delimiter: &[u8]) -> Box<[u8]> {
    crate::common::read_delimited(client, delimiter, FRAMED_READ_LIMIT, BOUND)
        .expect("the framed body never arrived within the bound")
}

/// Read a response body to the peer's close, bounded and capped at `limit`.
///
/// The size cap is the only part a case has to state: the bound is this tree's,
/// like every other read here, so a body that never finishes arriving fails
/// against the same deadline as the head that preceded it.
pub(super) fn read_to_close(client: &mut std::net::TcpStream, limit: usize) -> Box<[u8]> {
    crate::common::bounded_read(client, BOUND, limit)
        .expect("the body never finished arriving within the bound")
}

/// Read one whole HTTP response, bounded.
///
/// The deadline covers the head, the body, and every chunk between them. A
/// per-syscall bound would give each byte a fresh full budget, so a server that
/// stalled mid-response would park the case with nothing left to expire.
pub(super) fn read_response(client: &mut std::net::TcpStream) -> HttpResponse {
    crate::common::read_http_response(client, Some(Instant::now() + BOUND))
        .expect("the response never arrived within the bound")
}

/// Open a connection and send `path` without reading anything back.
pub(super) fn start_request(addr: SocketAddr, path: &str) -> std::net::TcpStream {
    let mut client = crate::common::connect(addr).expect("failed to connect the peer");
    crate::common::write_request(&mut client, "GET", path, &[], &[])
        .expect("failed to send the request");
    client
}

/// Open a request against a streaming route and read its produced head.
///
/// The four steps every streaming case opens with: send, wait for the handler
/// to arm its watcher, read the head to its framing delimiter, and require it
/// to be a produced `200`. The connection comes back live.
pub(super) fn open_produced_stream(
    addr: SocketAddr,
    path: &str,
    probe: &CauseProbe,
    subject: &str,
) -> std::net::TcpStream {
    let mut client = start_request(addr, path);
    probe.await_entry();
    let head = read_head(&mut client);
    assert_produced_head(&head, subject);
    client
}

/// Start a request, wait for its handler, abandon the peer, and report the
/// cause the response lifetime resolved to.
///
/// The barrier is what makes the close land on a genuinely in-flight response:
/// closing before the handler exists would prove nothing about one.
pub(super) fn abandon_after_entry(
    addr: SocketAddr,
    path: &str,
    probe: &CauseProbe,
) -> DisconnectCause {
    let client = start_request(addr, path);
    probe.await_entry();
    drop(client);
    probe.cause()
}

/// How many response bytes the server wrote within `QUIET`.
///
/// One read answers the whole question: a server that wrote a status line put
/// those bytes on the wire before this window opened, so they are in the first
/// read or they do not exist. Three outcomes mean the same thing — end of
/// stream, an expired window with nothing readable, and a peer already gone —
/// because under all three no response byte exists.
///
/// The gone-peer set is the shared one rather than a set named here. Which kind
/// a server that resets a half-closed peer reaches this reader as is the
/// platform's decision, and a narrower set would panic on the very outcome this
/// window expects.
///
/// `common::bounded_read` is deliberately not used. It restores the socket's
/// prior read timeout and reports a failed restore as a read failure, and macOS
/// refuses `setsockopt` on a fully closed socket, so a clean end of stream
/// comes back as `EINVAL` — exactly the observation this window is here to
/// make.
fn response_bytes_within_quiet(client: &mut std::net::TcpStream) -> usize {
    use std::io::Read;
    client
        .set_read_timeout(Some(QUIET))
        .expect("the peer rejected its quiet-window read bound");
    let mut buffer = [0_u8; 4096];
    match client.read(&mut buffer) {
        Ok(count) => count,
        Err(error)
            if crate::common::is_deadline_expiry(&error)
                || crate::common::is_closed_connection_error(&error) =>
        {
            0
        }
        Err(error) => panic!(
            "the peer could not be read after its half-close: {error} \
             (kind {:?}, raw {:?})",
            error.kind(),
            error.raw_os_error()
        ),
    }
}

/// What reading a peer to end of stream observed.
///
/// The two failures are kept apart deliberately: a transport that stayed open
/// past the bound and a read that broke are different defects, and collapsing
/// them discards the only description of the second.
#[cfg(feature = "ws")]
#[derive(Debug, Eq, PartialEq)]
pub(super) enum PeerClose {
    /// The peer reached end of stream within the bound.
    Closed,
    /// The bound expired with the transport still open.
    NeverClosed,
    /// The read itself failed before any close.
    ReadFailed(Box<str>),
}

/// Read a peer to end of stream, bounded.
///
/// The read is the shared one, which already counts an abortive close as a
/// close and bounds the whole read by one deadline rather than by one per
/// syscall. Its remaining errors are the expiry and a real I/O failure, which
/// this separates.
#[cfg(feature = "ws")]
pub(super) fn closed_within_bound(peer: &mut std::net::TcpStream) -> PeerClose {
    match crate::common::read_until_closed(peer, BOUND) {
        Ok(_) => PeerClose::Closed,
        Err(error) if crate::common::is_deadline_expiry(&error) => PeerClose::NeverClosed,
        Err(error) => PeerClose::ReadFailed(error.to_string().into_boxed_str()),
    }
}

/// Start a request, wait for its handler, half-close the peer, and require that
/// no response byte was written before reporting the cause.
///
/// The sibling of [`abandon_after_entry`] for the cases whose invariant names
/// the observation point: a full drop leaves nothing to read from, so "before
/// any header byte is written" could only be inferred. Half-closing sends the
/// same FIN — read EOF is exactly what the liveness wrapper keys on, so the
/// cause still resolves — while keeping the read side open to witness the
/// silence.
pub(super) fn half_close_after_entry(
    addr: SocketAddr,
    path: &str,
    probe: &CauseProbe,
) -> DisconnectCause {
    let mut client = start_request(addr, path);
    probe.await_entry();
    client
        .shutdown(Shutdown::Write)
        .expect("the peer's write side could not be half-closed");
    let written = response_bytes_within_quiet(&mut client);
    assert_eq!(
        written, 0,
        "the server wrote response bytes to a peer whose in-flight request \
         resolves before any header byte"
    );
    probe.cause()
}
