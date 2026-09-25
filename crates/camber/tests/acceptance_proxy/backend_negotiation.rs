#![cfg(feature = "ws")]

//! A proxied WebSocket negotiates with its backend before it answers its peer.
//!
//! Every row drives a live Camber proxy over TCP against a scripted raw
//! backend. The backend reads the proxy's whole offer and answers with the
//! bytes its row scripted — a refusal, a broken head, a selection — so what a
//! row proves about the downstream `101` is what actually crossed both sockets.

use crate::common::{
    COLLAPSED_STATUS, Collapsed, Journal, Observed, OwnedServer, assert_classification,
    assert_transport_eof, collapsing_mapper, journal, lifecycle_event, only, read_async_http_head,
    read_async_ws_frame_or_eof, status_from_raw, write_async_ws_frame, ws_upgrade_request_with,
};
use crate::http::header_values;
use crate::ws_backend_script::{
    BACKEND_GREETING, BackendEvent, BackendScript, ScriptedWsBackend, offered_protocols,
    switching_protocols as switching, switching_without_selection,
};
use camber::http::{RejectionKind, Router, ServerHandle};
use std::net::SocketAddr;
use tokio::net::TcpStream;

/// The suite every observation in this module is recorded under.
const ORIGIN: &str = "backend_negotiation";

/// What a failure to serve the negotiating proxy is reported as.
const PROXY_CONTEXT: &str = "the negotiating proxy";

/// The prefix the buffered proxy route answers under.
pub(crate) const BUFFERED: &str = "/ws";

/// The prefix the streaming proxy route answers under.
const STREAMING: &str = "/stream";

/// The backend path every proxied offer names once its prefix is stripped.
const BACKEND_PATH: &str = "/echo";

/// The text frame a downstream peer sends across a live bridge.
const CLIENT_GREETING: &[u8] = b"from-client";

/// The diagnostic every refused handshake row fails with.
pub(crate) const REFUSAL_REACHED_UPGRADE: &str = "backend refusal reached downstream as an upgrade";

/// The diagnostic an incomplete backend offer fails with.
const INCOMPLETE_OFFER: &str = "backend did not receive the complete ordered offer";

/// The diagnostic a downstream protocol the backend did not select fails with.
const SELECTION_MISMATCH: &str = "downstream protocol did not match backend selection";

/// The classification a backend that refused or broke its handshake keeps.
pub(crate) const BACKEND_REFUSED: Collapsed<'static> = Collapsed {
    kind: RejectionKind::Proxy,
    status: 502,
    message: "bad gateway",
};

// ── The proxy under test ───────────────────────────────────────────

/// One live proxy, the journal its mapper records into, and its owner.
///
/// Built from a router the row configured, so a row about a deadline can add
/// its policy while every row keeps the same mapper and the same teardown.
pub(crate) struct NegotiatingProxy {
    pub(crate) journal: Journal,
    server: OwnedServer,
}

impl NegotiatingProxy {
    pub(crate) async fn serve(router: Router) -> Self {
        let journal = journal();
        let server = OwnedServer::bind(Self::mapped(&journal, router), PROXY_CONTEXT).await;
        Self { journal, server }
    }

    /// Serve `router` on a listener the caller already bound.
    ///
    /// A row that installs a lifecycle controller has to name the address
    /// before the server exists, so it binds its own listener and hands it
    /// over rather than reading the address back afterwards.
    pub(crate) fn serve_on(listener: tokio::net::TcpListener, router: Router) -> Self {
        let journal = journal();
        let server = OwnedServer::serve_on(listener, Self::mapped(&journal, router), PROXY_CONTEXT);
        Self { journal, server }
    }

    /// `router` under the mapper that records into `journal`.
    fn mapped(journal: &Journal, router: Router) -> Router {
        router.rejection_mapper(collapsing_mapper(journal, ORIGIN, COLLAPSED_STATUS))
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.server.addr()
    }

    /// The owner this proxy's server is stopped and joined through.
    pub(crate) fn handle(&self) -> &ServerHandle {
        self.server.handle()
    }

    /// Join the owner, whichever stop the row asked for.
    pub(crate) async fn join(self) -> Result<(), camber::RuntimeError> {
        self.server.join().await
    }

    /// Stop the proxy gracefully and join its owner.
    pub(crate) async fn stop(self) -> Result<(), camber::RuntimeError> {
        self.server.stop().await
    }

    /// Stop the proxy gracefully, require a clean join, and prove its address
    /// is free again.
    ///
    /// Bounded by a Tokio timeout, so a paused-clock row cannot use it.
    async fn stop_cleanly(self) {
        self.server.stop_cleanly("the negotiating proxy join").await;
    }
}

/// A router forwarding both proxy kinds to one backend.
fn both_proxy_kinds(backend: &str) -> Router {
    let mut router = Router::new();
    router.proxy(BUFFERED, backend);
    router.proxy_stream(STREAMING, backend);
    router
}

/// Open a downstream peer and send one offer with `extra` headers.
pub(crate) async fn offer(addr: SocketAddr, prefix: &str, extra: &[(&str, &str)]) -> TcpStream {
    let mut peer = lifecycle_event("the downstream connection", TcpStream::connect(addr))
        .await
        .expect("connect the downstream peer");
    let request = ws_upgrade_request_with(&format!("{prefix}{BACKEND_PATH}"), extra);
    tokio::io::AsyncWriteExt::write_all(&mut peer, request.as_bytes())
        .await
        .expect("write the downstream offer");
    peer
}

/// Assert a downstream head is the configured mapper's refusal and never a
/// `101`, and return the one observation the mapper recorded.
///
/// The refusal names no protocol: nothing the backend refused or broke was a
/// selection the peer could have been told about.
pub(crate) fn assert_mapped_before_upgrade(
    head: &str,
    proxy: &NegotiatingProxy,
    route: &str,
    label: &str,
) -> Observed {
    assert_ne!(
        status_from_raw(head),
        101,
        "{REFUSAL_REACHED_UPGRADE} ({label}): {head}"
    );
    assert_eq!(
        status_from_raw(head),
        COLLAPSED_STATUS,
        "{label}: the configured mapper answered the refusal: {head}"
    );
    let seen = only(&proxy.journal, label);
    assert_eq!(seen.method.as_ref(), "GET", "{label}: method");
    assert_eq!(seen.route.as_deref(), Some(route), "{label}: route");
    assert!(seen.remote.is_some(), "{label}: the transport named a peer");
    assert_eq!(
        seen.subprotocol, None,
        "{label}: a refusal names no protocol the backend did not select"
    );
    seen
}

/// Assert a downstream head is the mapped Proxy refusal a backend that
/// refused or broke its handshake earns.
fn assert_backend_refused(head: &str, proxy: &NegotiatingProxy, route: &str, label: &str) {
    let seen = assert_mapped_before_upgrade(head, proxy, route, label);
    assert_classification(&seen, &BACKEND_REFUSED, label);
}

/// The route pattern one proxy prefix registers.
pub(crate) fn route_of(prefix: &str) -> Box<str> {
    format!("{prefix}/*proxy_path").into_boxed_str()
}

/// Exchange one real frame each way across a committed bridge, then close it
/// from the downstream side and require the backend's release.
pub(crate) async fn assert_bridged(
    peer: &mut TcpStream,
    backend: &mut ScriptedWsBackend,
    row: usize,
    label: &str,
) {
    assert_greetings_exchanged(peer, backend, row, label).await;
    assert_bridge_closed(peer, backend, row, CloseReply::Optional, label).await;
}

/// Whether a bridge the peer closes must answer that close before its
/// transport ends.
#[derive(Clone, Copy)]
pub(crate) enum CloseReply {
    /// End of stream answers the close as well as a close frame does.
    Optional,
    /// The peer must read a close frame back before its transport ends.
    Required,
}

/// Close a committed bridge from the downstream side, require the answer
/// `reply` admits, and require the backend's release.
pub(crate) async fn assert_bridge_closed(
    peer: &mut TcpStream,
    backend: &mut ScriptedWsBackend,
    row: usize,
    reply: CloseReply,
    label: &str,
) {
    write_async_ws_frame(peer, 0x8, &[], "the downstream close").await;
    match (
        reply,
        read_async_ws_frame_or_eof(peer, "the bridged close reply").await,
    ) {
        (_, Some((0x8, _))) => assert_transport_eof(peer, "the bridged transport").await,
        (CloseReply::Optional, None) => {}
        (CloseReply::Required, None) => {
            panic!("{label}: the bridge ended the peer's transport without a close")
        }
        (_, Some((opcode, payload))) => {
            panic!("{label}: the bridge answered its close with opcode {opcode:#x}: {payload:?}")
        }
    }
    backend
        .expect(
            BackendEvent::Released(row),
            &format!("{label}: the backend was released"),
        )
        .await;
}

/// Read the backend's greeting off a live bridge, then send the peer's own
/// frame across and require the backend to report it.
///
/// The first half of every committed bridge, whatever the row then does with
/// it: close it, or read a further frame behind it.
pub(crate) async fn assert_greetings_exchanged(
    peer: &mut TcpStream,
    backend: &mut ScriptedWsBackend,
    row: usize,
    label: &str,
) {
    let payload = read_bridged_text(peer, "the backend's frame", label).await;
    assert_eq!(payload.as_ref(), BACKEND_GREETING, "{label}: backend frame");

    write_async_ws_frame(peer, 0x1, CLIENT_GREETING, "the downstream frame").await;
    backend
        .expect(
            BackendEvent::Exchanged(row, CLIENT_GREETING.into()),
            &format!("{label}: the downstream frame crossed the bridge"),
        )
        .await;
}

/// Read the next frame off a live bridge and require it to be text.
///
/// End of stream fails the row: `what` names the frame the bridge ended
/// before.
pub(crate) async fn read_bridged_text(peer: &mut TcpStream, what: &str, label: &str) -> Box<[u8]> {
    let (opcode, payload) = read_async_ws_frame_or_eof(peer, what)
        .await
        .unwrap_or_else(|| panic!("{label}: the bridge ended before {what}"));
    assert_eq!(opcode, 0x1, "{label}: {what} is text");
    payload
}

/// Whether `head` is an offer for the backend path, as a WebSocket `GET`.
pub(crate) fn offers_backend_path(head: &str) -> bool {
    head.strip_prefix("GET ")
        .and_then(|rest| rest.strip_prefix(BACKEND_PATH))
        .is_some_and(|rest| rest.starts_with(" HTTP/1.1\r\n"))
}

/// Require that row `row`'s backend saw its whole offer as a WebSocket GET.
pub(crate) async fn assert_offer_arrived(
    backend: &mut ScriptedWsBackend,
    row: usize,
    label: &str,
) -> Box<str> {
    let head = backend.offered(row, label).await;
    assert!(
        offers_backend_path(&head),
        "{label}: the backend was offered {head:?}"
    );
    head
}

// ── Scripted backend answers ───────────────────────────────────────

fn forbidden(_: &str) -> String {
    "HTTP/1.1 403 Forbidden\r\nContent-Length: 9\r\nConnection: close\r\n\r\nforbidden".into()
}

fn selecting_second_offer(accept: &str) -> String {
    switching(accept, "Sec-WebSocket-Protocol: graphql-transport-ws\r\n")
}

fn selecting_first_offer(accept: &str) -> String {
    switching(accept, "Sec-WebSocket-Protocol: graphql-ws\r\n")
}

fn selecting_repeated(accept: &str) -> String {
    switching(
        accept,
        "Sec-WebSocket-Protocol: graphql-ws\r\nSec-WebSocket-Protocol: graphql-ws\r\n",
    )
}

fn selecting_list(accept: &str) -> String {
    switching(
        accept,
        "Sec-WebSocket-Protocol: graphql-ws, graphql-transport-ws\r\n",
    )
}

fn selecting_malformed(accept: &str) -> String {
    switching(accept, "Sec-WebSocket-Protocol: graphql/ws\r\n")
}

fn selecting_unoffered(accept: &str) -> String {
    switching(accept, "Sec-WebSocket-Protocol: mqtt\r\n")
}

fn accept_absent(_: &str) -> String {
    "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\r\n".into()
}

fn accept_repeated(accept: &str) -> String {
    switching(accept, &format!("Sec-WebSocket-Accept: {accept}\r\n"))
}

fn accept_wrong(_: &str) -> String {
    // The accept a correct backend derives from a key this proxy never sent.
    switching(
        &crate::ws_backend_script::accept_for("AAAAAAAAAAAAAAAAAAAAAA=="),
        "",
    )
}

fn status_not_switching(accept: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\nContent-Length: 0\r\n\r\n"
    )
}

fn upgrade_absent(accept: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    )
}

fn upgrade_wrong(accept: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: h2c\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    )
}

fn upgrade_repeated(accept: &str) -> String {
    switching(accept, "Upgrade: websocket\r\n")
}

fn connection_absent(accept: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    )
}

fn connection_without_upgrade(accept: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: keep-alive\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    )
}

fn connection_malformed(accept: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: upgrade/1\r\n\
         Sec-WebSocket-Accept: {accept}\r\n\r\n"
    )
}

fn extension_offered(accept: &str) -> String {
    switching(accept, "Sec-WebSocket-Extensions: permessage-deflate\r\n")
}

fn extension_empty(accept: &str) -> String {
    switching(accept, "Sec-WebSocket-Extensions: \r\n")
}

fn mixed_case_tokens(accept: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\nUpgrade: WebSocket\r\n\
         Connection: keep-alive, UPGRADE\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
    )
}

// ── 6.T1 ───────────────────────────────────────────────────────────

/// 6.T1
///
/// A backend that read the whole offer and refused it leaves the peer with the
/// proxy's structured refusal, on either proxy kind. The backend answered
/// before the peer was told anything, so no `101` can precede it.
#[camber::test]
async fn backend_refusal_precedes_downstream_upgrade() {
    let prefixes = [BUFFERED, STREAMING];
    let mut backend = ScriptedWsBackend::bind(
        prefixes
            .iter()
            .map(|_| BackendScript::Answer(forbidden))
            .collect(),
    )
    .await;
    let proxy = NegotiatingProxy::serve(both_proxy_kinds(&backend.http_url())).await;

    for (row, prefix) in prefixes.into_iter().enumerate() {
        let label = format!("backend 403 behind {prefix}");
        let mut peer = offer(proxy.addr(), prefix, &[]).await;
        assert_offer_arrived(&mut backend, row, &label).await;
        let head = read_async_http_head(&mut peer, "the refused downstream head").await;
        assert_backend_refused(&head, &proxy, &route_of(prefix), &label);
        backend.expect(BackendEvent::Released(row), &label).await;
    }

    proxy.stop_cleanly().await;
    backend.finish("backend refusal").await;
}

// ── 6.T2 ───────────────────────────────────────────────────────────

/// One backend answer the proxy must refuse before its peer's `101`.
struct HandshakeRow {
    label: &'static str,
    answer: fn(&str) -> String,
}

const INVALID_HANDSHAKES: [HandshakeRow; 12] = [
    HandshakeRow {
        label: "Sec-WebSocket-Accept absent",
        answer: accept_absent,
    },
    HandshakeRow {
        label: "Sec-WebSocket-Accept repeated",
        answer: accept_repeated,
    },
    HandshakeRow {
        label: "Sec-WebSocket-Accept wrong",
        answer: accept_wrong,
    },
    HandshakeRow {
        label: "status other than 101",
        answer: status_not_switching,
    },
    HandshakeRow {
        label: "Upgrade absent",
        answer: upgrade_absent,
    },
    HandshakeRow {
        label: "Upgrade names another protocol",
        answer: upgrade_wrong,
    },
    HandshakeRow {
        label: "Upgrade repeated",
        answer: upgrade_repeated,
    },
    HandshakeRow {
        label: "Connection absent",
        answer: connection_absent,
    },
    HandshakeRow {
        label: "Connection without an upgrade token",
        answer: connection_without_upgrade,
    },
    HandshakeRow {
        label: "Connection malformed around its upgrade token",
        answer: connection_malformed,
    },
    HandshakeRow {
        label: "unsolicited extension",
        answer: extension_offered,
    },
    HandshakeRow {
        label: "unsolicited empty extension value",
        answer: extension_empty,
    },
];

/// 6.T2
///
/// Every field the downstream `101` relies on is checked on the backend's
/// answer first. Each invalid answer closes its backend, which then observes
/// the proxy release the transport, while the peer reads the mapped refusal.
/// Mixed-case tokens are valid and still upgrade.
#[camber::test]
async fn backend_handshake_fields_gate_downstream_upgrade() {
    let script = INVALID_HANDSHAKES
        .iter()
        .map(|row| BackendScript::Answer(row.answer))
        .chain(std::iter::once(BackendScript::Upgrade(mixed_case_tokens)))
        .collect();
    let mut backend = ScriptedWsBackend::bind(script).await;
    let proxy = NegotiatingProxy::serve(both_proxy_kinds(&backend.http_url())).await;

    for (row, case) in INVALID_HANDSHAKES.iter().enumerate() {
        let label = case.label;
        let mut peer = offer(proxy.addr(), BUFFERED, &[]).await;
        assert_offer_arrived(&mut backend, row, label).await;
        let head = read_async_http_head(&mut peer, "the gated downstream head").await;
        assert_backend_refused(&head, &proxy, &route_of(BUFFERED), label);
        backend.expect(BackendEvent::Released(row), label).await;
    }

    let control = INVALID_HANDSHAKES.len();
    let label = "valid mixed-case upgrade tokens";
    let mut peer = offer(proxy.addr(), BUFFERED, &[]).await;
    assert_offer_arrived(&mut backend, control, label).await;
    let head = read_async_http_head(&mut peer, "the mixed-case downstream head").await;
    assert_eq!(status_from_raw(&head), 101, "{label}: {head}");
    assert_bridged(&mut peer, &mut backend, control, label).await;
    drop(peer);

    proxy.stop_cleanly().await;
    backend.finish("backend handshake fields").await;
}

// ── 6.T3 ───────────────────────────────────────────────────────────

/// 6.T3, the offer half.
///
/// The backend is offered every protocol the peer offered, in the peer's
/// order, across every line the peer carried them in — never a token the proxy
/// chose on its behalf.
#[camber::test]
async fn backend_offer_retains_all_protocols() {
    let mut backend = ScriptedWsBackend::bind(Box::new([BackendScript::Upgrade(
        switching_without_selection,
    )]))
    .await;
    let proxy = NegotiatingProxy::serve(both_proxy_kinds(&backend.http_url())).await;
    let label = "an ordered offer across two lines";

    let mut peer = offer(
        proxy.addr(),
        BUFFERED,
        &[
            ("Sec-WebSocket-Protocol", "graphql-ws, graphql-transport-ws"),
            ("Sec-WebSocket-Protocol", "chat"),
        ],
    )
    .await;
    let offered = assert_offer_arrived(&mut backend, 0, label).await;
    let protocols = offered_protocols(&offered);
    assert_eq!(
        *protocols
            .iter()
            .map(|token| &**token)
            .collect::<Box<[&str]>>(),
        ["graphql-ws", "graphql-transport-ws", "chat"],
        "{INCOMPLETE_OFFER} ({label}): {offered}"
    );

    let head = read_async_http_head(&mut peer, "the fully offered downstream head").await;
    assert_eq!(status_from_raw(&head), 101, "{label}: {head}");
    assert_bridged(&mut peer, &mut backend, 0, label).await;
    drop(peer);

    proxy.stop_cleanly().await;
    backend.finish(label).await;
}

/// One backend selection the downstream `101` must carry exactly.
struct SelectionRow {
    label: &'static str,
    answer: fn(&str) -> String,
    selected: &'static [&'static str],
}

const VALID_SELECTIONS: [SelectionRow; 2] = [
    SelectionRow {
        label: "backend selects the second offer",
        answer: selecting_second_offer,
        selected: &["graphql-transport-ws"],
    },
    SelectionRow {
        label: "backend selects nothing after offers",
        answer: switching_without_selection,
        selected: &[],
    },
];

/// One backend selection the proxy must refuse before its peer's `101`.
struct InvalidSelectionRow {
    label: &'static str,
    answer: fn(&str) -> String,
    offers: &'static [(&'static str, &'static str)],
}

/// The offer every selection row makes unless it is about an absent one.
const TWO_OFFERS: &[(&str, &str)] =
    &[("Sec-WebSocket-Protocol", "graphql-ws, graphql-transport-ws")];

const INVALID_SELECTIONS: [InvalidSelectionRow; 5] = [
    InvalidSelectionRow {
        label: "selection repeated",
        answer: selecting_repeated,
        offers: TWO_OFFERS,
    },
    InvalidSelectionRow {
        label: "selection is a comma list",
        answer: selecting_list,
        offers: TWO_OFFERS,
    },
    InvalidSelectionRow {
        label: "selection malformed",
        answer: selecting_malformed,
        offers: TWO_OFFERS,
    },
    InvalidSelectionRow {
        label: "selection never offered",
        answer: selecting_unoffered,
        offers: TWO_OFFERS,
    },
    InvalidSelectionRow {
        label: "selection with no offer",
        answer: selecting_first_offer,
        offers: &[],
    },
];

/// 6.T3, the selection half.
///
/// The downstream `101` carries exactly what the backend selected — a later
/// offer, or nothing at all — and a bridge then carries real frames each way.
/// A selection the backend was not entitled to make is a refused handshake.
#[camber::test]
async fn backend_selection_is_the_only_downstream_subprotocol() {
    let script = VALID_SELECTIONS
        .iter()
        .map(|row| BackendScript::Upgrade(row.answer))
        .chain(
            INVALID_SELECTIONS
                .iter()
                .map(|row| BackendScript::Answer(row.answer)),
        )
        .collect();
    let mut backend = ScriptedWsBackend::bind(script).await;
    let proxy = NegotiatingProxy::serve(both_proxy_kinds(&backend.http_url())).await;

    for (row, case) in VALID_SELECTIONS.iter().enumerate() {
        let label = case.label;
        let mut peer = offer(proxy.addr(), BUFFERED, TWO_OFFERS).await;
        assert_offer_arrived(&mut backend, row, label).await;
        let head = read_async_http_head(&mut peer, "the selected downstream head").await;
        let echoed: Box<[&str]> = header_values(&head, "sec-websocket-protocol").collect();
        assert!(
            status_from_raw(&head) == 101 && *echoed == *case.selected,
            "{SELECTION_MISMATCH} ({label}): expected {:?}, got: {head}",
            case.selected
        );
        assert_bridged(&mut peer, &mut backend, row, label).await;
    }

    for (index, case) in INVALID_SELECTIONS.iter().enumerate() {
        let row = VALID_SELECTIONS.len() + index;
        let label = case.label;
        let mut peer = offer(proxy.addr(), BUFFERED, case.offers).await;
        assert_offer_arrived(&mut backend, row, label).await;
        let head = read_async_http_head(&mut peer, "the refused selection head").await;
        assert_backend_refused(&head, &proxy, &route_of(BUFFERED), label);
        backend.expect(BackendEvent::Released(row), label).await;
    }

    proxy.stop_cleanly().await;
    backend.finish("backend selection").await;
}
