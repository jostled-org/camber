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

    /// An operation exceeded its configured timeout.
    #[error("operation timed out")]
    Timeout,

    /// Cooperative cancellation was requested.
    #[error("operation cancelled")]
    Cancelled,

    /// A spawned task unwound with a panic payload.
    #[error("task panicked: {0}")]
    TaskPanicked(Box<str>),

    /// An HTTP client, server, or protocol-level failure occurred.
    #[error("http error: {0}")]
    Http(Arc<str>),

    /// The caller supplied invalid request data.
    #[error("bad request: {0}")]
    BadRequest(Box<str>),

    /// Database interaction failed.
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

/// Returns true for IO error kinds that are expected during normal
/// operation (client disconnects, resets, broken pipes).
pub(crate) fn is_benign_io(err: &io::Error) -> bool {
    matches!(
        err.kind(),
        io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
            | io::ErrorKind::NotConnected
    )
}

/// Returns true for `RuntimeError::Io` variants wrapping benign IO errors.
pub(crate) fn is_benign_io_error(err: &RuntimeError) -> bool {
    match err {
        RuntimeError::Io(e) => is_benign_io(e),
        _ => false,
    }
}

/// POSIX error codes for file descriptor exhaustion.
const EMFILE: i32 = 24; // per-process fd limit
const ENFILE: i32 = 23; // system-wide fd limit

/// Returns true for transient accept errors (fd exhaustion) that should
/// trigger a backoff rather than crashing the server.
pub(crate) fn is_transient_accept_error(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(EMFILE | ENFILE))
}
