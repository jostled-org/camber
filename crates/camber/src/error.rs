use std::io;
use std::sync::Arc;

/// Common error type used across Camber runtime, HTTP, and support modules.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// Wrapper for underlying I/O failures.
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    /// A channel send or receive failed because the other side was dropped.
    #[error("channel closed")]
    ChannelClosed,

    /// A non-blocking channel send failed because the buffer is full.
    #[error("channel full")]
    ChannelFull,

    /// A blocking operation was requested from a single-thread async executor.
    #[error("blocking operation is unavailable in a current-thread async context")]
    BlockingInAsyncContext,

    /// A direct WebSocket operation found its connection already ended.
    ///
    /// Carries the connection's one immutable cause, which every sender clone
    /// and its receive owner read alike. Held apart from
    /// [`RuntimeError::ChannelClosed`] because the cause is what an application
    /// acts on: a peer that closed, a server that was cancelled, and a half
    /// that was dropped are three different answers, and a channel result flattens
    /// them into one.
    #[cfg(feature = "ws")]
    #[error("websocket closed: {0}")]
    WebSocketClosed(crate::http::WsCloseCause),

    /// An operation exceeded its configured timeout.
    #[error("operation timed out")]
    Timeout,

    /// A service operation crossed one of its configured deadlines.
    ///
    /// Held apart from [`RuntimeError::Timeout`] because the bound is what an
    /// operator acts on: a slow request body, a request that outlived its total,
    /// and a stalled upstream are three different answers, and one untyped
    /// "timed out" flattens them into one. The boundary is the same closed
    /// vocabulary the policy that configured it is written in.
    #[error("deadline exceeded: {0}")]
    DeadlineExceeded(crate::http::DeadlineBoundary),

    /// A buffered collection crossed the byte maximum its policy configured.
    ///
    /// The counterpart of [`RuntimeError::DeadlineExceeded`] for the other half
    /// of the same vocabulary: an oversized outbound response, upstream body,
    /// static file, or rendered profile names the bound it crossed, so an
    /// operator reads which maximum to widen rather than that "something was
    /// too big". The crossing bytes are never retained, and the payload that
    /// was retained before it is released with this failure.
    #[error("byte limit exceeded: {0}")]
    LimitExceeded(crate::http::ByteBoundary),

    /// Cooperative cancellation was requested.
    #[error("operation cancelled")]
    Cancelled,

    /// A spawned task unwound with a panic payload.
    #[error("task panicked: {0}")]
    TaskPanicked(Box<str>),

    /// A runtime-requiring entry point was called with no runtime context.
    #[error("no runtime context is established")]
    NoRuntime,

    /// Admission was attempted at or after the root scope's close transition.
    #[error("task scope is closed to admission")]
    ScopeClosed,

    /// The bounded scope drain expired before every child exited on its own,
    /// carrying how many the boundary found outstanding. Only the async subset
    /// of that count was then aborted and joined: a non-preemptible blocking
    /// child is counted here and cannot be force-stopped at all. So this counts
    /// children that failed to exit cooperatively — not children still running
    /// when `run` returns, and not children the drain managed to stop.
    #[error("scope drain timed out; children outstanding: {0}")]
    ScopeDrainTimeout(usize),

    /// An HTTP client, server, or protocol-level failure occurred.
    #[error("http error: {0}")]
    Http(Arc<str>),

    /// The caller supplied invalid request data.
    #[error("bad request: {0}")]
    BadRequest(Box<str>),

    /// A request body could not be parsed as its declared representation.
    ///
    /// Carries the parser's own account of the failure, which is operator
    /// detail: the HTTP rejection boundary answers the peer with fixed safe
    /// text and never with this string.
    #[error("malformed request body: {0}")]
    MalformedBody(Box<str>),

    /// A `multipart/form-data` body could not be parsed.
    ///
    /// Held apart from [`RuntimeError::MalformedBody`] so the two parsers stay
    /// distinguishable through a handler's `?`, rather than being told apart
    /// later by their text.
    #[error("invalid multipart body: {0}")]
    Multipart(Box<str>),

    /// A request body crossed the byte maximum its route admitted it under.
    ///
    /// Held apart from [`RuntimeError::Multipart`] because nothing about the
    /// body was malformed: it was well-formed and too large, and the two answer
    /// a peer differently.
    ///
    /// Carries the bound that was crossed, which is operator detail; the HTTP
    /// rejection boundary answers the peer with fixed safe text.
    #[error("request body limit exceeded: {0}")]
    RequestBodyLimit(Box<str>),

    /// An incoming request body could not be read from the transport.
    ///
    /// The peer's connection failed mid-upload. Nothing was refused and nothing
    /// was malformed, so this is neither a limit nor a parse failure.
    #[error("request body unreadable: {0}")]
    RequestBodyUnreadable(Box<str>),

    /// A database interaction failed.
    ///
    /// Camber never constructs this variant — it ships no database layer. It is
    /// provided so an application's own data access code can report through the
    /// same `RuntimeError` its handlers already return, instead of introducing a
    /// second error type and a conversion at every `?`.
    #[error("database error: {0}")]
    Database(Box<str>),

    /// TLS setup or handshake failed.
    #[error("tls error: {0}")]
    Tls(Box<str>),

    /// A public API was called with an invalid argument.
    #[error("invalid argument: {0}")]
    InvalidArgument(Box<str>),

    /// Schedule parsing or execution setup failed.
    #[error("schedule error: {0}")]
    Schedule(Box<str>),

    /// A message queue transport or protocol error occurred.
    #[error("message queue error: {0}")]
    MessageQueue(Box<str>),

    /// Configuration loading or validation failed.
    #[error("config error: {0}")]
    Config(Box<str>),

    /// Secret loading or decoding failed.
    #[error("secret error: {0}")]
    Secret(Box<str>),

    /// DNS provider or lookup handling failed.
    #[error("dns error: {0}")]
    Dns(Box<str>),

    /// ACME certificate provisioning or renewal failed.
    #[error("acme error: {0}")]
    Acme(Box<str>),
}

/// The IO error kind a `RuntimeError` wraps, or `None` for every other
/// variant.
///
/// One definition of the `RuntimeError::Io` unwrap, so each kind test below
/// states only the kinds it accepts and none of them re-derives which variant
/// carries a kind at all.
fn io_kind(err: &RuntimeError) -> Option<io::ErrorKind> {
    match err {
        RuntimeError::Io(e) => Some(e.kind()),
        _ => None,
    }
}

/// Returns true for IO error kinds that are expected during normal
/// operation (client disconnects, resets, broken pipes).
fn is_benign_kind(kind: io::ErrorKind) -> bool {
    matches!(
        kind,
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
    )
}

/// Returns true for a raw IO error that is expected during normal operation.
pub(crate) fn is_benign_io(err: &io::Error) -> bool {
    is_benign_kind(err.kind())
}

/// Returns true for `RuntimeError::Io` variants wrapping benign IO errors.
pub(crate) fn is_benign_io_error(err: &RuntimeError) -> bool {
    matches!(io_kind(err), Some(kind) if is_benign_kind(kind))
}

/// Returns true for `RuntimeError::Io` variants wrapping a datagram receive
/// failure that ends one datagram rather than the socket.
///
/// A socket that has had `connect` called on it reports a prior send's ICMP
/// port-unreachable as `ConnectionRefused` (or `ConnectionReset`) on the next
/// receive, and a signal can cut the syscall short with `Interrupted`. The
/// binding survives all three, so a recv loop continues past them.
pub(crate) fn is_transient_datagram_error(err: &RuntimeError) -> bool {
    matches!(
        io_kind(err),
        Some(
            io::ErrorKind::ConnectionRefused
                | io::ErrorKind::ConnectionReset
                | io::ErrorKind::Interrupted
        )
    )
}

/// POSIX error codes for file descriptor exhaustion.
const EMFILE: i32 = 24; // per-process fd limit
const ENFILE: i32 = 23; // system-wide fd limit

/// Returns true for transient accept errors (fd exhaustion) that should
/// trigger a backoff rather than crashing the server.
pub(crate) fn is_transient_accept_error(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(EMFILE | ENFILE))
}
