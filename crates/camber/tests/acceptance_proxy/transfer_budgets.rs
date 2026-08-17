//! 12.T1–12.T3: one proxy route owns its upstream, and its phases stay apart.
//!
//! Every claim here is read off a real peer, a real proxy, and a real local
//! upstream. What separates the rows is which phase of the forward failed —
//! reaching the upstream, taking a head from it, waiting for its next body
//! frame, sending this request's own payload, or carrying its answer out — and
//! what an operator is told about each.

use crate::common;

use camber::__private::frozen_proxy_client_identities;
use camber::http::mock::{InboundTerminal, LifecycleCheckpoint, LifecycleController};
use camber::http::{ProxyPolicy, RejectionKind, Router, TransferBudget};
use camber::runtime;
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
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

/// Open one chunked upload against a streaming route and send one frame.
fn open_upload(served: SocketAddr) -> TcpStream {
    let mut peer = common::connect(served).expect("the uploading peer connected");
    common::write_chunked_head(
        &mut peer,
        common::KEEP_CONNECTION,
        "POST",
        TARGET,
        common::DEFAULT_HOST,
    )
    .expect("the upload head reached the proxy");
    common::tolerate_dead_socket(common::write_chunk(&mut peer, FIRST_FRAME))
        .expect("the first frame reached the proxy");
    peer
}

/// The one payload frame every upload row sends before it stops.
const FIRST_FRAME: &[u8] = b"first-frame";

/// Assert no part of the upstream's own answer reached this peer.
fn assert_no_upstream_payload(body: &[u8], label: &str) {
    assert!(
        !body
            .windows(UPSTREAM_BODY.len())
            .any(|window| window == UPSTREAM_BODY.as_bytes()),
        "{label}: no part of the upstream's answer reaches the peer",
    );
}

/// Wait until `controller` is holding production at `checkpoint`.
fn wait_paused(controller: &LifecycleController, checkpoint: LifecycleCheckpoint, label: &str) {
    common::block_on(async {
        common::wait_until_paused_bounded(controller, checkpoint, label).await;
    });
}

/// What one refusal left the peer's connection able to do next.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Disposition {
    /// Framable: this request left nothing of its own unread.
    Reusable,
    /// Ended: unread payload, or a committed body that cannot be finished.
    Closed,
}

/// Assert one answer left the transport in the condition it owes.
fn assert_disposition(peer: &mut TcpStream, disposition: Disposition, label: &str) {
    match disposition {
        Disposition::Closed => common::assert_connection_closed(peer, label),
        Disposition::Reusable => {
            let answered = common::probe_connection_reuse(peer, "GET", REUSE_TARGET, &[], b"")
                .unwrap_or_else(|error| panic!("{label}: the reuse probe did not settle: {error}"));
            assert!(
                answered.is_some(),
                "{label}: a refusal that left nothing unread keeps the connection framable",
            );
        }
    }
}

/// The path a disposition probe asks for: outside the proxy prefix, so the
/// router answers it without reaching any upstream.
const REUSE_TARGET: &str = "/reuse-probe";

/// What the peer was answered with, what the operator was told, and the
/// connection it all arrived on.
struct Answered {
    status: u16,
    body: Box<[u8]>,
    kind: RejectionKind,
    cause: Box<str>,
    peer: TcpStream,
}

/// Ask one served proxy route for its answer, recording the mapper and the event.
///
/// One entry point for every mapped row: the journal proves the refusal reached
/// this route's own mapper exactly once, and the captured event carries the
/// typed cause an operator filters on. The connection is handed back rather than
/// dropped, because what the refusal left the transport able to do is a claim of
/// its own.
fn ask(served: SocketAddr, journal: &common::Journal, label: &str) -> Answered {
    let captured = common::capture_events(&format!("raw_path={TARGET}"));
    let mut peer = common::connect(served)
        .unwrap_or_else(|error| panic!("{label}: the peer never connected: {error}"));
    common::write_request_with_connection(
        &mut peer,
        common::KEEP_CONNECTION,
        "GET",
        TARGET,
        &[],
        b"",
    )
    .unwrap_or_else(|error| panic!("{label}: the request never reached the proxy: {error}"));
    let response = common::read_http_response_bounded(&mut peer)
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
        peer,
    }
}

/// Everything one phase failure owes the peer and the operator.
struct PhaseClaim {
    status: u16,
    safe: &'static str,
    cause: &'static str,
    disposition: Disposition,
}

/// Assert one phase failure carried the status, safe body, typed cause, and
/// transport disposition it owes.
fn assert_phase(answered: &mut Answered, label: &str, claim: &PhaseClaim) {
    assert_eq!(answered.status, claim.status, "{label}: unexpected status");
    assert_eq!(
        answered.body.as_ref(),
        claim.safe.as_bytes(),
        "{label}: the peer is told only what is safe",
    );
    assert_eq!(
        answered.kind,
        RejectionKind::Proxy,
        "{label}: an upstream phase keeps the proxy category",
    );
    assert!(
        answered.cause.contains(claim.cause),
        "{label}: the operator's cause must name the phase: {}",
        answered.cause,
    );
    assert_disposition(&mut answered.peer, claim.disposition, label);
}

/// Serve one buffered proxy route under `policy`, recording its refusals.
fn buffered_route(backend: &str, policy: ProxyPolicy) -> (SocketAddr, common::Journal) {
    let journal = common::journal();
    let mut router = Router::new().rejection_mapper(common::recording_mapper(&journal, "route"));
    router.proxy_with_policy(PREFIX, backend, policy);
    (common::spawn_server(router), journal)
}

/// Serve one streaming proxy route under `policy`, with its transfer owners
/// observable and its refusals recorded.
fn observed_streaming_route(
    backend: &str,
    policy: ProxyPolicy,
    journal: &common::Journal,
) -> (common::ObservedServer, Arc<LifecycleController>) {
    let port = common::reserve_observed();
    let controller = port.controller();
    let mut router = Router::new().rejection_mapper(common::recording_mapper(journal, "route"));
    router.proxy_stream_with_policy(PREFIX, backend, policy);
    (port.serve(router), controller)
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
    let mut answered = ask(served, &journal, label);
    assert_phase(
        &mut answered,
        label,
        &PhaseClaim {
            status: 502,
            safe: BAD_GATEWAY_BODY,
            cause: "cause=proxy connect failed",
            disposition: Disposition::Reusable,
        },
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
    let mut answered = ask(served, &journal, label);
    assert_phase(
        &mut answered,
        label,
        &PhaseClaim {
            status: 504,
            safe: GATEWAY_TIMEOUT_BODY,
            cause: "cause=deadline exceeded: proxy_request",
            disposition: Disposition::Reusable,
        },
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
    let mut answered = ask(served, &journal, label);
    assert_phase(
        &mut answered,
        label,
        &PhaseClaim {
            status: 504,
            safe: GATEWAY_TIMEOUT_BODY,
            cause: "cause=deadline exceeded: proxy_upstream_idle",
            disposition: Disposition::Reusable,
        },
    );
    assert!(
        !answered.body.windows(5).any(|window| window == b"first"),
        "{label}: no part of the stalled answer reaches the peer",
    );
}

/// An answer whose head promises payload the upstream then never sends.
fn truncated_answer() -> Box<[u8]> {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\nshort",
        UPSTREAM_BODY.len(),
    )
    .into_bytes()
    .into_boxed_slice()
}

/// The download phase, buffered: the answer's body could not be carried.
///
/// Distinct from the idle row on the same leg. An upstream that goes quiet is
/// waiting; this one framed a length and then ended the transport under it, so
/// what the collection reports is a fault and not a deadline — and the phase is
/// the whole of the provenance, because the cause names no bound of its own.
fn buffered_download_phase_row() {
    let label = "buffered download phase";
    let upstream = common::scripted_upstream(truncated_answer(), common::UpstreamAnswers::OnHead);
    let (served, journal) = buffered_route(&upstream.backend(), ProxyPolicy::default());
    let mut answered = ask(served, &journal, label);
    assert_phase(
        &mut answered,
        label,
        &PhaseClaim {
            status: 502,
            safe: BAD_GATEWAY_BODY,
            cause: "cause=proxy download failed",
            disposition: Disposition::Reusable,
        },
    );
}

/// Wait until one direction's transfer owner has fixed its terminal, and say
/// which one it fixed.
fn awaited_terminal(
    controller: &LifecycleController,
    direction: impl Fn(&LifecycleController) -> Option<InboundTerminal>,
    label: &str,
) -> InboundTerminal {
    let settled = common::poll_until(ROW_BOUND, || direction(controller).is_some());
    assert!(settled, "{label}: the transfer owner fixed no terminal");
    direction(controller).unwrap_or_else(|| panic!("{label}: the fixed terminal was taken back"))
}

/// The upload phase: this request's own payload went quiet inside its bound.
///
/// The typed provenance is the upload owner's, not the upstream leg's: this
/// direction is enforced by the Step 11 transfer adapter the route's frozen
/// policy narrowed, so the terminal, the boundary the operator reads, and the
/// category the peer is answered under all come from that owner.
fn upload_phase_row() {
    let label = "upload phase";
    let upstream = common::raw_upstream(200, UPSTREAM_BODY, common::UpstreamAnswers::Withheld);
    let journal = common::journal();
    let (server, controller) = observed_streaming_route(
        &upstream.backend(),
        policy(|policy| {
            policy.upload_budget(
                TransferBudget::unbounded()
                    .with_idle(PHASE_DEADLINE)
                    .expect("a finite upload quiet interval"),
            )
        }),
        &journal,
    );

    let captured = common::capture_events(&format!("raw_path={TARGET}"));
    let mut peer = open_upload(server.addr());
    let answered =
        common::read_http_response_bounded(&mut peer).expect("the quiet upload was answered");
    assert_upload_phase(&answered, &journal, &controller, label);
    let events = captured.events();
    let recorded = common::only_event(&events, common::REJECTION_MESSAGE, label);
    assert!(
        recorded.contains("cause=deadline exceeded: transfer_idle"),
        "{label}: the operator's cause names the boundary the upload owner measured: {recorded}",
    );
    assert_disposition(&mut peer, Disposition::Closed, label);
}

/// Assert the quiet upload's status, category, and fixed terminal.
fn assert_upload_phase(
    answered: &common::HttpResponse,
    journal: &common::Journal,
    controller: &LifecycleController,
    label: &str,
) {
    assert_eq!(
        answered.status, 408,
        "{label}: a quiet upload is answered under its own transfer deadline",
    );
    assert_no_upstream_payload(&answered.body, label);
    let seen = common::only(journal, label);
    assert_eq!(
        seen.kind,
        RejectionKind::BodyTimeout,
        "{label}: the upload direction keeps its own category",
    );
    assert_eq!(
        awaited_terminal(
            controller,
            |controller| controller.transfers_observed().upload.terminal,
            label,
        ),
        InboundTerminal::TransferIdle,
        "{label}: the upload owner fixed the quiet interval it was narrowed to",
    );
}

/// The download phase: the committed answer crossed this route's own maximum.
///
/// Post-commit by construction. The head is already on the wire, so the row
/// reads what the transport did instead of a status this stage may not rewrite:
/// the peer keeps the `200` it was given, the payload stops at the maximum, the
/// download owner names the byte terminal it fixed, and no mapper was reached.
fn download_phase_row() {
    let admitted = 8;
    let upstream =
        common::raw_upstream(200, "0123456789abcdefghij", common::UpstreamAnswers::OnHead);
    let journal = common::journal();
    let (server, controller) = observed_streaming_route(
        &upstream.backend(),
        policy(|policy| {
            policy.download_budget(
                TransferBudget::unbounded()
                    .with_max_bytes(admitted)
                    .expect("a finite download maximum"),
            )
        }),
        &journal,
    );

    let mut peer = common::connect(server.addr()).expect("the downloading peer connected");
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
    assert_download_phase(&raw, &journal, &controller, admitted);
}

/// Assert the truncated download's committed status, ceiling, terminal, and
/// that no mapper was reached behind the head already on the wire.
fn assert_download_phase(
    raw: &[u8],
    journal: &common::Journal,
    controller: &LifecycleController,
    admitted: usize,
) {
    let label = "download phase";
    let (head, body) = split_message(raw, label);
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "{label}: a committed status is never rewritten: {head}",
    );
    assert!(
        body.len() <= admitted,
        "{label}: no payload past the frozen maximum reaches the peer: {body:?}",
    );
    assert_eq!(
        awaited_terminal(
            controller,
            |controller| controller.transfers_observed().download.terminal,
            label,
        ),
        InboundTerminal::TransferBytes,
        "{label}: the download owner fixed the maximum this route froze",
    );
    assert!(
        common::drain(journal).is_empty(),
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
            buffered_download_phase_row();
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
fn ask_route(served: SocketAddr, prefix: &str, label: &str) -> common::HttpResponse {
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

/// The request total a row that stages it freezes.
///
/// Long enough to survive the staging below — the head has to be held before it
/// expires — and short enough that the row waits on it rather than on the suite.
const STAGED_TOTAL: Duration = Duration::from_millis(700);

/// The request total every other row freezes: past this suite's own bounds, so
/// no row can end on a deadline it did not stage.
const UNREACHED_TOTAL: Duration = Duration::from_secs(120);

/// The aggregate deadline a staged server mints when it is stopped.
const STAGED_SHUTDOWN: Duration = Duration::from_millis(200);

/// The upload interval and lifetime the transfer rows freeze.
const STAGED_QUIET: Duration = Duration::from_millis(200);

/// The upload ceiling the byte row's policy narrows its route's admission to.
const STAGED_CEILING: usize = 16;

/// The frame that carries the byte row past that ceiling.
const CROSSING_FRAME: &[u8] = b"crossing-frame-past-the-frozen-ceiling";

/// One staged row's server, the observer registered for it, and the refusals
/// its route recorded.
///
/// The raw handle is held rather than a guarded fixture, because two rows stop
/// the server themselves: the claim is what an admitted operation does when its
/// own server transitions under it, so the transition has to happen while the
/// upstream head is held and be read before anything is joined.
struct Staged {
    addr: SocketAddr,
    handle: camber::http::ServerHandle,
    controller: Arc<LifecycleController>,
    journal: common::Journal,
}

impl Staged {
    /// Serve one streaming proxy route whose upstream head can be held.
    fn serve(backend: &str, policy: ProxyPolicy, total: Duration) -> Self {
        let journal = common::journal();
        let mut router =
            Router::new().rejection_mapper(common::recording_mapper(&journal, "route"));
        router.proxy_stream_with_policy(PREFIX, backend, policy);
        let router = router.request_budget(
            camber::http::RequestBudget::unbounded()
                .with_total(total)
                .expect("a finite request total"),
        );
        let (listener, addr, controller) = common::reserve_observed().into_owned_parts();
        let handle = camber::http::server(router)
            .policy(
                camber::http::ServerPolicy::default()
                    .shutdown_timeout(STAGED_SHUTDOWN)
                    .expect("a finite aggregate deadline"),
            )
            .serve_background(listener)
            .expect("the owned server requires a Tokio runtime");
        Self {
            addr,
            handle,
            controller,
            journal,
        }
    }

    /// Arm one checkpoint this row stages against.
    fn arm(&self, checkpoint: LifecycleCheckpoint, label: &str) {
        self.controller
            .pause_once(checkpoint)
            .unwrap_or_else(|error| panic!("{label}: arming {checkpoint:?} failed: {error}"));
    }

    /// Let go of one checkpoint this row was holding.
    fn release(&self, checkpoint: LifecycleCheckpoint, label: &str) {
        self.controller
            .release(checkpoint)
            .unwrap_or_else(|error| panic!("{label}: releasing {checkpoint:?} failed: {error}"));
    }

    /// Open one upload and wait until its upstream head is held uncommitted.
    fn upload_with_held_head(&self, label: &str) -> TcpStream {
        self.arm(LifecycleCheckpoint::StreamingUpstreamHeadReady, label);
        let peer = open_upload(self.addr);
        wait_paused(
            &self.controller,
            LifecycleCheckpoint::StreamingUpstreamHeadReady,
            label,
        );
        peer
    }

    /// Assert this row's upload stopped where its local cause was selected.
    ///
    /// Read off the production upload owner itself: it released its source
    /// exactly once, and it polled no frame past the ones this row sent before
    /// that cause was selected. A later poll here would be an upload still
    /// reading payload for a request this service has already ended.
    fn assert_quiesced_upload(&self, frames: usize, label: &str) {
        let released = common::poll_until(ROW_BOUND, || {
            self.controller.transfers_observed().upload.releases >= 1
        });
        let observed = self.controller.transfers_observed().upload;
        assert!(
            released,
            "{label}: the upload owner released its source: {observed:?}",
        );
        assert_eq!(
            observed.releases, 1,
            "{label}: one upload owner, released once"
        );
        assert_eq!(
            observed.frames_polled, frames,
            "{label}: no frame is polled past the one the selected cause was weighed against",
        );
    }

    /// Assert how many times this row's local cause reached the route's mapper.
    fn assert_mapped(&self, calls: usize, label: &str) {
        assert_eq!(
            common::drain(&self.journal).len(),
            calls,
            "{label}: this cause's declared disposition allows {calls} mapper call(s)",
        );
    }

    /// Stop and join this row's server under its own bound.
    fn finish(self, label: &str) {
        common::ReadyServer::adopt(self.addr, self.handle)
            .shutdown_bounded(ROW_BOUND)
            .unwrap_or_else(|error| panic!("{label}: teardown failed: {error}"));
    }
}

/// What one row does to make an operation-carried source ready.
enum OperationSource {
    /// Wait out the request total the route froze.
    RequestTotal,
    /// End the peer's sending half while the head is held.
    Disconnect,
    /// Stop the server and let the aggregate deadline it mints expire.
    Shutdown,
    /// Cancel the server that admitted this request.
    Cancel,
}

/// What one staged row's peer is given.
#[derive(Clone, Copy)]
enum Answer {
    /// The route's own mapper answered, at this status and with its own body.
    Mapped(u16),
    /// The declared precedence gives this cause no mapper: a bodyless status
    /// and an ended transport are the whole answer.
    Unmapped(u16),
}

/// One row whose local cause the operation coordinator selects against the held
/// upstream head.
struct OperationRow {
    label: &'static str,
    source: OperationSource,
    terminal: InboundTerminal,
    answer: Answer,
}

/// The supervisor checkpoint the two control rows hold.
///
/// The abort a stop escalates to would take this connection's task away where
/// it stands, and this row is about what the coordinator decided. Held at the
/// production transition the supervisor selects it from, the transition is
/// published and the escalation behind it is not.
const SUPERVISOR: LifecycleCheckpoint = LifecycleCheckpoint::SupervisorSelectedControl;

/// Run one operation-source row end to end.
fn run_operation_row(row: &OperationRow) {
    let label = row.label;
    let upstream =
        common::raw_upstream(200, UPSTREAM_BODY, common::UpstreamAnswers::OnHeadThenHold);
    let staged = Staged::serve(
        &upstream.backend(),
        ProxyPolicy::default(),
        operation_total(&row.source),
    );
    let selected = LifecycleCheckpoint::InboundTerminalSelected(row.terminal);
    staged.arm(selected, label);
    let mut peer = staged.upload_with_held_head(label);

    stage_operation_source(&staged, &row.source, &mut peer, label);
    wait_paused(&staged.controller, selected, label);
    staged.assert_quiesced_upload(1, label);
    staged.release(selected, label);

    assert_staged_answer(&mut peer, row.answer, label);
    staged.assert_mapped(mapper_calls(row.answer), label);
    assert_upstream_leg(&upstream, label);
    finish_operation_row(staged, &row.source, label);
}

/// Assert what the upstream saw of the request its held head belonged to.
///
/// One leg, one committed head, and nothing behind it. The row deliberately
/// asserts no upstream-side close: Camber releases the abandoned leg back to
/// the owner its route froze, and Reqwest keeps that connection pooled for the
/// next forward through the same owner. A probe polling the upstream for a
/// closed transport therefore reads zero however promptly the release happened
/// — it is measuring the pool's retention, not this service's. What the release
/// is read off instead is the production upload owner, above.
fn assert_upstream_leg(upstream: &common::RawUpstream, label: &str) {
    assert_eq!(
        upstream.answered(),
        1,
        "{label}: the upstream committed the head this row held uncommitted",
    );
    assert_eq!(
        upstream.connections(),
        1,
        "{label}: one ended request never leaves a second leg open behind it",
    );
}

/// The request total one operation row's router freezes.
fn operation_total(source: &OperationSource) -> Duration {
    match source {
        OperationSource::RequestTotal => STAGED_TOTAL,
        OperationSource::Disconnect | OperationSource::Shutdown | OperationSource::Cancel => {
            UNREACHED_TOTAL
        }
    }
}

/// Make this row's operation-carried source ready while the head stays held.
fn stage_operation_source(
    staged: &Staged,
    source: &OperationSource,
    peer: &mut TcpStream,
    label: &str,
) {
    match source {
        OperationSource::RequestTotal => std::thread::sleep(STAGED_TOTAL + PHASE_DEADLINE),
        OperationSource::Disconnect => peer
            .shutdown(std::net::Shutdown::Write)
            .expect("the peer ended the half it was sending on"),
        OperationSource::Shutdown => {
            staged.arm(SUPERVISOR, label);
            staged.handle.shutdown();
            wait_paused(&staged.controller, SUPERVISOR, label);
        }
        OperationSource::Cancel => {
            staged.arm(SUPERVISOR, label);
            staged.handle.cancel();
            wait_paused(&staged.controller, SUPERVISOR, label);
        }
    }
}

/// Let go of whatever this row held, then stop and join its server.
fn finish_operation_row(staged: Staged, source: &OperationSource, label: &str) {
    match source {
        OperationSource::Shutdown | OperationSource::Cancel => staged.release(SUPERVISOR, label),
        OperationSource::RequestTotal | OperationSource::Disconnect => {}
    }
    staged.finish(label);
}

/// How many times one answer's cause may reach the route's mapper.
const fn mapper_calls(answer: Answer) -> usize {
    match answer {
        Answer::Mapped(_) => 1,
        Answer::Unmapped(_) => 0,
    }
}

/// Assert one staged cause answered as its declared disposition says and ended
/// the transport.
fn assert_staged_answer(peer: &mut TcpStream, answer: Answer, label: &str) {
    let answered = common::read_http_response_bounded(peer)
        .unwrap_or_else(|error| panic!("{label}: the staged row went unanswered: {error}"));
    let (status, mapped) = match answer {
        Answer::Mapped(status) => (status, true),
        Answer::Unmapped(status) => (status, false),
    };
    assert_eq!(
        answered.status, status,
        "{label}: the local cause is the one answered",
    );
    assert_eq!(
        !answered.body.is_empty(),
        mapped,
        "{label}: only a mapped cause carries a body: {:?}",
        answered.body,
    );
    assert_no_upstream_payload(&answered.body, label);
    common::assert_connection_closed(peer, label);
}

/// Which of the upload's own bounds one transfer row crosses.
enum UploadLimit {
    /// The byte ceiling this route's upload budget narrowed its admission to.
    Bytes,
    /// The upload's own quiet interval.
    Idle,
    /// The upload's own lifetime.
    Total,
}

/// One row whose local cause this request's own upload raises against the held
/// upstream head.
struct UploadRow {
    label: &'static str,
    limit: UploadLimit,
    /// The terminal the upload's transfer owner fixed, or `None` when
    /// route-aware admission refused the frame and no transfer bound was
    /// crossed — the byte row, whose ceiling has one authority and not two.
    terminal: Option<InboundTerminal>,
    kind: RejectionKind,
    status: u16,
    frames_polled: usize,
}

/// Run one upload-limit row end to end.
fn run_upload_row(row: &UploadRow) {
    let label = row.label;
    let upstream =
        common::raw_upstream(200, UPSTREAM_BODY, common::UpstreamAnswers::OnHeadThenHold);
    let staged = Staged::serve(
        &upstream.backend(),
        upload_policy(&row.limit),
        UNREACHED_TOTAL,
    );
    let mut peer = staged.upload_with_held_head(label);

    stage_upload_limit(&row.limit, &mut peer);
    let answered = common::read_http_response_bounded(&mut peer)
        .unwrap_or_else(|error| panic!("{label}: the staged row went unanswered: {error}"));
    assert_eq!(
        answered.status, row.status,
        "{label}: the local limit is the one answered",
    );
    assert_no_upstream_payload(&answered.body, label);
    assert_upload_provenance(&staged, row);
    common::assert_connection_closed(&mut peer, label);

    staged.assert_quiesced_upload(row.frames_polled, label);
    assert_upstream_leg(&upstream, label);
    staged.finish(label);
}

/// Assert this row's refusal came from the owner that measured its bound.
///
/// The journal is read through `only`, which is also this row's at-most-one
/// mapper claim: a second refusal for the same request fails here.
fn assert_upload_provenance(staged: &Staged, row: &UploadRow) {
    let label = row.label;
    let seen = common::only(&staged.journal, label);
    assert_eq!(
        seen.kind, row.kind,
        "{label}: the refusal keeps the category of the owner that raised it",
    );
    let observed = staged.controller.transfers_observed().upload;
    match row.terminal {
        Some(terminal) => assert_eq!(
            awaited_terminal(
                &staged.controller,
                |controller| controller.transfers_observed().upload.terminal,
                label,
            ),
            terminal,
            "{label}: the upload owner fixed the bound this route froze",
        ),
        None => assert_eq!(
            observed.terminal, None,
            "{label}: route-aware admission owns the byte ceiling, so the transfer owner \
             fixes no terminal of its own: {observed:?}",
        ),
    }
}

/// The proxy policy one upload row's route freezes.
fn upload_policy(limit: &UploadLimit) -> ProxyPolicy {
    policy(|policy| {
        policy.upload_budget(match limit {
            UploadLimit::Bytes => TransferBudget::unbounded()
                .with_max_bytes(STAGED_CEILING)
                .expect("a finite upload maximum"),
            UploadLimit::Idle => TransferBudget::unbounded()
                .with_idle(STAGED_QUIET)
                .expect("a finite upload quiet interval"),
            UploadLimit::Total => TransferBudget::unbounded()
                .with_total(STAGED_QUIET)
                .expect("a finite upload lifetime"),
        })
    })
}

/// Make this row's upload bound ready while the upstream head stays held.
fn stage_upload_limit(limit: &UploadLimit, peer: &mut TcpStream) {
    match limit {
        UploadLimit::Bytes => {
            common::tolerate_dead_socket(common::write_chunk(peer, CROSSING_FRAME))
                .expect("the crossing frame reached the proxy")
        }
        // Both deadlines are the upload owner's own, and the peer makes them
        // ready by sending nothing more: the frame it already sent is the last
        // one either bound sees.
        UploadLimit::Idle | UploadLimit::Total => {}
    }
}

/// 12.T3
///
/// Every local source an admitted streaming forward carries is staged against
/// an upstream head this service is holding and has not committed: the request
/// total, the peer's own lifetime, the aggregate deadline a stop mints, an
/// explicit cancellation, and the three bounds this request's own upload runs
/// under. Each row reads the cause off the production owner that selected it.
#[test]
fn streaming_proxy_equal_ready_local_limit_precedes_uncommitted_upstream_head() {
    for row in [
        OperationRow {
            label: "request total",
            source: OperationSource::RequestTotal,
            terminal: InboundTerminal::RequestTotal,
            answer: Answer::Mapped(408),
        },
        OperationRow {
            label: "peer disconnect",
            source: OperationSource::Disconnect,
            terminal: InboundTerminal::Disconnect,
            answer: Answer::Unmapped(408),
        },
        OperationRow {
            label: "shutdown deadline",
            source: OperationSource::Shutdown,
            terminal: InboundTerminal::ShutdownDeadline,
            answer: Answer::Unmapped(503),
        },
        OperationRow {
            label: "forced cancellation",
            source: OperationSource::Cancel,
            terminal: InboundTerminal::ForcedCancellation,
            answer: Answer::Unmapped(503),
        },
    ] {
        staged_runtime()
            .run(|| run_operation_row(&row))
            .expect("the fixture runtime ran to completion");
    }
    for row in [
        UploadRow {
            label: "upload byte ceiling",
            limit: UploadLimit::Bytes,
            terminal: None,
            kind: RejectionKind::BodyLimit,
            status: 413,
            frames_polled: 2,
        },
        UploadRow {
            label: "upload quiet interval",
            limit: UploadLimit::Idle,
            terminal: Some(InboundTerminal::TransferIdle),
            kind: RejectionKind::BodyTimeout,
            status: 408,
            frames_polled: 1,
        },
        UploadRow {
            label: "upload lifetime",
            limit: UploadLimit::Total,
            terminal: Some(InboundTerminal::TransferTotal),
            kind: RejectionKind::BodyTimeout,
            status: 408,
            frames_polled: 1,
        },
    ] {
        staged_runtime()
            .run(|| run_upload_row(&row))
            .expect("the fixture runtime ran to completion");
    }
}

/// The runtime one staged row runs under.
///
/// Each row owns a runtime of its own because two of them end their own server,
/// and a row that shared one would be reading counters a stopped sibling still
/// owned. Shutdown is never requested from inside: the rows that stop a server
/// stop the one they built, and the fixture runtime joins what is left.
fn staged_runtime() -> runtime::RuntimeBuilder {
    common::test_runtime().shutdown_timeout(Duration::from_secs(5))
}
