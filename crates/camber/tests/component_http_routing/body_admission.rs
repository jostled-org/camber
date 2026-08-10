//! Route-aware body admission, entered through live routing.
//!
//! Every case here drives a real accepted HTTP request through the public
//! `Router` or `HostRouter`, so what is asserted is what the routing stage
//! decided for a request a peer actually sent. The lifecycle observer supplies
//! counters only: it reads the production collector's own progress and the
//! production permit owner's own release, and chooses nothing.

use crate::http as wire;
use crate::rejection_support::{
    Collapsed, Journal, Observed, assert_classification, counting_handler, drain, recording_mapper,
};
use crate::runtime_support as common;

use camber::http::mock::LifecycleController;
use camber::http::{
    BodyAdmission, BodyAdmissionContext, HostRouter, RejectionKind, Request, RequestBodyMode,
    Response, Router,
};
use camber::{Resource, RuntimeError, runtime};
use std::future::Future;
use std::io::Write;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// What one admission callback was given.
#[derive(Debug)]
struct Seen {
    request_id: Box<str>,
    method: Box<str>,
    raw_path: Box<str>,
    route: Box<str>,
    mode: RequestBodyMode,
    declared_length: Option<u64>,
    tenant: Option<Box<str>>,
    absent: Option<Box<str>>,
    binary: Option<Box<str>>,
    frames_at_entry: usize,
}

/// Every admission callback one fixture saw, in order.
type Callbacks = Arc<Mutex<Vec<Seen>>>;

fn callbacks() -> Callbacks {
    Arc::new(Mutex::new(Vec::new()))
}

/// Take everything the policy has recorded so far.
fn seen(journal: &Callbacks) -> Box<[Seen]> {
    std::mem::take(&mut *journal.lock().unwrap_or_else(|error| error.into_inner()))
        .into_boxed_slice()
}

/// A policy that records what it saw and admits under `limit`.
///
/// The frame count is read at callback entry, through the same observer the
/// case reads afterwards, so "before the first body poll" is a claim about the
/// production collector rather than about the order two fixtures happened to
/// run in.
fn recording_policy(
    journal: &Callbacks,
    observer: &Arc<LifecycleController>,
    limit: usize,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let journal = Arc::clone(journal);
    let observer = Arc::clone(observer);
    move |context: &BodyAdmissionContext<'_>| {
        journal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(Seen {
                request_id: context.request_id().as_str().into(),
                method: context.method().into(),
                raw_path: context.raw_path().into(),
                route: context.route().into(),
                mode: context.mode(),
                declared_length: context.declared_length(),
                tenant: context.header("X-Tenant").map(Box::from),
                absent: context.header("x-not-sent").map(Box::from),
                binary: context.header("x-binary").map(Box::from),
                frames_at_entry: observer.body_frames_polled(),
            });
        Ok(BodyAdmission::new(limit))
    }
}

/// A handler that answers with exactly the body it was given, and names the
/// identity it ran under.
fn echo_handler(request: &Request) -> Pin<Box<dyn Future<Output = Response> + Send>> {
    let body: Box<str> = request.body().into();
    let request_id: Box<str> = request.request_id().as_str().into();
    Box::pin(async move {
        Response::text(200, &body)
            .expect("valid echo status")
            .with_header("X-Handled-Request-Id", &request_id)
    })
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

/// One raw exchange whose header bytes are not valid UTF-8.
///
/// Written byte by byte because the suite's readers take `&str` headers, and a
/// value that is not UTF-8 is exactly what this row is about.
fn send_with_binary_header(addr: SocketAddr, body: &str) -> wire::HttpResponse {
    let mut stream = wire::connect(addr).expect("connect the binary-header peer");
    let mut head = Vec::new();
    head.extend_from_slice(b"POST /upload/7 HTTP/1.1\r\nHost: localhost\r\n");
    head.extend_from_slice(b"Connection: close\r\nX-Binary: ");
    head.extend_from_slice(&[0xf0, 0x9f, 0x28]);
    head.extend_from_slice(b"\r\nContent-Length: ");
    head.extend_from_slice(body.len().to_string().as_bytes());
    head.extend_from_slice(b"\r\n\r\n");
    head.extend_from_slice(body.as_bytes());
    stream
        .write_all(&head)
        .expect("write the binary-header request");
    wire::read_http_response_bounded(&mut stream).expect("read the binary-header answer")
}

/// What one recorded callback must have been given.
///
/// The answered response is part of it: the identity the policy saw is only
/// meaningful against the identity the handler went on to report, and a row
/// that carried one without the other could compare a request to itself.
struct ExpectedCallback<'a> {
    method: &'a str,
    raw_path: &'a str,
    route: &'a str,
    declared_length: Option<u64>,
    /// The `X-Tenant` value the policy must have selected, if any.
    tenant: Option<&'a str>,
    /// The frame count read before this request was sent.
    frames_before: usize,
    answered: &'a wire::HttpResponse,
}

/// Assert one admitted request reached the echo handler and came back whole.
fn assert_echoed(response: &wire::HttpResponse, body: &[u8], label: &str) {
    assert_eq!(response.status, 200, "{label}: status");
    assert_eq!(
        response.body.as_ref(),
        body,
        "{label}: the handler echoed the exact body"
    );
}

/// Assert one callback saw the request whose handler then answered it.
fn assert_callback(seen: &Seen, expected: &ExpectedCallback<'_>, label: &str) {
    assert_eq!(seen.method.as_ref(), expected.method, "{label}: method");
    assert_eq!(
        seen.raw_path.as_ref(),
        expected.raw_path,
        "{label}: raw path"
    );
    assert_eq!(seen.route.as_ref(), expected.route, "{label}: route");
    assert_eq!(seen.mode, RequestBodyMode::Buffered, "{label}: body mode");
    assert_eq!(
        seen.declared_length, expected.declared_length,
        "{label}: normalized declaration"
    );
    assert_eq!(
        seen.tenant.as_deref(),
        expected.tenant,
        "{label}: selected header value"
    );
    assert_eq!(
        seen.request_id.as_ref(),
        expected
            .answered
            .header("x-handled-request-id")
            .expect("the handler reported its identity"),
        "{label}: the policy and the handler name one request"
    );
    assert_eq!(
        seen.frames_at_entry, expected.frames_before,
        "{label}: admission decides before this request's first body frame is polled"
    );
}

/// The three answers one fixture collected, with the frame count read before each.
///
/// The counts travel with the responses because that is the pairing the claim
/// rests on: each callback's reading must equal the count taken before its own
/// request was sent, not before some other one.
struct Exchanges {
    first: wire::HttpResponse,
    second: wire::HttpResponse,
    third: wire::HttpResponse,
    before: [usize; 3],
}

/// Drive the three admitted requests, reading the frame counter before each.
fn drive_exchanges(addr: SocketAddr, controller: &LifecycleController) -> Exchanges {
    let before_first = controller.body_frames_polled();
    let first = wire::send(
        addr,
        "POST",
        "/upload/42",
        &[("X-Tenant", "alpha"), ("X-Tenant", "beta")],
        b"first-body",
    );
    assert_echoed(&first, b"first-body", "repeated header");

    let before_second = controller.body_frames_polled();
    let second = wire::send(
        addr,
        "PUT",
        "/notes/release",
        &[("x-TENANT", "gamma")],
        b"second-body",
    );
    assert_echoed(&second, b"second-body", "mixed-case header");

    let before_third = controller.body_frames_polled();
    let third = send_with_binary_header(addr, "third-body");
    assert_echoed(&third, b"third-body", "header that is not UTF-8");

    Exchanges {
        first,
        second,
        third,
        before: [before_first, before_second, before_third],
    }
}

/// Assert what the policy was given for each of the three exchanges.
fn assert_exchange_contexts(recorded: &[Seen], sent: &Exchanges) {
    assert_callback(
        &recorded[0],
        &ExpectedCallback {
            method: "POST",
            raw_path: "/upload/42",
            route: "/upload/:id",
            declared_length: Some(10),
            tenant: Some("alpha"),
            frames_before: sent.before[0],
            answered: &sent.first,
        },
        "repeated header: the first value is the selected one",
    );
    assert_eq!(
        recorded[0].absent, None,
        "a header the peer never sent reads as absent"
    );

    assert_callback(
        &recorded[1],
        &ExpectedCallback {
            method: "PUT",
            raw_path: "/notes/release",
            route: "/notes/:slug",
            declared_length: Some(11),
            tenant: Some("gamma"),
            frames_before: sent.before[1],
            answered: &sent.second,
        },
        "mixed-case header: lookup is case-insensitive",
    );
    assert_ne!(
        recorded[0].request_id, recorded[1].request_id,
        "two requests carry two identities"
    );
    assert!(
        sent.before[1] > sent.before[0],
        "the first request did collect a body, so the claim is not vacuous"
    );

    assert_callback(
        &recorded[2],
        &ExpectedCallback {
            method: "POST",
            raw_path: "/upload/7",
            route: "/upload/:id",
            declared_length: Some(10),
            tenant: None,
            frames_before: sent.before[2],
            answered: &sent.third,
        },
        "header that is not UTF-8",
    );
    assert_eq!(
        recorded[2].binary, None,
        "a header value that is not UTF-8 reads as absent, exactly as it does on Request"
    );
}

#[test]
fn buffered_admission_context_precedes_first_body_poll() {
    common::test_runtime()
        .run(|| {
            let port = wire::reserve_observed();
            let observer = port.controller();
            let journal = callbacks();

            let mut router = Router::new();
            router.post("/upload/:id", echo_handler);
            router.put("/notes/:slug", echo_handler);
            let router = router.body_admission(recording_policy(&journal, &observer, 4096));
            let server = port.serve(router);

            let sent = drive_exchanges(server.addr(), server.controller());

            let recorded = seen(&journal);
            assert_eq!(
                recorded.len(),
                3,
                "each matched buffered route invokes its policy exactly once: {recorded:?}"
            );
            assert_exchange_contexts(&recorded, &sent);

            assert!(
                server.controller().body_frames_polled() > 0,
                "the fixture's own counter moves once real bodies are collected"
            );
            assert_eq!(
                server.controller().body_permit_owners_dropped(),
                0,
                "a policy that supplies no permit creates no permit owner"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

/// The tag the resolved child router's own mapper records under.
const HOST_CHILD_MAPPER: &str = "child";

/// The tag the host router's fallback mapper records under.
const HOST_FALLBACK_MAPPER: &str = "hosts";

/// The one authority a child router claims in the terminal table.
const ROUTED_HOST: &str = "routed.test";

/// The three categories the terminal table refuses under.
const ROUTING: RejectionKind = RejectionKind::Routing;
const MISMATCH: RejectionKind = RejectionKind::MethodSelection;
const PROXY: RejectionKind = RejectionKind::Proxy;

/// The refusal one terminal row expects, and the mapper that must produce it.
///
/// Held as one value rather than loose optional fields, so a row cannot state
/// the classification it expects without naming which registered mapper was
/// handed it. Both mappers write into one journal, so a row that read only the
/// classification would pass just as well with the host and child selection
/// inverted.
///
/// No status of its own: the recording mappers keep each producer's default, so
/// the number the wire carried and the number the producer classified under are
/// one fact, and the row states it once.
struct Refusal<'a> {
    origin: &'static str,
    kind: RejectionKind,
    message: &'a str,
}

impl<'a> Refusal<'a> {
    /// The refusal a row expects, from the mapper that must produce it.
    const fn by(origin: &'static str, kind: RejectionKind, message: &'a str) -> Self {
        Self {
            origin,
            kind,
            message,
        }
    }
}

/// One row of the terminal table, and what the wire must answer it with.
struct Terminal<'a> {
    label: &'a str,
    method: &'a str,
    path: &'a str,
    host: &'a str,
    status: u16,
    mapped: Option<Refusal<'a>>,
    route: Option<&'a str>,
}

/// The declared body every terminal row withholds.
const WITHHELD: &str = "64";

/// Send one terminal row, declaring a body the peer never sends.
fn send_withheld(addr: SocketAddr, row: &Terminal<'_>) -> wire::HttpResponse {
    wire::send_to_host_with(
        addr,
        row.method,
        row.path,
        row.host,
        &[("Content-Length", WITHHELD)],
    )
}

/// Assert what one terminal row's mapper was, or was not, given.
fn assert_row_mapping(row: &Terminal<'_>, observations: &[Observed]) {
    match &row.mapped {
        None => assert!(
            observations.is_empty(),
            "{}: an answered internal route invokes no rejection policy: {observations:?}",
            row.label
        ),
        Some(expected) => {
            assert_eq!(
                observations.len(),
                1,
                "{}: one refusal invokes one mapper once: {observations:?}",
                row.label
            );
            let seen = &observations[0];
            assert_eq!(
                seen.origin, expected.origin,
                "{}: the mapper the routing stage selected answered it",
                row.label
            );
            assert_classification(
                seen,
                &Collapsed {
                    kind: expected.kind,
                    status: row.status,
                    message: expected.message,
                },
                row.label,
            );
            assert_eq!(seen.method.as_ref(), row.method, "{}: method", row.label);
            assert_eq!(seen.raw_path.as_ref(), row.path, "{}: raw path", row.label);
            assert_eq!(seen.route.as_deref(), row.route, "{}: route", row.label);
            assert!(
                !seen.request_id.is_empty(),
                "{}: the refusal names the request",
                row.label
            );
        }
    }
}

/// Every terminal row, in the order one fixture sends them.
///
/// The deep path is the caller's, because it is the only subject here that is
/// built rather than written: a path past the segment limit comes from the
/// limit, not from a literal.
fn terminal_rows(deep: &str) -> [Terminal<'_>; 7] {
    [
        Terminal {
            label: "internal health route",
            method: "GET",
            path: "/health",
            host: ROUTED_HOST,
            status: 200,
            mapped: None,
            route: None,
        },
        Terminal {
            label: "no route claims the path",
            method: "GET",
            path: "/missing",
            host: ROUTED_HOST,
            status: 404,
            mapped: Some(Refusal::by(HOST_CHILD_MAPPER, ROUTING, "not found")),
            route: None,
        },
        Terminal {
            label: "ordinary method mismatch",
            method: "POST",
            path: "/only-get",
            host: ROUTED_HOST,
            status: 405,
            mapped: Some(Refusal::by(
                HOST_CHILD_MAPPER,
                MISMATCH,
                "method not allowed",
            )),
            route: Some("/only-get"),
        },
        Terminal {
            label: "unnameable method",
            method: "BREW",
            path: "/only-get",
            host: ROUTED_HOST,
            status: 405,
            mapped: Some(Refusal::by(
                HOST_CHILD_MAPPER,
                MISMATCH,
                "method not allowed",
            )),
            route: Some("/only-get"),
        },
        Terminal {
            label: "authority that is not one",
            method: "POST",
            path: "/upload/9",
            host: "not an authority",
            status: 400,
            mapped: Some(Refusal::by(
                HOST_FALLBACK_MAPPER,
                ROUTING,
                "invalid host header",
            )),
            route: None,
        },
        Terminal {
            label: "URI too deep to route",
            method: "POST",
            path: deep,
            host: ROUTED_HOST,
            status: 414,
            mapped: Some(Refusal::by(HOST_CHILD_MAPPER, ROUTING, "URI path too deep")),
            route: None,
        },
        Terminal {
            label: "buffered proxy with an unhealthy upstream",
            method: "POST",
            path: "/down/echo",
            host: ROUTED_HOST,
            status: 503,
            mapped: Some(Refusal::by(HOST_CHILD_MAPPER, PROXY, "service unavailable")),
            route: Some("/down/*proxy_path"),
        },
    ]
}

/// The host table every terminal row is sent to.
///
/// One child claims [`ROUTED_HOST`], and its proxy route registers an upstream
/// that is never well, so the unhealthy row refuses with no peer to reach. Both
/// mappers record into one journal, which is what makes each row's origin claim
/// a real selection rather than the only answer available.
fn terminal_hosts(
    journal: &Callbacks,
    observer: &Arc<LifecycleController>,
    mapped: &Journal,
    handled: &Arc<AtomicUsize>,
) -> HostRouter {
    let mut child = Router::new();
    child.get("/only-get", counting_handler(handled, "only-get"));
    child.post("/upload/:id", echo_handler);
    child.proxy_checked(
        "/down",
        "http://127.0.0.1:1",
        Arc::new(AtomicBool::new(false)),
    );
    let child = child
        .body_admission(recording_policy(journal, observer, 4096))
        .rejection_mapper(recording_mapper(mapped, HOST_CHILD_MAPPER));

    let mut hosts =
        HostRouter::new().rejection_mapper(recording_mapper(mapped, HOST_FALLBACK_MAPPER));
    hosts.add(ROUTED_HOST, child);
    hosts
}

/// Assert no terminal path touched a request body in any observable way.
fn assert_no_body_observations(controller: &LifecycleController) {
    assert_eq!(
        controller.body_frames_polled(),
        0,
        "no terminal path polls a body frame"
    );
    assert_eq!(
        controller.body_peak_retained_bytes(),
        0,
        "no terminal path retains payload bytes"
    );
    assert_eq!(
        controller.body_permit_owners_dropped(),
        0,
        "no terminal path takes ownership of a permit"
    );
}

/// Drive the matched buffered control that calibrates the same counters.
///
/// Without it, the zero observations above are a claim about counters nothing
/// in the fixture could have moved.
fn assert_buffered_control(
    addr: SocketAddr,
    journal: &Callbacks,
    controller: &LifecycleController,
) {
    let admitted = wire::request_to_host_with_body(
        addr,
        "POST",
        "/upload/12",
        ROUTED_HOST,
        &[],
        b"control-body",
    )
    .expect("the matched buffered control answered");
    assert_echoed(&admitted, b"control-body", "matched buffered control");
    let recorded = seen(journal);
    assert_eq!(
        recorded.len(),
        1,
        "the matched buffered control is the only policy call: {recorded:?}"
    );
    assert!(
        controller.body_frames_polled() > 0,
        "the same counter moves once a matched buffered route reads a body"
    );
}

#[test]
fn internal_and_routing_terminal_paths_skip_buffered_admission() {
    common::test_runtime()
        .resource(AlwaysHealthy)
        .run(|| {
            let port = wire::reserve_observed();
            let observer = port.controller();
            let journal = callbacks();
            let mapped: Journal = Arc::new(Mutex::new(Vec::new()));
            let handled = Arc::new(AtomicUsize::new(0));
            let server = port.serve_hosts(terminal_hosts(&journal, &observer, &mapped, &handled));
            let addr = server.addr();

            let deep = "/deep".repeat(80).into_boxed_str();
            for row in &terminal_rows(&deep) {
                let response = send_withheld(addr, row);
                assert_eq!(response.status, row.status, "{}: wire status", row.label);
                assert_row_mapping(row, &drain(&mapped));
            }

            assert!(
                seen(&journal).is_empty(),
                "no terminal or internal path invokes a body policy"
            );
            assert_eq!(
                handled.load(Ordering::SeqCst),
                0,
                "no terminal path runs an ordinary handler"
            );
            assert_no_body_observations(server.controller());
            assert_buffered_control(addr, &journal, server.controller());

            runtime::request_shutdown();
        })
        .unwrap();
}

/// A permit drawn from a pool two child routers share.
struct PoolPermit(Arc<AtomicUsize>);

impl Drop for PoolPermit {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// A policy that names the child it belongs to and admits under `limit`.
fn child_policy(
    seen_children: &Arc<Mutex<Vec<Box<str>>>>,
    pool: &Arc<AtomicUsize>,
    child: &'static str,
    limit: usize,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let seen_children = Arc::clone(seen_children);
    let pool = Arc::clone(pool);
    move |_context: &BodyAdmissionContext<'_>| {
        seen_children
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(child.into());
        pool.fetch_add(1, Ordering::SeqCst);
        Ok(BodyAdmission::with_permit(
            limit,
            PoolPermit(Arc::clone(&pool)),
        ))
    }
}

/// Declare `declared` bytes on a host-routed upload and never send them.
fn declare_upload(addr: SocketAddr, host: &str, declared: usize) -> wire::HttpResponse {
    wire::send_to_host_with(
        addr,
        "POST",
        "/upload/1",
        host,
        &[("Content-Length", &declared.to_string())],
    )
}

/// How many times one child's policy answered.
fn calls_for(children: &[Box<str>], child: &str) -> usize {
    children
        .iter()
        .filter(|seen| seen.as_ref() == child)
        .count()
}

/// The host ceiling every child in the table is contained by.
const HOST_CEILING: usize = 300;

/// The hard cap a configured ceiling is clamped to.
const HARD_MAX: usize = 256 * 1024 * 1024;

/// The ceiling a router that configures none is left with.
const DEFAULT_MAX: usize = 8 * 1024 * 1024;

/// One host, the maximum it must resolve to, and why that is the minimum.
struct Ceiling {
    host: &'static str,
    effective: usize,
    reason: &'static str,
}

/// Every host in the ceiling table, and the one effective minimum each resolves.
const CEILING_ROWS: [Ceiling; 4] = [
    Ceiling {
        host: "inherits.test",
        effective: HOST_CEILING,
        reason: "a child that configures none inherits the host ceiling",
    },
    Ceiling {
        host: "narrower.test",
        effective: 100,
        reason: "a child ceiling narrows the host ceiling",
    },
    Ceiling {
        host: "wider.test",
        effective: HOST_CEILING,
        reason: "a child cannot raise the host ceiling, even at the hard cap",
    },
    Ceiling {
        host: "selects.test",
        effective: 50,
        reason: "a selected admission maximum narrows both configured ceilings",
    },
];

/// The host table the ceiling rows are served from.
///
/// Four children over one host ceiling: one configuring nothing, one narrower,
/// one at the hard cap, and one whose policy selects below both. They share one
/// permit pool, so what the pool reports at the end is every child's releases
/// together.
fn ceiling_hosts(seen_children: &Arc<Mutex<Vec<Box<str>>>>, pool: &Arc<AtomicUsize>) -> HostRouter {
    let mut inherits = Router::new();
    inherits.post("/upload/:id", echo_handler);
    let inherits =
        inherits.body_admission(child_policy(seen_children, pool, "inherits", usize::MAX));

    let mut narrower = Router::new().max_request_body(100);
    narrower.post("/upload/:id", echo_handler);
    let narrower = narrower.body_admission(child_policy(seen_children, pool, "narrower", 1024));

    let mut wider = Router::new().max_request_body(usize::MAX);
    wider.post("/upload/:id", echo_handler);

    let mut selects = Router::new();
    selects.post("/upload/:id", echo_handler);
    let selects = selects.body_admission(child_policy(seen_children, pool, "selects", 50));

    let mut hosts = HostRouter::new().max_request_body(HOST_CEILING);
    hosts.add("inherits.test", inherits);
    hosts.add("narrower.test", narrower);
    hosts.add("wider.test", wider);
    hosts.add("selects.test", selects);
    hosts
}

/// Drive one host's refusal above and admission at its effective maximum.
///
/// Both halves are the claim: a ceiling proved only by a refusal could be any
/// number at or below the one declared.
fn assert_effective_ceiling(addr: SocketAddr, row: &Ceiling) {
    let Ceiling {
        host,
        effective,
        reason,
    } = *row;
    let refused = declare_upload(addr, host, effective + 1);
    assert_eq!(refused.status, 413, "{host}: {reason}");
    let admitted = wire::request_to_host_with_body(
        addr,
        "POST",
        "/upload/1",
        host,
        &[],
        &vec![b'x'; effective],
    )
    .expect("the effective-limit control answered");
    assert_eq!(admitted.status, 200, "{host}: the effective limit admits");
    assert_eq!(
        admitted.body.len(),
        effective,
        "{host}: the whole admitted body came back"
    );
}

/// Assert which child policies answered, and how often.
///
/// The counts are the ordering claim: a refusal decided by the configured
/// ceiling never reaches policy, and one decided by the policy's own selection
/// reaches it first.
fn assert_child_policy_calls(children: &[Box<str>]) {
    assert_eq!(
        calls_for(children, "inherits"),
        1,
        "a declaration above the configured ceiling is refused before policy: {children:?}"
    );
    assert_eq!(calls_for(children, "narrower"), 1);
    assert_eq!(
        calls_for(children, "selects"),
        2,
        "a declaration inside the ceiling reaches policy, then loses to its selection"
    );
    assert_eq!(
        calls_for(children, "wider"),
        0,
        "a child that configures no policy invokes none: {children:?}"
    );
}

/// The host-routed half: four children, one effective minimum each.
fn host_and_child_ceilings_resolve_one_minimum() {
    common::test_runtime()
        .run(|| {
            let port = wire::reserve_observed();
            let seen_children = Arc::new(Mutex::new(Vec::new()));
            let pool = Arc::new(AtomicUsize::new(0));
            let server = port.serve_hosts(ceiling_hosts(&seen_children, &pool));
            let addr = server.addr();

            for row in &CEILING_ROWS {
                assert_effective_ceiling(addr, row);
            }

            let children = std::mem::take(
                &mut *seen_children
                    .lock()
                    .unwrap_or_else(|error| error.into_inner()),
            )
            .into_boxed_slice();
            assert_child_policy_calls(&children);
            assert_eq!(
                pool.load(Ordering::SeqCst),
                0,
                "every permit the shared pool issued was released"
            );
            assert_eq!(
                server.controller().body_permit_owners_dropped(),
                4,
                "one owner is released for each admitted permit, refused afterwards or not"
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

/// The single-router half: the hard cap clamps, and the default is unchanged.
fn configured_ceilings_clamp_and_leave_the_default_intact() {
    common::test_runtime()
        .run(|| {
            let mut clamped = Router::new().max_request_body(usize::MAX);
            clamped.post("/upload/:id", echo_handler);
            let clamped_addr = common::spawn_server(clamped);
            assert_eq!(
                declare_upload(clamped_addr, "localhost", HARD_MAX + 1).status,
                413,
                "a configured ceiling above the hard cap is clamped to it"
            );

            let mut untouched = Router::new();
            untouched.post("/upload/:id", echo_handler);
            let untouched_addr = common::spawn_server(untouched);
            assert_eq!(
                declare_upload(untouched_addr, "localhost", DEFAULT_MAX + 1).status,
                413,
                "the unchanged eight-MiB default still bounds a router with no policy"
            );
            let within_default = wire::send(untouched_addr, "POST", "/upload/1", &[], b"ordinary");
            assert_echoed(&within_default, b"ordinary", "inside the default ceiling");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn buffered_host_and_child_ceilings_resolve_one_effective_minimum() {
    host_and_child_ceilings_resolve_one_minimum();
    configured_ceilings_clamp_and_leave_the_default_intact();
}
