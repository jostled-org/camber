//! What a refused request shows a peer, and what it shows an operator.
//!
//! The peer is told the fixed safe text and nothing else. The operator gets one
//! structured event per refusal carrying the request identity, the category,
//! the status actually sent, and the whole private cause chain — and one
//! completion event naming the same request and the same status. The counters
//! behind them carry a closed vocabulary: a category and a status, never an
//! identifier, a path, a route, a peer, or an error string.

use crate::common;

use camber::RuntimeError;
use camber::http::{Next, Rejection, RejectionContext, Request, Response, Router};
use camber::runtime;
use common::{
    COMPLETION_MESSAGE, CountedOutcome, DECLARED_MESSAGE, MIDDLE_CAUSE, REJECTION_MESSAGE,
    ROOT_CAUSE, UNREPRESENTABLE_HEADER, WIRE_TIMEOUT, assert_counted, assert_field_value,
    assert_fields, assert_message_is_fixed, assert_no_private_text, counted_rows, forbidden_values,
    one_level_failure, only_event, two_level_failure,
};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn built_in_internal_error_wire_response_is_redacted() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.get("/rejection-redaction", |_req: &Request| async {
                Err::<Response, RuntimeError>(two_level_failure())
            });

            let addr = common::spawn_server(router);
            let response =
                common::request(addr, "GET", "/rejection-redaction", &[], &[], WIRE_TIMEOUT)
                    .expect("the fixture request completed");

            assert_eq!(response.status, 500);
            assert_eq!(
                response.text().as_ref(),
                common::REDACTED_BODY,
                "a built-in 500 body is fixed text, not the error it came from"
            );
            common::request_id_of(&response, "built-in redaction");

            assert_no_private_text(
                &response,
                &[ROOT_CAUSE, MIDDLE_CAUSE, "io error"],
                "built-in redaction",
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── The operator's record of one refusal ───────────────────────────

/// One handler failure, and every private cause its diagnostic carries.
struct ChainRow {
    label: &'static str,
    path: &'static str,
    failure: fn() -> RuntimeError,
    causes: &'static [&'static str],
}

const CHAIN_ROWS: [ChainRow; 2] = [
    ChainRow {
        label: "one-level chain",
        path: "/rejection-chain-one",
        failure: one_level_failure,
        causes: &[ROOT_CAUSE],
    },
    ChainRow {
        label: "two-level chain",
        path: "/rejection-chain-two",
        failure: two_level_failure,
        causes: &[MIDDLE_CAUSE, ROOT_CAUSE],
    },
];

/// A table with no rows would drive no failure and still report success.
///
/// Stated as a compile-time claim, which is the only honest place for it: the
/// length is a literal, so a runtime check of it could never have failed.
const _: () = assert!(!CHAIN_ROWS.is_empty());

/// Register one failing route per declared chain row.
fn register_chain_routes(router: &mut Router) {
    for row in &CHAIN_ROWS {
        let failure = row.failure;
        router.get(row.path, move |_req: &Request| {
            std::future::ready(Err::<Response, RuntimeError>(failure()))
        });
    }
}

/// Assert every private cause reached the operator, and only through `cause`.
///
/// Shared by every record that carries a diagnostic: the field is where the
/// chain belongs, and a cause found before it is one that leaked into the
/// message or into a field an operator filters on.
///
/// An empty list is refused first. This is a chain assertion, and one handed no
/// chain to look for would prove only that some `cause=` field exists — a row
/// that declared no cause would pass it over any record at all.
fn assert_causes_in_field(event: &str, causes: &[&str], label: &str) {
    assert!(
        !causes.is_empty(),
        "{label}: no private cause was declared, so this record proves nothing: {event}"
    );
    let cause_at = event
        .find("cause=")
        .unwrap_or_else(|| panic!("{label}: the event carries no cause field: {event}"));
    for cause in causes {
        let at = event
            .find(cause)
            .unwrap_or_else(|| panic!("{label}: the chain lost {cause:?}: {event}"));
        assert!(
            at > cause_at,
            "{label}: {cause:?} appears outside the cause field: {event}"
        );
    }
}

/// Assert the whole private chain reached the operator, and only through `cause`.
fn assert_private_chain(row: &ChainRow, event: &str) {
    assert_causes_in_field(event, row.causes, row.label);
    assert_message_is_fixed(event, REJECTION_MESSAGE, row.label);
}

/// Assert the rejection event one chain row recorded.
fn assert_chain_event(row: &ChainRow, capture: &common::TraceCapture, request_id: &str) {
    let events = capture.events();
    let event = only_event(&events, REJECTION_MESSAGE, row.label);
    // Read at the field boundary rather than by containment, for the reason
    // `assert_field_value` states: `status=500` is a substring of
    // `default_status=500`, so a contains-check can report a value the event
    // never recorded under the name asked about. What stays below is the
    // literals no other field can spell.
    assert_field_value(event, "status", "500", row.label);
    assert_field_value(event, "request_id", request_id, row.label);
    assert_field_value(event, "kind", "internal_service", row.label);
    assert_field_value(event, "raw_path", row.path, row.label);
    assert_field_value(event, "route", row.path, row.label);
    assert_fields(
        event,
        &[
            "method=GET",
            "protocol=ordinary_http",
            "remote_addr=127.0.0.1",
        ],
        row.label,
    );
    assert_private_chain(row, event);
}

#[test]
fn rejection_event_fields_preserve_private_source_chain() {
    let captures: Box<[common::TraceCapture]> = CHAIN_ROWS
        .iter()
        .map(|row| common::capture_events(row.path))
        .collect();

    common::test_runtime()
        .with_tracing()
        .run(|| {
            let mut router = Router::new();
            register_chain_routes(&mut router);
            let addr = common::spawn_server(router);

            let mut rows = 0_usize;
            for (row, capture) in CHAIN_ROWS.iter().zip(captures.iter()) {
                let response = common::request(addr, "GET", row.path, &[], &[], WIRE_TIMEOUT)
                    .unwrap_or_else(|error| {
                        panic!("{}: the request did not complete: {error}", row.label)
                    });
                assert_eq!(response.status, 500, "{}: wire status", row.label);
                assert_eq!(
                    response.text().as_ref(),
                    common::REDACTED_BODY,
                    "{}: wire body",
                    row.label
                );
                let request_id = common::request_id_of(&response, row.label);
                assert_no_private_text(&response, row.causes, row.label);
                assert_chain_event(row, capture, &request_id);
                rows += 1;
            }
            assert_eq!(rows, CHAIN_ROWS.len(), "every declared chain row ran");

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── The status the peer was actually given ─────────────────────────

/// The status this root's policy answers a refusal with.
const CUSTOM_STATUS: u16 = 418;

/// The body a policy writes on a refusal it answers with its own status.
///
/// Both fixtures in this root answer through [`refusing_policy`], so both write
/// this one sentence; a second spelling of it would be two journeys claiming to
/// prove the same mapped answer.
const MAPPED_BODY: &str = "policy answered";

/// The route whose policy refuses to answer at all.
const FALLBACK_PATH: &str = "/completion-fallback";

/// The route whose policy answers with a response the wire cannot carry.
const DISPLACED_PATH: &str = "/completion-displaced";

/// The route whose refusal this fixture's policy answers with its own status.
const CUSTOM_PATH: &str = "/completion-custom";

/// The route asking for that same answer with a method that carries no body.
const HEAD_PATH: &str = "/completion-head";

/// Both routes above, as the list [`refusing_policy`] selects on.
///
/// Named rather than spelled twice: the table sends these and the policy selects
/// on them, and two literals would be two places the pair could drift apart.
const COMPLETION_CUSTOM_PATHS: &[&str] = &[CUSTOM_PATH, HEAD_PATH];

/// The fixed sentence a displaced mapped response is recorded under.
const DISPLACEMENT_MESSAGE: &str = "message=mapped rejection response could not be represented";

/// The needle that names this test's displaced row and no other test's.
///
/// Not the fixed sentence: the capture bus is one process-global fan-out over
/// every test running in this binary, and `rejection_metric_labels_are_bounded`
/// drives a displaced row of its own. A subscription on the sentence would be
/// handed that row's event too, and the one-event assertion would then fail on
/// an event this test never caused. The mapped status is the needle instead —
/// it is this fixture's own policy status, which nothing else in the binary
/// answers with, and every other capture in this file is likewise keyed on
/// something only its own test produces.
fn displacement_needle() -> String {
    format!("mapped_status={CUSTOM_STATUS}")
}

/// One journey, and the status its peer was given after every correction.
struct CompletionRow {
    label: &'static str,
    method: &'static str,
    path: &'static str,
    status: u16,
    body: &'static str,
    /// Whether the wire displaced the answer policy had settled on.
    displaced: bool,
}

const COMPLETION_ROWS: [CompletionRow; 4] = [
    CompletionRow {
        label: "custom mapped status",
        method: "GET",
        path: CUSTOM_PATH,
        status: CUSTOM_STATUS,
        body: MAPPED_BODY,
        displaced: false,
    },
    CompletionRow {
        label: "policy fallback",
        method: "GET",
        path: FALLBACK_PATH,
        status: 500,
        body: common::REDACTED_BODY,
        displaced: false,
    },
    CompletionRow {
        label: "head suppression",
        method: "HEAD",
        path: HEAD_PATH,
        status: CUSTOM_STATUS,
        body: "",
        displaced: false,
    },
    // Policy answered, and the wire then refused what it answered with. The
    // peer is given the fixed fallback, so `500` is the only status either
    // record may name: an operator correlating this refusal against the wire
    // must not read the status policy chose and never sent. The status policy
    // did choose is reported by its own event, which the row below asserts.
    CompletionRow {
        label: "displaced mapped response",
        method: "GET",
        path: DISPLACED_PATH,
        status: 500,
        body: common::REDACTED_BODY,
        displaced: true,
    },
];

/// A table with no rows would drive no journey and still report success.
///
/// Stated as a compile-time claim, which is the only honest place for it: the
/// length is a literal, so a runtime check of it could never have failed.
const _: () = assert!(!COMPLETION_ROWS.is_empty());

/// What [`refusing_policy`] owes the route one refusal arrived on.
///
/// The routes a policy names are runtime values, so the four answers cannot be
/// spelled as patterns over the path itself. Classified once here instead, and
/// the four answers are then written as one decision an operator reads whole
/// rather than as a chain whose last branch is whatever was left over.
enum PolicyArm {
    /// The route this policy cannot answer at all.
    Unanswerable,
    /// The route it answers with a response the wire cannot carry.
    Displaced,
    /// A route it answers with its own status.
    Mapped,
    /// Every other route, answered as its refusal's producer classified it.
    PassedThrough,
}

impl PolicyArm {
    /// Classify one refused request's path against the routes a policy names.
    ///
    /// The named routes are tried before the pass-through, in the order the
    /// policy declares them. No fixture in this root names one route twice, so
    /// today no path can reach two arms; the order is what keeps the answer
    /// fixed for a fixture that later does.
    fn of(path: &str, fallback: &str, custom: &[&str], displaced: &str) -> Self {
        match path {
            named if named == fallback => Self::Unanswerable,
            named if named == displaced => Self::Displaced,
            named if custom.iter().any(|declared| *declared == named) => Self::Mapped,
            _ => Self::PassedThrough,
        }
    }
}

/// A policy that answers the routes it names with its own status, one route it
/// cannot answer at all, and one route it answers with a response the wire
/// cannot carry.
///
/// Two fixtures in this root drive those same three arms and differ only in the
/// routes and the status they name. Two copies of the shape were two places the
/// fallback, the displacement, or the naming tail could drift from what the
/// other one proves.
///
/// `custom` is a list because one fixture answers two routes with its own
/// status — the ordinary one and the one asked for with `HEAD`. Every refusal
/// on no named route passes through under the status and message its producer
/// classified it with, which is what the counted matrix reads for its routing,
/// method, and body rows.
fn refusing_policy(
    fallback: &'static str,
    custom: &'static [&'static str],
    displaced: &'static str,
    status: u16,
) -> impl Fn(&Rejection, &RejectionContext) -> Result<Response, RuntimeError> + Send + Sync + 'static
{
    move |rejection: &Rejection, context: &RejectionContext| {
        let path = context.raw_path();
        let answered = match PolicyArm::of(path, fallback, custom, displaced) {
            PolicyArm::Unanswerable => {
                return Err(RuntimeError::Http("this policy cannot answer".into()));
            }
            PolicyArm::Displaced => Response::text(status, MAPPED_BODY)
                .map(|response| response.with_header(UNREPRESENTABLE_HEADER, "present")),
            PolicyArm::Mapped => Response::text(status, MAPPED_BODY),
            PolicyArm::PassedThrough => Response::text(rejection.status(), rejection.message()),
        };
        common::naming(answered, context)
    }
}

/// Register one failing route per declared completion row.
fn register_completion_routes(router: &mut Router) {
    for row in &COMPLETION_ROWS {
        router.get(row.path, |_req: &Request| {
            std::future::ready(Err::<Response, RuntimeError>(two_level_failure()))
        });
    }
}

/// Assert the two events one completion row recorded agree on request and status.
fn assert_completion_events(row: &CompletionRow, capture: &common::TraceCapture, request_id: &str) {
    let events = capture.events();
    let sent = row.status.to_string();

    let rejected = only_event(&events, REJECTION_MESSAGE, row.label);
    assert_field_value(rejected, "request_id", request_id, row.label);
    assert_field_value(rejected, "status", &sent, row.label);

    let completed = only_event(&events, COMPLETION_MESSAGE, row.label);
    assert_field_value(completed, "request_id", request_id, row.label);
    assert_field_value(completed, "status", &sent, row.label);
    assert_message_is_fixed(completed, COMPLETION_MESSAGE, row.label);
}

/// Assert the displaced answer named the status policy had settled on.
///
/// Captured on the mapped status rather than on the row's path: this event
/// names the request, the category, and the two statuses, and no path — an
/// operator reads it beside the rejection event through the identifier both
/// carry, which is what this asserts.
fn assert_displacement_event(capture: &common::TraceCapture, request_id: &str, label: &str) {
    let events = capture.events();
    let event = only_event(&events, DISPLACEMENT_MESSAGE, label);
    assert_field_value(event, "request_id", request_id, label);
    assert_field_value(event, "kind", "internal_service", label);
    assert_field_value(event, "mapped_status", &CUSTOM_STATUS.to_string(), label);
    assert_field_value(event, "status", "500", label);
    assert_message_is_fixed(event, DISPLACEMENT_MESSAGE, label);
}

#[test]
fn completion_event_uses_final_sent_status() {
    let captures: Box<[common::TraceCapture]> = COMPLETION_ROWS
        .iter()
        .map(|row| common::capture_events(row.path))
        .collect();
    let displacement = common::capture_events(&displacement_needle());

    common::test_runtime()
        .with_tracing()
        .run(|| {
            let mut router = Router::new();
            register_completion_routes(&mut router);
            let addr = common::spawn_server(router.rejection_mapper(refusing_policy(
                FALLBACK_PATH,
                COMPLETION_CUSTOM_PATHS,
                DISPLACED_PATH,
                CUSTOM_STATUS,
            )));

            let mut rows = 0_usize;
            for (row, capture) in COMPLETION_ROWS.iter().zip(captures.iter()) {
                let response = common::request(addr, row.method, row.path, &[], &[], WIRE_TIMEOUT)
                    .unwrap_or_else(|error| {
                        panic!("{}: the request did not complete: {error}", row.label)
                    });
                assert_eq!(response.status, row.status, "{}: wire status", row.label);
                assert_eq!(
                    response.text().as_ref(),
                    row.body,
                    "{}: wire body",
                    row.label
                );
                let request_id = common::request_id_of(&response, row.label);
                assert_completion_events(row, capture, &request_id);
                if row.displaced {
                    assert_displacement_event(&displacement, &request_id, row.label);
                }
                rows += 1;
            }
            assert_eq!(
                rows,
                COMPLETION_ROWS.len(),
                "every declared completion row ran"
            );
            // The flag gates the branch above, so its population is part of
            // what this test proves. A table whose rows all read `false` would
            // run the loop, pass every row, and never once reach
            // `assert_displacement_event` — the displacement claim would retire
            // silently rather than fail.
            assert_eq!(
                COMPLETION_ROWS.iter().filter(|row| row.displaced).count(),
                1,
                "exactly one declared row has its mapped answer displaced"
            );
            assert!(
                COMPLETION_ROWS.iter().any(|row| !row.displaced),
                "at least one declared row is answered with what policy settled on"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── The counters an operator scrapes ───────────────────────────────

/// The body limit the counted fixture configures.
const METRIC_BODY_LIMIT: usize = 1024;

/// The status the counted fixture's policy answers one route with.
const METRIC_CUSTOM_STATUS: u16 = 599;

/// The route whose refusal that policy answers with its own status.
const METRIC_CUSTOM_PATH: &str = "/metric-custom";

/// The route whose refusal that policy cannot answer at all.
const METRIC_FALLBACK_PATH: &str = "/metric-mapper-fails";

/// The route one middleware frame refuses.
const METRIC_MIDDLEWARE_PATH: &str = "/metric-middleware";

/// The route whose mapped refusal one middleware frame answers in place of.
const METRIC_REPLACED_PATH: &str = "/metric-replaced";

/// The route whose refusal that policy answers with an unsendable response.
const METRIC_DISPLACED_PATH: &str = "/metric-displaced";

/// The boundary the multipart row declares.
const METRIC_BOUNDARY: &str = "----metricboundary";

/// What a row names instead of a content type it cannot state as a literal.
///
/// The multipart representation carries the boundary its body frames parts
/// with, so the row names the representation and the sender fills the boundary
/// in.
const MULTIPART_REPRESENTATION: &str = "multipart";

/// How one row's body reaches the server.
///
/// A row has exactly one body shape, so it is one value rather than a flag per
/// shape: two flags could name an oversized body that also breaks its framing,
/// which is not a request anything sends. Two of the three are what the sender
/// fills in rather than what the row states — an oversized body is generated
/// from the configured limit, and a broken framing is a request the ordinary
/// helper cannot express.
#[derive(Eq, PartialEq)]
enum BodyShape {
    /// Exactly the bytes the row declares.
    Stated,
    /// More bytes than the configured limit collects.
    Oversized,
    /// A chunked framing that breaks after the head is accepted.
    Unreadable,
}

/// One refused journey: how it is provoked, and what it must be counted as.
struct MetricRow {
    label: &'static str,
    method: &'static str,
    path: &'static str,
    content_type: &'static str,
    body: &'static str,
    shape: BodyShape,
    kind: &'static str,
    status: u16,
    /// Whether the rejection counter moves for this row.
    ///
    /// The counter is labelled by the status the peer received, so it counts a
    /// refusal the peer was answered with. A refusal an outer frame answered in
    /// place of has no such status, and the row that produces one declares the
    /// silence rather than leaving it unstated.
    counted: bool,
}

const METRIC_ROWS: [MetricRow; 15] = [
    MetricRow {
        label: "unmatched path",
        method: "GET",
        path: "/metric-absent",
        content_type: "",
        body: "",
        shape: BodyShape::Stated,
        kind: "routing",
        status: 404,
        counted: true,
    },
    MetricRow {
        label: "head on an unmatched path",
        method: "HEAD",
        path: "/metric-absent",
        content_type: "",
        body: "",
        shape: BodyShape::Stated,
        kind: "routing",
        status: 404,
        counted: true,
    },
    MetricRow {
        label: "wrong method",
        method: "POST",
        path: "/metric-ordinary",
        content_type: "text/plain",
        body: "",
        shape: BodyShape::Stated,
        kind: "method_selection",
        status: 405,
        counted: true,
    },
    MetricRow {
        label: "oversized body",
        method: "POST",
        path: "/metric-upload",
        content_type: "text/plain",
        body: "",
        shape: BodyShape::Oversized,
        kind: "body_limit",
        status: 413,
        counted: true,
    },
    // The sibling branch of the same collection failure: the limit owns one
    // error, and every other one is a transport account of a request that was
    // the right size. Counting both under the limit's label reported a size
    // complaint operators could not tell from a peer that went away.
    MetricRow {
        label: "unreadable body",
        method: "POST",
        path: "/metric-upload",
        content_type: "application/octet-stream",
        body: "",
        shape: BodyShape::Unreadable,
        kind: "body_unreadable",
        status: 400,
        counted: true,
    },
    MetricRow {
        label: "malformed json",
        method: "POST",
        path: "/metric-json",
        content_type: "application/json",
        body: common::MALFORMED_JSON,
        shape: BodyShape::Stated,
        kind: "malformed_body",
        status: 400,
        counted: true,
    },
    MetricRow {
        label: "malformed multipart",
        method: "POST",
        path: "/metric-multipart",
        content_type: MULTIPART_REPRESENTATION,
        body: common::MALFORMED_MULTIPART,
        shape: BodyShape::Stated,
        kind: "multipart",
        status: 400,
        counted: true,
    },
    MetricRow {
        label: "declared application refusal",
        method: "GET",
        path: "/metric-declared",
        content_type: "",
        body: "",
        shape: BodyShape::Stated,
        kind: "application",
        status: 400,
        counted: true,
    },
    MetricRow {
        label: "middleware refusal",
        method: "GET",
        path: METRIC_MIDDLEWARE_PATH,
        content_type: "",
        body: "",
        shape: BodyShape::Stated,
        kind: "middleware",
        status: 500,
        counted: true,
    },
    MetricRow {
        label: "unrepresentable response head",
        method: "GET",
        path: "/metric-invalid-header",
        content_type: "",
        body: "",
        shape: BodyShape::Stated,
        kind: "invalid_header",
        status: 500,
        counted: true,
    },
    MetricRow {
        label: "no admissible backend",
        method: "GET",
        path: "/metric-proxy/upstream",
        content_type: "",
        body: "",
        shape: BodyShape::Stated,
        kind: "proxy",
        status: 503,
        counted: true,
    },
    MetricRow {
        label: "custom mapped status",
        method: "GET",
        path: METRIC_CUSTOM_PATH,
        content_type: "",
        body: "",
        shape: BodyShape::Stated,
        kind: "internal_service",
        status: METRIC_CUSTOM_STATUS,
        counted: true,
    },
    MetricRow {
        label: "policy fallback",
        method: "GET",
        path: METRIC_FALLBACK_PATH,
        content_type: "",
        body: "",
        shape: BodyShape::Stated,
        kind: "internal_service",
        status: 500,
        counted: true,
    },
    // The counter is read after finalization, so a mapped answer the wire
    // refused is counted under the fallback the peer actually received — not
    // under the status policy chose and the peer never saw.
    MetricRow {
        label: "displaced mapped response",
        method: "GET",
        path: METRIC_DISPLACED_PATH,
        content_type: "",
        body: "",
        shape: BodyShape::Stated,
        kind: "internal_service",
        status: 500,
        counted: true,
    },
    // An outer frame answered in place of the mapped response, so the peer was
    // never given this refusal. The counter is labelled by the status the peer
    // received, and that status belongs to the frame's answer — counting the
    // refusal here would report a refused request the peer never saw refused.
    // The operator's record of it is the event this journey's tracing sibling
    // asserts.
    MetricRow {
        label: "replaced mapped response",
        method: "GET",
        path: METRIC_REPLACED_PATH,
        content_type: "",
        body: "",
        shape: BodyShape::Stated,
        kind: "internal_service",
        status: REPLACED_STATUS,
        counted: false,
    },
];

/// A table with no rows would drive no category and still report success.
///
/// Stated as a compile-time claim, which is the only honest place for it: the
/// length is a literal, so a runtime check of it could never have failed.
const _: () = assert!(!METRIC_ROWS.is_empty());

/// Refuse one route inside middleware, replace one route's answer, and pass
/// every other request through.
///
/// The refusing arm never calls the terminal: a frame that refused and still
/// ran what it was guarding would produce two refusals for one request. The
/// replacing arm does the opposite — it runs the terminal, discards the mapped
/// response the terminal produced, and answers with its own.
fn guarding_middleware(router: &mut Router) {
    router.use_middleware(
        |req: &Request,
         next: Next|
         -> Pin<Box<dyn Future<Output = Result<Response, RuntimeError>> + Send>> {
            match req.path() == METRIC_MIDDLEWARE_PATH {
                true => Box::pin(async {
                    Err(RuntimeError::Http("this frame refused the request".into()))
                }),
                false => {
                    let replaced = req.path() == METRIC_REPLACED_PATH;
                    let entered = next.call(req);
                    Box::pin(async move { replace_or_pass(entered.await, replaced) })
                }
            }
        },
    );
}

/// Answer with the frame's own response, or with what it entered.
///
/// The replaced arm drops the response it was handed, which is the whole point
/// of the row it serves: a mapped refusal an outer frame answers in place of.
fn replace_or_pass(entered: Response, replaced: bool) -> Result<Response, RuntimeError> {
    match replaced {
        true => Response::text(REPLACED_STATUS, REPLACED_BODY),
        false => Ok(entered),
    }
}

/// Register one route per counted category.
///
/// `handled` is moved by the one route whose answer the wire cannot carry: the
/// row it serves claims the refusal came after the handler, and that claim is
/// only made by the counter moving.
fn register_metric_routes(router: &mut Router, handled: &Arc<AtomicUsize>) {
    guarding_middleware(router);
    router.get(METRIC_MIDDLEWARE_PATH, |_req: &Request| {
        std::future::ready(Response::text(200, "guarded"))
    });
    router.get("/metric-ordinary", |_req: &Request| {
        std::future::ready(Response::text(200, "ordinary"))
    });
    router.post("/metric-upload", |_req: &Request| {
        std::future::ready(Response::text(200, "stored"))
    });
    router.post("/metric-json", common::ticket_handler());
    router.post("/metric-multipart", common::multipart_count_handler());
    router.get("/metric-declared", |_req: &Request| {
        std::future::ready(Err::<Response, RuntimeError>(RuntimeError::BadRequest(
            DECLARED_MESSAGE.into(),
        )))
    });
    router.get(
        "/metric-invalid-header",
        common::unrepresentable_handler(handled),
    );
    for path in [
        METRIC_CUSTOM_PATH,
        METRIC_FALLBACK_PATH,
        METRIC_DISPLACED_PATH,
        METRIC_REPLACED_PATH,
    ] {
        router.get(path, |_req: &Request| {
            std::future::ready(Err::<Response, RuntimeError>(two_level_failure()))
        });
    }
    router.proxy_checked(
        "/metric-proxy",
        "http://127.0.0.1:1",
        Arc::new(AtomicBool::new(false)),
    );
}

/// The routes whose refusal the counted fixture's policy answers with its own
/// status.
///
/// One route, stated as the list [`refusing_policy`] selects on.
const METRIC_CUSTOM_PATHS: &[&str] = &[METRIC_CUSTOM_PATH];

/// Send one row whose framing the ordinary request helper cannot express.
///
/// Hyper accepts the head, so the refusal is Camber's own and leaves through
/// the same exit every other row does. The refusal closes the connection, so
/// this row owns its socket and reads exactly the one response it was given.
fn send_broken_framing(addr: SocketAddr, row: &MetricRow, content_type: &str) -> u16 {
    let label = row.label;
    let (refused, _socket) = common::send_unreadable_body(
        addr,
        common::CLOSE_AFTER_RESPONSE,
        row.method,
        row.path,
        content_type,
    )
    .unwrap_or_else(|error| panic!("{label}: the broken framing was not answered: {error}"));
    refused.status
}

/// Send one counted row and return the status its peer was given.
fn send_metric_row(addr: SocketAddr, row: &MetricRow, oversized: &[u8]) -> u16 {
    let multipart = common::multipart_content_type(METRIC_BOUNDARY);
    let content_type = match row.content_type {
        MULTIPART_REPRESENTATION => multipart.as_ref(),
        stated => stated,
    };
    if row.shape == BodyShape::Unreadable {
        return send_broken_framing(addr, row, content_type);
    }
    let headers: Box<[(&str, &str)]> = match content_type.is_empty() {
        true => Box::new([]),
        false => Box::new([("Content-Type", content_type)]),
    };
    let body: &[u8] = match row.shape {
        BodyShape::Oversized => oversized,
        _ => row.body.as_bytes(),
    };
    common::request(addr, row.method, row.path, &headers, body, WIRE_TIMEOUT)
        .unwrap_or_else(|error| panic!("{}: the request did not complete: {error}", row.label))
        .status
}

/// Read the live metrics endpoint through this root's own client and bound.
///
/// The send is what stays here — a component request under [`WIRE_TIMEOUT`] —
/// and nothing else: the `200` and the parse belong to `scraped_samples`, which
/// the shared counter reader drives for both roots that scrape these counters.
fn scrape(addr: SocketAddr) -> common::HttpResponse {
    common::request(addr, "GET", "/metrics", &[], &[], WIRE_TIMEOUT)
        .expect("the metrics endpoint answered")
}

/// Assert every counted row moved its own counter, and the completion counter with it.
///
/// A row the counter is silent for is measured the same way, against a declared
/// zero: the silence is asserted rather than inherited from a label pair nobody
/// looked at.
fn assert_counted_rows(before: &common::RejectionCounters, after: &common::RejectionCounters) {
    for row in &METRIC_ROWS {
        assert_counted(
            before,
            after,
            &CountedOutcome {
                label: row.label,
                kind: row.kind,
                status: row.status,
                rejections: counted_rows(&METRIC_ROWS, |other| {
                    other.counted && other.kind == row.kind && other.status == row.status
                }),
                completions: counted_rows(&METRIC_ROWS, |other| other.status == row.status),
            },
        );
    }
}

#[test]
fn rejection_metric_labels_are_bounded() {
    common::test_runtime()
        .with_metrics()
        .run(|| {
            let mut router = Router::new();
            let handled = Arc::new(AtomicUsize::new(0));
            register_metric_routes(&mut router, &handled);
            let router =
                router
                    .max_request_body(METRIC_BODY_LIMIT)
                    .rejection_mapper(refusing_policy(
                        METRIC_FALLBACK_PATH,
                        METRIC_CUSTOM_PATHS,
                        METRIC_DISPLACED_PATH,
                        METRIC_CUSTOM_STATUS,
                    ));
            let addr = common::spawn_server(router);

            // One identifier from this journey, taken before the counters are
            // read, so the label check has a real high-cardinality value to
            // look for rather than a shape — and so its own refusal is outside
            // the window the row counts are measured over.
            let named = common::request(addr, "GET", "/metric-absent", &[], &[], WIRE_TIMEOUT)
                .expect("the naming request completed");
            let request_id = common::request_id_of(&named, "metric identity");

            // The deltas below are taken against the process-global Prometheus
            // registry, and recording is gated per runtime by the metrics
            // handle — so every metrics-enabled test in this binary contributes
            // to the same counters. The rows measure statuses 202, 400, 404,
            // 405, 413, 500, and 599; no other metrics-enabled test in this
            // root may produce one of those, or a concurrent completion would
            // land inside this journey's window and make the delta wrong.
            let before = common::rejection_counters(|| scrape(addr));
            let oversized = vec![b'x'; METRIC_BODY_LIMIT * 2].into_boxed_slice();
            let mut rows = 0_usize;
            for row in &METRIC_ROWS {
                let status = send_metric_row(addr, row, &oversized);
                assert_eq!(status, row.status, "{}: wire status", row.label);
                rows += 1;
            }
            assert_eq!(rows, METRIC_ROWS.len(), "every declared metric row ran");
            assert_eq!(
                handled.load(Ordering::SeqCst),
                1,
                "the unrepresentable head was produced by a handler that ran, so \
                 its refusal is one the wire decided and not one that preceded it"
            );
            // The flag decides which side of `assert_counted_rows` each row is
            // measured against, so its population is part of what this test
            // proves. A table whose rows all read `true` would never exercise
            // the declared-zero branch, and one whose rows all read `false`
            // would assert that this journey moved no counter at all.
            assert!(
                METRIC_ROWS.iter().any(|row| row.counted),
                "at least one declared row moves the rejection counter"
            );
            assert!(
                METRIC_ROWS.iter().any(|row| !row.counted),
                "at least one declared row declares the counter's silence"
            );

            let after = common::rejection_counters(|| scrape(addr));
            common::assert_bounded_rejection_labels(
                &after.rejections,
                &forbidden_values(
                    &request_id,
                    &[DECLARED_MESSAGE, ROOT_CAUSE, MIDDLE_CAUSE],
                    METRIC_ROWS.iter().map(|row| row.path),
                ),
            );
            assert_counted_rows(&before, &after);

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── The refusal an outer frame answered in place of ────────────────

/// The route whose mapped refusal a middleware frame replaces.
const REPLACED_PATH: &str = "/rejection-replaced";

/// The status a middleware frame answers with in place of a mapped refusal.
///
/// One number for both journeys that replace one: the counted matrix reads it as
/// the completion status its silent row produced, and this journey reads it as
/// the answer the peer was given. Two spellings of it were two journeys claiming
/// to prove the same replacement.
///
/// Not `200`: the metrics scrapes that journey takes are themselves `200`
/// completions, and a row measuring a completion delta must name a status only
/// the row produces.
const REPLACED_STATUS: u16 = 202;

/// The body that frame writes.
const REPLACED_BODY: &str = "the frame answered";

/// The fixed sentence a discarded rejection response is recorded under.
const DISCARDED_MESSAGE: &str = "message=rejection response was not sent";

/// Replace the mapped response one route's failure produced.
///
/// The frame calls the terminal, so the handler runs and its failure is
/// classified and mapped where it was produced. Then the frame drops that
/// response and answers with its own — the shape an outer normalizing or
/// error-flattening frame has, and the one that used to leave the refusal
/// unrecorded.
fn replacing_middleware(router: &mut Router) {
    router.use_middleware(
        |req: &Request,
         next: Next|
         -> Pin<Box<dyn Future<Output = Result<Response, RuntimeError>> + Send>> {
            let entered = next.call(req);
            Box::pin(async move {
                drop(entered.await);
                Response::text(REPLACED_STATUS, REPLACED_BODY)
            })
        },
    );
}

/// Assert the discarded refusal reached the operator, and named no sent status.
fn assert_replaced_events(capture: &common::TraceCapture) {
    let label = "replaced rejection response";
    let events = capture.events();

    let discarded = only_event(&events, DISCARDED_MESSAGE, label);
    assert_field_value(discarded, "kind", "internal_service", label);
    assert_field_value(discarded, "mapped_status", "500", label);
    assert_field_value(discarded, "default_status", "500", label);
    let raw_path = format!("raw_path={REPLACED_PATH}");
    let route = format!("route={REPLACED_PATH}");
    assert_fields(
        discarded,
        &[
            "method=GET",
            raw_path.as_str(),
            route.as_str(),
            "protocol=ordinary_http",
            "remote_addr=127.0.0.1",
        ],
        label,
    );
    assert_causes_in_field(discarded, &[MIDDLE_CAUSE, ROOT_CAUSE], label);
    assert!(
        common::field_value(discarded, "status").is_none(),
        "{label}: a refusal no peer received recorded a sent status: {discarded}"
    );
    assert_message_is_fixed(discarded, DISCARDED_MESSAGE, label);
    assert!(
        !capture.recorded(&[REJECTION_MESSAGE]),
        "{label}: a refusal the peer never received was recorded as one it did"
    );

    let completed = only_event(&events, COMPLETION_MESSAGE, label);
    assert_field_value(completed, "status", &REPLACED_STATUS.to_string(), label);
    let request_id = common::field_value(discarded, "request_id")
        .unwrap_or_else(|| panic!("{label}: the discarded record names no request: {discarded}"));
    assert_field_value(completed, "request_id", request_id, label);
}

/// A refusal an outer frame answers in place of still reaches the operator.
///
/// The peer is given the frame's answer, and that is what the completion event
/// and the counters report. The refusal underneath it has no sent status and no
/// counter, so its record is the one thing that says the request failed at all —
/// and it carries the same identifier the completion event does, which is how an
/// operator reads the two together.
#[test]
fn replaced_rejection_response_reaches_the_operator_record() {
    let capture = common::capture_events(REPLACED_PATH);

    common::test_runtime()
        .with_tracing()
        .run(|| {
            let mut router = Router::new();
            replacing_middleware(&mut router);
            router.get(REPLACED_PATH, |_req: &Request| {
                std::future::ready(Err::<Response, RuntimeError>(two_level_failure()))
            });
            let addr = common::spawn_server(router);

            let response = common::request(addr, "GET", REPLACED_PATH, &[], &[], WIRE_TIMEOUT)
                .expect("the replaced request completed");
            assert_eq!(
                response.status, REPLACED_STATUS,
                "the frame's own answer reached the peer"
            );
            assert_eq!(response.text().as_ref(), REPLACED_BODY, "wire body");
            assert_no_private_text(
                &response,
                &[MIDDLE_CAUSE, ROOT_CAUSE],
                "replaced rejection response",
            );

            assert_replaced_events(&capture);

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── The deadline exit an operator still has to see ─────────────────

/// The route whose body never finishes arriving.
const DEADLINE_PATH: &str = "/deadline-upload";

/// The outer bound the stalled peer's read and this fixture's join run under.
///
/// Well past the 30-second collection deadline the read is waiting on, so it
/// never displaces the refusal this test exists to observe.
const ANSWER_BOUND: Duration = Duration::from_secs(300);

/// How many times that bound is armed before an unsettled wait is a failure.
///
/// More than one, because a paused clock advances to the nearest timer whenever
/// the runtime idles: an arming made before the request has reached body
/// collection is the only timer in the process, so the clock jumps straight to
/// it and it elapses having proved nothing about the deadline. Every idle park
/// before collection begins — the accept, the head read, the dispatch between
/// them — can consume one arming that way. Four is well past what those need,
/// and an arming spent on virtual time costs no real time at all.
const ANSWER_ARMINGS: usize = 4;

/// Assert the record one stalled body left behind.
fn assert_deadline_event(capture: &common::TraceCapture) {
    let label = "body deadline";
    let events = capture.events();
    let event = only_event(&events, REJECTION_MESSAGE, label);
    assert_field_value(event, "status", "408", label);
    let raw_path = format!("raw_path={DEADLINE_PATH}");
    let route = format!("route={DEADLINE_PATH}");
    assert_fields(
        event,
        &[
            "kind=body_timeout",
            "default_status=408",
            "method=POST",
            raw_path.as_str(),
            route.as_str(),
            "protocol=ordinary_http",
        ],
        label,
    );
    assert_message_is_fixed(event, REJECTION_MESSAGE, label);
}

/// The collection deadline records the refusal it produced, on virtual time.
///
/// The peer sends a head promising a body and then one byte of it. Nothing
/// waits on a wall clock: the runtime is paused, so the deadline is reached by
/// the scheduler running out of work rather than by elapsed real time. That is
/// also why this category cannot join the counted matrix above — the deadline
/// is a fixed 30 seconds, and a live-clock fixture could only reach it by
/// waiting, which is the race the hygiene command forbids.
///
/// Nor can this fixture scrape the counter itself, which would close that gap
/// without a race. `/metrics` is served from the metrics handle on the runtime
/// context, and the only public seam that installs a context outside
/// `RuntimeBuilder::run` — `runtime_test_support::install_runtime_context` —
/// installs one with no handle. `run` is the sole public way to attach one, and
/// it builds and blocks on an executor of its own, which a paused Tokio test
/// cannot host. So `body_timeout` is counted by nothing until that seam can
/// carry a handle.
///
/// What it proves is the exit: the refusal is recorded where every buffered
/// answer is converted, which is the one place a refusal is counted, and it is
/// recorded under the status the peer was actually given.
#[tokio::test(start_paused = true)]
async fn body_deadline_refusal_reaches_the_operator_record() {
    let capture = common::capture_events(DEADLINE_PATH);
    let _context = camber::runtime_test_support::install_runtime_context();

    let mut router = Router::new();
    router.post(DEADLINE_PATH, |_req: &Request| {
        std::future::ready(Response::text(200, "stored"))
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("the deadline fixture bound a listener");
    let addr = listener.local_addr().expect("the listener named its port");
    let server = camber::http::serve_background(listener, router);

    let mut peer = tokio::net::TcpStream::connect(addr)
        .await
        .expect("the stalled peer connected");
    // No `Connection` value: this case reads back the disposition the framework
    // decided, and a preference the peer stated is one the server could echo.
    let head = common::stalled_request_head(None, "POST", DEADLINE_PATH);
    peer.write_all(head.as_bytes())
        .await
        .expect("the stalled head was sent");
    peer.flush().await.expect("the stalled head flushed");

    let mut answer = Vec::new();
    common::bounded_under_pause(
        peer.read_to_end(&mut answer),
        ANSWER_BOUND,
        ANSWER_ARMINGS,
        "the stalled body's answer",
    )
    .await
    .expect("the stalled peer read its answer");
    drop(peer);

    // Teardown runs before anything is asserted, so it runs whatever the
    // answer turned out to be: a failed assertion between here and the join
    // would otherwise leave this fixture's listener and supervisor task alive
    // for the rest of the binary. The join is bounded for the reason the read
    // above is: the clock is paused, so a supervisor that never returns would
    // park this whole binary instead of failing here.
    server.shutdown();
    let joined = common::bounded_under_pause(
        server.join(),
        ANSWER_BOUND,
        ANSWER_ARMINGS,
        "the deadline fixture's server",
    )
    .await;

    // The expiry arm was already failed above, so what is left to classify is
    // the outcome itself — and a server that ended because it was told to is a
    // completed join, which `Cancelled` is one spelling of.
    common::assert_server_joined(Ok(joined));
    let text = String::from_utf8_lossy(&answer);
    assert!(
        text.starts_with("HTTP/1.1 408 "),
        "the stalled body was answered with {text}"
    );

    assert_deadline_event(&capture);
}
