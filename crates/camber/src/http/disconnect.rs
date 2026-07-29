//! Per-response disconnect signalling.
//!
//! Each in-flight request owns a response lifetime. The handler observes it
//! through [`Request::on_disconnect`](super::Request::on_disconnect), which
//! hands back a [`DisconnectSignal`] whose clones all resolve once, to the
//! same [`DisconnectCause`].
//!
//! For every body Camber produces, the producer side is a single armed guard
//! per response. The per-request service future holds it while the handler
//! runs; when a response is produced, ownership moves into that response's
//! body, so only the final holder can resolve. Hyper drops the per-request
//! future on peer close and on `RST_STREAM`, which is what makes the drop
//! observable before headers are written and per HTTP/2 stream.
//!
//! Two handoffs hand the body away instead of producing it, and they resolve
//! `Completed` through `DisconnectSignal::complete` rather than through the
//! guard: a gRPC response, whose body tonic owns, and a successful `101`,
//! whose response lifetime ends when `ws_proxy` passes the upgraded transport
//! to the WebSocket subsystem. Both still carry a guard — it resolves nothing,
//! because the terminal cause is already set by the time it drops.

use super::server_lifecycle::ServerControl;
use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};

/// Why a response lifetime ended.
///
/// `Completed` means the response body was fully produced to Hyper, not that
/// the last byte reached the peer: Hyper exposes frame production, not
/// transport delivery.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DisconnectCause {
    /// The peer closed the connection, or the transport failed.
    ///
    /// Read EOF is what marks a connection terminating, and a client that
    /// half-closes with `shutdown(WR)` after sending its request produces that
    /// same EOF while it still waits for the response. A completed response
    /// wins the cause table, so the reach of this is one still-pending
    /// handler: it observes `PeerDisconnect` while a live client is still
    /// waiting for the answer. The transport offers no clean way to tell a
    /// half-close from a full one, so this is a documented limit of the
    /// signal rather than a case the table tries to discriminate.
    PeerDisconnect,
    /// This request ended early while its connection stayed live. An HTTP/2
    /// `RST_STREAM` on this stream alone is the canonical source.
    ///
    /// It is also the residual row of the cause table: the connection is not
    /// terminating, the server is not shutting down, and no completion was
    /// recorded. Any per-request future that ends without producing its
    /// response over a still-live connection resolves here, on either protocol
    /// version. Two cases reach it beyond `RST_STREAM`. A panicking handler,
    /// whose unwind nothing in the HTTP module catches, is one. The other is a
    /// connection Hyper fails for a protocol reason — a malformed HTTP/2 frame,
    /// an oversized header on a later keep-alive request — because Hyper drops
    /// the per-request futures inside its own poll and only then reports the
    /// error: no read EOF and no write error occurred, so nothing marked the
    /// connection terminating while the guards were still alive.
    StreamReset,
    /// The server or runtime began shutting down.
    ServerShutdown,
    /// The response body finished being produced to Hyper.
    Completed,
}

/// Shared terminal state behind every clone of one response's signal.
#[derive(Debug)]
struct DisconnectState {
    cause: OnceLock<DisconnectCause>,
    notify: tokio::sync::Notify,
}

impl DisconnectState {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            cause: OnceLock::new(),
            notify: tokio::sync::Notify::new(),
        })
    }

    /// Record the terminal cause. The first transition wins; later ones are
    /// no-ops, which is row 1 of the cause table.
    fn resolve(&self, cause: DisconnectCause) {
        match self.cause.set(cause) {
            Ok(()) => self.notify.notify_waiters(),
            Err(_) => {}
        }
    }

    /// Wait for the terminal cause.
    ///
    /// Registration precedes the read of the set-once cell, so a resolution
    /// landing in the check/register gap still wakes this waiter.
    async fn cancelled(&self) -> DisconnectCause {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            match self.cause.get() {
                Some(cause) => return *cause,
                None => notified.await,
            }
        }
    }
}

/// Handle on one response's lifetime, obtained from
/// [`Request::on_disconnect`](super::Request::on_disconnect).
///
/// Cloning is a refcount bump; every clone shares one terminal cause. A
/// request built outside a served connection — through
/// [`Request::builder`](super::Request::builder) — carries a signal that never
/// resolves, because it has no transport to lose.
///
/// Hold the signal somewhere that outlives the handler. Hyper drops the
/// per-request future when the peer goes away — that drop is the observation —
/// so a handler awaiting its own signal is cancelled rather than woken.
///
/// ```rust,no_run
/// use camber::RuntimeError;
/// use camber::http::{DisconnectCause, Request, Response};
///
/// async fn handler(req: &Request) -> Result<Response, RuntimeError> {
///     let disconnect = req.on_disconnect();
///     camber::spawn_async(async move {
///         match disconnect.cancelled().await {
///             DisconnectCause::Completed => {}
///             _ => { /* release subprocesses, cursors, permits */ }
///         }
///     });
///     Response::text(200, "done")
/// }
/// ```
#[derive(Clone, Debug)]
pub struct DisconnectSignal {
    state: Arc<DisconnectState>,
}

impl DisconnectSignal {
    /// A signal with no producer. Nothing can resolve it, which is the correct
    /// contract for a request that was never served over a connection.
    pub(super) fn detached() -> Self {
        Self {
            state: DisconnectState::new(),
        }
    }

    /// Resolve once, to the terminal cause of this response lifetime.
    ///
    /// Every clone of one request's signal resolves to the same cause.
    pub async fn cancelled(&self) -> DisconnectCause {
        self.state.cancelled().await
    }

    /// Establish `Completed` from a handoff Camber cannot observe the body of.
    ///
    /// Two handoffs are that case. tonic owns a gRPC response body, so its
    /// frames never pass through a Camber body that could complete the signal.
    /// A successful `101` has an empty body that says nothing about the
    /// response lifetime, which ends when the upgraded transport passes to the
    /// WebSocket subsystem — so `ws_proxy` resolves it at that handoff instead.
    #[cfg(any(feature = "grpc", feature = "ws"))]
    pub(super) fn complete(&self) {
        self.state.resolve(DisconnectCause::Completed);
    }
}

/// The connection's shutdown predicate — row 3 of the cause table.
///
/// Two concrete sources, one predicate: the synchronous path already holds the
/// runtime's latching shutdown flag, and the owned path already holds the
/// server's control watch. Both are read synchronously, so a shutdown request
/// is visible to every guard that drops after it.
enum ShutdownPredicate {
    Latch(Arc<AtomicBool>),
    Control(tokio::sync::watch::Receiver<ServerControl>),
}

impl ShutdownPredicate {
    fn is_shutting_down(&self) -> bool {
        match self {
            Self::Latch(flag) => flag.load(Ordering::Acquire),
            Self::Control(control) => !matches!(*control.borrow(), ServerControl::Running),
        }
    }
}

/// Per-connection state the cause table reads: whether the transport is dying
/// and whether the server is shutting down.
///
/// Created where the stream is accepted — never on the per-server connection
/// context, which every connection shares.
///
/// It is only ever reachable through an `Arc`: the stream wrapper and every
/// response guard hold one, and both entry points below take one. The
/// constructors hand back the `Arc` rather than the bare value so that
/// invariant is structural, and no caller has to remember to wrap.
pub(super) struct ConnectionLiveness {
    terminating: AtomicBool,
    shutdown: ShutdownPredicate,
}

impl ConnectionLiveness {
    /// Liveness for a connection whose shutdown source is a latching flag.
    pub(super) fn latched(flag: Arc<AtomicBool>) -> Arc<Self> {
        Self::with_predicate(ShutdownPredicate::Latch(flag))
    }

    /// Liveness for a connection driven by an owned server's control watch.
    ///
    /// The receiver is not optional: a connection with no shutdown authority
    /// could only be given an inert flag nothing sets, which would read as
    /// "not shutting down" for the whole of a shutdown and resolve every guard
    /// on it as `PeerDisconnect` or `StreamReset`. The owned path resolves the
    /// receiver once per connection instead, before it serves anything.
    pub(super) fn controlled(control: tokio::sync::watch::Receiver<ServerControl>) -> Arc<Self> {
        Self::with_predicate(ShutdownPredicate::Control(control))
    }

    fn with_predicate(shutdown: ShutdownPredicate) -> Arc<Self> {
        Arc::new(Self {
            terminating: AtomicBool::new(false),
            shutdown,
        })
    }

    /// Wrap the raw stream so read EOF, read error, and write error mark this
    /// connection terminating before Hyper observes the same condition.
    ///
    /// The wrapper holds the connection's own handle, the same way the guards
    /// do: liveness only exists behind an `Arc`, so reaching `terminating`
    /// through it costs one refcount rather than a second allocation for a
    /// field the outer handle already owns.
    pub(super) fn wrap<S>(self: &Arc<Self>, stream: S) -> LivenessStream<S> {
        LivenessStream {
            inner: stream,
            connection: Arc::clone(self),
        }
    }

    /// Create one request's signal and its armed guard.
    ///
    /// It consumes the per-request handle the service closure already cloned,
    /// so arming a response costs one allocation — the shared terminal state —
    /// and no refcount traffic beyond it. The guard reads the cause table
    /// through this handle rather than through per-field clones of it.
    pub(super) fn begin_response(self: Arc<Self>) -> (DisconnectSignal, ResponseGuard) {
        let state = DisconnectState::new();
        let guard = ResponseGuard {
            state: Arc::clone(&state),
            connection: self,
        };
        (DisconnectSignal { state }, guard)
    }
}

/// The single armed guard for one response.
///
/// Ownership moves from the per-request service future into the response body;
/// a Rust move leaves no second holder, so exactly one drop can resolve. On
/// drop it applies the first-wins cause table over preconditions Camber owns.
///
/// It reads those preconditions through the connection's own handle. The
/// service future held one for this request already, so the guard borrows the
/// whole cause table for what per-field clones cost for one field of it.
pub(super) struct ResponseGuard {
    state: Arc<DisconnectState>,
    connection: Arc<ConnectionLiveness>,
}

impl ResponseGuard {
    /// Establish the normal completion point.
    pub(super) fn complete(&self) {
        self.state.resolve(DisconnectCause::Completed);
    }

    /// Rows 3 through 5: shutdown, then a dying transport, then a stream that
    /// ended early on a connection that is still live.
    fn cause(&self) -> DisconnectCause {
        match (
            self.connection.shutdown.is_shutting_down(),
            self.connection.terminating.load(Ordering::Acquire),
        ) {
            (true, _) => DisconnectCause::ServerShutdown,
            (false, true) => DisconnectCause::PeerDisconnect,
            (false, false) => DisconnectCause::StreamReset,
        }
    }
}

impl Drop for ResponseGuard {
    fn drop(&mut self) {
        // Row 1 of the cause table, spelled out: an already-resolved lifetime
        // has nothing left to decide, and it is the common case — every
        // completed response passes here. Reading the table first would take a
        // watch borrow and an atomic load per response only to throw both away.
        match self.state.cause.get() {
            Some(_) => {}
            None => self.state.resolve(self.cause()),
        }
    }
}

/// Raw-stream wrapper that records transport death before returning it upward.
///
/// It sits inside the Hyper IO adapter, at the slot the owned path already
/// uses to interpose its own transport, so the connection-terminating flag is
/// set before Hyper reacts and drops the per-request futures.
pub(super) struct LivenessStream<S> {
    inner: S,
    connection: Arc<ConnectionLiveness>,
}

impl<S> LivenessStream<S> {
    fn mark_terminating(&self) {
        self.connection.terminating.store(true, Ordering::Release);
    }

    /// Record the flag for any read outcome that ends this connection.
    ///
    /// A successful zero-length read is EOF only when the buffer had room to
    /// read into: a poll against a buffer with no capacity returns the same
    /// shape with nothing read. The flag is one-way, so mistaking one for the
    /// other would mis-resolve every later guard on the connection.
    ///
    /// The question arrives already answered, as one value. This is a generic
    /// adapter, and both facts it is derived from belong to a consumer's buffer
    /// that this type does not own; the number of bytes on either side is never
    /// the question.
    ///
    /// One parameter and not two: two adjacent bools of the same type can be
    /// transposed with nothing to catch it, and the transposition inverts this
    /// wrapper. A real EOF has room and fills nothing, so the swapped pair
    /// would never match it, the connection would never be marked terminating
    /// on read EOF, and every HTTP/1 peer close would resolve `StreamReset`
    /// instead of `PeerDisconnect`. No test could catch the swap either: both
    /// values are derived at the one call site from the same buffer.
    fn observe_read(&self, outcome: &std::task::Poll<std::io::Result<()>>, reached_eof: bool) {
        match (outcome, reached_eof) {
            (std::task::Poll::Ready(Err(_)), _) | (std::task::Poll::Ready(Ok(())), true) => {
                self.mark_terminating();
            }
            _ => {}
        }
    }

    /// Record the flag for any write outcome that ends this connection.
    fn observe_write<T>(&self, outcome: &std::task::Poll<std::io::Result<T>>) {
        match outcome {
            std::task::Poll::Ready(Err(_)) => self.mark_terminating(),
            _ => {}
        }
    }
}

impl<S> tokio::io::AsyncRead for LivenessStream<S>
where
    S: tokio::io::AsyncRead + Unpin,
{
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buffer.filled().len();
        // Room has to be read before the poll: afterwards a filled buffer and
        // an EOF are the same observation.
        let had_room = buffer.remaining() > 0;
        let outcome = std::pin::Pin::new(&mut this.inner).poll_read(context, buffer);
        let reached_eof = had_room && buffer.filled().len() == before;
        this.observe_read(&outcome, reached_eof);
        outcome
    }
}

impl<S> tokio::io::AsyncWrite for LivenessStream<S>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let outcome = std::pin::Pin::new(&mut this.inner).poll_write(context, buffer);
        this.observe_write(&outcome);
        outcome
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let outcome = std::pin::Pin::new(&mut this.inner).poll_flush(context);
        this.observe_write(&outcome);
        outcome
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let outcome = std::pin::Pin::new(&mut this.inner).poll_shutdown(context);
        this.observe_write(&outcome);
        outcome
    }

    fn poll_write_vectored(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        let outcome = std::pin::Pin::new(&mut this.inner).poll_write_vectored(context, buffers);
        this.observe_write(&outcome);
        outcome
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}
