//! The two bodies Camber owns around one tonic-owned RPC.
//!
//! Camber coordinates a gRPC call only until tonic commits a response head. Up
//! to that head, [`GrpcRequestBody`] reports the terminal it fixed to the pre-head
//! coordinator, which weighs it against the operation's own carried sources and
//! tonic's answer in one turn: tonic's answer is that coordinator's own read and
//! settles the turn it arrives in, and the reported terminal and the carried
//! sources decide only a turn it left open. Past that head tonic owns
//! the RPC: an upload terminal becomes the terminal error of tonic's request
//! body and nothing else, and a download terminal ends [`GrpcDownload`] without
//! synthesizing a `grpc-status`, replacing tonic's trailers, or reaching a
//! rejection mapper.

use super::body::BodyError;
use super::mock::{LifecycleScript, ResponseCommitmentEdge};
use super::operation::{OperationEnvelope, PreCommitCause};
use super::response_commitment::ResponseOrigin;
use super::transfer::{
    IncomingSource, SourceStep, Transfer, TransferFailure, TransferOwner, TransferSource, step_of,
};
use bytes::Bytes;
use hyper::body::Body as _;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::oneshot;

/// The request body every registered gRPC service is handed.
///
/// One admitted gRPC request's payload, under the upload budget bounding it.
/// Tonic owns every frame it admits from here; Camber owns the byte maximum,
/// the quiet interval, the lifetime, and the cancellation around them. A bound
/// this body crosses ends it with the typed
/// [`RuntimeError`](crate::RuntimeError) its own owner names, which becomes the
/// terminal error of tonic's request stream and the status tonic derives from
/// it. Camber maps nothing here.
///
/// It is public because it is named by the bound
/// [`GrpcRouter::add_service`](super::GrpcRouter::add_service) places on the
/// services it accepts. A tonic-generated server is already generic over its
/// request body and needs no mention of it; a hand-written `tower` service does.
pub struct GrpcRequestBody {
    transfer: Transfer<IncomingSource<hyper::body::Incoming>>,
    /// The channel the pre-head coordinator reads this upload's terminal from.
    ///
    /// Taken when a terminal is fixed, so exactly one report is ever sent.
    reported: Option<oneshot::Sender<PreCommitCause>>,
    /// Set once this body has answered with its terminal or its end.
    ended: bool,
}

impl GrpcRequestBody {
    /// Split one incoming payload into the body tonic reads and the report the
    /// pre-head coordinator weighs.
    pub(super) fn split(
        incoming: hyper::body::Incoming,
        upload: TransferOwner,
    ) -> (Self, oneshot::Receiver<PreCommitCause>) {
        let (reported, report) = oneshot::channel();
        (
            Self {
                transfer: upload.over(IncomingSource::new(incoming)),
                reported: Some(reported),
                ended: false,
            },
            report,
        )
    }

    /// Publish one fixed terminal to the coordinator, if one is still listening.
    fn report(&mut self, failure: &TransferFailure) {
        match self.reported.take() {
            Some(reported) => {
                let _ = reported.send(PreCommitCause::reported(
                    failure.terminal(),
                    failure.refusal(),
                ));
            }
            None => {}
        }
    }
}

impl hyper::body::Body for GrpcRequestBody {
    type Data = Bytes;
    /// The public typed failure the measuring owner names.
    ///
    /// Not the private body error the response side reports: tonic reads this
    /// one, and a caller writing its own service has to be able to name what it
    /// received.
    type Error = crate::RuntimeError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.ended {
            return Poll::Ready(None);
        }
        match this.transfer.poll_transfer(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(None)) => {
                this.ended = true;
                Poll::Ready(None)
            }
            Poll::Ready(Ok(Some(data))) => Poll::Ready(Some(Ok(hyper::body::Frame::data(data)))),
            Poll::Ready(Err(failure)) => {
                this.ended = true;
                this.report(&failure);
                Poll::Ready(Some(Err(failure.error())))
            }
        }
    }

    fn is_end_stream(&self) -> bool {
        self.ended
    }
}

/// Tonic's response body, read as one transfer source.
///
/// Trailers never travel through the transfer: it accounts payload bytes, and a
/// trailer set carries none. They are captured here and emitted by
/// [`GrpcDownload`] after the source reaches its normal end, which is the order
/// the wire already puts them in.
///
/// Tonic's own body error is captured the same way. It is a `Status` — the
/// RPC's real answer — so it becomes the trailer set tonic would have written
/// rather than a Camber terminal that would claim a boundary Camber never had.
struct TonicSource {
    inner: tonic::body::Body,
    trailers: Option<hyper::HeaderMap>,
}

impl TonicSource {
    const fn new(inner: tonic::body::Body) -> Self {
        Self {
            inner,
            trailers: None,
        }
    }

    /// The trailer set tonic ended this body with, if it wrote one.
    const fn trailers(&self) -> Option<&hyper::HeaderMap> {
        self.trailers.as_ref()
    }
}

impl TransferSource for TonicSource {
    fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<SourceStep> {
        match std::pin::Pin::new(&mut self.inner).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(self.stepped(frame)),
            Poll::Ready(Some(Err(status))) => {
                self.trailers = Some(status_trailers(&status));
                Poll::Ready(SourceStep::End)
            }
            Poll::Ready(None) => Poll::Ready(SourceStep::End),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl TonicSource {
    /// Name one frame tonic produced, keeping the trailer set it may carry.
    fn stepped(&mut self, frame: hyper::body::Frame<Bytes>) -> SourceStep {
        match frame.into_data() {
            Ok(data) => step_of(data),
            Err(trailers) => {
                self.trailers = trailers.into_trailers().ok();
                SourceStep::Empty
            }
        }
    }
}

/// The trailer set one tonic status is written to the wire as.
///
/// Written by tonic itself rather than by hand, because the encoding is tonic's
/// and not Camber's: `grpc-message` is percent-encoded under tonic's own set,
/// the status details become `grpc-status-details-bin`, and the status's
/// trailing metadata is extended onto the map. A message written raw reaches
/// the peer mangled by its percent-decode, and a message holding a byte no
/// header value admits — every non-ASCII one — reaches it not at all.
fn status_trailers(status: &tonic::Status) -> hyper::HeaderMap {
    let mut trailers = hyper::HeaderMap::with_capacity(3 + status.metadata().len());
    match status.add_header(&mut trailers) {
        Ok(()) => trailers,
        Err(unwritable) => {
            tracing::warn!(
                code = status.code() as i32,
                %unwritable,
                "grpc status trailers could not be written",
            );
            bare_status_trailers(status)
        }
    }
}

/// The one trailer a status that could not be written whole still owes.
///
/// A partially written map is not sent: a peer reads the code that ended the
/// RPC, or it reads a stream that carries no status at all, and half a status
/// is the second of those wearing the first one's name.
fn bare_status_trailers(status: &tonic::Status) -> hyper::HeaderMap {
    let mut trailers = hyper::HeaderMap::with_capacity(1);
    trailers.insert(
        "grpc-status",
        hyper::header::HeaderValue::from(status.code() as i32),
    );
    trailers
}

/// What a committed gRPC answer still owes the peer.
enum DownloadState {
    /// Tonic's payload frames, under the download owner bounding them.
    Streaming,
    /// The trailer set tonic wrote, which the transfer never carried.
    Trailing,
    /// Nothing: this body has reported its end or its terminal.
    Ended,
}

/// One committed gRPC answer, under the download owner bounding it.
///
/// The owner is what Hyper actually polls: bytes, quiet interval, lifetime,
/// cancellation, and the peer's response lifetime are decided inside it. A
/// terminal it fixes ends this body with a typed transport failure — HTTP/2
/// resets the one affected stream — and writes no status of its own, because the
/// head is already on the wire and the status behind it is tonic's.
pub(super) struct GrpcDownload {
    transfer: Transfer<TonicSource>,
    state: DownloadState,
}

impl GrpcDownload {
    /// Take tonic's response body under the download owner bounding it.
    pub(super) fn new(inner: tonic::body::Body, download: TransferOwner) -> Self {
        Self {
            transfer: download.over(TonicSource::new(inner)),
            state: DownloadState::Streaming,
        }
    }

    /// Produce the next frame this answer owes.
    fn poll_body(&mut self, cx: &mut Context<'_>) -> GrpcFramePoll {
        match self.state {
            DownloadState::Ended => Poll::Ready(None),
            DownloadState::Trailing => Poll::Ready(self.trailing()),
            DownloadState::Streaming => self.streaming(cx),
        }
    }

    /// Read one frame from the bounded transfer under tonic's body.
    fn streaming(&mut self, cx: &mut Context<'_>) -> GrpcFramePoll {
        match self.transfer.poll_transfer(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok(Some(data))) => Poll::Ready(Some(Ok(hyper::body::Frame::data(data)))),
            Poll::Ready(Ok(None)) => {
                self.state = DownloadState::Trailing;
                Poll::Ready(self.trailing())
            }
            Poll::Ready(Err(failure)) => {
                self.state = DownloadState::Ended;
                Poll::Ready(Some(Err(BodyError::from(failure))))
            }
        }
    }

    /// Hand over tonic's trailer set, once, and end.
    fn trailing(&mut self) -> Option<Result<hyper::body::Frame<Bytes>, BodyError>> {
        self.state = DownloadState::Ended;
        self.transfer
            .source()
            .trailers()
            .cloned()
            .map(|trailers| Ok(hyper::body::Frame::trailers(trailers)))
    }
}

type GrpcFramePoll = Poll<Option<Result<hyper::body::Frame<Bytes>, BodyError>>>;

impl hyper::body::Body for GrpcDownload {
    type Data = Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        self.get_mut().poll_body(cx)
    }

    /// Ended once this body has reported its terminal or handed over trailers.
    ///
    /// Tonic's own report is deliberately not forwarded: the transfer under it
    /// can hold a frame this body has not delivered, and a wrapper that claimed
    /// its source's end would have Hyper stop polling before that frame or the
    /// trailer set behind it reached the peer.
    fn is_end_stream(&self) -> bool {
        matches!(self.state, DownloadState::Ended)
    }
}

/// The report one pre-head coordinator weighs beside tonic's head.
///
/// A closed sender is an upload that ended without fixing a terminal, which is
/// no cause at all — so it never resolves, and the coordinator decides on its
/// own carried sources and tonic's answer alone.
pub(super) async fn reported_terminal(report: oneshot::Receiver<PreCommitCause>) -> PreCommitCause {
    match report.await {
        Ok(cause) => cause,
        Err(_closed) => std::future::pending().await,
    }
}

/// Publish tonic's head at the moment it became ready, before it is committed.
///
/// Taken here rather than after the coordinator returns, because a case that
/// has to make a local cause and this head ready in the same turn needs the
/// head held at the instant it arrived.
pub(super) async fn produced_head<T>(
    producing: impl std::future::Future<Output = T>,
    observer: Option<&Arc<LifecycleScript>>,
) -> T {
    let produced = producing.await;
    LifecycleScript::pause_at_response_commit(
        observer.map(Arc::as_ref),
        ResponseCommitmentEdge::GrpcHeadReady,
    )
    .await;
    produced
}

/// Commit one tonic head and take its body under this operation's download
/// owner.
///
/// The boundary itself: past this call no Camber cause reaches a mapper, and
/// each direction ends only its own body.
pub(super) async fn commit(
    response: hyper::Response<tonic::body::Body>,
    operation: &OperationEnvelope,
    observer: Option<Arc<LifecycleScript>>,
) -> hyper::Response<GrpcDownload> {
    // The handoff is the barrier, so the commitment is taken as it is crossed
    // and before anything can be held here. Past it no Camber cause reaches a
    // mapper: tonic owns the status, and a budget that expires afterwards is
    // its own owner's to end rather than a second answer to this request.
    operation.commit_response(ResponseOrigin::Grpc);
    LifecycleScript::pause_at_response_commit(
        observer.as_deref(),
        ResponseCommitmentEdge::GrpcHandoffCommitted,
    )
    .await;
    let download = operation.download(super::TransferBudget::unbounded(), observer);
    let (parts, body) = response.into_parts();
    hyper::Response::from_parts(parts, GrpcDownload::new(body, download))
}
