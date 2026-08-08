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
    [
        ("Host", host),
        ("Upgrade", "websocket"),
        ("Connection", "Upgrade"),
        ("Sec-WebSocket-Key", common::WS_KEY),
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
    let mut head = format!("GET {path} HTTP/1.1\r\n");
    common::append_headers(&mut head, headers.iter().copied());
    head.push_str("\r\n");
    head.into_boxed_str()
}
