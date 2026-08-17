//! 12.T1–12.T3: one proxy route owns its upstream, and its phases stay apart.
//!
//! Every claim here is read off a real peer, a real proxy, and a real local
//! upstream. What separates the rows is which phase of the forward failed —
//! reaching the upstream, taking a head from it, waiting for its next body
//! frame, sending this request's own payload, or carrying its answer out — and
//! what an operator is told about each.

use crate::common;

use camber::__private::frozen_proxy_client_identities;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController};
use camber::http::{ProxyPolicy, RejectionKind, Router, TransferBudget};
use camber::runtime;
use std::io::Write;
use std::time::Duration;

/// The prefix every proxy route here is mounted under.
const PREFIX: &str = "/api";

/// The route pattern a request behind that prefix matches.
const PREFIX_ROUTE: &str = "/api/*proxy_path";

/// The path every row asks for behind the prefix.
const TARGET: &str = "/api/answer";

/// How long one row waits for an answer its proxy already settled.
const ROW_BOUND: Duration = Duration::from_secs(5);

/// The phase deadline every timing row configures.
///
/// Short enough that a row waits on it rather than on the suite, and far above
/// the scheduling a local upstream needs to answer inside it.
const PHASE_DEADLINE: Duration = Duration::from_millis(300);

/// The redacted body a bad gateway is answered with.
const BAD_GATEWAY_BODY: &str = "bad gateway";

/// The redacted body a gateway timeout is answered with.
const GATEWAY_TIMEOUT_BODY: &str = "gateway timeout";

/// The payload a healthy upstream answers with.
const UPSTREAM_BODY: &str = "upstream-answered";

/// Build the policy every row starts from: default, with `configure` applied.
fn policy(configure: impl FnOnce(ProxyPolicy) -> ProxyPolicy) -> ProxyPolicy {
    configure(ProxyPolicy::default())
}

/// What the peer was answered with, and what the operator was told.
struct Answered {
    status: u16,
    body: Box<[u8]>,
    kind: RejectionKind,
    cause: Box<str>,
}

/// Ask one served proxy route for its answer, recording the mapper and the event.
///
/// One entry point for every mapped row: the journal proves the refusal reached
/// this route's own mapper exactly once, and the captured event carries the
/// typed cause an operator filters on.
fn ask(served: std::net::SocketAddr, journal: &common::Journal, label: &str) -> Answered {
    let captured = common::capture_events(&format!("raw_path={TARGET}"));
    let response = common::request(served, "GET", TARGET, &[], b"", ROW_BOUND)
        .unwrap_or_else(|error| panic!("{label}: the proxy never answered: {error}"));
    let seen = common::only(journal, label);
    assert_eq!(
        seen.route.as_deref(),
        Some(PREFIX_ROUTE),
        "{label}: the refusal names the route that froze the phase",
    );
    let events = captured.events();
    let recorded = common::only_event(&events, common::REJECTION_MESSAGE, label);
    Answered {
        status: response.status,
        body: response.body,
        kind: seen.kind,
        cause: recorded.into(),
    }
}

/// Assert one phase failure carried the status, safe body, and typed cause it owes.
fn assert_phase(answered: &Answered, label: &str, status: u16, safe: &str, cause: &str) {
    assert_eq!(answered.status, status, "{label}: unexpected status");
    assert_eq!(
        answered.body.as_ref(),
        safe.as_bytes(),
        "{label}: the peer is told only what is safe",
    );
    assert_eq!(
        answered.kind,
        RejectionKind::Proxy,
        "{label}: an upstream phase keeps the proxy category",
    );
    assert!(
        answered.cause.contains(cause),
        "{label}: the operator's cause must name the phase: {}",
        answered.cause,
    );
}

/// Serve one buffered proxy route under `policy`, recording its refusals.
fn buffered_route(backend: &str, policy: ProxyPolicy) -> (std::net::SocketAddr, common::Journal) {
    let journal = common::journal();
    let mut router = Router::new().rejection_mapper(common::recording_mapper(&journal, "route"));
    router.proxy_with_policy(PREFIX, backend, policy);
    (common::spawn_server(router), journal)
}

/// Serve one streaming proxy route under `policy`, recording its refusals.
fn streaming_route(backend: &str, policy: ProxyPolicy) -> (std::net::SocketAddr, common::Journal) {
    let journal = common::journal();
    let mut router = Router::new().rejection_mapper(common::recording_mapper(&journal, "route"));
    router.proxy_stream_with_policy(PREFIX, backend, policy);
    (common::spawn_server(router), journal)
}

/// An address nothing in this process is listening on.
///
/// Bound and released, so the port was real and is now closed: a connect to it
/// is refused by the kernel rather than dropped, which is what makes the connect
/// phase a deterministic row instead of a wait on an unreachable network.
fn closed_backend() -> Box<str> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a closed port");
    let addr = listener.local_addr().expect("name the closed port");
    drop(listener);
    format!("http://{addr}").into()
}

/// The connect phase: no upstream transport was ever established.
fn connect_phase_row() {
    let label = "connect phase";
    let (served, journal) = buffered_route(&closed_backend(), ProxyPolicy::default());
    let answered = ask(served, &journal, label);
    assert_phase(
        &answered,
        label,
        502,
        BAD_GATEWAY_BODY,
        "cause=proxy connect failed",
    );
}

/// The request phase: the upstream took the request and committed no head.
fn request_phase_row() {
    let label = "request phase";
    let upstream = common::raw_upstream(200, UPSTREAM_BODY, common::UpstreamAnswers::Withheld);
    let (served, journal) = buffered_route(
        &upstream.backend(),
        policy(|policy| {
            policy
                .request_timeout(PHASE_DEADLINE)
                .expect("a finite proxy request deadline")
        }),
    );
    let answered = ask(served, &journal, label);
    assert_phase(
        &answered,
        label,
        504,
        GATEWAY_TIMEOUT_BODY,
        "cause=deadline exceeded: proxy_request",
    );
}

/// The head of a chunked answer whose frames arrive one at a time.
const CHUNKED_HEAD: &str = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

/// An answer that commits its head, sends one frame, and then goes quiet.
fn stalled_answer() -> Box<[u8]> {
    format!("{CHUNKED_HEAD}5\r\nfirst\r\n")
        .into_bytes()
        .into_boxed_slice()
}

/// The upstream-idle phase: the answer's next body frame never arrived.
fn upstream_idle_phase_row() {
    let label = "upstream idle phase";
    let upstream =
        common::scripted_upstream(stalled_answer(), common::UpstreamAnswers::OnHeadThenHold);
    let (served, journal) = buffered_route(
        &upstream.backend(),
        policy(|policy| {
            policy
                .upstream_idle_timeout(PHASE_DEADLINE)
                .expect("a finite upstream idle deadline")
        }),
    );
    let answered = ask(served, &journal, label);
    assert_phase(
        &answered,
        label,
        504,
        GATEWAY_TIMEOUT_BODY,
        "cause=deadline exceeded: proxy_upstream_idle",
    );
    assert!(
        !answered.body.windows(5).any(|window| window == b"first"),
        "{label}: no part of the stalled answer reaches the peer",
    );
}

/// Open one chunked upload against a streaming route and send one frame.
fn open_upload(served: std::net::SocketAddr) -> std::net::TcpStream {
    let mut peer = common::connect(served).expect("the uploading peer connected");
    common::write_chunked_head(
        &mut peer,
        common::KEEP_CONNECTION,
        "POST",
        TARGET,
        common::DEFAULT_HOST,
    )
    .expect("the upload head reached the proxy");
    common::tolerate_dead_socket(common::write_chunk(&mut peer, b"first-frame"))
        .expect("the first frame reached the proxy");
    peer
}

/// The upload phase: this request's own payload went quiet inside its bound.
fn upload_phase_row() {
    let label = "upload phase";
    let upstream = common::raw_upstream(200, UPSTREAM_BODY, common::UpstreamAnswers::Withheld);
    let journal = common::journal();
    let mut router = Router::new().rejection_mapper(common::recording_mapper(&journal, "route"));
    router.proxy_stream_with_policy(
        PREFIX,
        &upstream.backend(),
        policy(|policy| {
            policy.upload_budget(
                TransferBudget::unbounded()
                    .with_idle(PHASE_DEADLINE)
                    .expect("a finite upload quiet interval"),
            )
        }),
    );
    let served = common::spawn_server(router);

    let mut peer = open_upload(served);
    let answered =
        common::read_http_response_bounded(&mut peer).expect("the quiet upload was answered");
    assert_eq!(
        answered.status, 408,
        "{label}: a quiet upload is answered under its own transfer deadline",
    );
    let seen = common::only(&journal, label);
    assert_eq!(
        seen.kind,
        RejectionKind::BodyTimeout,
        "{label}: the upload direction keeps its own category",
    );
    common::assert_connection_closed(&mut peer, label);
}

/// The download phase: the committed answer crossed this route's own maximum.
///
/// Post-commit by construction. The head is already on the wire, so the row
/// reads what the transport did instead of a status this stage may not rewrite:
/// the peer keeps the `200` it was given, the payload stops at the maximum, and
/// no mapper was reached at all.
fn download_phase_row() {
    let label = "download phase";
    let admitted = 8;
    let upstream =
        common::raw_upstream(200, "0123456789abcdefghij", common::UpstreamAnswers::OnHead);
    let (served, journal) = streaming_route(
        &upstream.backend(),
        policy(|policy| {
            policy.download_budget(
                TransferBudget::unbounded()
                    .with_max_bytes(admitted)
                    .expect("a finite download maximum"),
            )
        }),
    );

    let mut peer = common::connect(served).expect("the downloading peer connected");
    peer.write_all(
        format!(
            "GET {TARGET} HTTP/1.1\r\nHost: {}\r\n\r\n",
            common::DEFAULT_HOST
        )
        .as_bytes(),
    )
    .expect("the download request reached the proxy");
    // Read to closure rather than as a complete message: the head committed and
    // the transport then ended under a body that cannot be finished, which is
    // the disposition this row is about.
    let raw = common::read_until_closed(&mut peer, ROW_BOUND)
        .expect("the committed answer reached the peer");
    let (head, body) = split_message(&raw, label);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "{label}: a committed status is never rewritten: {head}",
    );
    assert!(
        body.len() <= admitted,
        "{label}: no payload past the frozen maximum reaches the peer: {body:?}",
    );
    assert!(
        common::drain(&journal).is_empty(),
        "{label}: a post-commit terminal reaches no mapper",
    );
}

/// Split one raw answer into its head text and the payload behind it.
fn split_message<'a>(raw: &'a [u8], label: &str) -> (Box<str>, &'a [u8]) {
    let end = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .unwrap_or_else(|| panic!("{label}: the peer never received a whole head"));
    (
        String::from_utf8_lossy(&raw[..end]).into_owned().into(),
        &raw[end + 4..],
    )
}

/// 12.T1
#[test]
fn proxy_phase_failures_keep_distinct_typed_provenance() {
    common::test_runtime()
        .with_tracing()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            connect_phase_row();
            request_phase_row();
            upstream_idle_phase_row();
            upload_phase_row();
            download_phase_row();
            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// The prefix of the route whose own request deadline is short.
const QUICK_PREFIX: &str = "/quick";

/// The prefix of a second route under exactly the same policy.
const TWIN_PREFIX: &str = "/twin";

/// The prefix of the route whose own request deadline is generous.
const PATIENT_PREFIX: &str = "/patient";

/// The deadline the patient route freezes: long enough that inheriting it would
/// keep the quick route waiting past this row's own bound.
const PATIENT_DEADLINE: Duration = Duration::from_secs(30);

/// One router carrying two equal policies and one different one.
fn interleaved_router(stalling: &str, answering: &str) -> Router {
    let mut router = Router::new();
    router.proxy_with_policy(QUICK_PREFIX, stalling, quick_policy());
    router.proxy_with_policy(TWIN_PREFIX, stalling, quick_policy());
    router.proxy_with_policy(PATIENT_PREFIX, answering, patient_policy());
    router
}

/// The policy the quick routes freeze.
fn quick_policy() -> ProxyPolicy {
    policy(|policy| {
        policy
            .request_timeout(PHASE_DEADLINE)
            .expect("a finite proxy request deadline")
    })
}

/// The policy the patient route freezes.
fn patient_policy() -> ProxyPolicy {
    policy(|policy| {
        policy
            .request_timeout(PATIENT_DEADLINE)
            .expect("a finite proxy request deadline")
    })
}

/// Ask one route for its answer under this row's own bound.
fn ask_route(served: std::net::SocketAddr, prefix: &str, label: &str) -> common::HttpResponse {
    common::request(
        served,
        "GET",
        &format!("{prefix}/answer"),
        &[],
        b"",
        ROW_BOUND,
    )
    .unwrap_or_else(|error| panic!("{label}: the proxy never answered: {error}"))
}

/// Assert the frozen client owners a router handed its proxy routes.
fn assert_frozen_owners(router: &Router, label: &str) -> Box<[usize]> {
    let owners = frozen_proxy_client_identities(router);
    assert_eq!(owners.len(), 3, "{label}: one owner per registered route");
    assert_eq!(
        owners[0], owners[1],
        "{label}: two routes under one equal policy share the owner their graph froze",
    );
    assert_ne!(
        owners[0], owners[2],
        "{label}: a route under a different policy never shares that owner",
    );
    owners
}

/// 12.T2
#[test]
fn proxy_policies_freeze_distinct_clients_and_never_share_timeout_state() {
    common::test_runtime()
        .shutdown_timeout(Duration::from_secs(5))
        .run(|| {
            let stalling =
                common::raw_upstream(200, UPSTREAM_BODY, common::UpstreamAnswers::Withheld);
            let answering =
                common::raw_upstream(200, UPSTREAM_BODY, common::UpstreamAnswers::OnHead);

            let first = interleaved_router(&stalling.backend(), &answering.backend());
            let first_owners = assert_frozen_owners(&first, "first graph");
            let second = interleaved_router(&stalling.backend(), &answering.backend());
            let second_owners = assert_frozen_owners(&second, "second graph");
            assert!(
                first_owners
                    .iter()
                    .all(|owner| !second_owners.contains(owner)),
                "an equal policy in a second graph freezes its own owner, not the first's",
            );

            let served = common::spawn_server(first);
            let beside = common::spawn_server(second);
            for round in 0..2 {
                let label = format!("round {round}");
                assert_eq!(
                    ask_route(served, QUICK_PREFIX, &label).status,
                    504,
                    "{label}: the quick route ends on the deadline it froze",
                );
                assert_eq!(
                    ask_route(served, PATIENT_PREFIX, &label).status,
                    200,
                    "{label}: the patient route keeps its own generous deadline",
                );
                assert_eq!(
                    ask_route(beside, TWIN_PREFIX, &label).status,
                    504,
                    "{label}: a second graph enforces its own copy of the same policy",
                );
            }
            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

/// What one staged row does to make its local source ready.
enum LocalSource {
    /// Wait out the request total the route froze.
    RequestTotal,
    /// End the peer's connection while the head is held.
    Disconnect,
    /// Cancel the server that admitted this request while the head is held.
    Cancel,
}

/// What one staged row expects the peer to have been given.
enum Expected {
    /// One mapped refusal, at this status.
    Mapped(u16),
    /// Nothing at all: this peer is gone, so no answer can reach it.
    Silent,
    /// A local cause, whichever of them this transition selected.
    ///
    /// A shutting-down server ends an admitted operation through whichever of
    /// its own sources becomes ready first — the deadline it mints, or the
    /// transport under an upload it can no longer read. The row does not pick
    /// between them; what it holds is that the answer is never the upstream head
    /// this service was still holding uncommitted.
    Local,
}

/// One equal-ready row: the local source it stages and the answer it owes.
struct Row {
    label: &'static str,
    source: LocalSource,
    expected: Expected,
}

/// The request total every staged row freezes.
///
/// Long enough to survive the staging below — the head has to be held before it
/// expires — and short enough that the row waits on it rather than on the suite.
const STAGED_TOTAL: Duration = Duration::from_millis(700);

/// The router every staged row serves.
fn staged_router(backend: &str, journal: &common::Journal) -> Router {
    let mut router = Router::new().rejection_mapper(common::recording_mapper(journal, "route"));
    router.proxy_stream_with_policy(PREFIX, backend, ProxyPolicy::default());
    router.request_budget(
        camber::http::RequestBudget::unbounded()
            .with_total(STAGED_TOTAL)
            .expect("a finite request total"),
    )
}

/// Serve one streaming proxy route whose upstream head can be held.
///
/// The observer is handed back beside the server because one row ends its
/// server itself: the counters this row reads are the listener's, and they
/// outlive the fixture that served it.
fn staged_server(
    backend: &str,
    journal: &common::Journal,
) -> (common::ObservedServer, std::sync::Arc<LifecycleController>) {
    let port = common::reserve_observed();
    let controller = port.controller();
    (port.serve(staged_router(backend, journal)), controller)
}

/// Hold this row's upstream head where it became ready and uncommitted.
fn hold_upstream_head(controller: &LifecycleController) {
    controller
        .pause_once(LifecycleCheckpoint::StreamingUpstreamHeadReady)
        .expect("arm the upstream-head checkpoint");
}

/// Wait until the held head is in hand and still uncommitted.
fn await_held_head(controller: &LifecycleController) {
    common::block_on(async {
        common::wait_until_paused_bounded(
            controller,
            LifecycleCheckpoint::StreamingUpstreamHeadReady,
            "the upstream answered and its head is held uncommitted",
        )
        .await;
    });
}

/// Run one staged row end to end.
fn run_staged_row(row: &Row) {
    let upstream = common::raw_upstream(200, UPSTREAM_BODY, common::UpstreamAnswers::OnHead);
    let journal = common::journal();
    let (server, controller) = staged_server(&upstream.backend(), &journal);
    hold_upstream_head(&controller);

    let peer = open_upload(server.addr());
    await_held_head(&controller);
    let peer = stage_local_source(row, peer);
    let cancelled = cancel_when_staged(row, server);

    assert_row_answer(row, peer);
    assert_quiesced_upload(row, &controller);
    assert!(
        common::drain(&journal).len() <= 1,
        "{}: a local cause maps at most once",
        row.label,
    );
    assert_eq!(
        upstream.answered(),
        1,
        "{}: the upstream committed the head this row held uncommitted",
        row.label,
    );
    drop(cancelled);
}

/// Stop the server this row staged, when stopping it is the local source.
///
/// The cancelling row owns its server's ending rather than leaving it to the
/// guard: the claim is what an admitted operation does when its own server is
/// cancelled under it, so the cancellation has to happen while the head is held
/// and be joined before this row reads what the peer got. Every other row hands
/// the guard back so its bounded teardown runs as usual.
fn cancel_when_staged(row: &Row, server: common::ObservedServer) -> Option<common::ObservedServer> {
    match row.source {
        LocalSource::Cancel => {
            server
                .shutdown_bounded(ROW_BOUND)
                .expect("the cancelled server joined inside this row's bound");
            None
        }
        LocalSource::RequestTotal | LocalSource::Disconnect => Some(server),
    }
}

/// Assert the upload stopped where the local cause was selected.
///
/// Read off the production upload owner itself: it released its source exactly
/// once, and it polled no frame past the one the peer sent before that cause was
/// selected. A later poll here would be an upload still reading payload for a
/// request this service has already ended.
fn assert_quiesced_upload(row: &Row, controller: &LifecycleController) {
    let released = common::poll_until(ROW_BOUND, || {
        controller.transfers_observed().upload.releases >= 1
    });
    let observed = controller.transfers_observed().upload;
    assert!(
        released,
        "{}: the upload owner released its source: {observed:?}",
        row.label,
    );
    assert_eq!(
        observed.releases, 1,
        "{}: one upload owner, released once",
        row.label,
    );
    assert_eq!(
        observed.frames_polled, 1,
        "{}: no frame is polled past the one the selected cause was weighed against",
        row.label,
    );
}

/// Make this row's local source ready while the upstream head stays held.
fn stage_local_source(row: &Row, peer: std::net::TcpStream) -> Option<std::net::TcpStream> {
    match row.source {
        LocalSource::RequestTotal => {
            std::thread::sleep(STAGED_TOTAL + PHASE_DEADLINE);
            Some(peer)
        }
        LocalSource::Disconnect => {
            peer.shutdown(std::net::Shutdown::Both)
                .expect("the peer ended its own connection");
            drop(peer);
            None
        }
        LocalSource::Cancel => Some(peer),
    }
}

/// Assert what one staged row's peer was given, and that nothing replaced it.
fn assert_row_answer(row: &Row, peer: Option<std::net::TcpStream>) {
    let Some(mut peer) = peer else {
        return;
    };
    match row.expected {
        Expected::Mapped(status) => {
            let answered = common::read_http_response_bounded(&mut peer).unwrap_or_else(|error| {
                panic!("{}: the staged row went unanswered: {error}", row.label)
            });
            assert_eq!(
                answered.status, status,
                "{}: the local cause is the one answered",
                row.label,
            );
            common::assert_connection_closed(&mut peer, row.label);
        }
        Expected::Silent => {
            common::assert_connection_closed(&mut peer, row.label);
        }
        Expected::Local => {
            let answered = common::read_http_response_bounded(&mut peer).ok();
            assert_local_cause(row, answered.as_ref());
            common::assert_connection_closed(&mut peer, row.label);
        }
    }
}

/// Assert one staged answer came from a local cause and not from the upstream.
fn assert_local_cause(row: &Row, answered: Option<&common::HttpResponse>) {
    let Some(answered) = answered else {
        return;
    };
    assert_ne!(
        answered.status, 200,
        "{}: the uncommitted upstream head is never the answer",
        row.label,
    );
    assert!(
        !answered
            .body
            .windows(UPSTREAM_BODY.len())
            .any(|window| window == UPSTREAM_BODY.as_bytes()),
        "{}: no part of the upstream's answer reaches the peer",
        row.label,
    );
}

/// 12.T3
#[test]
fn streaming_proxy_equal_ready_local_limit_precedes_uncommitted_upstream_head() {
    for row in [
        Row {
            label: "request total",
            source: LocalSource::RequestTotal,
            expected: Expected::Mapped(408),
        },
        Row {
            label: "peer disconnect",
            source: LocalSource::Disconnect,
            expected: Expected::Silent,
        },
        Row {
            label: "cancelled server",
            source: LocalSource::Cancel,
            expected: Expected::Local,
        },
    ] {
        common::test_runtime()
            .shutdown_timeout(Duration::from_secs(5))
            .run(|| {
                run_staged_row(&row);
                runtime::request_shutdown();
            })
            .expect("the fixture runtime ran to completion");
    }
}
