#![cfg(feature = "ws")]

//! One forwarding policy, read on every face a proxy forwards over.
//!
//! A proxy's header perimeter is one rule with five faces: the buffered and
//! streaming requests it sends upstream, the two answers it returns, and the
//! offer a proxied WebSocket makes its backend. Every row here drives a live
//! Camber proxy over TCP and reads what the far side actually received — the
//! upstream's own record of the request it served, or the whole head the
//! scripted backend was offered.
//!
//! No diagnostic in this module prints a header value. Each row sends a
//! credential the proxy must not forward, so [`HeaderInventory`] reports names
//! and only ever compares values, and nothing here derives `Debug`.

use crate::backend_negotiation::{assert_bridged, offer, offers_backend_path};
use crate::buffered_forwarding::{
    FORWARDING_METADATA_LEAK, generated_connection_value, generated_forwarded_field,
    generated_header_case, generated_padding,
};
use crate::common::{
    CLOSE_AFTER_RESPONSE, DeterministicCase, DeterministicGenerator, OwnedServer, append_headers,
    lifecycle_event, read_async_http_head, read_peer_to_eof, status_from_raw,
};
use crate::ws_backend_script::{BackendScript, ScriptedWsBackend, switching_without_selection};
use camber::http::{Request, Response, Router};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, MutexGuard};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

// ── What every row shares ──────────────────────────────────────────

/// The prefix the buffered proxy route answers under.
const BUFFERED_PREFIX: &str = "/buffered";

/// The prefix the streaming proxy route answers under.
const STREAMING_PREFIX: &str = "/streaming";

/// The prefix the proxied WebSocket route answers under.
const WS_PREFIX: &str = "/ws";

/// The upstream path an ordinary proxied request names once its prefix is
/// stripped.
const UPSTREAM_PATH: &str = "/headers";

/// The body the recording upstream answers every ordinary request with.
const UPSTREAM_BODY: &str = "ok";

/// The authority every ordinary peer addresses this proxy by.
///
/// Not `localhost`: `X-Forwarded-Host` has to carry a value that could only
/// have come from the peer's own `Host` line, so an upstream reading Camber's
/// authoritative metadata is reading something a default could not supply.
const DOWNSTREAM_HOST: &str = "client.example";

/// The two ordinary proxy faces, named as a row reports them.
const ORDINARY_PATHS: [(&str, &str); 2] = [
    ("buffered", BUFFERED_PREFIX),
    ("streaming", STREAMING_PREFIX),
];

/// The credential an `Authorization` field carries, and no named hop may read.
///
/// Opaque and fixed, and never printed: every diagnostic below reports header
/// names and compares values.
const BEARER: &str = "Bearer proxy-forwarding-perimeter-token";

/// The credential a `Cookie` field carries, under the same rule.
const SESSION: &str = "session=proxy-forwarding-perimeter-session";

/// Live rows each generated family sends.
///
/// The cap this plan sets for a live handshake, header, or frame family. Every
/// family below runs exactly it, so one seed and one index locate any row.
const CASES_PER_FAMILY: u64 = 24;

const ORDINARY_REQUEST_SEED: u64 = 0x4850_5245_5155_0801;
const ORDINARY_ANSWER_SEED: u64 = 0x4850_414e_5357_0802;
const WEBSOCKET_OFFER_SEED: u64 = 0x4850_5753_4f46_0803;
const METADATA_PREFIX_SEED: u64 = 0x4850_4d45_5441_0804;

// ── The diagnostics ────────────────────────────────────────────────

/// The diagnostic a field the peer named through `Connection` fails with when
/// it reaches a WebSocket backend.
pub(crate) const CONNECTION_NAMED_LEAK: &str = "connection-named field reached websocket backend";

/// What a field an ordinary peer's `Connection` named fails with.
///
/// Not a declared red: buffered and streaming forwarding already scan every
/// `Connection` value. The rows keep it so a repair to the WebSocket path
/// cannot regress the two faces that were already correct.
const ORDINARY_CONNECTION_NAMED: &str = "a field the peer's Connection named reached upstream";

/// What a field an upstream's own `Connection` named fails with downstream.
const ANSWERED_CONNECTION_NAMED: &str = "a field the upstream's Connection named reached the peer";

/// What a fixed hop-by-hop field that crossed a hop fails with.
const HOP_BY_HOP_TRAVELLED: &str = "a fixed hop-by-hop field travelled";

/// What a spoofed forwarding field Camber already replaces or drops fails
/// with.
const SPOOFED_METADATA_TRAVELLED: &str = "a spoofed forwarding field reached upstream";

/// What forwarding metadata on an answer fails with.
///
/// Forwarding metadata describes a request's peer, so an answer carrying any
/// is carrying one the proxy invented on a path where the vocabulary has no
/// meaning.
const ANSWER_ACQUIRED_METADATA: &str = "an answer acquired forwarding metadata";

// ── The generated material ─────────────────────────────────────────

/// Fragments no `Connection` value may spell as a token.
///
/// Each sits beside valid tokens in the same value or in a sibling field. A
/// policy that gives up on the whole list when one fragment will not parse
/// forwards the hop fields the valid tokens named.
const INVALID_CONNECTION_FRAGMENTS: [&str; 4] =
    ["invalid token", "(close)", "keep alive", "x-real-ip;q=1"];

/// The forwarding metadata every peer in this module spoofs.
///
/// Camber replaces the first four with what it knows about the peer and drops
/// the last, so no value here may ever reach an upstream as the client sent it.
const SPOOFED_METADATA: [(&str, &str); 5] = [
    ("X-Forwarded-For", "203.0.113.7"),
    ("X-Forwarded-Host", "spoofed.example"),
    ("X-Forwarded-Proto", "https"),
    ("X-Real-IP", "198.51.100.8"),
    ("Forwarded", "for=192.0.2.9;proto=https"),
];

/// What Camber's own forwarding metadata states about a local peer.
///
/// An ordinary upstream reads exactly these, once each: the peer's address as
/// the transport reported it, the authority the peer addressed, and the scheme
/// the request arrived over. A proxied handshake carries none of them, so a
/// WebSocket row asserts their absence instead.
const AUTHORITATIVE_METADATA: [(&str, &str); 4] = [
    ("x-forwarded-for", "127.0.0.1"),
    ("x-forwarded-host", DOWNSTREAM_HOST),
    ("x-forwarded-proto", "http"),
    ("x-real-ip", "127.0.0.1"),
];

/// The fixed hop-by-hop names an upstream request is read for.
///
/// `Transfer-Encoding` is left out. The framing of the request this proxy
/// writes belongs to the client that writes it — a streaming forward chunks a
/// body whose length it does not know — and no peer here sends the field, so
/// its absence would prove nothing about forwarding. `Connection` is in:
/// every row sends one, and none may cross.
const FORWARDED_HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
];

/// The fixed hop-by-hop names a downstream answer is read for.
///
/// `Connection` and `Transfer-Encoding` are left out on purpose: both are the
/// downstream transport's own framing, written by the server that answers the
/// peer, and a row forbidding them would be asserting about Hyper rather than
/// about what the upstream sent.
const ANSWERED_HOP_HEADERS: [&str; 7] = [
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
];

/// The fixed hop-by-hop fields a proxied handshake's peer offers, and no
/// backend may read.
///
/// `Connection` and `Upgrade` are not here. This proxy writes its own of each
/// for the offer it makes, and [`assert_generated_handshake`] already pins both
/// to exactly one generated value; a second `Upgrade` from the peer would name
/// a protocol the handshake is not for, and the row would be refused for a
/// reason it is not about. `Transfer-Encoding` is out for the reason
/// [`FORWARDED_HOP_HEADERS`] gives.
///
/// What remains is every fixed hop name a peer can spell that a handshake has
/// no use for. They stop today only because the WebSocket rule is a positive
/// allowlist, so nothing else in this repository would notice that allowlist
/// widening by one entry and carrying `Proxy-Authorization` to a backend. Sent
/// rather than only asserted, so each row states both halves of one fact.
const OFFERED_HOP_VALUES: [(&str, &str); 6] = [
    ("Keep-Alive", "timeout=5"),
    ("Proxy-Authenticate", "Basic realm=downstream"),
    ("Proxy-Authorization", "Basic ZG93bnN0cmVhbQ=="),
    ("Proxy-Connection", "keep-alive"),
    ("TE", "trailers"),
    ("Trailer", "X-Checksum"),
];

/// The hop-by-hop fields an upstream answers with that its peer must not read.
///
/// Sent as an answer rather than only asserted, so each row states both halves
/// of one fact: the upstream wrote them, and the peer never saw them.
const ANSWERED_HOP_VALUES: [(&str, &str); 7] = [
    ("Keep-Alive", "timeout=5"),
    ("Proxy-Authenticate", "Basic realm=upstream"),
    ("Proxy-Authorization", "Basic dXBzdHJlYW0="),
    ("Proxy-Connection", "keep-alive"),
    ("TE", "trailers"),
    ("Trailer", "X-Checksum"),
    ("Upgrade", "h2c"),
];

/// `Connection` values a proxied handshake must be refused at ingress for.
///
/// Every one carries a valid `Upgrade` token beside a fragment that is not a
/// token at all. A handshake is held to a stricter rule than ordinary
/// forwarding: the list cannot be read, so the offer is refused outright
/// rather than sanitized, and no backend is ever reached.
const MALFORMED_WS_CONNECTIONS: [&str; 4] = [
    "Upgrade, bad token",
    "Upgrade,,keep-alive",
    "(Upgrade)",
    "Upgrade, x-hop;q=1",
];

/// The status Camber answers a handshake whose head it read and refused.
const REFUSED_HANDSHAKE_STATUS: u16 = 400;

// ── What one message carried ───────────────────────────────────────

/// Every header one message carried, in wire order.
///
/// The pairs are private and nothing here derives `Debug`. Every row in this
/// module sends a credential the proxy must not forward, so a diagnostic that
/// could print a value would put that credential in the test log. Names are
/// reported; values are only ever compared.
struct HeaderInventory {
    pairs: Box<[(Box<str>, Box<str>)]>,
}

impl HeaderInventory {
    /// The inventory of a head still in its wire form, start line and all.
    fn from_head(head: &str) -> Self {
        let pairs = head
            .split("\r\n")
            .skip(1)
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (Box::from(name.trim()), Box::from(value.trim())))
            .collect();
        Self { pairs }
    }

    /// The inventory of a request a Camber upstream already collected.
    fn from_request(request: &Request) -> Self {
        let pairs = request
            .headers()
            .map(|(name, value)| (Box::from(name), Box::from(value)))
            .collect();
        Self { pairs }
    }

    /// Every header name, in wire order — the only part a diagnostic carries.
    fn names(&self) -> Box<[&str]> {
        self.pairs.iter().map(|(name, _)| name.as_ref()).collect()
    }

    /// Every value one name carries, in wire order.
    fn values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.pairs
            .iter()
            .filter(move |(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_ref())
    }
}

/// Require that `name` reached nothing, under the diagnostic it would break.
fn assert_absent(inventory: &HeaderInventory, name: &str, diagnostic: &str, label: &str) {
    assert!(
        inventory.values(name).next().is_none(),
        "{diagnostic} ({label}): {name} arrived among {:?}",
        inventory.names()
    );
}

/// Require that `name` arrived exactly once, carrying `value`.
fn assert_once(inventory: &HeaderInventory, name: &str, value: &str, label: &str) {
    assert_eq!(
        *inventory.values(name).collect::<Box<[&str]>>(),
        [value],
        "{label}: {name} must arrive once with the value its sender set, among {:?}",
        inventory.names()
    );
}

/// Require that every name in `names` reached nothing.
fn assert_all_absent(inventory: &HeaderInventory, names: &[&str], diagnostic: &str, label: &str) {
    names
        .iter()
        .for_each(|name| assert_absent(inventory, name, diagnostic, label));
}

/// Require that the handshake fields a backend reads are this proxy's own.
///
/// A generated `Upgrade` and `Connection` are not a leak of the peer's hop
/// fields: this proxy builds its own offer, so the backend reads exactly one
/// of each, naming the upgrade and nothing else.
fn assert_generated_handshake(offered: &HeaderInventory, label: &str) {
    assert_once(offered, "upgrade", "websocket", label);
    assert_once(offered, "connection", "Upgrade", label);
    assert_once(offered, "sec-websocket-version", "13", label);
}

// ── The live servers a row owns ────────────────────────────────────

/// The header lines every answer carries, shared rather than copied per answer.
type ScriptedHeaders = Arc<[(Box<str>, Box<str>)]>;

/// What one ordinary upstream recorded, and what it answers with.
///
/// One owner for both halves of the perimeter. The request an upstream
/// received and the answer it was told to send are the two ends of the same
/// row, and a fixture holding them apart lets a row wire only one of them.
#[derive(Clone)]
struct Upstream {
    received: Arc<Mutex<Vec<HeaderInventory>>>,
    /// Shared with every answer rather than copied into it: a request takes a
    /// handle on the current script, and a new script replaces the handle.
    scripted: Arc<Mutex<ScriptedHeaders>>,
}

impl Upstream {
    fn new() -> Self {
        Self {
            received: Arc::new(Mutex::new(Vec::new())),
            scripted: Arc::new(Mutex::new(Arc::from([]))),
        }
    }

    /// A router that records every request and answers the scripted headers.
    fn router(&self) -> Router {
        let upstream = self.clone();
        let mut router = Router::new();
        router.get(UPSTREAM_PATH, move |request: &Request| {
            upstream.record(request);
            let scripted = upstream.answer_headers();
            async move {
                Response::text(200, UPSTREAM_BODY).map(|answer| {
                    scripted.iter().fold(answer, |answer, (name, value)| {
                        answer.with_header(name, value)
                    })
                })
            }
        });
        router
    }

    fn record(&self, request: &Request) {
        self.locked_received()
            .push(HeaderInventory::from_request(request));
    }

    fn answer_headers(&self) -> ScriptedHeaders {
        Arc::clone(&self.locked_scripted())
    }

    /// Script the headers every following answer carries.
    fn answers_with(&self, headers: &[(&str, &str)]) {
        let scripted = headers
            .iter()
            .map(|(name, value)| (Box::from(*name), Box::from(*value)))
            .collect();
        *self.locked_scripted() = scripted;
    }

    fn locked_scripted(&self) -> MutexGuard<'_, ScriptedHeaders> {
        self.scripted
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// The one request the upstream served since the last read.
    ///
    /// Exactly one: a proxy that retried, or one that sent a probe of its own,
    /// would leave a second record here and fail the row that read the first.
    fn took_one(&self, label: &str) -> HeaderInventory {
        let mut received = self.locked_received();
        assert_eq!(
            received.len(),
            1,
            "{label}: the upstream served {} requests for one row",
            received.len()
        );
        received.pop().expect("one recorded request")
    }

    /// Discard the one request served, for a row that reads only its answer.
    fn forgot_one(&self, label: &str) {
        drop(self.took_one(label));
    }

    fn locked_received(&self) -> MutexGuard<'_, Vec<HeaderInventory>> {
        self.received
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }
}

/// A router forwarding a proxied WebSocket to `backend`.
fn websocket_router(backend: &str) -> Router {
    let mut router = Router::new();
    router.proxy(WS_PREFIX, backend);
    router
}

/// A router forwarding every face this module reads.
fn perimeter_router(upstream: &str, backend: &str) -> Router {
    let mut router = websocket_router(backend);
    router.proxy(BUFFERED_PREFIX, upstream);
    router.proxy_stream(STREAMING_PREFIX, upstream);
    router
}

/// Every live server a generated perimeter case reads through.
///
/// The backend is scripted for one family of handshakes, one per case.
struct Perimeter {
    backend: ScriptedWsBackend,
    upstream: Upstream,
    served: OwnedServer,
    proxy: OwnedServer,
}

impl Perimeter {
    async fn start(label: &str) -> Self {
        let script = (0..CASES_PER_FAMILY)
            .map(|_| BackendScript::Upgrade(switching_without_selection))
            .collect();
        let backend = ScriptedWsBackend::bind(script).await;
        let upstream = Upstream::new();
        let served = OwnedServer::bind(upstream.router(), label).await;
        let proxy =
            OwnedServer::bind(perimeter_router(&served.url(), &backend.http_url()), label).await;
        Self {
            backend,
            upstream,
            served,
            proxy,
        }
    }

    /// Stop the proxy, then its upstream, then require the backend saw nothing
    /// no case consumed.
    async fn stop(self, label: &str) {
        self.proxy.stop_cleanly(label).await;
        self.served.stop_cleanly(label).await;
        self.backend.finish(label).await;
    }
}

// ── The wire ───────────────────────────────────────────────────────

/// Send one ordinary proxied request and read the head of its answer.
///
/// The peer asks to close, so the answer ends on its own and the row's whole
/// observation is complete before the next row opens a connection.
async fn ordinary_exchange(
    addr: SocketAddr,
    prefix: &str,
    headers: &[(&str, &str)],
    label: &str,
) -> HeaderInventory {
    let mut peer = lifecycle_event(label, TcpStream::connect(addr))
        .await
        .unwrap_or_else(|error| panic!("{label}: connecting the downstream peer failed: {error}"));
    let mut request = format!("GET {prefix}{UPSTREAM_PATH} HTTP/1.1\r\n");
    append_headers(&mut request, headers.iter().copied());
    request.push_str("\r\n");
    lifecycle_event(label, peer.write_all(request.as_bytes()))
        .await
        .unwrap_or_else(|error| panic!("{label}: writing the downstream request failed: {error}"));
    let answer = read_peer_to_eof(&mut peer, label).await;
    let head = answer
        .split_once("\r\n\r\n")
        .map(|(head, _)| head)
        .unwrap_or_else(|| panic!("{label}: the proxied answer carried no head"));
    assert_eq!(
        status_from_raw(head),
        200,
        "{label}: the proxied request did not reach its upstream"
    );
    HeaderInventory::from_head(head)
}

/// Take row `row`'s offered head as an inventory, reporting only names.
///
/// Every row here offers a credential the proxy must not forward, so the wait
/// is the one that never prints a head.
async fn offered_inventory(
    backend: &mut ScriptedWsBackend,
    row: usize,
    label: &str,
) -> HeaderInventory {
    let head = backend.offered_unprinted(row, label).await;
    let inventory = HeaderInventory::from_head(&head);
    assert!(
        offers_backend_path(&head),
        "{label}: the backend was offered a head naming {:?}",
        inventory.names()
    );
    inventory
}

/// Offer one proxied handshake, require the `101`, and take what the backend
/// was offered.
///
/// The backend's offer is read before the downstream head: the offer is what
/// every row here is about, and a peer that never switched would otherwise
/// fail on the status and never name the field that leaked.
async fn websocket_exchange(
    proxy: &OwnedServer,
    backend: &mut ScriptedWsBackend,
    row: usize,
    headers: &[(&str, &str)],
    label: &str,
) -> HeaderInventory {
    let mut peer = offer(proxy.addr(), WS_PREFIX, headers).await;
    let offered = offered_inventory(backend, row, label).await;
    let head = read_async_http_head(&mut peer, "the proxied downstream head").await;
    assert_eq!(
        status_from_raw(&head),
        101,
        "{label}: the proxied upgrade did not switch: {head}"
    );
    assert_bridged(&mut peer, backend, row, label).await;
    drop(peer);
    offered
}

// ── Generation ─────────────────────────────────────────────────────

/// Push the `Connection` lines one case spells `tokens` across.
///
/// One line or two, each list separated and padded as the case chose and each
/// field name cased by it. A policy that reads `Connection` in one spelling,
/// or reads only the first of them, fails here.
fn push_connection_lines(
    headers: &mut Vec<(Box<str>, Box<str>)>,
    tokens: &[&str],
    case: &mut DeterministicCase,
) {
    let split = match case.boolean() {
        true => tokens.len(),
        false => 1 + case.below(tokens.len() - 1),
    };
    for group in [&tokens[..split], &tokens[split..]] {
        if group.is_empty() {
            continue;
        }
        let name = generated_header_case("connection", case);
        let value = generated_padding(&generated_connection_value(group, case), case);
        push(headers, &name, &value);
    }
}

/// One field a generated row requires to stop at this hop, and the diagnostic
/// its arrival fails under.
///
/// The diagnostic travels with the field because one row carries both kinds:
/// a hop field an ordinary proxy already strips is a retained control, and the
/// forwarding suffix beside it is the declared failure.
struct Stopped {
    name: Box<str>,
    diagnostic: &'static str,
}

impl Stopped {
    fn new(name: &str, diagnostic: &'static str) -> Self {
        Self {
            name: Box::from(name),
            diagnostic,
        }
    }
}

/// One generated row: the lines it sends, and what it must leave behind.
struct Generated {
    /// Every header line the row sends, in wire order.
    headers: Box<[(Box<str>, Box<str>)]>,
    /// Every field that must stop at this hop.
    stopped: Box<[Stopped]>,
    /// The end-to-end field whose value must survive unchanged.
    control: (Box<str>, Box<str>),
    /// Seed, case index, family, and the non-secret category.
    label: Box<str>,
}

impl Generated {
    /// The lines as the borrowed pairs a sender takes.
    fn lines(&self) -> Box<[(&str, &str)]> {
        self.borrowed().collect()
    }

    /// The lines borrowed one at a time, for a caller that filters them first.
    fn borrowed(&self) -> impl Iterator<Item = (&str, &str)> {
        self.headers
            .iter()
            .map(|(name, value)| (name.as_ref(), value.as_ref()))
    }
}

/// Require that every field this row named stopped, and its control did not.
fn assert_stopped(inventory: &HeaderInventory, generated: &Generated, label: &str) {
    generated.stopped.iter().for_each(|stopped| {
        assert_absent(inventory, &stopped.name, stopped.diagnostic, label);
    });
    let (name, value) = &generated.control;
    assert_once(inventory, name, value, label);
}

/// The backend accept index generated case `index` is served at.
///
/// Every family scripts one handshake per case, in case order.
fn accept_index(index: u64) -> usize {
    usize::try_from(index).expect("a case index fits an accept index")
}

/// Push one owned header line.
fn push(headers: &mut Vec<(Box<str>, Box<str>)>, name: &str, value: &str) {
    headers.push((Box::from(name), Box::from(value)));
}

/// One `X-Forwarded-` field this case spells, beyond the three Camber knows.
fn spoofed_forwarded_field(index: u64, case: &mut DeterministicCase) -> (Box<str>, Box<str>) {
    (
        generated_forwarded_field(case).into_boxed_str(),
        format!("spoofed-{index}").into_boxed_str(),
    )
}

/// One ordinary proxied request: valid hop tokens beside an invalid fragment,
/// every fixed hop field, every spoofed metadata field, and one control.
fn ordinary_request_case(index: u64, generator: &DeterministicGenerator) -> Generated {
    let mut case = generator.case(index);
    let first = generated_header_case("x-generated-hop-one", &mut case);
    let second = generated_header_case("x-generated-hop-two", &mut case);
    let fragment = *case.pick(&INVALID_CONNECTION_FRAGMENTS);
    let tokens = [
        CLOSE_AFTER_RESPONSE,
        first.as_str(),
        fragment,
        second.as_str(),
    ];

    let mut headers = Vec::new();
    push(&mut headers, "Host", DOWNSTREAM_HOST);
    push_connection_lines(&mut headers, &tokens, &mut case);
    // The field names are cased again, independently of the tokens that named
    // them: a policy matching them by bytes rather than case fails here.
    let first_field = generated_header_case("x-generated-hop-one", &mut case);
    let second_field = generated_header_case("x-generated-hop-two", &mut case);
    push(&mut headers, &first_field, "first-hop-only");
    push(&mut headers, &second_field, "second-hop-only");
    // Every fixed hop field but `Connection`, which the generated lines above
    // already spell.
    FORWARDED_HOP_HEADERS
        .iter()
        .filter(|name| !name.eq_ignore_ascii_case("connection"))
        .for_each(|name| push(&mut headers, name, "hop-only"));
    SPOOFED_METADATA
        .iter()
        .for_each(|(name, value)| push(&mut headers, name, value));
    let (metadata, metadata_value) = spoofed_forwarded_field(index, &mut case);
    let metadata_leak = Stopped::new(&metadata, FORWARDING_METADATA_LEAK);
    headers.push((metadata, metadata_value));
    let control_value = format!("preserved-{index}");
    push(&mut headers, "X-End-To-End", &control_value);

    let stopped = Box::new([
        Stopped::new(&first, ORDINARY_CONNECTION_NAMED),
        Stopped::new(&second, ORDINARY_CONNECTION_NAMED),
        Stopped::new("forwarded", SPOOFED_METADATA_TRAVELLED),
        metadata_leak,
    ]);
    Generated {
        headers: headers.into_boxed_slice(),
        stopped,
        control: (Box::from("x-end-to-end"), control_value.into_boxed_str()),
        label: format!(
            "{case} family=ordinary request category=valid tokens beside an invalid fragment"
        )
        .into_boxed_str(),
    }
}

/// One upstream answer: hop fields the peer must not read, fields the
/// upstream's own `Connection` names, and one control that must survive.
fn ordinary_answer_case(index: u64, generator: &DeterministicGenerator) -> Generated {
    let mut case = generator.case(index);
    let first = generated_header_case("x-answered-hop-one", &mut case);
    let second = generated_header_case("x-answered-hop-two", &mut case);
    let fragment = *case.pick(&INVALID_CONNECTION_FRAGMENTS);
    let tokens = [first.as_str(), fragment, second.as_str()];

    let mut headers = Vec::new();
    push_connection_lines(&mut headers, &tokens, &mut case);
    let first_field = generated_header_case("x-answered-hop-one", &mut case);
    let second_field = generated_header_case("x-answered-hop-two", &mut case);
    push(&mut headers, &first_field, "first-hop-only");
    push(&mut headers, &second_field, "second-hop-only");
    ANSWERED_HOP_VALUES
        .iter()
        .for_each(|(name, value)| push(&mut headers, name, value));
    let control_value = format!("answered-{index}");
    push(&mut headers, "X-End-To-End", &control_value);

    let stopped = Box::new([
        Stopped::new(&first, ANSWERED_CONNECTION_NAMED),
        Stopped::new(&second, ANSWERED_CONNECTION_NAMED),
    ]);
    Generated {
        headers: headers.into_boxed_slice(),
        stopped,
        control: (Box::from("x-end-to-end"), control_value.into_boxed_str()),
        label: format!("{case} family=ordinary answer category=hop fields an upstream answered")
            .into_boxed_str(),
    }
}

/// One proxied handshake: credentials and an allowed field named through
/// repeated, valid `Connection` values, every fixed hop field a handshake has
/// no use for, the spoofed forwarding metadata, and a control that is named by
/// none of them.
fn websocket_offer_case(index: u64, generator: &DeterministicGenerator) -> Generated {
    let mut case = generator.case(index);
    let authorization = generated_header_case("authorization", &mut case);
    let cookie = generated_header_case("cookie", &mut case);
    let hop = generated_header_case("x-generated-ws-hop", &mut case);
    let mut tokens = vec![authorization.as_str(), cookie.as_str(), hop.as_str()];
    // Repetition is a spelling, not a second field: a token listed twice names
    // the same header once.
    if case.boolean() {
        tokens.push(*case.pick(&[authorization.as_str(), hop.as_str()]));
    }

    let mut headers = Vec::new();
    push_connection_lines(&mut headers, &tokens, &mut case);
    push(&mut headers, "Authorization", BEARER);
    push(&mut headers, "Cookie", SESSION);
    let hop_field = generated_header_case("x-generated-ws-hop", &mut case);
    push(&mut headers, &hop_field, "hop-only");
    OFFERED_HOP_VALUES
        .iter()
        .for_each(|(name, value)| push(&mut headers, name, value));
    SPOOFED_METADATA
        .iter()
        .for_each(|(name, value)| push(&mut headers, name, value));
    let (metadata, metadata_value) = spoofed_forwarded_field(index, &mut case);
    let metadata_leak = Stopped::new(&metadata, FORWARDING_METADATA_LEAK);
    headers.push((metadata, metadata_value));
    let control_value = format!("preserved-{index}");
    push(&mut headers, "X-End-To-End", &control_value);

    let mut stopped = vec![
        Stopped::new("authorization", CONNECTION_NAMED_LEAK),
        Stopped::new("cookie", CONNECTION_NAMED_LEAK),
        Stopped::new(&hop, CONNECTION_NAMED_LEAK),
        metadata_leak,
    ];
    stopped.extend(
        OFFERED_HOP_VALUES
            .iter()
            .map(|(name, _)| Stopped::new(name, HOP_BY_HOP_TRAVELLED)),
    );
    stopped.extend(
        SPOOFED_METADATA
            .iter()
            .map(|(name, _)| Stopped::new(name, SPOOFED_METADATA_TRAVELLED)),
    );
    Generated {
        headers: headers.into_boxed_slice(),
        stopped: stopped.into_boxed_slice(),
        control: (Box::from("x-end-to-end"), control_value.into_boxed_str()),
        label: format!("{case} family=websocket offer category=valid Connection-named fields")
            .into_boxed_str(),
    }
}

/// One request carrying forwarding metadata and nothing else: the two fixed
/// suffixes, one generated suffix, the fields Camber replaces, and a control
/// no `Connection` value names.
fn metadata_prefix_case(index: u64, generator: &DeterministicGenerator) -> Generated {
    let mut case = generator.case(index);
    let port = generated_header_case("x-forwarded-port", &mut case);
    let prefix = generated_header_case("x-forwarded-prefix", &mut case);
    let (metadata, metadata_value) = spoofed_forwarded_field(index, &mut case);

    let mut headers = Vec::new();
    push(&mut headers, "Host", DOWNSTREAM_HOST);
    push(&mut headers, "Connection", CLOSE_AFTER_RESPONSE);
    push(&mut headers, &port, "4443");
    push(&mut headers, &prefix, "/spoofed");
    let metadata_leak = Stopped::new(&metadata, FORWARDING_METADATA_LEAK);
    headers.push((metadata, metadata_value));
    SPOOFED_METADATA
        .iter()
        .for_each(|(name, value)| push(&mut headers, name, value));
    let control_value = format!("unrelated-{index}");
    push(&mut headers, "X-Unrelated", &control_value);

    let stopped = Box::new([
        Stopped::new(&port, FORWARDING_METADATA_LEAK),
        Stopped::new(&prefix, FORWARDING_METADATA_LEAK),
        metadata_leak,
        Stopped::new("forwarded", SPOOFED_METADATA_TRAVELLED),
    ]);
    Generated {
        headers: headers.into_boxed_slice(),
        stopped,
        control: (Box::from("x-unrelated"), control_value.into_boxed_str()),
        label: format!(
            "{case} family=forwarding metadata category=peer-supplied X-Forwarded- suffixes"
        )
        .into_boxed_str(),
    }
}

/// The same generated lines, minus the two a handshake supplies itself.
///
/// A peer's `Host` and `Connection` belong to the downstream handshake, which
/// `offer` already spells. Repeating them would send a second `Host` and a
/// `Connection` naming no upgrade, and the row would be refused for a reason
/// it is not about.
fn websocket_lines(generated: &Generated) -> Box<[(&str, &str)]> {
    generated
        .borrowed()
        .filter(|(name, _)| !name.eq_ignore_ascii_case("host"))
        .filter(|(name, _)| !name.eq_ignore_ascii_case("connection"))
        .collect()
}

// ── 8.T1 ───────────────────────────────────────────────────────────

/// 8.T1
///
/// A proxied handshake names `Authorization`, `Cookie`, and an allowed `X-*`
/// field across repeated, valid `Connection` values. Every named field stops
/// at this hop. The one `X-*` field no `Connection` value names travels, so
/// the row separates the policy from a blanket refusal, and the offer the
/// backend reads carries this proxy's own handshake fields and nothing of the
/// peer's.
#[camber::test]
async fn websocket_connection_named_credentials_are_not_forwarded() {
    let label = "credentials named through repeated Connection values";
    let mut backend = ScriptedWsBackend::bind(Box::new([BackendScript::Upgrade(
        switching_without_selection,
    )]))
    .await;
    let proxy = OwnedServer::bind(websocket_router(&backend.http_url()), label).await;

    let offered = websocket_exchange(
        &proxy,
        &mut backend,
        0,
        &[
            ("Connection", "Authorization, X-Hop-Marker"),
            ("cOnNeCtIoN", "\tCookie "),
            ("Authorization", BEARER),
            ("Cookie", SESSION),
            ("X-Hop-Marker", "hop-only"),
            ("X-End-To-End", "preserved"),
        ],
        label,
    )
    .await;

    assert_all_absent(
        &offered,
        &["authorization", "cookie", "x-hop-marker"],
        CONNECTION_NAMED_LEAK,
        label,
    );
    assert_once(&offered, "x-end-to-end", "preserved", label);
    assert_generated_handshake(&offered, label);

    proxy.stop_cleanly(label).await;
    backend.finish(label).await;
}

// ── 8.T2 ───────────────────────────────────────────────────────────

/// 8.T2
///
/// The same policy, generated across every face a proxy forwards over: the
/// buffered and streaming requests, the two answers, and a proxied handshake's
/// offer. Ordinary rows put valid tokens beside fragments that are not tokens,
/// in cased, padded, repeated `Connection` fields; handshake rows keep their
/// tokens valid, because a handshake whose list cannot be read is refused at
/// ingress rather than sanitized. Answers carry the metadata question in
/// reverse: an answer never acquires forwarding fields the upstream did not
/// send.
#[camber::test]
async fn generated_proxy_header_perimeter() {
    let label = "the generated proxy header perimeter";
    let mut perimeter = Perimeter::start(label).await;

    let requests = DeterministicGenerator::new(ORDINARY_REQUEST_SEED);
    for index in 0..CASES_PER_FAMILY {
        let generated = ordinary_request_case(index, &requests);
        let lines = generated.lines();
        perimeter.upstream.answers_with(&[]);
        for (face, prefix) in ORDINARY_PATHS {
            let row = format!("{} path={face}", generated.label);
            let answer = ordinary_exchange(perimeter.proxy.addr(), prefix, &lines, &row).await;
            assert_no_invented_metadata(&answer, &row);
            assert_request_perimeter(&perimeter.upstream.took_one(&row), &generated, &row);
        }
    }

    let answers = DeterministicGenerator::new(ORDINARY_ANSWER_SEED);
    for index in 0..CASES_PER_FAMILY {
        let generated = ordinary_answer_case(index, &answers);
        perimeter.upstream.answers_with(&generated.lines());
        for (face, prefix) in ORDINARY_PATHS {
            let row = format!("{} path={face}", generated.label);
            let sent = [
                ("Host", DOWNSTREAM_HOST),
                ("Connection", CLOSE_AFTER_RESPONSE),
            ];
            let answer = ordinary_exchange(perimeter.proxy.addr(), prefix, &sent, &row).await;
            perimeter.upstream.forgot_one(&row);
            assert_answer_perimeter(&answer, &generated, &row);
        }
    }
    perimeter.upstream.answers_with(&[]);

    let offers = DeterministicGenerator::new(WEBSOCKET_OFFER_SEED);
    for index in 0..CASES_PER_FAMILY {
        let generated = websocket_offer_case(index, &offers);
        let row = &*generated.label;
        let accepted = accept_index(index);
        let offered = websocket_exchange(
            &perimeter.proxy,
            &mut perimeter.backend,
            accepted,
            &generated.lines(),
            row,
        )
        .await;
        assert_stopped(&offered, &generated, row);
        assert_generated_handshake(&offered, row);
    }

    for value in MALFORMED_WS_CONNECTIONS {
        let row = format!("{label}: a handshake Connection list that is not tokens: {value}");
        assert_handshake_refused_at_ingress(&perimeter.proxy, value, &row).await;
    }

    perimeter.stop(label).await;
}

/// Require that an ordinary upstream read no hop field and nothing its peer
/// supplied about forwarding, and that it read Camber's own metadata once.
fn assert_request_perimeter(reached: &HeaderInventory, generated: &Generated, label: &str) {
    assert_all_absent(reached, &FORWARDED_HOP_HEADERS, HOP_BY_HOP_TRAVELLED, label);
    assert_stopped(reached, generated, label);
    assert_authoritative_metadata(reached, label);
}

/// Require that an ordinary upstream read Camber's own metadata, once each.
fn assert_authoritative_metadata(reached: &HeaderInventory, label: &str) {
    AUTHORITATIVE_METADATA
        .iter()
        .for_each(|(name, value)| assert_once(reached, name, value, label));
}

/// Require that a downstream answer carried no hop field, none of the fields
/// the upstream's own `Connection` named, and no metadata at all.
fn assert_answer_perimeter(answer: &HeaderInventory, generated: &Generated, label: &str) {
    assert_all_absent(answer, &ANSWERED_HOP_HEADERS, HOP_BY_HOP_TRAVELLED, label);
    assert_stopped(answer, generated, label);
    assert_no_invented_metadata(answer, label);
}

/// Require that an answer acquired no forwarding metadata at all.
fn assert_no_invented_metadata(answer: &HeaderInventory, label: &str) {
    SPOOFED_METADATA.iter().for_each(|(name, _)| {
        assert_absent(answer, name, ANSWER_ACQUIRED_METADATA, label);
    });
}

/// Require that one handshake is refused before any backend is reached.
///
/// The backend's own fixture proves the second half: an offer it was never
/// meant to read is a report no row consumed, and `finish` fails on it.
async fn assert_handshake_refused_at_ingress(proxy: &OwnedServer, connection: &str, label: &str) {
    let mut peer = offer(proxy.addr(), WS_PREFIX, &[("Connection", connection)]).await;
    let head = read_async_http_head(&mut peer, "the refused handshake head").await;
    assert_ne!(
        status_from_raw(&head),
        101,
        "{label}: an unreadable Connection list reached the sanitizer: {head}"
    );
    assert_eq!(
        status_from_raw(&head),
        REFUSED_HANDSHAKE_STATUS,
        "{label}: the handshake was refused with: {head}"
    );
}

// ── 8.T3 ───────────────────────────────────────────────────────────

/// 8.T3
///
/// `X-Forwarded-` is one family, not three names. A peer sends
/// `X-Forwarded-Port`, `X-Forwarded-Prefix`, and a generated suffix, in
/// generated casing, through the buffered, streaming and proxied-WebSocket
/// request paths. None of them is named in any `Connection` value, so the row
/// turns on metadata classification alone. Every one stops at this hop, the
/// unrelated `X-*` control travels, and Camber's own metadata is what the
/// ordinary upstreams read.
#[camber::test]
async fn forwarding_metadata_prefix_is_stripped_on_every_request_path() {
    let label = "the peer-supplied forwarding-metadata prefix";
    let mut perimeter = Perimeter::start(label).await;

    let generator = DeterministicGenerator::new(METADATA_PREFIX_SEED);
    for index in 0..CASES_PER_FAMILY {
        let generated = metadata_prefix_case(index, &generator);
        let lines = generated.lines();
        for (face, prefix) in ORDINARY_PATHS {
            let row = format!("{} path={face}", generated.label);
            ordinary_exchange(perimeter.proxy.addr(), prefix, &lines, &row).await;
            let reached = perimeter.upstream.took_one(&row);
            assert_stopped(&reached, &generated, &row);
            assert_authoritative_metadata(&reached, &row);
        }

        let row = format!("{} path=websocket", generated.label);
        let accepted = accept_index(index);
        let offered = websocket_exchange(
            &perimeter.proxy,
            &mut perimeter.backend,
            accepted,
            &websocket_lines(&generated),
            &row,
        )
        .await;
        assert_stopped(&offered, &generated, &row);
        // A proxied handshake carries no metadata of Camber's own, so the
        // fields an ordinary upstream reads authoritatively must be absent
        // here rather than replaced.
        AUTHORITATIVE_METADATA.iter().for_each(|(name, _)| {
            assert_absent(&offered, name, SPOOFED_METADATA_TRAVELLED, &row);
        });
        assert_generated_handshake(&offered, &row);
    }

    perimeter.stop(label).await;
}
