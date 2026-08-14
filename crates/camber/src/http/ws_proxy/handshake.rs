//! Whether a request is a WebSocket handshake, and what a valid one earns.
//!
//! Header validation, subprotocol selection, and the `101` itself. Nothing here
//! owns a transport or a lifecycle: this file either produces the upgrade both
//! bridges start from, or the refusal the peer gets instead. The Origin policy
//! is a separate question and lives beside this one.

use super::super::Request;
use super::super::body::HyperResponseBody;
use super::super::rejection::Rejected;

pub(in crate::http) enum WsUpgrade {
    Ready(hyper::upgrade::OnUpgrade, Box<str>),
    Rejected(WsHandshakeError),
}

pub(in crate::http) enum WsHandshakeError {
    BadRequest,
    UnsupportedVersion,
}

/// Extract the WebSocket upgrade future and accept key before consuming the request.
pub(in crate::http) fn extract_ws_upgrade(
    req: &mut hyper::Request<hyper::body::Incoming>,
) -> WsUpgrade {
    let accept_key = match validate_ws_handshake(req) {
        Ok(key) => tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes()),
        Err(error) => return WsUpgrade::Rejected(error),
    };
    WsUpgrade::Ready(hyper::upgrade::on(req), accept_key.into())
}

fn validate_ws_handshake(
    request: &hyper::Request<hyper::body::Incoming>,
) -> Result<&hyper::header::HeaderValue, WsHandshakeError> {
    if request.method() != hyper::Method::GET || request.version() != hyper::Version::HTTP_11 {
        return Err(WsHandshakeError::BadRequest);
    }
    let headers = request.headers();
    // `&&` rather than a tuple of both scans: the `Upgrade` header is the
    // cheap lookup and the one an ordinary request fails, so the token scan
    // over `Connection` never runs for traffic that was never a handshake.
    let asks_to_upgrade =
        is_ws_upgrade_head(headers) && header_contains_token(headers, "connection", "upgrade");
    match asks_to_upgrade {
        true => {}
        false => return Err(WsHandshakeError::BadRequest),
    }
    validate_ws_version(headers)?;
    validate_ws_subprotocols(headers)?;

    let key = match single_header(headers, "sec-websocket-key") {
        Some(key) if valid_ws_key(key.as_bytes()) => key,
        _ => return Err(WsHandshakeError::BadRequest),
    };
    Ok(key)
}

fn validate_ws_subprotocols(headers: &hyper::HeaderMap) -> Result<(), WsHandshakeError> {
    for value in headers.get_all("sec-websocket-protocol") {
        let value = value.to_str().map_err(|_| WsHandshakeError::BadRequest)?;
        if !value.split(',').map(str::trim).all(is_http_token) {
            return Err(WsHandshakeError::BadRequest);
        }
    }
    Ok(())
}

fn validate_ws_version(headers: &hyper::HeaderMap) -> Result<(), WsHandshakeError> {
    let mut versions = headers.get_all("sec-websocket-version").iter();
    let version = match versions.next() {
        Some(version) => version,
        None => return Err(WsHandshakeError::BadRequest),
    };
    match (version == "13", versions.next()) {
        (true, None) => Ok(()),
        _ => Err(WsHandshakeError::UnsupportedVersion),
    }
}

/// Whether a request head asks to leave HTTP for the WebSocket protocol.
///
/// The two routing predicates and the handshake validator all ask through
/// here, so they read a repeated `Upgrade` header the same way: a request
/// cannot be routed as an upgrade and then refused `400` by the validator for
/// a header the router was happy with.
pub(in crate::http) fn is_ws_upgrade_head(headers: &hyper::HeaderMap) -> bool {
    single_header_equals(headers, "upgrade", "websocket")
}

/// The same question, of a request whose head has already been collected.
pub(in crate::http) fn is_ws_upgrade_request(req: &Request) -> bool {
    single_value_equals(named_request_headers(req, "upgrade"), "websocket")
}

/// Every value a collected request carries under one header name.
///
/// The name is `'static`, the same way `single_header`'s is: every caller
/// passes a literal, and unifying it with the request borrow would cap the
/// returned values — which come from the request alone — at the shorter of the
/// two lifetimes.
pub(super) fn named_request_headers<'a>(
    req: &'a Request,
    name: &'static str,
) -> impl Iterator<Item = &'a str> {
    req.headers()
        .filter_map(move |(candidate, value)| candidate.eq_ignore_ascii_case(name).then_some(value))
}

fn single_header<'a>(
    headers: &'a hyper::HeaderMap,
    name: &'static str,
) -> Option<&'a hyper::header::HeaderValue> {
    let mut values = headers.get_all(name).iter();
    match (values.next(), values.next()) {
        (Some(value), None) => Some(value),
        _ => None,
    }
}

fn single_header_equals(headers: &hyper::HeaderMap, name: &'static str, expected: &str) -> bool {
    // An unreadable value keeps its place in the count rather than being
    // filtered out: two values, one of them invalid, is still a repeat.
    single_value_equals(
        headers
            .get_all(name)
            .iter()
            .map(|value| value.to_str().unwrap_or("")),
        expected,
    )
}

/// Whether a header carries exactly one value, and that value is `expected`.
///
/// A repeated header is not a match. Both header representations — the borrowed
/// hyper map and a collected request — answer through this one rule.
fn single_value_equals<'a>(mut values: impl Iterator<Item = &'a str>, expected: &str) -> bool {
    match (values.next(), values.next()) {
        (Some(value), None) => value.eq_ignore_ascii_case(expected),
        _ => false,
    }
}

fn header_contains_token(headers: &hyper::HeaderMap, name: &'static str, expected: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .try_fold(false, |found, value| {
            value
                .to_str()
                .ok()?
                .split(',')
                .try_fold(found, |seen, token| {
                    let token = token.trim_matches([' ', '\t']);
                    is_http_token(token).then_some(seen || token.eq_ignore_ascii_case(expected))
                })
        })
        .is_some_and(|found| found)
}

fn valid_ws_key(key: &[u8]) -> bool {
    match key {
        [symbols @ .., b'=', b'='] if symbols.len() == 22 => {
            symbols.iter().copied().all(is_base64_symbol)
                && symbols
                    .last()
                    .copied()
                    .and_then(base64_value)
                    .is_some_and(|value| value & 0x0f == 0)
        }
        _ => false,
    }
}

const fn is_base64_symbol(byte: u8) -> bool {
    base64_value(byte).is_some()
}

const fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

/// Select the first syntactically valid protocol offered by the client.
pub(super) fn extract_ws_subprotocol(req: &Request) -> Option<&str> {
    req.headers()
        .filter(|(name, _)| name.eq_ignore_ascii_case("sec-websocket-protocol"))
        .flat_map(|(_, value)| value.split(','))
        .map(str::trim)
        .find(|protocol| is_http_token(protocol))
}

pub(super) fn is_http_token(value: &str) -> bool {
    !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(
                    byte,
                    b'!' | b'#'
                        | b'$'
                        | b'%'
                        | b'&'
                        | b'\''
                        | b'*'
                        | b'+'
                        | b'-'
                        | b'.'
                        | b'^'
                        | b'_'
                        | b'`'
                        | b'|'
                        | b'~'
                )
        })
}

pub(super) fn ws_handshake_rejection(error: WsHandshakeError) -> Rejected {
    match error {
        WsHandshakeError::BadRequest => Rejected::ws_bad_handshake(),
        WsHandshakeError::UnsupportedVersion => Rejected::ws_unsupported_version(),
    }
}

/// Build the `101` a validated handshake earns.
///
/// `Err` is a builder failure — unreachable while the accept key is derived
/// base64 and the subprotocol is token-validated, but a response that is not a
/// `101` must never be handed back as one: the caller would register a bridge
/// and resolve the handoff for an upgrade Hyper will never perform.
///
/// The builder's own error travels with the failure rather than being logged
/// here. It is the only account of what could not be represented, and it
/// belongs in the refusal record that already names the request, the route and
/// the subprotocol.
pub(super) fn ws_switching_protocols(
    accept_key: &str,
    subprotocol: Option<&str>,
) -> Result<hyper::Response<HyperResponseBody>, hyper::http::Error> {
    let mut builder = hyper::Response::builder()
        .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
        .header("Upgrade", "websocket")
        .header("Connection", "Upgrade")
        .header("Sec-WebSocket-Accept", accept_key);

    if let Some(proto) = subprotocol {
        builder = builder.header("Sec-WebSocket-Protocol", proto);
    }

    builder.body(HyperResponseBody::Full(http_body_util::Full::new(
        bytes::Bytes::new(),
    )))
}
