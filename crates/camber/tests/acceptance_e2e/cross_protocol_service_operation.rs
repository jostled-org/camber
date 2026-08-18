//! The gRPC handoff boundary, and the protocol owner table around it.
//!
//! Camber coordinates a gRPC call only until tonic commits a response head. Up
//! to that head one coordinator weighs the local upload's terminals and the
//! operation's own carried deadlines against it, and a local winner cancels
//! tonic and maps exactly once. Past it tonic owns status and trailers: an
//! upload failure becomes the terminal error of tonic's request body, and a
//! download failure ends Camber's outer response body without inventing a
//! `grpc-status` or replacing the trailers tonic wrote.

#![cfg(feature = "grpc")]

use crate::common;
use crate::http as http_support;

use camber::http::mock::{InboundTerminal, LifecycleCheckpoint, LifecycleController};
use camber::http::{
    GrpcRouter, RejectionKind, RejectionProtocol, RequestBudget, Router, ServerPolicy,
    TransferBudget,
};
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

// The whole generated module is included, and this root drives only the
// full-duplex service in it. The unary wrapper beside it is generated all the
// same, so its unused constructor is allowed here rather than in the codegen.
#[allow(dead_code)]
mod proto {
    tonic::include_proto!("greeter");
}

use proto::{HelloReply, HelloRequest, streamer_server};

/// The wire path tonic dispatches the full-duplex fixture RPC on.
const ECHO_PATH: &str = "/greeter.Streamer/Echo";

/// How long any one exchange, checkpoint wait, or teardown here may take.
const BOUND: Duration = Duration::from_secs(10);

/// The quiet interval a staged transfer row freezes.
const STAGED_QUIET: Duration = Duration::from_millis(250);

/// The request lifetime a staged pre-head row freezes.
const STAGED_TOTAL: Duration = Duration::from_millis(500);

/// A deadline no row is meant to reach.
const UNREACHED: Duration = Duration::from_secs(300);

/// The margin a row waits past a frozen deadline before it reads its effect.
const PAST_DEADLINE: Duration = Duration::from_millis(150);

// ---------------------------------------------------------------------------
// The full-duplex fixture service
// ---------------------------------------------------------------------------

/// What the fixture RPC does with the request stream tonic hands it.
#[derive(Clone, Copy, Eq, PartialEq)]
enum EchoPlan {
    /// Reply once per request message, and end when the request stream ends.
    Echo,
    /// Refuse the call with a status of the service's own after one message.
    Refuse,
}

/// What the fixture service recorded about the stream it was given.
///
/// Written by the service itself, which is the only owner that can say what
/// tonic handed it: a request-body failure reaches the service as a
/// `tonic::Status`, and its code is the one tonic then writes into trailers.
#[derive(Default)]
struct EchoLog {
    /// The status tonic derived from a failed request body, once it has one.
    fault: Mutex<Option<(i32, Box<str>)>>,
}

impl EchoLog {
    fn record_fault(&self, status: &tonic::Status) {
        let mut fault = self.fault.lock().unwrap_or_else(|error| error.into_inner());
        *fault = Some((status.code() as i32, status.message().into()));
    }

    /// The status code tonic handed the service for a failed request body.
    fn fault_code(&self) -> Option<i32> {
        self.fault
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|(code, _)| *code)
    }

    /// What tonic said about the failed request body.
    fn fault_message(&self) -> Option<Box<str>> {
        self.fault
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|(_, message)| message.clone())
    }
}

/// The status the refusing plan answers with, on the service's own authority.
const REFUSED_CODE: i32 = tonic::Code::PermissionDenied as i32;
/// Written as one token: gRPC percent-encodes a trailer's message, so a phrase
/// with spaces would make this row assert an encoding rather than the message.
const REFUSED_MESSAGE: &str = "fixture-service-refused-this-call";

struct EchoService {
    plan: EchoPlan,
    log: Arc<EchoLog>,
}

type EchoItem = Result<HelloReply, tonic::Status>;

#[tonic::async_trait]
impl streamer_server::Streamer for EchoService {
    type EchoStream = tokio_stream::wrappers::ReceiverStream<EchoItem>;

    /// Answer before the request stream ends, and keep reading it afterwards.
    ///
    /// The head is available the moment this returns, so a case can hold it
    /// uncommitted while the request body it is still reading reaches a
    /// terminal of its own — which is the tie the pre-head coordinator decides.
    /// Draining from a task of the service's own is what keeps that body polled
    /// while the head waits; a service that only read inside its own answer
    /// would leave the upload unpolled and unable to reach anything.
    async fn echo(
        &self,
        request: tonic::Request<tonic::Streaming<HelloRequest>>,
    ) -> Result<tonic::Response<Self::EchoStream>, tonic::Status> {
        let mut inbound = request.into_inner();
        let log = Arc::clone(&self.log);
        let plan = self.plan;
        let (replies, stream) = tokio::sync::mpsc::channel(4);
        tokio::spawn(async move { while forwarded(&mut inbound, &replies, plan, &log).await {} });
        Ok(tonic::Response::new(
            tokio_stream::wrappers::ReceiverStream::new(stream),
        ))
    }
}

/// Read one request message and answer it. `false` ends this stream.
async fn forwarded(
    inbound: &mut tonic::Streaming<HelloRequest>,
    replies: &tokio::sync::mpsc::Sender<EchoItem>,
    plan: EchoPlan,
    log: &EchoLog,
) -> bool {
    match inbound.message().await {
        Ok(Some(request)) => replied(replies, plan, answer_for(plan, &request.name)).await,
        Ok(None) => false,
        // The terminal error of tonic's request body. Recorded and forwarded
        // unchanged: tonic writes this status into the trailers, and the case
        // reads that the wire carries the same one rather than something Camber
        // invented.
        Err(status) => {
            log.record_fault(&status);
            let _ = replies.send(Err(status)).await;
            false
        }
    }
}

/// Send one answer, and say whether this stream reads another message.
///
/// A refusing plan stops after the one status it produced, and a peer that
/// stopped reading leaves nothing this stream could still say.
async fn replied(
    replies: &tokio::sync::mpsc::Sender<EchoItem>,
    plan: EchoPlan,
    item: EchoItem,
) -> bool {
    let delivered = replies.send(item).await.is_ok();
    delivered && matches!(plan, EchoPlan::Echo)
}

/// The item one received request message produces under `plan`.
fn answer_for(plan: EchoPlan, name: &str) -> EchoItem {
    match plan {
        EchoPlan::Echo => Ok(HelloReply {
            message: format!("Hello, {name}!"),
        }),
        EchoPlan::Refuse => Err(tonic::Status::permission_denied(REFUSED_MESSAGE)),
    }
}

// ---------------------------------------------------------------------------
// gRPC wire framing
// ---------------------------------------------------------------------------

/// One length-prefixed gRPC message carrying a `HelloRequest` named `name`.
///
/// Hand-framed rather than driven through a tonic client, because these rows
/// turn on when a request frame reaches the wire and when the body after it
/// does not. A generated client sends the whole call and reads the whole
/// answer, which no row that holds a head uncommitted can use.
fn grpc_message(name: &str) -> Box<[u8]> {
    assert!(
        name.len() < 128,
        "the fixture encoder writes one length byte"
    );
    let mut message = Vec::with_capacity(name.len() + 7);
    message.push(0x0A);
    message.push(u8::try_from(name.len()).expect("the fixture name is short"));
    message.extend_from_slice(name.as_bytes());
    let mut framed = Vec::with_capacity(message.len() + 5);
    framed.push(0);
    framed.extend_from_slice(
        &u32::try_from(message.len())
            .expect("one short message")
            .to_be_bytes(),
    );
    framed.extend_from_slice(&message);
    framed.into_boxed_slice()
}

/// The head every gRPC row opens its stream with.
fn grpc_headers() -> [(&'static str, &'static str); 2] {
    [("content-type", "application/grpc"), ("te", "trailers")]
}

// ---------------------------------------------------------------------------
// The served fixture
// ---------------------------------------------------------------------------

/// The budgets one row's router freezes.
#[derive(Clone, Copy)]
struct RowBudgets {
    request: RequestBudget,
    upload: TransferBudget,
    download: TransferBudget,
}

impl RowBudgets {
    /// Budgets no row reaches, for the rows that stage one dimension only.
    fn unreached() -> Self {
        Self {
            request: RequestBudget::unbounded()
                .with_total(UNREACHED)
                .expect("a finite request total"),
            upload: TransferBudget::unbounded(),
            download: TransferBudget::unbounded(),
        }
    }

    fn with_request_total(self, total: Duration) -> Self {
        Self {
            request: RequestBudget::unbounded()
                .with_total(total)
                .expect("a finite request total"),
            ..self
        }
    }

    fn with_upload_idle(self, idle: Duration) -> Self {
        Self {
            upload: TransferBudget::unbounded()
                .with_idle(idle)
                .expect("a finite upload quiet interval"),
            ..self
        }
    }

    fn with_download_idle(self, idle: Duration) -> Self {
        Self {
            download: TransferBudget::unbounded()
                .with_idle(idle)
                .expect("a finite download quiet interval"),
            ..self
        }
    }
}

/// One served gRPC fixture, its observer, and what its mapper recorded.
struct GrpcFixture {
    server: http_support::ObservedServer,
    controller: Arc<LifecycleController>,
    journal: common::Journal,
    log: Arc<EchoLog>,
}

impl GrpcFixture {
    /// Serve one router carrying the full-duplex service under `budgets`.
    fn serve(plan: EchoPlan, budgets: RowBudgets) -> Self {
        let journal = common::journal();
        let log = Arc::new(EchoLog::default());
        let mut router =
            Router::new().rejection_mapper(common::recording_mapper(&journal, "grpc-route"));
        router.grpc(
            GrpcRouter::new().add_service(streamer_server::StreamerServer::new(EchoService {
                plan,
                log: Arc::clone(&log),
            })),
        );
        let router = router
            .request_budget(budgets.request)
            .upload_budget(budgets.upload)
            .download_budget(budgets.download);
        let port = http_support::reserve_observed();
        let controller = port.controller();
        let server = port.serve_with_policy(
            router,
            ServerPolicy::default()
                .shutdown_timeout(Duration::from_millis(500))
                .expect("a finite aggregate deadline"),
        );
        Self {
            server,
            controller,
            journal,
            log,
        }
    }

    fn addr(&self) -> std::net::SocketAddr {
        self.server.addr()
    }

    fn arm(&self, checkpoint: LifecycleCheckpoint, label: &str) {
        self.controller
            .pause_once(checkpoint)
            .unwrap_or_else(|error| panic!("{label}: arming {checkpoint:?} failed: {error}"));
    }

    fn release(&self, checkpoint: LifecycleCheckpoint, label: &str) {
        self.controller
            .release(checkpoint)
            .unwrap_or_else(|error| panic!("{label}: releasing {checkpoint:?} failed: {error}"));
    }

    async fn wait_paused(&self, checkpoint: LifecycleCheckpoint, label: &str) {
        http_support::wait_until_paused_bounded(&self.controller, checkpoint, label).await;
    }

    /// Everything the route's mapper was handed, taken once.
    fn mapped(&self) -> Box<[common::Observed]> {
        common::drain(&self.journal)
    }

    fn assert_never_mapped(&self, label: &str) {
        let mapped = self.mapped();
        assert!(
            mapped.is_empty(),
            "{label}: tonic owned this boundary, so no Camber mapper may run: {mapped:?}",
        );
    }

    fn finish(self, label: &str) {
        self.server
            .shutdown_bounded(BOUND)
            .unwrap_or_else(|error| panic!("{label}: teardown failed: {error}"));
    }
}

/// One opened RPC: the request half, and the connection it lives on.
struct OpenedRpc {
    client: common::PersistentH2Client,
    stream: common::H2RequestStream,
}

impl OpenedRpc {
    /// Open the fixture RPC and send one request message.
    async fn start(addr: std::net::SocketAddr, name: &str) -> Self {
        let mut client = common::PersistentH2Client::connect(addr, BOUND).await;
        let mut stream = client
            .open_paced("POST", ECHO_PATH, "localhost", &grpc_headers())
            .await;
        let offered = stream.offer(&grpc_message(name), BOUND).await;
        assert_eq!(
            offered,
            common::H2Offer::Sent,
            "the fixture peer could not send its first gRPC message",
        );
        Self { client, stream }
    }

    async fn close(self) {
        self.client.close().await;
    }
}

// ---------------------------------------------------------------------------
// 13.T1
// ---------------------------------------------------------------------------

/// One row whose local cause is staged against an uncommitted tonic head.
struct PreHeadRow {
    label: &'static str,
    budgets: RowBudgets,
    terminal: InboundTerminal,
    /// How long the row waits for the deadline it froze to expire.
    elapse: Duration,
    kind: RejectionKind,
    status: u16,
}

/// 13.T1
///
/// A local budget the operation carries, and a local terminal the upload it
/// handed tonic fixed, both beat a tonic head that became ready in the same
/// scheduling turn. The winner cancels tonic, maps once through the route's own
/// mapper, and commits nothing tonic produced. A head that wins alone commits
/// without any mapper call and hands authority on.
#[test]
fn grpc_prehead_budget_beats_equal_ready_tonic_head_and_maps_once() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                for row in &pre_head_rows() {
                    run_pre_head_row(row).await;
                }
                assert_tonic_head_wins_alone().await;
            });
        })
        .expect("the gRPC pre-head runtime ran to completion");
}

/// The two local causes staged against an uncommitted head.
fn pre_head_rows() -> [PreHeadRow; 2] {
    [
        PreHeadRow {
            label: "request total",
            budgets: RowBudgets::unreached().with_request_total(STAGED_TOTAL),
            terminal: InboundTerminal::RequestTotal,
            elapse: STAGED_TOTAL + PAST_DEADLINE,
            kind: RejectionKind::RequestTimeout,
            status: 408,
        },
        PreHeadRow {
            label: "upload quiet interval",
            budgets: RowBudgets::unreached().with_upload_idle(STAGED_QUIET),
            terminal: InboundTerminal::TransferIdle,
            elapse: STAGED_QUIET + PAST_DEADLINE,
            kind: RejectionKind::BodyTimeout,
            status: 408,
        },
    ]
}

/// Run one pre-head row: hold the head, make the local cause ready, release.
async fn run_pre_head_row(row: &PreHeadRow) {
    let label = row.label;
    let fixture = GrpcFixture::serve(EchoPlan::Echo, row.budgets);
    fixture.arm(LifecycleCheckpoint::GrpcHeadReady, label);
    let mut rpc = OpenedRpc::start(fixture.addr(), "camber").await;
    fixture
        .wait_paused(LifecycleCheckpoint::GrpcHeadReady, label)
        .await;

    let selected = LifecycleCheckpoint::InboundTerminalSelected(row.terminal);
    fixture.arm(selected, label);
    // The frozen deadline is real time, so waiting it out is the production
    // event itself rather than a race made likely by a sleep.
    tokio::time::sleep(row.elapse).await;
    fixture.release(LifecycleCheckpoint::GrpcHeadReady, label);
    fixture.wait_paused(selected, label).await;
    fixture.release(selected, label);

    let answered = rpc.stream.commit().await;
    assert_eq!(
        answered.status(),
        row.status,
        "{label}: a local cause selected before commitment is answered by the route's mapper",
    );
    let settled = answered.settle().await;
    assert_eq!(
        settled.trailer("grpc-status"),
        None,
        "{label}: Camber owned this boundary, so nothing may write a gRPC status: {settled:?}",
    );

    let mapped = fixture.mapped();
    assert_eq!(
        mapped.len(),
        1,
        "{label}: a pre-commit cause invokes the selected mapper exactly once: {mapped:?}",
    );
    let observed = &mapped[0];
    assert_eq!(
        observed.kind, row.kind,
        "{label}: mapped under the wrong kind"
    );
    assert_eq!(
        observed.protocol,
        Some(RejectionProtocol::Grpc),
        "{label}: the refusal keeps the class the pre-check established",
    );
    assert_eq!(
        observed.route.as_deref(),
        Some("grpc"),
        "{label}: the refusal keeps the identity gRPC dispatch is named by",
    );

    rpc.close().await;
    fixture.finish(label);
}

/// A tonic head that becomes ready with no local cause beside it commits, and
/// every later status is tonic's.
async fn assert_tonic_head_wins_alone() {
    let label = "tonic head";
    let fixture = GrpcFixture::serve(EchoPlan::Echo, RowBudgets::unreached());
    let committed = LifecycleCheckpoint::GrpcHandoffCommitted;
    fixture.arm(committed, label);

    let mut rpc = OpenedRpc::start(fixture.addr(), "camber").await;
    // The head reaches the peer only after this boundary, so being held here is
    // itself the proof that an uncontested head crossed it.
    fixture.wait_paused(committed, label).await;
    let observed = fixture.controller.operations_observed();
    assert_eq!(
        (observed.admitted, observed.distinct_identities),
        (1, 1),
        "{label}: one admitted gRPC head mints one envelope: {observed:?}",
    );
    assert_eq!(
        (observed.dispatch, observed.middleware, observed.body),
        (1, 1, 1),
        "{label}: every pre-head owner reads that one envelope once: {observed:?}",
    );
    fixture.release(committed, label);

    let answered = rpc.stream.commit().await;
    assert_eq!(
        answered.status(),
        200,
        "{label}: an uncontested tonic head commits as tonic produced it",
    );
    rpc.stream.finish();
    let settled = answered.settle().await;

    assert_eq!(
        settled.trailer("grpc-status"),
        Some("0"),
        "{label}: tonic owns the status behind a committed head: {settled:?}",
    );
    assert!(
        settled.bytes > 0,
        "{label}: tonic's reply reached the peer: {settled:?}",
    );
    assert!(
        !settled.reset,
        "{label}: an uncontested call ends its stream rather than resetting it",
    );
    assert_eq!(
        fixture.controller.operations_observed().response_head,
        1,
        "{label}: one committed head reaches the response-head owner exactly once",
    );
    fixture.assert_never_mapped(label);

    rpc.close().await;
    fixture.finish(label);
}

// ---------------------------------------------------------------------------
// 13.T2
// ---------------------------------------------------------------------------

/// 13.T2
///
/// Past a committed tonic head Camber maps nothing. An upload terminal becomes
/// the terminal error of tonic's request body and reaches the service as the
/// status tonic then writes into trailers; a download terminal ends Camber's
/// outer body and resets the one stream without a `grpc-status` of its own; and
/// a status the service itself produced travels to the peer unchanged.
#[test]
fn grpc_posthead_failures_preserve_tonic_status_and_trailers() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_posthead_upload_failure_reaches_tonic().await;
                assert_posthead_download_failure_stays_stream_local().await;
                assert_service_status_and_trailers_are_authoritative().await;
            });
        })
        .expect("the gRPC post-handoff runtime ran to completion");
}

/// An upload terminal after the head ends tonic's request body, and the status
/// tonic derived from it is the one the wire carries.
async fn assert_posthead_upload_failure_reaches_tonic() {
    let label = "post-head upload";
    let fixture = GrpcFixture::serve(
        EchoPlan::Echo,
        RowBudgets::unreached().with_upload_idle(STAGED_QUIET),
    );
    let mut rpc = OpenedRpc::start(fixture.addr(), "camber").await;
    let answered = rpc.stream.commit().await;
    assert_eq!(
        answered.status(),
        200,
        "{label}: the head commits before the upload can fail",
    );

    // Nothing more is sent, so the upload's own quiet interval expires under a
    // head that is already on the wire.
    let settled = answered.settle().await;

    assert_eq!(
        fixture.controller.transfers_observed().upload.terminal,
        Some(InboundTerminal::TransferIdle),
        "{label}: the upload owner fixed the terminal its own policy names",
    );
    assert_eq!(
        fixture.log.fault_code().is_some(),
        true,
        "{label}: the request body's terminal error reached tonic's service",
    );
    let carried = settled
        .trailer("grpc-status")
        .expect("a committed gRPC answer carries tonic's status in its trailers")
        .parse::<i32>()
        .expect("the gRPC status trailer is an integer");
    assert_eq!(
        Some(carried),
        fixture.log.fault_code(),
        "{label}: the trailer carries the status tonic derived, not one Camber wrote: {settled:?}",
    );
    assert_ne!(
        carried, 0,
        "{label}: a failed request body is not a success"
    );
    assert!(
        fixture
            .log
            .fault_message()
            .is_some_and(|message| !message.is_empty()),
        "{label}: tonic named the request-body failure it read",
    );
    fixture.assert_never_mapped(label);

    rpc.close().await;
    fixture.finish(label);
}

/// A download terminal after the head ends Camber's outer body, resets that one
/// stream, and writes no gRPC status.
async fn assert_posthead_download_failure_stays_stream_local() {
    let label = "post-head download";
    let fixture = GrpcFixture::serve(
        EchoPlan::Echo,
        RowBudgets::unreached().with_download_idle(STAGED_QUIET),
    );
    let mut rpc = OpenedRpc::start(fixture.addr(), "camber").await;
    let answered = rpc.stream.commit().await;
    assert_eq!(
        answered.status(),
        200,
        "{label}: the head commits before the download can fail",
    );

    // The peer sends nothing more, so the service produces nothing more and the
    // download's own quiet interval is what ends the answer.
    let settled = answered.settle().await;

    assert_eq!(
        fixture.controller.transfers_observed().download.terminal,
        Some(InboundTerminal::TransferIdle),
        "{label}: the download owner fixed the terminal its own policy names",
    );
    assert!(
        settled.reset,
        "{label}: a post-commit terminal resets its own HTTP/2 stream: {settled:?}",
    );
    assert_eq!(
        settled.trailer("grpc-status"),
        None,
        "{label}: Camber's own terminal never synthesizes a gRPC status: {settled:?}",
    );
    assert_eq!(
        settled.status, 200,
        "{label}: a committed status cannot be replaced after the head",
    );
    fixture.assert_never_mapped(label);

    // The connection under that stream is still usable, which is what makes the
    // reset stream-local rather than a connection failure.
    let reused = rpc
        .client
        .send_complete("GET", "/absent", "localhost", &[], b"")
        .await;
    assert_eq!(
        reused.status, 404,
        "{label}: a reset gRPC stream leaves its connection framable",
    );
    let mapped = fixture.mapped();
    assert_eq!(
        mapped.len(),
        1,
        "{label}: only the reused stream's own routing refusal is mapped: {mapped:?}",
    );

    rpc.close().await;
    fixture.finish(label);
}

/// A status the service produced reaches the peer as tonic wrote it.
async fn assert_service_status_and_trailers_are_authoritative() {
    let label = "service status";
    let fixture = GrpcFixture::serve(EchoPlan::Refuse, RowBudgets::unreached());
    let mut rpc = OpenedRpc::start(fixture.addr(), "camber").await;
    let answered = rpc.stream.commit().await;
    assert_eq!(
        answered.status(),
        200,
        "{label}: an application refusal is still a committed gRPC answer",
    );
    rpc.stream.finish();
    let settled = answered.settle().await;

    assert_eq!(
        settled
            .trailer("grpc-status")
            .and_then(|code| code.parse().ok()),
        Some(REFUSED_CODE),
        "{label}: tonic's trailers stay authoritative: {settled:?}",
    );
    assert_eq!(
        settled.trailer("grpc-message"),
        Some(REFUSED_MESSAGE),
        "{label}: the message behind that status is the service's own",
    );
    fixture.assert_never_mapped(label);

    rpc.close().await;
    fixture.finish(label);
}

// ---------------------------------------------------------------------------
// 13.T3
// ---------------------------------------------------------------------------

/// 13.T3
///
/// One owner table over real transports. Only the boundaries Camber owned reach
/// a rejection mapper: a head Hyper refused never became an operation, tonic's
/// post-handoff status is tonic's, a WebSocket direction after `101` belongs to
/// its bridge, a proxy failure before commitment is Camber's to map, and a
/// stream terminal after commitment ends its body without replacing a status.
#[test]
fn prehead_and_posthandoff_protocol_failures_stay_with_their_owner() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_hyper_prehead_failure_reaches_no_mapper().await;
                assert_tonic_posthandoff_status_reaches_no_mapper().await;
                assert_proxy_precommit_failure_is_camber_mapped().await;
                assert_stream_postcommit_terminal_reaches_no_mapper().await;
                #[cfg(feature = "ws")]
                assert_websocket_post_101_reaches_no_mapper().await;
            });
        })
        .expect("the protocol owner table ran to completion");
}

/// A head Hyper refuses has no request policy, so no Camber mapper runs and no
/// operation is ever minted.
async fn assert_hyper_prehead_failure_reaches_no_mapper() {
    let label = "hyper pre-head";
    let journal = common::journal();
    let mut router =
        Router::new().rejection_mapper(common::recording_mapper(&journal, "hyper-owner"));
    router.get("/quick", |_req: &camber::http::Request| async {
        camber::http::Response::text(200, "quick")
    });
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let server = port.serve(router);
    let addr = server.addr();

    // A second, unparsable `content-length` beside the framed one: Hyper
    // refuses the head at its own parser, before any route, policy, or body
    // owner exists to answer for it.
    let answered = tokio::task::spawn_blocking(move || {
        http_support::request(
            addr,
            "GET",
            "/quick",
            &[("content-length", "invalid")],
            b"",
            BOUND,
        )
    })
    .await
    .expect("the malformed peer settled")
    .expect("Hyper answered the malformed head");

    assert_eq!(
        answered.status,
        400,
        "{label}: Hyper refuses a head it cannot parse: {}",
        answered.text(),
    );
    assert!(
        common::drain(&journal).is_empty(),
        "{label}: a head that never reached routing has no policy to map under",
    );
    assert_eq!(
        controller.operations_observed().admitted,
        0,
        "{label}: a refused head mints no operation envelope",
    );
    server
        .shutdown_bounded(BOUND)
        .expect("the Hyper pre-head fixture tore down");
}

/// A status tonic produced past its handoff never reaches a Camber mapper.
async fn assert_tonic_posthandoff_status_reaches_no_mapper() {
    assert_service_status_and_trailers_are_authoritative().await;
}

/// A proxy failure before commitment is Camber's own boundary, and it maps.
async fn assert_proxy_precommit_failure_is_camber_mapped() {
    let label = "proxy pre-commit";
    let journal = common::journal();
    let mut router =
        Router::new().rejection_mapper(common::recording_mapper(&journal, "proxy-owner"));
    // A backend nothing listens on: the connect phase fails before any upstream
    // head exists, which is the phase Camber still owns.
    router.proxy("/upstream", "http://127.0.0.1:1");
    let server = http_support::reserve_observed().serve(router);
    let addr = server.addr();

    let answered = tokio::task::spawn_blocking(move || {
        http_support::request(addr, "GET", "/upstream/thing", &[], b"", BOUND)
    })
    .await
    .expect("the proxy peer settled")
    .expect("the proxy answer was read");

    assert_eq!(
        answered.status, 502,
        "{label}: an unreachable upstream is refused before commitment",
    );
    let mapped = common::drain(&journal);
    assert_eq!(
        mapped.len(),
        1,
        "{label}: a boundary Camber owns maps exactly once: {mapped:?}",
    );
    assert_eq!(
        mapped[0].protocol,
        Some(RejectionProtocol::Proxy),
        "{label}: the refusal keeps the class the proxy route established",
    );
    server
        .shutdown_bounded(BOUND)
        .expect("the proxy owner fixture tore down");
}

/// A stream terminal after commitment ends its body and replaces no status.
async fn assert_stream_postcommit_terminal_reaches_no_mapper() {
    let label = "stream post-commit";
    let journal = common::journal();
    let mut router =
        Router::new().rejection_mapper(common::recording_mapper(&journal, "stream-owner"));
    router.get_stream("/held", |_req: &camber::http::Request| {
        Box::pin(async move {
            let (streamed, sender) = camber::http::StreamResponse::new(200);
            tokio::spawn(async move {
                let _committed = sender.send("first").await;
                tokio::time::sleep(UNREACHED).await;
            });
            streamed
        })
    });
    let router = router.download_budget(
        TransferBudget::unbounded()
            .with_idle(STAGED_QUIET)
            .expect("a finite download quiet interval"),
    );
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let server = port.serve(router);
    let addr = server.addr();

    let mut client = common::PersistentH2Client::connect(addr, BOUND).await;
    let mut held = client.open_paced("GET", "/held", "localhost", &[]).await;
    held.finish();
    let answered = held.commit().await;
    assert_eq!(
        answered.status(),
        200,
        "{label}: a streaming head commits before its producer stalls",
    );
    let settled = answered.settle().await;

    assert_eq!(
        controller.transfers_observed().download.terminal,
        Some(InboundTerminal::TransferIdle),
        "{label}: the download owner fixed the terminal its own policy names",
    );
    assert!(
        settled.reset,
        "{label}: a post-commit terminal resets its own stream: {settled:?}",
    );
    assert!(
        common::drain(&journal).is_empty(),
        "{label}: a committed status reaches no mapper",
    );
    client.close().await;
    server
        .shutdown_bounded(BOUND)
        .expect("the stream owner fixture tore down");
}

/// A WebSocket direction after `101` belongs to its bridge, not to a mapper.
#[cfg(feature = "ws")]
async fn assert_websocket_post_101_reaches_no_mapper() {
    let label = "websocket post-101";
    let journal = common::journal();
    let mut router = Router::new().rejection_mapper(common::recording_mapper(&journal, "ws-owner"));
    router.ws(
        "/ws",
        |_req: &camber::http::Request, mut conn: camber::http::WsConn| {
            while let Some(message) = conn.recv() {
                if conn.send(&message).is_err() {
                    break;
                }
            }
            Ok(())
        },
    );
    let server = http_support::reserve_observed().serve(router);
    let addr = server.addr();

    let observed = tokio::task::spawn_blocking(move || {
        let mut peer = common::start_upgrade(addr, "/ws");
        let head = common::read_until_double_crlf(&mut peer);
        common::write_ws_close_frame(&mut peer);
        head
    })
    .await
    .expect("the WebSocket peer settled");

    assert!(
        observed.starts_with("HTTP/1.1 101"),
        "{label}: the handshake commits before the bridge owns the transport: {observed}",
    );
    assert!(
        common::drain(&journal).is_empty(),
        "{label}: a direction that ends after 101 is the bridge's terminal, not a mapped refusal",
    );
    server
        .shutdown_bounded(BOUND)
        .expect("the WebSocket owner fixture tore down");
}
