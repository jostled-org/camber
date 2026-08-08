//! Direct WebSocket refusals, answered before any `101` is committed.
//!
//! A handshake is a claim about the wire, so every row here writes a real
//! upgrade request onto a socket and reads the bytes the server wrote back.
//! Nothing is asserted from a value a dispatch path built.

#![cfg(feature = "ws")]

use crate::common;
use crate::common::{
    COLLAPSED_STATUS, Collapsed, Established, Journal, MAPPER_VERSION, Trail, assert_collapsed,
    assert_established, collapsing_mapper, counting_ws_handler, drain, mark, marking, take,
};
use crate::handshake::{
    Header, LOCAL_HOST, accepted, accepted_plus, accepted_with, accepted_without, handshake_request,
};

use camber::http::{
    Next, Rejection, RejectionContext, RejectionKind, RejectionProtocol, Request, Response, Router,
    WsConn,
};
use camber::{RuntimeError, runtime};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The suite every observation in this module is recorded under.
const ORIGIN: &str = "component_websocket";

/// The route every direct-handshake row asks for.
const SOCKET: &str = "/socket";

/// The headers a handshake Camber accepts carries.
fn valid_handshake() -> Box<[Header<'static>]> {
    accepted(LOCAL_HOST).into()
}

/// Send one upgrade request and read the whole answer off the socket.
fn upgrade(addr: SocketAddr, path: &str, headers: &[Header<'_>]) -> common::HttpResponse {
    let mut peer = common::start_upgrade_with(addr, &handshake_request(path, headers));
    common::read_http_response_bounded(&mut peer).expect("no answer to the upgrade request")
}

/// The valid handshake with its declared version replaced.
fn replaced_version(version: &'static str) -> Box<[Header<'static>]> {
    accepted_with(LOCAL_HOST, "Sec-WebSocket-Version", version)
}

/// A handshake whose version header names a version Camber does not speak.
fn unsupported_version() -> Box<[Header<'static>]> {
    replaced_version("8")
}

/// The valid handshake with one more header appended.
fn handshake_plus(extra: Header<'static>) -> Box<[Header<'static>]> {
    accepted_plus(LOCAL_HOST, &[extra])
}

/// The valid handshake with one header removed.
fn handshake_without(dropped: &'static str) -> Box<[Header<'static>]> {
    accepted_without(LOCAL_HOST, dropped)
}

/// Prove this server's WebSocket handler can be reached at all.
///
/// Every case here asserts that some refusal never entered the handler, and a
/// count of zero says that only if the handler was reachable to begin with: it
/// reads identically for a router that registered none, for a closure that never
/// incremented, and for a counter nothing was ever going to move. One accepted
/// handshake against the same server turns that absence into a delta.
///
/// The peer is held until the count moves rather than dropped at the `101`. The
/// handler runs on the transport the handshake committed, so a client that
/// closed the moment it read its head would leave the entry under proof racing
/// its own teardown. The wait is bounded, so a handler never entered fails here
/// instead of parking the binary.
///
/// One entry is the expectation at all three call sites: every exchange before
/// this one is refused, so the handler has not run yet.
fn assert_handler_reachable(addr: SocketAddr, path: &str, entered: &AtomicUsize) {
    let mut peer = common::start_upgrade_with(addr, &handshake_request(path, &valid_handshake()));
    let head = common::read_head(&mut peer, common::WIRE_TIMEOUT)
        .expect("no answer to the accepted upgrade request");
    let head = String::from_utf8_lossy(&head).into_owned();
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "an accepted handshake still commits its upgrade on this server: {head}"
    );
    assert!(
        common::poll_until(common::WIRE_TIMEOUT, || entered.load(Ordering::SeqCst) == 1),
        "the committed handshake reached the WebSocket handler"
    );
}

// ── The specialized matrix ─────────────────────────────────────────

/// The gate paths the matrix drives its non-handshake producers through.
const GATE_BAD: &str = "/gate/bad";
const GATE_CLOSED: &str = "/gate/closed";

/// A gate frame that refuses two declared paths and passes everything else.
fn refusing_gate(router: &mut Router) {
    router.use_middleware(|req: &Request, next: Next| {
        let refusal = match req.path() {
            GATE_BAD => Some(RuntimeError::BadRequest("gate refused".into())),
            GATE_CLOSED => Some(RuntimeError::ScopeClosed),
            _ => None,
        };
        // The rest of the chain is built only when this frame means to pass:
        // the gate terminal marks itself reached the moment it is built, so a
        // frame that built it and then refused would be reporting a gate it
        // never used.
        let passed = refusal.is_none().then(|| next.call(req));
        async move {
            match (refusal, passed) {
                (Some(error), _) => Err(error),
                (None, Some(inner)) => Ok(inner.await),
                (None, None) => Err(RuntimeError::Http("unreachable gate state".into())),
            }
        }
    });
}

/// The router every matrix row is answered by.
fn matrix_router(journal: &Journal, entries: &Arc<AtomicUsize>) -> Router {
    let mut router = Router::new();
    refusing_gate(&mut router);
    router.ws(SOCKET, counting_ws_handler(entries));
    router.ws(GATE_BAD, counting_ws_handler(entries));
    router.ws(GATE_CLOSED, counting_ws_handler(entries));
    router.rejection_mapper(collapsing_mapper(journal, ORIGIN, COLLAPSED_STATUS))
}

/// One producer the matrix drives, and the classification it must keep.
struct MatrixRow {
    label: &'static str,
    path: &'static str,
    headers: fn() -> Box<[Header<'static>]>,
    kind: RejectionKind,
    status: u16,
    message: &'static str,
}

const MATRIX_ROWS: [MatrixRow; 5] = [
    MatrixRow {
        label: "handshake syntax",
        path: SOCKET,
        headers: || handshake_without("Sec-WebSocket-Key"),
        kind: RejectionKind::WebSocketHandshake,
        status: 400,
        message: "invalid WebSocket upgrade headers",
    },
    MatrixRow {
        label: "unsupported version",
        path: SOCKET,
        headers: unsupported_version,
        kind: RejectionKind::WebSocketHandshake,
        status: 426,
        message: "unsupported WebSocket version",
    },
    MatrixRow {
        label: "cross-host origin",
        path: SOCKET,
        headers: || handshake_plus(("Origin", "http://elsewhere.test")),
        kind: RejectionKind::WebSocketHandshake,
        status: 403,
        message: "WebSocket origin rejected",
    },
    MatrixRow {
        label: "gate declared refusal",
        path: GATE_BAD,
        headers: valid_handshake,
        kind: RejectionKind::Middleware,
        status: 400,
        message: "gate refused",
    },
    MatrixRow {
        label: "gate service state",
        path: GATE_CLOSED,
        headers: valid_handshake,
        kind: RejectionKind::InternalService,
        status: 503,
        message: "service unavailable",
    },
];

/// A table with no rows would drive no producer and still report success.
///
/// Stated as a compile-time claim, which is the only honest place for it: the
/// length is a literal, so a runtime check of it could never have failed.
const _: () = assert!(!MATRIX_ROWS.is_empty());

#[test]
fn specialized_rejection_matrix_keeps_kind_and_context() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let entries = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(matrix_router(&journal, &entries));

            // Every row ran is proved by the rows themselves: `assert_collapsed`
            // drains the journal and requires exactly the one observation its row
            // caused, so a row that never reached the production path fails
            // there.
            for row in &MATRIX_ROWS {
                let answer = upgrade(addr, row.path, &(row.headers)());
                let label = row.label;

                let seen = assert_collapsed(
                    &journal,
                    &answer,
                    label,
                    &Collapsed {
                        kind: row.kind,
                        status: row.status,
                        message: row.message,
                    },
                );
                assert_established(
                    &seen,
                    &Established {
                        method: "GET",
                        raw_path: row.path,
                        route: Some(row.path),
                        protocol: Some(RejectionProtocol::WebSocket),
                        content_type: None,
                    },
                    label,
                );
                assert_eq!(
                    seen.subprotocol, None,
                    "{label}: no subprotocol was negotiated before this refusal"
                );
            }

            assert_eq!(
                entries.load(Ordering::SeqCst),
                0,
                "no refused handshake reaches the WebSocket handler"
            );

            assert_handler_reachable(addr, SOCKET, &entries);

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── Gate ordering ──────────────────────────────────────────────────

/// The path whose outer gate frame refuses before the inner one is entered.
const OUTER_REFUSES: &str = "/order/outer";

/// The path whose outer gate frame refuses after the inner chain returned.
const OUTER_REFUSES_LATE: &str = "/order/outer-late";

/// The path whose inner gate frame refuses after the outer one was entered.
const INNER_REFUSES: &str = "/order/inner";

/// The path whose gate passes and whose handshake then fails.
const POST_GATE_FAILS: &str = "/order/post-gate";

/// The path whose gate answers deliberately with its own response.
const GATE_ANSWERS: &str = "/order/answered";

/// The status a deliberate gate response carries.
const DELIBERATE_STATUS: u16 = 401;

/// What one gate frame does with the request it is handed.
///
/// Held as one value per frame so the two frames differ only in data: a second
/// frame written by hand is a second place the entry, unwind, and refusal
/// ordering could drift.
///
/// Each of the three is what a frame does on ONE path and nothing else, so a
/// frame that does none of them on any path states that as absence. An empty
/// string cannot: `""` is a path no request carries, but it reads as a stated
/// value, and the production type under test is the one that documents against
/// exactly this — an unestablished value stays absent rather than becoming an
/// empty sentinel.
#[derive(Clone, Copy)]
struct FrameBehavior {
    enter: &'static str,
    exit: &'static str,
    /// Refuse without entering the rest of the chain.
    refuse_before: Option<&'static str>,
    /// Refuse after the rest of the chain has already returned.
    refuse_after: Option<&'static str>,
    /// Answer deliberately without entering the rest of the chain.
    answer: Option<&'static str>,
}

impl FrameBehavior {
    /// Whether this frame does one of its declared things on `path`.
    ///
    /// Absence answers `false` for every path, which is what "this frame never
    /// does that" means. Written once because the three questions are one
    /// question asked of three fields.
    fn acts_on(declared: Option<&str>, path: &str) -> bool {
        declared == Some(path)
    }
}

const OUTER_FRAME: FrameBehavior = FrameBehavior {
    enter: "outer:enter",
    exit: "outer:exit",
    refuse_before: Some(OUTER_REFUSES),
    refuse_after: Some(OUTER_REFUSES_LATE),
    answer: None,
};

const INNER_FRAME: FrameBehavior = FrameBehavior {
    enter: "inner:enter",
    exit: "inner:exit",
    refuse_before: Some(INNER_REFUSES),
    refuse_after: None,
    answer: Some(GATE_ANSWERS),
};

/// Register one gate frame that records entry and unwind, and may refuse.
fn ordering_frame(router: &mut Router, trail: &Trail, behavior: FrameBehavior) {
    let trail = Arc::clone(trail);
    router.use_middleware(move |req: &Request, next: Next| {
        let trail = Arc::clone(&trail);
        mark(&trail, behavior.enter);
        let path: Box<str> = req.path().into();
        let refuses_before = FrameBehavior::acts_on(behavior.refuse_before, &path);
        let short_circuits = refuses_before || FrameBehavior::acts_on(behavior.answer, &path);
        let inner = (!short_circuits).then(|| next.call(req));
        async move {
            let response = match inner {
                None if refuses_before => {
                    return Err(RuntimeError::BadRequest("gate refused".into()));
                }
                None => Response::text(DELIBERATE_STATUS, "gated")?,
                Some(inner) => inner.await,
            };
            mark(&trail, behavior.exit);
            match FrameBehavior::acts_on(behavior.refuse_after, &path) {
                true => Err(RuntimeError::BadRequest("gate refused".into())),
                false => Ok(response),
            }
        }
    });
}

/// The router the ordering rows are answered by.
fn ordering_router(trail: &Trail, journal: &Journal, entries: &Arc<AtomicUsize>) -> Router {
    let mut router = Router::new();
    ordering_frame(&mut router, trail, OUTER_FRAME);
    ordering_frame(&mut router, trail, INNER_FRAME);
    for path in [
        OUTER_REFUSES,
        OUTER_REFUSES_LATE,
        INNER_REFUSES,
        POST_GATE_FAILS,
        GATE_ANSWERS,
    ] {
        router.ws(path, counting_ws_handler(entries));
    }
    router.rejection_mapper(marking(
        trail,
        "mapper",
        collapsing_mapper(journal, ORIGIN, COLLAPSED_STATUS),
    ))
}

/// One gate-ordering row: what it sends, and the markers it must produce.
struct OrderRow {
    label: &'static str,
    path: &'static str,
    version: &'static str,
    status: u16,
    mapped: usize,
    trail: &'static [&'static str],
}

const ORDER_ROWS: [OrderRow; 5] = [
    OrderRow {
        label: "outer gate refuses before entering the chain",
        path: OUTER_REFUSES,
        version: "13",
        status: COLLAPSED_STATUS,
        mapped: 1,
        trail: &["outer:enter", "mapper"],
    },
    OrderRow {
        label: "inner gate refuses",
        path: INNER_REFUSES,
        version: "13",
        status: COLLAPSED_STATUS,
        mapped: 1,
        trail: &["outer:enter", "inner:enter", "mapper", "outer:exit"],
    },
    OrderRow {
        label: "outer gate refuses after the chain returned",
        path: OUTER_REFUSES_LATE,
        version: "13",
        status: COLLAPSED_STATUS,
        mapped: 1,
        trail: &[
            "outer:enter",
            "inner:enter",
            "inner:exit",
            "outer:exit",
            "mapper",
        ],
    },
    OrderRow {
        label: "handshake fails after a completed gate",
        path: POST_GATE_FAILS,
        version: "8",
        status: COLLAPSED_STATUS,
        mapped: 1,
        trail: &[
            "outer:enter",
            "inner:enter",
            "inner:exit",
            "outer:exit",
            "mapper",
        ],
    },
    OrderRow {
        label: "gate answers deliberately",
        path: GATE_ANSWERS,
        version: "13",
        status: DELIBERATE_STATUS,
        mapped: 0,
        trail: &["outer:enter", "inner:enter", "inner:exit", "outer:exit"],
    },
];

/// A table with no rows would drive no gate and still report success.
const _: () = assert!(!ORDER_ROWS.is_empty());

#[test]
fn specialized_gate_rejections_follow_declared_stage_order() {
    common::test_runtime()
        .run(|| {
            let trail: Trail = Arc::new(Mutex::new(Vec::new()));
            let journal = Journal::default();
            let entries = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(ordering_router(&trail, &journal, &entries));

            // Every row ran is proved by the trail each row takes: a row that
            // never reached the production path takes an empty trail and fails
            // its own stage-order assertion.
            for row in &ORDER_ROWS {
                let answer = upgrade(addr, row.path, &replaced_version(row.version));
                let label = row.label;
                assert_eq!(answer.status, row.status, "{label}: wire status");
                assert_eq!(take(&trail).as_ref(), row.trail, "{label}: stage order");
                assert_eq!(
                    drain(&journal).len(),
                    row.mapped,
                    "{label}: mapper invocations"
                );
            }

            assert_eq!(
                entries.load(Ordering::SeqCst),
                0,
                "no gated handshake reaches the WebSocket handler"
            );

            // Driven on the one path both gates pass, so the handshake commits and
            // the count the rows above left at zero moves. The marks are written
            // while the chain unwinds, which is before the head this reads, so the
            // trail below is complete by the time it is taken.
            assert_handler_reachable(addr, POST_GATE_FAILS, &entries);
            assert_eq!(
                take(&trail).as_ref(),
                ["outer:enter", "inner:enter", "inner:exit", "outer:exit"].as_slice(),
                "a committed handshake runs both gates and reaches no policy"
            );
            assert!(
                drain(&journal).is_empty(),
                "a committed handshake invokes no mapper"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── Protected version, and no false handoff ────────────────────────

/// The path whose policy answers a version refusal without naming a version.
const SILENT: &str = "/silent";

/// A router whose policy answers a version refusal with values to correct.
///
/// Only [`SILENT`] leaves the header out. Every other row offers a conflicting
/// value, so the two branches enforcement has — overwrite a value that is
/// there, insert one that is not — are each driven by a row of their own.
fn version_router(entries: &Arc<AtomicUsize>, calls: &Arc<AtomicUsize>) -> Router {
    let mut router = Router::new();
    router.ws(SOCKET, counting_ws_handler(entries));
    router.ws("/teapot", counting_ws_handler(entries));
    router.ws("/handoff", counting_ws_handler(entries));
    router.ws(SILENT, counting_ws_handler(entries));
    let calls = Arc::clone(calls);
    router.rejection_mapper(move |rejection: &Rejection, context: &RejectionContext| {
        calls.fetch_add(1, Ordering::SeqCst);
        let (answer, offered) = match context.raw_path() {
            "/teapot" => (Response::text(418, "teapot")?, Some(MAPPER_VERSION)),
            "/handoff" => (Response::text(101, "switching")?, Some(MAPPER_VERSION)),
            SILENT => (Response::text(426, rejection.message())?, None),
            _ => (
                Response::text(426, rejection.message())?,
                Some(MAPPER_VERSION),
            ),
        };
        let answer = answer.with_header("X-Custom", "kept");
        Ok(match offered {
            Some(version) => answer.with_header("Sec-WebSocket-Version", version),
            None => answer,
        })
    })
}

/// Assert one final 426 advertises exactly the version the framework requires.
///
/// Two rows end here: one whose policy named a conflicting version and one whose
/// policy named none. Enforcement overwrites the first and inserts into the
/// second, and both owe the same answer — a single advertised value, with the
/// rest of the policy's output untouched. Stated once, because two hand-written
/// copies are two places that shared answer could come to differ.
fn assert_required_version_advertised(answer: &common::HttpResponse, label: &str) {
    assert_eq!(answer.status, 426, "{label}: a mapped 426 keeps its status");
    assert_eq!(
        answer.header_values("sec-websocket-version").as_ref(),
        ["13"],
        "{label}: a final 426 advertises exactly the required version"
    );
    assert_eq!(
        answer.header("x-custom"),
        Some("kept"),
        "{label}: correction leaves the rest of the mapper's output alone"
    );
}

#[test]
fn websocket_mapper_cannot_commit_handoff_and_required_version_wins() {
    common::test_runtime()
        .run(|| {
            let entries = Arc::new(AtomicUsize::new(0));
            let calls = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(version_router(&entries, &calls));

            let corrected = upgrade(addr, SOCKET, &unsupported_version());
            assert_required_version_advertised(&corrected, "a conflicting version");
            assert_eq!(
                corrected.text().as_ref(),
                "unsupported WebSocket version",
                "the mapper still owns the body"
            );

            let uncorrected = upgrade(addr, "/teapot", &unsupported_version());
            assert_eq!(
                uncorrected.status, 418,
                "a mapper may choose another status"
            );
            assert_eq!(
                uncorrected.header("sec-websocket-version"),
                Some(MAPPER_VERSION),
                "a final status other than 426 has no framework-required version"
            );

            // A policy that names no version at all is corrected the same way:
            // enforcement inserts the required value rather than only replacing
            // one that was already there.
            let silent = upgrade(addr, SILENT, &unsupported_version());
            assert_required_version_advertised(&silent, "no version at all");

            let refused = upgrade(addr, "/handoff", &unsupported_version());
            assert_eq!(
                refused.status, 500,
                "an informational mapped status reaches the fixed fallback"
            );
            assert_eq!(
                refused.text().as_ref(),
                "internal server error",
                "the fixed fallback body"
            );

            assert_eq!(
                calls.load(Ordering::SeqCst),
                4,
                "each refusal invoked policy exactly once"
            );
            assert_eq!(
                entries.load(Ordering::SeqCst),
                0,
                "no rejected handshake reaches the WebSocket handler"
            );

            assert_handler_reachable(addr, SOCKET, &entries);
            assert_eq!(
                calls.load(Ordering::SeqCst),
                4,
                "a committed handshake raises no refusal, so it invokes no policy"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── The boundary the mapper stops at ───────────────────────────────

/// A router whose WebSocket handler fails only after its `101` is committed.
fn post_commitment_router(calls: &Arc<AtomicUsize>) -> Router {
    let mut router = Router::new();
    router.ws(SOCKET, |_req: &Request, _ws: WsConn| {
        Err(RuntimeError::Http(
            "handler failed after the upgrade".into(),
        ))
    });
    let calls = Arc::clone(calls);
    router.rejection_mapper(move |rejection: &Rejection, _context: &RejectionContext| {
        calls.fetch_add(1, Ordering::SeqCst);
        Response::text(rejection.status(), rejection.message())
    })
}

#[test]
fn mapper_boundary_stops_at_parser_and_protocol_commitment() {
    common::test_runtime()
        .run(|| {
            let calls = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(post_commitment_router(&calls));

            // The mapper is proved reachable on this very server first. A zero
            // asserted on its own would hold just as well for a server whose
            // policy was never registered, so what the committed handshake is
            // measured against is a delta from a live mapper rather than an
            // absence nothing established.
            let refused = upgrade(addr, SOCKET, &handshake_without("Sec-WebSocket-Key"));
            assert_eq!(
                refused.status, 400,
                "a pre-commitment refusal reaches this server's policy"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "the registered mapper answered the pre-commitment refusal"
            );

            let mut peer =
                common::start_upgrade_with(addr, &handshake_request(SOCKET, &valid_handshake()));
            let head = common::read_head(&mut peer, common::WIRE_TIMEOUT)
                .expect("no answer to the committed upgrade");
            let head = String::from_utf8_lossy(&head).into_owned();
            assert!(
                head.starts_with("HTTP/1.1 101"),
                "the handshake committed its upgrade: {head}"
            );

            // The transport belongs to the WebSocket close contract now. What a
            // failing handler leaves is a closed transport, never a replacement
            // HTTP response.
            let remainder = common::read_until_closed(&mut peer, common::WIRE_TIMEOUT)
                .expect("the committed transport never ended");
            assert!(
                !String::from_utf8_lossy(&remainder).contains("HTTP/1.1"),
                "a failure after 101 never produces a second HTTP response"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "a failure after protocol commitment adds no mapper execution"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}
