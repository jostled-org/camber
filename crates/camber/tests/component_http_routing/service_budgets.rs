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

    let configured = LifecycleCheckpoint::HeaderTimeoutConfigured(row.expected_header);
    http_support::wait_until_paused_bounded(
        &controller,
        configured,
        &format!("{}: {configured:?}", row.name),
    )
    .await;
    controller
        .release(configured)
        .expect("release the header-timeout observation");

    let budgets = LifecycleCheckpoint::RouteBudgetsResolved {
        request: row.expected_request,
        upload: row.expected_upload,
        download: row.expected_download,
    };
    http_support::wait_until_paused_bounded(
        &controller,
        budgets,
        &format!("{}: {budgets:?}", row.name),
    )
    .await;
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

use crate::streaming_multipart::{BOUNDARY, Field, content_type, multipart_body};
use camber::http::mock::{InboundTerminal, OperationObservation};
use camber::http::{
    BodyAdmission, BodyAdmissionContext, Method, MultipartLimits, MultipartStream, Rejection,
    RejectionContext, ServerHandle,
};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

/// The request deadlines every envelope row is admitted under.
const CARRIED_BODY_IDLE: Duration = Duration::from_secs(2);
const CARRIED_TOTAL: Duration = Duration::from_secs(4);
/// The request total the host layer of the nested row names.
const HOST_TOTAL: Duration = Duration::from_secs(2);
/// The request total the child router beneath it names, which is the one the
/// admitted envelope must carry.
const CHILD_TOTAL: Duration = Duration::from_secs(1);
/// The quiet interval and total a precedence row stages equal-ready.
const STAGED_IDLE: Duration = Duration::from_millis(120);
const STAGED_TOTAL: Duration = Duration::from_millis(120);
/// A deadline a precedence row carries so nothing except the source it stages
/// can end the request.
const STAGED_UNREACHED: Duration = Duration::from_secs(30);
/// The aggregate shutdown deadline the silent rows carry.
const STAGED_SHUTDOWN: Duration = Duration::from_secs(1);
/// How long a staged row leaves production to read a transition of its own
/// before the row stages the next one.
const STAGED_SETTLE: Duration = Duration::from_millis(250);
/// The byte ceiling a precedence row crosses.
const STAGED_CEILING: usize = 8;

/// The backend the two negative controls register and never dial.
///
/// Both are refused before any route authority resolves, so the proxy route in
/// the table they serve is never selected. Port nine is the discard service, so
/// a control that did dial it would fail rather than quietly succeed.
const UNDIALLED_UPSTREAM: &str = "http://127.0.0.1:9";

/// The policy every envelope and precedence row serves under.
fn carried_policy(budget: RequestBudget, shutdown: Duration) -> ServerPolicy {
    ServerPolicy::default()
        .header_timeout(Duration::from_secs(30))
        .expect("a header boundary no carried row reaches")
        .request_budget(budget)
        .shutdown_timeout(shutdown)
        .expect("the carried row's shutdown deadline")
}

/// The routes the envelope rows are driven through.
///
/// Every admitted class this step wires is registered here, because each reads
/// the envelope from call sites of its own: buffered collection, the streaming
/// multipart session, and the streaming proxy each observe their own payload
/// owner and their own response-head handoff. One table is what lets a row about
/// carry name the class it drove.
fn carried_routes(middleware_runs: &Arc<AtomicUsize>, upstream: &str) -> Router {
    let runs = Arc::clone(middleware_runs);
    let mut router = Router::new();
    router.post("/buffered", |_req: &Request| async {
        Response::text(200, "buffered")
    });
    router.get("/head-only", |_req: &Request| async {
        Response::text(200, "head-only")
    });
    router.multipart(
        Method::Post,
        "/upload",
        MultipartLimits::builder()
            .build()
            .expect("the multipart row's limits"),
        |_req: &Request, fields: MultipartStream| async move {
            let mut fields = fields;
            while let Some(field) = fields.next_field().await? {
                field.discard().await?;
            }
            Response::text(200, "uploaded")
        },
    );
    router.proxy_stream("/proxied", upstream);
    router.use_middleware(move |req: &Request, next| {
        runs.fetch_add(1, Ordering::SeqCst);
        next.call(req)
    });
    router
}

/// The upstream the served table's streaming-proxy route forwards to.
fn carried_upstream() -> http_support::ReadyServer {
    let mut upstream = Router::new();
    upstream.post("/echo", |req: &Request| {
        let len = req.body().len();
        async move { Response::text(200, &format!("echoed {len}")) }
    });
    http_support::spawn_server_ready(upstream, EVENT_TIMEOUT)
        .expect("the carried row's upstream answered")
}

/// One admitted row's envelope reading, taken from the production owners.
fn observed_envelope(controller: &LifecycleController) -> OperationObservation {
    controller.operations_observed()
}

/// What one admitted row must be able to say about the envelope its head minted.
#[derive(Clone, Copy)]
struct CarriedClaim<'a> {
    row: &'a str,
    /// The request total the whole policy chain resolved to for this row.
    total: Duration,
    /// How many times the payload owner read the envelope.
    ///
    /// Named per row rather than fixed: the payload owner is the one pre-head
    /// owner a request can reach zero times, so a shared count could not tell an
    /// owner that read from one that never ran.
    body_reads: usize,
}

/// Read the one envelope back from every production owner that carried it.
fn assert_carried_envelope(
    controller: &LifecycleController,
    runs: &Arc<AtomicUsize>,
    claim: CarriedClaim<'_>,
) {
    let CarriedClaim {
        row,
        total,
        body_reads,
    } = claim;
    let observed = observed_envelope(controller);
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
        Some(total),
        "{row}: the carried total is the resolved policy's, computed once at admission",
    );
    assert!(observed.dispatch >= 1, "{row}: dispatch read no identity");
    assert_eq!(
        observed.middleware, 1,
        "{row}: exactly one middleware owner per admitted request",
    );
    assert_eq!(
        observed.body, body_reads,
        "{row}: the payload owner must read the same envelope every other owner did",
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
}

/// One admitted request an envelope row drives through the served table.
struct AdmittedRequest<'a> {
    method: &'a str,
    path: &'a str,
    headers: &'a [(&'a str, &'a str)],
    body: &'a [u8],
}

/// Send one admitted request from a peer of its own, and assert it was answered.
async fn answer_admitted(addr: SocketAddr, request: &AdmittedRequest<'_>, row: &str) {
    let method: Box<str> = request.method.into();
    let path: Box<str> = request.path.into();
    let headers: Box<[(Box<str>, Box<str>)]> = request
        .headers
        .iter()
        .map(|(name, value)| ((*name).into(), (*value).into()))
        .collect();
    let body: Box<[u8]> = request.body.into();
    let answered = tokio::task::spawn_blocking(move || {
        let sent: Box<[(&str, &str)]> = headers
            .iter()
            .map(|(name, value)| (name.as_ref(), value.as_ref()))
            .collect();
        http_support::request(addr, &method, &path, &sent, &body, EVENT_TIMEOUT)
    })
    .await
    .unwrap_or_else(|error| panic!("{row}: the peer task failed: {error}"))
    .unwrap_or_else(|error| panic!("{row}: the request failed: {error}"));
    assert_eq!(answered.status, 200, "{row}: {}", answered.text());
}

/// Drive one admitted request through a served router and read what its
/// envelope reached.
async fn assert_one_envelope(request: AdmittedRequest<'_>, body_reads: usize, row: &str) {
    let upstream = carried_upstream();
    let backend = format!("http://{}", upstream.local_addr());
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let runs = Arc::new(AtomicUsize::new(0));
    let server = port.serve_with_policy(
        carried_routes(&runs, &backend),
        carried_policy(
            RequestBudget::bounded(CARRIED_BODY_IDLE, CARRIED_TOTAL)
                .expect("the carried request budget"),
            SHUTDOWN_TIMEOUT,
        ),
    );
    answer_admitted(server.addr(), &request, row).await;
    assert_carried_envelope(
        &controller,
        &runs,
        CarriedClaim {
            row,
            total: CARRIED_TOTAL,
            body_reads,
        },
    );

    server
        .shutdown_bounded(SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("{row}: teardown failed: {error}"));
    upstream
        .shutdown_bounded(SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("{row}: upstream teardown failed: {error}"));
}

/// An admitted head that resolved through a host table and a child router
/// carries the narrowed authority, not the one its server named.
///
/// The child's total is the smallest in the chain, so it is the only value the
/// envelope can be carrying: a mint that read the server's policy, or the host's,
/// reports a different total to every owner that reads it.
async fn assert_host_child_envelope() {
    let row = "host and child policy";
    let upstream = carried_upstream();
    let backend = format!("http://{}", upstream.local_addr());
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let runs = Arc::new(AtomicUsize::new(0));
    let child = carried_routes(&runs, &backend).request_budget(
        RequestBudget::unbounded()
            .with_total(CHILD_TOTAL)
            .expect("the child's request total"),
    );
    let mut hosts = HostRouter::new();
    hosts.set_default(child);
    let hosts = hosts.request_budget(
        RequestBudget::unbounded()
            .with_total(HOST_TOTAL)
            .expect("the host's request total"),
    );
    let server = port.serve_hosts(hosts);
    answer_admitted(
        server.addr(),
        &AdmittedRequest {
            method: "POST",
            path: "/buffered",
            headers: &[],
            body: b"payload",
        },
        row,
    )
    .await;
    assert_carried_envelope(
        &controller,
        &runs,
        CarriedClaim {
            row,
            total: CHILD_TOTAL,
            body_reads: 1,
        },
    );

    server
        .shutdown_bounded(SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("{row}: teardown failed: {error}"));
    upstream
        .shutdown_bounded(SHUTDOWN_TIMEOUT)
        .unwrap_or_else(|error| panic!("{row}: upstream teardown failed: {error}"));
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
    hosts.set_default(carried_routes(&runs, UNDIALLED_UPSTREAM));
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
/// the same request-total value. The four admitted shapes this step wires are
/// driven in turn — ordinary buffered, bodyless, streaming multipart, and
/// streaming proxy — and a fifth row resolves its authority through a host table
/// and a child router, so what the envelope carries is the narrowed value. Two
/// deterministic negative controls sit beside them: a head Hyper never resolved
/// a route for exposes no operation at all, and neither does one whose authority
/// Camber cannot parse.
#[test]
fn admitted_operation_carries_one_envelope_to_each_prehead_owner() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_one_envelope(
                    AdmittedRequest {
                        method: "POST",
                        path: "/buffered",
                        headers: &[],
                        body: b"payload",
                    },
                    1,
                    "ordinary buffered",
                )
                .await;
                assert_one_envelope(
                    AdmittedRequest {
                        method: "GET",
                        path: "/head-only",
                        headers: &[],
                        body: b"",
                    },
                    1,
                    "head-only",
                )
                .await;
                let declared = content_type(BOUNDARY);
                let framed = multipart_body(BOUNDARY, &[Field::text("carried", "payload")]);
                assert_one_envelope(
                    AdmittedRequest {
                        method: "POST",
                        path: "/upload",
                        headers: &[("Content-Type", declared.as_ref())],
                        body: &framed,
                    },
                    1,
                    "streaming multipart",
                )
                .await;
                assert_one_envelope(
                    AdmittedRequest {
                        method: "POST",
                        path: "/proxied/echo",
                        headers: &[],
                        body: b"forwarded",
                    },
                    1,
                    "streaming proxy",
                )
                .await;
                assert_host_child_envelope().await;
                // Hyper refuses this head before a request exists at all.
                assert_negative_control(
                    b"GET /buffered HTTP/1.1\r\nHost: localhost\r\nBad Header\r\n\r\n",
                    "hyper pre-head refusal",
                )
                .await;
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

/// What one staged peer puts on the wire after its request head.
///
/// The bodies are chunked, so route admission has no declaration to refuse and
/// the byte maximum is decided by the frames the coordinator actually reads.
enum StagedWire {
    /// Nothing at all: the row's carried deadlines are its whole ready set.
    Withheld,
    /// One data frame the route's byte ceiling must refuse.
    Crossing,
    /// A body that ends cleanly, so the payload is complete.
    Complete,
    /// A body with no data frame at all, so the only thing the wire can answer
    /// with in the staged turn is its end.
    Ended,
    /// Framing the transport cannot parse, which is the source's own failure.
    Broken,
    /// One data frame, written when the row says so.
    ///
    /// The only way to put a turn boundary where a row needs one: a source the
    /// coordinator reads on its own decides when its turns happen, so a row that
    /// stages what a later turn must weigh has to own that moment.
    Gated(std::sync::mpsc::Receiver<()>),
}

/// Write the head this row's wire is framed by.
fn write_staged_head(peer: &mut TcpStream, wire: &StagedWire) {
    match wire {
        // Its own head: the framing this row breaks is declared by the same
        // helper every unreadable-body row in the tree sends.
        StagedWire::Broken => {
            http_support::write_unreadable_body(peer, "close", "POST", "/staged", "text/plain")
                .expect("write framing the transport cannot parse");
        }
        StagedWire::Withheld
        | StagedWire::Crossing
        | StagedWire::Complete
        | StagedWire::Ended
        | StagedWire::Gated(_) => {
            http_support::write_chunked_head(peer, "close", "POST", "/staged", "localhost")
                .expect("write the staged chunked head");
        }
    }
}

/// Write whatever this row puts on the wire after its head.
fn write_staged_body(peer: &mut TcpStream, wire: StagedWire) {
    match wire {
        StagedWire::Withheld | StagedWire::Broken => {}
        StagedWire::Crossing => {
            http_support::write_chunk(peer, &vec![b'x'; STAGED_CEILING * 4])
                .expect("write the crossing frame");
        }
        StagedWire::Complete => {
            http_support::write_chunk(peer, b"staged").expect("write the completed frame");
            http_support::write_chunked_end(peer).expect("end the staged body");
        }
        StagedWire::Ended => {
            http_support::write_chunked_end(peer).expect("end the staged body");
        }
        StagedWire::Gated(gate) => {
            gate.recv().expect("the row released its staged frame");
            http_support::write_chunk(peer, b"staged").expect("write the staged frame");
        }
    }
}

/// Drive one staged peer, and hand back the status the server answered with.
fn staged_peer(addr: SocketAddr, wire: StagedWire) -> u16 {
    let mut peer = http_support::connect(addr).expect("the staged peer connected");
    write_staged_head(&mut peer, &wire);
    write_staged_body(&mut peer, wire);
    http_support::read_http_response_bounded(&mut peer)
        .expect("the staged request was answered")
        .status
}

/// What one precedence row expects the declared order to have decided.
#[derive(Clone, Copy)]
struct PrecedenceExpectation {
    terminal: InboundTerminal,
    /// The status the peer must read.
    answers: u16,
    /// How many times this terminal's disposition lets the route's own mapper
    /// run: once for a mapped cause, never for a silent one.
    mapper_calls: usize,
    /// How many data frames the coordinator had polled when it selected.
    ///
    /// It is what makes a row's pair exact rather than merely its winner: a turn
    /// that selected having polled no data frame read the payload's end, and one
    /// that polled the crossing frame weighed the route's own refusal.
    frames_polled: usize,
}

/// One staged row's served table, the observer registered for its listener, and
/// the counters production writes into.
///
/// The raw handle is kept rather than guarded, because two rows publish a
/// shutdown transition while their request is in flight, and that authority is
/// the handle's alone.
struct StagedServer {
    controller: Arc<LifecycleController>,
    handle: ServerHandle,
    addr: SocketAddr,
    released: Arc<AtomicUsize>,
    mapper_calls: Arc<AtomicUsize>,
}

/// Serve one staged row's table under the budget and shutdown deadline it names.
fn stage_server(budget: RequestBudget, shutdown: Duration) -> StagedServer {
    let port = http_support::reserve_observed();
    let (listener, addr, controller) = port.into_owned_parts();
    let released = Arc::new(AtomicUsize::new(0));
    let mapper_calls = Arc::new(AtomicUsize::new(0));
    let handle = camber::http::server(staged_routes(&released, &mapper_calls))
        .policy(carried_policy(budget, shutdown))
        .serve_background(listener)
        .expect("owned serving requires a Tokio runtime");
    StagedServer {
        controller,
        handle,
        addr,
        released,
        mapper_calls,
    }
}

impl StagedServer {
    /// Spawn this row's peer against the served table.
    fn peer(&self, wire: StagedWire) -> tokio::task::JoinHandle<u16> {
        let addr = self.addr;
        tokio::task::spawn_blocking(move || staged_peer(addr, wire))
    }

    /// Arm one checkpoint this row stages against.
    fn arm(&self, checkpoint: LifecycleCheckpoint, row: &str) {
        self.controller
            .pause_once(checkpoint)
            .unwrap_or_else(|error| panic!("{row}: arming {checkpoint:?} failed: {error}"));
    }

    /// Let go of one checkpoint this row was holding.
    fn release(&self, checkpoint: LifecycleCheckpoint, row: &str) {
        self.controller
            .release(checkpoint)
            .unwrap_or_else(|error| panic!("{row}: releasing {checkpoint:?} failed: {error}"));
    }

    /// Record one checkpoint's release without waking what waits there.
    ///
    /// The held owner stays parked until something else provokes its next poll,
    /// which is what lets a row re-arm the same checkpoint before that owner can
    /// take the turn the re-arm is meant for.
    fn stage_release(&self, checkpoint: LifecycleCheckpoint, row: &str) {
        self.controller
            .stage_release(checkpoint)
            .unwrap_or_else(|error| panic!("{row}: staging {checkpoint:?} failed: {error}"));
    }

    /// Read back the terminal production selected, and take the fixture down.
    ///
    /// Nothing here chooses the terminal. The row waits at production's own
    /// selection checkpoint, reads the frame counter production keeps while it
    /// is held there, and only then lets it go.
    async fn settle(
        self,
        peer: tokio::task::JoinHandle<u16>,
        expect: PrecedenceExpectation,
        row: &str,
    ) {
        let selected = LifecycleCheckpoint::InboundTerminalSelected(expect.terminal);
        http_support::wait_until_paused_bounded(
            &self.controller,
            selected,
            &format!("{row}: {selected:?}"),
        )
        .await;
        let polled = self.controller.body_frames_polled();
        self.release(selected, row);

        let answered = tokio::time::timeout(EVENT_TIMEOUT, peer)
            .await
            .unwrap_or_else(|_| panic!("{row}: the staged peer never settled"))
            .unwrap_or_else(|error| panic!("{row}: the staged peer task failed: {error}"));
        assert_eq!(
            answered, expect.answers,
            "{row}: the staged terminal answered {answered}"
        );
        assert_eq!(
            polled, expect.frames_polled,
            "{row}: the turn that selected had polled {polled} data frames",
        );
        assert_eq!(
            self.controller.body_frames_polled(),
            polled,
            "{row}: no frame may be polled after a terminal is selected",
        );
        assert_eq!(
            self.mapper_calls.load(Ordering::SeqCst),
            expect.mapper_calls,
            "{row}: this terminal's declared disposition allows {} mapper call(s)",
            expect.mapper_calls,
        );
        http_support::assert_released(&self.released, 1, row);

        http_support::ReadyServer::adopt(self.addr, self.handle)
            .shutdown_bounded(SHUTDOWN_TIMEOUT)
            .unwrap_or_else(|error| panic!("{row}: teardown failed: {error}"));
    }
}

/// A row whose source becomes ready without anything being held.
///
/// The wire is what decides these: a payload that ends, and framing that cannot
/// be parsed, are answers the coordinator reads in the turn it reads them, and
/// holding it would only postpone the same turn.
async fn assert_live_row(budget: RequestBudget, wire: StagedWire, expect: PrecedenceExpectation) {
    let row = format!("{:?}", expect.terminal);
    let server = stage_server(budget, SHUTDOWN_TIMEOUT);
    server.arm(
        LifecycleCheckpoint::InboundTerminalSelected(expect.terminal),
        &row,
    );
    let peer = server.peer(wire);
    server.settle(peer, expect, &row).await;
}

/// A row that holds the coordinator at its own pre-selection checkpoint and
/// makes every source it wants weighed ready while it waits.
async fn assert_held_row(budget: RequestBudget, wire: StagedWire, expect: PrecedenceExpectation) {
    let row = format!("{:?}", expect.terminal);
    let held = LifecycleCheckpoint::BeforeInboundTerminalSelection;
    let server = stage_server(budget, SHUTDOWN_TIMEOUT);
    server.arm(held, &row);
    server.arm(
        LifecycleCheckpoint::InboundTerminalSelected(expect.terminal),
        &row,
    );
    let peer = server.peer(wire);

    http_support::wait_until_paused_bounded(&server.controller, held, &format!("{row}: {held:?}"))
        .await;
    // Staged while the coordinator is held: every source this row wants weighed
    // is ready before the turn that weighs them begins.
    tokio::time::sleep(STAGED_IDLE * 2).await;
    server.release(held, &row);

    server.settle(peer, expect, &row).await;
}

/// Forced cancellation outranks both carried deadlines that expired beside it.
///
/// The supervisor is held at the control transition it selected, so the abort it
/// is about to apply cannot take this connection's task away before the
/// coordinator has weighed the cancellation its server published. What is staged
/// is the transition and the waiting; the terminal is production's.
async fn assert_forced_cancellation_row() {
    let expect = PrecedenceExpectation {
        terminal: InboundTerminal::ForcedCancellation,
        answers: 503,
        mapper_calls: 0,
        frames_polled: 0,
    };
    let row = format!("{:?}", expect.terminal);
    let supervisor = LifecycleCheckpoint::SupervisorSelectedControl;
    let held = LifecycleCheckpoint::BeforeInboundTerminalSelection;
    let server = stage_server(
        RequestBudget::bounded(STAGED_IDLE, STAGED_TOTAL).expect("the staged request budget"),
        STAGED_SHUTDOWN,
    );
    server.arm(supervisor, &row);
    server.arm(held, &row);
    server.arm(
        LifecycleCheckpoint::InboundTerminalSelected(expect.terminal),
        &row,
    );
    let peer = server.peer(StagedWire::Withheld);

    http_support::wait_until_paused_bounded(&server.controller, held, &format!("{row}: {held:?}"))
        .await;
    server.handle.cancel();
    http_support::wait_until_paused_bounded(
        &server.controller,
        supervisor,
        &format!("{row}: {supervisor:?}"),
    )
    .await;
    tokio::time::sleep(STAGED_IDLE * 2).await;
    server.release(held, &row);

    let selected = LifecycleCheckpoint::InboundTerminalSelected(expect.terminal);
    http_support::wait_until_paused_bounded(
        &server.controller,
        selected,
        &format!("{row}: {selected:?}"),
    )
    .await;
    server.release(supervisor, &row);
    server.settle(peer, expect, &row).await;
}

/// The aggregate shutdown deadline outranks the forced cancellation that became
/// ready in the same turn.
///
/// Three moments have to fall in this order for the pair to exist at all, and
/// each is anchored on production rather than on timing. The graceful transition
/// is published while the coordinator is held, so the deadline it mints is minted
/// in the turn this row releases. The peer's one frame is what opens the turn
/// held next, and it is written only after the coordinator has had that
/// transition. The cancellation is published last, while that turn is held and
/// after the minted deadline has passed.
async fn assert_shutdown_deadline_row() {
    let expect = PrecedenceExpectation {
        terminal: InboundTerminal::ShutdownDeadline,
        answers: 503,
        mapper_calls: 0,
        frames_polled: 1,
    };
    let row = format!("{:?}", expect.terminal);
    let supervisor = LifecycleCheckpoint::SupervisorSelectedControl;
    let held = LifecycleCheckpoint::BeforeInboundTerminalSelection;
    let server = stage_server(
        RequestBudget::bounded(STAGED_UNREACHED, STAGED_UNREACHED)
            .expect("deadlines the shutdown row must not reach"),
        STAGED_SHUTDOWN,
    );
    server.arm(supervisor, &row);
    server.arm(held, &row);
    server.arm(
        LifecycleCheckpoint::InboundTerminalSelected(expect.terminal),
        &row,
    );
    let (gate, staged) = std::sync::mpsc::channel();
    let peer = server.peer(StagedWire::Gated(staged));

    http_support::wait_until_paused_bounded(&server.controller, held, &format!("{row}: {held:?}"))
        .await;
    server.handle.shutdown();
    http_support::wait_until_paused_bounded(
        &server.controller,
        supervisor,
        &format!("{row}: {supervisor:?}"),
    )
    .await;

    // The release is staged rather than woken, so the coordinator is still
    // standing on it while the same checkpoint is armed for the turn after. A
    // release that woke it would leave that re-arm racing the owner, and a turn
    // the row did not stage could be the one held.
    server.stage_release(held, &row);
    server.arm(held, &row);

    // The frame is what provokes the poll that observes the staged release, so
    // the turn it opens is the one held — while the minted deadline expires and
    // the cancellation is published beside it.
    gate.send(()).expect("release the staged frame");
    http_support::wait_until_paused_bounded(&server.controller, held, &format!("{row}: {held:?}"))
        .await;
    tokio::time::sleep(STAGED_SHUTDOWN + STAGED_SETTLE).await;
    server.handle.cancel();
    server.release(held, &row);

    let selected = LifecycleCheckpoint::InboundTerminalSelected(expect.terminal);
    http_support::wait_until_paused_bounded(
        &server.controller,
        selected,
        &format!("{row}: {selected:?}"),
    )
    .await;
    server.release(supervisor, &row);
    server.settle(peer, expect, &row).await;
}

/// The three rows whose ready sources are the route's own ceiling and the
/// deadlines the envelope carries.
///
/// Each holds the coordinator, makes what it wants weighed ready while it waits,
/// and reads back which of them the declared order named.
async fn assert_carried_deadline_rows() {
    // The crossing frame is already on the wire when the turn runs, and the
    // quiet interval expired while the turn was held.
    assert_held_row(
        RequestBudget::bounded(STAGED_IDLE, STAGED_TOTAL).expect("the staged request budget"),
        StagedWire::Crossing,
        PrecedenceExpectation {
            terminal: InboundTerminal::RouteBodyLimit,
            answers: 413,
            mapper_calls: 1,
            frames_polled: 1,
        },
    )
    .await;
    // Nothing is on the wire, so the two carried deadlines are the whole of the
    // turn's ready set and idle outranks total.
    assert_held_row(
        RequestBudget::bounded(STAGED_IDLE, STAGED_TOTAL).expect("the staged request budget"),
        StagedWire::Withheld,
        PrecedenceExpectation {
            terminal: InboundTerminal::BodyIdle,
            answers: 408,
            mapper_calls: 1,
            frames_polled: 0,
        },
    )
    .await;
    // The payload ended in the same turn the total expired, and no data frame
    // was polled to reach that end: the total outranks the response head it was
    // weighed against.
    assert_held_row(
        RequestBudget::unbounded()
            .with_total(STAGED_TOTAL)
            .expect("the staged request total"),
        StagedWire::Ended,
        PrecedenceExpectation {
            terminal: InboundTerminal::RequestTotal,
            answers: 408,
            mapper_calls: 1,
            frames_polled: 0,
        },
    )
    .await;
}

/// The two rows the wire decides on its own, and the two dispositions that
/// separate them: a payload's end refuses nothing, and framing that cannot be
/// parsed is mapped under the refusal the wire itself minted.
async fn assert_wire_answer_rows() {
    assert_live_row(
        RequestBudget::bounded(STAGED_UNREACHED, STAGED_UNREACHED)
            .expect("deadlines the completed row must not reach"),
        StagedWire::Complete,
        PrecedenceExpectation {
            terminal: InboundTerminal::ResponseHead,
            answers: 200,
            mapper_calls: 0,
            frames_polled: 1,
        },
    )
    .await;
    assert_live_row(
        RequestBudget::bounded(STAGED_UNREACHED, STAGED_UNREACHED)
            .expect("deadlines the unreadable row must not reach"),
        StagedWire::Broken,
        PrecedenceExpectation {
            terminal: InboundTerminal::SourceFailure,
            answers: 400,
            mapper_calls: 1,
            frames_polled: 0,
        },
    )
    .await;
}

/// 6.T5
///
/// Sources that become ready in one scheduling turn are resolved by the one
/// declared precedence table, not by poll order. Each row holds the production
/// coordinator at its pre-selection checkpoint, or lets the wire open the turn
/// itself, and then reads back the terminal production selected together with the
/// disposition that terminal declares: a mapped cause owes the peer one mapper
/// call, and a silent cause owes it none.
///
/// The rows are the pairs this step's own sources can reach. Step 11 extends the
/// same selector with its transfer sources once their adapters exist.
#[test]
fn equal_ready_inbound_events_follow_the_declared_precedence() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_carried_deadline_rows().await;
                assert_wire_answer_rows().await;
                assert_forced_cancellation_row().await;
                assert_shutdown_deadline_row().await;
            });
        })
        .expect("the precedence runtime ran");
}
