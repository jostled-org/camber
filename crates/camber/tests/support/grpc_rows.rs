//! The served-row controls the native gRPC lifecycle matrices share.
//!
//! Steps 18 and 19 drive the same four tonic forms through one Camber listener
//! per row. Both read the same settlement — a baseline taken after the
//! readiness probe, one recorded completion, a held sibling, and every
//! connection permit returned — and both stage tonic's head the same way. The
//! staging steps only through owner checkpoints: a hold observes a commitment,
//! and a release lets production decide.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use camber::http::mock::{
    ConnectionOwnerController, ConnectionOwnershipEvent, ConnectionOwnershipObservation,
    InboundTerminal, ResponseCommit, ResponseCommitmentController, ResponseCommitmentEdge,
    ResponseOrigin, TransferObservation, TransferOwnerController, TransferOwnerEdge,
};
use camber::http::{
    GrpcRouter, Request, RequestBudget, Response, Router, StreamResponse, TransferBudget,
};

use crate::common;
use crate::grpc_forms::{ClientAnswer, Form, FormScript, GRPC_HEADERS, ReplyFeed, ScriptedForms};
use crate::integration_rows::{Row, all, expect, expect_eq};

/// How long one exchange, wait, or teardown may take.
pub const BOUND: Duration = Duration::from_secs(10);

/// The plain route whose handler holds the sibling's answer.
pub const SIBLING_HANDLER_PATH: &str = "/sibling/handler";
/// The streamed route whose committed head holds the sibling's body.
pub const SIBLING_STREAM_PATH: &str = "/sibling/stream";
/// The body both sibling routes answer with.
pub const SIBLING_BODY: &str = "sibling";

/// The commitment tonic's own head takes at the handoff.
pub const GRPC_HEAD: ResponseCommit = ResponseCommit::Head(ResponseOrigin::Grpc);

/// Poll `ready` until it holds or [`BOUND`] passes.
pub async fn eventually(mut ready: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + BOUND;
    loop {
        if ready() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(common::POLL_INTERVAL).await;
    }
}

/// What one listener had settled before a row drove its call.
pub struct Baseline {
    pub admitted: usize,
    pub recorded: usize,
    pub commits: usize,
}

/// Wait until every operation the listener admitted has been recorded.
///
/// The readiness probe is recorded at its own body's end, which can trail the
/// probe's return; a baseline read before it would count the probe as a row's.
async fn settled_baseline(owners: &impl common::Owns<ResponseCommitmentController>) -> Baseline {
    let commitment = owners.owner();
    let settled = eventually(|| {
        let operations = commitment.operations_observed();
        operations.completions_recorded == operations.admitted
    })
    .await;
    assert!(
        settled,
        "the listener's admitted operations were not all recorded within {BOUND:?}"
    );
    let operations = commitment.operations_observed();
    Baseline {
        admitted: operations.admitted,
        recorded: operations.completions_recorded,
        commits: commitment.observed().commits,
    }
}

impl Baseline {
    /// How many completions were recorded past this baseline.
    pub fn completions_since(
        &self,
        owners: &impl common::Owns<ResponseCommitmentController>,
    ) -> usize {
        owners.owner().operations_observed().completions_recorded - self.recorded
    }

    /// Wait until the row's call recorded a completion past this baseline.
    pub async fn call_recorded(
        &self,
        owners: &impl common::Owns<ResponseCommitmentController>,
    ) -> Row {
        let recorded = eventually(|| self.completions_since(owners) > 0).await;
        expect("the call's completion was never recorded", recorded)
    }
}

/// The budgets one row's router freezes.
#[derive(Clone, Copy)]
pub struct Budgets {
    pub request: RequestBudget,
    pub upload: TransferBudget,
    pub download: TransferBudget,
}

impl Budgets {
    pub fn unbounded() -> Self {
        Self {
            request: RequestBudget::unbounded(),
            upload: TransferBudget::unbounded(),
            download: TransferBudget::unbounded(),
        }
    }

    pub const fn with_request(self, request: RequestBudget) -> Self {
        Self { request, ..self }
    }

    pub const fn with_upload(self, upload: TransferBudget) -> Self {
        Self { upload, ..self }
    }

    pub const fn with_download(self, download: TransferBudget) -> Self {
        Self { download, ..self }
    }
}

/// The scripted forms, sibling gate, and mapper record one row's router
/// serves.
pub struct Forms {
    pub script: Arc<FormScript>,
    pub feed: ReplyFeed,
    pub journal: common::Journal,
    pub gate: Arc<SiblingGate>,
}

impl Forms {
    /// A router with both sibling routes and the scripted forms, under
    /// `budgets`, refusing through a mapper recorded as `origin`.
    pub fn router(answer: ClientAnswer, budgets: Budgets, origin: &'static str) -> (Self, Router) {
        let journal = common::journal();
        let (script, feed) = FormScript::new(answer);
        let gate = Arc::new(SiblingGate::default());
        let mut router = Router::new();
        SiblingGate::route(&gate, &mut router);
        router.grpc(GrpcRouter::new().add_service(ScriptedForms::serve(&script)));
        let router = router
            .request_budget(budgets.request)
            .upload_budget(budgets.upload)
            .download_budget(budgets.download)
            .rejection_mapper(common::recording_mapper(&journal, origin));
        let forms = Self {
            script,
            feed,
            journal,
            gate,
        };
        (forms, router)
    }

    /// The listener's settled baseline, with the readiness probe's refusal
    /// taken off the mapper record.
    ///
    /// The readiness probe is an operation like any other, and its refusal
    /// went through this mapper. Nothing a row reads may count it.
    pub async fn baseline(
        &self,
        owners: &impl common::Owns<ResponseCommitmentController>,
    ) -> Baseline {
        let before = settled_baseline(owners).await;
        let _probe = common::drain(&self.journal);
        before
    }
}

/// Whether every connection and request the listener admitted has settled.
fn permits_returned(observed: &ConnectionOwnershipObservation) -> bool {
    observed.events.iter().all(|event| match *event {
        ConnectionOwnershipEvent::ServerConnectionRegistered { connection } => {
            observed.contains(ConnectionOwnershipEvent::ServerConnectionSettled { connection })
        }
        ConnectionOwnershipEvent::ConnectionRequestAdmitted {
            connection,
            request,
        } => observed.contains(ConnectionOwnershipEvent::ConnectionRequestSettled {
            connection,
            request,
        }),
        _ => true,
    })
}

/// Wait until every connection permit the listener admitted came back.
pub async fn permits_back(owners: &impl common::Owns<ConnectionOwnerController>) -> Row {
    let connections = owners.owner();
    let returned = eventually(|| permits_returned(&connections.observed())).await;
    expect(
        &format!(
            "the connection permit never came back: {:?}",
            connections.observed().events
        ),
        returned,
    )
}

/// Camber's unmapped refusal head: `status`, no gRPC status, no reset.
pub fn unmapped_refusal(settled: &common::H2Settled, status: u16) -> Row {
    all([
        expect_eq("refusal status", settled.status, status),
        expect_eq("gRPC status header", settled.header("grpc-status"), None),
        expect_eq("gRPC status trailer", settled.trailer("grpc-status"), None),
        expect_eq("stream reset", settled.reset, false),
    ])
}

/// tonic's committed head, with this one stream reset and no gRPC status
/// behind it.
pub fn stream_reset_under_head(settled: &common::H2Settled) -> Row {
    all([
        expect_eq("committed status", settled.status, 200),
        expect_eq("stream reset", settled.reset, true),
        expect_eq("gRPC status trailer", settled.trailer("grpc-status"), None),
    ])
}

/// tonic's committed answer, whole, with `code` in its trailers.
pub fn answered_with(settled: &common::H2Settled, code: tonic::Code) -> Row {
    all([
        expect_eq("committed status", settled.status, 200),
        expect_eq("stream reset", settled.reset, false),
        expect_eq(
            "trailer code",
            trailer_status(settled).as_ref().map(tonic::Status::code),
            Some(code),
        ),
    ])
}

/// The status one committed answer carried in its trailers.
pub fn trailer_status(settled: &common::H2Settled) -> Option<tonic::Status> {
    let mut trailers = ::http::HeaderMap::new();
    for (name, value) in &settled.trailers {
        let name = ::http::HeaderName::from_bytes(name.as_bytes()).ok()?;
        let value = ::http::HeaderValue::from_str(value).ok()?;
        trailers.append(name, value);
    }
    tonic::Status::from_header_map(&trailers)
}

/// Offer `chunk`, requiring the peer to take it.
pub async fn sent(stream: &mut common::H2RequestStream, chunk: &[u8]) -> Row {
    expect_eq(
        "offered chunk",
        stream.offer(chunk, BOUND).await,
        common::H2Offer::Sent,
    )
}

/// Send every chunk of a whole request, requiring the peer to take each, then
/// end it.
pub async fn sent_whole(stream: &mut common::H2RequestStream, chunks: &[Box<[u8]>]) -> Row {
    for chunk in chunks {
        sent(stream, chunk).await?;
    }
    stream.finish();
    Ok(())
}

/// Open a call on `form`'s path, its body still owed.
pub async fn open_call(
    client: &mut common::PersistentH2Client,
    form: Form,
) -> common::H2RequestStream {
    client
        .open_paced("POST", form.path(), "localhost", &GRPC_HEADERS)
        .await
}

/// The test's release for the sibling's held answer.
#[derive(Default)]
pub struct SiblingGate {
    entered: AtomicBool,
    release: tokio::sync::Notify,
}

impl SiblingGate {
    /// Register both sibling routes on `router`, each waiting on this gate.
    pub fn route(gate: &Arc<Self>, router: &mut Router) {
        let held = Arc::clone(gate);
        router.get(SIBLING_HANDLER_PATH, move |_req: &Request| {
            let gate = Arc::clone(&held);
            async move {
                gate.entered.store(true, Ordering::SeqCst);
                gate.release.notified().await;
                Response::text(200, SIBLING_BODY)
            }
        });
        let streamed = Arc::clone(gate);
        router.get_stream(SIBLING_STREAM_PATH, move |_req: &Request| {
            let gate = Arc::clone(&streamed);
            Box::pin(async move {
                let (response, sender) = StreamResponse::new(200);
                tokio::spawn(async move {
                    gate.release.notified().await;
                    // A sibling the row's failure took has no reader left;
                    // its peer reports that.
                    let _sent = sender.send(SIBLING_BODY).await;
                });
                response
            })
        });
    }

    /// Whether the sibling's handler has been entered.
    pub fn entered(&self) -> bool {
        self.entered.load(Ordering::SeqCst)
    }

    /// Let the held sibling answer.
    ///
    /// Remembered when nothing waits yet, so the order of the two cannot
    /// strand the sibling.
    pub fn release(&self) {
        self.release.notify_one();
    }
}

/// The sibling's stream, as far as the row's hold let it get.
pub enum Sibling {
    /// The request is sent and its answer is held inside the handler.
    Open(common::H2RequestStream),
    /// The head is read and the body is withheld.
    Committed(common::H2ReadHalf),
}

impl Sibling {
    /// Open the sibling on `client` and hold its answer inside the handler.
    ///
    /// A plain answer has no transfer owner, so no transfer bound and no
    /// transfer hold a row arms can reach it.
    pub async fn held_in_handler(
        client: &mut common::PersistentH2Client,
        gate: &SiblingGate,
    ) -> Result<Self, String> {
        let mut sibling = client
            .open_paced("GET", SIBLING_HANDLER_PATH, "localhost", &[])
            .await;
        sibling.finish();
        let entered = eventually(|| gate.entered()).await;
        expect("the sibling never reached its handler", entered)?;
        Ok(Self::Open(sibling))
    }

    /// Read the released answer: `200` and the sibling's whole body.
    pub async fn answered(self) -> Row {
        match self {
            Self::Open(mut stream) => {
                let answer = stream
                    .try_answer()
                    .await
                    .map_err(|error| format!("the held sibling ended unanswered: {error}"))?;
                all([
                    expect_eq("sibling status", answer.status, 200),
                    expect_eq("sibling body", answer.text().as_ref(), SIBLING_BODY),
                ])
            }
            Self::Committed(read) => {
                let settled = read.settle().await;
                all([
                    expect_eq("sibling status", settled.status, 200),
                    expect_eq("sibling reset", settled.reset, false),
                    expect_eq("sibling body bytes", settled.bytes, SIBLING_BODY.len()),
                ])
            }
        }
    }
}

/// Release the held sibling, and require its answer, its completion recorded
/// second past `before`, and no mapper invocation.
///
/// A failure that reset every stream open on the connection would have taken
/// the sibling with it.
pub async fn sibling_settled(
    sibling: Option<Sibling>,
    gate: &SiblingGate,
    owners: &impl common::Owns<ResponseCommitmentController>,
    before: &Baseline,
    journal: &common::Journal,
) -> Row {
    gate.release();
    let answered = match sibling {
        Some(sibling) => sibling.answered().await,
        None => Err("the sibling stream was never opened".into()),
    };
    let recorded = eventually(|| before.completions_since(owners) == 2).await;
    all([
        answered,
        expect("the sibling's completion was never recorded", recorded),
        expect_eq(
            "sibling mapper invocations",
            common::drain(journal).len(),
            0,
        ),
    ])
}

/// The checkpoints one row steps its call's owners through.
///
/// Borrowed from the row's scoped view, so a row reaches only the owners its
/// view lends, and every step names the row it failed in.
pub struct Stage<'a, O> {
    pub owners: &'a O,
    pub label: &'a str,
}

impl<O> Stage<'_, O>
where
    O: common::Owns<ResponseCommitmentController> + common::Owns<TransferOwnerController>,
{
    pub fn arm<P>(&self, point: P)
    where
        P: common::OwnerPoint,
        O: common::Owns<P::Owner>,
    {
        common::arm_point(self.owners, point, self.label);
    }

    pub async fn paused<P>(&self, point: P)
    where
        P: common::OwnerPoint,
        O: common::Owns<P::Owner>,
    {
        common::wait_until_paused_within(self.owners, point, BOUND, self.label).await;
    }

    pub fn release<P>(&self, point: P)
    where
        P: common::OwnerPoint,
        O: common::Owns<P::Owner>,
    {
        common::release_point(self.owners, point, self.label);
    }

    /// What both transfer directions observed so far.
    pub fn transfers(&self) -> TransferObservation {
        common::Owns::<TransferOwnerController>::owner(self.owners).observed()
    }

    /// Wait until the upload reached its normal end.
    pub async fn upload_ended(&self) -> Row {
        let ended =
            eventually(|| self.transfers().upload.terminal == Some(InboundTerminal::ResponseHead))
                .await;
        expect(
            &format!(
                "the upload never reached its end: {:?}",
                self.transfers().upload
            ),
            ended,
        )
    }

    /// Wait until the upload owner and all `downloads` download owners were
    /// released.
    pub async fn owners_released(&self, downloads: usize) -> Row {
        let released = eventually(|| {
            let transfers = self.transfers();
            transfers.upload.releases == 1 && transfers.download.releases == downloads
        })
        .await;
        expect(
            &format!(
                "direction owners were not all released: {:?}",
                self.transfers()
            ),
            released,
        )
    }

    /// Send a whole request, commit tonic's head, and hold its first DATA.
    ///
    /// The head is committed at the handoff before the download owner exists,
    /// so the download's first source poll can be held behind a head the peer
    /// has already read. The caller releases the held poll.
    pub async fn staged_reply(
        &self,
        stream: &mut common::H2RequestStream,
        payload: &[Box<[u8]>],
    ) -> Result<common::H2ReadHalf, String> {
        let handoff = ResponseCommitmentEdge::GrpcHandoffCommitted;
        self.arm(handoff);
        sent_whole(stream, payload).await?;
        self.paused(handoff).await;
        self.upload_ended().await?;
        Ok(self.first_data_held(stream).await)
    }

    /// Release the held handoff with the download's first source poll armed,
    /// and read the committed head while that poll is held.
    pub async fn first_data_held(
        &self,
        stream: &mut common::H2RequestStream,
    ) -> common::H2ReadHalf {
        let handoff = ResponseCommitmentEdge::GrpcHandoffCommitted;
        self.arm(TransferOwnerEdge::BeforeSourcePoll);
        self.release(handoff);
        let read = stream.commit().await;
        self.paused(TransferOwnerEdge::BeforeSourcePoll).await;
        read
    }
}
