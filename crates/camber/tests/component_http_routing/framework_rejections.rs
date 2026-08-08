//! Ordinary handler rejections, entered through a live router.
//!
//! Every case here drives a real accepted HTTP request through the public
//! router, so what is asserted is the response a peer is given rather than a
//! value a dispatch path happened to build.
//!
//! One spelling per kind of send: `wire::send` addresses the default authority,
//! and `wire::send_to_host` addresses a named one. A case reaches for the second
//! only where the authority is part of what it proves, so a second way to ask
//! for the same request cannot grow back.

use crate::http as wire;
use crate::http::PathSpec;
use crate::rejection_support::{
    Collapsed, Established, Journal, Observed, REDACTED_BODY, UNREPRESENTABLE_HEADER,
    assert_classification, assert_established, assert_fixed_fallback, assert_no_private_text,
    assert_request_id_shape, counting_handler, drain, only, recording_mapper, request_id_of,
};
use crate::runtime_support as common;

use camber::RuntimeError;
use camber::http::{
    HostRouter, Rejection, RejectionContext, RejectionKind, RejectionProtocol, Request, Response,
    Router,
};
use camber::runtime;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// The private text a failing mapper carries.
///
/// One literal for every failure row, so a leak is caught by searching the
/// whole raw response rather than by a per-row spelling.
const MAPPER_SECRET: &str = "mapper-private-diagnostic";

#[test]
fn ordinary_handler_rejection_maps_exactly_once() {
    common::test_runtime()
        .run(|| {
            let calls = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&calls);

            let mut router = Router::new();
            router.get("/handler-error", |_req: &Request| async {
                Err::<Response, RuntimeError>(RuntimeError::BadRequest(
                    "field id is required".into(),
                ))
            });
            router.get("/intentional", |_req: &Request| async {
                Response::text(500, "intentional application failure")
            });
            let router = router.rejection_mapper(
                move |rejection: &Rejection, _context: &RejectionContext| {
                    observed.fetch_add(1, Ordering::SeqCst);
                    Response::text(rejection.status(), rejection.message()).map(|response| {
                        response.with_header("X-Mapped-Kind", &format!("{:?}", rejection.kind()))
                    })
                },
            );

            let addr = common::spawn_server(router);

            let rejected = wire::send(addr, "GET", "/handler-error", &[], &[]);
            assert_eq!(rejected.status, 400);
            assert_eq!(rejected.text().as_ref(), "field id is required");
            assert_eq!(rejected.header("x-mapped-kind"), Some("Application"));
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "one rejection invokes one mapper exactly once"
            );

            let intentional = wire::send(addr, "GET", "/intentional", &[], &[]);
            assert_eq!(intentional.status, 500);
            assert_eq!(
                intentional.text().as_ref(),
                "intentional application failure"
            );
            assert_eq!(
                intentional.header("x-mapped-kind"),
                None,
                "a deliberate application response is not reclassified by its status"
            );
            assert_eq!(
                calls.load(Ordering::SeqCst),
                1,
                "an application response never reaches rejection policy"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// How one row's mapper fails to supply a usable response.
#[derive(Clone, Copy)]
enum MapperFailure {
    ReturnsError,
    Panics,
    ReturnsSwitchingProtocols,
    ReturnsEarlyHints,
    ReturnsUnrepresentableHeader,
}

/// A mapper that fails the way its row declares, and counts its own entries.
///
/// Every row carries [`MAPPER_SECRET`] somewhere in what it hands back, because
/// the redaction claim is asserted of every row: a row that carried the secret
/// nowhere would satisfy that claim without ever putting it at risk.
fn failing_mapper(
    failure: MapperFailure,
    calls: Arc<AtomicUsize>,
) -> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
{
    move |_rejection: &Rejection, _context: &RejectionContext| {
        calls.fetch_add(1, Ordering::SeqCst);
        match failure {
            MapperFailure::ReturnsError => Err(RuntimeError::Http(MAPPER_SECRET.into())),
            MapperFailure::Panics => panic!("{MAPPER_SECRET}"),
            MapperFailure::ReturnsSwitchingProtocols => {
                Response::empty(101).map(|response| response.with_header("X-Cause", MAPPER_SECRET))
            }
            MapperFailure::ReturnsEarlyHints => {
                Response::empty(103).map(|response| response.with_header("X-Cause", MAPPER_SECRET))
            }
            MapperFailure::ReturnsUnrepresentableHeader => Response::text(200, "mapped")
                .map(|response| response.with_header(UNREPRESENTABLE_HEADER, MAPPER_SECRET)),
        }
    }
}

/// One mapper-failure row: the way it fails, under the name it fails as.
struct FailureRow {
    label: &'static str,
    failure: MapperFailure,
}

#[test]
fn mapper_failure_matrix_uses_one_fixed_fallback_without_recursion() {
    // A table with no rows would drive no mapper and still report success.
    // Stated as a compile-time claim, which is the only honest place for it:
    // the length is a literal, so a runtime check could never have failed.
    const _: () = assert!(!FAILURES.is_empty());
    const FAILURES: [FailureRow; 5] = [
        FailureRow {
            label: "mapper returns an error",
            failure: MapperFailure::ReturnsError,
        },
        FailureRow {
            label: "mapper panics",
            failure: MapperFailure::Panics,
        },
        FailureRow {
            label: "mapper returns 101",
            failure: MapperFailure::ReturnsSwitchingProtocols,
        },
        FailureRow {
            label: "mapper returns 103",
            failure: MapperFailure::ReturnsEarlyHints,
        },
        FailureRow {
            label: "mapper returns an unrepresentable header",
            failure: MapperFailure::ReturnsUnrepresentableHeader,
        },
    ];

    common::test_runtime()
        .run(|| {
            // Every row ran is proved by the row itself: each one asserts its
            // own mapper was entered exactly once, so a row that never reached
            // the production path fails there.
            for row in &FAILURES {
                let label = row.label;
                let calls = Arc::new(AtomicUsize::new(0));

                let mut router = Router::new();
                router.get("/failing", |_req: &Request| async {
                    Err::<Response, RuntimeError>(RuntimeError::Http("handler failed".into()))
                });
                let router =
                    router.rejection_mapper(failing_mapper(row.failure, Arc::clone(&calls)));

                let addr = common::spawn_server(router);
                let response = wire::send(addr, "GET", "/failing", &[], &[]);

                assert_fixed_fallback(&response, label);
                assert_no_private_text(&response, &[MAPPER_SECRET], label);
                assert_eq!(
                    calls.load(Ordering::SeqCst),
                    1,
                    "{label}: the fallback never calls a mapper again"
                );
            }

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// The identifiers one participant in a request observed.
///
/// Sealed: an identifier is recorded as it was given and read back as it
/// stands, so nothing here ever appends to one.
type IdLog = Arc<Mutex<Vec<Box<str>>>>;

/// Record one observed identifier, reading through a poisoned lock.
fn record_id(log: &IdLog, id: &str) {
    log.lock()
        .unwrap_or_else(|error| error.into_inner())
        .push(id.into());
}

/// The one identifier a participant observed, or a failure naming what it got.
///
/// Taken rather than read, and counted rather than indexed. One request reaches
/// each participant once, so a second observation is a correlation claim about
/// two requests: reading the first and discarding the rest would let the two
/// differ and still agree. Mirrors `only`, which guards the mapper journal the
/// same way.
fn observed_id(log: &IdLog, label: &str) -> Box<str> {
    let seen = std::mem::take(&mut *log.lock().unwrap_or_else(|error| error.into_inner()));
    assert_eq!(
        seen.len(),
        1,
        "{label}: one request is observed once, not {seen:?}"
    );
    seen.into_iter()
        .next()
        .expect("one observation was just asserted")
}

/// A router whose only route fails after recording the identifier it was given.
fn identity_router(path: &str, handler_ids: &IdLog) -> Router {
    let seen = Arc::clone(handler_ids);
    let mut router = Router::new();
    router.get(path, move |req: &Request| {
        record_id(&seen, req.request_id().as_str());
        async { Err::<Response, RuntimeError>(RuntimeError::Http("handler failed".into())) }
    });
    router
}

#[test]
fn request_id_correlates_request_mapper_and_response_header() {
    const SPOOFED: &str = "spoofed-inbound-request-id";

    common::test_runtime()
        .run(|| {
            let built_in_ids = IdLog::default();
            let built_in_addr =
                common::spawn_server(identity_router("/identity-built-in", &built_in_ids));

            let built_in = wire::send(
                built_in_addr,
                "GET",
                "/identity-built-in",
                &[("X-Request-Id", SPOOFED)],
                &[],
            );
            let header_id = request_id_of(&built_in, "built-in");
            assert_eq!(
                observed_id(&built_in_ids, "the built-in handler"),
                header_id,
                "the built-in response header carries the identifier the handler saw"
            );
            assert_ne!(
                header_id.as_ref(),
                SPOOFED,
                "an inbound request-id header is application data, not Camber authority"
            );

            let handler_ids = IdLog::default();
            let mapper_ids = IdLog::default();
            let mapper_seen = Arc::clone(&mapper_ids);
            let mapped_router = identity_router("/identity-mapped", &handler_ids).rejection_mapper(
                move |_rejection: &Rejection, context: &RejectionContext| {
                    record_id(&mapper_seen, context.request_id().as_str());
                    Response::text(503, "mapped")
                },
            );
            let mapped_addr = common::spawn_server(mapped_router);

            let mapped = wire::send(
                mapped_addr,
                "GET",
                "/identity-mapped",
                &[("X-Request-Id", SPOOFED)],
                &[],
            );
            assert_eq!(mapped.status, 503);
            let mapper_id = observed_id(&mapper_ids, "the mapper");
            assert_eq!(
                observed_id(&handler_ids, "the mapped handler"),
                mapper_id,
                "the handler and the mapper name one request"
            );
            assert_ne!(mapper_id.as_ref(), SPOOFED);
            // Both halves of the shape, not the length alone: a spoofed header
            // of the right length is exactly what this case exists to refuse.
            assert_request_id_shape(Some(mapper_id.as_ref()), "the mapper");

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// The private cause the faulted producer below reports to operators only.
///
/// One literal for the producer that carries it and the search that must not
/// find it. Spelled at both ends, a producer that changed its wording would
/// leave a search looking for text nothing sends, and the leak claim would pass
/// over every response there is.
const PRIVATE_POOL_CAUSE: &str = "connection pool exhausted";

/// Every category the mapper is offered stays distinguishable from its status.
///
/// Step 1 owns the ordinary handler producers; the remaining categories are
/// proved by their own producers in later steps. Named here so the ordinary
/// path's classification cannot silently collapse into one kind.
#[test]
fn ordinary_handler_errors_keep_distinct_categories() {
    common::test_runtime()
        .run(|| {
            let seen = Arc::new(Mutex::new(Vec::<(RejectionKind, u16)>::new()));
            let observed = Arc::clone(&seen);

            let mut router = Router::new();
            router.get("/declared", |_req: &Request| async {
                Err::<Response, RuntimeError>(RuntimeError::BadRequest(
                    "id must be a number".into(),
                ))
            });
            router.get("/draining", |_req: &Request| async {
                Err::<Response, RuntimeError>(RuntimeError::ScopeClosed)
            });
            router.get("/faulted", |_req: &Request| async {
                Err::<Response, RuntimeError>(RuntimeError::Database(PRIVATE_POOL_CAUSE.into()))
            });
            let router = router.rejection_mapper(
                move |rejection: &Rejection, _context: &RejectionContext| {
                    observed
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .push((rejection.kind(), rejection.status()));
                    Response::text(499, rejection.message())
                },
            );

            let addr = common::spawn_server(router);

            let declared = wire::send(addr, "GET", "/declared", &[], &[]);
            assert_eq!(declared.status, 499);
            assert_eq!(declared.text().as_ref(), "id must be a number");

            let draining = wire::send(addr, "GET", "/draining", &[], &[]);
            assert_eq!(draining.text().as_ref(), "service unavailable");

            let faulted = wire::send(addr, "GET", "/faulted", &[], &[]);
            assert_eq!(faulted.text().as_ref(), REDACTED_BODY);
            assert_no_private_text(&faulted, &[PRIVATE_POOL_CAUSE], "faulted");

            let observed = seen.lock().unwrap_or_else(|error| error.into_inner());
            assert_eq!(
                observed.as_slice(),
                [
                    (RejectionKind::Application, 400),
                    (RejectionKind::InternalService, 503),
                    (RejectionKind::InternalService, 500),
                ],
                "each producer's category and default status survive to the mapper"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── Routing identity and mapper authority ──────────────────────────

/// What one row of the routing matrix expects.
struct RoutingRow {
    label: &'static str,
    method: &'static str,
    host: &'static str,
    path: PathSpec,
    origin: &'static str,
    kind: RejectionKind,
    status: u16,
    body: &'static str,
    route: Option<&'static str>,
    allow: Option<&'static str>,
}

/// The routed path a row asks for when it means to reach the registered route.
const MATRIX_PATH: &str = "/items/7";

/// The target a row asks for when it means to reach no route at all.
const MATRIX_MISS: &str = "/nothing-here";

const ROUTING_ROWS: [RoutingRow; 6] = [
    RoutingRow {
        label: "malformed host",
        method: "GET",
        host: "bad host",
        path: PathSpec::Exact(MATRIX_PATH),
        origin: "host",
        kind: RejectionKind::Routing,
        status: 400,
        body: "invalid host header",
        route: None,
        allow: None,
    },
    RoutingRow {
        label: "unmatched host",
        method: "GET",
        host: "nowhere.test",
        path: PathSpec::Exact(MATRIX_PATH),
        origin: "host",
        kind: RejectionKind::Routing,
        status: 404,
        body: "not found",
        route: None,
        allow: None,
    },
    RoutingRow {
        label: "URI depth",
        method: "GET",
        host: "app.test",
        path: PathSpec::Deep,
        origin: "child",
        kind: RejectionKind::Routing,
        status: 414,
        body: "URI path too deep",
        route: None,
        allow: None,
    },
    RoutingRow {
        label: "unmatched path",
        method: "GET",
        host: "app.test",
        path: PathSpec::Exact(MATRIX_MISS),
        origin: "child",
        kind: RejectionKind::Routing,
        status: 404,
        body: "not found",
        route: None,
        allow: None,
    },
    RoutingRow {
        label: "wrong method",
        method: "DELETE",
        host: "app.test",
        path: PathSpec::Exact(MATRIX_PATH),
        origin: "child",
        kind: RejectionKind::MethodSelection,
        status: 405,
        body: "method not allowed",
        route: Some("/items/:id"),
        allow: Some("GET, HEAD, POST"),
    },
    RoutingRow {
        label: "unsupported method",
        method: "PROPFIND",
        host: "app.test",
        path: PathSpec::Exact(MATRIX_PATH),
        origin: "child",
        kind: RejectionKind::MethodSelection,
        status: 405,
        body: "method not allowed",
        route: Some("/items/:id"),
        allow: Some("GET, HEAD, POST"),
    },
];

/// The body the routed handler answers with when it is reached.
const MATRIX_BODY: &str = "item";

/// The host-routed fixture every routing-matrix row shares.
///
/// `GET` and `POST` are registered on one pattern so the refused methods have a
/// non-trivial allowed set to be answered with, and `HEAD` is left unregistered
/// so the canonical set has to supply it.
fn routing_matrix_hosts(journal: &Journal, handled: &Arc<AtomicUsize>) -> HostRouter {
    let mut child = Router::new();
    child.get("/items/:id", counting_handler(handled, MATRIX_BODY));
    child.post("/items/:id", counting_handler(handled, MATRIX_BODY));
    let child = child.rejection_mapper(recording_mapper(journal, "child"));

    let mut hosts = HostRouter::new().rejection_mapper(recording_mapper(journal, "host"));
    hosts.add("app.test", child);
    hosts
}

/// Assert the wire answer one routing row is given.
fn assert_routing_answer(row: &RoutingRow, response: &wire::HttpResponse) {
    let label = row.label;
    assert_eq!(response.status, row.status, "{label}: wire status");
    assert_eq!(response.text().as_ref(), row.body, "{label}: wire body");
    assert_eq!(
        response.header("allow"),
        row.allow,
        "{label}: canonical allowed-method set"
    );
}

/// Assert the context one routing row's refusal was mapped with.
fn assert_routing_context(row: &RoutingRow, path: &str, seen: &Observed) {
    let label = row.label;
    assert_eq!(seen.origin, row.origin, "{label}: selected mapper");
    assert_classification(
        seen,
        &Collapsed {
            kind: row.kind,
            status: row.status,
            message: row.body,
        },
        label,
    );
    // A routing-stage refusal happens before any response head, so it
    // negotiates no representation and establishes no protocol.
    assert_established(
        seen,
        &Established {
            method: row.method,
            raw_path: path,
            route: row.route,
            protocol: None,
            content_type: None,
        },
        label,
    );
    assert_eq!(
        seen.allow.as_deref(),
        row.allow,
        "{label}: default allow header"
    );
}

#[test]
fn routing_rejection_matrix_has_exact_context_status_and_non_entry() {
    // A table with no rows would drive no request and still report success.
    // Stated as a compile-time claim, which is the only honest place for it:
    // the length is a literal, so a runtime check could never have failed.
    const _: () = assert!(!ROUTING_ROWS.is_empty());

    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let handled = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_host_server(routing_matrix_hosts(&journal, &handled));
            drain(&journal);

            let deep = wire::overdeep_path();
            let mut rows = 0_usize;
            for row in &ROUTING_ROWS {
                let path = row.path.resolve(&deep);
                let response = wire::send_to_host(addr, row.method, path, row.host);

                assert_routing_answer(row, &response);
                assert_routing_context(row, path, &only(&journal, row.label));
                assert_eq!(
                    handled.load(Ordering::SeqCst),
                    0,
                    "{}: no rejection reaches an application handler",
                    row.label
                );
                rows += 1;
            }
            assert_eq!(rows, ROUTING_ROWS.len(), "every declared routing row ran");

            let accepted = wire::send_to_host(addr, "GET", MATRIX_PATH, "app.test");
            assert_eq!(accepted.status, 200);
            assert_eq!(accepted.text().as_ref(), MATRIX_BODY);
            assert_eq!(
                handled.load(Ordering::SeqCst),
                1,
                "a selected route reaches its handler exactly once"
            );
            assert!(
                drain(&journal).is_empty(),
                "an accepted request invokes no mapper"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

#[test]
fn structurally_identical_routes_keep_their_registered_patterns() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();

            let mut router = Router::new();
            router.get("/orders/:order_id", |req: &Request| {
                let captured: Box<str> = req.param("order_id").unwrap_or("absent").into();
                async move { Err::<Response, RuntimeError>(RuntimeError::BadRequest(captured)) }
            });
            router.post("/orders/:reference", |req: &Request| {
                let captured: Box<str> = req.param("reference").unwrap_or("absent").into();
                async move { Err::<Response, RuntimeError>(RuntimeError::BadRequest(captured)) }
            });
            let router = router.rejection_mapper(recording_mapper(&journal, "router"));

            let addr = common::spawn_server(router);
            drain(&journal);

            wire::send(addr, "GET", "/orders/A1", &[], &[]);
            let matched_get = only(&journal, "GET /orders/A1");
            assert_eq!(matched_get.route.as_deref(), Some("/orders/:order_id"));
            assert_eq!(
                matched_get.message.as_ref(),
                "A1",
                "the GET route captured through its own parameter name"
            );

            wire::send(addr, "POST", "/orders/B2", &[], &[]);
            let matched_post = only(&journal, "POST /orders/B2");
            assert_eq!(matched_post.route.as_deref(), Some("/orders/:reference"));
            assert_eq!(
                matched_post.message.as_ref(),
                "B2",
                "the POST route captured through its own parameter name"
            );

            wire::send(addr, "DELETE", "/orders/C3", &[], &[]);
            let refused = only(&journal, "DELETE /orders/C3");
            assert_eq!(refused.kind, RejectionKind::MethodSelection);
            assert_eq!(
                refused.route.as_deref(),
                Some("/orders/:order_id"),
                "a method refusal names a registered pattern, never the received path"
            );
            assert_eq!(refused.allow.as_deref(), Some("GET, HEAD, POST"));

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// A path two branches claim is refused with what both of them serve.
///
/// A static child and a param child can claim one concrete path, each with its
/// own methods. The allowed set a refusal carries has to name the union, or the
/// peer's next request disproves it — the method the other branch serves
/// succeeds against the path it was just told does not allow it.
#[test]
fn method_refusal_allows_every_branch_that_claims_the_path() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();

            let mut router = Router::new();
            router.post("/orders/new", |_req: &Request| {
                let captured: Box<str> = "static".into();
                async move { Err::<Response, RuntimeError>(RuntimeError::BadRequest(captured)) }
            });
            router.get("/orders/:order_id", |req: &Request| {
                let captured: Box<str> = req.param("order_id").unwrap_or("absent").into();
                async move { Err::<Response, RuntimeError>(RuntimeError::BadRequest(captured)) }
            });
            let router = router.rejection_mapper(recording_mapper(&journal, "router"));

            let addr = common::spawn_server(router);
            drain(&journal);

            wire::send(addr, "POST", "/orders/new", &[], &[]);
            assert_eq!(
                only(&journal, "POST /orders/new").message.as_ref(),
                "static",
                "the static branch serves POST on this path"
            );

            wire::send(addr, "GET", "/orders/new", &[], &[]);
            assert_eq!(
                only(&journal, "GET /orders/new").message.as_ref(),
                "new",
                "the param branch serves GET on the same path"
            );

            let answered = wire::send(addr, "DELETE", "/orders/new", &[], &[]);
            let refused = only(&journal, "DELETE /orders/new");
            assert_eq!(refused.kind, RejectionKind::MethodSelection);
            assert_eq!(
                refused.route.as_deref(),
                Some("/orders/new"),
                "the refusal names the branch a match would have reached first"
            );
            assert_eq!(
                refused.allow.as_deref(),
                Some("GET, HEAD, POST"),
                "the allowed set merges every branch that claims this path"
            );
            assert_eq!(
                answered.header("allow"),
                Some("GET, HEAD, POST"),
                "the peer is told the same merged set"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// The representation an accepted response head declares.
const ACCEPTED_CONTENT_TYPE: &str = "application/health+json";

/// A response head Camber accepts establishes the content type policy is told.
///
/// The only ordinary refusal that follows an accepted head is the one Hyper
/// raises about that head, so it is the producer this establishment transition
/// belongs to.
#[test]
fn accepted_response_head_establishes_negotiated_content_type() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();

            let mut router = Router::new();
            router.get("/report", |_req: &Request| async {
                Response::text(200, "{}").map(|response| {
                    response
                        .with_content_type(ACCEPTED_CONTENT_TYPE)
                        .with_header(UNREPRESENTABLE_HEADER, "present")
                })
            });
            router.get("/no-head", |_req: &Request| async {
                Err::<Response, RuntimeError>(RuntimeError::Http("handler failed".into()))
            });
            let router = router.rejection_mapper(recording_mapper(&journal, "router"));

            let addr = common::spawn_server(router);
            drain(&journal);

            let refused = wire::send(addr, "GET", "/report", &[], &[]);
            assert_eq!(refused.status, 500, "an unrepresentable head is refused");
            assert_eq!(
                refused.text().as_ref(),
                REDACTED_BODY,
                "the peer learns nothing"
            );

            let headed = only(&journal, "/report");
            assert_eq!(headed.kind, RejectionKind::InvalidHeader, "category");
            assert_eq!(
                headed.protocol,
                Some(RejectionProtocol::OrdinaryHttp),
                "method selection established the dispatch class"
            );
            assert_eq!(
                headed.content_type.as_deref(),
                Some(ACCEPTED_CONTENT_TYPE),
                "the accepted head established the representation"
            );

            let unheaded_answer = wire::send(addr, "GET", "/no-head", &[], &[]);
            assert_eq!(
                unheaded_answer.status, 500,
                "a handler failure with no accepted head is refused"
            );
            assert_eq!(
                unheaded_answer.text().as_ref(),
                REDACTED_BODY,
                "the peer learns nothing here either"
            );

            let unheaded = only(&journal, "/no-head");
            assert_eq!(
                unheaded.protocol,
                Some(RejectionProtocol::OrdinaryHttp),
                "the dispatch class is established without a response head"
            );
            assert_eq!(
                unheaded.content_type, None,
                "a failure with no accepted head negotiates no representation"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// Each authority the precedence fixture is asked for, and the policy it selects.
///
/// A table with no rows would drive no request, and the count below it would
/// hold at zero against zero. Stated as a compile-time claim, which is the only
/// honest place for it: the length is a literal, so a runtime check could never
/// have failed.
const SELECTIONS: [(&str, &str); 3] = [
    ("child.test", "child"),
    ("plain.test", "host"),
    ("nowhere.test", "host"),
];

const _: () = assert!(!SELECTIONS.is_empty());

#[test]
fn host_child_and_builtin_mapper_precedence_is_frozen_once() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();

            let mut hosts = HostRouter::new().rejection_mapper(recording_mapper(&journal, "host"));
            hosts.add(
                "child.test",
                Router::new().rejection_mapper(recording_mapper(&journal, "child")),
            );
            hosts.add("plain.test", Router::new());
            let addr = common::spawn_host_server(hosts);
            drain(&journal);

            let mut rows = 0_usize;
            for (host, expected) in SELECTIONS {
                wire::send_to_host(addr, "GET", "/missing", host);
                assert_eq!(
                    only(&journal, host).origin,
                    expected,
                    "{host}: selected policy"
                );
                rows += 1;
            }
            assert_eq!(rows, SELECTIONS.len(), "every declared precedence row ran");

            let built_in_addr = common::spawn_server(Router::new());
            let built_in = wire::send(built_in_addr, "GET", "/missing", &[], &[]);
            assert_eq!(built_in.status, 404);
            assert_eq!(built_in.text().as_ref(), "not found");
            assert_eq!(built_in.header("content-type"), Some("text/plain"));
            request_id_of(&built_in, "built-in routing refusal");
            assert!(
                drain(&journal).is_empty(),
                "a router with no configured policy reaches no other router's mapper"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}
