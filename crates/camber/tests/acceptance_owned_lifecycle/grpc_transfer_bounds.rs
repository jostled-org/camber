//! 18.T1: every native tonic RPC form under Camber's transfer bounds.
//!
//! Each row drives one generated tonic form through Camber's own HTTP/2
//! serving, with one bound frozen on one direction. Before tonic's head
//! commits, an upload byte, quiet, or lifetime bound and the request total are
//! Camber's: the row withholds tonic's head at the commit barrier, and the
//! crossing cause takes the commitment and maps once. After the head, tonic owns
//! the status: a retained upload's typed failure reaches the method that reads
//! it, and a download bound ends or resets that one stream without a status of
//! its own. A client-streaming answer holds its first DATA while its retained
//! upload's bound fires, so the request stream outlives its head.
//!
//! Every row names its form, direction, bound, stimulus, wire result, owner
//! terminal, and commitment. Every row then proves the same settlement: one
//! commitment, one recorded completion, every direction owner released, a
//! sibling stream held in flight on the same connection across the failure and
//! answered after it, and the connection permit returned. Rows run one at a
//! time on their own listener, and every row is torn down before the matrix
//! reports.
//!
//! This is regression proof over the existing owners. It assumes no defect.

use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::mock::{
    InboundTerminal, ResponseCommit, ResponseCommitmentEdge, ScopedAdmittedOperation,
    TransferDirectionObservation, TransferObservation, TransferOwnerEdge,
};
use camber::http::{
    ByteBoundary, DeadlineBoundary, RejectionKind, RejectionProtocol, RequestBudget, TransferBudget,
};
use futures_util::FutureExt;

use crate::common;
use crate::grpc_forms::{
    ClientAnswer, Form, FormScript, ReplyFeed, greeting_text, hello_frame, reply_frame,
};
use crate::grpc_rows::{
    BOUND, Baseline, Budgets, Forms, GRPC_HEAD, SIBLING_STREAM_PATH, Sibling, SiblingGate, Stage,
    answered_with, eventually, open_call, permits_back, sent, sent_whole, sibling_settled,
    stream_reset_under_head, trailer_status, unmapped_refusal,
};
use crate::integration_rows::{Row, all, expect, expect_eq};

/// A request total no row reaches.
const UNREACHED: Duration = Duration::from_secs(300);

/// The quiet interval an idle row freezes.
const QUIET: Duration = Duration::from_millis(300);

/// The lifetime a transfer-total row freezes.
const LIFETIME: Duration = Duration::from_millis(500);

/// The request total a request-total row freezes.
///
/// Wider than the transfer bounds: the row has to reach tonic's withheld head
/// before it expires.
const REQUEST_TOTAL: Duration = Duration::from_secs(1);

/// The cadence a moving direction keeps, well inside [`QUIET`].
const PACE: Duration = Duration::from_millis(60);

/// How many steps a moving direction may take. Many lifetimes long, so a total
/// that progress restarted would outlast them.
const STEPS: usize = 80;

/// The margin a row waits past a frozen deadline to prove it cannot fire.
const PAST: Duration = Duration::from_millis(200);

// ---------------------------------------------------------------------------
// Row descriptions
// ---------------------------------------------------------------------------

/// The direction a row bounds.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Direction {
    Upload,
    Download,
    /// The request total, from the admitted head to the committed head.
    Request,
}

/// The one bound a row freezes on its direction.
#[derive(Clone, Copy, Debug)]
enum Bound {
    Bytes(usize),
    Idle(Duration),
    Total(Duration),
}

impl Bound {
    /// This bound as a transfer budget.
    fn transfer(self) -> TransferBudget {
        let budget = TransferBudget::unbounded();
        match self {
            Self::Bytes(max) => budget.with_max_bytes(max),
            Self::Idle(interval) => budget.with_idle(interval),
            Self::Total(lifetime) => budget.with_total(lifetime),
        }
        .expect("every row bound is a valid transfer budget")
    }
}

/// A request budget bounded by `lifetime` alone.
fn request_lifetime(lifetime: Duration) -> RequestBudget {
    RequestBudget::unbounded()
        .with_total(lifetime)
        .expect("every row lifetime is a valid request total")
}

/// Where a byte row puts its maximum against the payload it sends.
#[derive(Clone, Copy, Debug)]
enum Limit {
    Below,
    Equal,
    Crossing,
}

impl Limit {
    const ALL: [Self; 3] = [Self::Below, Self::Equal, Self::Crossing];

    /// The maximum this limit freezes for a payload of `bytes`.
    const fn maximum(self, bytes: usize) -> usize {
        match self {
            Self::Below => bytes + 1,
            Self::Equal => bytes,
            Self::Crossing => bytes - 1,
        }
    }
}

/// What a retained upload is ended by after the head.
#[derive(Clone, Copy, Debug)]
enum Retained {
    Bytes,
    Idle,
    Total,
}

impl Retained {
    /// The typed error the upload owner ends tonic's request stream with.
    const fn error(self) -> RuntimeError {
        match self {
            Self::Bytes => RuntimeError::LimitExceeded(ByteBoundary::TransferUpload),
            Self::Idle => RuntimeError::DeadlineExceeded(DeadlineBoundary::TransferIdle),
            Self::Total => RuntimeError::DeadlineExceeded(DeadlineBoundary::TransferTotal),
        }
    }

    const fn terminal(self) -> InboundTerminal {
        match self {
            Self::Bytes => InboundTerminal::TransferBytes,
            Self::Idle => InboundTerminal::TransferIdle,
            Self::Total => InboundTerminal::TransferTotal,
        }
    }

    fn bound(self) -> Bound {
        match self {
            Self::Bytes => {
                Bound::Bytes(hello_frame("first").len() + hello_frame("second").len() - 1)
            }
            Self::Idle => Bound::Idle(QUIET),
            Self::Total => Bound::Total(LIFETIME),
        }
    }
}

/// How one row drives its call.
#[derive(Clone, Copy, Debug)]
enum Drive {
    PreHeadBytes(Limit),
    PreHeadIdle,
    PreHeadTotal,
    RequestTotal,
    RetainedUpload(Retained),
    AnswerEndsRetainedUpload,
    CompletedUpload,
    DownloadBytes(Limit),
    DownloadIdle,
    DownloadTotal,
    UploadMovesDownloadQuiet,
    DownloadMovesUploadQuiet,
    TwoWayUploadTotal,
    TwoWayDownloadTotal,
}

/// What the peer must read at the end of a row.
#[derive(Debug)]
enum Wire {
    /// Refused before any head: this status, mapped once under this kind.
    Mapped(u16, RejectionKind),
    /// tonic's committed answer, with this status in its trailers. The message
    /// is compared when the row knows it.
    Status(tonic::Code, Option<Box<str>>),
    /// tonic's committed head stays, and this one stream is reset with no
    /// gRPC status behind it.
    Reset,
}

/// One matrix row: what it drives and what it must observe.
struct Case {
    family: &'static str,
    form: Form,
    direction: Direction,
    bound: Bound,
    stimulus: &'static str,
    wire: Wire,
    /// The terminal the bounded direction's owner fixes.
    terminal: InboundTerminal,
    commitment: ResponseCommit,
    drive: Drive,
}

impl Case {
    fn label(&self) -> Box<str> {
        format!(
            "{} | {} | {:?} {:?} | {} | {:?} | {:?} | {:?}",
            self.family,
            self.form.name(),
            self.direction,
            self.bound,
            self.stimulus,
            self.wire,
            self.terminal,
            self.commitment,
        )
        .into_boxed_str()
    }

    /// The budgets this row's router freezes.
    fn budgets(&self) -> Budgets {
        let base = Budgets::unbounded().with_request(request_lifetime(UNREACHED));
        let bounded = match (self.direction, self.bound) {
            (Direction::Upload, bound) => base.with_upload(bound.transfer()),
            (Direction::Download, bound) => base.with_download(bound.transfer()),
            (Direction::Request, Bound::Total(lifetime)) => {
                base.with_request(request_lifetime(lifetime))
            }
            // A request is bounded here by its lifetime alone.
            (Direction::Request, Bound::Bytes(_) | Bound::Idle(_)) => base,
        };
        match self.drive {
            // Every bound a completed upload could still have answered to.
            Drive::CompletedUpload => bounded
                .with_request(request_lifetime(LIFETIME))
                .with_upload(
                    Bound::Idle(QUIET)
                        .transfer()
                        .with_total(LIFETIME)
                        .expect("a valid upload lifetime"),
                ),
            // Both directions get the same quiet interval, so only progress
            // separates them.
            Drive::UploadMovesDownloadQuiet => bounded.with_upload(Bound::Idle(QUIET).transfer()),
            Drive::DownloadMovesUploadQuiet => bounded.with_download(Bound::Idle(QUIET).transfer()),
            _ => bounded,
        }
    }

    /// Where this row holds its sibling's answer across the failure.
    ///
    /// Only the request-total and completed-upload rows bound the request
    /// total, and neither bounds the download.
    const fn sibling_hold(&self) -> SiblingHold {
        match self.drive {
            Drive::RequestTotal | Drive::CompletedUpload => SiblingHold::Body,
            _ => SiblingHold::Handler,
        }
    }

    /// How this row's client-streaming method answers.
    ///
    /// Only the retained-upload rows leave the request stream open under the
    /// head; a bidirectional method answers to no such choice. A retained row
    /// parks the reader until its answer's first DATA is held.
    const fn client_answer(&self) -> ClientAnswer {
        match self.drive {
            Drive::RetainedUpload(_) => ClientAnswer::EarlyParked,
            Drive::AnswerEndsRetainedUpload => ClientAnswer::Early,
            _ => ClientAnswer::AfterUpload,
        }
    }
}

// ---------------------------------------------------------------------------
// Payloads
// ---------------------------------------------------------------------------

/// Request or reply bytes, as the frames a row sends or counts.
type Chunks = Box<[Box<[u8]>]>;

/// The request chunks an upload row sends, in order.
///
/// A unary or server-streaming call takes one message, so it arrives split in
/// two. The streaming-request forms send two whole messages.
fn upload_chunks(form: Form) -> Chunks {
    match form {
        Form::Unary | Form::ServerStreaming => {
            let frame = hello_frame("bounded-upload");
            let (head, tail) = frame.split_at(frame.len() / 2);
            Box::new([head.into(), tail.into()])
        }
        Form::ClientStreaming | Form::Bidirectional => {
            Box::new([hello_frame("first"), hello_frame("second")])
        }
    }
}

fn payload_len(chunks: &[Box<[u8]>]) -> usize {
    chunks.iter().map(|chunk| chunk.len()).sum()
}

/// The opening an idle row sends before it goes quiet.
///
/// Short of a whole message for the forms tonic reads to the end before the
/// method runs; one whole message for the forms whose method reads it.
fn quiet_opening(form: Form) -> Box<[u8]> {
    let frame = hello_frame("quiet");
    match form {
        Form::Unary | Form::ServerStreaming => frame[..3].into(),
        Form::ClientStreaming | Form::Bidirectional => frame,
    }
}

/// The pieces a moving upload sends, one per step.
///
/// One long message cut into pieces for the forms tonic reads to the end
/// first, so every piece is progress and none completes the request. Whole
/// messages for the forms whose method reads each one.
fn moving_pieces(form: Form) -> Chunks {
    match form {
        Form::Unary | Form::ServerStreaming => hello_frame(&"x".repeat(STEPS * 16))
            .chunks(16)
            .map(Box::from)
            .collect(),
        Form::ClientStreaming | Form::Bidirectional => {
            (0..STEPS).map(|_| hello_frame("moving")).collect()
        }
    }
}

/// The replies a streamed download answer carries, released in order.
const STREAMED_REPLIES: &[&str] = &["one", "two"];

/// The request payload a download row sends.
fn download_request(form: Form) -> Chunks {
    match form {
        Form::Unary | Form::ServerStreaming => Box::new([hello_frame("bytes")]),
        Form::ClientStreaming => Box::new([hello_frame("first"), hello_frame("second")]),
        Form::Bidirectional => Box::new([]),
    }
}

/// The replies a download row releases, or none for a single-message answer.
const fn released_replies(form: Form) -> &'static [&'static str] {
    match form {
        Form::ServerStreaming | Form::Bidirectional => STREAMED_REPLIES,
        Form::Unary | Form::ClientStreaming => &[],
    }
}

/// The reply frames a download row's answer carries.
///
/// A single-message answer greets the request; a streamed one carries the
/// replies the row releases.
fn download_frames(form: Form) -> Chunks {
    match form {
        Form::Unary => Box::new([reply_frame(&greeting_text("bytes"))]),
        Form::ClientStreaming => Box::new([reply_frame(&greeting_text("first,second"))]),
        Form::ServerStreaming | Form::Bidirectional => released_replies(form)
            .iter()
            .map(|reply| reply_frame(reply))
            .collect(),
    }
}

/// The status tonic derives from one typed request-body failure.
fn typed_status(error: RuntimeError) -> (tonic::Code, Box<str>) {
    let status = tonic::Status::from_error(Box::new(error));
    (status.code(), status.message().into())
}

// ---------------------------------------------------------------------------
// The served row
// ---------------------------------------------------------------------------

/// Where a row holds its sibling's answer while the row fails.
///
/// Both holds keep the sibling's stream open on the row's connection and out
/// of every bound the row freezes.
#[derive(Clone, Copy, Debug)]
enum SiblingHold {
    /// Inside its handler, before its commitment. A plain answer has no
    /// transfer owner, so no transfer bound reaches it.
    Handler,
    /// Behind its committed head, its body withheld. Its commitment is taken,
    /// so no request total reaches it.
    Body,
}

/// One row's listener, script, mapper record, and connection.
struct Session {
    label: Box<str>,
    server: common::ObservedServer<ScopedAdmittedOperation>,
    owners: Arc<ScopedAdmittedOperation>,
    script: Arc<FormScript>,
    feed: ReplyFeed,
    journal: common::Journal,
    client: common::PersistentH2Client,
    gate: Arc<SiblingGate>,
    /// The sibling, open from before the row's call until its answer is read.
    sibling: Option<Sibling>,
    before: Baseline,
}

impl Session {
    async fn open(case: &Case) -> Self {
        let label = case.label();
        let (forms, router) =
            Forms::router(case.client_answer(), case.budgets(), "grpc-transfer-bounds");
        let port = common::reserve_admitted_operation();
        let owners = port.controller();
        let server = port.serve(router);
        let before = forms.baseline(&owners).await;
        let Forms {
            script,
            feed,
            journal,
            gate,
        } = forms;
        let client = common::PersistentH2Client::connect(server.addr(), BOUND).await;
        Self {
            label,
            server,
            owners,
            script,
            feed,
            journal,
            client,
            gate,
            sibling: None,
            before,
        }
    }

    /// Open the sibling stream and hold its answer where `hold` names.
    ///
    /// Called before the row's call opens, so the sibling is in flight on the
    /// same connection for the whole failure.
    async fn hold_sibling(&mut self, hold: SiblingHold) -> Row {
        match hold {
            SiblingHold::Handler => {
                let sibling = Sibling::held_in_handler(&mut self.client, &self.gate).await?;
                self.sibling = Some(sibling);
                Ok(())
            }
            SiblingHold::Body => {
                // The sibling's download is the only transfer owner yet, so it
                // takes this hold. Released, it parks on its withheld body and
                // takes no turn before the gate opens, so it can never reach a
                // poll edge the row arms later.
                let poll = TransferOwnerEdge::BeforeSourcePoll;
                self.arm(poll);
                let mut sibling = self
                    .client
                    .open_paced("GET", SIBLING_STREAM_PATH, "localhost", &[])
                    .await;
                sibling.finish();
                let read = sibling.commit().await;
                self.paused(poll).await;
                self.release(poll);
                let status = read.status();
                self.sibling = Some(Sibling::Committed(read));
                expect_eq("sibling head status", status, 200)
            }
        }
    }

    /// Open this row's call on its form's path, its body still owed.
    async fn call(&mut self, form: Form) -> common::H2RequestStream {
        open_call(&mut self.client, form).await
    }

    /// Arm the commit barrier, then open this row's call, so tonic's head is
    /// withheld whenever it is produced.
    async fn call_head_withheld(&mut self, form: Form) -> common::H2RequestStream {
        self.arm(ResponseCommitmentEdge::GrpcHeadReady);
        self.call(form).await
    }

    /// Read the answer a crossing cause committed, then let go of tonic's
    /// withheld head.
    ///
    /// tonic's head was produced and withheld; the cause took the commitment
    /// first, so it never crossed the handoff.
    async fn refused_past_withheld_head(
        &self,
        stream: &mut common::H2RequestStream,
    ) -> common::H2Settled {
        let settled = stream.commit().await.settle().await;
        let head = ResponseCommitmentEdge::GrpcHeadReady;
        self.paused(head).await;
        self.release(head);
        settled
    }

    /// The checkpoints this row steps its call's owners through.
    fn stage(&self) -> Stage<'_, ScopedAdmittedOperation> {
        Stage {
            owners: &self.owners,
            label: &self.label,
        }
    }

    fn transfers(&self) -> TransferObservation {
        self.stage().transfers()
    }

    fn arm<P>(&self, point: P)
    where
        P: common::OwnerPoint,
        ScopedAdmittedOperation: common::Owns<P::Owner>,
    {
        self.stage().arm(point);
    }

    async fn paused<P>(&self, point: P)
    where
        P: common::OwnerPoint,
        ScopedAdmittedOperation: common::Owns<P::Owner>,
    {
        self.stage().paused(point).await;
    }

    fn release<P>(&self, point: P)
    where
        P: common::OwnerPoint,
        ScopedAdmittedOperation: common::Owns<P::Owner>,
    {
        self.stage().release(point);
    }

    /// Wait until the upload owner admitted `bytes` in all.
    async fn upload_admitted(&self, bytes: usize) -> Row {
        let admitted = eventually(|| self.transfers().upload.admitted_bytes == bytes).await;
        expect(
            &format!(
                "the upload never admitted {bytes} bytes: {:?}",
                self.transfers().upload
            ),
            admitted,
        )
    }

    /// Wait until the upload reached its normal end.
    async fn upload_ended(&self) -> Row {
        self.stage().upload_ended().await
    }

    /// Wait until tonic handed the method a failed request stream.
    async fn upload_failure(&self) -> Result<(tonic::Code, Box<str>), String> {
        let failed = eventually(|| self.script.upload_failure().is_some()).await;
        self.script
            .upload_failure()
            .filter(|_| failed)
            .ok_or_else(|| "no request-stream failure reached the method".into())
    }

    /// Settle the row: generic oracles, sibling, permit, teardown.
    async fn finish(mut self, case: &Case, answered: Result<Answered, String>) -> Row {
        let mapped = common::drain(&self.journal);
        let verdict = answered.and_then(|answered| {
            all([
                answered.checks,
                wire_checks(&case.wire, &answered.settled, &mapped),
            ])
        });
        let call = self.call_settled(case).await;
        let sibling = self.sibling().await;
        let teardown = self.tear_down().await;
        all([verdict, call, sibling, teardown])
    }

    /// One commitment, one completion, the owner terminal, and every
    /// direction owner released, with the sibling still held.
    ///
    /// The sibling was admitted before the call and has not completed, so it
    /// counts once among the admissions and never among the completions. A
    /// sibling held behind its head also committed first.
    async fn call_settled(&self, case: &Case) -> Row {
        let recorded = self.before.call_recorded(&self.owners).await;
        let operations = self.owners.commitment.operations_observed();
        let commitment = self.owners.commitment.observed();
        let downloads = usize::from(case.commitment == HEAD);
        let released = self.stage().owners_released(downloads).await;
        let transfers = self.transfers();
        all([
            recorded,
            expect_eq(
                "admitted operations, the held sibling's with the call's",
                operations.admitted - self.before.admitted,
                2,
            ),
            expect_eq(
                "recorded completions",
                operations.completions_recorded - self.before.recorded,
                1,
            ),
            expect_eq(
                "commitments taken",
                commitment.commits - self.before.commits,
                match case.sibling_hold() {
                    SiblingHold::Handler => 1,
                    SiblingHold::Body => 2,
                },
            ),
            expect_eq("commitment", commitment.committed, Some(case.commitment)),
            released,
            owner_terminal(case, &transfers),
        ])
    }

    /// Release the sibling held across the row, and require its answer.
    async fn sibling(&mut self) -> Row {
        sibling_settled(
            self.sibling.take(),
            &self.gate,
            &self.owners,
            &self.before,
            &self.journal,
        )
        .await
    }

    /// Close the connection, require its permit back, and stop the listener.
    async fn tear_down(self) -> Row {
        let Self {
            server,
            owners,
            script,
            client,
            ..
        } = self;
        client.close().await;
        let returned = permits_back(&owners).await;
        let reader = script.join_reader(BOUND).await;
        let stopped = server
            .shutdown_bounded(BOUND)
            .map_err(|error| format!("the row's listener did not stop: {error}"));
        all([returned, reader, stopped])
    }
}

/// The bounded direction's owner fixed the row's terminal.
fn owner_terminal(case: &Case, transfers: &TransferObservation) -> Row {
    match direction_of(transfers, case.direction) {
        Some(direction) => expect_eq(
            &format!("{:?} owner terminal", case.direction),
            direction.terminal,
            Some(case.terminal),
        ),
        // The request total's terminal is the commitment's cause.
        None => Ok(()),
    }
}

fn direction_of(
    transfers: &TransferObservation,
    direction: Direction,
) -> Option<&TransferDirectionObservation> {
    match direction {
        Direction::Upload => Some(&transfers.upload),
        Direction::Download => Some(&transfers.download),
        Direction::Request => None,
    }
}

/// What the peer read, against what the row's wire result names.
fn wire_checks(wire: &Wire, settled: &common::H2Settled, mapped: &[common::Observed]) -> Row {
    match wire {
        Wire::Mapped(status, kind) => all([
            unmapped_refusal(settled, *status),
            expect_eq("mapper invocations", mapped.len(), 1),
            mapped.first().map_or(Ok(()), |observed| {
                all([
                    expect_eq("mapped kind", observed.kind, *kind),
                    expect_eq("mapped status", observed.status, *status),
                    expect_eq(
                        "mapped protocol",
                        observed.protocol,
                        Some(RejectionProtocol::Grpc),
                    ),
                ])
            }),
        ]),
        Wire::Status(code, message) => all([
            answered_with(settled, *code),
            expect_eq("mapper invocations", mapped.len(), 0),
            message.as_ref().map_or(Ok(()), |message| {
                expect_eq(
                    "trailer message",
                    trailer_status(settled).as_ref().map(tonic::Status::message),
                    Some(message.as_ref()),
                )
            }),
        ]),
        Wire::Reset => all([
            stream_reset_under_head(settled),
            expect_eq("mapper invocations", mapped.len(), 0),
        ]),
    }
}

/// What a row's call ended on, and the row's own checks.
struct Answered {
    settled: common::H2Settled,
    checks: Row,
}

/// Keep `upload` and the reply feed moving until `until`'s owner fixes a
/// terminal, and hand back the observation that saw it.
///
/// Fails when every step ran out first: a bound that progress restarted would
/// never expire while either direction keeps moving.
async fn moving(
    session: &Session,
    stream: &mut common::H2RequestStream,
    upload: Option<&[Box<[u8]>]>,
    replies: bool,
    until: Direction,
) -> Result<TransferObservation, String> {
    for step in 0..STEPS {
        let transfers = session.transfers();
        if direction_of(&transfers, until)
            .and_then(|direction| direction.terminal)
            .is_some()
        {
            return Ok(transfers);
        }
        match upload.and_then(|pieces| pieces.get(step)) {
            Some(piece) => {
                let _offered = stream.offer_until_reset(piece, BOUND).await;
            }
            None => {}
        }
        match replies {
            true => {
                let _released = session.feed.reply("moving");
            }
            false => {}
        }
        tokio::time::sleep(PACE).await;
    }
    Err(format!(
        "the {until:?} bound never expired while the row kept moving: {:?}",
        session.transfers()
    ))
}

/// Commit a client-streaming answer's head over a request stream it left
/// open, and hold its first DATA.
///
/// The method answers after `first` and a retained reader keeps reading the
/// rest. With the first DATA held, the answer is unfinished, so the upload
/// under it keeps every bound of its own. The caller releases the held poll.
///
/// The reader stays parked until the hold is taken. Nothing polls the upload
/// until then, so the download is the only owner that can take it.
async fn open_single_reply(
    session: &Session,
    stream: &mut common::H2RequestStream,
    first: &[u8],
) -> Result<common::H2ReadHalf, String> {
    let handoff = ResponseCommitmentEdge::GrpcHandoffCommitted;
    session.arm(handoff);
    sent(stream, first).await?;
    session.paused(handoff).await;
    let read = session.stage().first_data_held(stream).await;
    session.script.start_reader();
    Ok(read)
}

// ---------------------------------------------------------------------------
// Pre-head rows
// ---------------------------------------------------------------------------

/// A payload below, at, or across the upload maximum, with tonic's head
/// withheld at the commit barrier.
async fn pre_head_bytes(
    case: &Case,
    session: &mut Session,
    limit: Limit,
) -> Result<Answered, String> {
    let chunks = upload_chunks(case.form);
    let head = ResponseCommitmentEdge::GrpcHeadReady;
    let mut stream = session.call_head_withheld(case.form).await;
    sent(&mut stream, &chunks[0]).await?;
    session.upload_admitted(chunks[0].len()).await?;
    let offered = stream.offer(&chunks[1], BOUND).await;
    match limit {
        Limit::Crossing => {
            let settled = session.refused_past_withheld_head(&mut stream).await;
            let upload = session.transfers().upload;
            Ok(Answered {
                settled,
                checks: all([
                    expect_eq("crossing chunk", offered, common::H2Offer::Sent),
                    expect_eq(
                        "admitted before the crossing",
                        upload.admitted_bytes,
                        chunks[0].len(),
                    ),
                    expect(
                        "the crossing frame was retained",
                        upload.crossings_released >= 1,
                    ),
                ]),
            })
        }
        Limit::Below | Limit::Equal => {
            expect_eq("last chunk", offered, common::H2Offer::Sent)?;
            session.upload_admitted(payload_len(&chunks)).await?;
            stream.finish();
            session.paused(head).await;
            let ended = session.upload_ended().await;
            session.release(head);
            session.feed.end();
            let settled = stream.commit().await.settle().await;
            Ok(Answered {
                settled,
                checks: all([
                    ended,
                    expect_eq("method entries", session.script.entered(case.form), 1),
                ]),
            })
        }
    }
}

/// A framed request left incomplete while tonic waits for the rest.
async fn pre_head_idle(case: &Case, session: &mut Session) -> Result<Answered, String> {
    let mut stream = session.call_head_withheld(case.form).await;
    let opening = quiet_opening(case.form);
    sent(&mut stream, &opening).await?;
    session.upload_admitted(opening.len()).await?;
    // The quiet interval is real time, so the answer is the committed terminal.
    let settled = session.refused_past_withheld_head(&mut stream).await;
    let upload = session.transfers().upload;
    Ok(Answered {
        settled,
        checks: all([
            expect_eq("frozen quiet interval", upload.idle, Some(QUIET)),
            expect_eq("upload terminals", upload.terminals, 1),
        ]),
    })
}

/// An upload that keeps moving until its lifetime ends it.
async fn pre_head_total(case: &Case, session: &mut Session) -> Result<Answered, String> {
    let mut stream = session.call_head_withheld(case.form).await;
    let pieces = moving_pieces(case.form);
    let ended = moving(
        session,
        &mut stream,
        Some(&pieces),
        false,
        Direction::Upload,
    )
    .await?;
    let settled = session.refused_past_withheld_head(&mut stream).await;
    Ok(Answered {
        settled,
        checks: all([
            expect_eq("frozen lifetime", ended.upload.total, Some(LIFETIME)),
            expect(
                "the upload made no progress before its lifetime ended",
                ended.upload.frames_polled >= 2,
            ),
        ]),
    })
}

/// The method entered, the upload complete, and tonic's head withheld past
/// the request total.
async fn request_total(case: &Case, session: &mut Session) -> Result<Answered, String> {
    let head = ResponseCommitmentEdge::GrpcHeadReady;
    let mut stream = session.call_head_withheld(case.form).await;
    sent_whole(&mut stream, &upload_chunks(case.form)).await?;
    session.paused(head).await;
    let ended = session.upload_ended().await;
    let entered = session.script.entered(case.form);
    let settled = stream.commit().await.settle().await;
    session.release(head);
    let operations = session.owners.commitment.operations_observed();
    let upload = session.transfers().upload;
    Ok(Answered {
        settled,
        checks: all([
            ended,
            expect_eq("method entries", entered, 1),
            expect_eq(
                "request total from admission",
                operations.total_from_admission,
                Some(REQUEST_TOTAL),
            ),
            expect_eq("upload terminals", upload.terminals, 1),
        ]),
    })
}

// ---------------------------------------------------------------------------
// Post-head rows
// ---------------------------------------------------------------------------

/// A request stream left open past the committed head, then ended by its own
/// upload bound.
///
/// A client-streaming answer is one reply, so the row holds its first DATA
/// across the stimulus and releases it once the method has the failure: tonic's
/// committed answer then completes with its own status.
async fn retained_upload(
    case: &Case,
    session: &mut Session,
    retained: Retained,
) -> Result<Answered, String> {
    let mut stream = session.call(case.form).await;
    let first = hello_frame("first");
    let single = matches!(case.form, Form::ClientStreaming);
    let read = match single {
        true => open_single_reply(session, &mut stream, &first).await?,
        false => {
            sent(&mut stream, &first).await?;
            stream.commit().await
        }
    };
    let head_status = read.status();
    let handed_off = session.owners.commitment.observed().committed;
    let open_at_head = session.transfers().upload.terminal;
    session.upload_admitted(first.len()).await?;
    match retained {
        Retained::Bytes => {
            let _offered = stream
                .offer_until_reset(&hello_frame("second"), BOUND)
                .await;
        }
        Retained::Idle => {}
        Retained::Total => {
            let pieces = moving_pieces(case.form);
            let _ended = moving(
                session,
                &mut stream,
                Some(&pieces),
                false,
                Direction::Upload,
            )
            .await?;
        }
    }
    let failure = session.upload_failure().await;
    // Read at the upload's failure, before the held poll is released.
    let download_at_failure = session.transfers().download;
    match single {
        true => session.release(TransferOwnerEdge::BeforeSourcePoll),
        false => {}
    }
    let settled = read.settle().await;
    let single_reply = match single {
        true => all([
            expect_eq(
                "answer frames polled when the upload bound fired",
                download_at_failure.frames_polled,
                0,
            ),
            expect_eq(
                "delivered bytes",
                settled.bytes,
                reply_frame(&greeting_text("first")).len(),
            ),
        ]),
        false => Ok(()),
    };
    Ok(Answered {
        checks: all([
            expect_eq("head status", head_status, 200),
            expect_eq("handoff before the stimulus", handed_off, Some(HEAD)),
            expect_eq("upload open at the head", open_at_head, None),
            expect_eq(
                "the status tonic handed the method",
                failure,
                Ok(typed_status(retained.error())),
            ),
            single_reply,
        ]),
        settled,
    })
}

/// A request stream left open under a single-message answer sent at once.
///
/// The control beside the retained client-streaming rows. The method answers
/// after one message and keeps reading the rest. Nothing holds the reply, so
/// the answer completes well inside the upload's quiet interval, and the upload
/// it left open ends with the operation's response lifetime instead.
async fn answer_ends_retained_upload(
    case: &Case,
    session: &mut Session,
) -> Result<Answered, String> {
    let mut stream = session.call(case.form).await;
    sent(&mut stream, &hello_frame("first")).await?;
    let read = stream.commit().await;
    let head_status = read.status();
    let failure = session.upload_failure().await;
    let settled = read.settle().await;
    Ok(Answered {
        checks: all([
            expect_eq("head status", head_status, 200),
            expect_eq(
                "the status tonic handed the method",
                failure,
                Ok(typed_status(RuntimeError::ChannelClosed)),
            ),
            expect("the answer carried no reply", settled.bytes > 0),
        ]),
        settled,
    })
}

/// A single request consumed before the head, then the request total and every
/// upload bound allowed to pass under an answer still being delivered.
async fn completed_upload(case: &Case, session: &mut Session) -> Result<Answered, String> {
    let mut stream = session.call(case.form).await;
    let request = [hello_frame("completed")];
    let read = match case.form {
        Form::Unary => session.stage().staged_reply(&mut stream, &request).await?,
        _ => {
            sent_whole(&mut stream, &request).await?;
            stream.commit().await
        }
    };
    let ended = session.upload_ended().await;
    tokio::time::sleep(LIFETIME.max(QUIET) + PAST).await;
    let (expected, released) = match case.form {
        Form::Unary => {
            session.release(TransferOwnerEdge::BeforeSourcePoll);
            (reply_frame(&greeting_text("completed")).len(), Ok(()))
        }
        _ => {
            let released = expect(
                "the owed reply found no response stream",
                session.feed.reply("one"),
            );
            session.feed.end();
            (reply_frame("one").len(), released)
        }
    };
    let settled = read.settle().await;
    let upload = session.transfers().upload;
    Ok(Answered {
        checks: all([
            ended,
            released,
            expect_eq("upload terminals", upload.terminals, 1),
            expect_eq("delivered bytes", settled.bytes, expected),
        ]),
        settled,
    })
}

/// An answer below, at, or across the download maximum, released only after
/// the peer read the committed head.
async fn download_bytes(
    case: &Case,
    session: &mut Session,
    limit: Limit,
) -> Result<Answered, String> {
    let request = download_request(case.form);
    let frames = download_frames(case.form);
    let mut stream = session.call(case.form).await;
    let read = match case.form {
        Form::Unary | Form::ClientStreaming => {
            let read = session.stage().staged_reply(&mut stream, &request).await?;
            session.release(TransferOwnerEdge::BeforeSourcePoll);
            read
        }
        Form::ServerStreaming | Form::Bidirectional => {
            sent_whole(&mut stream, &request).await?;
            let read = stream.commit().await;
            released_in_turn(session, released_replies(case.form), &frames).await?;
            read
        }
    };
    session.feed.end();
    let settled = read.settle().await;
    let total = payload_len(&frames);
    let last = frames.last().map_or(0, |frame| frame.len());
    let download = session.transfers().download;
    let delivered = match limit {
        Limit::Below | Limit::Equal => total,
        Limit::Crossing => total - last,
    };
    Ok(Answered {
        checks: all([
            expect_eq("delivered bytes", settled.bytes, delivered),
            expect_eq(
                "admitted download bytes",
                download.admitted_bytes,
                delivered,
            ),
            expect_eq(
                "crossing frames released",
                download.crossings_released,
                usize::from(matches!(limit, Limit::Crossing)),
            ),
        ]),
        settled,
    })
}

/// Release each reply only after the one before it was admitted, so no two
/// replies share a frame.
async fn released_in_turn(session: &Session, replies: &[&str], frames: &[Box<[u8]>]) -> Row {
    let mut admitted = 0;
    for (reply, frame) in replies.iter().zip(frames) {
        let _released = session.feed.reply(reply);
        admitted += frame.len();
        let reached = eventually(|| {
            let download = session.transfers().download;
            download.admitted_bytes == admitted || download.terminal.is_some()
        })
        .await;
        expect("a released reply was never read", reached)?;
    }
    Ok(())
}

/// A committed answer whose next DATA is withheld past the quiet interval.
///
/// A single-message answer is ready the moment its head commits, so its quiet
/// interval cannot expire: the row holds its first DATA past the interval and
/// requires the answer to complete.
async fn download_idle(case: &Case, session: &mut Session) -> Result<Answered, String> {
    let request = download_request(case.form);
    let mut stream = session.call(case.form).await;
    let read = match case.form {
        Form::Unary | Form::ClientStreaming => {
            let read = session.stage().staged_reply(&mut stream, &request).await?;
            tokio::time::sleep(QUIET + PAST).await;
            session.release(TransferOwnerEdge::BeforeSourcePoll);
            read
        }
        Form::ServerStreaming => {
            sent_whole(&mut stream, &request).await?;
            let read = stream.commit().await;
            // The first DATA goes; the next is withheld.
            released_in_turn(session, &["one"], &[reply_frame("one")]).await?;
            read
        }
        Form::Bidirectional => stream.commit().await,
    };
    let settled = read.settle().await;
    let download = session.transfers().download;
    let delivered = match case.form {
        Form::Unary | Form::ClientStreaming => payload_len(&download_frames(case.form)),
        Form::ServerStreaming => reply_frame("one").len(),
        Form::Bidirectional => 0,
    };
    Ok(Answered {
        checks: all([
            expect_eq("frozen quiet interval", download.idle, Some(QUIET)),
            expect_eq("delivered bytes", settled.bytes, delivered),
        ]),
        settled,
    })
}

/// A committed answer whose lifetime ends it, moving or held.
async fn download_total(case: &Case, session: &mut Session) -> Result<Answered, String> {
    let request = download_request(case.form);
    let mut stream = session.call(case.form).await;
    let read = match case.form {
        Form::Unary | Form::ClientStreaming => {
            let read = session.stage().staged_reply(&mut stream, &request).await?;
            // The lifetime was armed at the held poll, so it passes while the
            // first DATA is withheld.
            tokio::time::sleep(LIFETIME + PAST).await;
            session.release(TransferOwnerEdge::BeforeSourcePoll);
            read
        }
        Form::ServerStreaming | Form::Bidirectional => {
            sent_whole(&mut stream, &request).await?;
            let read = stream.commit().await;
            let _ended = moving(session, &mut stream, None, true, Direction::Download).await?;
            read
        }
    };
    let settled = read.settle().await;
    let download = session.transfers().download;
    let moved = matches!(case.form, Form::ServerStreaming | Form::Bidirectional);
    Ok(Answered {
        checks: all([
            expect_eq("frozen lifetime", download.total, Some(LIFETIME)),
            expect_eq(
                "bytes delivered before the lifetime ended",
                settled.bytes > 0,
                moved,
            ),
        ]),
        settled,
    })
}

// ---------------------------------------------------------------------------
// Independent directions
// ---------------------------------------------------------------------------

/// The upload moves and the download stays quiet: only the download expires.
async fn upload_moves_download_quiet(
    case: &Case,
    session: &mut Session,
) -> Result<Answered, String> {
    let mut stream = session.call(case.form).await;
    let read = stream.commit().await;
    let pieces = moving_pieces(case.form);
    let ended = moving(
        session,
        &mut stream,
        Some(&pieces),
        false,
        Direction::Download,
    )
    .await?;
    let settled = read.settle().await;
    Ok(Answered {
        settled,
        checks: all([
            expect(
                &format!("the moving upload expired too: {:?}", ended.upload),
                ended.upload.terminal != Some(InboundTerminal::TransferIdle),
            ),
            expect("the upload never moved", ended.upload.admitted_bytes > 0),
        ]),
    })
}

/// The download moves and the upload stays quiet: only the upload expires.
async fn download_moves_upload_quiet(
    case: &Case,
    session: &mut Session,
) -> Result<Answered, String> {
    let mut stream = session.call(case.form).await;
    let read = stream.commit().await;
    let ended = moving(session, &mut stream, None, true, Direction::Upload).await?;
    let failure = session.upload_failure().await;
    let settled = read.settle().await;
    Ok(Answered {
        settled,
        checks: all([
            expect(
                &format!("the moving download expired too: {:?}", ended.download),
                ended.download.terminal != Some(InboundTerminal::TransferIdle),
            ),
            expect(
                "the download never moved",
                ended.download.admitted_bytes > 0,
            ),
            expect_eq(
                "the status tonic handed the method",
                failure,
                Ok(typed_status(Retained::Idle.error())),
            ),
        ]),
    })
}

/// Both directions move, and one direction's lifetime still ends it.
async fn two_way_total(case: &Case, session: &mut Session) -> Result<Answered, String> {
    let mut stream = session.call(case.form).await;
    let read = stream.commit().await;
    let pieces = moving_pieces(case.form);
    let ended = moving(session, &mut stream, Some(&pieces), true, case.direction).await?;
    let settled = read.settle().await;
    let other = match case.direction {
        Direction::Upload => ended.download,
        Direction::Download | Direction::Request => ended.upload,
    };
    Ok(Answered {
        settled,
        checks: all([
            expect(
                &format!("the other direction's lifetime ended too: {other:?}"),
                other.terminal != Some(InboundTerminal::TransferTotal),
            ),
            expect("the upload never moved", ended.upload.admitted_bytes > 0),
            expect(
                "the download never moved",
                ended.download.admitted_bytes > 0,
            ),
        ]),
    })
}

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

/// Drive one row's call to the answer its peer read.
async fn driven(case: &Case, session: &mut Session) -> Result<Answered, String> {
    match case.drive {
        Drive::PreHeadBytes(limit) => pre_head_bytes(case, session, limit).await,
        Drive::PreHeadIdle => pre_head_idle(case, session).await,
        Drive::PreHeadTotal => pre_head_total(case, session).await,
        Drive::RequestTotal => request_total(case, session).await,
        Drive::RetainedUpload(retained) => retained_upload(case, session, retained).await,
        Drive::AnswerEndsRetainedUpload => answer_ends_retained_upload(case, session).await,
        Drive::CompletedUpload => completed_upload(case, session).await,
        Drive::DownloadBytes(limit) => download_bytes(case, session, limit).await,
        Drive::DownloadIdle => download_idle(case, session).await,
        Drive::DownloadTotal => download_total(case, session).await,
        Drive::UploadMovesDownloadQuiet => upload_moves_download_quiet(case, session).await,
        Drive::DownloadMovesUploadQuiet => download_moves_upload_quiet(case, session).await,
        Drive::TwoWayUploadTotal | Drive::TwoWayDownloadTotal => two_way_total(case, session).await,
    }
}

/// Run one row on a listener of its own, and settle it whatever it found.
async fn run(case: &Case) -> Row {
    let mut session = Session::open(case).await;
    let answered = match session.hold_sibling(case.sibling_hold()).await {
        Ok(()) => driven(case, &mut session).await,
        Err(reason) => Err(reason),
    };
    session.finish(case, answered).await
}

const HEAD: ResponseCommit = GRPC_HEAD;
const COMPLETED: Wire = Wire::Status(tonic::Code::Ok, None);

/// Every pre-head row: bytes, quiet interval, lifetime, and request total.
fn pre_head_cases(form: Form) -> Box<[Case]> {
    let payload = payload_len(&upload_chunks(form));
    let bytes = Limit::ALL.map(|limit| {
        let crossing = matches!(limit, Limit::Crossing);
        Case {
            family: "upload bytes before the head",
            form,
            direction: Direction::Upload,
            bound: Bound::Bytes(limit.maximum(payload)),
            stimulus: "payload offered against the maximum, tonic's head withheld",
            wire: match crossing {
                true => Wire::Mapped(413, RejectionKind::BodyLimit),
                false => COMPLETED,
            },
            terminal: match crossing {
                true => InboundTerminal::TransferBytes,
                false => InboundTerminal::ResponseHead,
            },
            commitment: match crossing {
                true => ResponseCommit::Cause(InboundTerminal::TransferBytes),
                false => HEAD,
            },
            drive: Drive::PreHeadBytes(limit),
        }
    });
    let mut cases = Vec::from(bytes);
    cases.extend([
        Case {
            family: "upload quiet interval before the head",
            form,
            direction: Direction::Upload,
            bound: Bound::Idle(QUIET),
            stimulus: "framed request left incomplete while tonic waits",
            wire: Wire::Mapped(408, RejectionKind::BodyTimeout),
            terminal: InboundTerminal::TransferIdle,
            commitment: ResponseCommit::Cause(InboundTerminal::TransferIdle),
            drive: Drive::PreHeadIdle,
        },
        Case {
            family: "upload lifetime before the head",
            form,
            direction: Direction::Upload,
            bound: Bound::Total(LIFETIME),
            stimulus: "upload kept moving while tonic waits",
            wire: Wire::Mapped(408, RejectionKind::BodyTimeout),
            terminal: InboundTerminal::TransferTotal,
            commitment: ResponseCommit::Cause(InboundTerminal::TransferTotal),
            drive: Drive::PreHeadTotal,
        },
        Case {
            family: "request total before the head",
            form,
            direction: Direction::Request,
            bound: Bound::Total(REQUEST_TOTAL),
            stimulus: "method entered, upload complete, tonic's head withheld",
            wire: Wire::Mapped(408, RejectionKind::RequestTimeout),
            terminal: InboundTerminal::RequestTotal,
            commitment: ResponseCommit::Cause(InboundTerminal::RequestTotal),
            drive: Drive::RequestTotal,
        },
    ]);
    cases.into_boxed_slice()
}

/// The retained-upload rows for one streaming-request form.
///
/// A bidirectional method forwards the failure into its own response stream,
/// so tonic's trailers carry it. A client-streaming method has already
/// answered, so the failure reaches the method and its answer keeps tonic's
/// committed `OK`.
fn retained_cases(form: Form) -> [Case; 3] {
    [Retained::Bytes, Retained::Idle, Retained::Total].map(|retained| {
        let (code, message) = typed_status(retained.error());
        Case {
            family: "retained upload after the head",
            form,
            direction: Direction::Upload,
            bound: retained.bound(),
            stimulus: match form {
                Form::ClientStreaming => {
                    "request stream left open under the head, its one reply held"
                }
                _ => "request stream left open under the committed head",
            },
            wire: match form {
                Form::ClientStreaming => COMPLETED,
                _ => Wire::Status(code, Some(message)),
            },
            terminal: retained.terminal(),
            commitment: HEAD,
            drive: Drive::RetainedUpload(retained),
        }
    })
}

/// The post-head upload rows: retained under a streaming-request head, the
/// client-streaming control whose answer outruns its upload, and a completed
/// control for the others.
fn post_head_upload_cases(form: Form) -> Box<[Case]> {
    match form {
        Form::Bidirectional => retained_cases(form).into(),
        Form::ClientStreaming => {
            let mut cases = Vec::from(retained_cases(form));
            cases.push(Case {
                family: "answer ends retained upload (control)",
                form,
                direction: Direction::Upload,
                bound: Bound::Idle(QUIET),
                stimulus: "one reply sent at once, inside the upload's quiet interval",
                wire: COMPLETED,
                terminal: InboundTerminal::Disconnect,
                commitment: HEAD,
                drive: Drive::AnswerEndsRetainedUpload,
            });
            cases.into_boxed_slice()
        }
        Form::Unary | Form::ServerStreaming => Box::new([Case {
            family: "completed upload after the head",
            form,
            direction: Direction::Upload,
            bound: Bound::Total(LIFETIME),
            stimulus: "request total and upload bounds pass under an answer still owed",
            wire: COMPLETED,
            terminal: InboundTerminal::ResponseHead,
            commitment: HEAD,
            drive: Drive::CompletedUpload,
        }]),
    }
}

/// Every download row: bytes, quiet interval, and lifetime.
fn download_cases(form: Form) -> Box<[Case]> {
    let answer = payload_len(&download_frames(form));
    let streamed = matches!(form, Form::ServerStreaming | Form::Bidirectional);
    let bytes = Limit::ALL.map(|limit| {
        let crossing = matches!(limit, Limit::Crossing);
        Case {
            family: "download bytes after the head",
            form,
            direction: Direction::Download,
            bound: Bound::Bytes(limit.maximum(answer)),
            stimulus: "replies released after the peer read the head",
            wire: match crossing {
                true => Wire::Reset,
                false => COMPLETED,
            },
            terminal: match crossing {
                true => InboundTerminal::TransferBytes,
                false => InboundTerminal::ResponseHead,
            },
            commitment: HEAD,
            drive: Drive::DownloadBytes(limit),
        }
    });
    let mut cases = Vec::from(bytes);
    cases.extend([
        Case {
            family: "download quiet interval after the head",
            form,
            direction: Direction::Download,
            bound: Bound::Idle(QUIET),
            stimulus: match streamed {
                true => "next reply withheld after the head",
                false => "single reply held at its first poll past the interval",
            },
            wire: match streamed {
                true => Wire::Reset,
                false => COMPLETED,
            },
            terminal: match streamed {
                true => InboundTerminal::TransferIdle,
                false => InboundTerminal::ResponseHead,
            },
            commitment: HEAD,
            drive: Drive::DownloadIdle,
        },
        Case {
            family: "download lifetime after the head",
            form,
            direction: Direction::Download,
            bound: Bound::Total(LIFETIME),
            stimulus: match streamed {
                true => "replies kept moving",
                false => "single reply held at its first poll past the lifetime",
            },
            wire: Wire::Reset,
            terminal: InboundTerminal::TransferTotal,
            commitment: HEAD,
            drive: Drive::DownloadTotal,
        },
    ]);
    cases.into_boxed_slice()
}

/// The bidirectional rows that hold one direction against the other.
fn independent_cases() -> [Case; 4] {
    let form = Form::Bidirectional;
    let (idle_code, idle_message) = typed_status(Retained::Idle.error());
    let (total_code, total_message) = typed_status(Retained::Total.error());
    [
        Case {
            family: "independent directions",
            form,
            direction: Direction::Download,
            bound: Bound::Idle(QUIET),
            stimulus: "only the upload moves",
            wire: Wire::Reset,
            terminal: InboundTerminal::TransferIdle,
            commitment: HEAD,
            drive: Drive::UploadMovesDownloadQuiet,
        },
        Case {
            family: "independent directions",
            form,
            direction: Direction::Upload,
            bound: Bound::Idle(QUIET),
            stimulus: "only the download moves",
            wire: Wire::Status(idle_code, Some(idle_message)),
            terminal: InboundTerminal::TransferIdle,
            commitment: HEAD,
            drive: Drive::DownloadMovesUploadQuiet,
        },
        Case {
            family: "independent directions",
            form,
            direction: Direction::Upload,
            bound: Bound::Total(LIFETIME),
            stimulus: "both directions move",
            wire: Wire::Status(total_code, Some(total_message)),
            terminal: InboundTerminal::TransferTotal,
            commitment: HEAD,
            drive: Drive::TwoWayUploadTotal,
        },
        Case {
            family: "independent directions",
            form,
            direction: Direction::Download,
            bound: Bound::Total(LIFETIME),
            stimulus: "both directions move",
            wire: Wire::Reset,
            terminal: InboundTerminal::TransferTotal,
            commitment: HEAD,
            drive: Drive::TwoWayDownloadTotal,
        },
    ]
}

/// How many rows the matrix instantiates.
///
/// Per form: six pre-head rows and five download rows. Post-head upload adds
/// three retained rows each for bidirectional and client-streaming, the
/// client-streaming control, and one completed control each for unary and
/// server-streaming. The four independent-direction rows are bidirectional
/// only.
const MATRIX_ROWS: usize = 4 * (6 + 5) + 2 * 3 + 1 + 2 + 4;

/// Every row the matrix instantiates.
fn cases() -> Box<[Case]> {
    let mut cases: Vec<Case> = Form::ALL
        .into_iter()
        .flat_map(|form| {
            pre_head_cases(form)
                .into_iter()
                .chain(post_head_upload_cases(form))
                .chain(download_cases(form))
        })
        .collect();
    cases.extend(independent_cases());
    cases.into_boxed_slice()
}

/// Run every row, each to its own teardown, and report every failure together.
async fn assert_transfer_bounds() {
    let cases = cases();
    assert_eq!(
        cases.len(),
        MATRIX_ROWS,
        "the matrix must instantiate every applicable form and phase row",
    );
    let mut failures = Vec::new();
    for case in &cases {
        let verdict = AssertUnwindSafe(run(case))
            .catch_unwind()
            .await
            .unwrap_or_else(|panic| {
                Err(format!("panicked: {}", common::panic_text(panic.as_ref())))
            });
        failures.extend(
            verdict
                .err()
                .map(|reason| format!("{}: {reason}", case.label())),
        );
    }
    assert!(
        failures.is_empty(),
        "{} of {} gRPC transfer-bound rows failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n"),
    );
}

/// 18.T1 — invariants 6 and 12.
#[test]
fn native_tonic_forms_enforce_transfer_bounds() {
    camber::runtime::builder()
        .run(|| camber::runtime::block_on(assert_transfer_bounds()))
        .expect("the gRPC transfer-bound runtime ran to completion");
}
