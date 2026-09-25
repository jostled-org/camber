//! Whether a request is a WebSocket handshake, and what a valid one offers.
//!
//! Header validation, the offer it produces, and the `101` itself. Nothing here
//! owns a transport or a lifecycle: this file either produces the offer both
//! bridges start from, or the refusal the peer gets instead. The Origin policy
//! is a separate question and lives beside this one; an offer becomes usable
//! only once that policy has admitted it.

use super::super::Request;
use super::super::body::HyperResponseBody;
use super::super::rejection::Rejected;
use super::super::util::is_token;
use super::origin::check_ws_origin;

/// What a request head earned: a validated offer, or the refusal it is owed.
pub(in crate::http) enum WsUpgrade {
    Offered(WsHandshakeOffer),
    Rejected(WsHandshakeError),
}

pub(in crate::http) enum WsHandshakeError {
    BadRequest,
    UnsupportedVersion,
}

/// One validated client offer.
///
/// Owns the Hyper upgrade future, the accept key derived from the client's key,
/// and the subprotocols the client offered, in the order it offered them. An
/// offer is not a selection: which protocol, if any, the `101` names is decided
/// by the bridge that answers it.
pub(in crate::http) struct WsHandshakeOffer {
    on_upgrade: hyper::upgrade::OnUpgrade,
    accept_key: Box<str>,
    protocols: WsProtocolOffers,
}

impl WsHandshakeOffer {
    /// The protocols the client offered.
    pub(super) const fn protocols(&self) -> &WsProtocolOffers {
        &self.protocols
    }

    /// Build the `101` this offer earns, naming `selection` as its protocol.
    ///
    /// `Err` is a builder failure — unreachable while the accept key is derived
    /// base64 and the protocol is token-validated, but a response that is not a
    /// `101` must never be handed back as one: the caller would register a
    /// bridge and resolve the handoff for an upgrade Hyper will never perform.
    ///
    /// The builder's own error travels with the failure rather than being
    /// logged here. It is the only account of what could not be represented,
    /// and it belongs in the refusal record that already names the request, the
    /// route and the protocol.
    pub(super) fn switching_protocols(
        &self,
        selection: WsSelection,
    ) -> Result<hyper::Response<HyperResponseBody>, hyper::http::Error> {
        let builder = hyper::Response::builder()
            .status(hyper::StatusCode::SWITCHING_PROTOCOLS)
            .header("Upgrade", "websocket")
            .header("Connection", "Upgrade")
            .header("Sec-WebSocket-Accept", self.accept_key.as_ref());
        let builder = match self.protocols.named(selection) {
            Some(protocol) => builder.header("Sec-WebSocket-Protocol", protocol),
            None => builder,
        };
        builder.body(HyperResponseBody::Full(http_body_util::Full::new(
            bytes::Bytes::new(),
        )))
    }

    /// Give up the upgrade future, keeping what the client offered.
    pub(super) fn into_transfer(self) -> (hyper::upgrade::OnUpgrade, WsProtocolOffers) {
        (self.on_upgrade, self.protocols)
    }
}

/// The subprotocols one client offered, in order, each a valid token.
///
/// Boxed once at validation and never grown: the offer is what the client said,
/// and nothing after the head adds to it.
pub(super) struct WsProtocolOffers(Box<[Box<str>]>);

impl WsProtocolOffers {
    /// The direct selection policy: the first offer, or none.
    pub(super) fn first(&self) -> WsSelection {
        WsSelection((!self.0.is_empty()).then_some(0))
    }

    /// The offer `token` names exactly, if the client made one.
    ///
    /// The proxy selection policy: only the backend selects, and only from what
    /// the client offered. A list, a malformed token, or a protocol nobody
    /// offered names no offer, so it selects nothing here.
    pub(super) fn select(&self, token: &str) -> Option<WsSelection> {
        self.0
            .iter()
            .position(|offered| **offered == *token)
            .map(|index| WsSelection(Some(index)))
    }

    /// The protocol one selection names, if it names one.
    pub(super) fn named(&self, selection: WsSelection) -> Option<&str> {
        selection
            .0
            .and_then(|index| self.0.get(index))
            .map(AsRef::as_ref)
    }

    /// Offers stated directly rather than read from a head, each a token.
    ///
    /// The test adapter's way in: the same token rule a head's offers pass.
    pub(super) fn from_tokens(tokens: &[&str]) -> Option<Self> {
        tokens
            .iter()
            .map(|token| offered_token(token))
            .collect::<Option<_>>()
            .map(Self)
    }

    /// Every offer, in the order the client made them, as the one
    /// comma-separated field a backend is sent; `None` when nothing was offered.
    ///
    /// A `String` because that is what a header value is built from.
    pub(super) fn joined(&self) -> Option<String> {
        (!self.0.is_empty()).then(|| self.0.join(", "))
    }
}

/// Which offered protocol, if any, a `101` names.
///
/// An index into the offers rather than a token, so a selection can only ever
/// name something the client offered: the `101` echoes the client's own
/// token, and a refusal reports it, without either holding a second copy.
#[derive(Clone, Copy)]
pub(super) struct WsSelection(Option<usize>);

impl WsSelection {
    /// A `101` that names no protocol.
    pub(super) const NONE: Self = Self(None);
}

impl WsUpgrade {
    /// Admit this upgrade under the request's Origin.
    ///
    /// The only way to a usable offer. The Origin refusal outranks every head
    /// refusal, so a cross-origin handshake is told `403` whatever else is
    /// wrong with it.
    pub(super) fn admit(self, req: &Request) -> Result<WsHandshakeOffer, Rejected> {
        match (check_ws_origin(req), self) {
            (Some(rejected), _) => Err(rejected),
            (None, Self::Offered(offer)) => Ok(offer),
            (None, Self::Rejected(error)) => Err(ws_handshake_rejection(error)),
        }
    }
}

/// Validate the handshake head and take its upgrade future before the request
/// is consumed.
pub(in crate::http) fn extract_ws_upgrade(
    req: &mut hyper::Request<hyper::body::Incoming>,
) -> WsUpgrade {
    let (accept_key, protocols) = match validate_ws_handshake(req) {
        Ok((key, protocols)) => (
            tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes()),
            protocols,
        ),
        Err(error) => return WsUpgrade::Rejected(error),
    };
    WsUpgrade::Offered(WsHandshakeOffer {
        on_upgrade: hyper::upgrade::on(req),
        accept_key: accept_key.into(),
        protocols,
    })
}

/// The key and the ordered offers of a head that passed every field rule.
fn validate_ws_handshake(
    request: &hyper::Request<hyper::body::Incoming>,
) -> Result<(&hyper::header::HeaderValue, WsProtocolOffers), WsHandshakeError> {
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
    validate_bodyless_handshake(headers)?;
    validate_ws_version(headers)?;
    let protocols = ws_protocol_offers(headers)?;

    let key = match single_header(headers, "sec-websocket-key") {
        Some(key) if valid_ws_key(key.as_bytes()) => key,
        _ => return Err(WsHandshakeError::BadRequest),
    };
    Ok((key, protocols))
}

/// Refuse a handshake that declares a payload.
///
/// A `101` hands the transport to the bridge, which leaves those declared bytes
/// unframed. HTTP/1 cannot both honour that framing and give the connection
/// away, so Hyper marks such a response to close — and RFC 6455 §4.1 requires a
/// conforming client to fail a handshake whose reply does not say `Connection:
/// Upgrade`. Refusing at the head is the only answer that is not an
/// unusable `101`: a handshake carries no payload, so nothing legitimate is
/// turned away, and the route's body policy is still never asked.
///
/// `Content-Length: 0` declares no payload and is not a refusal. A repeated
/// length that disagrees with itself never reaches here — Hyper answers that
/// from the head.
fn validate_bodyless_handshake(headers: &hyper::HeaderMap) -> Result<(), WsHandshakeError> {
    let declares_payload = headers.contains_key(hyper::header::TRANSFER_ENCODING)
        || headers
            .get_all(hyper::header::CONTENT_LENGTH)
            .iter()
            .any(|length| length != "0");
    match declares_payload {
        true => Err(WsHandshakeError::BadRequest),
        false => Ok(()),
    }
}

/// Every offered subprotocol, in field order and then list order.
///
/// One invalid element refuses the whole offer: a list the client cannot
/// state cleanly is not one a bridge may select from.
fn ws_protocol_offers(headers: &hyper::HeaderMap) -> Result<WsProtocolOffers, WsHandshakeError> {
    headers
        .get_all("sec-websocket-protocol")
        .iter()
        .try_fold(Vec::new(), |offers, value| {
            let field = value.to_str().map_err(|_| WsHandshakeError::BadRequest)?;
            field.split(',').map(str::trim).try_fold(offers, push_offer)
        })
        .map(|offers| WsProtocolOffers(offers.into_boxed_slice()))
}

/// Append one offered element, or refuse the offer it is not a token of.
fn push_offer(mut offers: Vec<Box<str>>, token: &str) -> Result<Vec<Box<str>>, WsHandshakeError> {
    offers.push(offered_token(token).ok_or(WsHandshakeError::BadRequest)?);
    Ok(offers)
}

/// One offered protocol, owned, if it is a valid token.
fn offered_token(token: &str) -> Option<Box<str>> {
    is_token(token.as_bytes()).then(|| Box::from(token))
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

pub(super) fn single_header<'a>(
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

pub(super) fn header_contains_token(
    headers: &hyper::HeaderMap,
    name: &'static str,
    expected: &str,
) -> bool {
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
                    is_token(token.as_bytes())
                        .then_some(seen || token.eq_ignore_ascii_case(expected))
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

fn ws_handshake_rejection(error: WsHandshakeError) -> Rejected {
    match error {
        WsHandshakeError::BadRequest => Rejected::ws_bad_handshake(),
        WsHandshakeError::UnsupportedVersion => Rejected::ws_unsupported_version(),
    }
}
