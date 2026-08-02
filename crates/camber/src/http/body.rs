use super::disconnect::ResponseGuard;

#[derive(Debug, thiserror::Error)]
pub(super) enum BodyError {
    #[error("upstream proxy body read failed: {0}")]
    UpstreamProxy(std::sync::Arc<str>),
}

/// Response body that owns this response's disconnect guard.
///
/// The guard moves here from the per-request service future, so the body is
/// its final holder: a service future dropped after handing off a response can
/// no longer resolve anything. The body establishes `Completed` when its
/// content has been produced to Hyper.
pub(super) struct GuardedBody {
    inner: HyperResponseBody,
    guard: ResponseGuard,
    /// Bytes left to produce when the length is known before the first poll.
    /// `None` for a body whose length only its end-of-stream can report.
    remaining: Option<u64>,
}

impl GuardedBody {
    /// Move the armed guard into the response's body.
    ///
    /// `bodyless_request` is the one fact about the request the body cannot
    /// read off itself: a `HEAD` gets a response Hyper refuses to write a body
    /// for, whatever that body reports about itself.
    pub(super) fn attach(
        response: hyper::Response<HyperResponseBody>,
        guard: ResponseGuard,
        bodyless_request: bool,
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
            std::task::Poll::Ready(Some(Ok(frame))) => this.produced(frame),
            _ => {}
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
    Channel(tokio::sync::mpsc::Receiver<bytes::Bytes>),
    Proxy(tokio::sync::mpsc::Receiver<Result<bytes::Bytes, BodyError>>),
    Drained,
}

type BodyFramePoll = std::task::Poll<Option<Result<hyper::body::Frame<bytes::Bytes>, BodyError>>>;

fn impossible_body_error(never: std::convert::Infallible) -> BodyError {
    match never {}
}

fn poll_byte_receiver(
    receiver: &mut tokio::sync::mpsc::Receiver<bytes::Bytes>,
    cx: &mut std::task::Context<'_>,
) -> BodyFramePoll {
    match receiver.poll_recv(cx) {
        std::task::Poll::Ready(Some(data)) => {
            std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(data))))
        }
        std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
        std::task::Poll::Pending => std::task::Poll::Pending,
    }
}

fn poll_proxy_receiver(
    receiver: &mut tokio::sync::mpsc::Receiver<Result<bytes::Bytes, BodyError>>,
    cx: &mut std::task::Context<'_>,
) -> BodyFramePoll {
    match receiver.poll_recv(cx) {
        std::task::Poll::Ready(Some(Ok(data))) => {
            std::task::Poll::Ready(Some(Ok(hyper::body::Frame::data(data))))
        }
        std::task::Poll::Ready(Some(Err(error))) => std::task::Poll::Ready(Some(Err(error))),
        std::task::Poll::Ready(None) => std::task::Poll::Ready(None),
        std::task::Poll::Pending => std::task::Poll::Pending,
    }
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
            StreamBody::Channel(receiver) => poll_byte_receiver(receiver, cx),
            StreamBody::Proxy(receiver) => poll_proxy_receiver(receiver, cx),
            StreamBody::Drained => std::task::Poll::Ready(None),
        }
    }
}

pub(super) enum HyperResponseBody {
    Full(http_body_util::Full<bytes::Bytes>),
    Streaming(StreamBody),
    #[cfg(feature = "grpc")]
    Grpc(GrpcBody),
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
            HyperResponseBody::Grpc(body) => {
                map_infallible_frame(std::pin::Pin::new(body).poll_frame(cx))
            }
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

/// Body wrapper for gRPC responses (tonic's UnsyncBoxBody).
#[cfg(feature = "grpc")]
pub(super) struct GrpcBody {
    pub(super) inner: tonic::body::Body,
    pub(super) finished: bool,
}

/// The same trait [`HyperResponseBody`] forwards to, not a private set of
/// look-alike methods.
///
/// Inherent methods left the forwarding hand-maintained: the wrapper called
/// whatever this type happened to define, so a report this type did not define
/// was silently answered by the trait's default instead of by tonic's body.
/// A trait impl makes each missing forward a compile error.
#[cfg(feature = "grpc")]
impl hyper::body::Body for GrpcBody {
    type Data = bytes::Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let this = self.get_mut();
        if this.finished {
            return std::task::Poll::Ready(None);
        }

        match std::pin::Pin::new(&mut this.inner).poll_frame(cx) {
            std::task::Poll::Ready(Some(Ok(frame))) => std::task::Poll::Ready(Some(Ok(frame))),
            std::task::Poll::Ready(Some(Err(status))) => {
                this.finished = true;
                std::task::Poll::Ready(Some(Ok(grpc_error_frame(&status))))
            }
            std::task::Poll::Ready(None) => {
                this.finished = true;
                std::task::Poll::Ready(None)
            }
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }

    fn size_hint(&self) -> hyper::body::SizeHint {
        self.inner.size_hint()
    }

    /// Ended once tonic's body has produced its last frame, or once tonic's
    /// own body says so.
    fn is_end_stream(&self) -> bool {
        match self.finished {
            true => true,
            false => self.inner.is_end_stream(),
        }
    }
}

#[cfg(feature = "grpc")]
fn grpc_error_frame(status: &tonic::Status) -> hyper::body::Frame<bytes::Bytes> {
    let mut trailers = hyper::HeaderMap::with_capacity(2);
    let code = status.code() as i32;
    trailers.insert("grpc-status", hyper::header::HeaderValue::from(code));
    if let Ok(message) = hyper::header::HeaderValue::from_str(status.message()) {
        trailers.insert("grpc-message", message);
    }
    hyper::body::Frame::trailers(trailers)
}
