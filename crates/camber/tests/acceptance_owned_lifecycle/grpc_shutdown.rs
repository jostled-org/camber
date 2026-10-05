//! 19.T1: every native tonic RPC form under cancellation and shutdown.
//!
//! Each row drives one generated tonic form through Camber's own HTTP/2
//! serving and stops it at one phase. Before the head, tonic's head is produced
//! and withheld at the commit barrier with the upload complete. After the head,
//! the peer has read tonic's committed head and the download's first source
//! poll is held, so even a unary answer stays open at the transport body
//! boundary.
//!
//! Five stimuli reach that held call. A peer reset ends only its own stream: a
//! sibling held in flight on the same connection is answered after it. A
//! server cancellation, a graceful shutdown, and a shutdown whose aggregate
//! deadline expires are server-wide, so no sibling is required to outlive them.
//! The supervisor's forced abort is held until the peer has read what the
//! operation committed, so a row reads the operation's own decision, not the
//! abort behind it. The race row issues a reset and a cancellation with no
//! barrier between them and accepts either first commitment, with identical
//! cleanup.
//!
//! Every row proves the same settlement: one recorded completion, no mapper
//! invocation, every direction owner released, the server joined with the flat
//! result its stop names, and every connection permit returned. A second
//! reading after teardown adds no completion, terminal, or release. Each row
//! runs under a runtime of its own, because one runtime mints one aggregate
//! shutdown expiry and a later row would drain under what an earlier one left.
//!
//! This is regression proof over the existing lifecycle owners. It assumes no
//! defect.

use std::net::SocketAddr;
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::mock::{
    InboundTerminal, ResponseCommit, ResponseCommitmentEdge, ScopedStoppedOperation,
    ServerStopEdge, ServerStopObservation, TransferObservation, TransferOwnerEdge,
};
use camber::http::{ServerHandle, ServerPolicy};
use futures_util::FutureExt;

use crate::common;
use crate::grpc_forms::{
    ClientAnswer, Form, FormScript, GRPC_HEADERS, ReplyFeed, greeting_text, hello_frame,
    reply_frame,
};
use crate::grpc_rows::{
    BOUND, Baseline, Budgets, Forms, GRPC_HEAD, Sibling, SiblingGate, Stage, answered_with,
    eventually, open_call, permits_back, sent_whole, sibling_settled, stream_reset_under_head,
    unmapped_refusal,
};
use crate::integration_rows::{Row, all, expect, expect_eq};

/// The grace a row drains under when no call outlives it.
///
/// Longer than every bound a row waits on, so a row that waited the grace out
/// fails at its own bound first.
const DRAIN: Duration = Duration::from_secs(30);

/// The grace a forced row's held call outlives.
const EXPIRY: Duration = Duration::from_millis(300);

/// The one request message every row sends.
const REQUEST_NAME: &str = "held";

/// The reply a streamed answer releases under a graceful drain.
const STREAMED_REPLY: &str = "one";

// ---------------------------------------------------------------------------
// Row descriptions
// ---------------------------------------------------------------------------

/// Where the call is held when its stimulus arrives.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    /// tonic's head is produced and withheld at the commit barrier.
    PreHead,
    /// The peer read tonic's head; the download's first source poll is held.
    PostHead,
}

impl Phase {
    const ALL: [Self; 2] = [Self::PreHead, Self::PostHead];
}

/// What reaches the held call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Stimulus {
    /// The peer resets the call's stream with `CANCEL`.
    PeerReset,
    /// The server is cancelled while the peer stays connected.
    ServerCancel,
    /// The server stops gracefully, and the call is released inside the grace.
    Graceful,
    /// The server stops gracefully, and the call outlives the aggregate
    /// deadline.
    Forced,
    /// The peer resets the stream and the server is cancelled, unordered.
    StopResetRace,
}

impl Stimulus {
    const ALL: [Self; 5] = [
        Self::PeerReset,
        Self::ServerCancel,
        Self::Graceful,
        Self::Forced,
        Self::StopResetRace,
    ];

    /// The aggregate grace this row's runtime and server drain under.
    const fn grace(self) -> Duration {
        match self {
            Self::Forced => EXPIRY,
            Self::PeerReset | Self::ServerCancel | Self::Graceful | Self::StopResetRace => DRAIN,
        }
    }

    /// The flat result the server's join returns.
    const fn joined(self) -> Joined {
        match self {
            Self::PeerReset | Self::Graceful => Joined::Completed,
            Self::ServerCancel | Self::StopResetRace => Joined::Cancelled,
            Self::Forced => Joined::Timeout,
        }
    }

    /// The outcome the stop owner settles on.
    const fn outcome(self) -> &'static str {
        match self {
            Self::PeerReset | Self::Graceful => "completed",
            Self::ServerCancel | Self::StopResetRace => "cancelled",
            Self::Forced => "deadline-expired",
        }
    }
}

/// The flat result one server join returns.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Joined {
    Completed,
    Cancelled,
    Timeout,
}

impl Joined {
    fn of(result: &Result<(), RuntimeError>) -> Option<Self> {
        match result {
            Ok(()) => Some(Self::Completed),
            Err(RuntimeError::Cancelled) => Some(Self::Cancelled),
            Err(RuntimeError::Timeout) => Some(Self::Timeout),
            Err(_) => None,
        }
    }
}

/// What the peer must read on the row's call.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Wire {
    /// The peer withdrew its own stream before any head reached it.
    Withdrawn,
    /// Camber's unmapped head for a cause that took the commitment: this
    /// status, no gRPC status, and no mapper.
    Refused(u16),
    /// tonic's committed answer, whole, with its `OK` trailers.
    Completed,
    /// tonic's committed head stays, and this one stream is reset with no gRPC
    /// status behind it.
    Reset,
    /// tonic's committed head stays, and the forced abort takes the
    /// connection under it.
    Collapsed,
}

/// The commitment the call took, if any.
type Commitment = Option<ResponseCommit>;

const HEAD: Commitment = Some(GRPC_HEAD);
const CANCELLED: Commitment = Some(ResponseCommit::Cause(InboundTerminal::ForcedCancellation));
const EXPIRED: Commitment = Some(ResponseCommit::Cause(InboundTerminal::ShutdownDeadline));

/// The completion record's dimensions that say how the call settled.
const SETTLEMENT_FIELDS: [&str; 3] = ["delivery", "connection_end", "shutdown"];

/// How one completion record says the call settled: its delivery, its
/// connection end, and the server phase it observed, in
/// [`SETTLEMENT_FIELDS`] order.
type Settlement = [&'static str; 3];

/// The peer reset the stream before any head committed.
const RESET_BEFORE_HEAD: Settlement = ["not-committed", "stream-reset", "none"];
/// The peer reset the stream under tonic's committed head.
const RESET_UNDER_HEAD: Settlement = ["interrupted", "stream-reset", "none"];
/// The peer's reset dropped an uncommitted call after the cancellation.
const STOP_BEFORE_HEAD: Settlement = ["not-committed", "none", "cancelled"];
/// The cancellation's own refusal took the call.
const CANCEL_ANSWERED: Settlement = ["produced", "none", "cancelled"];
/// The cancellation ended the answer under tonic's committed head.
const CANCEL_UNDER_HEAD: Settlement = ["interrupted", "none", "cancelled"];
/// tonic's answer went out whole inside the grace.
const DRAINED: Settlement = ["produced", "none", "graceful"];
/// The coordinator answered the expiry while the supervisor was held.
const EXPIRY_ANSWERED: Settlement = ["produced", "none", "graceful"];
/// The forced abort behind the expiry took the committed answer.
const EXPIRY_UNDER_HEAD: Settlement = ["interrupted", "none", "deadline-expired"];

/// One matrix row.
#[derive(Clone, Copy, Debug)]
struct Case {
    stimulus: Stimulus,
    form: Form,
    phase: Phase,
}

impl Case {
    fn label(self) -> Box<str> {
        format!(
            "{:?} | {} | {:?} | {:?} | {:?}",
            self.stimulus,
            self.form.name(),
            self.phase,
            self.wire(),
            self.stimulus.joined(),
        )
        .into_boxed_str()
    }

    /// What the peer reads.
    const fn wire(self) -> Wire {
        match (self.stimulus, self.phase) {
            (Stimulus::PeerReset | Stimulus::StopResetRace, Phase::PreHead) => Wire::Withdrawn,
            (Stimulus::ServerCancel | Stimulus::Forced, Phase::PreHead) => Wire::Refused(503),
            (Stimulus::Graceful, _) => Wire::Completed,
            (Stimulus::PeerReset | Stimulus::ServerCancel | Stimulus::StopResetRace, _) => {
                Wire::Reset
            }
            (Stimulus::Forced, Phase::PostHead) => Wire::Collapsed,
        }
    }

    /// Every commitment the call may take.
    ///
    /// One member for an exact cause. The race row names both first
    /// commitments: a reset the transport saw first drops the call before any
    /// commitment, and a cancellation the coordinator read first takes it.
    const fn commitments(self) -> &'static [Commitment] {
        match (self.stimulus, self.phase) {
            (_, Phase::PostHead) | (Stimulus::Graceful, Phase::PreHead) => &[HEAD],
            (Stimulus::PeerReset, Phase::PreHead) => &[None],
            (Stimulus::ServerCancel, Phase::PreHead) => &[CANCELLED],
            (Stimulus::Forced, Phase::PreHead) => &[EXPIRED],
            (Stimulus::StopResetRace, Phase::PreHead) => &[None, CANCELLED],
        }
    }

    /// Every terminal the download owner may fix.
    ///
    /// `None` is no terminal of its own: no head committed, so no download
    /// owner existed, or the owner was dropped by the transport — the peer's
    /// reset, or the forced abort.
    const fn download_terminals(self) -> &'static [Option<InboundTerminal>] {
        match (self.stimulus, self.phase) {
            (Stimulus::Graceful, _) => &[Some(InboundTerminal::ResponseHead)],
            (_, Phase::PreHead) | (Stimulus::PeerReset | Stimulus::Forced, Phase::PostHead) => {
                &[None]
            }
            (Stimulus::ServerCancel, Phase::PostHead) => {
                &[Some(InboundTerminal::ForcedCancellation)]
            }
            (Stimulus::StopResetRace, Phase::PostHead) => {
                &[None, Some(InboundTerminal::ForcedCancellation)]
            }
        }
    }

    /// Every settlement the call's completion record may name, once the call
    /// took `committed`.
    ///
    /// A peer reset on a live connection is the cause table's stream reset, and
    /// a reset that drops the call after a stop committed is the cause table's
    /// server shutdown, which names no connection end. The race row keys its
    /// set to the commitment it took: a cancellation the coordinator answered
    /// first is recorded as that answer, and a reset the transport saw first
    /// drops the call under either cause.
    fn settlements(self, committed: Commitment) -> &'static [Settlement] {
        match (self.stimulus, self.phase) {
            (Stimulus::PeerReset, Phase::PreHead) => &[RESET_BEFORE_HEAD],
            (Stimulus::PeerReset, Phase::PostHead) => &[RESET_UNDER_HEAD],
            (Stimulus::ServerCancel, Phase::PreHead) => &[CANCEL_ANSWERED],
            (Stimulus::ServerCancel, Phase::PostHead) => &[CANCEL_UNDER_HEAD],
            (Stimulus::Graceful, _) => &[DRAINED],
            (Stimulus::Forced, Phase::PreHead) => &[EXPIRY_ANSWERED],
            (Stimulus::Forced, Phase::PostHead) => &[EXPIRY_UNDER_HEAD],
            (Stimulus::StopResetRace, Phase::PreHead) if committed == CANCELLED => {
                &[CANCEL_ANSWERED]
            }
            (Stimulus::StopResetRace, Phase::PreHead) => &[RESET_BEFORE_HEAD, STOP_BEFORE_HEAD],
            (Stimulus::StopResetRace, Phase::PostHead) => &[RESET_UNDER_HEAD, CANCEL_UNDER_HEAD],
        }
    }

    /// How many download owners the call created and released.
    ///
    /// A call whose head never committed has none.
    fn download_owners(self) -> usize {
        usize::from(self.commitments() == [HEAD])
    }

    /// Whether a sibling stream must outlive this row's failure.
    ///
    /// Only a peer reset ends one stream alone. Every stop is server-wide, so
    /// a sibling there would participate in the shutdown.
    const fn sibling(self) -> bool {
        matches!(self.stimulus, Stimulus::PeerReset)
    }

    /// The bytes tonic's whole answer carries.
    fn answer_bytes(self) -> usize {
        match self.form {
            Form::Unary | Form::ClientStreaming => reply_frame(&greeting_text(REQUEST_NAME)).len(),
            Form::ServerStreaming | Form::Bidirectional => reply_frame(STREAMED_REPLY).len(),
        }
    }
}

// ---------------------------------------------------------------------------
// The served row
// ---------------------------------------------------------------------------

/// What the peer read on the row's call.
#[derive(Debug)]
enum Read {
    /// The stream ended before any head reached the peer.
    Withdrawn,
    /// A head reached a peer that had withdrawn its stream.
    Unwithdrawn { status: u16 },
    /// The stream ended on an error other than the peer's own withdrawal.
    Faulted(Box<str>),
    /// A head, and how what followed it ended.
    Settled(common::H2Settled),
    /// A head, and the connection taken under it.
    Collapsed { status: u16 },
}

/// What a row's call ended on, and the row's own checks.
struct Answered {
    read: Read,
    checks: Row,
}

/// One row's server, owners, script, mapper record, and connection.
struct Session {
    case: Case,
    label: Box<str>,
    addr: SocketAddr,
    /// Taken by the join, so the server is joined exactly once.
    handle: Option<ServerHandle>,
    owners: Arc<ScopedStoppedOperation>,
    script: Arc<FormScript>,
    feed: ReplyFeed,
    journal: common::Journal,
    client: common::PersistentH2Client,
    gate: Arc<SiblingGate>,
    /// The sibling, open from before the row's call until its answer is read.
    sibling: Option<Sibling>,
    /// The one aggregate expiry the row's graceful commit minted, if it made
    /// one. A cancellation mints none.
    minted: Option<tokio::time::Instant>,
    before: Baseline,
    /// The completion events recorded on the call's form path.
    records: common::TraceCapture,
}

impl Session {
    async fn open(case: Case) -> Self {
        let label = case.label();
        // No bound of the call's own competes with the stop.
        let (forms, router) = Forms::router(
            ClientAnswer::AfterUpload,
            Budgets::unbounded(),
            "grpc-shutdown",
        );
        let policy = ServerPolicy::default()
            .shutdown_timeout(case.stimulus.grace())
            .expect("every row grace is a valid shutdown timeout");
        let port = common::reserve_stopped_operation();
        let owners = port.controller();
        let server = port.serve_with_policy(router, policy);
        let addr = server.addr();
        let handle = server.into_handle();
        let before = forms.baseline(&owners).await;
        let Forms {
            script,
            feed,
            journal,
            gate,
        } = forms;
        // Only the call travels its form's path: the probe and the sibling
        // have paths of their own.
        let records = common::capture_events(&format!("path={}", case.form.path()));
        let client = common::PersistentH2Client::connect(addr, BOUND).await;
        Self {
            case,
            label,
            addr,
            handle: Some(handle),
            owners,
            script,
            feed,
            journal,
            client,
            gate,
            sibling: None,
            minted: None,
            before,
            records,
        }
    }

    /// The checkpoints this row steps its call's owners through.
    fn stage(&self) -> Stage<'_, ScopedStoppedOperation> {
        Stage {
            owners: &self.owners,
            label: &self.label,
        }
    }

    fn transfers(&self) -> TransferObservation {
        self.owners.transfers.observed()
    }

    fn stop(&self) -> ServerStopObservation {
        self.owners.stop.observed()
    }

    fn server(&self) -> Result<&ServerHandle, String> {
        self.handle
            .as_ref()
            .ok_or_else(|| "the server was already joined".into())
    }

    /// Open this row's call on its form's path, its body still owed.
    async fn call(&mut self, form: Form) -> common::H2RequestStream {
        open_call(&mut self.client, form).await
    }

    /// Wait until the call's completion is recorded.
    async fn call_recorded(&self) -> Row {
        self.before.call_recorded(&self.owners).await
    }

    /// A new call on the row's own connection is refused, and nothing new is
    /// admitted.
    ///
    /// The connection outlives the closed listener because it carries the held
    /// call, so a drain that still took streams on it would keep taking work.
    async fn new_stream_refused(&mut self) -> Row {
        let refused = self
            .client
            .refused_after_goaway("POST", self.case.form.path(), "localhost", &GRPC_HEADERS)
            .await;
        all([
            refused.map_err(String::from),
            expect_eq(
                "operations admitted after the drain began",
                self.owners.commitment.operations_observed().admitted - self.before.admitted,
                1,
            ),
        ])
    }

    /// The held call is still owned: no completion recorded and no download
    /// owner released, whichever phase holds it.
    ///
    /// tonic has already returned its response: its head is produced, and a
    /// unary or client-streaming answer is whole. What this reads is Camber's
    /// ownership through the last frame and trailer, not tonic's return.
    fn still_owned(&self) -> Row {
        let operations = self.owners.commitment.operations_observed();
        all([
            expect_eq(
                "completions recorded while the call is held",
                operations.completions_recorded,
                self.before.recorded,
            ),
            expect_eq(
                "download owners released while the call is held",
                self.transfers().download.releases,
                0,
            ),
        ])
    }

    /// Send the whole request and hold tonic's produced head at the commit
    /// barrier, with the upload ended.
    async fn head_held(&self, form: Form, stream: &mut common::H2RequestStream) -> Row {
        let stage = self.stage();
        let head = ResponseCommitmentEdge::GrpcHeadReady;
        stage.arm(head);
        sent_whole(stream, &[hello_frame(REQUEST_NAME)]).await?;
        stage.paused(head).await;
        all([
            stage.upload_ended().await,
            expect_eq("method entries", self.script.entered(form), 1),
        ])
    }

    /// Release the held call's answer, and the streamed reply behind it.
    fn release_answer(&mut self) -> Row {
        self.release_held_call();
        match self.case.form {
            Form::ServerStreaming | Form::Bidirectional => {
                let released = expect(
                    "the streamed reply found no response stream",
                    self.feed.reply(STREAMED_REPLY),
                );
                self.feed.end();
                released
            }
            Form::Unary | Form::ClientStreaming => Ok(()),
        }
    }

    /// Let go of the hold the call's phase left on it: the head held at the
    /// barrier, or the first DATA held at its poll.
    fn release_held_call(&self) {
        match self.case.phase {
            Phase::PreHead => self.stage().release(ResponseCommitmentEdge::GrpcHeadReady),
            Phase::PostHead => self.stage().release(TransferOwnerEdge::BeforeSourcePoll),
        }
    }

    /// Settle the row: the call, the sibling, the join, the permits, and a
    /// second reading.
    async fn finish(mut self, answered: Result<Answered, String>) -> Row {
        let mapped = common::drain(&self.journal).len();
        let verdict = answered.and_then(|answered| {
            all([
                answered.checks,
                wire_checks(self.case, &answered.read),
                expect_eq("mapper invocations", mapped, 0),
            ])
        });
        let call = self.call_settled().await;
        let sibling = self.sibling().await;
        let teardown = self.tear_down().await;
        all([verdict, call, sibling, teardown])
    }

    /// One admission, one completion, the permitted commitment, and every
    /// direction owner released with a permitted terminal.
    ///
    /// A sibling is admitted before the call and has neither committed nor
    /// completed yet, so it counts once among the admissions only.
    async fn call_settled(&self) -> Row {
        let case = self.case;
        let recorded = self.call_recorded().await;
        let downloads = case.download_owners();
        let released = self.stage().owners_released(downloads).await;
        let operations = self.owners.commitment.operations_observed();
        let commitment = self.owners.commitment.observed();
        let committed = match commitment.commits - self.before.commits {
            0 => Ok(None),
            1 => Ok(commitment.committed),
            commits => Err(format!("the call took {commits} commitments")),
        };
        let transfers = self.transfers();
        let record = self.record().await;
        all([
            recorded,
            expect_eq(
                "admitted operations",
                operations.admitted - self.before.admitted,
                1 + usize::from(case.sibling()),
            ),
            expect_eq(
                "recorded completions",
                operations.completions_recorded - self.before.recorded,
                1,
            ),
            committed.and_then(|committed| {
                all([
                    expect(
                        &format!(
                            "commitment {committed:?} is not one of {:?}",
                            case.commitments()
                        ),
                        case.commitments().contains(&committed),
                    ),
                    record.and_then(|record| {
                        settled_as(&record, case.settlements(committed))
                            .map_err(|reason| format!("after commitment {committed:?}: {reason}"))
                    }),
                ])
            }),
            released,
            expect_eq(
                "upload terminal",
                transfers.upload.terminal,
                Some(InboundTerminal::ResponseHead),
            ),
            expect(
                &format!(
                    "download terminal {:?} is not one of {:?}",
                    transfers.download.terminal,
                    case.download_terminals()
                ),
                case.download_terminals()
                    .contains(&transfers.download.terminal),
            ),
        ])
    }

    /// The call's one completion record.
    ///
    /// Read off the record production published, not off an owner probe: the
    /// record is what an operator reads to tell a peer's reset from a server
    /// stop.
    async fn record(&self) -> Result<Box<str>, String> {
        let _published = eventually(|| {
            !common::events_saying(&self.records.events(), common::COMPLETION_MESSAGE).is_empty()
        })
        .await;
        let events = self.records.events();
        match common::events_saying(&events, common::COMPLETION_MESSAGE).as_ref() {
            [record] => Ok(Box::from(*record)),
            records => Err(format!(
                "the call left {} completion records: {records:?}",
                records.len()
            )),
        }
    }

    /// Release the sibling held across a reset, and require its answer.
    async fn sibling(&mut self) -> Row {
        match (self.case.sibling(), self.sibling.take()) {
            (false, None) => Ok(()),
            (false, Some(_)) => Err("a stop row opened a sibling".into()),
            (true, sibling) => {
                sibling_settled(
                    sibling,
                    &self.gate,
                    &self.owners,
                    &self.before,
                    &self.journal,
                )
                .await
            }
        }
    }

    /// Join the server, close the connection, and require every permit back
    /// and a second reading unchanged.
    ///
    /// A reset row's server is still running, so its connection is closed
    /// first and the server is stopped gracefully. A stop row's server was
    /// stopped by the row, so it is joined first and the connection read as
    /// its server ended it.
    async fn tear_down(self) -> Row {
        let Self {
            case,
            handle,
            owners,
            script,
            client,
            minted,
            ..
        } = self;
        let expected = case.stimulus.joined();
        let (joined, closed) = match case.stimulus {
            Stimulus::PeerReset => {
                client.close().await;
                let joined = joined_within(handle, Some(ServerHandle::shutdown), expected).await;
                (joined, Ok(()))
            }
            Stimulus::ServerCancel
            | Stimulus::Graceful
            | Stimulus::Forced
            | Stimulus::StopResetRace => {
                let joined = joined_within(handle, None, expected).await;
                (joined, client.close_stopped().await)
            }
        };
        let returned = permits_back(&owners).await;
        let reader = script.join_reader(BOUND).await;
        let settled = owners.stop.observed();
        let first = Reading::of(&owners);
        tokio::time::sleep(common::POLL_INTERVAL * 4).await;
        let second = Reading::of(&owners);
        all([
            joined,
            closed.map_err(String::from),
            returned,
            reader,
            expect_eq("stop outcome", settled.outcome, case.stimulus.outcome()),
            unrestarted(case, minted, &settled),
            expect_eq("a second reading after teardown", second, first),
        ])
    }
}

/// The joined server kept the one aggregate expiry its stop committed.
///
/// No escalation, expiry, or settlement mints a second grace, and a
/// cancellation mints none at all. A reset row's server is stopped by its own
/// teardown, which states nothing about the row.
fn unrestarted(
    case: Case,
    minted: Option<tokio::time::Instant>,
    settled: &ServerStopObservation,
) -> Row {
    match case.stimulus {
        Stimulus::PeerReset => Ok(()),
        Stimulus::ServerCancel
        | Stimulus::Graceful
        | Stimulus::Forced
        | Stimulus::StopResetRace => expect_eq(
            "aggregate deadline after the join",
            settled.aggregate_deadline,
            minted,
        ),
    }
}

/// Join `handle` under [`BOUND`], issuing `stop` first when given, and require
/// the flat result `expected` names.
async fn joined_within(
    handle: Option<ServerHandle>,
    stop: Option<fn(&ServerHandle)>,
    expected: Joined,
) -> Row {
    let Some(handle) = handle else {
        return Err("the server was already joined".into());
    };
    match stop {
        Some(stop) => stop(&handle),
        None => {}
    }
    match tokio::time::timeout(BOUND, handle).await {
        Ok(result) => expect_eq("joined result", Joined::of(&result), Some(expected))
            .map_err(|reason| format!("{reason}: {result:?}")),
        // The handle went with the timeout; dropping it forces the stop.
        Err(_) => Err("the server did not join within its bound".into()),
    }
}

/// The settled counts a teardown reads twice.
#[derive(Debug, Eq, PartialEq)]
struct Reading {
    completions: usize,
    commits: usize,
    upload: (usize, Option<InboundTerminal>, usize),
    download: (usize, Option<InboundTerminal>, usize),
    connection_events: usize,
}

impl Reading {
    fn of(owners: &ScopedStoppedOperation) -> Self {
        let transfers = owners.transfers.observed();
        Self {
            completions: owners.commitment.operations_observed().completions_recorded,
            commits: owners.commitment.observed().commits,
            upload: (
                transfers.upload.releases,
                transfers.upload.terminal,
                transfers.upload.terminals,
            ),
            download: (
                transfers.download.releases,
                transfers.download.terminal,
                transfers.download.terminals,
            ),
            connection_events: owners.connections.observed().events.len(),
        }
    }
}

/// The record settled as one of `permitted`.
fn settled_as(record: &str, permitted: &[Settlement]) -> Row {
    let read = SETTLEMENT_FIELDS.map(|field| common::field_value(record, field));
    expect(
        &format!("the record settled as {read:?}, not one of {permitted:?}: {record}"),
        permitted
            .iter()
            .any(|settlement| settlement.map(Some) == read),
    )
}

/// What the peer read, against what the row's wire result names.
fn wire_checks(case: Case, read: &Read) -> Row {
    match (case.wire(), read) {
        (Wire::Withdrawn, Read::Withdrawn) => Ok(()),
        (Wire::Withdrawn, Read::Unwithdrawn { status }) => Err(format!(
            "a {status} head reached the peer after it withdrew its stream"
        )),
        (Wire::Refused(status), Read::Settled(settled)) => all([
            unmapped_refusal(settled, status),
            expect_eq("refusal body bytes", settled.bytes, 0),
        ]),
        (Wire::Completed, Read::Settled(settled)) => all([
            answered_with(settled, tonic::Code::Ok),
            expect_eq("delivered bytes", settled.bytes, case.answer_bytes()),
        ]),
        (Wire::Reset, Read::Settled(settled)) => all([
            stream_reset_under_head(settled),
            expect_eq("delivered bytes", settled.bytes, 0),
        ]),
        (Wire::Collapsed, Read::Collapsed { status }) => {
            expect_eq("committed status", *status, 200)
        }
        (wire, Read::Faulted(error)) => Err(format!(
            "the peer's stream faulted with {error} where {wire:?} was owed"
        )),
        (wire, read) => Err(format!("the peer read {read:?} where {wire:?} was owed")),
    }
}

// ---------------------------------------------------------------------------
// Stimuli
// ---------------------------------------------------------------------------

/// The call as its phase left it: no head yet, or the head read.
enum Staged {
    Pending,
    Committed(common::H2ReadHalf),
}

/// The peer's own reset, before the head or under it.
async fn peer_reset(
    session: &mut Session,
    stream: &mut common::H2RequestStream,
    staged: Staged,
) -> Result<Answered, String> {
    stream.reset();
    match staged {
        Staged::Pending => {
            let (read, recorded) = withdrawn_and_recorded(session, stream).await;
            Ok(Answered {
                read,
                checks: recorded,
            })
        }
        Staged::Committed(read) => {
            // The transport drops the held answer on the reset.
            let dropped = eventually(|| session.transfers().download.releases == 1).await;
            session.release_held_call();
            Ok(Answered {
                read: Read::Settled(read.settle().await),
                checks: expect("the reset never released the held answer", dropped),
            })
        }
    }
}

/// A cancellation committed while the peer stays connected.
///
/// The supervisor is held where it selected the cancellation until the peer
/// has read what the operation committed, so its forced abort cannot take the
/// connection first.
async fn server_cancel(
    session: &mut Session,
    stream: &mut common::H2RequestStream,
    staged: Staged,
) -> Result<Answered, String> {
    let control = ServerStopEdge::SupervisorSelectedControl;
    session.stage().arm(control);
    let commanded = cancel_twice(session);
    let read = match staged {
        Staged::Pending => {
            let settled = stream.commit().await.settle().await;
            session.release_held_call();
            settled
        }
        Staged::Committed(read) => {
            session.release_held_call();
            read.settle().await
        }
    };
    session.stage().paused(control).await;
    session.stage().release(control);
    Ok(Answered {
        read: Read::Settled(read),
        checks: commanded,
    })
}

/// Cancel the server twice, and require one committed cancellation.
fn cancel_twice(session: &Session) -> Row {
    let server = session.server()?;
    server.cancel();
    let commanded = session.stop();
    server.cancel();
    let repeated = session.stop();
    all([
        expect_eq("phase after cancel", commanded.phase, "cancelled"),
        expect(
            "cancel did not record its caller",
            commanded.cancel_commanded,
        ),
        expect_eq("cancel commits", commanded.commits, 1),
        expect_eq("repeated cancel commits", repeated.commits, 1),
    ])
}

/// A graceful stop that closes admission, then releases the accepted call
/// inside its grace.
///
/// The supervisor is held past its graceful transition, where the listener is
/// already given up, while admission is probed.
async fn graceful(
    session: &mut Session,
    stream: &mut common::H2RequestStream,
    staged: Staged,
) -> Result<Answered, String> {
    let control = ServerStopEdge::SupervisorSelectedControl;
    let select = ServerStopEdge::BeforeSupervisorSelect;
    session.stage().arm(control);
    session.server()?.shutdown();
    let committed = session.stop();
    session.minted = committed.aggregate_deadline;
    session.stage().paused(control).await;
    session.stage().arm(select);
    session.stage().release(control);
    session.stage().paused(select).await;
    let admission = AssertUnwindSafe(common::assert_admission_closed(session.addr, BOUND))
        .catch_unwind()
        .await
        .map_err(|panic| {
            format!(
                "admission stayed open after the graceful transition: {}",
                common::panic_text(panic.as_ref())
            )
        });
    let refused = session.new_stream_refused().await;
    session.stage().release(select);
    let released = session.release_answer();
    let read = match staged {
        Staged::Pending => stream.commit().await,
        Staged::Committed(read) => read,
    };
    let settled = read.settle().await;
    Ok(Answered {
        read: Read::Settled(settled),
        checks: all([
            expect_eq("phase after shutdown", committed.phase, "graceful"),
            expect(
                "the graceful commit minted no aggregate deadline",
                committed.aggregate_deadline.is_some(),
            ),
            admission,
            refused,
            released,
        ]),
    })
}

/// A graceful stop whose aggregate deadline the held call outlives.
///
/// The supervisor is held where it selected the expiry. Before the head, the
/// operation's coordinator reads the same expiry and answers it. After the
/// head, the held poll keeps the answer from moving, so the forced abort the
/// held expiry escalates to is what ends the call.
async fn forced(
    session: &mut Session,
    stream: &mut common::H2RequestStream,
    staged: Staged,
) -> Result<Answered, String> {
    let expiry = ServerStopEdge::SupervisorSelectedDeadline;
    session.stage().arm(expiry);
    session.server()?.shutdown();
    let minted = session.stop().aggregate_deadline;
    session.minted = minted;
    session.stage().paused(expiry).await;
    let read = match staged {
        Staged::Pending => {
            let settled = stream.commit().await.settle().await;
            session.release_held_call();
            session.stage().release(expiry);
            Read::Settled(settled)
        }
        Staged::Committed(read) => {
            session.stage().release(expiry);
            let status = read.status();
            let collapsed = read.settle_or_collapse().await;
            session.release_held_call();
            collapsed.map_or_else(|_collapse| Read::Collapsed { status }, Read::Settled)
        }
    };
    Ok(Answered {
        read,
        checks: expect(
            "the graceful commit minted no aggregate deadline",
            minted.is_some(),
        ),
    })
}

/// A peer reset and a server cancellation, issued with no barrier between.
///
/// The supervisor's abort is held until the call settles either way.
async fn stop_reset_race(
    session: &mut Session,
    stream: &mut common::H2RequestStream,
    staged: Staged,
) -> Result<Answered, String> {
    let control = ServerStopEdge::SupervisorSelectedControl;
    session.stage().arm(control);
    stream.reset();
    session.server()?.cancel();
    let (read, checks) = match staged {
        Staged::Pending => withdrawn_and_recorded(session, stream).await,
        Staged::Committed(read) => {
            session.release_held_call();
            (Read::Settled(read.settle().await), Ok(()))
        }
    };
    // Released whatever the call read, so the join is not left behind a held
    // supervisor.
    session.stage().paused(control).await;
    session.stage().release(control);
    Ok(Answered { read, checks })
}

/// Read a stream the peer withdrew before its head, wait for the call's
/// completion, then let go of the head held at the barrier.
async fn withdrawn_and_recorded(
    session: &Session,
    stream: &mut common::H2RequestStream,
) -> (Read, Row) {
    let read = withdrawn(stream).await;
    let recorded = session.call_recorded().await;
    session.release_held_call();
    (read, recorded)
}

/// Read a stream the peer withdrew: no head may follow its reset, and the
/// stream must end on that reset alone.
async fn withdrawn(stream: &mut common::H2RequestStream) -> Read {
    match stream.try_answer().await {
        Ok(answer) => Read::Unwithdrawn {
            status: answer.status,
        },
        // The peer's own `CANCEL` is recorded locally before anything the
        // server sends, so even the race row reads it rather than a server
        // reset or GOAWAY.
        Err(error) if error.reason() == Some(h2::Reason::CANCEL) && !error.is_remote() => {
            Read::Withdrawn
        }
        Err(error) => Read::Faulted(format!("{error:?}").into_boxed_str()),
    }
}

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

/// Stage the call at its phase, then deliver the row's stimulus.
async fn driven(case: Case, session: &mut Session) -> Result<Answered, String> {
    let mut stream = session.call(case.form).await;
    let staged = match case.phase {
        Phase::PreHead => {
            session.head_held(case.form, &mut stream).await?;
            Staged::Pending
        }
        Phase::PostHead => {
            let request = [hello_frame(REQUEST_NAME)];
            let read = session.stage().staged_reply(&mut stream, &request).await?;
            expect_eq("method entries", session.script.entered(case.form), 1)?;
            Staged::Committed(read)
        }
    };
    session.still_owned()?;
    match case.stimulus {
        Stimulus::PeerReset => peer_reset(session, &mut stream, staged).await,
        Stimulus::ServerCancel => server_cancel(session, &mut stream, staged).await,
        Stimulus::Graceful => graceful(session, &mut stream, staged).await,
        Stimulus::Forced => forced(session, &mut stream, staged).await,
        Stimulus::StopResetRace => stop_reset_race(session, &mut stream, staged).await,
    }
}

/// Run one row on a server of its own, and settle it whatever it found.
async fn run(case: Case) -> Row {
    let mut session = Session::open(case).await;
    let sibling = match case.sibling() {
        true => Sibling::held_in_handler(&mut session.client, &session.gate)
            .await
            .map(|sibling| session.sibling = Some(sibling)),
        false => Ok(()),
    };
    let answered = match sibling {
        Ok(()) => driven(case, &mut session).await,
        Err(reason) => Err(reason),
    };
    session.finish(answered).await
}

/// Run one row under a runtime of its own, on a thread of its own.
///
/// The runtime's aggregate grace is the row's, so a stop measures the whole
/// deadline it names. A panic is the row's failure, not the matrix's.
fn isolated(case: Case) -> Row {
    let row = std::thread::spawn(move || {
        // Traced, so the call's completion record is published as an event.
        camber::runtime::builder()
            .with_tracing()
            .shutdown_timeout(case.stimulus.grace())
            .run(|| camber::runtime::block_on(run(case)))
    })
    .join();
    match row {
        Ok(Ok(verdict)) => verdict,
        Ok(Err(error)) => Err(format!("the row's runtime failed: {error:?}")),
        Err(panic) => Err(format!("panicked: {}", common::panic_text(panic.as_ref()))),
    }
}

/// How many rows the matrix instantiates: every stimulus, form, and phase.
const MATRIX_ROWS: usize = 5 * 4 * 2;

/// Every row the matrix instantiates.
fn cases() -> Box<[Case]> {
    Stimulus::ALL
        .into_iter()
        .flat_map(|stimulus| {
            Form::ALL.into_iter().flat_map(move |form| {
                Phase::ALL.map(|phase| Case {
                    stimulus,
                    form,
                    phase,
                })
            })
        })
        .collect()
}

/// 19.T1 — invariants 6, 8, and 12.
#[test]
fn native_tonic_forms_settle_cancellation_and_shutdown() {
    let cases = cases();
    assert_eq!(
        cases.len(),
        MATRIX_ROWS,
        "the matrix must instantiate every stimulus, form, and phase",
    );
    let failures: Box<[String]> = cases
        .iter()
        .filter_map(|case| {
            isolated(*case)
                .err()
                .map(|reason| format!("{}: {reason}", case.label()))
        })
        .collect();
    assert!(
        failures.is_empty(),
        "{} of {} gRPC shutdown rows failed:\n{}",
        failures.len(),
        cases.len(),
        failures.join("\n"),
    );
}
