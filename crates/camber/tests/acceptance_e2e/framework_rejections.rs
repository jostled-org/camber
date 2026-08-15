#![cfg(all(feature = "ws", feature = "grpc"))]

//! Routing refusals over the transports a service actually answers on.
//!
//! Mapper precedence and protocol-owned output are claims about the wire, so
//! every case here reads a real HTTP/1 response off a socket or a real HTTP/2
//! response off an h2 stream. Nothing is asserted from a value a dispatch path
//! built.

use crate::common;
use crate::http as wire;

use crate::http::{HttpResponse, WIRE_TIMEOUT, bounded};
use camber::RuntimeError;
use camber::http::{
    HostRouter, Next, Rejection, RejectionContext, RejectionProtocol, Request, Response, Router,
    WsConn,
};
use camber::runtime;
use common::{
    Journal, Observed, assert_field_value, assert_fields, assert_message_is_fixed, only,
    only_event, recording_mapper,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Send one HTTP/1 request without stating a connection preference.
///
/// [`wire::send_to_host`] asks the server to close, and the one case that sends
/// this way reads back the disposition the framework chose: a request that
/// stated its own preference would be reading that preference back.
fn http1_keepalive(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    host: &str,
) -> HttpResponse {
    wire::send_to_host_with(addr, method, path, host, &[])
}

/// Send one HTTP/2 request over its own connection and close that connection.
fn http2(addr: std::net::SocketAddr, method: &str, path: &str, host: &str) -> HttpResponse {
    http2_with(addr, method, path, host, &[])
}

/// Send one HTTP/2 request carrying extra headers over its own connection.
fn http2_with(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
) -> HttpResponse {
    common::block_on(common::h2_request(
        addr,
        method,
        path,
        host,
        headers,
        WIRE_TIMEOUT,
    ))
}

/// How many of one batch of recorded invocations came from one policy.
///
/// The journal already carries the origin every policy recorded under, so a
/// per-policy counter beside it would be a second, weaker record of the same
/// thing: it counts, where this names what was counted when a row disagrees.
fn invocations(seen: &[Observed], policy: &str) -> usize {
    seen.iter().filter(|entry| entry.origin == policy).count()
}

/// Which transport carried one precedence row.
type Transport = fn(std::net::SocketAddr, &str, &str, &str) -> HttpResponse;

/// Both transports a precedence claim has to hold on.
const TRANSPORTS: [(&str, Transport); 2] = [("HTTP/1", wire::send_to_host), ("HTTP/2", http2)];

#[test]
fn host_child_and_builtin_mapper_precedence_is_frozen_once() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();

            let mut hosts =
                HostRouter::new().rejection_mapper(recording_mapper(&journal, HOST_POLICY));
            hosts.add(
                "child.test",
                Router::new().rejection_mapper(recording_mapper(&journal, CHILD_POLICY)),
            );
            hosts.add("plain.test", Router::new());
            // Nothing is drained after the spawn: the readiness probe sends a
            // head Hyper refuses below Camber's mapper boundary — the claim
            // `mapper_boundary_stops_at_parser_and_protocol_commitment` proves —
            // so no policy is asked for it and the journal starts empty.
            let hosted = common::spawn_host_server(hosts);
            let bare = common::spawn_server(Router::new());

            let selections = [
                ("child.test", 1_usize, 0_usize),
                ("plain.test", 0, 1),
                ("nowhere.test", 0, 1),
            ];
            for (transport, send) in TRANSPORTS {
                for (host, child_answers, host_answers) in selections {
                    let answer = send(hosted, "GET", "/missing", host);
                    let label = format!("{transport} {host}");

                    assert_eq!(answer.status, 404, "{label}: wire status");
                    assert_eq!(answer.text().as_ref(), "not found", "{label}: safe body");
                    // Taken per row rather than as a delta off a running count:
                    // the drain empties the record, so what it reports is this
                    // row's invocations and no earlier row's.
                    let seen = common::drain(&journal);
                    assert_eq!(
                        invocations(&seen, CHILD_POLICY),
                        child_answers,
                        "{label}: child policy invocations, out of {seen:?}"
                    );
                    assert_eq!(
                        invocations(&seen, HOST_POLICY),
                        host_answers,
                        "{label}: host policy invocations, out of {seen:?}"
                    );
                }

                let built_in = send(bare, "GET", "/missing", "localhost");
                assert_eq!(built_in.status, 404, "{transport}: built-in status");
                assert_eq!(
                    built_in.text().as_ref(),
                    "not found",
                    "{transport}: built-in body"
                );
                assert_eq!(
                    built_in.header("content-type"),
                    Some("text/plain"),
                    "{transport}: built-in content type"
                );
                common::request_id_of(
                    &built_in,
                    &format!("{transport}: built-in request identity"),
                );
            }

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// The value a mapper offers for every protected header, to be corrected or not.
const MAPPER_ALLOW: &str = "TRACE";

/// An application header the `Connection` value below names as its own.
///
/// HTTP/2 forbids `Connection`, and Hyper deletes both it and every header it
/// lists. Camber's finalizer deletes a fixed set instead — the five names in
/// `CONNECTION_SPECIFIC_HEADERS`: `Connection`, `Keep-Alive`,
/// `Proxy-Connection`, `Transfer-Encoding`, and `Upgrade`. This header is
/// outside that set, so it survives exactly when Camber removed `Connection`
/// before Hyper ever saw it. A name inside the set would be deleted either way
/// and would tell the two orderings apart from neither.
const LISTED_HEADER: &str = "X-Listed";

/// The value that header carries when it reaches the peer.
const LISTED_VALUE: &str = "kept";

/// A `Connection` value that claims an application header as connection-specific.
const MAPPER_CONNECTION: &str = "keep-alive, X-Listed";

/// The headers a response offers for the transport to correct or to delete.
fn connection_claiming_headers(response: Response) -> Response {
    response
        .with_header(LISTED_HEADER, LISTED_VALUE)
        .with_header("Connection", MAPPER_CONNECTION)
}

/// A router whose policy answers with values the protocol may have to correct.
fn protected_output_router() -> Router {
    let mut router = Router::new();
    router.get("/only-get", |_req: &Request| async {
        Response::text(200, "ok")
    });
    router.get("/teapot", |_req: &Request| async {
        Response::text(200, "ok")
    });
    router.get("/accepted", |_req: &Request| async {
        Response::text(200, "ok").map(connection_claiming_headers)
    });
    router.rejection_mapper(|rejection: &Rejection, context: &RejectionContext| {
        let answer = match context.raw_path() {
            "/teapot" => Response::text(418, "teapot")?,
            _ => Response::text(rejection.status(), rejection.message())?,
        };
        Ok(connection_claiming_headers(
            answer
                .with_header("Allow", MAPPER_ALLOW)
                .with_header("X-Custom", "kept"),
        ))
    })
}

/// A host policy that asks to keep a connection the framework must close.
fn forced_close_hosts() -> HostRouter {
    let mut hosts =
        HostRouter::new().rejection_mapper(|rejection: &Rejection, _context: &RejectionContext| {
            Ok(Response::text(rejection.status(), rejection.message())?
                .with_header("Connection", "keep-alive"))
        });
    hosts.add("app.test", Router::new());
    hosts
}

/// `Allow` is the framework's at `405` and the mapper's at any other status.
fn assert_allow_authority(addr: std::net::SocketAddr) {
    let corrected = wire::send_to_host(addr, "DELETE", "/only-get", "localhost");
    assert_eq!(corrected.status, 405, "a mapped 405 keeps its status");
    assert_eq!(
        corrected.header("allow"),
        Some("GET, HEAD"),
        "a final 405 carries the frozen canonical set, not the mapper's value"
    );
    assert_eq!(
        corrected.header("x-custom"),
        Some("kept"),
        "every other mapper header survives correction"
    );

    let uncorrected = wire::send_to_host(addr, "DELETE", "/teapot", "localhost");
    assert_eq!(
        uncorrected.status, 418,
        "a mapper may choose another status"
    );
    assert_eq!(
        uncorrected.header("allow"),
        Some(MAPPER_ALLOW),
        "a final status other than 405 has no framework-required Allow"
    );
}

/// A `HEAD` rejection loses its body and nothing else.
///
/// The `GET` row calibrates the channel: it is the same refusal on the same
/// address, so it establishes that this policy produces a body at all. Without
/// it an empty `HEAD` answer would read the same whether the body was
/// suppressed or the mapper never wrote one.
fn assert_head_suppression(addr: std::net::SocketAddr) {
    let got = wire::send_to_host(addr, "GET", "/missing", "localhost");
    assert_eq!(got.status, 404, "the calibrating GET is mapped");
    assert_eq!(
        got.text().as_ref(),
        "not found",
        "the calibrating GET carries a body"
    );
    assert_eq!(
        got.header("content-type"),
        Some("text/plain"),
        "the calibrating GET names its representation"
    );

    let head = wire::send_to_host(addr, "HEAD", "/missing", "localhost");
    assert_eq!(
        head.status, got.status,
        "HEAD does not change the mapped status"
    );
    assert_eq!(
        head.header("content-type"),
        got.header("content-type"),
        "HEAD does not change the representation headers"
    );
    assert!(head.body.is_empty(), "HEAD sends zero body bytes");
}

/// HTTP/2 loses the connection-specific header and keeps what it claimed.
///
/// The accepted row calibrates the channel: a response the finalizer never
/// touches reaches Hyper with the same `Connection` value, and Hyper deletes
/// both it and the header that value lists. Without that row the mapped
/// assertion could not tell Camber's removal from a value Hyper took away.
fn assert_http2_connection_authority(addr: std::net::SocketAddr) {
    let accepted = http2(addr, "GET", "/accepted", "localhost");
    assert_eq!(accepted.status, 200, "an accepted response is not mapped");
    assert_eq!(
        accepted.header("connection"),
        None,
        "Hyper deletes a Connection header it is handed"
    );
    assert_eq!(
        accepted.header(LISTED_HEADER),
        None,
        "Hyper also deletes every header that Connection value lists"
    );

    let over_h2 = http2(addr, "DELETE", "/only-get", "localhost");
    assert_eq!(over_h2.status, 405, "HTTP/2 carries the same mapped status");
    assert_eq!(over_h2.header("allow"), Some("GET, HEAD"));
    assert_eq!(over_h2.header("x-custom"), Some("kept"));
    assert_eq!(
        over_h2.header("connection"),
        None,
        "HTTP/2 carries no connection-specific header"
    );
    assert_eq!(
        over_h2.header(LISTED_HEADER),
        Some(LISTED_VALUE),
        "the finalizer removed Connection before Hyper could delete what it listed"
    );
}

/// A required close overwrites the disposition a mapper asked for.
fn assert_required_close() {
    let hosted = common::spawn_host_server(forced_close_hosts());
    let closed = http1_keepalive(hosted, "GET", "/anything", "bad host");
    assert_eq!(closed.status, 400, "a malformed authority is refused");
    assert_eq!(
        closed.header("connection"),
        Some("close"),
        "a required close disposition overwrites what the mapper asked for"
    );
}

#[test]
fn protocol_finalizer_corrects_protected_output_after_custom_mapping() {
    common::test_runtime()
        .run(|| {
            let addr = common::spawn_server(protected_output_router());

            assert_allow_authority(addr);
            assert_head_suppression(addr);
            assert_http2_connection_authority(addr);
            assert_required_close();

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── One precedence rule for every dispatch class (4.T5) ────────────

/// The ordinary buffered route the class table asks for.
const ORDINARY: &str = "/ordinary";

/// The streaming-response route.
const STREAMED: &str = "/streamed";

/// The server-sent events route.
const EVENTS: &str = "/events";

/// The direct WebSocket route.
const SOCKET: &str = "/socket";

/// The proxied prefix, and the pattern its wildcard route registers under.
const PROXIED: &str = "/proxied";
const PROXIED_ROUTE: &str = "/proxied/*proxy_path";

/// The path every proxied row asks for under that prefix.
const PROXIED_PATH: &str = "/proxied/upstream";

/// The message the gate every class row passes through declares safe.
const GATE_MESSAGE: &str = "gate refused";

/// A gate that refuses every request, whichever class dispatch selected.
///
/// One frame for every class, so a row's category and context come from the
/// class the request took rather than from a frame written for it.
///
/// Named for the unconditional refusal, because the WebSocket root has a gate
/// that decides per route: two frames spelled `refusing_gate` in two binaries
/// left a reader moving between them expecting the same behavior from both.
fn gate_refusing_every_request(router: &mut Router) {
    router.use_middleware(|_req: &Request, _next: Next| async {
        Err::<Response, RuntimeError>(RuntimeError::BadRequest(GATE_MESSAGE.into()))
    });
}

/// Register every dispatch class on one router, behind the refusing gate.
fn specialized_routes(router: &mut Router) {
    gate_refusing_every_request(router);
    router.get(ORDINARY, |_req: &Request| async {
        Response::text(200, "ok")
    });
    router.get_stream(STREAMED, |_req: &Request| {
        Box::pin(async {
            let (response, _sender) = camber::http::StreamResponse::new(200);
            response
        })
    });
    router.get_sse(
        EVENTS,
        |_req: &Request, _writer: &mut camber::http::SseWriter| Ok(()),
    );
    router.ws(SOCKET, |_req: &Request, _ws: camber::http::WsConn| Ok(()));
    // The upstream is never dialled: the gate refuses first, which is the whole
    // claim. Pointing it at a closed port keeps that true even if it were.
    router.proxy_stream(PROXIED, "http://127.0.0.1:1");
}

/// The names the two configured policies record their invocations under.
const CHILD_POLICY: &str = "child";
const HOST_POLICY: &str = "host";

/// The router every class row is answered by, with the policy it was given.
///
/// The journal is per-fixture rather than per-process: two of this root's cases
/// build these routers, and the root runs its cases in parallel.
fn specialized_router(policy: Option<(&Journal, &'static str)>) -> Router {
    let mut router = Router::new();
    specialized_routes(&mut router);
    match policy {
        Some((journal, origin)) => router.rejection_mapper(recording_mapper(journal, origin)),
        None => router,
    }
}

/// How one class row is put on the wire.
///
/// Address, path, and authority: the method is not a parameter because every
/// class here selects on the route it was registered under, and a `GET` is what
/// each of them accepts.
type ClassSender = fn(std::net::SocketAddr, &str, &str) -> HttpResponse;

/// Send one ordinary `GET`, addressed to one authority.
fn http1_get(addr: std::net::SocketAddr, path: &str, host: &str) -> HttpResponse {
    wire::send_to_host(addr, "GET", path, host)
}

/// Send the same `GET` over an HTTP/2 connection of its own.
fn http2_get(addr: std::net::SocketAddr, path: &str, host: &str) -> HttpResponse {
    http2(addr, "GET", path, host)
}

/// Send an upgrade request and read the answer, addressed to one authority.
fn ws_upgrade(addr: std::net::SocketAddr, path: &str, host: &str) -> HttpResponse {
    let mut peer = common::start_upgrade_with(addr, &common::ws_upgrade_request_to(host, path));
    wire::read_http_response_bounded(&mut peer).expect("no answer to the upgrade request")
}

/// The transports a class answering ordinary requests has to hold on.
///
/// Precedence is a claim about which policy answered, and HTTP/2 reaches
/// dispatch through its own Hyper connection service — so a row that only ever
/// ran over HTTP/1 would leave the other half of that claim unsent.
const BOTH_TRANSPORTS: &[(&str, ClassSender)] = &[("HTTP/1", http1_get), ("HTTP/2", http2_get)];

/// The transport a raw upgrade has: HTTP/1 is what a `101` handshake is spoken on.
const UPGRADE_TRANSPORT: &[(&str, ClassSender)] = &[("HTTP/1", ws_upgrade)];

/// One dispatch class, and the identity its refusal must report.
struct ClassRow {
    label: &'static str,
    path: &'static str,
    route: &'static str,
    protocol: RejectionProtocol,
    sends: &'static [(&'static str, ClassSender)],
}

const CLASS_ROWS: [ClassRow; 6] = [
    ClassRow {
        label: "buffered handler",
        path: ORDINARY,
        route: ORDINARY,
        protocol: RejectionProtocol::OrdinaryHttp,
        sends: BOTH_TRANSPORTS,
    },
    ClassRow {
        label: "streaming response",
        path: STREAMED,
        route: STREAMED,
        protocol: RejectionProtocol::StreamingHttp,
        sends: BOTH_TRANSPORTS,
    },
    ClassRow {
        label: "server-sent events",
        path: EVENTS,
        route: EVENTS,
        protocol: RejectionProtocol::ServerSentEvents,
        sends: BOTH_TRANSPORTS,
    },
    ClassRow {
        label: "direct WebSocket",
        path: SOCKET,
        route: SOCKET,
        protocol: RejectionProtocol::WebSocket,
        sends: UPGRADE_TRANSPORT,
    },
    ClassRow {
        label: "proxied request",
        path: PROXIED_PATH,
        route: PROXIED_ROUTE,
        protocol: RejectionProtocol::Proxy,
        sends: BOTH_TRANSPORTS,
    },
    ClassRow {
        label: "proxied WebSocket",
        path: PROXIED_PATH,
        route: PROXIED_ROUTE,
        protocol: RejectionProtocol::WebSocket,
        sends: UPGRADE_TRANSPORT,
    },
];

/// Every leg the class table declares, across its rows and their transports.
fn declared_legs() -> usize {
    CLASS_ROWS
        .iter()
        .map(|row| row.sends.len() * SELECTIONS.len())
        .sum()
}

/// Which authority a selection names, and the policy that must answer it.
struct Selection {
    host: &'static str,
    policy: &'static str,
}

const SELECTIONS: [Selection; 2] = [
    Selection {
        host: "child.test",
        policy: CHILD_POLICY,
    },
    Selection {
        host: "plain.test",
        policy: HOST_POLICY,
    },
];

/// The host router the class table drives, with both configured policies.
fn class_hosts(journal: &Journal) -> HostRouter {
    let mut hosts = HostRouter::new().rejection_mapper(recording_mapper(journal, HOST_POLICY));
    hosts.add(
        "child.test",
        specialized_router(Some((journal, CHILD_POLICY))),
    );
    hosts.add("plain.test", specialized_router(None));
    hosts
}

/// Assert one class row against the selection and transport that answered it.
fn assert_class_row(
    row: &ClassRow,
    answer: &HttpResponse,
    seen: &Observed,
    selection: &Selection,
    transport: &str,
) {
    let label = format!("{} via {} over {}", row.label, selection.host, transport);
    assert_eq!(answer.status, 400, "{label}: the gate's default status");
    assert_eq!(
        answer.text().as_ref(),
        GATE_MESSAGE,
        "{label}: the safe message"
    );
    assert_eq!(
        seen.origin, selection.policy,
        "{label}: the policy this selection resolves to"
    );
    assert_eq!(
        seen.route.as_deref(),
        Some(row.route),
        "{label}: the selected route is established"
    );
    assert_eq!(
        seen.protocol,
        Some(row.protocol),
        "{label}: the selected dispatch class is established"
    );
    // Only the built-in and fixed-fallback answers name the request on the
    // response; a configured policy owns every header of what it returns. The
    // built-in row asserts the other half of this, so the pair is what says
    // which policy actually wrote the answer the peer read.
    assert_eq!(
        answer.header("x-request-id"),
        None,
        "{label}: a configured policy's answer carries only what that policy wrote"
    );
}

/// The built-in policy answers a class no configured mapper claimed.
///
/// `X-Request-Id` is the built-in's own signature: it is added by
/// `built_in_response` and by the fixed fallback, and by nothing else. Its
/// presence here and its absence from every configured answer is what separates
/// the two policies on the wire.
fn assert_built_in_class(
    addr: std::net::SocketAddr,
    row: &ClassRow,
    send: ClassSender,
    transport: &str,
) {
    let answer = send(addr, row.path, "localhost");
    let label = format!("{} over {transport}", row.label);
    assert_eq!(answer.status, 400, "{label}: built-in status");
    assert_eq!(
        answer.text().as_ref(),
        GATE_MESSAGE,
        "{label}: built-in body"
    );
    common::request_id_of(&answer, &format!("{label}: built-in request identity"));
}

/// Run one class row on one transport under every selection, and count the legs.
///
/// The count is taken inside the loop, after the assertions each selection is
/// held to, so it reports the legs that actually completed rather than the legs
/// the table declares.
fn assert_class_transport(
    hosted: std::net::SocketAddr,
    row: &ClassRow,
    send: ClassSender,
    transport: &str,
    journal: &Journal,
) -> usize {
    let mut legs = 0_usize;
    for selection in &SELECTIONS {
        let answer = send(hosted, row.path, selection.host);
        let label = format!("{} via {} over {transport}", row.label, selection.host);
        let seen = only(journal, &label);
        assert_class_row(row, &answer, &seen, selection, transport);
        legs += 1;
    }
    legs
}

#[test]
fn specialized_routes_share_child_host_builtin_precedence() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            // Nothing is drained after the spawn: the readiness probe sends a
            // head Hyper refuses below Camber's mapper boundary — the claim
            // `mapper_boundary_stops_at_parser_and_protocol_commitment` proves —
            // so no policy is asked for it and the journal starts empty.
            let hosted = common::spawn_host_server(class_hosts(&journal));
            let bare = common::spawn_server(specialized_router(None));

            let mut legs = 0_usize;
            for row in &CLASS_ROWS {
                // A row declaring no transport contributes zero to the count
                // taken below and zero to the count it is compared against, so
                // the whole class would go unexercised and the totals would
                // still agree. `sends` is a slice, so only this rules it out.
                assert!(
                    !row.sends.is_empty(),
                    "{}: declares no transport to answer on",
                    row.label
                );
                for (transport, send) in row.sends {
                    legs += assert_class_transport(hosted, row, *send, transport, &journal);
                    assert_built_in_class(bare, row, *send, transport);
                }
            }

            assert_eq!(
                legs,
                declared_legs(),
                "every declared class ran on every transport it answers on, under every selection"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── gRPC selects the same way, on the router that carries it ───────

/// The content type that makes a request gRPC as far as dispatch is concerned.
const GRPC_CONTENT_TYPE: &str = "application/grpc";

/// The fixed identity the gRPC dispatch class names its refusals with.
const GRPC_ROUTE: &str = "grpc";

/// Send one gRPC-shaped call over its own HTTP/2 connection.
///
/// Real gRPC transport, because the pre-check that selects this dispatch class
/// runs before anything a proto would add: what the row turns on is the content
/// type and the HTTP/2 stream, both of which are here.
fn grpc_call(addr: std::net::SocketAddr, path: &str) -> HttpResponse {
    http2_with(
        addr,
        "POST",
        path,
        "localhost",
        &[("content-type", GRPC_CONTENT_TYPE)],
    )
}

/// A gRPC-carrying router, gated so the request never reaches tonic.
fn gated_grpc_router(policy: Option<(&Journal, &'static str)>) -> Router {
    let mut router = Router::new();
    gate_refusing_every_request(&mut router);
    router.grpc(camber::http::GrpcRouter::new());
    match policy {
        Some((journal, origin)) => router.rejection_mapper(recording_mapper(journal, origin)),
        None => router,
    }
}

#[test]
fn grpc_gate_rejection_selects_the_same_policy_as_every_other_class() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let configured =
                common::spawn_server(gated_grpc_router(Some((&journal, CHILD_POLICY))));
            let bare = common::spawn_server(gated_grpc_router(None));

            let answer = grpc_call(configured, "/greeter.Greeter/SayHello");
            assert_eq!(answer.status, 400, "the gate's default status");
            assert_eq!(answer.text().as_ref(), GATE_MESSAGE, "the safe message");
            let seen = only(&journal, "gRPC gate refusal");
            assert_eq!(
                seen.origin, CHILD_POLICY,
                "the configured policy answered it"
            );
            assert_eq!(
                seen.protocol,
                Some(RejectionProtocol::Grpc),
                "the gRPC dispatch class is established"
            );
            // The pre-check resolved the gRPC router this request dispatches
            // to, so the class establishes the fixed identity that registration
            // is named by — no trie pattern, and no absence either.
            assert_eq!(
                seen.route.as_deref(),
                Some(GRPC_ROUTE),
                "the gRPC dispatch class establishes its route identity"
            );
            // The built-in's signature, absent here and present below: only
            // `built_in_response` and the fixed fallback name the request on the
            // answer, so the pair says which policy wrote each of them.
            assert_eq!(
                answer.header("x-request-id"),
                None,
                "a configured policy's answer carries only what that policy wrote"
            );

            let built_in = grpc_call(bare, "/greeter.Greeter/SayHello");
            assert_eq!(built_in.status, 400, "built-in status");
            assert_eq!(built_in.text().as_ref(), GATE_MESSAGE, "built-in body");
            common::request_id_of(&built_in, "built-in request identity");

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── Where the mapper's authority ends (4.T6) ───────────────────────

/// The route whose channel-backed source dies after its head is committed.
const ABANDONED: &str = "/abandoned";

/// The body length that route's committed head promises the peer.
const PROMISED_LENGTH: usize = 32;

/// The bytes its source produces before it abandons the stream.
const PRODUCED: &str = "part";

/// The head has to promise more than the source will ever produce.
///
/// Stated at compile time, which is the only honest place for it: both values
/// are literals, so a runtime comparison of them could never have failed. What
/// the runtime assertions below read is what the peer actually got.
const _: () = assert!(PRODUCED.len() < PROMISED_LENGTH);

/// The name the boundary case's one policy records its invocations under.
const BOUNDARY_POLICY: &str = "boundary";

/// The signal that tells the abandoned source to give its stream up.
///
/// The source holds its sender until the case has the produced bytes off the
/// wire, then drops it. Without that rendezvous the send races the head: Hyper
/// answers a `Content-Length` the body ends short of by tearing the connection
/// down, and whether the produced bytes were flushed first is then decided by
/// which of the two happened to run — so the case could read an empty body from
/// a source that had produced, or a whole one from a source that had not.
///
/// Closed on the way out, whichever path the case leaves by. The source parks on
/// this permit, and an assertion failing between the head and [`Abandon::release`]
/// unwinds past the release the task is waiting for — leaving it parked on a
/// runtime worker for the rest of the binary. Closing the semaphore ends that
/// wait on every exit.
struct Abandon(Arc<tokio::sync::Semaphore>);

impl Abandon {
    fn new() -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(0)))
    }

    /// The half the abandoned source waits on.
    fn signal(&self) -> Arc<tokio::sync::Semaphore> {
        Arc::clone(&self.0)
    }

    /// Let the source give its stream up, short of the promise its head made.
    fn release(&self) {
        self.0.add_permits(1);
    }
}

impl Drop for Abandon {
    fn drop(&mut self) {
        self.0.close();
    }
}

/// A router whose only policy records, so an excluded failure records nothing.
///
/// The abandoned route is the channel-backed half of the commitment claim: its
/// head declares more bytes than the producer sends, and the sender then drops.
/// That is the one way a `StreamBody::Channel` source can fail after Hyper has
/// written its status line — the same point the truncated-upstream row reaches
/// through `StreamBody::Proxy`, on the other body variant.
///
/// Named for the claim rather than for the counter it used to hold: the
/// connection-limit root has a `counted_router` that counts something else
/// entirely, and one name over two behaviors misleads a reader moving between
/// them.
fn mapper_boundary_router(journal: &Journal, abandon: &Abandon) -> Router {
    let mut router = Router::new();
    router.get(ORDINARY, |_req: &Request| async {
        Response::text(200, "ok")
    });
    let abandon = abandon.signal();
    router.get_stream(ABANDONED, move |_req: &Request| {
        let abandon = Arc::clone(&abandon);
        Box::pin(async move {
            let (response, sender) = camber::http::StreamResponse::new(200);
            tokio::spawn(async move {
                sender
                    .send(PRODUCED)
                    .await
                    .expect("the abandoned source could not produce its bytes");
                // A closed rendezvous is the case ending rather than the
                // abandonment it waits for, and the stream is given up either
                // way — so the wait ends here instead of parking this task.
                if let Ok(permit) = abandon.acquire().await {
                    permit.forget();
                }
            });
            response.with_header("Content-Length", &PROMISED_LENGTH.to_string())
        })
    });
    router.grpc(camber::http::GrpcRouter::new());
    router.rejection_mapper(recording_mapper(journal, BOUNDARY_POLICY))
}

/// Write bytes that are not an HTTP request line and read what comes back.
fn malformed_request(addr: std::net::SocketAddr) -> String {
    use std::io::Write;
    let mut peer = wire::connect(addr).expect("the malformed peer could not connect");
    peer.write_all(b"@@ NOT-A-REQUEST-LINE\r\n\r\n")
        .expect("the malformed request could not be sent");
    peer.flush().expect("the malformed request could not flush");
    wire::drain_to_close(&mut peer, WIRE_TIMEOUT).expect("the malformed request was never answered")
}

/// Fail a TLS handshake against a real TLS listener, before any HTTP is written.
///
/// The client trusts nothing, so it refuses the server's certificate during the
/// handshake. Nothing Camber can classify has happened yet, which is the claim.
fn failed_tls_handshake(addr: std::net::SocketAddr) {
    let refused = common::block_on(bounded(
        async move {
            let connector =
                tokio_rustls::TlsConnector::from(Arc::new(common::tls_client_config(&[])));
            let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
            let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
            match connector.connect(name, tcp).await {
                Ok(_stream) => {
                    panic!("a client that trusts nothing must not complete this handshake")
                }
                Err(error) => error,
            }
        },
        WIRE_TIMEOUT,
        "TLS handshake",
    ));
    // Named rather than merely counted: an ALPN mismatch, a listener that never
    // armed TLS, and a refused certificate all fail the handshake, and only the
    // last of them is what this case claims happened. The name is the whole
    // check — rustls surfaces nearly every handshake failure as `InvalidData`,
    // so a kind admitted alongside it would readmit all three. This client's
    // root store is empty, so what it reports is `UnknownIssuer` against the
    // server's certificate.
    assert!(
        refused.to_string().contains("certificate"),
        "the handshake failed for something other than trust: {refused}"
    );
}

/// A channel-backed source that dies after its head is on the wire is excluded.
///
/// The head is committed before the source fails, so the peer keeps the status
/// it was already given, gets no second answer, and no policy is asked for one.
///
/// Read in three steps, because the claim has three parts and each has to be
/// observed where it happens: the head that commits the status, the bytes the
/// source really produced, and the close that ends the stream short of the
/// length that head promised. A single drain-to-close could distinguish none of
/// them — an empty body would satisfy "shorter than promised" as readily as a
/// source that produced nothing at all.
fn assert_committed_stream_excluded(
    addr: std::net::SocketAddr,
    journal: &Journal,
    abandon: &Abandon,
) {
    let mut peer = wire::connect(addr).expect("the streaming peer could not connect");
    wire::write_request(&mut peer, "GET", ABANDONED, &[], b"")
        .expect("the streaming request could not be sent");

    let head = wire::read_head(&mut peer, WIRE_TIMEOUT)
        .expect("the committed streaming head never reached the peer");
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "the committed streaming head reaches the peer: {head}"
    );
    assert!(
        head.contains(&format!("content-length: {PROMISED_LENGTH}")),
        "the committed head promised a length the source never produced: {head}"
    );

    // The produced bytes, off the wire, before anything has been abandoned.
    // This is what says the source produced at all, which every claim below
    // about it stopping short depends on.
    let produced = wire::read_delimited(
        &mut peer,
        PRODUCED.as_bytes(),
        PROMISED_LENGTH,
        WIRE_TIMEOUT,
    )
    .expect("the committed stream produced none of its body");
    assert_eq!(
        String::from_utf8_lossy(&produced),
        PRODUCED,
        "the peer read exactly the bytes the source produced"
    );

    // Now let the source give the stream up, short of the promise its head made.
    abandon.release();
    let rest =
        wire::drain_to_close(&mut peer, WIRE_TIMEOUT).expect("the committed stream never closed");
    assert_eq!(
        rest, "",
        "the stream ends where the source abandoned it, short of the promised {PROMISED_LENGTH} bytes: {rest:?}"
    );
    let whole = format!("{head}{PRODUCED}{rest}");
    assert_eq!(
        whole.matches("HTTP/1.1 ").count(),
        1,
        "an abandoned source produces no replacement response: {whole}"
    );
    assert_no_mapping(
        journal,
        "a streaming-source failure after header commitment claims no mapper execution",
    );
}

/// The path no route claims, whose refusal is inside the mapper's authority.
const UNCLAIMED: &str = "/nothing-here";

/// Require that no policy was asked to answer anything since the last drain.
///
/// The record is taken rather than read, so a row that follows this one starts
/// empty. What the entries were is reported with the failure: a boundary that
/// let something through is told apart from a second copy of the row before it
/// only by what the policy was handed.
fn assert_no_mapping(journal: &Journal, claim: &str) {
    let seen = common::drain(journal);
    assert!(seen.is_empty(), "{claim}: {seen:?}");
}

/// Drive one refusal the mapper does own, and take the record it left.
///
/// Every empty record below is a claim that the mapper was not entered, and a
/// journal nothing had ever recorded would read empty whether the boundary held
/// or the probe was simply wired to nothing. This is the calibration those empty
/// records are read against: a routing refusal on the same server, answered by
/// the same policy, which must record exactly one invocation.
fn calibrate_mapper(addr: std::net::SocketAddr, journal: &Journal) {
    let refused = wire::send_to_host(addr, "GET", UNCLAIMED, "localhost");
    assert_eq!(refused.status, 404, "the calibrating refusal is answered");
    assert_eq!(
        refused.text().as_ref(),
        "not found",
        "the calibrating refusal carries the policy's own body"
    );
    let seen = only(
        journal,
        "the calibrating refusal inside the mapper's authority",
    );
    assert_eq!(
        seen.origin, BOUNDARY_POLICY,
        "the configured policy is the one every empty record below is read against"
    );
}

#[test]
fn mapper_boundary_stops_at_parser_and_protocol_commitment() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let abandon = Abandon::new();
            let plain = common::spawn_server(mapper_boundary_router(&journal, &abandon));

            // The readiness probe sends a head Hyper refuses below Camber's
            // mapper boundary — the very claim the malformed row below states —
            // so the journal starts empty.
            calibrate_mapper(plain, &journal);

            let below_hyper = malformed_request(plain);
            assert!(
                below_hyper.starts_with("HTTP/1.1 400"),
                "Hyper answers a request line it never accepted: {below_hyper}"
            );
            assert_no_mapping(
                &journal,
                "a failure below request-head admission claims no mapper execution",
            );

            // Past the gRPC handoff the response is tonic's, not Camber's: an
            // unimplemented method is reported through gRPC's own status.
            let handed_off = grpc_call(plain, "/nothing.Registered/Method");
            assert_eq!(
                handed_off.header("grpc-status"),
                Some("12"),
                "tonic reports an unimplemented method through its own contract"
            );
            assert_no_mapping(
                &journal,
                "a failure after the gRPC handoff claims no mapper execution",
            );

            assert_committed_stream_excluded(plain, &journal, &abandon);

            // The connector a paired helper would also build is deliberately not
            // used: this row connects trusting nothing, which is what makes the
            // handshake fail, so a cert-trusting client here would be dead weight
            // a reader had to rule out.
            let (cert_pem, key_pem) = common::generate_self_signed_cert();
            let server_config = common::server_tls_config(&cert_pem, &key_pem);
            // The same journal, so a TLS-side mapper entry would show here too.
            let secured =
                owned_tls_server(mapper_boundary_router(&journal, &abandon), server_config);
            failed_tls_handshake(secured.local_addr());
            assert_no_mapping(
                &journal,
                "a TLS failure before HTTP claims no mapper execution",
            );
            drop(secured);

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── An owned server's protected handshake output (4.T7) ────────────

/// The router the owned journey serves, counting real handoffs.
fn owned_ws_router(dispatched: &Arc<AtomicUsize>) -> Router {
    let mut router = Router::new();
    router.ws(SOCKET, common::counting_ws_handler(dispatched));
    router.rejection_mapper(|rejection: &Rejection, _context: &RejectionContext| {
        Ok(Response::text(426, rejection.message())?
            .with_header("Sec-WebSocket-Version", common::MAPPER_VERSION))
    })
}

/// Send one upgrade whose declared version Camber does not speak.
fn unsupported_upgrade(addr: std::net::SocketAddr) -> HttpResponse {
    let head = common::ws_upgrade_request_to("localhost", SOCKET)
        .replace("Sec-WebSocket-Version: 13", "Sec-WebSocket-Version: 8");
    let mut peer = common::start_upgrade_with(addr, &head);
    wire::read_http_response_bounded(&mut peer).expect("no answer to the upgrade request")
}

#[test]
fn owned_websocket_rejection_enforces_version_without_false_handoff() {
    common::test_runtime()
        .run(|| {
            let dispatched = Arc::new(AtomicUsize::new(0));
            let served = owned_server(owned_ws_router(&dispatched));
            let addr = served.local_addr();

            let refused = unsupported_upgrade(addr);
            assert_eq!(refused.status, 426, "the mapped status survives");
            assert_eq!(
                refused.header_values("sec-websocket-version").as_ref(),
                ["13"],
                "the refusal advertises only the version this server speaks"
            );
            assert_eq!(
                refused.text().as_ref(),
                "unsupported WebSocket version",
                "the mapper still owns the body"
            );
            assert_eq!(
                dispatched.load(Ordering::SeqCst),
                0,
                "a refused handshake never dispatches the WebSocket handler"
            );

            // The same configured service still completes a valid upgrade: the
            // refusal shaped one response, not the route.
            let mut peer = common::start_upgrade_with(
                addr,
                &common::ws_upgrade_request_to("localhost", SOCKET),
            );
            let committed = wire::read_head(&mut peer, WIRE_TIMEOUT)
                .expect("no answer to the valid upgrade request");
            let committed = String::from_utf8_lossy(&committed).into_owned();
            assert!(
                committed.starts_with("HTTP/1.1 101"),
                "a valid handshake still commits its upgrade: {committed}"
            );
            assert!(
                wire::poll_until(WIRE_TIMEOUT, || dispatched.load(Ordering::SeqCst) == 1),
                "the committed upgrade reached the WebSocket handler"
            );

            drop(peer);
            drop(served);

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── The operator's view of a refused service journey ───────────────

/// The identifier a peer sends, which is application data and not authority.
const SPOOFED_ID: &str = "ffffffffffffffffffffffffffffffff";

/// The route the operator journey drives.
const JOURNEY_PATH: &str = "/operator-journey";

/// Serve one router on an owned listener the fixture keeps.
fn owned_server(router: Router) -> wire::ReadyServer {
    owned(|listener| {
        camber::http::serve_background(listener, router)
            .expect("owned server requires a Tokio runtime")
    })
}

/// Serve one router over TLS on an owned listener the fixture keeps.
fn owned_tls_server(router: Router, config: Arc<rustls::ServerConfig>) -> wire::ReadyServer {
    owned(|listener| {
        camber::http::serve_background_tls(listener, router, config)
            .expect("owned server requires a Tokio runtime")
    })
}

/// Bind an ephemeral port and hand the listener to whichever server serves it.
///
/// The bind is the whole of what stays here; which server function takes the
/// listener is all a caller varies. The join comes from [`wire::ReadyServer`],
/// which owes it on every exit and not only the successful one: an assertion
/// between construction and teardown unwinds past a bare `cancel`-and-`await`
/// pair, leaving the listener bound and its task running for the rest of the
/// binary.
fn owned(
    serve: impl FnOnce(tokio::net::TcpListener) -> camber::http::ServerHandle,
) -> wire::ReadyServer {
    common::block_on(async move {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        wire::serve_owned(listener, serve)
    })
    .unwrap()
}

/// The policy the operator journey answers with.
///
/// It records what it was handed through the shared journal, and the shared tail
/// then names the request on its own response.
fn journey_policy(
    journal: &Journal,
) -> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
{
    let recorder = recording_mapper(journal, "journey");
    move |rejection: &Rejection, context: &RejectionContext| {
        common::naming(recorder(rejection, context), context)
    }
}

/// What one handler read about the request it was given.
type HandlerView = Arc<std::sync::Mutex<Option<Box<str>>>>;

/// Serve the journey route, reporting the identifier its handler read.
fn journey_server(journal: &Journal, handled: &HandlerView) -> wire::ReadyServer {
    let mut router = Router::new();
    let observed_by_handler = Arc::clone(handled);
    router.get(JOURNEY_PATH, move |req: &Request| {
        let mut seen = observed_by_handler
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        *seen = Some(req.request_id().as_str().into());
        std::future::ready(Err::<Response, RuntimeError>(common::two_level_failure()))
    });
    owned_server(router.rejection_mapper(journey_policy(journal)))
}

/// Assert the handler, the policy, and the response all name one request.
fn assert_one_identity(journal: &Journal, handled: &HandlerView, request_id: &str) {
    assert_ne!(
        request_id, SPOOFED_ID,
        "an inbound identifier is application data, not Camber authority"
    );
    let mapped = only(journal, "operator journey");
    assert_eq!(
        mapped.request_id.as_ref(),
        request_id,
        "policy read the identifier the response carries"
    );
    let in_handler = handled
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone()
        .expect("the handler ran and named its request");
    assert_eq!(
        in_handler.as_ref(),
        request_id,
        "the handler read the identifier the response carries"
    );
}

/// Assert the two operator events agree on the request and the sent status.
fn assert_one_outcome(captured: &common::TraceCapture, request_id: &str) {
    let events = captured.events();
    let identity = format!("request_id={request_id}");
    let label = "operator journey";
    let target = format!("raw_path={JOURNEY_PATH}");
    let rejected = only_event(&events, common::REJECTION_MESSAGE, label);
    assert_field_value(rejected, "status", "500", label);
    assert_fields(
        rejected,
        &[
            &identity,
            "kind=internal_service",
            &target,
            common::MIDDLE_CAUSE,
            common::ROOT_CAUSE,
        ],
        label,
    );
    assert_message_is_fixed(rejected, common::REJECTION_MESSAGE, label);
    let completed = only_event(&events, common::COMPLETION_MESSAGE, label);
    assert_field_value(completed, "status", "500", label);
    assert_fields(completed, &[&identity], label);
}

#[test]
fn operator_rejection_journey_correlates_one_redacted_outcome() {
    let captured = common::capture_events(JOURNEY_PATH);

    common::test_runtime()
        .with_tracing()
        .run(|| {
            let journal = Journal::default();
            let handled: HandlerView = Arc::new(std::sync::Mutex::new(None));
            let served = journey_server(&journal, &handled);

            let response = wire::send(
                served.local_addr(),
                "GET",
                JOURNEY_PATH,
                &[(common::REQUEST_ID_HEADER, SPOOFED_ID)],
                b"",
            );
            assert_eq!(response.status, 500, "the redacted wire status");
            assert_eq!(
                response.text().as_ref(),
                common::REDACTED_BODY,
                "the redacted wire body"
            );

            common::assert_no_private_text(
                &response,
                &[common::ROOT_CAUSE, common::MIDDLE_CAUSE, "io error"],
                "operator journey",
            );

            let request_id = common::request_id_of(&response, "operator journey");
            assert_one_identity(&journal, &handled, &request_id);
            assert_one_outcome(&captured, &request_id);

            drop(served);
            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── The counters that journey leaves behind ────────────────────────

/// The status this journey's policy answers a refusal it can answer with.
const JOURNEY_STATUS: u16 = 451;

/// The body that policy writes.
const JOURNEY_BODY: &str = "unavailable for legal reasons";

/// The route whose refusal that policy cannot answer at all.
const METRICS_FALLBACK_PATH: &str = "/metrics-journey-fallback";

/// The WebSocket route this journey offers, and refuses an upgrade to.
const METRICS_SOCKET_PATH: &str = "/metrics-journey-socket";

/// The handshake headers for an upgrade this service cannot complete.
///
/// The version is the refusal; the key and the upgrade pair are what make this
/// a handshake at all, so the row reaches the WebSocket gate rather than an
/// ordinary route and is counted under the category that gate raises.
const UNSPOKEN_UPGRADE: [(&str, &str); 4] = [
    ("Connection", "Upgrade"),
    ("Upgrade", "websocket"),
    ("Sec-WebSocket-Key", common::WS_KEY),
    ("Sec-WebSocket-Version", "8"),
];

/// One measured journey, and the outcome its peer was actually given.
struct OutcomeRow {
    label: &'static str,
    method: &'static str,
    path: &'static str,
    headers: &'static [(&'static str, &'static str)],
    kind: &'static str,
    status: u16,
    body: &'static str,
}

const OUTCOME_ROWS: [OutcomeRow; 4] = [
    OutcomeRow {
        label: "custom mapped status",
        method: "GET",
        path: "/metrics-journey-custom",
        headers: &[],
        kind: "internal_service",
        status: JOURNEY_STATUS,
        body: JOURNEY_BODY,
    },
    OutcomeRow {
        label: "fixed fallback",
        method: "GET",
        path: METRICS_FALLBACK_PATH,
        headers: &[],
        kind: "internal_service",
        status: 500,
        body: common::REDACTED_BODY,
    },
    OutcomeRow {
        label: "head suppression",
        method: "HEAD",
        path: "/metrics-journey-head",
        headers: &[],
        kind: "internal_service",
        status: JOURNEY_STATUS,
        body: "",
    },
    // A refusal the transport raised rather than a handler: it never reaches an
    // application frame, and the counter still has to see it under its own
    // category and the status policy answered the handshake with.
    OutcomeRow {
        label: "refused handshake",
        method: "GET",
        path: METRICS_SOCKET_PATH,
        headers: &UNSPOKEN_UPGRADE,
        kind: "websocket_handshake",
        status: JOURNEY_STATUS,
        body: JOURNEY_BODY,
    },
];

/// The policy the measured journey answers with, and the one refusal it cannot.
fn outcome_policy()
-> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
{
    |_rejection: &Rejection, context: &RejectionContext| match context.raw_path() {
        METRICS_FALLBACK_PATH => Err(RuntimeError::Http("this policy cannot answer".into())),
        _ => Response::text(JOURNEY_STATUS, JOURNEY_BODY),
    }
}

/// Register one failing route per measured row.
///
/// The handshake row's route is a real WebSocket route: the category it is
/// counted under is raised by the gate that would have completed the upgrade,
/// so the refusal has to be a refusal of something this service offers.
fn register_outcome_routes(router: &mut Router) {
    for row in OUTCOME_ROWS.iter().filter(|row| row.headers.is_empty()) {
        router.get(row.path, |_req: &Request| {
            std::future::ready(Err::<Response, RuntimeError>(common::two_level_failure()))
        });
    }
    router.ws(METRICS_SOCKET_PATH, |_req: &Request, _ws: WsConn| Ok(()));
}

/// A table with no rows would drive no journey and still report success.
///
/// Stated as a compile-time claim, which is the only honest place for it: the
/// length is a literal, so a runtime check of it could never have failed. What
/// the case still asserts at runtime is that every declared row actually ran.
const _: () = assert!(!OUTCOME_ROWS.is_empty());

/// What one row's refusal must have done to both counters.
fn expected_outcome(row: &OutcomeRow) -> common::CountedOutcome<'_> {
    common::CountedOutcome {
        label: row.label,
        kind: row.kind,
        status: row.status,
        rejections: common::counted_rows(&OUTCOME_ROWS, |other| {
            other.kind == row.kind && other.status == row.status
        }),
        completions: common::counted_rows(&OUTCOME_ROWS, |other| other.status == row.status),
    }
}

#[test]
fn operator_metrics_report_the_final_rejection_outcome() {
    common::test_runtime()
        .with_metrics()
        .run(|| {
            let mut router = Router::new();
            register_outcome_routes(&mut router);
            let served = owned_server(router.rejection_mapper(outcome_policy()));
            let addr = served.local_addr();

            // Taken before the measured window, so the label check has a real
            // identifier to look for and this refusal is outside the counts.
            let named = wire::send(addr, "GET", METRICS_FALLBACK_PATH, &[], b"");
            let request_id = common::request_id_of(&named, "metrics journey");

            // The scrape is this journey's own send, not the component client's:
            // what is shared is the assert-and-parse half, which the counter
            // names, the paired read, and the `200` check all belong to.
            let scrape = || wire::send(addr, "GET", "/metrics", &[], b"");
            let before = common::rejection_counters(scrape);
            let mut rows = 0_usize;
            for row in &OUTCOME_ROWS {
                let response = wire::send(addr, row.method, row.path, row.headers, b"");
                assert_eq!(response.status, row.status, "{}: wire status", row.label);
                assert_eq!(
                    response.text().as_ref(),
                    row.body,
                    "{}: wire body",
                    row.label
                );
                rows += 1;
            }
            assert_eq!(rows, OUTCOME_ROWS.len(), "every measured row ran");

            let after = common::rejection_counters(scrape);
            common::assert_bounded_rejection_labels(
                &after.rejections,
                &common::forbidden_values(
                    &request_id,
                    &[JOURNEY_BODY, common::ROOT_CAUSE, common::MIDDLE_CAUSE],
                    OUTCOME_ROWS.iter().map(|row| row.path),
                ),
            );
            for row in &OUTCOME_ROWS {
                common::assert_counted(&before, &after, &expected_outcome(row));
            }

            drop(served);
            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}
