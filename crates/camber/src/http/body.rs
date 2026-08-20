use super::completion::Completion;
use super::disconnect::ResponseGuard;
use super::operation::InboundTerminal;
use super::transfer::{ChannelSource, Transfer, TransferOwner, UpstreamSource};

#[derive(Debug, thiserror::Error)]
pub(super) enum BodyError {
    /// One streaming upstream read failed, carrying the account of why.
    ///
    /// Owned rather than shared: the account is minted by the streamer, sent
    /// down one channel, and read exactly once by the transfer source that
    /// receives it. Nothing ever holds a second handle to it, so a shared
    /// allocation would buy a reference count no reader needs — which is why
    /// [`SourceStep::Failed`](super::transfer::SourceStep::Failed) spells the
    /// identical payload the same way.
    #[error("upstream proxy body read failed: {0}")]
    UpstreamProxy(Box<str>),
    /// One committed response body ended on a bound its download owner
    /// enforced.
    ///
    /// The head is already on the wire, so this replaces no status and reaches
    /// no mapper. It is what tells the transport that the framing cannot
    /// continue: HTTP/1 closes the connection, and HTTP/2 resets the one stream
    /// it belongs to.
    #[error("{direction} transfer ended on {terminal:?}: {diagnostic}")]
    Transfer {
        direction: &'static str,
        terminal: InboundTerminal,
        diagnostic: Box<str>,
    },
}

/// Response body that owns this response's disconnect guard and its account.
///
/// The guard moves here from the per-request service future, so the body is
/// its final holder: a service future dropped after handing off a response can
/// no longer resolve anything. The body establishes `Completed` when its
/// content has been produced to Hyper.
///
/// It is therefore also this operation's true terminal, which is why the
/// account the answering exit staged is recorded from here and nowhere else. A
/// record written at the head would name a streaming, proxied, gRPC, or
/// upgraded request as finished while every byte of it was still owed.
pub(super) struct GuardedBody {
    inner: HyperResponseBody,
    guard: ResponseGuard,
    /// Bytes left to produce when the length is known before the first poll.
    /// `None` for a body whose length only its end-of-stream can report.
    remaining: Option<u64>,
    /// The account this response owes, until its terminal writes it.
    ///
    /// `None` once recorded, and `None` from the start for a response no Camber
    /// exit answered.
    completion: Option<Completion>,
    /// The terminal this body itself fixed, if a bound ended it.
    fixed: Option<InboundTerminal>,
}

impl GuardedBody {
    /// Move the armed guard and the staged account into the response's body.
    ///
    /// `bodyless_request` is the one fact about the request the body cannot
    /// read off itself: a `HEAD` gets a response Hyper refuses to write a body
    /// for, whatever that body reports about itself.
    pub(super) fn attach(
        response: hyper::Response<HyperResponseBody>,
        guard: ResponseGuard,
        bodyless_request: bool,
        completion: Option<Completion>,
    ) -> hyper::Response<Self> {
        use hyper::body::Body;
        let (parts, inner) = response.into_parts();
        let remaining = inner
            .size_hint()
            .exact()
            .or_else(|| declared_content_length(&parts.headers));
        let body = Self {
            inner,
            guard,
            remaining,
            completion,
            fixed: None,
        };
        // Both halves of "Hyper will not poll this body". The report is asked
        // of the wrapper, which forwards it, so it is the exact report Hyper
        // reads; the countdown covers the body that reports nothing about
        // itself and declares every byte it will ever produce in its headers.
        let nothing_to_produce = body.is_end_stream() || body.remaining == Some(0);
        match completes_at_construction(parts.status, nothing_to_produce, bodyless_request) {
            true => body.guard.complete(),
            false => {}
        }
        hyper::Response::from_parts(parts, body)
    }

    /// Count a produced frame against a known content length.
    ///
    /// Hyper stops polling a body once it has every byte of a declared content
    /// length, so for a buffered response the last frame — not a trailing
    /// `None` — is the point where the body finishes being produced.
    fn produced(&mut self, frame: &hyper::body::Frame<bytes::Bytes>) {
        let bytes = frame.data_ref().map_or(0, |data| data.len() as u64);
        self.remaining = self
            .remaining
            .map(|remaining| remaining.saturating_sub(bytes));
        match self.remaining {
            Some(0) => self.guard.complete(),
            _ => {}
        }
    }

    /// Resolve a frame the wrapped body has declared to be its last.
    ///
    /// The third way Hyper can stop polling, beside a trailing `None` and an
    /// exhausted content length: a body that reports end of stream alongside
    /// its final frame is not polled again. A gRPC answer is that shape — its
    /// last frame is tonic's trailer set, and the HTTP/2 encoder ends the
    /// stream on it — so without this the response produced in full would fall
    /// through to the cause table.
    fn last_frame(&mut self) {
        use hyper::body::Body;
        match self.inner.is_end_stream() {
            true => self.finished(),
            false => {}
        }
    }

    /// Resolve an end of stream against what the body still owed.
    ///
    /// End of stream completes only a body that owed nothing more. A body that
    /// declared a length and stopped short was not produced in full, so it
    /// falls through to the cause table rather than claiming completion. The
    /// streaming proxy is the reachable case: `content-length` is not
    /// hop-by-hop, so an upstream's declared length is forwarded to the peer,
    /// and an upstream body that dies mid-read closes the channel — an end of
    /// stream with bytes still owed.
    ///
    /// The tradeoff is that a response declaring MORE bytes than it produces
    /// now resolves `PeerDisconnect` or `StreamReset` instead of `Completed`.
    /// That response is already a protocol violation Hyper fails the connection
    /// over, so non-completion is the honest answer.
    fn finished(&self) {
        match self.remaining {
            None | Some(0) => self.guard.complete(),
            Some(_) => {}
        }
    }

    /// Keep the typed terminal a bound this body enforced ended it on.
    ///
    /// Read off the error the transport is about to be given, so an operator's
    /// record and the peer's transport disposition name the same cause. Only a
    /// transfer terminal is one: an upstream read that failed is the source's
    /// own account, and the settled response lifetime names that row.
    fn ended_on(&mut self, error: &BodyError) {
        match error {
            BodyError::Transfer { terminal, .. } => self.fixed = Some(*terminal),
            BodyError::UpstreamProxy(_) => {}
        }
    }
}

impl Drop for GuardedBody {
    /// Record this operation's one account, at the terminal it actually reached.
    ///
    /// The lifetime is settled here rather than left to the guard's own drop,
    /// because the account has to name the cause: the guard below then finds it
    /// already resolved and leaves it alone, so one cause table still decides.
    fn drop(&mut self) {
        match self.completion.take() {
            Some(completion) => completion.record(self.guard.settle(), self.fixed),
            None => {}
        }
    }
}

impl hyper::body::Body for GuardedBody {
    type Data = bytes::Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        let outcome = std::pin::Pin::new(&mut this.inner).poll_frame(cx);
        match &outcome {
            std::task::Poll::Ready(None) => this.finished(),
            std::task::Poll::Ready(Some(Ok(frame))) => {
                this.produced(frame);
                this.last_frame();
            }
            std::task::Poll::Ready(Some(Err(error))) => this.ended_on(error),
            std::task::Poll::Pending => {}
        }
        outcome
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }

    /// Report the wrapped body's end-of-stream unchanged.
    ///
    /// A wrapper that hid it would make Hyper poll a body with nothing to
    /// produce; reporting it is what lets an empty response go out with no
    /// poll at all, which is why `attach` completes such a body itself.
    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }
}

/// The content length a response declares to the peer.
///
/// Hyper's HTTP/1 encoder is chosen from the HEADERS, not from the body's size
/// hint: a body that reports no exact length paired with a `content-length`
/// header still produces a length-delimited write, and Hyper clears the body
/// the moment that many bytes are written — without a final poll. A streaming
/// body forwarding an upstream `content-length` would therefore never see
/// `Ready(None)`, so the declared length is its only countdown. Read second,
/// after the exact hint, matching Hyper's own precedence.
fn declared_content_length(headers: &hyper::HeaderMap) -> Option<u64> {
    headers
        .get(hyper::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse().ok())
}

/// Whether a response is already produced the moment its head is written.
///
/// Three cases are, and they are not the same question. A body with nothing
/// left to produce is one: Hyper skips polling it, so nothing later would run
/// to establish completion. Two sources answer "nothing left", because Hyper
/// reads two. The body's own end-of-stream report is the first, and the rule
/// keys on that report rather than on a zero-length size hint, because the
/// report is the thing Hyper acts on. The HTTP/1 encoder is the second: it is
/// chosen from the headers, so a body that reports nothing about itself and
/// declares `content-length: 0` gets `Encoder::length(0)`, which refuses the
/// body and drops it unpolled — a streaming response forwarding an empty
/// upstream is exactly that shape, and has no later completion point either.
///
/// The other two are decided by the exchange, not by the body. Hyper refuses to
/// write a body at all for a `HEAD` request and for a `204` or `304`, and it
/// refuses it before the first `poll_frame`: it drops the body unpolled. So
/// neither source above can carry those cases. A buffered response survives
/// on shape alone — `strip_body_if_head` empties it, so it reports
/// end-of-stream — while a streaming response deliberately reports the
/// opposite, and would be left with no completion point at all and a spurious
/// disconnect cause for a response that was produced in full.
///
/// A `101` is the exception in the other direction. Its empty body says nothing
/// about the response lifetime — that lifetime ends when the upgraded transport
/// is handed to the WebSocket subsystem, which `ws_proxy` resolves explicitly.
/// Completing here would take that transition away from the handoff that owns
/// it and hand it to a rule about body shape.
fn completes_at_construction(
    status: hyper::StatusCode,
    nothing_to_produce: bool,
    bodyless_request: bool,
) -> bool {
    match status {
        hyper::StatusCode::SWITCHING_PROTOCOLS => false,
        hyper::StatusCode::NO_CONTENT | hyper::StatusCode::NOT_MODIFIED => true,
        _ => nothing_to_produce || bodyless_request,
    }
}

/// Streaming body backed by an mpsc channel.
///
/// Its end of stream — not its producer closing the channel — is the streaming
/// completion point. `poll_recv` yields every queued chunk before it reports
/// `None`, so the `None` the wrapping [`GuardedBody`] completes on can only
/// arrive once the last chunk has been produced to Hyper.
///
/// It deliberately does not report `is_end_stream` for a drained channel: a
/// body Hyper skips polling would never reach the `None` that establishes
/// completion, which is exactly what a refused producer's empty stream needs.
/// `Drained` is that same nothing-to-produce stream stated directly — a HEAD
/// response or a builder-failure fallback has no producer to hold a channel
/// open for, and allocating one purely to close it buys nothing.
pub(super) enum StreamBody {
    Channel(Box<TransferBody<ChannelSource>>),
    Proxy(Box<TransferBody<UpstreamSource>>),
    Drained,
}

impl StreamBody {
    /// One channel-backed response body under the download owner bounding it.
    pub(super) fn channel(
        owner: TransferOwner,
        receiver: tokio::sync::mpsc::Receiver<bytes::Bytes>,
    ) -> Self {
        Self::Channel(Box::new(TransferBody::new(
            owner.over(ChannelSource::new(receiver)),
        )))
    }

    /// One upstream-backed response body under the download owner bounding it.
    pub(super) fn upstream(
        owner: TransferOwner,
        receiver: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, BodyError>>,
    ) -> Self {
        Self::Proxy(Box::new(TransferBody::new(
            owner.over(UpstreamSource::new(receiver)),
        )))
    }
}

/// One streaming response body, under the one download owner that bounds it.
///
/// Held behind one allocation by both [`StreamBody`] variants. The owner carries
/// a guard, a peer-lifetime wait, and a deadline wake, which inline would set the
/// width of every response body this server builds — including the buffered ones
/// that stream nothing. One allocation per streaming response buys that back.
///
/// The owner is what Hyper actually polls: bytes, quiet interval, lifetime,
/// cancellation, and the peer's response lifetime are all decided inside it, and
/// what reaches Hyper is either the frame it admitted or the one terminal it
/// fixed. A terminal is reported once, and the body reports end of stream
/// afterwards rather than a second failure, so no transport is asked to fail
/// twice for one transfer.
pub(super) struct TransferBody<S> {
    transfer: Transfer<S>,
    reported: bool,
}

impl<S> TransferBody<S> {
    const fn new(transfer: Transfer<S>) -> Self {
        Self {
            transfer,
            reported: false,
        }
    }
}

impl<S: super::transfer::TransferSource> TransferBody<S> {
    /// Produce the next frame this body was admitted to write.
    fn poll_body(&mut self, cx: &mut std::task::Context<'_>) -> BodyFramePoll {
        if self.reported {
            return std::task::Poll::Ready(None);
        }
        match self.transfer.poll_transfer(cx) {
            std::task::Poll::Pending => std::task::Poll::Pending,
            std::task::Poll::Ready(Ok(None)) => std::task::Poll::Ready(None),
            std::task::Poll::Ready(Ok(Some(data))) => {
                std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(data))))
            }
            std::task::Poll::Ready(Err(failure)) => {
                self.reported = true;
                std::task::Poll::Ready(Some(Err(BodyError::from(failure))))
            }
        }
    }
}

type BodyFramePoll = std::task::Poll<Option<Result<hyper::body::Frame<bytes::Bytes>, BodyError>>>;

fn impossible_body_error(never: std::convert::Infallible) -> BodyError {
    match never {}
}

fn map_infallible_frame(
    poll: std::task::Poll<
        Option<Result<hyper::body::Frame<bytes::Bytes>, std::convert::Infallible>>,
    >,
) -> BodyFramePoll {
    match poll {
        std::task::Poll::Ready(Some(Ok(frame))) => std::task::Poll::Ready(Some(Ok(frame))),
        std::task::Poll::Ready(Some(Err(never))) => {
            std::task::Poll::Ready(Some(Err(impossible_body_error(never))))
        }
        std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
        std::task::Poll::Pending => std::task::Poll::Pending,
    }
}

impl hyper::body::Body for StreamBody {
    type Data = bytes::Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            StreamBody::Channel(body) => body.poll_body(cx),
            StreamBody::Proxy(body) => body.poll_body(cx),
            StreamBody::Drained => std::task::Poll::Ready(None),
        }
    }
}

pub(super) enum HyperResponseBody {
    Full(http_body_util::Full<bytes::Bytes>),
    Streaming(StreamBody),
    /// One committed gRPC answer, under the download owner bounding it.
    ///
    /// Held behind one allocation for the reason [`StreamBody`]'s variants are:
    /// the owner carries a guard, a peer-lifetime wait, and a deadline wake,
    /// which inline would set the width of every response body this server
    /// builds — including the buffered ones that stream nothing.
    #[cfg(feature = "grpc")]
    Grpc(Box<super::grpc_handoff::GrpcDownload>),
}

impl hyper::body::Body for HyperResponseBody {
    type Data = bytes::Bytes;
    type Error = BodyError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        match self.get_mut() {
            HyperResponseBody::Full(body) => {
                map_infallible_frame(std::pin::Pin::new(body).poll_frame(cx))
            }
            HyperResponseBody::Streaming(body) => std::pin::Pin::new(body).poll_frame(cx),
            #[cfg(feature = "grpc")]
            HyperResponseBody::Grpc(body) => std::pin::Pin::new(body.as_mut()).poll_frame(cx),
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        match self {
            HyperResponseBody::Full(body) => body.size_hint(),
            HyperResponseBody::Streaming(body) => body.size_hint(),
            #[cfg(feature = "grpc")]
            HyperResponseBody::Grpc(body) => body.size_hint(),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            HyperResponseBody::Full(body) => body.is_end_stream(),
            HyperResponseBody::Streaming(body) => body.is_end_stream(),
            #[cfg(feature = "grpc")]
            HyperResponseBody::Grpc(body) => body.is_end_stream(),
        }
    }
}
