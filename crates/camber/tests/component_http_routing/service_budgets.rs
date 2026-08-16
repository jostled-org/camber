//! 2.T1: runtime, server, host, and child policies narrow, and never widen.

use crate::http as http_support;

use camber::http::mock::{LifecycleCheckpoint, LifecycleController};
use camber::http::{
    HostRouter, Request, RequestBudget, Response, Router, ServerPolicy, TransferBudget,
};
use std::time::Duration;

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The header boundary the runtime envelope carries, which only a server naming
/// a shorter one replaces.
const RUNTIME_HEADER_TIMEOUT: Duration = Duration::from_millis(100);
/// The header boundary one server names beneath the runtime's own.
const SERVER_HEADER_TIMEOUT: Duration = Duration::from_millis(50);
/// The request deadlines the runtime envelope carries, which no row narrows away.
const RUNTIME_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The deadline the runtime's own teardown shares with its servers.
const RUNTIME_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

/// The header boundary an ordering row writes as a single field.
const FIELD_HEADER_TIMEOUT: Duration = Duration::from_millis(250);
/// The header boundary an ordering row writes as part of a whole policy.
const WHOLESALE_HEADER_TIMEOUT: Duration = Duration::from_millis(400);
/// The body-idle deadline only the wholesale policy carries.
const WHOLESALE_BODY_IDLE: Duration = Duration::from_secs(3);
/// The request-total deadline only the wholesale policy carries.
const WHOLESALE_TOTAL: Duration = Duration::from_secs(4);

/// One row of the precedence table: what was configured, and what must reach
/// the production owners that resolve it.
struct PolicyRow {
    name: &'static str,
    policy: ServerPolicy,
    expected_header: Duration,
    expected_request: RequestBudget,
    expected_upload: TransferBudget,
    expected_download: TransferBudget,
}

fn budget_route() -> Router {
    let mut router = Router::new();
    router.get("/budgets", |_req: &Request| async {
        Response::text(200, "budgets")
    });
    router
}

async fn wait_paused(controller: &LifecycleController, checkpoint: LifecycleCheckpoint, row: &str) {
    tokio::time::timeout(EVENT_TIMEOUT, controller.wait_until_paused(checkpoint))
        .await
        .unwrap_or_else(|_| panic!("{row}: {checkpoint:?} was never reached"))
        .unwrap_or_else(|error| panic!("{row}: waiting for {checkpoint:?} failed: {error}"));
}

/// Serve one row and prove the production owners resolved exactly its values.
///
/// Both checkpoints are armed before anything connects, and each is armed with
/// the exact value expected: a server that resolved a different header timeout
/// or a different budget never pauses, and the bounded wait reports that as the
/// failure it is. The values are read from the connection owner that configures
/// Hyper and from the routing owner that resolves the budgets — no helper here
/// recomputes either.
async fn assert_row(row: PolicyRow, hosts: Option<HostRouter>) {
    let port = http_support::reserve_observed();
    let (listener, addr, controller) = port.into_owned_parts();

    controller
        .pause_once(LifecycleCheckpoint::HeaderTimeoutConfigured(
            row.expected_header,
        ))
        .expect("arm the header-timeout observation");
    controller
        .pause_once(LifecycleCheckpoint::RouteBudgetsResolved {
            request: row.expected_request,
            upload: row.expected_upload,
            download: row.expected_download,
        })
        .expect("arm the resolved-budget observation");

    let handle = match hosts {
        Some(hosts) => camber::http::server_hosts(hosts).policy(row.policy),
        None => camber::http::server(budget_route()).policy(row.policy),
    }
    .serve_background(listener)
    .expect("owned server requires a Tokio runtime");
    let server = http_support::ReadyServer::adopt(addr, handle);

    let request = tokio::spawn(async move {
        reqwest::Client::new()
            .get(format!("http://{addr}/budgets"))
            .send()
            .await
    });

    wait_paused(
        &controller,
        LifecycleCheckpoint::HeaderTimeoutConfigured(row.expected_header),
        row.name,
    )
    .await;
    controller
        .release(LifecycleCheckpoint::HeaderTimeoutConfigured(
            row.expected_header,
        ))
        .expect("release the header-timeout observation");

    let budgets = LifecycleCheckpoint::RouteBudgetsResolved {
        request: row.expected_request,
        upload: row.expected_upload,
        download: row.expected_download,
    };
    wait_paused(&controller, budgets, row.name).await;
    controller
        .release(budgets)
        .expect("release the resolved-budget observation");

    let response = tokio::time::timeout(EVENT_TIMEOUT, request)
        .await
        .unwrap_or_else(|_| panic!("{}: the probe never completed", row.name))
        .unwrap_or_else(|error| panic!("{}: the probe task failed: {error}", row.name))
        .unwrap_or_else(|error| panic!("{}: the probe request failed: {error}", row.name));
    assert_eq!(response.status().as_u16(), 200, "{}", row.name);

    server
        .shutdown_bounded(SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("{}: teardown failed: {error}", row.name));
}

/// A server that configures nothing inherits the runtime's bounds, and its own
/// longer defaults cannot widen them.
async fn assert_server_inherits_the_runtime() {
    assert_row(
        PolicyRow {
            name: "inherit the runtime",
            policy: ServerPolicy::default(),
            expected_header: RUNTIME_HEADER_TIMEOUT,
            expected_request: RequestBudget::bounded(
                RUNTIME_REQUEST_TIMEOUT,
                RUNTIME_REQUEST_TIMEOUT,
            )
            .expect("the runtime's default request budget"),
            expected_upload: TransferBudget::unbounded(),
            expected_download: TransferBudget::unbounded(),
        },
        None,
    )
    .await;
}

/// A server that names a header boundary shorter than the runtime's wins that
/// dimension, and wins only that dimension.
///
/// This is the header row the other direction: every other row leaves the
/// runtime holding the shorter value, so a resolver that read the outer header
/// boundary and discarded the server's would pass them all. Here the server's
/// fifty milliseconds is the only value that can reach the connection owner,
/// while the request budget beside it must still be the runtime's.
async fn assert_the_server_header_narrows_the_runtime() {
    assert_row(
        PolicyRow {
            name: "server header narrows the runtime",
            policy: ServerPolicy::default()
                .header_timeout(SERVER_HEADER_TIMEOUT)
                .expect("a header timeout narrower than the runtime's"),
            expected_header: SERVER_HEADER_TIMEOUT,
            expected_request: RequestBudget::bounded(
                RUNTIME_REQUEST_TIMEOUT,
                RUNTIME_REQUEST_TIMEOUT,
            )
            .expect("the runtime's request budget stands beside the narrowed header"),
            expected_upload: TransferBudget::unbounded(),
            expected_download: TransferBudget::unbounded(),
        },
        None,
    )
    .await;
}

/// A server narrows every dimension it names, and a longer one it names is
/// still capped by the runtime's.
async fn assert_server_narrows_the_runtime() {
    assert_row(
        PolicyRow {
            name: "server narrows",
            policy: ServerPolicy::default()
                .header_timeout(Duration::from_secs(30))
                .expect("a header timeout wider than the runtime's")
                .request_budget(
                    RequestBudget::bounded(Duration::from_secs(10), Duration::from_secs(20))
                        .expect("a narrower request budget"),
                )
                .upload_budget(
                    TransferBudget::unbounded()
                        .with_max_bytes(4096)
                        .expect("a finite upload maximum"),
                ),
            expected_header: RUNTIME_HEADER_TIMEOUT,
            expected_request: RequestBudget::bounded(
                Duration::from_secs(10),
                Duration::from_secs(20),
            )
            .expect("the server's request budget"),
            expected_upload: TransferBudget::unbounded()
                .with_max_bytes(4096)
                .expect("the server's upload maximum"),
            expected_download: TransferBudget::unbounded(),
        },
        None,
    )
    .await;
}

/// Host and child layers narrow further, per dimension, and an explicitly
/// unbounded inner value inherits rather than erasing what contains it.
async fn assert_host_and_child_narrow_the_server() {
    let child = budget_route()
        .request_budget(RequestBudget::unbounded())
        .upload_budget(
            TransferBudget::unbounded()
                .with_max_bytes(1024)
                .expect("the child's upload maximum"),
        )
        .download_budget(
            TransferBudget::unbounded()
                .with_idle(Duration::from_secs(2))
                .expect("the child's download idle bound"),
        );
    let mut hosts = HostRouter::new();
    hosts.set_default(child);
    let hosts = hosts
        .request_budget(
            RequestBudget::unbounded()
                .with_total(Duration::from_secs(5))
                .expect("the host's request total"),
        )
        .upload_budget(
            TransferBudget::unbounded()
                .with_max_bytes(2048)
                .expect("the host's upload maximum"),
        );

    assert_row(
        PolicyRow {
            name: "host and child narrow",
            policy: ServerPolicy::default()
                .request_budget(
                    RequestBudget::bounded(Duration::from_secs(10), Duration::from_secs(20))
                        .expect("the server's request budget"),
                )
                .upload_budget(
                    TransferBudget::unbounded()
                        .with_max_bytes(4096)
                        .expect("the server's upload maximum"),
                ),
            expected_header: RUNTIME_HEADER_TIMEOUT,
            // body_idle: only the server named one. total: the host's five
            // seconds beat the server's twenty, and the child's unbounded
            // request budget erased neither.
            expected_request: RequestBudget::bounded(
                Duration::from_secs(10),
                Duration::from_secs(5),
            )
            .expect("the narrowed request budget"),
            // The smallest finite maximum in the chain wins.
            expected_upload: TransferBudget::unbounded()
                .with_max_bytes(1024)
                .expect("the child's upload maximum"),
            // Nothing above the child bounded the download, so its own idle
            // bound stands alone.
            expected_download: TransferBudget::unbounded()
                .with_idle(Duration::from_secs(2))
                .expect("the child's download idle bound"),
        },
        Some(hosts),
    )
    .await;
}

/// The outer envelope every table row serves under.
///
/// Written as one whole policy so the runtime layer of this table enters
/// through `RuntimeBuilder::server_policy` — the setter this step publishes —
/// rather than through a test-only config the public API never reaches.
fn runtime_envelope() -> ServerPolicy {
    ServerPolicy::default()
        .header_timeout(RUNTIME_HEADER_TIMEOUT)
        .expect("the runtime's header boundary")
        .request_budget(
            RequestBudget::bounded(RUNTIME_REQUEST_TIMEOUT, RUNTIME_REQUEST_TIMEOUT)
                .expect("the runtime's request budget"),
        )
        .shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT)
        .expect("the runtime's shutdown deadline")
}

/// The whole policy both ordering rows write, and never the value a single-field
/// setter writes.
///
/// Its request budget is what makes the second row discriminating: only a
/// `header_timeout` that wrote onto this stored value — rather than replacing it
/// — leaves these deadlines standing.
fn ordering_envelope() -> ServerPolicy {
    ServerPolicy::default()
        .header_timeout(WHOLESALE_HEADER_TIMEOUT)
        .expect("the wholesale header boundary")
        .request_budget(
            RequestBudget::bounded(WHOLESALE_BODY_IDLE, WHOLESALE_TOTAL)
                .expect("the wholesale request budget"),
        )
        .shutdown_timeout(RUNTIME_SHUTDOWN_TIMEOUT)
        .expect("the ordering runtime's shutdown deadline")
}

/// The row both ordering cases serve: a server that names nothing, so every
/// value it resolves came from the runtime.
fn ordering_row(name: &'static str, expected_header: Duration) -> PolicyRow {
    PolicyRow {
        name,
        policy: ServerPolicy::default(),
        expected_header,
        expected_request: RequestBudget::bounded(WHOLESALE_BODY_IDLE, WHOLESALE_TOTAL)
            .expect("the wholesale request budget reaches the server"),
        expected_upload: TransferBudget::unbounded(),
        expected_download: TransferBudget::unbounded(),
    }
}

/// `server_policy` replaces the whole envelope, so a field written before it is
/// a field it overwrites.
///
/// A `server_policy` that never reached the stored config would leave the
/// single field in place, so the observation armed on the whole policy's
/// boundary would never be reached and the bounded wait would report it.
fn assert_a_whole_policy_replaces_an_earlier_field() {
    camber::runtime::builder()
        .header_timeout(FIELD_HEADER_TIMEOUT)
        .server_policy(ordering_envelope())
        .run(|| {
            camber::runtime::block_on(assert_row(
                ordering_row(
                    "whole policy replaces an earlier field",
                    WHOLESALE_HEADER_TIMEOUT,
                ),
                None,
            ));
        })
        .expect("the replacing-order runtime ran");
}

/// A single-field setter after it writes onto that same value: its own field
/// wins, and every other dimension the whole policy set still stands.
fn assert_a_later_field_writes_onto_the_whole_policy() {
    camber::runtime::builder()
        .server_policy(ordering_envelope())
        .header_timeout(FIELD_HEADER_TIMEOUT)
        .run(|| {
            camber::runtime::block_on(assert_row(
                ordering_row(
                    "later field writes onto the whole policy",
                    FIELD_HEADER_TIMEOUT,
                ),
                None,
            ));
        })
        .expect("the writing-order runtime ran");
}

/// 2.T1
///
/// The runtime layer enters through `RuntimeBuilder`, so the outer envelope
/// every row narrows is the one a caller of the public API would have.
#[test]
fn nested_policies_only_narrow_outer_finite_limits() {
    camber::runtime::builder()
        .server_policy(runtime_envelope())
        .run(|| {
            camber::runtime::block_on(async {
                assert_server_inherits_the_runtime().await;
                assert_the_server_header_narrows_the_runtime().await;
                assert_server_narrows_the_runtime().await;
                assert_host_and_child_narrow_the_server().await;
            });
        })
        .expect("the precedence runtime ran");

    // Call order decides which write survives, so each order is its own runtime.
    assert_a_whole_policy_replaces_an_earlier_field();
    assert_a_later_field_writes_onto_the_whole_policy();
}

// ── 6.T3 and 6.T5 ──────────────────────────────────────────────────

use camber::http::mock::{InboundTerminal, OperationObservation};
use camber::http::{BodyAdmission, BodyAdmissionContext, Rejection, RejectionContext};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The request deadlines every envelope row is admitted under.
const CARRIED_BODY_IDLE: Duration = Duration::from_millis(400);
const CARRIED_TOTAL: Duration = Duration::from_millis(800);
/// The quiet interval and total a precedence row stages equal-ready.
const STAGED_IDLE: Duration = Duration::from_millis(120);
const STAGED_TOTAL: Duration = Duration::from_millis(120);
/// The byte ceiling a precedence row crosses.
const STAGED_CEILING: usize = 8;

/// The policy every envelope and precedence row serves under.
fn carried_policy(budget: RequestBudget) -> ServerPolicy {
    ServerPolicy::default()
        .header_timeout(Duration::from_secs(30))
        .expect("a header boundary no carried row reaches")
        .request_budget(budget)
        .shutdown_timeout(SHUTDOWN_TIMEOUT)
        .expect("the carried row's shutdown deadline")
}

/// The routes the envelope rows are driven through.
fn carried_routes(middleware_runs: &Arc<AtomicUsize>) -> Router {
    let runs = Arc::clone(middleware_runs);
    let mut router = Router::new();
    router.post("/buffered", |_req: &Request| async {
        Response::text(200, "buffered")
    });
    router.get("/head-only", |_req: &Request| async {
        Response::text(200, "head-only")
    });
    router.use_middleware(move |req: &Request, next| {
        runs.fetch_add(1, Ordering::SeqCst);
        next.call(req)
    });
    router
}

/// One admitted row's envelope reading, taken from the production owners.
fn observed_envelope(controller: &LifecycleController) -> OperationObservation {
    controller.operations_observed()
}

/// Drive one admitted request and read what its envelope reached.
async fn assert_one_envelope(method: &str, path: &str, body: &[u8], row: &str) {
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let runs = Arc::new(AtomicUsize::new(0));
    let server = port.serve_with_policy(
        carried_routes(&runs),
        carried_policy(
            RequestBudget::bounded(CARRIED_BODY_IDLE, CARRIED_TOTAL)
                .expect("the carried request budget"),
        ),
    );
    let addr = server.addr();
    let method: Box<str> = method.into();
    let path: Box<str> = path.into();
    let body: Box<[u8]> = body.into();
    let answered = tokio::task::spawn_blocking(move || {
        http_support::request(addr, &method, &path, &[], &body, EVENT_TIMEOUT)
    })
    .await
    .unwrap_or_else(|error| panic!("{row}: the peer task failed: {error}"))
    .unwrap_or_else(|error| panic!("{row}: the request failed: {error}"));
    assert_eq!(answered.status, 200, "{row}: {}", answered.text());

    let observed = observed_envelope(&controller);
    assert_eq!(
        observed.admitted, 1,
        "{row}: one admitted head, one envelope"
    );
    assert_eq!(
        observed.distinct_identities, 1,
        "{row}: every owner must read the one identity the mint published",
    );
    assert_eq!(
        observed.distinct_totals, 1,
        "{row}: every owner must read one request-total identity",
    );
    assert_eq!(
        observed.total_from_admission,
        Some(CARRIED_TOTAL),
        "{row}: the carried total is the policy's, computed once at admission",
    );
    assert!(observed.dispatch >= 1, "{row}: dispatch read no identity");
    assert_eq!(
        observed.middleware, 1,
        "{row}: exactly one middleware owner per admitted request",
    );
    assert_eq!(
        observed.response_head, 1,
        "{row}: exactly one response-head handoff per admitted request",
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "{row}: exactly one middleware execution per admitted request",
    );

    server
        .shutdown_bounded(SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("{row}: teardown failed: {error}"));
}

/// A refusal decided before route authority exists mints no envelope.
///
/// Served through a host table, because that is the shape whose authority can
/// fail to resolve at all: a single router claims every authority, so a head it
/// cannot match is still a head it selected a policy for.
async fn assert_negative_control(head: &[u8], row: &str) {
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let runs = Arc::new(AtomicUsize::new(0));
    let mut hosts = HostRouter::new();
    hosts.set_default(carried_routes(&runs));
    let server = port.serve_hosts(hosts);
    let addr = server.addr();
    let head: Box<[u8]> = head.into();
    tokio::task::spawn_blocking(move || {
        use std::io::Write as _;
        let mut peer = http_support::connect(addr).expect("the control peer connected");
        peer.write_all(&head).expect("write the refused head");
        peer.flush().expect("flush the refused head");
        let _answered = http_support::read_http_response_bounded(&mut peer);
    })
    .await
    .unwrap_or_else(|error| panic!("{row}: the control peer failed: {error}"));

    let observed = observed_envelope(&controller);
    assert_eq!(
        observed.admitted, 0,
        "{row}: a head that resolved no route authority must expose no operation",
    );
    assert_eq!(
        observed.identity, None,
        "{row}: no owner may read an identity for a head that minted none",
    );
    assert_eq!(
        runs.load(Ordering::SeqCst),
        0,
        "{row}: a refused head runs no middleware",
    );

    server
        .shutdown_bounded(SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("{row}: teardown failed: {error}"));
}

/// 6.T3
///
/// Each admitted head mints exactly one envelope, and dispatch, middleware, the
/// payload owner, and the response-head handoff all read that same identity and
/// the same request-total value. Two deterministic negative controls sit beside
/// the admitted rows: a head Hyper never resolved a route for exposes no
/// operation at all, and neither does one whose authority Camber cannot parse.
#[test]
fn admitted_operation_carries_one_envelope_to_each_prehead_owner() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_one_envelope("POST", "/buffered", b"payload", "ordinary buffered").await;
                assert_one_envelope("GET", "/head-only", b"", "head-only").await;
                // Hyper refuses this head before a request exists at all.
                assert_negative_control(
                    b"GET /buffered HTTP/1.1\r\nHost: localhost\r\nBad Header\r\n\r\n",
                    "hyper pre-head refusal",
                )
                .await;
                // Classification refuses this one: no route authority resolves,
                // so nothing selects a policy an operation could carry.
                // Classification refuses this one: the authority Camber cannot
                // parse resolves no router, so nothing selects a policy an
                // operation could carry.
                assert_negative_control(
                    b"GET /buffered HTTP/1.1\r\nHost: bad host\r\nConnection: close\r\n\r\n",
                    "unresolved route",
                )
                .await;
            });
        })
        .expect("the carried-envelope runtime ran");
}

/// The route a precedence row stages its terminals on.
fn staged_routes(released: &Arc<AtomicUsize>, log: &Arc<AtomicUsize>) -> Router {
    let released = Arc::clone(released);
    let log = Arc::clone(log);
    let mut router = Router::new();
    router.post("/staged", |_req: &Request| async {
        Response::text(200, "staged")
    });
    router
        .body_admission(move |_context: &BodyAdmissionContext<'_>| {
            Ok(BodyAdmission::with_permit(
                STAGED_CEILING,
                http_support::permit_probe(&released),
            ))
        })
        .rejection_mapper(move |rejection: &Rejection, _: &RejectionContext| {
            log.fetch_add(1, Ordering::SeqCst);
            Response::text(rejection.status(), rejection.message())
        })
}

/// Stage one turn's ready sources through the production checkpoint, and read
/// back the terminal the declared precedence selected.
///
/// The coordinator is held at its own pre-selection checkpoint, so everything
/// this row makes ready while it waits is weighed by the same turn. Nothing
/// here chooses the terminal: the release is the only thing staged, and the
/// selection checkpoint the row waits on is production's own.
///
/// The body is chunked, so route admission has no declaration to refuse and the
/// byte maximum is decided by the frames the coordinator actually reads.
async fn assert_precedence_row(
    crossing: Option<Box<[u8]>>,
    expected: InboundTerminal,
    answers: u16,
) {
    let row = format!("{expected:?}");
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let released = Arc::new(AtomicUsize::new(0));
    let mapper_calls = Arc::new(AtomicUsize::new(0));
    let server = port.serve_with_policy(
        staged_routes(&released, &mapper_calls),
        carried_policy(
            RequestBudget::bounded(STAGED_IDLE, STAGED_TOTAL).expect("the staged request budget"),
        ),
    );
    let addr = server.addr();

    controller
        .pause_once(LifecycleCheckpoint::BeforeInboundTerminalSelection)
        .expect("arm the pre-selection checkpoint");
    controller
        .pause_once(LifecycleCheckpoint::InboundTerminalSelected(expected))
        .expect("arm the selected-terminal observation");

    let peer = tokio::task::spawn_blocking(move || staged_peer(addr, crossing));

    wait_paused(
        &controller,
        LifecycleCheckpoint::BeforeInboundTerminalSelection,
        &row,
    )
    .await;
    // Staged while the coordinator is held: every source this row wants weighed
    // is ready before the turn that weighs them begins.
    tokio::time::sleep(STAGED_IDLE * 2).await;
    controller
        .release(LifecycleCheckpoint::BeforeInboundTerminalSelection)
        .expect("release the pre-selection checkpoint");

    let selected = LifecycleCheckpoint::InboundTerminalSelected(expected);
    wait_paused(&controller, selected, &row).await;
    let polled = controller.body_frames_polled();
    controller
        .release(selected)
        .expect("release the selected-terminal observation");

    let answered = tokio::time::timeout(EVENT_TIMEOUT, peer)
        .await
        .unwrap_or_else(|_| panic!("{row}: the staged peer never settled"))
        .unwrap_or_else(|error| panic!("{row}: the staged peer task failed: {error}"));
    assert_eq!(
        answered, answers,
        "{row}: the staged terminal answered {answered}"
    );
    assert_eq!(
        controller.body_frames_polled(),
        polled,
        "{row}: no frame may be polled after a terminal is selected",
    );
    assert_eq!(
        mapper_calls.load(Ordering::SeqCst),
        1,
        "{row}: a pre-commit failure invokes the selected mapper exactly once",
    );
    http_support::assert_released(&released, 1, &row);

    server
        .shutdown_bounded(SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("{row}: teardown failed: {error}"));
}

/// Drive one staged peer: a chunked head, an optional crossing frame, then the
/// status the server answered with.
fn staged_peer(addr: std::net::SocketAddr, crossing: Option<Box<[u8]>>) -> u16 {
    let mut peer = http_support::connect(addr).expect("the staged peer connected");
    http_support::write_chunked_head(&mut peer, "close", "POST", "/staged", "localhost")
        .expect("write the staged chunked head");
    match crossing.as_deref() {
        Some(frame) => {
            http_support::write_chunk(&mut peer, frame).expect("write the crossing frame")
        }
        None => {}
    }
    http_support::read_http_response_bounded(&mut peer)
        .expect("the staged request was answered")
        .status
}

/// 6.T5
///
/// Sources that become ready in one scheduling turn are resolved by the one
/// declared precedence table, not by poll order. Each row holds the production
/// coordinator at its pre-selection checkpoint, makes every source it wants
/// weighed ready while it waits, and then reads back the terminal production
/// selected.
///
/// The rows are the pairs this step's own sources can reach. A crossing frame
/// arriving after the quiet interval expired ties the route byte maximum
/// against the body-idle deadline, and a withheld body ties body idle against
/// the request total. Step 11 extends the same selector with its transfer
/// sources once their adapters exist.
#[test]
fn equal_ready_inbound_events_follow_the_declared_precedence() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                // The crossing frame is already on the wire when the turn runs,
                // and the quiet interval expired while the turn was held.
                assert_precedence_row(
                    Some(vec![b'x'; STAGED_CEILING * 4].into_boxed_slice()),
                    InboundTerminal::RouteBodyLimit,
                    413,
                )
                .await;
                // Nothing is on the wire, so the two carried deadlines are the
                // whole of the turn's ready set and idle outranks total.
                assert_precedence_row(None, InboundTerminal::BodyIdle, 408).await;
            });
        })
        .expect("the precedence runtime ran");
}
