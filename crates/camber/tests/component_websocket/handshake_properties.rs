//! Generated inbound handshakes, each answered on a real socket.
//!
//! Every inbound `101` follows validation of the whole offer: method, HTTP
//! version, the upgrade headers, body absence, version, key shape, Origin, and
//! subprotocol token syntax. Each family below varies one part of that offer
//! from a checked-in seed. A valid row must reach the callback and echo; an
//! invalid row must emit no `101` and never enter the callback. A refusal also
//! names its owner: Hyper answers a head it cannot parse, and Camber's rejection
//! mapper answers every head it can.

#![cfg(feature = "ws")]

use crate::common;
use crate::deterministic::{DeterministicCase, Family};
use crate::handshake::{
    LOCAL_HOST, RequestLine, accepted_keyed, assert_handshake_rejected,
    assert_websocket_switch_accepting, complete_server_close, perform_raw_ws_handshake,
    raw_request,
};
use camber::http::{Rejection, RejectionContext, Request, Response, Router, WsConn};
use camber::runtime;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// The route every generated handshake asks for.
const SOCKET: &str = "/ws";

/// The header Camber's rejection mapper stamps on every refusal it answers.
///
/// Hyper never runs the mapper, so its absence is what names Hyper as the
/// owner of a refusal.
const MAPPED_REFUSAL: &str = "x-mapped-refusal";

/// Live rows each family sends against the one ready server.
const CASES_PER_FAMILY: u64 = 24;

const VALID_OFFER_SEED: u64 = 0x5753_4f46_4645_5201;
const HEAD_SEED: u64 = 0x5753_4845_4144_5202;
const VERSION_KEY_SEED: u64 = 0x5753_4b45_5953_5203;
const ORIGIN_PROTOCOL_SEED: u64 = 0x5753_4f52_4947_5204;

/// Base64 symbols whose low four bits are zero: the only ones that may end a
/// key that encodes exactly sixteen bytes.
const KEY_FINAL_SYMBOLS: &[u8] = b"AQgw";

/// Base64 symbols that encode leftover bits, which a sixteen-byte key forbids.
const KEY_UNALIGNED_FINAL_SYMBOLS: &[u8] = b"BRhx/9";

const BASE64_SYMBOLS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Tokens a client may offer. Every RFC 9110 `tchar` appears in one of them.
const PROTOCOL_TOKENS: &[&str] = &[
    "chat",
    "superchat",
    "v2.json",
    "x-proto_1",
    "a!#$%&'*+-.^_`|~z",
];

/// Who refused a handshake.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RefusedBy {
    /// Hyper refused the head before Camber could read it.
    Hyper,
    /// Camber read the head and its mapper answered the refusal.
    Camber,
}

/// What one generated row must earn.
#[derive(Debug, Eq, PartialEq)]
enum Expected {
    /// A `101` for this key, echoing this protocol, then one echoed frame.
    Switch {
        key: Box<str>,
        protocol: Option<Box<str>>,
    },
    /// No `101`, this status, from this owner.
    Refused { status: u16, by: RefusedBy },
}

/// One generated request, stated in full, and what it must earn.
#[derive(Debug, Eq, PartialEq)]
struct Row {
    category: &'static str,
    method: &'static str,
    version: &'static str,
    headers: Vec<(Box<str>, Box<str>)>,
    body: &'static str,
    expected: Expected,
}

impl Row {
    /// The accepted offer for `key`: every other row is a change to it.
    fn accepted(category: &'static str, key: Box<str>) -> Self {
        Self {
            category,
            method: "GET",
            version: "HTTP/1.1",
            headers: accepted_keyed(LOCAL_HOST, &key)
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
            body: "",
            expected: Expected::Switch {
                key,
                protocol: None,
            },
        }
    }

    /// The same offer, refused with `status` by `by`.
    fn refused(mut self, status: u16, by: RefusedBy) -> Self {
        self.expected = Expected::Refused { status, by };
        self
    }

    /// The same offer, required to select `selected`.
    fn selecting(mut self, selected: &str) -> Self {
        if let Expected::Switch { protocol, .. } = &mut self.expected {
            *protocol = Some(selected.into());
        }
        self
    }

    /// Replace every value under `name` with one `value`.
    fn replace(self, name: &'static str, value: impl Into<Box<str>>) -> Self {
        self.without(name).plus(name, value)
    }

    fn without(mut self, name: &'static str) -> Self {
        self.headers.retain(|(candidate, _)| &**candidate != name);
        self
    }

    fn plus(mut self, name: &'static str, value: impl Into<Box<str>>) -> Self {
        self.headers.push((name.into(), value.into()));
        self
    }

    fn request(&self) -> Box<str> {
        raw_request(
            RequestLine {
                method: self.method,
                path: SOCKET,
                version: self.version,
            },
            self.headers.iter().map(|(name, value)| (&**name, &**value)),
            self.body,
        )
    }
}

const FAMILIES: [Family<Row>; 4] = [
    Family {
        name: "valid offers",
        seed: VALID_OFFER_SEED,
        generate: valid_offer,
    },
    Family {
        name: "request line, upgrade headers, and body",
        seed: HEAD_SEED,
        generate: invalid_head,
    },
    Family {
        name: "version and key",
        seed: VERSION_KEY_SEED,
        generate: invalid_version_or_key,
    },
    Family {
        name: "Origin and subprotocol tokens",
        seed: ORIGIN_PROTOCOL_SEED,
        generate: invalid_origin_or_protocol,
    },
];

#[test]
fn generated_websocket_handshake_requires_a_valid_offer() {
    common::test_runtime()
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let entered = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(echo_once_router(&entered));
            let mut switched = 0;
            let mut refused_by = [0_usize; 2];

            FAMILIES.iter().for_each(|family| {
                family.rows(CASES_PER_FAMILY).for_each(|(case, row)| {
                    let context =
                        format!("{case} family={} category={}", family.name, row.category);
                    match assert_row(addr, &row, case.index(), &context) {
                        Some(RefusedBy::Hyper) => refused_by[0] += 1,
                        Some(RefusedBy::Camber) => refused_by[1] += 1,
                        None => switched += 1,
                    }
                    assert_eq!(
                        entered.load(Ordering::Acquire),
                        switched,
                        "{context}: callback entry and the 101 diverged"
                    );
                });
            });

            assert_eq!(
                u64::try_from(switched).expect("a switch count fits"),
                CASES_PER_FAMILY,
                "every valid offer switched"
            );
            assert!(
                refused_by.iter().all(|count| *count > 0),
                "both refusal owners were exercised: {refused_by:?}"
            );
            runtime::request_shutdown();
        })
        .unwrap();
}

/// Send one row and assert what it earned. `None` is a switch.
fn assert_row(addr: SocketAddr, row: &Row, index: u64, context: &str) -> Option<RefusedBy> {
    let (mut stream, head) = perform_raw_ws_handshake(addr, &row.request());
    match &row.expected {
        Expected::Switch { key, protocol } => {
            let accept = tungstenite::handshake::derive_accept_key(key.as_bytes());
            assert_websocket_switch_accepting(&head, &accept, context);
            assert_eq!(
                *head.header_values("sec-websocket-protocol"),
                *protocol.as_deref().as_slice(),
                "{context}: the first offer is the selection: {head:?}"
            );
            let payload = format!("echo {index}");
            common::write_ws_text_frame(&mut stream, &payload);
            assert_eq!(
                &*common::read_ws_text_frame(&mut stream),
                payload,
                "{context}"
            );
            complete_server_close(&mut stream, context);
            None
        }
        Expected::Refused { status, by } => {
            assert_handshake_rejected(&head, *status, context);
            let mapped = !head.header_values(MAPPED_REFUSAL).is_empty();
            assert_eq!(
                mapped,
                *by == RefusedBy::Camber,
                "{context}: expected {by:?} to own the refusal: {head:?}"
            );
            Some(*by)
        }
    }
}

/// One `/ws` route that echoes one message, and a mapper that marks refusals.
fn echo_once_router(entered: &Arc<AtomicUsize>) -> Router {
    let entered = Arc::clone(entered);
    let mut router = Router::new();
    router.ws(SOCKET, move |_request: &Request, mut connection: WsConn| {
        entered.fetch_add(1, Ordering::AcqRel);
        if let Some(message) = connection.recv() {
            connection.send(&message)?;
        }
        Ok(())
    });
    router.rejection_mapper(|rejection: &Rejection, _context: &RejectionContext| {
        Ok(Response::text(rejection.status(), rejection.message())?
            .with_header(MAPPED_REFUSAL, "camber"))
    })
}

fn symbols(case: &mut DeterministicCase, count: usize) -> String {
    (0..count)
        .map(|_| char::from(*case.pick(BASE64_SYMBOLS)))
        .collect()
}

/// A key that encodes exactly sixteen bytes.
fn valid_key(case: &mut DeterministicCase) -> Box<str> {
    padded_key(case, KEY_FINAL_SYMBOLS)
}

/// A padded twenty-four symbol key whose last data symbol is one of `finals`.
fn padded_key(case: &mut DeterministicCase, finals: &[u8]) -> Box<str> {
    let mut key = symbols(case, 21);
    key.push(char::from(*case.pick(finals)));
    key.push_str("==");
    key.into_boxed_str()
}

/// Between one and three offered tokens, spread over one or two fields with
/// optional whitespace. Returns the fields and the first token offered.
fn protocol_offer(case: &mut DeterministicCase) -> (Box<[Box<str>]>, &'static str) {
    let tokens: Box<[&str]> = (0..=case.below(3))
        .map(|_| *case.pick(PROTOCOL_TOKENS))
        .collect();
    let first = tokens[0];
    let split = case.below(tokens.len());
    let (head, tail) = tokens.split_at(split);
    let fields = [head, tail]
        .into_iter()
        .filter(|field| !field.is_empty())
        .map(|field| {
            let separator = *case.pick(&[",", ", ", " ,\t"]);
            let padded = format!("{}{}", *case.pick(&["", " ", "\t"]), field.join(separator));
            padded.into_boxed_str()
        })
        .collect();
    (fields, first)
}

/// The accepted offer for a fresh key, named for the category it becomes.
fn base(category: &'static str, case: &mut DeterministicCase) -> Row {
    let key = valid_key(case);
    Row::accepted(category, key)
}

/// A valid offer, varied over every part the validator reads.
fn valid_offer(_: u64, case: &mut DeterministicCase) -> Row {
    let mut row = base("valid offer", case)
        .replace(
            "Upgrade",
            *case.pick(&["websocket", "WebSocket", "WEBSOCKET"]),
        )
        .replace(
            "Connection",
            *case.pick(&[
                "Upgrade",
                "upgrade",
                "keep-alive, Upgrade",
                "Upgrade, Upgrade",
                "Upgrade\t,keep-alive",
            ]),
        );
    if case.boolean() {
        row = row.plus("Connection", "keep-alive");
    }
    if case.boolean() {
        row = row.plus("Content-Length", "0");
    }
    let origin = *case.pick(&[
        None,
        Some("http://localhost"),
        Some("HTTP://LocalHost:80"),
        Some("https://localhost:443"),
    ]);
    if let Some(origin) = origin {
        row = row.plus("Origin", origin);
    }
    match case.boolean() {
        true => {
            let (fields, first) = protocol_offer(case);
            fields.into_iter().fold(row.selecting(first), |row, field| {
                row.plus("Sec-WebSocket-Protocol", field)
            })
        }
        false => row,
    }
}

/// One refusal category: the rule that builds its row.
type RowRule = fn(&mut DeterministicCase) -> Row;

/// The rule case `index` falls to. The table's length is the modulus, so a
/// category cannot be added without joining the cycle.
fn rule_at(rules: &[RowRule], index: u64) -> RowRule {
    rules[usize::try_from(index).expect("a case index fits") % rules.len()]
}

/// A request line, upgrade header, or body declaration the validator refuses.
fn invalid_head(index: u64, case: &mut DeterministicCase) -> Row {
    rule_at(&INVALID_HEAD, index)(case)
}

const INVALID_HEAD: [RowRule; 14] = [
    |case| Row {
        method: "POST",
        ..base("non-GET method", case).refused(405, RefusedBy::Camber)
    },
    |case| Row {
        method: case.pick(&["PUT", "DELETE", "PATCH"]),
        ..base("non-GET method", case).refused(405, RefusedBy::Camber)
    },
    |case| Row {
        version: "HTTP/1.0",
        ..base("HTTP/1.0", case).refused(400, RefusedBy::Camber)
    },
    |case| Row {
        version: case.pick(&["HTTP/2.0", "HTTP/1.2", "HTTP/3"]),
        ..base("unparseable HTTP version", case).refused(400, RefusedBy::Hyper)
    },
    |case| {
        base("repeated Upgrade", case)
            .plus("Upgrade", "websocket")
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("missing Upgrade", case)
            .without("Upgrade")
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("Upgrade names another protocol", case)
            .replace(
                "Upgrade",
                *case.pick(&["h2c", "websocket2", "websocket, h2c"]),
            )
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("Connection lacks upgrade", case)
            .replace(
                "Connection",
                *case.pick(&["keep-alive", "close", "upgraded"]),
            )
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("missing Connection", case)
            .without("Connection")
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("invalid Connection fragment", case)
            .replace(
                "Connection",
                *case.pick(&["Upgrade, bad token", "Upgrade,,close", "(Upgrade)"]),
            )
            .refused(400, RefusedBy::Camber)
    },
    |case| Row {
        body: "hello",
        ..base("declared payload", case)
            .plus("Content-Length", "5")
            .refused(400, RefusedBy::Camber)
    },
    |case| Row {
        body: "0\r\n\r\n",
        ..base("chunked payload", case)
            .plus("Transfer-Encoding", "chunked")
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("conflicting Content-Length", case)
            .plus("Content-Length", "5")
            .plus("Content-Length", "6")
            .refused(400, RefusedBy::Hyper)
    },
    |case| {
        base("repeated Connection without upgrade", case)
            .replace("Connection", "keep-alive")
            .plus("Connection", "close")
            .refused(400, RefusedBy::Camber)
    },
];

/// A version or key the validator refuses.
fn invalid_version_or_key(index: u64, case: &mut DeterministicCase) -> Row {
    rule_at(&INVALID_VERSION_OR_KEY, index)(case)
}

const INVALID_VERSION_OR_KEY: [RowRule; 12] = [
    |case| {
        base("missing version", case)
            .without("Sec-WebSocket-Version")
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("repeated version", case)
            .plus("Sec-WebSocket-Version", "13")
            .refused(426, RefusedBy::Camber)
    },
    |case| {
        base("unsupported version", case)
            .replace("Sec-WebSocket-Version", *case.pick(&["8", "12", "14", "0"]))
            .refused(426, RefusedBy::Camber)
    },
    |case| {
        base("version list", case)
            .replace(
                "Sec-WebSocket-Version",
                *case.pick(&["13, 8", "8, 13", "13,13"]),
            )
            .refused(426, RefusedBy::Camber)
    },
    |case| {
        base("missing key", case)
            .without("Sec-WebSocket-Key")
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        let second = valid_key(case);
        base("repeated key", case)
            .plus("Sec-WebSocket-Key", second)
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        let mut key = String::from(valid_key(case));
        let position = case.below(21);
        let foreign = char::from(*case.pick(b"-_@*.~"));
        key.replace_range(position..=position, foreign.encode_utf8(&mut [0; 4]));
        base("key outside the Base64 alphabet", case)
            .replace("Sec-WebSocket-Key", key)
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        let key = valid_key(case);
        let unpadded = *case.pick(&["", "=", "==="]);
        let key = format!("{}{unpadded}", key.trim_end_matches('='));
        base("key with wrong padding", case)
            .replace("Sec-WebSocket-Key", key)
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        let key = padded_key(case, KEY_UNALIGNED_FINAL_SYMBOLS);
        base("key with leftover bits", case)
            .replace("Sec-WebSocket-Key", key)
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        let key = *case.pick(&[16_usize, 20, 28, 32]);
        let key = symbols(case, key);
        base("key of the wrong decoded length", case)
            .replace("Sec-WebSocket-Key", key)
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        let listed = format!("{}, {}", valid_key(case), valid_key(case));
        base("key inside a list", case)
            .replace("Sec-WebSocket-Key", listed)
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("empty key", case)
            .replace("Sec-WebSocket-Key", "")
            .refused(400, RefusedBy::Camber)
    },
];

/// An Origin or subprotocol offer the validator refuses.
fn invalid_origin_or_protocol(index: u64, case: &mut DeterministicCase) -> Row {
    rule_at(&INVALID_ORIGIN_OR_PROTOCOL, index)(case)
}

const INVALID_ORIGIN_OR_PROTOCOL: [RowRule; 8] = [
    |case| {
        base("cross-host Origin", case)
            .plus(
                "Origin",
                *case.pick(&[
                    "http://attacker.test",
                    "http://localhost:8080",
                    "http://localhost.attacker.test",
                ]),
            )
            .refused(403, RefusedBy::Camber)
    },
    |case| {
        base("null Origin", case)
            .plus("Origin", "null")
            .refused(403, RefusedBy::Camber)
    },
    |case| {
        base("repeated Origin", case)
            .plus("Origin", "http://localhost")
            .plus("Origin", "http://localhost")
            .refused(403, RefusedBy::Camber)
    },
    |case| {
        base("offer containing whitespace", case)
            .plus(
                "Sec-WebSocket-Protocol",
                *case.pick(&["chat room", "chat, super chat"]),
            )
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("empty list element", case)
            .plus(
                "Sec-WebSocket-Protocol",
                *case.pick(&["chat,,superchat", "chat,", ",chat", ""]),
            )
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("separator inside an offer", case)
            .plus(
                "Sec-WebSocket-Protocol",
                *case.pick(&["chat/1", "a:b", "(x)", "a;b", "\"chat\"", "a=b"]),
            )
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("invalid offer in a later field", case)
            .plus("Sec-WebSocket-Protocol", *case.pick(PROTOCOL_TOKENS))
            .plus("Sec-WebSocket-Protocol", *case.pick(&["x y", "a@b", "[v]"]))
            .refused(400, RefusedBy::Camber)
    },
    |case| {
        base("Origin refusal outranks a malformed offer", case)
            .plus("Origin", "http://attacker.test")
            .plus("Sec-WebSocket-Protocol", "bad token")
            .refused(403, RefusedBy::Camber)
    },
];
