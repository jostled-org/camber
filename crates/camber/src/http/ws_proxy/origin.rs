//! The Origin policy one WebSocket handshake is held to.
//!
//! What a handshake states about itself, and whether that is enough to compare.
//! The comparison itself belongs to `authority`; what this file owns is which
//! handshakes are refused and with what account of why.

use super::super::Request;
use super::super::rejection::Rejected;
use super::authority::origin_matches_host;
use super::handshake::named_request_headers;

/// Check the WebSocket Origin header against the request Host.
///
/// Returns `None` if the origin is acceptable (missing or same-host).
/// Returns the refusal the handshake earned otherwise.
///
/// Each arm states its own reason. A handshake that repeats `Origin`, one that
/// states no single `Host` to compare against, and one whose authorities
/// genuinely differ are three faults, and one sentence for all three tells an
/// operator something false about two of them.
pub(in crate::http) fn check_ws_origin(req: &Request) -> Option<Rejected> {
    let origin = match unique_request_header(req, "origin") {
        HeaderPresence::Absent => return None,
        HeaderPresence::Unique(origin) => origin,
        HeaderPresence::Repeated => {
            return rejected_origin("handshake carries more than one Origin header");
        }
    };
    let host = match unique_request_header(req, "host") {
        HeaderPresence::Unique(host) => host,
        HeaderPresence::Absent | HeaderPresence::Repeated => {
            return rejected_origin("handshake states no single Host to match the Origin against");
        }
    };

    match origin_matches_host(origin, host) {
        true => None,
        false => rejected_origin("handshake Origin does not match the requested Host"),
    }
}

/// How many values a request carries under one header name.
///
/// Three named answers rather than a `Result` with an empty error: a repeated
/// header and an absent one are different facts about the handshake, and each
/// of them is refused with its own account.
enum HeaderPresence<'a> {
    /// No value under this name.
    Absent,
    /// Exactly one value.
    Unique(&'a str),
    /// More than one value, so no single one names the request.
    Repeated,
}

fn unique_request_header<'a>(req: &'a Request, name: &'static str) -> HeaderPresence<'a> {
    let mut values = named_request_headers(req, name);
    match (values.next(), values.next()) {
        (Some(value), None) => HeaderPresence::Unique(value),
        (None, _) => HeaderPresence::Absent,
        (Some(_), Some(_)) => HeaderPresence::Repeated,
    }
}

fn rejected_origin(detail: &'static str) -> Option<Rejected> {
    Some(Rejected::ws_origin_rejected(detail))
}
