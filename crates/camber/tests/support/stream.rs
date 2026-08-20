use camber::http::mock::{LifecycleController, TransferObservation};
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

/// Open one HTTP/1 request for a streaming response, and send its head.
///
/// The connection stays in the caller's hands: a post-commit claim is about what
/// the peer keeps receiving and about when the transport ends, neither of which a
/// request-and-answer helper can report.
pub async fn open_streaming(
    addr: std::net::SocketAddr,
    path: &str,
    bound: Duration,
) -> tokio::net::TcpStream {
    use tokio::io::AsyncWriteExt;

    let mut stream = tokio::time::timeout(bound, tokio::net::TcpStream::connect(addr))
        .await
        .expect("the streaming peer connected inside its bound")
        .expect("the streaming peer connected");
    let head = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream
        .write_all(head.as_bytes())
        .await
        .expect("the streaming request head was written");
    stream.flush().await.expect("the streaming request flushed");
    stream
}

/// Read one streaming response's head, leaving its body on the wire.
///
/// For the rows whose cause is the peer or the server rather than the producer:
/// each needs the head committed before it acts, and neither may consume the body
/// it is about to interrupt.
pub async fn read_streaming_head(stream: &mut tokio::net::TcpStream, bound: Duration) -> u16 {
    use tokio::io::AsyncReadExt;

    let deadline = tokio::time::Instant::now() + bound;
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        let read = tokio::time::timeout_at(deadline, stream.read(&mut byte))
            .await
            .expect("the committed head arrived inside its bound")
            .expect("the committed head was readable");
        assert!(read == 1, "the peer closed before its head was complete");
        head.push(byte[0]);
    }
    status_line(&head)
}

/// Read the rest of one streaming body, and report whether it was cut.
///
/// For the rows whose head was already taken off the wire: what is left to
/// establish is only how the body ended, and a cut is what a post-commit terminal
/// leaves behind.
///
/// The ending is read straight off the raw bytes rather than through
/// [`decoded`], because this caller's head is gone: a reader that split a head
/// off what is left would take the terminal chunk for one and call a body that
/// ended a cut.
pub async fn read_cut(stream: &mut tokio::net::TcpStream) -> bool {
    !chunks_ended(&drain_chunked(stream).await)
}

/// What one peer saw of a committed streaming response.
///
/// The three facts every post-commit claim is made of: the status that was
/// committed before anything could be rewritten, the payload that actually
/// reached the peer, and whether the body reached its terminal chunk or was cut
/// under the status already on the wire.
#[derive(Debug, Eq, PartialEq)]
pub struct Streamed {
    pub status: u16,
    pub bytes: usize,
    pub complete: bool,
}

/// Read one streaming HTTP/1 response, stopping at its terminal chunk or its cut.
///
/// A cut is what a post-commit terminal looks like on HTTP/1: the head is already
/// written, so the transport ends under it rather than replacing it. Both endings
/// are answers a case asserts on, so neither is treated as a fixture failure.
pub async fn read_streamed(stream: &mut tokio::net::TcpStream) -> Streamed {
    decoded(&drain_chunked(stream).await)
}

/// Read one chunked body off the wire until it ends, is cut, or outlasts its
/// bound.
///
/// The read is the same for both callers; only the reading of what came back
/// differs, and a second copy of the loop is a second place the bound below can
/// go missing.
async fn drain_chunked(stream: &mut tokio::net::TcpStream) -> Box<[u8]> {
    use tokio::io::AsyncReadExt;

    let deadline = tokio::time::Instant::now() + STREAMED_READ_BOUND;
    let mut raw = Vec::new();
    let mut buffer = [0_u8; 4096];
    loop {
        let read = tokio::time::timeout_at(deadline, stream.read(&mut buffer))
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "the streaming response neither ended nor was cut inside {STREAMED_READ_BOUND:?}: {:?}",
                    String::from_utf8_lossy(&raw)
                )
            });
        let read = match read {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        raw.extend_from_slice(&buffer[..read]);
        if chunks_ended(&raw) {
            break;
        }
    }
    raw.into_boxed_slice()
}

/// How long one streaming read waits for its body to end or be cut.
///
/// A bound rather than an open read, because both of this reader's endings are
/// production's: an owner that selected no terminal at all leaves the peer with
/// a producer that never stops and a transport that never closes. Without this,
/// that defect stops the suite instead of failing the row that staged it.
const STREAMED_READ_BOUND: Duration = Duration::from_secs(20);

/// Wait until one listener's download owner reaches `settled`, and report what
/// it was observed at.
///
/// Polled rather than read once: an owner fixes its terminal and releases its
/// source on turns of its own, so a row that read the record once would race the
/// turn that produces the fact it asserts on. `what` names the state the row
/// waited for, so an expired bound says which one never arrived, and the record
/// is returned either way so a row that never got there fails against what the
/// owner actually did.
pub fn download_observed(
    controller: &LifecycleController,
    row: &str,
    what: &str,
    settled: impl Fn(&TransferObservation) -> bool,
) -> TransferObservation {
    let reached = super::http::poll_until(DOWNLOAD_OBSERVED_BOUND, || {
        settled(&controller.transfers_observed())
    });
    let observed = controller.transfers_observed();
    assert!(
        reached,
        "{row}: the download owner never {what}: {observed:?}"
    );
    observed
}

/// Wait until one listener's download owner reached its release.
///
/// For the rows whose cause is the peer: a transport taken away can release the
/// owner before it weighs another turn, so the release is the whole claim. `row`
/// names the case, so the same wait serves a streamed response and a feed.
pub fn released_download(controller: &LifecycleController, row: &str) -> TransferObservation {
    download_observed(controller, row, "released", |observed| {
        observed.download.releases >= 1
    })
}

/// How long a row waits for the production download owner to reach a state.
const DOWNLOAD_OBSERVED_BOUND: Duration = Duration::from_secs(5);

/// Whether this response has reached its terminal chunk.
fn chunks_ended(raw: &[u8]) -> bool {
    raw.windows(5).any(|window| window == b"\r\n0\r\n") && raw.ends_with(b"\r\n\r\n")
}

/// The status one HTTP/1 status line carries.
///
/// Which token that is is stated once: the two readers here take their status
/// off the same wire, and a second spelling is a second answer to the same
/// question.
fn status_line(raw: &[u8]) -> u16 {
    String::from_utf8_lossy(raw)
        .split_whitespace()
        .nth(1)
        .and_then(|token| token.parse::<u16>().ok())
        .unwrap_or(0)
}

/// Split one raw HTTP/1 response into the status and the payload it delivered.
fn decoded(raw: &[u8]) -> Streamed {
    let status = status_line(raw);
    let body = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map_or(&[][..], |head| &raw[head + 4..]);
    let (bytes, complete) = chunked_payload(body);
    Streamed {
        status,
        bytes,
        complete,
    }
}

/// Count the payload of a chunked body, and say whether it ended.
fn chunked_payload(mut body: &[u8]) -> (usize, bool) {
    let mut bytes = 0;
    loop {
        let Some(end) = body.windows(2).position(|window| window == b"\r\n") else {
            return (bytes, false);
        };
        let size = std::str::from_utf8(&body[..end]).ok().and_then(|line| {
            usize::from_str_radix(line.split(';').next().unwrap_or(line), 16).ok()
        });
        let Some(size) = size else {
            return (bytes, false);
        };
        if size == 0 {
            return (bytes, true);
        }
        // The chunk's own trailing CRLF is framing, not payload: a decoder that
        // left it in front of the next size line would read the empty string as
        // that line and report a body that ended early.
        let payload = end + 2;
        if body.len() < payload + size + 2 {
            return (bytes, false);
        }
        bytes += size;
        body = &body[payload + size + 2..];
    }
}
