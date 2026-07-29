use std::io;
use std::net::TcpStream;
use std::time::Duration;

#[derive(Debug, thiserror::Error)]
pub enum BoundedReadError {
    #[error("stream read timed out after {timeout:?} with {bytes_read} bytes read")]
    Timeout {
        timeout: Duration,
        bytes_read: usize,
    },
    #[error("stream exceeded the {limit}-byte read limit")]
    LimitExceeded { limit: usize },
    #[error("stream read failed: {0}")]
    Io(#[from] io::Error),
    #[error("supporting thread did not finish before {timeout:?}")]
    JoinTimeout { timeout: Duration },
    #[error("supporting thread panicked")]
    ThreadPanicked,
    /// The caller already took this handle, so there is no thread left to join.
    /// A different fault from a thread that panicked, and reported as one: a
    /// test that joins twice is asking about a thread it already has the answer
    /// for.
    #[error("supporting thread was already joined")]
    AlreadyJoined,
}

pub fn join_thread_bounded<T>(
    thread: &mut Option<std::thread::JoinHandle<T>>,
    timeout: Duration,
) -> Result<T, BoundedReadError> {
    match thread.as_ref() {
        None => return Err(BoundedReadError::AlreadyJoined),
        Some(handle) if super::http::poll_until(timeout, || handle.is_finished()) => {}
        Some(_) => return Err(BoundedReadError::JoinTimeout { timeout }),
    }
    thread
        .take()
        .expect("the wait only ends on a handle it observed finished")
        .join()
        .map_err(|_| BoundedReadError::ThreadPanicked)
}

/// Read a stream to closure under `timeout`, capped at `limit` bytes.
///
/// The read itself is the shared drain-to-close, so the framing, the chunk size,
/// the size cap, and the one deadline over the whole read are stated once for
/// the whole suite. Only the verdicts differ here: this caller reports the
/// expiry and the overflow as its own named faults rather than as `io::Error`
/// kinds, so a test can assert on which of them it hit.
pub fn bounded_read(
    stream: &mut TcpStream,
    timeout: Duration,
    limit: usize,
) -> Result<Box<[u8]>, BoundedReadError> {
    let mut bytes = Vec::new();
    let read = super::http::with_read_deadline(stream, timeout, |stream, deadline| {
        super::http::read_to_eof(stream, &mut bytes, limit, Some(deadline))
    });
    match read {
        Ok(()) => Ok(bytes.into_boxed_slice()),
        Err(error) if super::http::is_deadline_expiry(&error) => Err(BoundedReadError::Timeout {
            timeout,
            bytes_read: bytes.len(),
        }),
        // The drain-to-close produces exactly one `InvalidData`, and it is the
        // size cap. Reading the kind rather than the message keeps the two
        // modules from agreeing on a sentence.
        Err(error) if error.kind() == io::ErrorKind::InvalidData => {
            Err(BoundedReadError::LimitExceeded { limit })
        }
        Err(error) => Err(BoundedReadError::Io(error)),
    }
}
