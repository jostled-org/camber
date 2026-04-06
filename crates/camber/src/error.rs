use std::io;
use std::sync::Arc;

#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),

    #[error("channel closed")]
    ChannelClosed,

    #[error("channel full")]
    ChannelFull,

    #[error("operation timed out")]
    Timeout,

    #[error("operation cancelled")]
    Cancelled,

    #[error("task panicked: {0}")]
    TaskPanicked(Box<str>),

    #[error("http error: {0}")]
    Http(Arc<str>),

    #[error("bad request: {0}")]
    BadRequest(Box<str>),

    #[error("database error: {0}")]
    Database(Box<str>),

    #[error("tls error: {0}")]
    Tls(Box<str>),

    #[error("invalid argument: {0}")]
    InvalidArgument(Box<str>),

    #[error("schedule error: {0}")]
    Schedule(Box<str>),

    #[error("message queue error: {0}")]
    MessageQueue(Box<str>),

    #[error("config error: {0}")]
    Config(Box<str>),

    #[error("secret error: {0}")]
    Secret(Box<str>),

    #[error("dns error: {0}")]
    Dns(Box<str>),

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
