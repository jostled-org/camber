//! Where a refusal is mapped, relative to the middleware around it.
//!
//! A refusal produced after a child router was selected is that child's
//! terminal: it maps at the terminal and unwinds through the frames already
//! entered outside it. A Host value that selects no child has no child chain to
//! unwind through, so none runs. The same rule governs a middleware frame's own
//! failure: it maps where it is produced, and only the frames already entered
//! outside it see the mapped response.

use crate::http as wire;
use crate::http::PathSpec;
use crate::rejection_support::{
    self as observed, COLLAPSED_STATUS, Journal, MALFORMED_JSON, Ticket, Trail,
    UNREPRESENTABLE_HEADER, mark, take,
};
use crate::runtime_support as common;

use camber::http::{
    HostRouter, Next, Rejection, RejectionContext, RejectionKind, RejectionProtocol, Request,
    Response, Router, validate,
};
use camber::{Resource, RuntimeError, runtime};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// Register one middleware frame that records its entry and its unwind.
fn record_frame(router: &mut Router, trail: &Trail, enter: &'static str, exit: &'static str) {
    let trail = Arc::clone(trail);
    router.use_middleware(move |req: &Request, next: Next| {
        let trail = Arc::clone(&trail);
        mark(&trail, enter);
        let inner = next.call(req);
        async move {
            let response = inner.await;
            mark(&trail, exit);
            response
        }
    });
}

/// The host-routed fixture every ordering row shares.
///
/// The child registers `GET /items/:id` only, so a wrong or unsupported method
/// on that path is a terminal of the child's own chain rather than a miss.
fn ordering_hosts(trail: &Trail) -> HostRouter {
    let mut child = Router::new();
    record_frame(&mut child, trail, "outer:enter", "outer:exit");
    record_frame(&mut child, trail, "inner:enter", "inner:exit");
    child.get("/items/:id", |_req: &Request| async {
        Response::text(200, "item")
    });

    let mapper_trail = Arc::clone(trail);
    let child =
        child.rejection_mapper(move |rejection: &Rejection, _context: &RejectionContext| {
            mark(&mapper_trail, "child:mapper");
            Response::text(rejection.status(), rejection.message())
        });

    let host_trail = Arc::clone(trail);
    let mut hosts = HostRouter::new().rejection_mapper(
        move |rejection: &Rejection, _context: &RejectionContext| {
            mark(&host_trail, "host:mapper");
            Response::text(rejection.status(), rejection.message())
        },
    );
    hosts.add("app.test", child);
    hosts
}

/// What one ordering row sends, and the markers it must produce in order.
struct OrderingRow {
    label: &'static str,
    method: &'static str,
    host: &'static str,
    path: PathSpec,
    status: u16,
    trail: &'static [&'static str],
}

/// The markers a terminal inside the selected child's chain produces.
const CHILD_TERMINAL: &[&str] = &[
    "outer:enter",
    "inner:enter",
    "child:mapper",
    "inner:exit",
    "outer:exit",
];

/// The markers a Host that selects no child produces.
const HOST_TERMINAL: &[&str] = &["host:mapper"];

/// The target a row asks for when it means to reach no route at all.
const ORDERING_MISS: &str = "/nothing/here";

/// The routed path a row asks for when it means to reach the registered route.
const SELECTED_PATH: &str = "/items/7";

const ORDERING_ROWS: [OrderingRow; 6] = [
    OrderingRow {
        label: "malformed host",
        method: "GET",
        host: "bad host",
        path: PathSpec::Exact(ORDERING_MISS),
        status: 400,
        trail: HOST_TERMINAL,
    },
    OrderingRow {
        label: "unmatched host",
        method: "GET",
        host: "nowhere.test",
        path: PathSpec::Exact(ORDERING_MISS),
        status: 404,
        trail: HOST_TERMINAL,
    },
    OrderingRow {
        label: "URI depth",
        method: "GET",
        host: "app.test",
        path: PathSpec::Deep,
        status: 414,
        trail: CHILD_TERMINAL,
    },
    OrderingRow {
        label: "unmatched path",
        method: "GET",
        host: "app.test",
        path: PathSpec::Exact(ORDERING_MISS),
        status: 404,
        trail: CHILD_TERMINAL,
    },
    OrderingRow {
        label: "wrong method",
        method: "DELETE",
        host: "app.test",
        path: PathSpec::Exact(SELECTED_PATH),
        status: 405,
        trail: CHILD_TERMINAL,
    },
    OrderingRow {
        label: "unsupported method",
        method: "PROPFIND",
        host: "app.test",
        path: PathSpec::Exact(SELECTED_PATH),
        status: 405,
        trail: CHILD_TERMINAL,
    },
];

/// A table with no rows would drive no request and still report success.
///
/// Stated as a compile-time claim, which is the only honest place for it: the
/// length is a literal, so a runtime check could never have failed.
const _: () = assert!(!ORDERING_ROWS.is_empty());

#[test]
fn routing_stage_rejections_follow_declared_middleware_order() {
    common::test_runtime()
        .run(|| {
            let trail = Trail::default();
            let addr = common::spawn_host_server(ordering_hosts(&trail));
            take(&trail);

            let deep = wire::overdeep_path();
            for row in &ORDERING_ROWS {
                let path = row.path.resolve(&deep);
                let response = wire::send_to_host(addr, row.method, path, row.host);
                let label = row.label;

                assert_eq!(response.status, row.status, "{label}: wire status");
                assert_eq!(
                    take(&trail).as_ref(),
                    row.trail,
                    "{label}: mapping and middleware order"
                );
            }

            let accepted = wire::send_to_host(addr, "GET", SELECTED_PATH, "app.test");
            assert_eq!(accepted.status, 200);
            assert_eq!(
                take(&trail).as_ref(),
                ["outer:enter", "inner:enter", "inner:exit", "outer:exit"].as_slice(),
                "an accepted request runs the same chain with no mapper in it"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── Buffered producers around the middleware chain ─────────────────

/// A JSON body the validated route accepts.
const VALID_JSON: &str = "{\"id\":\"ticket-1\"}";

/// The declared client-safe message a middleware refusal carries.
const DECLARED_MESSAGE: &str = "tenant header is required";

/// The private cause a faulted middleware frame reports to operators only.
const PRIVATE_MIDDLEWARE_CAUSE: &str = "policy store unreachable";

/// The body the counting terminal answers with when it is reached.
const HANDLED_BODY: &str = "handled";

/// Send one request under the representation every JSON case here declares.
///
/// The send itself, its bound, and the sentence a send that never completed
/// fails with are `wire::send`'s. What is stated here is the one thing these
/// cases vary: the body is JSON and it says so, in one place rather than at
/// every call.
fn post_json(addr: SocketAddr, path: &str, body: &str) -> wire::HttpResponse {
    wire::send(
        addr,
        "POST",
        path,
        &[("Content-Type", "application/json")],
        body.as_bytes(),
    )
}

/// Register the middleware frame that fails with the error it is given.
fn failing_frame(router: &mut Router, error: fn() -> RuntimeError) {
    router.use_middleware(move |_req: &Request, _next: Next| {
        let refused = error();
        async move { Err::<Response, RuntimeError>(refused) }
    });
}

#[test]
fn validation_and_invalid_head_map_once_around_the_chain() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let handled = Arc::new(AtomicUsize::new(0));

            let mut router = Router::new();
            router.use_middleware(validate::json::<Ticket>());
            router.post(
                "/validated",
                observed::counting_handler(&handled, HANDLED_BODY),
            );
            router.post("/unsendable", observed::unrepresentable_handler(&handled));
            let router = router.rejection_mapper(observed::recording_mapper(&journal, "router"));
            let addr = common::spawn_server(router);
            observed::drain(&journal);

            let refused = post_json(addr, "/validated", MALFORMED_JSON);
            assert_eq!(refused.status, 400, "validation failure: wire status");
            assert_eq!(refused.text().as_ref(), "malformed request body");
            let validation = observed::only(&journal, "validation failure");
            assert_eq!(validation.kind, RejectionKind::MalformedBody);
            assert_eq!(validation.status, 400);
            assert_eq!(
                handled.load(Ordering::SeqCst),
                0,
                "a validation failure never reaches the application handler"
            );

            let unsendable = post_json(addr, "/unsendable", VALID_JSON);
            assert_eq!(unsendable.status, 500, "invalid response head: wire status");
            assert_eq!(unsendable.text().as_ref(), observed::REDACTED_BODY);
            let invalid = observed::only(&journal, "invalid response head");
            assert_eq!(invalid.kind, RejectionKind::InvalidHeader);
            assert_eq!(invalid.status, 500);
            assert_eq!(
                handled.load(Ordering::SeqCst),
                1,
                "a response Camber cannot represent is produced by the handler that ran"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// What one middleware classification row produces, and the category it keeps.
///
/// `error` is the failure this row's frame raises, and `None` only for the row
/// whose failure the terminal itself produces. The frame reads it back by the
/// row's own `path`, which is also the path the row's route is registered under
/// and the path the row sends to: one string, so a lookup cannot find a
/// different row than the request reached.
///
/// `handler_calls` is what makes the frame's position load-bearing. Every row
/// whose frame refuses claims the refusal happened before the terminal, and
/// only the counter says whether it did.
///
/// `private` is the cause this row's producer holds and the peer must never be
/// shown. Only one producer here holds one, and a leak search handed nothing to
/// look for passes over every answer there is — so a row that holds nothing says
/// so, and the guard below the loop states how many rows are of each kind.
struct MiddlewareRow {
    label: &'static str,
    path: &'static str,
    error: Option<fn() -> RuntimeError>,
    kind: RejectionKind,
    status: u16,
    message: &'static str,
    handler_calls: usize,
    private: Option<&'static str>,
}

const MIDDLEWARE_ROWS: [MiddlewareRow; 4] = [
    MiddlewareRow {
        label: "middleware declared refusal",
        path: "/declared",
        error: Some(|| RuntimeError::BadRequest(DECLARED_MESSAGE.into())),
        kind: RejectionKind::Middleware,
        status: 400,
        message: DECLARED_MESSAGE,
        handler_calls: 0,
        private: None,
    },
    MiddlewareRow {
        label: "middleware draining service",
        path: "/draining",
        error: Some(|| RuntimeError::ScopeClosed),
        kind: RejectionKind::InternalService,
        status: 503,
        message: observed::UNAVAILABLE_BODY,
        handler_calls: 0,
        private: None,
    },
    MiddlewareRow {
        label: "middleware fault",
        path: "/faulted",
        error: Some(|| RuntimeError::Http(PRIVATE_MIDDLEWARE_CAUSE.into())),
        kind: RejectionKind::Middleware,
        status: 500,
        message: observed::REDACTED_BODY,
        handler_calls: 0,
        private: Some(PRIVATE_MIDDLEWARE_CAUSE),
    },
    MiddlewareRow {
        label: "invalid response head",
        path: "/unsendable",
        error: None,
        kind: RejectionKind::InvalidHeader,
        status: 500,
        message: observed::REDACTED_BODY,
        handler_calls: 1,
        private: None,
    },
];

/// The same claim the ordering table owes, for the table below it.
const _: () = assert!(!MIDDLEWARE_ROWS.is_empty());

/// The failure the frame raises for the row that declares this path.
///
/// A path no row declares gets no frame. No request the table drives asks for
/// one, and a probe of the fixture server that did would find no route either.
/// The fall-through cannot reach a declared row: a route is registered under
/// the same `path` its row is found by and the same one its request is sent to,
/// so a request that reached a row's route reached that row here.
fn declared_failure(path: &str) -> Option<RuntimeError> {
    MIDDLEWARE_ROWS
        .iter()
        .find(|row| row.path == path)
        .and_then(|row| row.error)
        .map(|error| error())
}

/// Serve every classification row from one router.
///
/// One frame in front of every row's terminal, raising the failure the requested
/// path declares. The rows already differ by path, so a listener per row proved
/// nothing the path does not already separate — and four servers for four
/// adjacent edge cases is what the testing strategy asks a suite not to start.
fn classification_addr(journal: &Journal, handled: &Arc<AtomicUsize>) -> SocketAddr {
    let mut router = Router::new();
    router.use_middleware(|req: &Request, next: Next| {
        let refused = declared_failure(req.path());
        // Resolved before the future, because a frame that refuses must not
        // enter the rest of the chain at all: the counter under every refusing
        // row is what says the refusal preceded the terminal.
        let inner = refused.is_none().then(|| next.call(req));
        async move {
            match refused {
                Some(error) => Err(error),
                None => Ok(inner
                    .expect("a path with no declared failure entered the rest of the chain")
                    .await),
            }
        }
    });
    for row in &MIDDLEWARE_ROWS {
        router.post(row.path, observed::unrepresentable_handler(handled));
    }
    common::spawn_server(router.rejection_mapper(observed::collapsing_mapper(
        journal,
        "router",
        COLLAPSED_STATUS,
    )))
}

#[test]
fn classification_table_keeps_middleware_kinds_distinct() {
    common::test_runtime()
        .run(|| {
            let journal = Journal::default();
            let handled = Arc::new(AtomicUsize::new(0));
            let addr = classification_addr(&journal, &handled);
            observed::drain(&journal);

            for row in &MIDDLEWARE_ROWS {
                let response = post_json(addr, row.path, VALID_JSON);
                let label = row.label;

                observed::assert_collapsed(
                    &journal,
                    &response,
                    label,
                    &observed::Collapsed {
                        kind: row.kind,
                        status: row.status,
                        message: row.message,
                    },
                );
                if let Some(private) = row.private {
                    observed::assert_no_private_text(&response, &[private], label);
                }
                // Taken rather than read, so the count a row asserts is the
                // change its own request made to the one shared terminal.
                assert_eq!(
                    handled.swap(0, Ordering::SeqCst),
                    row.handler_calls,
                    "{label}: whether the refusal preceded the terminal"
                );
            }

            // The leak search above is conditional, so the condition owes an
            // account of itself: a table where no row held a private cause would
            // search for nothing and report success, and one where every row
            // claimed the same cause would be searching three answers that never
            // carried it.
            let holding = MIDDLEWARE_ROWS
                .iter()
                .filter(|row| row.private.is_some())
                .count();
            assert_eq!(
                holding, 1,
                "one producer here holds a private cause, and its row searched for it"
            );
            assert!(
                holding < MIDDLEWARE_ROWS.len(),
                "the rows whose producers hold nothing private search for nothing"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── Ordinary unwind order ──────────────────────────────────────────

/// The path every ordering row below is driven through.
const UNWIND_PATH: &str = "/ordered";

/// What the inner middleware frame of one unwind row does.
#[derive(Clone, Copy)]
enum InnerFrame {
    /// Enter, call the rest of the chain, and record the unwind.
    Pass,
    /// Enter and fail, so this frame is where the refusal is produced.
    Fail,
    /// Enter and answer deliberately, without calling the rest of the chain.
    Answer,
    /// Validate the body before the rest of the chain runs.
    Validate,
}

/// What the route one unwind row reaches answers with.
#[derive(Clone, Copy)]
enum Route {
    /// A handler that answers normally.
    Handled,
    /// A handler that fails.
    Faulted,
    /// A handler whose response head Hyper cannot carry.
    Unsendable,
}

/// A mapper that records only that it ran, in the trail's own order.
///
/// Two fixtures place policy in the trail this way and answer with the safe
/// defaults; the marking itself is the harness's.
fn marking_mapper(
    trail: &Trail,
) -> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
{
    observed::marking(
        trail,
        "mapper",
        |rejection: &Rejection, _context: &RejectionContext| {
            Response::text(rejection.status(), rejection.message())
        },
    )
}

/// Register the inner frame one unwind row declares.
fn register_inner(router: &mut Router, trail: &Trail, inner: InnerFrame) {
    match inner {
        InnerFrame::Pass => record_frame(router, trail, "inner:enter", "inner:exit"),
        InnerFrame::Fail => {
            let trail = Arc::clone(trail);
            router.use_middleware(move |_req: &Request, _next: Next| {
                mark(&trail, "inner:enter");
                async {
                    Err::<Response, RuntimeError>(RuntimeError::Http(
                        PRIVATE_MIDDLEWARE_CAUSE.into(),
                    ))
                }
            });
        }
        InnerFrame::Answer => {
            let trail = Arc::clone(trail);
            router.use_middleware(move |_req: &Request, _next: Next| {
                mark(&trail, "inner:enter");
                async { Response::text(503, "intentional middleware response") }
            });
        }
        InnerFrame::Validate => router.use_middleware(validate::json::<Ticket>()),
    }
}

/// Register the route one unwind row reaches.
fn register_route(router: &mut Router, route: Route, handled: &Arc<AtomicUsize>) {
    match route {
        Route::Handled => router.post(
            UNWIND_PATH,
            observed::counting_handler(handled, HANDLED_BODY),
        ),
        Route::Faulted => {
            let handled = Arc::clone(handled);
            router.post(UNWIND_PATH, move |_req: &Request| {
                handled.fetch_add(1, Ordering::SeqCst);
                std::future::ready(Err::<Response, RuntimeError>(RuntimeError::Http(
                    PRIVATE_MIDDLEWARE_CAUSE.into(),
                )))
            });
        }
        Route::Unsendable => router.post(UNWIND_PATH, observed::unrepresentable_handler(handled)),
    }
}

/// What one unwind row runs, and the order its participants must run in.
struct UnwindRow {
    label: &'static str,
    inner: InnerFrame,
    route: Route,
    body: &'static str,
    status: u16,
    handler_calls: usize,
    trail: &'static [&'static str],
}

const UNWIND_ROWS: [UnwindRow; 5] = [
    UnwindRow {
        label: "validation failure",
        inner: InnerFrame::Validate,
        route: Route::Handled,
        body: MALFORMED_JSON,
        status: 400,
        handler_calls: 0,
        trail: &["outer:enter", "mapper", "outer:exit"],
    },
    UnwindRow {
        label: "inner middleware error",
        inner: InnerFrame::Fail,
        route: Route::Handled,
        body: VALID_JSON,
        status: 500,
        handler_calls: 0,
        trail: &["outer:enter", "inner:enter", "mapper", "outer:exit"],
    },
    UnwindRow {
        label: "handler error",
        inner: InnerFrame::Pass,
        route: Route::Faulted,
        body: VALID_JSON,
        status: 500,
        handler_calls: 1,
        trail: &[
            "outer:enter",
            "inner:enter",
            "mapper",
            "inner:exit",
            "outer:exit",
        ],
    },
    UnwindRow {
        label: "intentional middleware response",
        inner: InnerFrame::Answer,
        route: Route::Handled,
        body: VALID_JSON,
        status: 503,
        handler_calls: 0,
        trail: &["outer:enter", "inner:enter", "outer:exit"],
    },
    UnwindRow {
        label: "post-unwind invalid response",
        inner: InnerFrame::Pass,
        route: Route::Unsendable,
        body: VALID_JSON,
        status: 500,
        handler_calls: 1,
        trail: &[
            "outer:enter",
            "inner:enter",
            "inner:exit",
            "outer:exit",
            "mapper",
        ],
    },
];

/// The same claim the two tables above owe, for the unwind table.
const _: () = assert!(!UNWIND_ROWS.is_empty());

/// Serve one unwind row behind the frames it declares.
fn unwind_addr(trail: &Trail, row: &UnwindRow, handled: &Arc<AtomicUsize>) -> SocketAddr {
    let mut router = Router::new();
    record_frame(&mut router, trail, "outer:enter", "outer:exit");
    register_inner(&mut router, trail, row.inner);
    register_route(&mut router, row.route, handled);
    common::spawn_server(router.rejection_mapper(marking_mapper(trail)))
}

/// A resource that is always well, so `/health` exists to be routed to.
struct AlwaysHealthy;

impl Resource for AlwaysHealthy {
    fn name(&self) -> &str {
        "fixture"
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
}

/// Serve `/health` behind one recording frame, with the bypass flag set.
fn internal_route_addr(trail: &Trail, skip: bool) -> SocketAddr {
    let mut router = Router::new();
    record_frame(&mut router, trail, "outer:enter", "outer:exit");
    common::spawn_server(
        router
            .skip_middleware_for_internal(skip)
            .rejection_mapper(marking_mapper(trail)),
    )
}

/// Serve `/health` behind a frame that refuses it, without the bypass.
///
/// Policy both takes its place in the trail and records what it was handed. The
/// internal-route case needs both: the trail states where policy ran relative to
/// the chain, and the observation states what policy was given. Asserting only
/// the first leaves the category free to be anything.
fn refused_internal_route_addr(trail: &Trail, journal: &Journal) -> SocketAddr {
    let mut router = Router::new();
    failing_frame(&mut router, || {
        RuntimeError::BadRequest(DECLARED_MESSAGE.into())
    });
    common::spawn_server(router.skip_middleware_for_internal(false).rejection_mapper(
        observed::marking(
            trail,
            "mapper",
            observed::recording_mapper(journal, "router"),
        ),
    ))
}

/// Assert the middleware an internal route runs through follows the flag.
fn assert_internal_route_ordering(trail: &Trail, journal: &Journal) {
    let wrapped = wire::request_with_host(
        internal_route_addr(trail, false),
        "GET",
        "/health",
        "localhost",
    )
    .expect("the wrapped internal route answered");
    assert_eq!(wrapped.status, 200, "wrapped internal route: wire status");
    assert_eq!(
        take(trail).as_ref(),
        ["outer:enter", "outer:exit"].as_slice(),
        "an internal route runs the chain when the bypass is off"
    );

    let bypassed = wire::request_with_host(
        internal_route_addr(trail, true),
        "GET",
        "/health",
        "localhost",
    )
    .expect("the bypassed internal route answered");
    assert_eq!(bypassed.status, 200, "bypassed internal route: wire status");
    assert!(
        take(trail).is_empty(),
        "an internal route runs no middleware when the bypass is on"
    );

    // Drained after the server is up rather than before it. A drain that ran
    // first would leave whatever the readiness probe reached policy with in the
    // journal, and the `only` below would read it as this request's own. Every
    // other site here drains in this order.
    let refused_addr = refused_internal_route_addr(trail, journal);
    observed::drain(journal);
    let refused = wire::request_with_host(refused_addr, "GET", "/health", "localhost")
        .expect("the refused internal route answered");
    assert_eq!(refused.status, 400, "refused internal route: wire status");
    assert_eq!(refused.text().as_ref(), DECLARED_MESSAGE);
    assert_eq!(
        take(trail).as_ref(),
        ["mapper"].as_slice(),
        "an internal route refusal still reaches the selected policy"
    );

    let seen = observed::only(journal, "refused internal route");
    assert_eq!(
        seen.kind,
        RejectionKind::Middleware,
        "refused internal route: the refusing frame's own category"
    );
    assert_eq!(seen.status, 400, "refused internal route: default status");
    assert_eq!(
        seen.message.as_ref(),
        DECLARED_MESSAGE,
        "refused internal route: safe message"
    );
    assert_eq!(
        seen.route.as_deref(),
        Some("/health"),
        "an internal route names the fixed identity it dispatches under"
    );
    assert_eq!(
        seen.protocol,
        Some(RejectionProtocol::OrdinaryHttp),
        "an internal route establishes its dispatch class"
    );
}

#[test]
fn ordinary_rejection_unwinds_only_entered_middleware_frames() {
    common::test_runtime()
        .resource(AlwaysHealthy)
        .run(|| {
            let trail = Trail::default();
            let journal = Journal::default();

            for row in &UNWIND_ROWS {
                let handled = Arc::new(AtomicUsize::new(0));
                let addr = unwind_addr(&trail, row, &handled);
                take(&trail);

                let response = post_json(addr, UNWIND_PATH, row.body);
                let label = row.label;

                assert_eq!(response.status, row.status, "{label}: wire status");
                assert_eq!(
                    take(&trail).as_ref(),
                    row.trail,
                    "{label}: mapping and unwind order"
                );
                assert_eq!(
                    handled.load(Ordering::SeqCst),
                    row.handler_calls,
                    "{label}: handler entry"
                );
            }

            assert_internal_route_ordering(&trail, &journal);

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// What one profiling failure row configures and must record around mapping.
#[cfg(feature = "profiling")]
struct InternalFailureRow {
    label: &'static str,
    skip_middleware: bool,
    trail: &'static [&'static str],
}

#[cfg(feature = "profiling")]
const INTERNAL_FAILURE_ROWS: [InternalFailureRow; 2] = [
    InternalFailureRow {
        label: "wrapped internal execution failure",
        skip_middleware: false,
        trail: &["outer:enter", "mapper", "outer:exit"],
    },
    InternalFailureRow {
        label: "bypassed internal execution failure",
        skip_middleware: true,
        trail: &["mapper"],
    },
];

/// The same claim the three tables above owe, for the profiling table.
#[cfg(feature = "profiling")]
const _: () = assert!(!INTERNAL_FAILURE_ROWS.is_empty());

/// Serve the profiling route behind one frame with the declared bypass mode.
///
/// Policy takes its place in the trail and records what it was handed, while
/// collapsing every category onto one wire status: what the journal carries is
/// then the producer's own classification rather than something a reader could
/// have re-derived from the response.
#[cfg(feature = "profiling")]
fn profiling_failure_addr(trail: &Trail, journal: &Journal, skip_middleware: bool) -> SocketAddr {
    let mut router = Router::new();
    record_frame(&mut router, trail, "outer:enter", "outer:exit");
    common::spawn_server(
        router
            .skip_middleware_for_internal(skip_middleware)
            .rejection_mapper(observed::marking(
                trail,
                "mapper",
                observed::collapsing_mapper(journal, "router", COLLAPSED_STATUS),
            )),
    )
}

/// Assert the safe typed projection of one failed internal-route execution.
#[cfg(feature = "profiling")]
fn assert_internal_failure(row: &InternalFailureRow, seen: &observed::Observed) {
    let label = row.label;
    observed::assert_classification(
        seen,
        &observed::Collapsed {
            kind: RejectionKind::InternalService,
            status: 500,
            message: observed::REDACTED_BODY,
        },
        label,
    );
    assert_eq!(
        seen.route.as_deref(),
        Some("/debug/pprof/cpu"),
        "{label}: route"
    );
    assert_eq!(
        seen.protocol,
        Some(RejectionProtocol::OrdinaryHttp),
        "{label}: protocol"
    );
}

#[cfg(feature = "profiling")]
#[test]
fn internal_route_execution_failure_keeps_internal_service_classification() {
    let profiler = pprof::ProfilerGuardBuilder::default()
        .frequency(1000)
        .build()
        .expect("the fixture acquired the process profiler");

    let result = common::test_runtime().with_profiling().run(|| {
        let trail = Trail::default();
        let journal = Journal::default();
        for row in &INTERNAL_FAILURE_ROWS {
            let addr = profiling_failure_addr(&trail, &journal, row.skip_middleware);
            take(&trail);
            observed::drain(&journal);

            let response =
                wire::send_to_host(addr, "GET", "/debug/pprof/cpu?seconds=0", "localhost");
            assert_eq!(
                response.status, COLLAPSED_STATUS,
                "{}: wire status",
                row.label
            );
            assert_eq!(response.text().as_ref(), observed::REDACTED_BODY);
            assert_eq!(
                take(&trail).as_ref(),
                row.trail,
                "{}: middleware order",
                row.label
            );
            assert_internal_failure(row, &observed::only(&journal, row.label));
            observed::assert_no_private_text(&response, &["profiler"], row.label);
        }
        runtime::request_shutdown();
    });
    drop(profiler);
    result.expect("the fixture runtime ran to completion");
}

/// A mapper that counts its entries and answers with a head Hyper cannot carry.
fn unsendable_mapper(
    calls: &Arc<AtomicUsize>,
) -> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
{
    let calls = Arc::clone(calls);
    move |_rejection: &Rejection, _context: &RejectionContext| {
        calls.fetch_add(1, Ordering::SeqCst);
        Response::text(503, "mapped")
            .map(|response| response.with_header(UNREPRESENTABLE_HEADER, "present"))
    }
}

/// An ordinary response the wire cannot carry reaches policy exactly once.
fn assert_ordinary_invalid_response_maps_once() {
    let journal = Journal::default();
    let handled = Arc::new(AtomicUsize::new(0));

    let mut router = Router::new();
    router.post(UNWIND_PATH, observed::unrepresentable_handler(&handled));
    let addr = common::spawn_server(
        router.rejection_mapper(observed::recording_mapper(&journal, "router")),
    );
    observed::drain(&journal);

    let mapped = post_json(addr, UNWIND_PATH, VALID_JSON);
    assert_eq!(mapped.status, 500, "ordinary invalid head: wire status");
    assert_eq!(
        mapped.text().as_ref(),
        observed::REDACTED_BODY,
        "ordinary invalid head: wire body"
    );
    // `only` is the once: it fails unless exactly one mapper invocation was
    // recorded for this refusal.
    let seen = observed::only(&journal, "ordinary invalid head");
    assert_eq!(seen.kind, RejectionKind::InvalidHeader, "category");
    // The head Camber cannot represent is the handler's own, so the refusal is
    // only about a terminal that ran. A route that never dispatched would
    // produce the same category from nothing at all.
    assert_eq!(
        handled.load(Ordering::SeqCst),
        1,
        "the terminal that produced the unsendable head ran"
    );
}

/// A response policy itself cannot represent uses the fixed fallback instead.
fn assert_invalid_mapper_response_falls_back() {
    let handled = Arc::new(AtomicUsize::new(0));
    let entered = Arc::new(AtomicUsize::new(0));
    let mapper_calls = Arc::new(AtomicUsize::new(0));

    let mut router = Router::new();
    let counted = Arc::clone(&entered);
    router.use_middleware(move |req: &Request, next: Next| {
        counted.fetch_add(1, Ordering::SeqCst);
        next.call(req)
    });
    router.post(UNWIND_PATH, observed::unrepresentable_handler(&handled));
    let addr = common::spawn_server(router.rejection_mapper(unsendable_mapper(&mapper_calls)));

    let fell_back = post_json(addr, UNWIND_PATH, VALID_JSON);
    observed::assert_fixed_fallback(&fell_back, "mapper invalid head");
    assert_eq!(
        mapper_calls.load(Ordering::SeqCst),
        1,
        "the fixed fallback never calls a mapper again"
    );
    assert_eq!(
        entered.load(Ordering::SeqCst),
        1,
        "the fixed fallback does not re-enter middleware"
    );
    assert_eq!(
        handled.load(Ordering::SeqCst),
        1,
        "the fixed fallback does not re-enter the terminal either"
    );
}

#[test]
fn ordinary_invalid_response_maps_once_but_invalid_mapper_response_falls_back() {
    common::test_runtime()
        .run(|| {
            assert_ordinary_invalid_response_maps_once();
            assert_invalid_mapper_response_falls_back();

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}
