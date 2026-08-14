//! What one authority string names, and when two of them name the same place.
//!
//! Split from the Origin policy above it because these are different
//! questions: this file decides what a host and port are, and only that. A
//! handshake's Origin header is compared through here rather than parsed at the
//! policy, so an authority that cannot be represented is refused once instead of
//! once per comparison.

/// One authority, and the port it stated for itself.
///
/// The port is kept beside the parsed value rather than re-read from it,
/// because the two answers differ: a missing port is a default this comparison
/// supplies, and a malformed one is a refusal.
struct ParsedAuthority {
    authority: hyper::http::uri::Authority,
    port: Option<u16>,
}

/// Whether an Origin authority names the same place as a Host authority.
///
/// Each arm refuses rather than guesses. An origin with no scheme this proxy
/// serves, an authority neither side can parse, and a port that was written but
/// not understood are all different ways of not matching, and none of them is a
/// match.
pub(super) fn origin_matches_host(origin: &str, host: &str) -> bool {
    let (scheme, origin_authority) = match origin.split_once("://") {
        Some(parts) => parts,
        None => return false,
    };
    let default_port = match scheme {
        value if value.eq_ignore_ascii_case("http") => 80,
        value if value.eq_ignore_ascii_case("https") => 443,
        _ => return false,
    };
    let origin = match parse_authority(origin_authority) {
        Some(authority) => authority,
        None => return false,
    };
    let host = match parse_authority(host) {
        Some(authority) => authority,
        None => return false,
    };
    let host_matches = origin
        .authority
        .host()
        .eq_ignore_ascii_case(host.authority.host());
    let port_matches = match host.port {
        Some(host_port) => host_port == origin.port.unwrap_or(default_port),
        None => origin
            .port
            .is_none_or(|origin_port| origin_port == default_port),
    };
    host_matches && port_matches
}

fn parse_authority(value: &str) -> Option<ParsedAuthority> {
    match value
        .bytes()
        .any(|byte| matches!(byte, b'@' | b'/' | b'?' | b'#' | b',' | b' ' | b'\t'))
    {
        true => return None,
        false => {}
    }
    let authority: hyper::http::uri::Authority = value.parse().ok()?;
    match authority.host().is_empty() {
        true => return None,
        false => {}
    }
    let port = explicit_authority_port(&authority)?;
    Some(ParsedAuthority { authority, port })
}

fn explicit_authority_port(authority: &hyper::http::uri::Authority) -> Option<Option<u16>> {
    let value = authority.as_str();
    let has_separator = match value.starts_with('[') {
        true => value.contains("]:"),
        false => value.contains(':'),
    };
    match (has_separator, authority.port_u16()) {
        (false, None) => Some(None),
        (true, Some(port)) => Some(Some(port)),
        _ => None,
    }
}
