//! The one owner a streaming upload or download runs under.
//!
//! An admitted operation resolves its two transfer policies once, and each
//! concrete streaming consumer takes exactly one narrow owner from it: a
//! generic [`StreamResponse`](super::StreamResponse) body, an SSE body, a
//! proxied download, and a streaming multipart upload each get their own. The
//! two directions of one operation share nothing but the operation: bytes,
//! quiet interval, and lifetime are counted per owner, so an upload that has
//! been quiet for a minute cannot be reported as a download that stalled, and a
//! download that ran for an hour does not spend the upload's lifetime.
//!
//! One turn of [`Transfer::poll_transfer`] reads the source first and weighs
//! every carried source second, which is what keeps a frame that was already
//! waiting from being pre-empted by the quiet interval that frame restarts. The
//! terminal itself is selected by the one shared
//! [`InboundReady::select`](super::operation::InboundReady) every pre-commit
//! coordinator uses, so a transfer bound and a request bound that become ready
//! in the same turn are resolved by the declared precedence rather than by which
//! source this file happened to read first.
//!
//! Nothing is polled after a terminal is fixed, and the frame in hand when one
//! is fixed is dropped rather than delivered.

use super::body::BodyError;
use super::boundary::{ByteBoundary, DeadlineBoundary};
use super::mock::{CheckpointHold, LifecycleCheckpoint, LifecycleScript, TransferEvent};
use super::operation::{InboundReady, InboundTerminal, InboundWatch};
use super::rejection::Rejected;
use super::transfer_budget::TransferBudget;
use crate::RuntimeError;
use bytes::Bytes;
use std::fmt::Display;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::time::Instant;

/// Which direction of one admitted operation a transfer owner enforces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TransferDirection {
    /// Payload the peer sends, read by a framework consumer.
    Upload,
    /// Payload Camber produces, written to the peer.
    Download,
}

impl TransferDirection {
    /// The byte maximum a crossing in this direction is reported under.
    pub(super) const fn bytes(self) -> ByteBoundary {
        match self {
            Self::Upload => ByteBoundary::TransferUpload,
            Self::Download => ByteBoundary::TransferDownload,
        }
    }
}

/// One step a transfer's source produced.
///
/// Payload and no-payload frames are separate answers rather than one answer
/// with a length: only payload restarts the quiet interval and only payload is
/// counted, so a peer or a producer cannot hold a transfer open with trailers
/// and empty frames.
pub(super) enum SourceStep {
    /// A frame carrying payload.
    Data(Bytes),
    /// A frame that carried none: a trailer set, or an empty data frame.
    Empty,
    /// The source will produce nothing more.
    End,
    /// The source failed, carrying its own account of why.
    Failed(Box<str>),
}

/// The producer one transfer direction reads.
///
/// Implemented rather than boxed: each direction has exactly one concrete source
/// per route class, and the transfer owner is generic over it, so no streaming
/// frame pays for dynamic dispatch.
pub(super) trait TransferSource {
    /// Read one step, registering interest when the source has nothing yet.
    fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<SourceStep>;
}

/// A bounded channel one in-process producer writes response frames to.
pub(super) struct ChannelSource(tokio::sync::mpsc::Receiver<Bytes>);

impl ChannelSource {
    pub(super) const fn new(receiver: tokio::sync::mpsc::Receiver<Bytes>) -> Self {
        Self(receiver)
    }
}

impl TransferSource for ChannelSource {
    fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<SourceStep> {
        match self.0.poll_recv(cx) {
            Poll::Ready(Some(data)) => Poll::Ready(step_of(data)),
            Poll::Ready(None) => Poll::Ready(SourceStep::End),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A bounded channel one upstream reader writes frames or its failure to.
pub(super) struct UpstreamSource(tokio::sync::mpsc::Receiver<Result<Bytes, BodyError>>);

impl UpstreamSource {
    pub(super) const fn new(
        receiver: tokio::sync::mpsc::Receiver<Result<Bytes, BodyError>>,
    ) -> Self {
        Self(receiver)
    }
}

impl TransferSource for UpstreamSource {
    fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<SourceStep> {
        match self.0.poll_recv(cx) {
            Poll::Ready(Some(Ok(data))) => Poll::Ready(step_of(data)),
            Poll::Ready(Some(Err(error))) => {
                Poll::Ready(SourceStep::Failed(error.to_string().into()))
            }
            Poll::Ready(None) => Poll::Ready(SourceStep::End),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// One hyper body a peer is still sending.
pub(super) struct IncomingSource<B>(B);

impl<B> IncomingSource<B> {
    pub(super) const fn new(body: B) -> Self {
        Self(body)
    }
}

impl<B> TransferSource for IncomingSource<B>
where
    B: hyper::body::Body<Data = Bytes> + Unpin,
    B::Error: Display,
{
    fn poll_step(&mut self, cx: &mut Context<'_>) -> Poll<SourceStep> {
        match std::pin::Pin::new(&mut self.0).poll_frame(cx) {
            Poll::Ready(Some(Ok(frame))) => Poll::Ready(frame_step(frame)),
            Poll::Ready(Some(Err(error))) => {
                Poll::Ready(SourceStep::Failed(error.to_string().into()))
            }
            Poll::Ready(None) => Poll::Ready(SourceStep::End),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Name one hyper frame by whether it carried payload.
///
/// A trailer set is payload-free by definition, so it joins the empty data
/// frame: neither is counted and neither restarts the quiet interval.
fn frame_step(frame: hyper::body::Frame<Bytes>) -> SourceStep {
    match frame.into_data() {
        Ok(data) => step_of(data),
        Err(_trailers) => SourceStep::Empty,
    }
}

/// Name one read frame by whether it carried payload.
pub(super) fn step_of(data: Bytes) -> SourceStep {
    match data.is_empty() {
        true => SourceStep::Empty,
        false => SourceStep::Data(data),
    }
}

/// The narrow transfer authority one production consumer takes from an
/// operation.
///
/// The policy minima are resolved when this value is built — runtime, server,
/// host, router, route, and the consumer's own registration — so the concrete
/// consumer receives one settled budget rather than a chain it could re-resolve
/// differently.
pub(super) struct TransferOwner {
    direction: TransferDirection,
    budget: TransferBudget,
    /// The admitted operation's cancellation, shutdown, and peer-lifetime
    /// authority. `None` is a transfer with no admitted operation behind it,
    /// which is the controlled session fixture and nothing a served request
    /// reaches.
    authority: Option<InboundWatch>,
    observer: Option<Arc<LifecycleScript>>,
}

impl TransferOwner {
    /// The owner one production consumer runs under.
    pub(super) const fn new(
        direction: TransferDirection,
        budget: TransferBudget,
        authority: InboundWatch,
        observer: Option<Arc<LifecycleScript>>,
    ) -> Self {
        Self {
            direction,
            budget,
            authority: Some(authority),
            observer,
        }
    }

    /// The owner a transfer with no admitted operation behind it runs under.
    pub(super) const fn detached(direction: TransferDirection, budget: TransferBudget) -> Self {
        Self {
            direction,
            budget,
            authority: None,
            observer: None,
        }
    }

    /// This owner reduced to its two deadlines.
    ///
    /// For the one direction whose payload bytes another owner already accounts
    /// for. Streaming multipart is that direction: route-aware body admission is
    /// its single payload-byte authority, and a second counter over the same
    /// frames would be a second authority over the same bytes. Transfer policy
    /// still contributes its quiet interval and its lifetime.
    pub(super) fn deadlines_only(self) -> Self {
        Self {
            budget: self.budget.without_max_bytes(),
            ..self
        }
    }

    /// Take ownership of `source` under the bounds this owner carries.
    pub(super) fn over<S: TransferSource>(self, source: S) -> Transfer<S> {
        let Self {
            direction,
            budget,
            authority,
            observer,
        } = self;
        LifecycleScript::observe_transfer(
            observer.as_deref(),
            direction,
            TransferEvent::PolicyFrozen(budget),
        );
        Transfer {
            source,
            direction,
            budget,
            counted: 0,
            total: None,
            quiet_since: Instant::now(),
            ended: None,
            pending: None,
            authority,
            account: None,
            held: None,
            selecting: None,
            observer,
        }
    }
}

/// One streaming direction's independent bytes, quiet interval, and lifetime.
///
/// Deliberately not `Clone`. One direction of one operation has one owner: a
/// second copy of a byte counter or a lifetime deadline would be a second
/// authority over the same transfer, which is the condition this type exists to
/// remove.
pub(super) struct Transfer<S> {
    source: S,
    direction: TransferDirection,
    budget: TransferBudget,
    /// Payload bytes this direction has admitted, added with checked addition
    /// before a frame is retained or delivered.
    counted: usize,
    /// The lifetime deadline, armed when this consumer first became eligible to
    /// poll rather than when its policy was frozen.
    total: Option<Instant>,
    /// When a non-empty data frame last crossed this owner.
    quiet_since: Instant,
    /// The terminal this owner fixed. Set once, and nothing is polled after it.
    ended: Option<InboundTerminal>,
    /// The one frame read from the source and not yet delivered to the sink.
    pending: Option<Bytes>,
    authority: Option<InboundWatch>,
    /// The source's own account of its failure, while one has been read.
    account: Option<Box<str>>,
    /// The checkpoint this owner is being held at, while a controller holds one.
    held: Option<CheckpointHold>,
    /// One turn's collected readiness, while a controller holds its selection.
    ///
    /// The turn resumes from here rather than starting again, so a hold in front
    /// of a decision cannot change which sources that decision was made over.
    selecting: Option<InboundReady>,
    observer: Option<Arc<LifecycleScript>>,
}

/// The source accessor the gRPC handoff needs, and no other owner does.
#[cfg(feature = "grpc")]
impl<S> Transfer<S> {
    /// The source this transfer owns.
    ///
    /// For the one consumer whose source carries something a transfer never
    /// does: tonic's response body ends with a trailer set, which carries no
    /// payload byte and so never crosses this owner as a frame.
    pub(super) const fn source(&self) -> &S {
        &self.source
    }
}

impl<S: TransferSource> Transfer<S> {
    /// Read the next frame this transfer delivers, or the terminal it ended on.
    ///
    /// One turn reads the source, weighs every carried source against this
    /// transfer's own two deadlines, and lets the declared precedence name the
    /// single terminal. `Ok(None)` is the source's normal end.
    pub(super) fn poll_transfer(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<Result<Option<Bytes>, TransferFailure>> {
        loop {
            if let Some(ended) = self.ended {
                return Poll::Ready(self.answer(ended));
            }
            // A turn already collected and then held at its selection resumes
            // from that reading. Reading its source again would make one turn
            // into two, and a source that became ready in between would be
            // weighed against a reading it never belonged to.
            let resumed = self.selecting.take();
            let turn = match resumed {
                Some(ready) => Turn::Collected(ready),
                None => self.collect(cx),
            };
            let ready = match turn {
                Turn::Collected(ready) => ready,
                Turn::Restart => continue,
                Turn::Held => return Poll::Pending,
            };
            if self
                .holding(cx, LifecycleCheckpoint::BeforeTransferTerminalSelection)
                .is_pending()
            {
                self.selecting = Some(ready);
                return Poll::Pending;
            }
            let Some(terminal) = ready.select() else {
                return match self.pending.take() {
                    Some(frame) => Poll::Ready(Ok(Some(frame))),
                    None => Poll::Pending,
                };
            };
            self.fix(terminal);
            return Poll::Ready(self.answer(terminal));
        }
    }

    /// Collect one fresh turn: its source read, and every carried source with it.
    fn collect(&mut self, cx: &mut Context<'_>) -> Turn {
        // Armed before the checkpoint below, never after it: the lifetime starts
        // when this consumer first became eligible to poll, and a seam that armed
        // it later would move a production deadline.
        self.arm();
        if self
            .holding(cx, LifecycleCheckpoint::BeforeTransferSourcePoll)
            .is_pending()
        {
            return Turn::Held;
        }
        match self.read_source(cx) {
            SourceRead::Nothing => Turn::Restart,
            read => Turn::Collected(self.weighed(read, cx)),
        }
    }

    /// The next frame this transfer delivers, awaited.
    ///
    /// The same one turn [`Self::poll_transfer`] runs, for the consumers that
    /// drive a transfer from an async owner instead of from inside a body poll.
    pub(super) async fn frame(&mut self) -> Result<Option<Bytes>, TransferFailure> {
        std::future::poll_fn(|cx| self.poll_transfer(cx)).await
    }

    /// Arm the lifetime deadline at the first turn this consumer took.
    ///
    /// Not at construction: the policy is frozen where the route resolves it,
    /// and a transfer that waits behind a middleware chain or a handler has not
    /// begun spending its own lifetime yet.
    fn arm(&mut self) {
        match (self.total, self.budget.total()) {
            (None, Some(total)) => self.total = Some(Instant::now() + total),
            (Some(_) | None, _) => {}
        }
    }

    /// Read one step from the source into this turn.
    ///
    /// The source is read before anything is weighed, so a frame that was
    /// already waiting restarts the quiet interval in the same turn that
    /// interval is measured in. A frame already in hand is not a reason to read
    /// another: this owner retains exactly one.
    fn read_source(&mut self, cx: &mut Context<'_>) -> SourceRead {
        if self.pending.is_some() {
            return SourceRead::Quiet;
        }
        match self.source.poll_step(cx) {
            Poll::Pending => SourceRead::Quiet,
            Poll::Ready(SourceStep::Empty) => {
                self.count_frame();
                SourceRead::Nothing
            }
            Poll::Ready(SourceStep::End) => SourceRead::Ended,
            Poll::Ready(SourceStep::Failed(account)) => {
                self.account = Some(account);
                SourceRead::Failed
            }
            Poll::Ready(SourceStep::Data(data)) => {
                self.count_frame();
                self.admitted(data)
            }
        }
    }

    /// Account one payload frame against this direction's byte maximum.
    ///
    /// Nothing retains it until the addition has been checked: a frame that
    /// would carry this transfer past its maximum is released here, while it is
    /// still only a value this turn holds, so the crossing bytes are never
    /// delivered and never retained.
    fn admitted(&mut self, data: Bytes) -> SourceRead {
        let delivered = data.len();
        let crossed = self
            .counted
            .checked_add(delivered)
            .is_none_or(|total| self.budget.max_bytes().is_some_and(|max| total > max));
        match crossed {
            true => {
                drop(data);
                LifecycleScript::observe_transfer(
                    self.observer.as_deref(),
                    self.direction,
                    TransferEvent::CrossingReleased,
                );
                SourceRead::Crossed
            }
            false => {
                self.counted += delivered;
                self.quiet_since = Instant::now();
                self.pending = Some(data);
                LifecycleScript::observe_transfer(
                    self.observer.as_deref(),
                    self.direction,
                    TransferEvent::Admitted(self.counted),
                );
                SourceRead::Quiet
            }
        }
    }

    /// Collect every source this turn can decide, including the carried ones.
    ///
    /// The operation's authority is read through the same guard the inbound
    /// coordinator uses, so a shutdown deadline, a forced cancellation, or a
    /// peer whose lifetime ended reaches a transfer terminal through the one
    /// declared order rather than through a rule of this file's own.
    fn weighed(&mut self, read: SourceRead, cx: &mut Context<'_>) -> InboundReady {
        let wanted = self.next_deadline();
        let carried = match self.authority.as_mut() {
            Some(authority) => authority.observed(cx, wanted),
            None => InboundReady::default(),
        };
        let now = Instant::now();
        let idle = self
            .budget
            .idle()
            .is_some_and(|idle| now.saturating_duration_since(self.quiet_since) >= idle);
        let total = self.total.is_some_and(|total| now >= total);
        let carried = match idle {
            true => carried.with_transfer_idle(),
            false => carried,
        };
        let carried = match total {
            true => carried.with_transfer_total(),
            false => carried,
        };
        match read {
            SourceRead::Crossed => carried.with_transfer_bytes(),
            SourceRead::Ended => carried.with_response_head(),
            SourceRead::Failed => carried.with_source_failure(),
            SourceRead::Quiet | SourceRead::Nothing => carried,
        }
    }

    /// The earliest instant one of this transfer's own deadlines can decide.
    fn next_deadline(&self) -> Option<Instant> {
        [
            self.budget.idle().map(|idle| self.quiet_since + idle),
            self.total,
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Fix this transfer's one terminal and release what it still holds.
    ///
    /// The frame in hand is released rather than delivered: a terminal selected
    /// in the same turn as a read frame ends the transfer, and delivering that
    /// frame first would put payload on a transfer that has already ended.
    fn fix(&mut self, terminal: InboundTerminal) {
        self.ended = Some(terminal);
        match self.pending.take() {
            Some(frame) => {
                drop(frame);
                LifecycleScript::observe_transfer(
                    self.observer.as_deref(),
                    self.direction,
                    TransferEvent::CrossingReleased,
                );
            }
            None => {}
        }
        LifecycleScript::observe_transfer(
            self.observer.as_deref(),
            self.direction,
            TransferEvent::Terminal(terminal),
        );
    }

    /// The answer one fixed terminal produces.
    fn answer(&self, terminal: InboundTerminal) -> Result<Option<Bytes>, TransferFailure> {
        match terminal {
            InboundTerminal::ResponseHead => Ok(None),
            ended => Err(TransferFailure {
                direction: self.direction,
                terminal: ended,
                budget: self.budget,
                account: self.account.clone(),
            }),
        }
    }

    /// Hold at one checkpoint while a controller holds one, from inside a poll.
    ///
    /// The gate is taken synchronously and polled here, so the one turn this
    /// file runs can be held open by a case without this file owning an async
    /// wait a body poll cannot take.
    fn holding(&mut self, cx: &mut Context<'_>, checkpoint: LifecycleCheckpoint) -> Poll<()> {
        let Some(held) = self
            .held
            .take()
            .or_else(|| LifecycleScript::hold_at(self.observer.as_deref(), checkpoint))
        else {
            return Poll::Ready(());
        };
        match held.poll_released(cx) {
            Poll::Ready(()) => Poll::Ready(()),
            Poll::Pending => {
                self.held = Some(held);
                Poll::Pending
            }
        }
    }

    /// Count one frame this owner polled out of its source.
    fn count_frame(&self) {
        LifecycleScript::observe_transfer(
            self.observer.as_deref(),
            self.direction,
            TransferEvent::FramePolled,
        );
    }
}

impl<S> Drop for Transfer<S> {
    /// Record that this direction's source and producer handle were released.
    ///
    /// Written where the release happens rather than inferred from a terminal:
    /// a transfer dropped by disconnect, reset, shutdown, or cancellation
    /// releases its source through the same destruction as one that ended on a
    /// bound it selected.
    fn drop(&mut self) {
        LifecycleScript::observe_transfer(
            self.observer.as_deref(),
            self.direction,
            TransferEvent::Released,
        );
    }
}

/// What one fresh turn produced before anything was selected.
enum Turn {
    /// Every source this turn can decide.
    Collected(InboundReady),
    /// A payload-free frame was read; the turn starts again.
    Restart,
    /// A controller is holding this turn in front of its source read.
    Held,
}

/// What one turn's read of the source contributed.
///
/// Named rather than folded into the ready set directly, because two of the
/// answers are not sources at all: a frame this owner is holding and a frame
/// that carried no payload decide nothing, and the second one restarts the turn.
enum SourceRead {
    /// Nothing this turn can decide came from the source.
    Quiet,
    /// A frame with no payload was read; the turn starts again.
    Nothing,
    /// A payload frame crossed this direction's byte maximum.
    Crossed,
    /// The source produced its last frame.
    Ended,
    /// The source failed.
    Failed,
}

/// How one transfer direction ended when it did not complete.
///
/// It carries the direction, the terminal the shared precedence selected, and
/// the policy that terminal was measured against, so every consumer reports the
/// same typed provenance without re-deriving which bound was crossed.
#[derive(Debug)]
pub(super) struct TransferFailure {
    direction: TransferDirection,
    terminal: InboundTerminal,
    budget: TransferBudget,
    /// The source's own account, for the row where the source is what failed.
    account: Option<Box<str>>,
}

impl TransferFailure {
    /// The terminal the shared precedence selected.
    pub(super) const fn terminal(&self) -> InboundTerminal {
        self.terminal
    }

    /// The refusal a pre-commit consumer answers this terminal with.
    ///
    /// `None` is a silent row: the declared table gives it no mapper, so the
    /// transport ends and ownership releases without a mapped response. The
    /// three transfer rows mint their refusal here, at the owner that measured
    /// the bound, so no later stage restates a maximum or a deadline it did not
    /// enforce.
    pub(super) fn refusal(&self) -> Option<Rejected> {
        match self.terminal {
            InboundTerminal::TransferBytes => Some(Rejected::transfer_too_large(
                self.direction.bytes(),
                self.budget.max_bytes().unwrap_or(usize::MAX),
            )),
            InboundTerminal::TransferIdle => self
                .budget
                .idle()
                .map(|idle| Rejected::transfer_timeout(DeadlineBoundary::TransferIdle, idle)),
            InboundTerminal::TransferTotal => self
                .budget
                .total()
                .map(|total| Rejected::transfer_timeout(DeadlineBoundary::TransferTotal, total)),
            InboundTerminal::SourceFailure => {
                Some(Rejected::transfer_source_failed(self.diagnostic()))
            }
            InboundTerminal::ShutdownDeadline
            | InboundTerminal::ForcedCancellation
            | InboundTerminal::RouteBodyLimit
            | InboundTerminal::BodyIdle
            | InboundTerminal::RequestTotal
            | InboundTerminal::Disconnect
            | InboundTerminal::ResponseHead => None,
        }
    }

    /// This failure as the typed error a consumer that reports errors returns.
    pub(super) fn error(&self) -> RuntimeError {
        match self.terminal {
            InboundTerminal::TransferBytes => RuntimeError::LimitExceeded(self.direction.bytes()),
            InboundTerminal::TransferIdle => {
                RuntimeError::DeadlineExceeded(DeadlineBoundary::TransferIdle)
            }
            InboundTerminal::TransferTotal => {
                RuntimeError::DeadlineExceeded(DeadlineBoundary::TransferTotal)
            }
            InboundTerminal::SourceFailure => {
                RuntimeError::RequestBodyUnreadable(self.diagnostic())
            }
            InboundTerminal::RouteBodyLimit => RuntimeError::RequestBodyLimit(self.diagnostic()),
            InboundTerminal::BodyIdle => {
                RuntimeError::DeadlineExceeded(DeadlineBoundary::RequestBodyIdle)
            }
            InboundTerminal::RequestTotal => {
                RuntimeError::DeadlineExceeded(DeadlineBoundary::RequestTotal)
            }
            InboundTerminal::ShutdownDeadline | InboundTerminal::ForcedCancellation => {
                RuntimeError::DeadlineExceeded(DeadlineBoundary::AggregateShutdown)
            }
            InboundTerminal::Disconnect => RuntimeError::ChannelClosed,
            InboundTerminal::ResponseHead => RuntimeError::RequestBodyUnreadable(self.diagnostic()),
        }
    }

    /// The account this failure reports, or a stated absence.
    ///
    /// A row whose producer minted no account is still described: the terminal
    /// names the bound, and the diagnostic says which owner observed it rather
    /// than leaving an operator an empty string to read.
    fn diagnostic(&self) -> Box<str> {
        match &self.account {
            Some(account) => account.clone(),
            None => format!("{} transfer ended on {:?}", self.label(), self.terminal).into(),
        }
    }

    /// The direction's name, for the one diagnostic that states it.
    const fn label(&self) -> &'static str {
        match self.direction {
            TransferDirection::Upload => "upload",
            TransferDirection::Download => "download",
        }
    }
}

impl From<TransferFailure> for BodyError {
    /// A post-commit transfer terminal ends its body and nothing else.
    ///
    /// No mapper runs and no status is rewritten: the head is already on the
    /// wire, so the answer the peer gets is its transport's — HTTP/1 closes
    /// because the framing cannot continue safely, and HTTP/2 resets the one
    /// affected stream. What travels here is only the typed direction and
    /// boundary an operator reads afterwards.
    fn from(failure: TransferFailure) -> Self {
        Self::Transfer {
            direction: failure.label(),
            terminal: failure.terminal,
            diagnostic: failure.diagnostic(),
        }
    }
}
