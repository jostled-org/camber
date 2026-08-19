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
    BodyAdmission, BodyAdmissionContext, GrpcRouter, RejectionKind, RejectionProtocol,
    RequestBudget, Router, ServerPolicy, TransferBudget,
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
    /// Handed to every head this service produces. See [`HeadWitness`].
    head_dropped: tokio::sync::mpsc::UnboundedSender<()>,
}

type EchoItem = Result<HelloReply, tonic::Status>;

/// The witness over one head tonic produced.
///
/// The answer's own stream carries it, so its drop is the moment the produced
/// head was released, and its silence is a head still held somewhere. That is
/// the difference between cancelling tonic and leaving its answer alive beside
/// a request this service already ended — a distinction the status on the wire
/// cannot make, because a retained head writes nothing either.
struct HeadWitness {
    replies: tokio_stream::wrappers::ReceiverStream<EchoItem>,
    dropped: tokio::sync::mpsc::UnboundedSender<()>,
}

impl tokio_stream::Stream for HeadWitness {
    type Item = EchoItem;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        std::pin::Pin::new(&mut self.replies).poll_next(cx)
    }
}

impl Drop for HeadWitness {
    fn drop(&mut self) {
        // The reader outlives every head this fixture produces, so a send that
        // fails means the case is already gone.
        let _ = self.dropped.send(());
    }
}

#[tonic::async_trait]
impl streamer_server::Streamer for EchoService {
    type EchoStream = HeadWitness;

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
        Ok(tonic::Response::new(HeadWitness {
            replies: tokio_stream::wrappers::ReceiverStream::new(stream),
            dropped: self.head_dropped.clone(),
        }))
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

/// One length-prefixed gRPC message whose payload is not a `HelloRequest`.
///
/// The frame itself is well formed — a five-byte prefix declaring exactly the
/// two payload bytes behind it — so it reaches tonic's codec intact and fails
/// inside it. The payload opens field 1 as length-delimited and declares five
/// bytes that never arrive, which prost refuses outright rather than skipping
/// the way it skips a field it does not know.
fn grpc_undecodable_message() -> Box<[u8]> {
    let message = [0x0A_u8, 0x05];
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
    /// Every head this fixture's service produced and then lost.
    head_dropped: tokio::sync::mpsc::UnboundedReceiver<()>,
}

impl GrpcFixture {
    /// Serve one router carrying the full-duplex service under `budgets`.
    fn serve(plan: EchoPlan, budgets: RowBudgets) -> Self {
        let journal = common::journal();
        let log = Arc::new(EchoLog::default());
        let (head_dropped, dropped_heads) = tokio::sync::mpsc::unbounded_channel();
        let mut router =
            Router::new().rejection_mapper(common::recording_mapper(&journal, "grpc-route"));
        router.grpc(
            GrpcRouter::new().add_service(streamer_server::StreamerServer::new(EchoService {
                plan,
                log: Arc::clone(&log),
                head_dropped,
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
                .shutdown_timeout(FORCED_AFTER)
                .expect("a finite aggregate deadline"),
        );
        Self {
            server,
            controller,
            journal,
            log,
            head_dropped: dropped_heads,
        }
    }

    /// Assert no head this service produced has been released yet.
    ///
    /// Read while a checkpoint holds one, so the silence is a head still in
    /// hand rather than a head that was never produced.
    fn assert_head_held(&mut self, label: &str) {
        assert_eq!(
            self.head_dropped.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty),
            "{label}: the head tonic produced was released before any cause selected it",
        );
    }

    /// Wait, bounded, for the head tonic produced to be released.
    async fn await_head_released(&mut self, label: &str) {
        let released = tokio::time::timeout(BOUND, self.head_dropped.recv())
            .await
            .unwrap_or_else(|_| {
                panic!("{label}: the head tonic produced outlived the cause that ended its request")
            });
        assert!(
            released.is_some(),
            "{label}: the witness over the produced head was dropped with the service",
        );
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
    let mut fixture = GrpcFixture::serve(EchoPlan::Echo, row.budgets);
    fixture.arm(LifecycleCheckpoint::GrpcHeadReady, label);
    let mut rpc = OpenedRpc::start(fixture.addr(), "camber").await;
    fixture
        .wait_paused(LifecycleCheckpoint::GrpcHeadReady, label)
        .await;
    // The head exists and is still tonic's answer to give: the checkpoint holds
    // it, and the witness inside it has not fired.
    fixture.assert_head_held(label);

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
    // Cancelled, not merely unread: the local winner released the head tonic
    // had already produced rather than leaving it alive past the request it
    // belonged to.
    fixture.await_head_released(label).await;

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
/// a rejection mapper: a head Hyper refused never became an operation, a frame
/// tonic's own codec refused past the handoff is answered in tonic's trailers,
/// a frame the WebSocket bridge refuses after `101` is the bridge's terminal, a
/// proxy failure before commitment is Camber's to map, and a stream terminal
/// after commitment ends its body without replacing a status.
///
/// Every row drives a failure of its own. A row that took a clean journey would
/// hold with the ownership it claims unwired, because an empty mapper journal is
/// what a success looks like too.
#[test]
fn prehead_and_posthandoff_protocol_failures_stay_with_their_owner() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_hyper_prehead_failure_reaches_no_mapper().await;
                assert_tonic_posthandoff_decode_failure_reaches_no_mapper().await;
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

/// A failure inside tonic's own codec past the handoff is tonic's to answer.
///
/// The owner-table row for tonic, and its own journey rather than a second run
/// of 13.T2's: the cause here is neither a budget Camber froze nor a status the
/// application chose. tonic reads a frame it cannot decode, derives the status
/// itself, and writes it into its own trailers under the head it already
/// committed. Camber sees a boundary it does not own and maps nothing.
async fn assert_tonic_posthandoff_decode_failure_reaches_no_mapper() {
    let label = "tonic post-handoff";
    let fixture = GrpcFixture::serve(EchoPlan::Echo, RowBudgets::unreached());
    let mut rpc = OpenedRpc::start(fixture.addr(), "camber").await;
    let answered = rpc.stream.commit().await;
    assert_eq!(
        answered.status(),
        200,
        "{label}: the head commits before the codec behind it can fail",
    );

    let offered = rpc.stream.offer(&grpc_undecodable_message(), BOUND).await;
    assert_eq!(
        offered,
        common::H2Offer::Sent,
        "{label}: the undecodable message reached the wire",
    );
    rpc.stream.finish();
    let settled = answered.settle().await;

    let carried = settled
        .trailer("grpc-status")
        .expect("a committed gRPC answer carries tonic's status in its trailers")
        .parse::<i32>()
        .expect("the gRPC status trailer is an integer");
    assert_eq!(
        Some(carried),
        fixture.log.fault_code(),
        "{label}: the trailer carries the status tonic derived from its own decode: {settled:?}",
    );
    assert_ne!(
        carried, 0,
        "{label}: a message tonic could not decode is not a success"
    );
    assert_ne!(
        carried, REFUSED_CODE,
        "{label}: the status is tonic's own, not one the application chose",
    );
    assert_eq!(
        settled.status, 200,
        "{label}: a committed status cannot be replaced after the head",
    );
    assert!(
        !settled.reset,
        "{label}: tonic ends its own stream with trailers rather than resetting it: {settled:?}",
    );
    fixture.assert_never_mapped(label);

    rpc.close().await;
    fixture.finish(label);
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

/// Echo one message the WebSocket bridge delivered back to the peer.
///
/// A send that fails needs no answer here: the connection is over, and the very
/// next receive reports the bridge's own cause for it.
#[cfg(feature = "ws")]
fn echoed(sender: &camber::http::WsSender, message: camber::http::WsMessage) {
    match message {
        camber::http::WsMessage::Text(text) => {
            let _ = sender.send(&text);
        }
        // Nothing in this row sends a binary frame.
        camber::http::WsMessage::Binary(_) => {}
    }
}

/// A WebSocket direction that *fails* after `101` belongs to its bridge, not to
/// a mapper.
///
/// The peer completes the handshake, exchanges one message so the bridge is
/// provably pumping the transport, and then writes a frame with the mask bit
/// clear — which a server must refuse. That refusal is a real terminal on a
/// direction past `101`, and the row reads both halves of the claim: the bridge
/// fixed the cause itself, and nothing about it reached the route's mapper. A
/// clean close would satisfy the second half on its own, which is why this row
/// does not take one.
#[cfg(feature = "ws")]
async fn assert_websocket_post_101_reaches_no_mapper() {
    let label = "websocket post-101";
    let journal = common::journal();
    let (reported, causes) = std::sync::mpsc::channel();
    let reported = Arc::new(Mutex::new(reported));
    let mut router = Router::new().rejection_mapper(common::recording_mapper(&journal, "ws-owner"));
    router.ws(
        "/ws",
        move |_req: &camber::http::Request, conn: camber::http::WsConn| {
            let (sender, mut receiver) = conn.split();
            // The bridge's own answer for this connection, taken from the
            // receive owner rather than inferred from a closed socket.
            let cause = loop {
                match receiver.recv()? {
                    camber::http::WsReceive::Message(message) => echoed(&sender, message),
                    camber::http::WsReceive::Closed(cause) => break cause,
                }
            };
            let _ = reported
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .send(cause);
            Ok(())
        },
    );
    let server = http_support::reserve_observed().serve(router);
    let addr = server.addr();

    let (head, echoed, cause) = tokio::task::spawn_blocking(move || {
        let mut peer = common::start_upgrade(addr, "/ws");
        let head = common::read_until_double_crlf(&mut peer);
        // One well-formed exchange first: the bridge owns a live transport
        // before the frame that breaks it arrives.
        common::write_ws_text_frame(&mut peer, "live");
        let echoed = common::read_ws_text_frame(&mut peer);
        common::write_unmasked_text_frame(&mut peer, "unmasked");
        // The socket is still held open here, so the cause the bridge reports
        // is the refused frame and not a peer that went away.
        let cause = causes
            .recv_timeout(BOUND)
            .expect("the WebSocket bridge never reported a terminal cause");
        (head, echoed, cause)
    })
    .await
    .expect("the WebSocket peer settled");

    assert!(
        head.starts_with("HTTP/1.1 101"),
        "{label}: the handshake commits before the bridge owns the transport: {head}",
    );
    assert_eq!(
        echoed.as_ref(),
        "live",
        "{label}: the bridge was pumping this transport before the refused frame",
    );
    assert_eq!(
        cause,
        camber::http::WsCloseCause::PeerDisconnected,
        "{label}: the bridge fixed the terminal for a frame it refused after 101",
    );
    assert!(
        common::drain(&journal).is_empty(),
        "{label}: a direction that fails after 101 is the bridge's terminal, not a mapped refusal",
    );
    server
        .shutdown_bounded(BOUND)
        .expect("the WebSocket owner fixture tore down");
}

// ---------------------------------------------------------------------------
// 14.T2
// ---------------------------------------------------------------------------

/// The authority the matrix child is registered under.
const MATRIX_HOST: &str = "matrix.test";

/// A second authority, whose own child is what proves selection happened once.
const OTHER_HOST: &str = "other.test";

/// The header one admitted request carries, and the value the chain admits on.
const CREDENTIAL_HEADER: &str = "Authorization";
const CREDENTIAL: &str = "matrix-credential";

/// The metadata a passing chain states, which every real head must carry.
const MATRIX_PROJECTED: &str = "X-Camber-Projected";
const MATRIX_PROJECTED_VALUE: &str = "applied";
const MATRIX_ORIGIN_HEADER: &str = "Access-Control-Allow-Origin";
const MATRIX_ORIGIN: &str = "https://matrix.test";

/// The representation a passing chain states, which every protocol that owns
/// one of its own must override.
const MATRIX_TYPE: &str = "text/x-projected";

/// The representation the proxied upstream states for its own head.
const MATRIX_UPSTREAM_TYPE: &str = "text/x-upstream";

/// The answer a chain that never reaches its terminal gives.
const MATRIX_REFUSED: u16 = 401;

/// The paths the matrix child serves each class under.
const MATRIX_HTTP: &str = "/http";
const MATRIX_STREAM: &str = "/stream";
const MATRIX_SSE: &str = "/sse";
const MATRIX_BUFFERED: &str = "/buffered/echo";
const MATRIX_STREAMED: &str = "/streamed/echo";
const MATRIX_DEAD: &str = "/dead/echo";
/// The route the second authority's own child answers, with no chain over it.
const MATRIX_OPEN: &str = "/open";
#[cfg(feature = "ws")]
const MATRIX_WS: &str = "/ws";
#[cfg(feature = "ws")]
const MATRIX_WS_PROXY: &str = "/wsproxy/ws";

/// A backend nothing listens on, for the pre-commit refusal row.
const DEAD_BACKEND: &str = "http://127.0.0.1:1";

/// What the matrix fixture's own producers, upstream, and service recorded.
///
/// One counter per claim rather than one shared total: a row that says nothing
/// specialized ran is only saying so because the class it drove has a counter of
/// its own, and a row that says a producer saw its peer leave needs that
/// distinct from a producer that merely started.
#[derive(Default)]
struct MatrixWork {
    /// Producers, upstream routes, and services entered.
    entered: std::sync::atomic::AtomicUsize,
    /// Event producers that stopped because their peer had gone.
    ended: std::sync::atomic::AtomicUsize,
    /// Upgrade owners that returned, however their session ended.
    ///
    /// Counted apart from the producers above because the two rows that read
    /// these counters make different claims about different owners, and every
    /// one of them is monotonic across the whole matrix: a shared count already
    /// driven past a threshold by an earlier row is a predicate that cannot
    /// fail. Each row baselines the counter it reads, and reads the one whose
    /// owner it is actually asking about.
    upgrades_ended: std::sync::atomic::AtomicUsize,
}

impl MatrixWork {
    fn entered(&self) -> usize {
        self.entered.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn ended(&self) -> usize {
        self.ended.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn upgrades_ended(&self) -> usize {
        self.upgrades_ended
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn enter(&self) {
        self.entered
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn end(&self) {
        self.ended.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn end_upgrade(&self) {
        self.upgrades_ended
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }
}

/// The whole matrix service: a host-routed server, its upstream, and the one
/// class no host table can carry.
///
/// gRPC is served beside the host-routed classes rather than under them because
/// `ServerDispatch::Host` resolves no gRPC router: a host table has no gRPC
/// class today. It runs the identical chain, so the row it answers is still the
/// same chain's, at the boundary tonic owns.
struct MatrixFixture {
    server: http_support::ObservedServer,
    controller: Arc<LifecycleController>,
    grpc: http_support::ObservedServer,
    upstream: http_support::ObservedServer,
    journal: common::Journal,
    work: Arc<MatrixWork>,
}

impl MatrixFixture {
    fn serve() -> Self {
        Self::serve_under(matrix_policy)
    }

    /// The same matrix, admitting one transport at a time.
    ///
    /// The permit row's own fixture: what it reads is a peer parked on the one
    /// permit a live transport holds, which no unbounded listener can show.
    fn serve_limited() -> Self {
        Self::serve_under(limited_matrix_policy)
    }

    /// Serve every matrix owner under the bounds `policy` names.
    fn serve_under(policy: fn() -> ServerPolicy) -> Self {
        let work = Arc::new(MatrixWork::default());
        let journal = common::journal();
        let upstream = http_support::reserve_observed().serve(matrix_upstream(&work));
        let backend = format!("http://{}", upstream.addr());

        let mut hosts = camber::http::HostRouter::new();
        hosts.add(MATRIX_HOST, matrix_child(&work, &journal, &backend));
        hosts.add(OTHER_HOST, other_child());
        let port = http_support::reserve_observed();
        let controller = port.controller();
        // Served through the builder rather than through the host helper: the
        // shutdown row needs the aggregate deadline this matrix names, and the
        // helper takes the server's own default.
        let server = port.serve_hosts_with_policy(hosts, policy());

        Self {
            server,
            controller,
            grpc: http_support::reserve_observed()
                .serve_with_policy(matrix_grpc_router(&work, &journal), policy()),
            upstream,
            journal,
            work,
        }
    }

    /// Stop every owner this fixture serves, in bounded order.
    ///
    /// The permit row holds nothing open past its own assertions, so each of
    /// the three ends gracefully; a row that left a transport behind reports it
    /// here rather than in whichever row ran next.
    fn tear_down(self, label: &str) {
        for (owner, name) in [
            (self.server, "host-routed"),
            (self.grpc, "grpc"),
            (self.upstream, "upstream"),
        ] {
            owner.shutdown_bounded(BOUND).unwrap_or_else(|error| {
                panic!("{label}: the {name} owner did not end gracefully: {error}")
            });
        }
    }

    fn addr(&self) -> std::net::SocketAddr {
        self.server.addr()
    }

    fn grpc_addr(&self) -> std::net::SocketAddr {
        self.grpc.addr()
    }

    /// Everything the matrix child's mapper was handed, taken once.
    fn mapped(&self) -> Box<[common::Observed]> {
        common::drain(&self.journal)
    }
}

/// The aggregate deadline every matrix server names.
///
/// Short on purpose: the shutdown row holds one upgrade open, and what it reads
/// is that the service ends on its own deadline rather than waiting on a peer
/// that never leaves.
const FORCED_AFTER: Duration = Duration::from_millis(500);

/// The bounds every matrix server serves under.
fn matrix_policy() -> ServerPolicy {
    ServerPolicy::default()
        .shutdown_timeout(FORCED_AFTER)
        .expect("a finite aggregate deadline")
}

/// How many transports a limited matrix server admits at once.
const MATRIX_PERMITS: usize = 1;

/// The bounds the permit row's matrix serves under.
///
/// One transport at a time, and a pre-head boundary no holder reaches: the row
/// keeps each protocol's transport open with no further request on it while a
/// second peer waits, and a short header boundary would end the holder rather
/// than the row.
fn limited_matrix_policy() -> ServerPolicy {
    matrix_policy()
        .connection_limit(MATRIX_PERMITS)
        .expect("one transport at a time")
        .header_timeout(UNREACHED)
        .expect("a pre-head boundary no permit row reaches")
}

/// The upstream both proxy classes and the proxied upgrade forward to.
fn matrix_upstream(work: &Arc<MatrixWork>) -> Router {
    let mut upstream = Router::new();
    let entered = Arc::clone(work);
    upstream.get("/echo", move |_req: &camber::http::Request| {
        entered.enter();
        async {
            camber::http::Response::text(200, "upstream")
                .map(|resp| resp.with_content_type(MATRIX_UPSTREAM_TYPE))
        }
    });
    register_matrix_upstream_upgrade(&mut upstream, work);
    upstream
}

/// The upgrade the proxied WebSocket class forwards to.
#[cfg(feature = "ws")]
fn register_matrix_upstream_upgrade(upstream: &mut Router, work: &Arc<MatrixWork>) {
    let entered = Arc::clone(work);
    upstream.ws(
        "/ws",
        move |_req: &camber::http::Request, conn: camber::http::WsConn| {
            entered.enter();
            // Held until the peer or the bridge ends it, so the session is live
            // while the shutdown row runs.
            let ended = held_session(conn);
            entered.end_upgrade();
            ended
        },
    );
}

#[cfg(not(feature = "ws"))]
fn register_matrix_upstream_upgrade(_upstream: &mut Router, _work: &Arc<MatrixWork>) {}

/// How many payload bytes the matrix admits on any one request.
///
/// Far above anything this matrix drives. The policy below exists to hand out a
/// permit, not to bound a body.
const MATRIX_BODY_CEILING: usize = 64 * 1024;

/// Hand every admitted request a permit of its own.
///
/// Without one there is nothing for a terminal to release, so the release count
/// 15.T3 reads would stand at zero however production behaved. The permit value
/// itself is inert: what is under observation is the single owner Camber wraps
/// it in and the one drop that owner reaches on every terminal path —
/// completion, refusal, disconnect, and cancellation alike.
fn permitting_body_admission(router: Router) -> Router {
    router.body_admission(|_context: &BodyAdmissionContext<'_>| {
        Ok(BodyAdmission::with_permit(MATRIX_BODY_CEILING, ()))
    })
}

/// The child the matrix authority selects: one chain over every class.
fn matrix_child(work: &Arc<MatrixWork>, journal: &common::Journal, backend: &str) -> Router {
    let mut child = Router::new();
    gate_on_credential(&mut child);
    register_matrix_classes(&mut child, work, backend);
    permitting_body_admission(
        child.rejection_mapper(common::recording_mapper(journal, "matrix-child")),
    )
}

/// The second authority's child: a route, and no chain at all.
fn other_child() -> Router {
    let mut child = Router::new();
    child.get(MATRIX_OPEN, |_req: &camber::http::Request| async {
        camber::http::Response::text(200, OTHER_HOST)
    });
    child
}

/// The single-router service carrying the same chain over the gRPC class.
fn matrix_grpc_router(work: &Arc<MatrixWork>, journal: &common::Journal) -> Router {
    let mut router = Router::new();
    gate_on_credential(&mut router);
    // The route the peer parked on this listener's permit is answered by. It
    // records no work: what the permit row reads is the transport a gRPC call
    // held, and this peer exists only to observe when that permit came back.
    router.get(MATRIX_HTTP, |_req: &camber::http::Request| async {
        camber::http::Response::text(200, "grpc host")
    });
    let (head_dropped, _dropped) = tokio::sync::mpsc::unbounded_channel();
    router.grpc(
        GrpcRouter::new().add_service(streamer_server::StreamerServer::new(MatrixService {
            work: Arc::clone(work),
            inner: EchoService {
                plan: EchoPlan::Echo,
                log: Arc::new(EchoLog::default()),
                head_dropped,
            },
        })),
    );
    permitting_body_admission(
        router.rejection_mapper(common::recording_mapper(journal, "matrix-grpc")),
    )
}

/// The fixture service, counted as entered by the matrix's own work record.
struct MatrixService {
    work: Arc<MatrixWork>,
    inner: EchoService,
}

#[tonic::async_trait]
impl streamer_server::Streamer for MatrixService {
    type EchoStream = HeadWitness;

    async fn echo(
        &self,
        request: tonic::Request<tonic::Streaming<HelloRequest>>,
    ) -> Result<tonic::Response<Self::EchoStream>, tonic::Status> {
        self.work.enter();
        self.inner.echo(request).await
    }
}

/// Register the one frame every matrix class runs under.
///
/// A request without the credential is refused before the terminal, the way
/// authentication is. One that carries it reaches the terminal and has the
/// chain's own response metadata stated over whatever it answered with.
fn gate_on_credential(router: &mut Router) {
    router.use_middleware(|req: &camber::http::Request, next: camber::http::Next| {
        let entered = (req.header(CREDENTIAL_HEADER) == Some(CREDENTIAL)).then(|| next.call(req));
        async move {
            match entered {
                None => camber::http::Response::text(MATRIX_REFUSED, "unauthenticated"),
                Some(entered) => Ok(entered
                    .await
                    .with_header(MATRIX_PROJECTED, MATRIX_PROJECTED_VALUE)
                    .with_header(MATRIX_ORIGIN_HEADER, MATRIX_ORIGIN)
                    .with_content_type(MATRIX_TYPE)),
            }
        }
    });
}

/// Register every class the matrix child serves.
fn register_matrix_classes(child: &mut Router, work: &Arc<MatrixWork>, backend: &str) {
    let entered = Arc::clone(work);
    child.get(MATRIX_HTTP, move |_req: &camber::http::Request| {
        entered.enter();
        async { camber::http::Response::text(200, "buffered") }
    });
    register_matrix_stream(child, work);
    register_matrix_sse(child, work);
    register_matrix_upgrade(child, work, backend);
    child.proxy("/buffered", backend);
    child.proxy_stream("/streamed", backend);
    child.proxy_stream("/dead", DEAD_BACKEND);
}

/// The generic streaming class, which states a representation of its own.
fn register_matrix_stream(child: &mut Router, work: &Arc<MatrixWork>) {
    let entered = Arc::clone(work);
    child.get_stream(MATRIX_STREAM, move |_req: &camber::http::Request| {
        let entered = Arc::clone(&entered);
        Box::pin(async move {
            entered.enter();
            let (streamed, sender) = camber::http::StreamResponse::new(200);
            tokio::spawn(async move {
                let _sent = sender.send("streamed").await;
            });
            streamed.with_header("Content-Type", "text/x-stream")
        })
            as std::pin::Pin<
                Box<dyn std::future::Future<Output = camber::http::StreamResponse> + Send>,
            >
    });
}

/// The SSE class, whose producer reports the peer that went away.
fn register_matrix_sse(child: &mut Router, work: &Arc<MatrixWork>) {
    let work = Arc::clone(work);
    child.get_sse(
        MATRIX_SSE,
        move |_req: &camber::http::Request, writer: &mut camber::http::SseWriter| {
            work.enter();
            // Until the peer is gone: the send that fails is this producer's own
            // report that its response ended, which is what the disconnect row
            // reads rather than inferring from a closed socket.
            while writer.event("tick", "matrix").is_ok() {
                std::thread::sleep(Duration::from_millis(25));
            }
            work.end();
            Ok(())
        },
    );
}

/// The direct upgrade class, and the proxy prefix a proxied one travels over.
#[cfg(feature = "ws")]
fn register_matrix_upgrade(child: &mut Router, work: &Arc<MatrixWork>, backend: &str) {
    let work = Arc::clone(work);
    child.ws(
        MATRIX_WS,
        move |_req: &camber::http::Request, conn: camber::http::WsConn| {
            work.enter();
            let ended = held_session(conn);
            work.end_upgrade();
            ended
        },
    );
    child.proxy("/wsproxy", backend);
}

/// Read one upgrade until its session ends, and report how it ended.
///
/// The owner's return is recorded by its caller whatever this hands back: an
/// upgrade the peer closed and one the service ended under it are both this
/// owner finishing, and a count that only rose on the graceful spelling would
/// leave the shutdown row unable to say the owner returned at all.
#[cfg(feature = "ws")]
fn held_session(conn: camber::http::WsConn) -> Result<(), camber::RuntimeError> {
    let (_sender, mut receiver) = conn.split();
    loop {
        match receiver.recv() {
            Ok(camber::http::WsReceive::Message(_)) => continue,
            Ok(_) => return Ok(()),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(not(feature = "ws"))]
fn register_matrix_upgrade(_child: &mut Router, _work: &Arc<MatrixWork>, _backend: &str) {}

/// Every class answered over ordinary HTTP framing, and the label it reports
/// under.
const MATRIX_HTTP_CLASSES: [(&str, &str); 5] = [
    ("ordinary http", MATRIX_HTTP),
    ("generic stream", MATRIX_STREAM),
    ("sse", MATRIX_SSE),
    ("buffered proxy", MATRIX_BUFFERED),
    ("streaming proxy", MATRIX_STREAMED),
];

/// The headers a matrix request carries, with or without its credential.
fn matrix_headers(credentialed: bool) -> Box<[(&'static str, &'static str)]> {
    match credentialed {
        true => Box::new([("Connection", "close"), (CREDENTIAL_HEADER, CREDENTIAL)]),
        false => Box::new([("Connection", "close")]),
    }
}

/// Drive one matrix class over ordinary HTTP framing.
async fn matrix_exchange(
    addr: std::net::SocketAddr,
    path: &'static str,
    credentialed: bool,
) -> http_support::HttpResponse {
    tokio::task::spawn_blocking(move || {
        http_support::send_to_host_with(
            addr,
            "GET",
            path,
            MATRIX_HOST,
            &matrix_headers(credentialed),
        )
    })
    .await
    .expect("the matrix peer settled")
}

/// Whether one raw head carries `name: value`, however the wire cased it.
///
/// A handshake answer is read as text rather than as parsed pairs, and HTTP/2
/// lowercases every name it writes: a case-sensitive search would report a
/// header the peer was given as one it was not.
fn head_carries(head: &str, name: &str, value: &str) -> bool {
    head.to_ascii_lowercase().contains(&format!(
        "{}: {}",
        name.to_ascii_lowercase(),
        value.to_ascii_lowercase()
    ))
}

/// Assert one answered head carries the metadata a passing chain stated.
fn assert_matrix_projected(head: &http_support::HttpResponse, label: &str) {
    assert_eq!(
        head.header(MATRIX_PROJECTED),
        Some(MATRIX_PROJECTED_VALUE),
        "{label}: the chain's own metadata never reached the real head",
    );
    assert_eq!(
        head.header(MATRIX_ORIGIN_HEADER),
        Some(MATRIX_ORIGIN),
        "{label}: the chain's cross-origin metadata never reached the real head",
    );
}

/// 14.T2
///
/// One host-routed service, every supported class, and one chain over all of
/// them. Host selection runs once and the selected child's policy governs; a
/// chain that refuses answers before any upstream, producer, upgrade, or
/// service begins; a chain that passes has its metadata carried into every real
/// head under the protocol's own corrections; a pre-commit refusal is mapped by
/// the child that owns it; a peer that leaves ends the producer it was reading;
/// and shutdown releases every protocol owner within its own deadline.
#[test]
fn cross_protocol_policy_matrix_reaches_every_supported_head() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                let fixture = MatrixFixture::serve();
                assert_host_selection_precedes_child_policy(&fixture).await;
                assert_short_circuit_precedes_every_protocol(&fixture).await;
                assert_success_metadata_reaches_every_head(&fixture).await;
                assert_the_same_chain_decides_over_tls(&fixture.work).await;
                assert_precommit_refusal_is_mapped_by_its_owner(&fixture).await;
                assert_disconnect_ends_the_producer_it_was_read_by(&fixture).await;
                assert_permit_returns_at_each_protocol_boundary().await;
                assert_shutdown_releases_every_protocol_owner(fixture).await;
            });
        })
        .expect("the cross-protocol matrix ran to completion");
}

/// Host selection happens once, and the child it selected owns the policy.
async fn assert_host_selection_precedes_child_policy(fixture: &MatrixFixture) {
    let label = "host selection";
    let addr = fixture.addr();
    let open = tokio::task::spawn_blocking(move || {
        http_support::send_to_host(addr, "GET", MATRIX_OPEN, OTHER_HOST)
    })
    .await
    .expect("the second authority's peer settled");
    assert_eq!(
        open.status, 200,
        "{label}: the second authority's own child answered under its own policy",
    );
    assert_eq!(
        open.header(MATRIX_PROJECTED),
        None,
        "{label}: a child that registered no chain projected nothing",
    );

    let unclaimed = tokio::task::spawn_blocking(move || {
        http_support::send_to_host(addr, "GET", MATRIX_HTTP, "unclaimed.test")
    })
    .await
    .expect("the unclaimed authority's peer settled");
    assert_eq!(
        unclaimed.status, 404,
        "{label}: an authority no child claims reaches no child chain",
    );
    assert!(
        fixture.mapped().is_empty(),
        "{label}: no child mapper answers for an authority no child claims",
    );
}

/// A chain that refuses before its terminal answers every class, and nothing
/// specialized runs behind it.
async fn assert_short_circuit_precedes_every_protocol(fixture: &MatrixFixture) {
    let label = "short circuit";
    for (class, path) in MATRIX_HTTP_CLASSES {
        let refused = matrix_exchange(fixture.addr(), path, false).await;
        assert_eq!(
            refused.status, MATRIX_REFUSED,
            "{label}: {class} is answered by the chain, not by its producer",
        );
        assert_eq!(
            refused.header(MATRIX_PROJECTED),
            None,
            "{label}: {class} projected metadata from a chain that never passed",
        );
    }
    assert_short_circuited_upgrades(fixture).await;
    assert_short_circuited_grpc(fixture).await;
    assert_eq!(
        fixture.work.entered(),
        0,
        "{label}: a producer, an upstream, an upgrade, or a service ran behind a refused chain",
    );
}

/// Both upgrade classes are refused before any `101`.
#[cfg(feature = "ws")]
async fn assert_short_circuited_upgrades(fixture: &MatrixFixture) {
    for (class, path) in [
        ("direct upgrade", MATRIX_WS),
        ("proxied upgrade", MATRIX_WS_PROXY),
    ] {
        let head = matrix_handshake(fixture.addr(), path, false).await;
        assert!(
            head.starts_with(&format!("HTTP/1.1 {MATRIX_REFUSED}")),
            "short circuit: {class} was upgraded by a chain that refused it: {head}",
        );
    }
}

#[cfg(not(feature = "ws"))]
async fn assert_short_circuited_upgrades(_fixture: &MatrixFixture) {}

/// The gRPC class is refused before tonic is handed anything.
async fn assert_short_circuited_grpc(fixture: &MatrixFixture) {
    let refused = matrix_rpc(fixture.grpc_addr(), false).await;
    assert_eq!(
        refused.status, MATRIX_REFUSED,
        "short circuit: gRPC was handed to tonic by a chain that refused it",
    );
}

/// A passing chain's metadata reaches every real head, and each protocol's own
/// corrections still win the names it owns.
async fn assert_success_metadata_reaches_every_head(fixture: &MatrixFixture) {
    let label = "success metadata";
    let expected_types = [
        ("ordinary http", MATRIX_TYPE),
        ("generic stream", "text/x-stream"),
        ("sse", "text/event-stream"),
        ("buffered proxy", MATRIX_TYPE),
        ("streaming proxy", MATRIX_UPSTREAM_TYPE),
    ];
    for ((class, path), (_, representation)) in MATRIX_HTTP_CLASSES.iter().zip(expected_types) {
        // The SSE producer runs until its peer goes away, so this class is read
        // by the disconnect row rather than by a whole-body exchange here.
        if *class == "sse" {
            continue;
        }
        let answered = matrix_exchange(fixture.addr(), path, true).await;
        assert_eq!(answered.status, 200, "{label}: {class} wire status");
        assert_matrix_projected(&answered, class);
        assert_eq!(
            answered.header("Content-Type"),
            Some(representation),
            "{label}: {class} answered under the wrong owner's representation",
        );
    }
    assert_upgrade_heads_projected(fixture).await;
    assert_grpc_head_projected(fixture).await;
}

/// Both upgrade classes commit a `101` carrying the chain's metadata beside the
/// handshake's own corrections.
#[cfg(feature = "ws")]
async fn assert_upgrade_heads_projected(fixture: &MatrixFixture) {
    for (class, path) in [
        ("direct upgrade", MATRIX_WS),
        ("proxied upgrade", MATRIX_WS_PROXY),
    ] {
        let head = matrix_handshake(fixture.addr(), path, true).await;
        assert!(
            head.starts_with("HTTP/1.1 101"),
            "success metadata: {class} never committed its upgrade: {head}",
        );
        assert!(
            head_carries(&head, MATRIX_PROJECTED, MATRIX_PROJECTED_VALUE),
            "success metadata: {class} lost the chain's metadata: {head}",
        );
        assert!(
            head_carries(&head, MATRIX_ORIGIN_HEADER, MATRIX_ORIGIN),
            "success metadata: {class} lost the chain's cross-origin metadata: {head}",
        );
        assert!(
            head.to_ascii_lowercase().contains("sec-websocket-accept:"),
            "success metadata: {class} lost the handshake's own correction: {head}",
        );
    }
}

#[cfg(not(feature = "ws"))]
async fn assert_upgrade_heads_projected(_fixture: &MatrixFixture) {}

/// Tonic's committed head carries the chain's metadata under its own type.
async fn assert_grpc_head_projected(fixture: &MatrixFixture) {
    let label = "success metadata";
    let answered = matrix_rpc(fixture.grpc_addr(), true).await;
    assert_eq!(answered.status, 200, "{label}: grpc wire status");
    assert_matrix_projected(&answered, "grpc");
    assert_eq!(
        answered.header("content-type"),
        Some("application/grpc"),
        "{label}: grpc answered under the wrong owner's representation",
    );
}

// ---------------------------------------------------------------------------
// 14.T2: the same chain, past a real handshake
// ---------------------------------------------------------------------------

/// The name the matrix's TLS peer verifies the fixture certificate under.
///
/// Distinct from the authority the host table selects on: the certificate names
/// what the transport is, and `Host` names which child answers over it.
const TLS_SERVER_NAME: &str = "localhost";

/// The chain's decisions hold over a real TLS transport, not only a plaintext
/// one.
///
/// The gated class is what makes this worth serving twice: its head is the
/// producer's, committed after the chain has already passed, and TLS is the one
/// transport where an answer is written through another owner entirely.
async fn assert_the_same_chain_decides_over_tls(work: &Arc<MatrixWork>) {
    let label = "tls";
    let (server_config, connector) = common::self_signed_server_and_connector();
    let journal = common::journal();
    let upstream = http_support::reserve_observed().serve(matrix_upstream(work));
    let backend = format!("http://{}", upstream.addr());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the TLS matrix listener");
    let addr = listener.local_addr().expect("the TLS matrix address");
    let mut hosts = camber::http::HostRouter::new();
    hosts.add(MATRIX_HOST, matrix_child(work, &journal, &backend));
    let handle = camber::http::server_hosts(hosts)
        .policy(matrix_policy())
        .tls(server_config)
        .serve_background(listener)
        .expect("owned TLS serving requires a Tokio runtime");
    let server = http_support::ReadyServer::adopt(addr, handle);

    let refused = tls_exchange(&connector, addr, MATRIX_STREAM, false).await;
    assert!(
        refused.starts_with(&format!("HTTP/1.1 {MATRIX_REFUSED}")),
        "{label}: a chain that refuses is answered past the handshake too: {refused}",
    );
    assert!(
        !head_carries(&refused, MATRIX_PROJECTED, MATRIX_PROJECTED_VALUE),
        "{label}: a refused chain projected its metadata over TLS: {refused}",
    );

    let answered = tls_exchange(&connector, addr, MATRIX_STREAM, true).await;
    assert!(
        answered.starts_with("HTTP/1.1 200"),
        "{label}: the generic stream never committed its head over TLS: {answered}",
    );
    assert!(
        head_carries(&answered, MATRIX_PROJECTED, MATRIX_PROJECTED_VALUE),
        "{label}: the head committed over TLS lost the chain's metadata: {answered}",
    );
    assert!(
        head_carries(&answered, MATRIX_ORIGIN_HEADER, MATRIX_ORIGIN),
        "{label}: the head committed over TLS lost the chain's cross-origin metadata: {answered}",
    );
    assert!(
        head_carries(&answered, "Content-Type", "text/x-stream"),
        "{label}: the producer's own representation was displaced over TLS: {answered}",
    );

    server
        .shutdown_bounded(BOUND)
        .expect("the TLS matrix fixture tore down");
    upstream
        .shutdown_bounded(BOUND)
        .expect("the TLS matrix upstream tore down");
}

/// Drive one exchange past a real handshake and read everything it answered.
///
/// The transport is closed by the service rather than by a length the peer
/// counts: a streamed answer is framed by its producer, and reading to close is
/// what a peer with no framing of its own can do.
async fn tls_exchange(
    connector: &tokio_rustls::TlsConnector,
    addr: std::net::SocketAddr,
    path: &str,
    credentialed: bool,
) -> Box<str> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("the TLS peer connected");
    let name = rustls::pki_types::ServerName::try_from(TLS_SERVER_NAME).expect("a server name");
    let mut tls = connector
        .connect(name, stream)
        .await
        .expect("the TLS handshake completed");
    let credential = match credentialed {
        true => format!("{CREDENTIAL_HEADER}: {CREDENTIAL}\r\n"),
        false => String::new(),
    };
    tls.write_all(
        format!(
            "GET {path} HTTP/1.1\r\nHost: {MATRIX_HOST}\r\n{credential}Connection: close\r\n\r\n"
        )
        .as_bytes(),
    )
    .await
    .expect("the TLS request was sent");
    tls.flush().await.expect("the TLS request was flushed");

    let mut answered = Vec::new();
    tokio::time::timeout(BOUND, tls.read_to_end(&mut answered))
        .await
        .expect("the TLS answer arrived within the bound")
        .expect("the TLS answer was readable");
    String::from_utf8_lossy(&answered).into_owned().into()
}

/// A refusal raised before commitment is mapped by the child that owns it, and
/// carries the chain's metadata like any other pre-commit answer.
async fn assert_precommit_refusal_is_mapped_by_its_owner(fixture: &MatrixFixture) {
    let label = "pre-commit refusal";
    let _drained = fixture.mapped();
    let refused = matrix_exchange(fixture.addr(), MATRIX_DEAD, true).await;
    assert_eq!(
        refused.status, 502,
        "{label}: an unreachable upstream is refused before commitment",
    );
    assert_matrix_projected(&refused, label);
    let mapped = fixture.mapped();
    assert_eq!(
        mapped.len(),
        1,
        "{label}: the child that owns this boundary maps it exactly once: {mapped:?}",
    );
    assert_eq!(
        mapped[0].protocol,
        Some(RejectionProtocol::Proxy),
        "{label}: the refusal keeps the class the proxied route established",
    );
}

/// A peer that goes away ends the producer that was being read for it.
async fn assert_disconnect_ends_the_producer_it_was_read_by(fixture: &MatrixFixture) {
    let label = "disconnect";
    let addr = fixture.addr();
    let before = fixture.controller.transfers_observed().download.releases;
    // Baselined for the same reason the release count beside it is: every one
    // of these counters is monotonic across the whole matrix, so the claim is
    // what this row's own peer caused, not what the count already stood at.
    let ended_before = fixture.work.ended();
    let head = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the SSE peer connected");
        let head = format!(
            "GET {MATRIX_SSE} HTTP/1.1\r\nHost: {MATRIX_HOST}\r\n{CREDENTIAL_HEADER}: \
             {CREDENTIAL}\r\n\r\n"
        );
        std::io::Write::write_all(&mut peer, head.as_bytes()).expect("the SSE request was sent");
        let answered = common::read_until_double_crlf(&mut peer);
        // Dropped here, inside the blocking owner: the peer is gone from this
        // point, and everything after it is what the service did about that.
        drop(peer);
        answered
    })
    .await
    .expect("the SSE peer settled");

    assert!(
        head.starts_with("HTTP/1.1 200"),
        "{label}: the event stream never committed its head: {head}",
    );
    assert!(
        head_carries(&head, MATRIX_PROJECTED, MATRIX_PROJECTED_VALUE),
        "{label}: the committed event-stream head lost the chain's metadata: {head}",
    );
    assert!(
        head_carries(&head, "Content-Type", "text/event-stream"),
        "{label}: the event stream answered under the wrong representation: {head}",
    );

    let ended = http_support::poll_until(BOUND, || fixture.work.ended() > ended_before);
    assert!(
        ended,
        "{label}: the event producer outlived the peer it was being read by",
    );
    let released = http_support::poll_until(BOUND, || {
        fixture.controller.transfers_observed().download.releases > before
    });
    assert!(
        released,
        "{label}: the download owner was never released for a peer that left",
    );
}

// ---------------------------------------------------------------------------
// 14.T2: the permit each protocol's transport holds
// ---------------------------------------------------------------------------

/// The transport one class holds while a second peer waits for its permit.
#[derive(Clone, Copy, Debug)]
enum Held {
    /// A committed head whose transport the peer keeps open.
    Exchange(&'static str),
    /// A committed upgrade.
    #[cfg(feature = "ws")]
    Upgrade(&'static str),
}

impl Held {
    /// What the head this transport committed begins with.
    fn committed(self) -> &'static str {
        match self {
            Self::Exchange(_) => "HTTP/1.1 200",
            #[cfg(feature = "ws")]
            Self::Upgrade(_) => "HTTP/1.1 101",
        }
    }
}

/// Every class whose own transport is what holds the service's one permit.
///
/// The event stream and both upgrades are the classes that keep a transport
/// alive past their head; the rest hold one open after theirs. Each is here
/// because the boundary that returns the permit is a different owner's — a
/// producer's, a bridge's, a forwarded leg's — and a merge of them into one row
/// could not name which one failed.
fn permit_classes() -> Box<[(&'static str, Held)]> {
    [
        ("ordinary http", Held::Exchange(MATRIX_HTTP)),
        ("generic stream", Held::Exchange(MATRIX_STREAM)),
        ("sse", Held::Exchange(MATRIX_SSE)),
        ("buffered proxy", Held::Exchange(MATRIX_BUFFERED)),
        ("streaming proxy", Held::Exchange(MATRIX_STREAMED)),
    ]
    .into_iter()
    .chain(permit_upgrades())
    .collect()
}

#[cfg(feature = "ws")]
fn permit_upgrades() -> Box<[(&'static str, Held)]> {
    Box::new([
        ("direct upgrade", Held::Upgrade(MATRIX_WS)),
        ("proxied upgrade", Held::Upgrade(MATRIX_WS_PROXY)),
    ])
}

#[cfg(not(feature = "ws"))]
fn permit_upgrades() -> Box<[(&'static str, Held)]> {
    Box::new([])
}

/// Every protocol holds the service's one connection permit for the whole of
/// its own transport, and returns it at that transport's own boundary.
async fn assert_permit_returns_at_each_protocol_boundary() {
    for (class, held) in permit_classes() {
        assert_permit_returns_after_the_transport(class, held).await;
    }
    assert_permit_returns_after_the_grpc_transport().await;
}

/// One class, on a fixture of its own that admits one transport at a time.
///
/// A fixture per class rather than one shared across them: the claim is what a
/// waiting peer was given while exactly one transport lived, and a transport
/// another row had not finished releasing would be a second answer to that
/// question.
async fn assert_permit_returns_after_the_transport(class: &'static str, held: Held) {
    let fixture = MatrixFixture::serve_limited();
    let addr = fixture.addr();
    let holder = hold_transport(addr, held, class).await;
    let parked = park_on_permit(&fixture.controller, addr, class).await;
    let parked = assert_still_waiting(parked, class).await;
    // The transport's own boundary: the peer that owned it is gone, so the
    // producer, bridge, or forwarded leg reading for it ends and the permit it
    // held goes back.
    drop(holder);
    assert_admitted_on_the_returned_permit(parked, class).await;
    fixture.tear_down(class);
}

/// The gRPC class, whose transport is an HTTP/2 connection tonic answered on.
async fn assert_permit_returns_after_the_grpc_transport() {
    let class = "grpc";
    let fixture = MatrixFixture::serve_limited();
    let addr = fixture.grpc_addr();
    let mut client = common::PersistentH2Client::connect(addr, BOUND).await;
    let answered = client
        .send_complete(
            "POST",
            ECHO_PATH,
            MATRIX_HOST,
            &grpc_headers()
                .into_iter()
                .chain(std::iter::once((CREDENTIAL_HEADER, CREDENTIAL)))
                .collect::<Box<[(&str, &str)]>>(),
            &grpc_message("permit"),
        )
        .await;
    assert_eq!(
        answered.status, 200,
        "{class}: the call that holds the permit was never answered",
    );

    // The gRPC class is served on a listener of its own, so the permit under
    // observation is that listener's rather than the host-routed one's.
    let parked = park_on_permit(fixture.grpc.controller(), addr, class).await;
    let parked = assert_still_waiting(parked, class).await;
    // tonic's answer did not end the transport; closing the connection does,
    // and that is the boundary the permit comes back at.
    client.close().await;
    assert_admitted_on_the_returned_permit(parked, class).await;
    fixture.tear_down(class);
}

/// Open one class's transport, read the head it committed, and hand the peer
/// back still open.
async fn hold_transport(
    addr: std::net::SocketAddr,
    held: Held,
    class: &'static str,
) -> std::net::TcpStream {
    tokio::task::spawn_blocking(move || {
        let mut peer = match held {
            Held::Exchange(path) => credentialed_peer(addr, path),
            #[cfg(feature = "ws")]
            Held::Upgrade(path) => common::start_upgrade_with(
                addr,
                &common::ws_upgrade_request_to_with(
                    MATRIX_HOST,
                    path,
                    &[(CREDENTIAL_HEADER, CREDENTIAL)],
                ),
            ),
        };
        let head = common::read_until_double_crlf(&mut peer);
        assert!(
            head.starts_with(held.committed()),
            "{class}: the transport that must hold the permit never committed its head: {head}",
        );
        peer
    })
    .await
    .expect("the permit-holding peer settled")
}

/// Connect a second peer and leave it parked on the permit the holder owns.
///
/// The production checkpoint is what makes this a wait rather than a guess: the
/// peer has reached the permit wait and been let go of the checkpoint, so the
/// silence that follows is the limit holding.
async fn park_on_permit(
    controller: &LifecycleController,
    addr: std::net::SocketAddr,
    class: &'static str,
) -> std::net::TcpStream {
    controller
        .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .unwrap_or_else(|error| panic!("{class}: arming the permit wait failed: {error}"));
    let parked = tokio::task::spawn_blocking(move || credentialed_peer(addr, MATRIX_HTTP))
        .await
        .expect("the waiting peer settled");
    tokio::time::timeout(
        BOUND,
        controller.wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending),
    )
    .await
    .unwrap_or_else(|_| panic!("{class}: the second peer never reached the permit wait"))
    .unwrap_or_else(|error| panic!("{class}: waiting for the permit wait failed: {error}"));
    controller
        .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .unwrap_or_else(|error| panic!("{class}: releasing the permit wait failed: {error}"));
    parked
}

/// Require the parked peer to still be given nothing, and hand it back.
async fn assert_still_waiting(
    parked: std::net::TcpStream,
    class: &'static str,
) -> std::net::TcpStream {
    tokio::task::spawn_blocking(move || {
        let mut parked = parked;
        parked
            .set_read_timeout(Some(STAGED_QUIET))
            .expect("arm the quiet observation");
        let mut answered = [0u8; 1];
        match std::io::Read::read(&mut parked, &mut answered) {
            Err(error) if http_support::is_deadline_expiry(&error) => parked,
            other => panic!(
                "{class}: a peer waiting on a permit another transport holds was answered: \
                 {other:?}",
            ),
        }
    })
    .await
    .expect("the waiting peer settled")
}

/// Require the parked peer to enter now that the permit has come back.
async fn assert_admitted_on_the_returned_permit(parked: std::net::TcpStream, class: &'static str) {
    let head = tokio::task::spawn_blocking(move || {
        let mut parked = parked;
        common::read_until_double_crlf(&mut parked)
    })
    .await
    .expect("the admitted peer settled");
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "{class}: the waiting peer never entered on the permit its transport returned: {head}",
    );
}

/// Connect one credentialed peer, send its request, and leave the transport
/// open.
///
/// Keep-alive framing on purpose: a permit is the transport's, not the
/// response's, so the peer that holds it is one that has its answer and has not
/// gone away.
fn credentialed_peer(addr: std::net::SocketAddr, path: &str) -> std::net::TcpStream {
    let mut peer = http_support::connect(addr).expect("the permit row's peer connected");
    let head = format!(
        "GET {path} HTTP/1.1\r\nHost: {MATRIX_HOST}\r\n{CREDENTIAL_HEADER}: {CREDENTIAL}\r\n\r\n"
    );
    std::io::Write::write_all(&mut peer, head.as_bytes())
        .expect("the permit row's request was sent");
    std::io::Write::flush(&mut peer).expect("the permit row's request was flushed");
    peer
}

/// Shutdown ends every protocol owner within the deadline the service names.
///
/// Both halves of the contract, in the order they can be read: a service with
/// nothing open ends gracefully, and one holding a live session ends on its own
/// aggregate deadline instead of waiting for a peer that never leaves. The
/// owner is released either way.
async fn assert_shutdown_releases_every_protocol_owner(fixture: MatrixFixture) {
    let label = "shutdown";
    let MatrixFixture {
        server,
        grpc,
        upstream,
        work,
        ..
    } = fixture;
    assert!(
        work.entered() > 0,
        "{label}: nothing had been admitted for shutdown to release",
    );
    grpc.shutdown_bounded(BOUND)
        .unwrap_or_else(|error| panic!("{label}: a quiet service did not end gracefully: {error}"));

    assert_forced_shutdown_ends_the_held_upgrade(server, &work).await;

    upstream
        .shutdown_bounded(BOUND)
        .unwrap_or_else(|error| panic!("{label}: the upstream did not end gracefully: {error}"));
}

/// The host-routed service ends on its own deadline with a session still open.
#[cfg(feature = "ws")]
async fn assert_forced_shutdown_ends_the_held_upgrade(
    server: http_support::ObservedServer,
    work: &Arc<MatrixWork>,
) {
    let label = "shutdown";
    // Taken before this row's own upgrade is committed: the projection row
    // already opened and dropped two of them, so what proves the service ended
    // this owner is the count rising past where that left it.
    let ended_before = work.upgrades_ended();
    let peer = held_upgrade(server.addr()).await;
    let forced = server.shutdown_bounded(BOUND);
    assert!(
        matches!(
            forced,
            Err(http_support::FixtureError::Runtime(
                camber::RuntimeError::Timeout
            ))
        ),
        "{label}: a service holding a live session ended on something other than its own \
         aggregate deadline: {forced:?}",
    );
    // Released after the teardown it was held across, by the one owner that has
    // it: what ended the bridge was the shutdown, not a peer that left first.
    drop(peer);
    let ended = http_support::poll_until(BOUND, || work.upgrades_ended() > ended_before);
    assert!(
        ended,
        "{label}: an upgrade owner outlived the service that admitted it",
    );
}

/// Without upgrades there is no session to hold, so the service ends gracefully.
#[cfg(not(feature = "ws"))]
async fn assert_forced_shutdown_ends_the_held_upgrade(
    server: http_support::ObservedServer,
    _work: &Arc<MatrixWork>,
) {
    server
        .shutdown_bounded(BOUND)
        .expect("shutdown: a quiet service did not end gracefully");
}

/// Commit one upgrade and hand its peer back, still open.
///
/// The socket travels to the caller rather than being forgotten here: the
/// shutdown row holds it across the teardown it is asserting about, and one
/// owner then drops it. A forgotten socket would leave the claim resting on a
/// descriptor nothing releases.
#[cfg(feature = "ws")]
async fn held_upgrade(addr: std::net::SocketAddr) -> std::net::TcpStream {
    let (peer, head) = tokio::task::spawn_blocking(move || {
        let mut peer = common::start_upgrade_with(
            addr,
            &common::ws_upgrade_request_to_with(
                MATRIX_HOST,
                MATRIX_WS,
                &[(CREDENTIAL_HEADER, CREDENTIAL)],
            ),
        );
        let head = common::read_until_double_crlf(&mut peer);
        (peer, head)
    })
    .await
    .expect("the held upgrade peer settled");
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "shutdown: the held upgrade never committed: {head}",
    );
    peer
}

/// Drive one matrix upgrade handshake and read the head it answered with.
#[cfg(feature = "ws")]
async fn matrix_handshake(
    addr: std::net::SocketAddr,
    path: &'static str,
    credentialed: bool,
) -> Box<str> {
    tokio::task::spawn_blocking(move || {
        let head = match credentialed {
            true => common::ws_upgrade_request_to_with(
                MATRIX_HOST,
                path,
                &[(CREDENTIAL_HEADER, CREDENTIAL)],
            ),
            false => common::ws_upgrade_request_to(MATRIX_HOST, path),
        };
        let mut peer = common::start_upgrade_with(addr, &head);
        common::read_until_double_crlf(&mut peer)
    })
    .await
    .expect("the matrix upgrade peer settled")
}

/// Drive one matrix gRPC call and read the head it answered with.
async fn matrix_rpc(addr: std::net::SocketAddr, credentialed: bool) -> http_support::HttpResponse {
    let headers: Box<[(&str, &str)]> = grpc_headers()
        .into_iter()
        .chain(credentialed.then_some((CREDENTIAL_HEADER, CREDENTIAL)))
        .collect();
    let mut client = common::PersistentH2Client::connect(addr, BOUND).await;
    let answered = client
        .send_complete(
            "POST",
            ECHO_PATH,
            MATRIX_HOST,
            &headers,
            &grpc_message("matrix"),
        )
        .await;
    client.close().await;
    answered
}

// ---------------------------------------------------------------------------
// 15.T3: one envelope, one chain, one account, per admitted row
// ---------------------------------------------------------------------------

/// The counter one completed operation is recorded under.
const COMPLETION_METRIC: &str = "http_requests_total";

/// The sentence one completed operation is recorded under.
const COMPLETION_EVENT: &str = "message=request completed";

/// The method this row scrapes the live endpoint with.
///
/// Deliberately not `GET`: a scrape is itself an admitted operation, recorded
/// under a label set of its own, and a scrape sharing one with a row under
/// measurement would be counted as that row's second account. No matrix class is
/// driven with `OPTIONS`, so this label set belongs to the scrape alone.
const SCRAPE_METHOD: &str = "OPTIONS";

/// How long between re-scrapes while a row's record is still owed.
const RECORDED_POLL: Duration = Duration::from_millis(20);

/// How many permit owners a chain that refused before its terminal releases.
///
/// One: route-aware body admission is resolved from the matched route before
/// the chain over it runs, so a request the chain then refuses is already
/// holding the permit that admission handed it — and every terminal path,
/// refusal included, releases it exactly once.
const SHORT_CIRCUIT_PERMITS: usize = 1;

/// How many permit owners a handshake Camber refused releases.
///
/// None: an upgrade is dispatched head-only, so no body plan resolves and no
/// permit is ever handed out to be returned.
const FAILED_UPGRADE_PERMITS: usize = 0;

/// The answer a handshake Camber cannot complete is refused with.
const REFUSED_HANDSHAKE: u16 = 400;

/// What one row's own listener published while that row ran.
///
/// Deltas rather than totals: the observations are cumulative per listener, and
/// the whole claim is per row. A row read against a total already driven past
/// its threshold by an earlier row is a predicate that cannot fail.
struct OwnedOnce {
    envelopes: usize,
    identities: usize,
    middleware: usize,
    staged: usize,
    recorded: usize,
    permits: usize,
}

impl OwnedOnce {
    /// What `controller` published since `baseline` was taken.
    fn since(baseline: &Baseline, controller: &LifecycleController) -> Self {
        let after = controller.operations_observed();
        let before = &baseline.observed;
        Self {
            envelopes: after.admitted - before.admitted,
            identities: after.distinct_identities - before.distinct_identities,
            middleware: after.middleware - before.middleware,
            staged: after.completions_staged - before.completions_staged,
            recorded: after.completions_recorded - before.completions_recorded,
            permits: controller.body_permit_owners_dropped() - baseline.permits,
        }
    }
}

/// Everything one row is measured against, taken before that row is driven.
///
/// The scrape belongs here rather than beside the row because it has to be
/// taken first and settled first: a scrape is an admitted operation of its own,
/// and its record is written where it ends — after its body already reached the
/// peer. A row baselined while that record was still owed would read the
/// scrape's account as its own.
struct Baseline {
    observed: camber::http::mock::OperationObservation,
    permits: usize,
    scraped: Box<[common::Sample]>,
}

impl Baseline {
    /// Take everything one row on `addr` is measured against.
    async fn take(addr: std::net::SocketAddr, controller: &LifecycleController) -> Self {
        let before = controller.operations_observed();
        let scraped = scrape_completions(addr).await;
        let settled = http_support::poll_until(BOUND, || {
            controller.operations_observed().completions_recorded > before.completions_recorded
        });
        assert!(
            settled,
            "baseline: the scrape's own account was never recorded, so no row after it \
             can be told apart from it",
        );
        Self {
            observed: controller.operations_observed(),
            permits: controller.body_permit_owners_dropped(),
            scraped,
        }
    }
}

/// Scrape one live listener's completion counter through the chain gating it.
///
/// The credential is carried because an internal route still runs the selected
/// child's middleware, and the authority is named because the host table selects
/// on it.
async fn scrape_completions(addr: std::net::SocketAddr) -> Box<[common::Sample]> {
    let scrape = tokio::task::spawn_blocking(move || {
        http_support::send_to_host_with(
            addr,
            SCRAPE_METHOD,
            "/metrics",
            MATRIX_HOST,
            &matrix_headers(true),
        )
    })
    .await
    .expect("the scrape peer settled");
    common::scraped_samples(&scrape, COMPLETION_METRIC)
}

/// Re-scrape `addr` until this row's own label set has moved, and report by how
/// far.
///
/// Bounded rather than read once: the record is written where the operation
/// ends, which for several of these classes is after the peer already has its
/// answer. `expected` is what the caller is waiting for rather than what it
/// accepts — the reading is handed back whole, so a row that moved further than
/// it declared fails at the caller instead of being waited past. Every
/// re-scrape is an `OPTIONS` operation, so none of them can move the label set
/// being waited on.
async fn moved_on_the_wire(
    addr: std::net::SocketAddr,
    before: &[common::Sample],
    labels: &[(&str, &str)],
    expected: u64,
) -> u64 {
    let deadline = std::time::Instant::now() + BOUND;
    loop {
        let moved = common::delta(before, &scrape_completions(addr).await, labels);
        match moved >= expected || std::time::Instant::now() >= deadline {
            true => return moved,
            false => tokio::time::sleep(RECORDED_POLL).await,
        }
    }
}

/// Wait until one row's account has been recorded and its permit returned.
///
/// The record is written where the operation ends, and the permit the request
/// held goes back when the owner holding it drops, which can be later still.
/// Both waits are bounded and their expiry is the failure, so a row that never
/// records or never releases fails here rather than reading a zero it cannot
/// explain.
///
/// `permits` is what this row is owed: a class that consumes a request body
/// holds one, and a head-only class — an upgrade, an event stream — is never
/// offered one to hold.
fn owned_once(
    baseline: &Baseline,
    controller: &LifecycleController,
    permits: usize,
    label: &str,
) -> OwnedOnce {
    let settled = http_support::poll_until(BOUND, || {
        let owned = OwnedOnce::since(baseline, controller);
        owned.recorded >= 1 && owned.permits >= permits
    });
    let owned = OwnedOnce::since(baseline, controller);
    assert!(
        settled,
        "{label}: no account was recorded and released within {BOUND:?}: staged {}, \
         recorded {}, permits {} of {permits}",
        owned.staged, owned.recorded, owned.permits,
    );
    owned
}

/// Assert one admitted row owned exactly one of each, and released what it held.
fn assert_owned_once(owned: &OwnedOnce, permits: usize, label: &str) {
    assert_eq!(owned.envelopes, 1, "{label}: envelopes minted");
    assert_eq!(owned.identities, 1, "{label}: distinct identities read");
    assert_eq!(owned.middleware, 1, "{label}: middleware executions");
    assert_eq!(
        owned.staged, 1,
        "{label}: accounts staged by an answering exit"
    );
    assert_eq!(
        owned.recorded, 1,
        "{label}: accounts recorded at a terminal"
    );
    assert_eq!(owned.permits, permits, "{label}: permit owners released");
}

/// Assert one row that never reached a Camber owner left nothing behind.
fn assert_owned_nothing(owned: &OwnedOnce, label: &str) {
    assert_eq!(owned.envelopes, 0, "{label}: an envelope was minted");
    assert_eq!(owned.middleware, 0, "{label}: a chain ran");
    assert_eq!(owned.staged, 0, "{label}: an account was staged");
    assert_eq!(owned.recorded, 0, "{label}: an account was recorded");
    assert_eq!(owned.permits, 0, "{label}: a permit owner was released");
}

/// One admitted class 15.T3 drives, and everything its one account names.
///
/// The class label and the terminal are stated rather than read back off the
/// record: a row that took production's own answer as its expectation would
/// agree with whatever the recorder wrote.
struct Admitted {
    class: &'static str,
    method: &'static str,
    path: &'static str,
    status: u16,
    protocol: &'static str,
    terminal: &'static str,
    /// How many permit owners this class releases.
    ///
    /// One for a class that consumes a request body, and none for a head-only
    /// class: an upgrade and an event stream are dispatched without a body
    /// plan at all, so production never offers them a permit to hold. Stated
    /// per row rather than assumed, because "released what it held" is only a
    /// claim where something was held.
    permits: usize,
    /// How many records this row's whole journey writes under that label set.
    ///
    /// One per admitted operation, and a proxied row drives two of them in this
    /// process: the peer's, served by the matrix, and the forwarded leg the
    /// upstream served for it. The counter is process-wide, so a row whose two
    /// operations are answered on the same class sees both. The per-listener
    /// account beside it is what says each of those was recorded once.
    records: u64,
}

impl Admitted {
    /// The whole label set this row's record carries.
    ///
    /// Built as one value because it is one claim: a delta taken over a subset
    /// of the labels would count a record another class produced. No row here
    /// crosses a configured bound, so every one of them names the absence.
    fn labels<'a>(&'a self, status: &'a str) -> [(&'a str, &'a str); 5] {
        [
            ("method", self.method),
            ("status", status),
            ("protocol", self.protocol),
            ("terminal", self.terminal),
            ("boundary", "none"),
        ]
    }

    /// The fields this row's completion event must carry.
    fn stated(&self) -> [String; 4] {
        [
            format!("status={}", self.status),
            format!("protocol={}", self.protocol),
            format!("terminal={}", self.terminal),
            "boundary=none".to_owned(),
        ]
    }
}

/// Every class answered over ordinary HTTP framing that this row drives whole.
const OWNED_EXCHANGE_CLASSES: [Admitted; 4] = [
    Admitted {
        class: "ordinary http",
        method: "GET",
        path: MATRIX_HTTP,
        status: 200,
        protocol: "ordinary_http",
        terminal: "completed",
        permits: 1,
        records: 1,
    },
    Admitted {
        class: "generic stream",
        method: "GET",
        path: MATRIX_STREAM,
        status: 200,
        protocol: "streaming_http",
        terminal: "completed",
        permits: 1,
        records: 1,
    },
    Admitted {
        class: "buffered proxy",
        method: "GET",
        path: MATRIX_BUFFERED,
        status: 200,
        // The forwarded leg the upstream serves for this row is answered as
        // ordinary HTTP, so it lands under a label set that is not this one.
        protocol: "proxy",
        terminal: "completed",
        permits: 1,
        records: 1,
    },
    Admitted {
        class: "streaming proxy",
        method: "GET",
        path: MATRIX_STREAMED,
        status: 200,
        protocol: "proxy",
        terminal: "completed",
        permits: 1,
        records: 1,
    },
];

/// The event stream, whose producer runs until the peer reading it is gone.
///
/// Driven as a row of its own rather than folded into the exchange loop above,
/// because its terminal is the peer's departure rather than a body that ended.
const OWNED_SSE_CLASS: Admitted = Admitted {
    class: "sse",
    method: "GET",
    path: MATRIX_SSE,
    status: 200,
    protocol: "server_sent_events",
    terminal: "disconnect",
    permits: 0,
    records: 1,
};

/// The gRPC class, answered on its own listener at the boundary tonic owns.
const OWNED_GRPC_CLASS: Admitted = Admitted {
    class: "grpc",
    method: "POST",
    path: ECHO_PATH,
    status: 200,
    protocol: "grpc",
    terminal: "completed",
    permits: 0,
    records: 1,
};

/// Both upgrade classes, whose HTTP operation ends at the handoff it committed.
#[cfg(feature = "ws")]
const OWNED_UPGRADE_CLASSES: [Admitted; 2] = [
    Admitted {
        class: "direct upgrade",
        method: "GET",
        path: MATRIX_WS,
        status: 101,
        protocol: "websocket",
        terminal: "completed",
        permits: 0,
        records: 1,
    },
    Admitted {
        class: "proxied upgrade",
        method: "GET",
        path: MATRIX_WS_PROXY,
        status: 101,
        // The handshake establishes the class, not the registration: a proxied
        // upgrade is answered as an upgrade, whichever route forwarded it.
        protocol: "websocket",
        terminal: "completed",
        permits: 0,
        // Two operations, on one class: the peer's upgrade, committed by the
        // matrix, and the forwarded upgrade the upstream committed for it.
        records: 2,
    },
];

/// The one completion event this row's raw path produced.
fn completion_event_of(capture: &common::TraceCapture, label: &str) -> Box<str> {
    let settled = http_support::poll_until(BOUND, || !capture.is_empty());
    assert!(settled, "{label}: no completion event was recorded");
    common::only_event(&capture.events(), COMPLETION_EVENT, label).into()
}

/// Assert this row's own account reached the operator, in both channels, once.
///
/// The event and the counter are read together because production writes them
/// from one account: a row that moved one and not the other would be a request
/// no operator can correlate between the two.
async fn assert_recorded_for_the_operator(
    addr: std::net::SocketAddr,
    baseline: &Baseline,
    capture: &common::TraceCapture,
    row: &Admitted,
) {
    let event = completion_event_of(capture, row.class);
    let stated = row.stated();
    let stated: Box<[&str]> = stated.iter().map(String::as_str).collect();
    common::assert_fields(&event, &stated, row.class);

    let status = row.status.to_string();
    assert_eq!(
        moved_on_the_wire(addr, &baseline.scraped, &row.labels(&status), row.records).await,
        row.records,
        "{}: unexpected records under this row's whole label set",
        row.class,
    );
}

/// Capture only what this row's own raw path records.
fn capture_for(row: &Admitted) -> common::TraceCapture {
    common::capture_events(&format!("path={}", row.path))
}

/// 15.T3
///
/// Every admitted class over the same public matrix 14.T2 serves owns exactly
/// one envelope, one middleware execution, one staged account, one recorded
/// account, one released permit, and the wire status its own owner committed —
/// and that account reaches the operator once, in the event and in the counter
/// alike. The three rows that reach no owner they might have had — a chain that
/// refused before its terminal, a handshake that never committed, and a head
/// Hyper never admitted — are read the same way and prove the absent owners
/// stay absent.
#[test]
fn cross_protocol_operation_owns_one_envelope_middleware_and_completion() {
    common::run_in_child(
        "cross_protocol_service_operation::cross_protocol_operation_owns_one_envelope_middleware_and_completion",
        "cross-protocol-operation-ownership",
        "CROSS_PROTOCOL_OPERATION_OWNERSHIP_COMPLETE",
        BOUND,
        assert_cross_protocol_operation_ownership,
    );
}

/// Run the process-global metrics claim where no sibling test can add records.
fn assert_cross_protocol_operation_ownership() {
    camber::runtime::builder()
        .with_metrics()
        .with_tracing()
        .run(|| {
            camber::runtime::block_on(async {
                let fixture = MatrixFixture::serve();
                assert_each_admitted_class_owns_one_account(&fixture).await;
                assert_event_stream_owns_one_account(&fixture).await;
                assert_upgrades_own_one_account(&fixture).await;
                assert_grpc_owns_one_account(&fixture).await;
                assert_short_circuit_owns_its_account_and_nothing_else(&fixture).await;
                assert_failed_upgrade_owns_only_its_refusal(&fixture).await;
                assert_unadmitted_head_owns_nothing(&fixture).await;
                fixture.tear_down("owned once");
            });
        })
        .expect("the owned-account matrix ran to completion");
}

/// Each class driven whole owns one of everything, and releases its permit.
async fn assert_each_admitted_class_owns_one_account(fixture: &MatrixFixture) {
    for row in &OWNED_EXCHANGE_CLASSES {
        let baseline = Baseline::take(fixture.addr(), &fixture.controller).await;
        let capture = capture_for(row);
        let answered = matrix_exchange(fixture.addr(), row.path, true).await;
        assert_eq!(answered.status, row.status, "{}: wire status", row.class);

        let owned = owned_once(&baseline, &fixture.controller, row.permits, row.class);
        assert_owned_once(&owned, row.permits, row.class);
        assert_recorded_for_the_operator(fixture.addr(), &baseline, &capture, row).await;
    }
}

/// The event stream owns one account, written when its peer goes away.
///
/// The producer runs until the peer reading it is gone, so this row's terminal
/// is that departure rather than a body that ended. It is read here rather than
/// left to 14.T2's disconnect row: that row asserts what the producer did, and
/// nothing there says the operation was accounted for exactly once.
async fn assert_event_stream_owns_one_account(fixture: &MatrixFixture) {
    let row = &OWNED_SSE_CLASS;
    let baseline = Baseline::take(fixture.addr(), &fixture.controller).await;
    let capture = capture_for(row);
    let ended_before = fixture.work.ended();
    let addr = fixture.addr();

    let head = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the event-stream peer connected");
        let head = format!(
            "GET {MATRIX_SSE} HTTP/1.1\r\nHost: {MATRIX_HOST}\r\n{CREDENTIAL_HEADER}: \
             {CREDENTIAL}\r\n\r\n"
        );
        std::io::Write::write_all(&mut peer, head.as_bytes())
            .expect("the event-stream request was sent");
        let answered = common::read_until_double_crlf(&mut peer);
        // Dropped inside the blocking owner: the peer is gone from here, and
        // the account that follows is what the service made of that.
        drop(peer);
        answered
    })
    .await
    .expect("the event-stream peer settled");
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "{}: the event stream never committed its head: {head}",
        row.class,
    );
    let ended = http_support::poll_until(BOUND, || fixture.work.ended() > ended_before);
    assert!(
        ended,
        "{}: the event producer outlived the peer it was being read by",
        row.class,
    );

    let owned = owned_once(&baseline, &fixture.controller, row.permits, row.class);
    assert_owned_once(&owned, row.permits, row.class);
    assert_recorded_for_the_operator(addr, &baseline, &capture, row).await;
}

/// Both upgrade classes own one account, written at the handoff they committed.
///
/// An upgrade has no body to end: its HTTP operation ends the moment the
/// transport passes on, and the session past that point reports through its own
/// terminals rather than through this account.
#[cfg(feature = "ws")]
async fn assert_upgrades_own_one_account(fixture: &MatrixFixture) {
    for row in &OWNED_UPGRADE_CLASSES {
        let baseline = Baseline::take(fixture.addr(), &fixture.controller).await;
        let capture = capture_for(row);
        let head = matrix_handshake(fixture.addr(), row.path, true).await;
        assert!(
            head.starts_with("HTTP/1.1 101"),
            "{}: the upgrade never committed: {head}",
            row.class,
        );

        let owned = owned_once(&baseline, &fixture.controller, row.permits, row.class);
        assert_owned_once(&owned, row.permits, row.class);
        assert_recorded_for_the_operator(fixture.addr(), &baseline, &capture, row).await;
    }
}

#[cfg(not(feature = "ws"))]
async fn assert_upgrades_own_one_account(_fixture: &MatrixFixture) {}

/// The gRPC class owns the same one of everything, at the boundary tonic owns.
async fn assert_grpc_owns_one_account(fixture: &MatrixFixture) {
    let row = &OWNED_GRPC_CLASS;
    let controller = fixture.grpc.controller();
    let addr = fixture.grpc_addr();
    let baseline = Baseline::take(addr, controller).await;
    let capture = capture_for(row);
    let answered = matrix_rpc(addr, true).await;
    assert_eq!(answered.status, row.status, "{}: wire status", row.class);

    let owned = owned_once(&baseline, controller, row.permits, row.class);
    assert_owned_once(&owned, row.permits, row.class);
    assert_recorded_for_the_operator(addr, &baseline, &capture, row).await;
}

/// A chain that refused still owns one envelope, one execution, and one account.
///
/// The negative half is what makes it a control: no specialized owner ran behind
/// the refusal, so the account this row recorded is the chain's own answer and
/// not a producer's.
async fn assert_short_circuit_owns_its_account_and_nothing_else(fixture: &MatrixFixture) {
    let label = "short-circuited";
    let entered = fixture.work.entered();
    let baseline = Baseline::take(fixture.addr(), &fixture.controller).await;
    let refused = matrix_exchange(fixture.addr(), MATRIX_STREAM, false).await;
    assert_eq!(refused.status, MATRIX_REFUSED, "{label}: wire status");

    let owned = owned_once(&baseline, &fixture.controller, SHORT_CIRCUIT_PERMITS, label);
    assert_owned_once(&owned, SHORT_CIRCUIT_PERMITS, label);
    assert_eq!(
        fixture.work.entered(),
        entered,
        "{label}: a producer ran behind a chain that refused",
    );
}

/// A handshake Camber refused owns its refusal and no session at all.
///
/// The head carries the credential, so the chain passes and the refusal is the
/// handshake's own rather than the chain's. Nothing about it is a `101`, so the
/// upgrade owner behind it is one that never ran — and the account this row
/// recorded is the refusal Camber answered with, not a session it never had.
#[cfg(feature = "ws")]
async fn assert_failed_upgrade_owns_only_its_refusal(fixture: &MatrixFixture) {
    let label = "failed upgrade";
    let addr = fixture.addr();
    let entered = fixture.work.entered();
    let upgrades_ended = fixture.work.upgrades_ended();
    let baseline = Baseline::take(addr, &fixture.controller).await;

    let head = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the broken-handshake peer connected");
        // Everything an upgrade needs except the key it must be answered
        // against, so Camber refuses the handshake it cannot complete.
        let head = format!(
            "GET {MATRIX_WS} HTTP/1.1\r\nHost: {MATRIX_HOST}\r\n{CREDENTIAL_HEADER}: \
             {CREDENTIAL}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Version: 13\r\n\r\n"
        );
        std::io::Write::write_all(&mut peer, head.as_bytes())
            .expect("the broken handshake was sent");
        let answered = common::read_until_double_crlf(&mut peer);
        drop(peer);
        answered
    })
    .await
    .expect("the broken-handshake peer settled");
    assert!(
        head.starts_with(&format!("HTTP/1.1 {REFUSED_HANDSHAKE}")),
        "{label}: a handshake carrying no key was answered as something other than \
         {REFUSED_HANDSHAKE}: {head}",
    );

    let owned = owned_once(
        &baseline,
        &fixture.controller,
        FAILED_UPGRADE_PERMITS,
        label,
    );
    assert_owned_once(&owned, FAILED_UPGRADE_PERMITS, label);
    assert_eq!(
        fixture.work.entered(),
        entered,
        "{label}: a session owner ran behind a handshake that never committed",
    );
    assert_eq!(
        fixture.work.upgrades_ended(),
        upgrades_ended,
        "{label}: a session owner returned for a handshake that never committed",
    );
}

#[cfg(not(feature = "ws"))]
async fn assert_failed_upgrade_owns_only_its_refusal(_fixture: &MatrixFixture) {}

/// A head Hyper never admitted reaches no envelope, no chain, and no account.
async fn assert_unadmitted_head_owns_nothing(fixture: &MatrixFixture) {
    let label = "unadmitted head";
    let addr = fixture.addr();
    let baseline = Baseline::take(addr, &fixture.controller).await;
    let answered = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the unadmitted peer connected");
        // A request line Hyper's parser refuses outright: the head never becomes
        // a request, so no Camber owner is ever created for it.
        std::io::Write::write_all(&mut peer, b"GET / HTTP/9.9\r\nHost: matrix.test\r\n\r\n")
            .expect("the unadmitted head was written");
        http_support::drain_to_close(&mut peer, BOUND)
    })
    .await
    .expect("the unadmitted peer settled")
    .expect("the unadmitted peer read its answer");
    assert!(
        !answered.starts_with("HTTP/1.1 200"),
        "{label}: the parser admitted a head it should have refused: {answered}",
    );
    assert_owned_nothing(&OwnedOnce::since(&baseline, &fixture.controller), label);
}
