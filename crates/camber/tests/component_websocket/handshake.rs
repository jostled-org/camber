//! One upgrade request, stated header by header.
//!
//! `common::ws_upgrade_request_with` states the head Camber accepts and can only
//! add to it. Half the cases in this binary own the opposite claim — a head that
//! omits `Connection` or `Sec-WebSocket-Version`, or carries a version Camber
//! does not speak — and a repeated header is a different refusal than a replaced
//! one, so appending cannot express them. Two modules answered that by spelling
//! the whole request out, which left two statements of what a handshake looks
//! like in one binary. This is the one.
//!
//! The list is full rather than incremental because a full list expresses both
//! claims at once: a header left out is one the list omits, and a header
//! replaced is one the list states differently.

#![cfg(feature = "ws")]

use crate::common;
use std::io::{Read, Write};
use std::net::TcpStream;

/// One header line of an upgrade request.
///
/// Borrowed rather than owned: every case either states a literal or holds a
/// value that outlives the request it is spliced into.
pub type Header<'a> = (&'a str, &'a str);

/// The authority every case addresses, unless the case is about the authority.
pub const LOCAL_HOST: &str = "localhost";

/// The headers a handshake Camber accepts carries, addressed to `host`.
///
/// The accepted head in one place, because every case here is stated as a change
/// to it: one header dropped, one replaced, one added. A copy per module is a
/// copy that can drift from what Camber accepts, and the copy that drifts stops
/// provoking what its cases claim while going on reporting success.
pub fn accepted(host: &str) -> [Header<'_>; 5] {
    accepted_keyed(host, common::WS_KEY)
}

/// The accepted head, addressed to `host` and offering `key`.
///
/// A generated case varies the key per row; every other line stays the one
/// accepted head.
pub fn accepted_keyed<'a>(host: &'a str, key: &'a str) -> [Header<'a>; 5] {
    [
        ("Host", host),
        ("Upgrade", "websocket"),
        ("Connection", "Upgrade"),
        ("Sec-WebSocket-Key", key),
        ("Sec-WebSocket-Version", "13"),
    ]
}

/// The accepted head with `extra` headers after it.
///
/// Sealed: a case hands the result straight to [`handshake_request`], and
/// nothing appends to a header list once the case has stated it.
pub fn accepted_plus<'a>(host: &'a str, extra: &[Header<'a>]) -> Box<[Header<'a>]> {
    accepted(host)
        .into_iter()
        .chain(extra.iter().copied())
        .collect()
}

/// The accepted head with one header left out.
pub fn accepted_without<'a>(host: &'a str, dropped: &str) -> Box<[Header<'a>]> {
    accepted(host)
        .into_iter()
        .filter(|(name, _)| *name != dropped)
        .collect()
}

/// The accepted head with one header carrying another value.
///
/// A replacement rather than an addition, because a handshake carrying two
/// `Sec-WebSocket-Version` values is a different refusal than one carrying a
/// version Camber does not speak — and it is the second that these cases claim.
pub fn accepted_with<'a>(host: &'a str, replaced: &str, value: &'a str) -> Box<[Header<'a>]> {
    accepted(host)
        .into_iter()
        .map(|(name, current)| match name == replaced {
            true => (name, value),
            false => (name, current),
        })
        .collect()
}

/// The upgrade request carrying exactly `headers`, in the order given.
///
/// Sealed, because nothing appends to a request once it is framed. The header
/// lines themselves are written by the shared appender, so a case that added a
/// header through `common::ws_upgrade_request_with` and one that stated its
/// whole list here cannot disagree about how a header line is spelled.
pub fn handshake_request(path: &str, headers: &[Header<'_>]) -> Box<str> {
    raw_request(
        RequestLine {
            method: "GET",
            path,
            version: "HTTP/1.1",
        },
        headers.iter().copied(),
        "",
    )
}

/// The request line a raw request opens with.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RequestLine<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub version: &'a str,
}

/// Any request, stated line by line, with `body` after the head.
///
/// The form a case uses when the request line or body is its subject. It frames
/// the head exactly as [`handshake_request`] does, so the two cannot disagree
/// on anything but what the case varies.
pub fn raw_request<'h>(
    line: RequestLine<'_>,
    headers: impl IntoIterator<Item = Header<'h>>,
    body: &str,
) -> Box<str> {
    let RequestLine {
        method,
        path,
        version,
    } = line;
    let mut request = format!("{method} {path} {version}\r\n");
    common::append_headers(&mut request, headers);
    request.push_str("\r\n");
    request.push_str(body);
    request.into_boxed_str()
}

/// Send `request` and read the response it answers with.
///
/// The connection and its bounds come from `common::connect`, and the head is
/// parsed by the shared response reader: a handshake case owns what it sends
/// and what the head must say, not a second statement of how a socket is
/// opened or how a status line is split.
///
/// Read under the bounded form, which arms one deadline over the whole reply.
/// The socket's own timeout bounds a single syscall, so a peer dribbling one
/// byte per read would never be cut off by it.
pub fn perform_raw_ws_handshake(
    addr: std::net::SocketAddr,
    request: &str,
) -> (TcpStream, common::HttpResponse) {
    let mut stream = common::connect(addr).expect("connect raw WebSocket client");
    stream
        .write_all(request.as_bytes())
        .expect("write raw WebSocket handshake");
    let response = common::read_http_response_bounded(&mut stream)
        .expect("read raw WebSocket handshake reply");
    (stream, response)
}

/// The switch an accepted upgrade answers with, transport disposition included.
///
/// A `101` that does not say `Connection: Upgrade` is one RFC 6455 §4.1
/// requires a conforming client to fail the handshake over, so no row is
/// entitled to a weaker predicate. The only request that could earn a different
/// answer — one declaring a payload the bridge would leave unframed — is
/// refused at the head instead.
pub fn assert_websocket_switch_accepting(head: &common::HttpResponse, accept: &str, context: &str) {
    assert_eq!(head.status, 101, "{context}: unexpected status: {head:?}");
    let upgrade = head.header_values("upgrade");
    assert_eq!(upgrade.len(), 1, "{context}: Upgrade header: {head:?}");
    assert!(
        upgrade[0].eq_ignore_ascii_case("websocket"),
        "{context}: invalid Upgrade header: {head:?}"
    );
    assert_eq!(
        *head.header_values("sec-websocket-accept"),
        [accept],
        "{context}: Sec-WebSocket-Accept header"
    );
    let connection = head.header_values("connection");
    assert_eq!(
        connection.len(),
        1,
        "{context}: Connection header: {head:?}"
    );
    assert!(
        connection[0].eq_ignore_ascii_case("upgrade"),
        "{context}: invalid Connection header: {head:?}"
    );
}

/// A refused handshake: the expected status, and nothing a `101` would carry.
pub fn assert_handshake_rejected(head: &common::HttpResponse, expected_status: u16, context: &str) {
    assert_eq!(
        head.status, expected_status,
        "{context}: unexpected rejection: {head:?}"
    );
    assert!(
        head.header_values("sec-websocket-accept").is_empty(),
        "{context}: rejection exposed Sec-WebSocket-Accept: {head:?}"
    );
    assert!(
        head.header_values("sec-websocket-protocol").is_empty(),
        "{context}: rejection selected a subprotocol: {head:?}"
    );
}

/// Finish the close a server-side callback started by returning.
///
/// The server's close frame first, then the reply it is owed, then the end of
/// the transport. A peer that dropped its socket at the `101` would leave the
/// bridge it was served by racing the teardown of the server under test.
pub fn complete_server_close(stream: &mut TcpStream, context: &str) {
    let (opcode, _) = common::try_read_ws_frame_raw(stream)
        .unwrap_or_else(|error| panic!("{context}: server close frame: {error}"));
    assert_eq!(opcode, 0x8, "{context}: expected the server's close frame");
    common::write_ws_close_frame(stream);
    assert_transport_ends(stream, context);
}

/// Read the end of a transport whose close handshake has completed.
pub fn assert_transport_ends(stream: &mut TcpStream, context: &str) {
    let mut rest = [0_u8; 64];
    match stream.read(&mut rest) {
        Ok(0) => {}
        Err(error) if common::is_closed_connection_error(&error) => {}
        Ok(count) => panic!("{context}: {count} bytes after the close handshake"),
        Err(error) => panic!("{context}: transport did not end after close: {error}"),
    }
}
