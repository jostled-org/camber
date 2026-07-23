use std::io::{self, Read};
use std::net::TcpStream;
use std::time::{Duration, Instant};

const POLL_INTERVAL: Duration = Duration::from_millis(10);

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
    Io(#[source] io::Error),
    #[error("supporting thread did not finish before {timeout:?}")]
    JoinTimeout { timeout: Duration },
    #[error("supporting thread panicked")]
    ThreadPanicked,
}

pub fn join_thread_bounded<T>(
    thread: &mut Option<std::thread::JoinHandle<T>>,
    timeout: Duration,
) -> Result<T, BoundedReadError> {
    let deadline = Instant::now() + timeout;
    loop {
        match thread.as_ref() {
            Some(handle) if handle.is_finished() => break,
            Some(_) if Instant::now() < deadline => std::thread::sleep(POLL_INTERVAL),
            Some(_) => return Err(BoundedReadError::JoinTimeout { timeout }),
            None => return Err(BoundedReadError::ThreadPanicked),
        }
    }
    let handle = thread.take().ok_or(BoundedReadError::ThreadPanicked)?;
    handle.join().map_err(|_| BoundedReadError::ThreadPanicked)
}

pub fn bounded_read(
    stream: &mut TcpStream,
    timeout: Duration,
    limit: usize,
) -> Result<Box<[u8]>, BoundedReadError> {
    let prior_timeout = stream.read_timeout().map_err(BoundedReadError::Io)?;
    let result = read_with_limit(stream, timeout, limit);
    let restore = stream
        .set_read_timeout(prior_timeout)
        .map_err(BoundedReadError::Io);
    match (result, restore) {
        (Ok(bytes), Ok(())) => Ok(bytes),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
    }
}

fn read_with_limit(
    stream: &mut TcpStream,
    timeout: Duration,
    limit: usize,
) -> Result<Box<[u8]>, BoundedReadError> {
    let deadline = Instant::now() + timeout;
    let mut bytes = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(BoundedReadError::Timeout {
                timeout,
                bytes_read: bytes.len(),
            });
        }
        stream
            .set_read_timeout(Some(remaining))
            .map_err(BoundedReadError::Io)?;
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(bytes.into_boxed_slice()),
            Ok(count)
                if bytes
                    .len()
                    .checked_add(count)
                    .is_some_and(|length| length <= limit) =>
            {
                bytes.extend_from_slice(&chunk[..count]);
            }
            Ok(_) => return Err(BoundedReadError::LimitExceeded { limit }),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock
                ) =>
            {
                return Err(BoundedReadError::Timeout {
                    timeout,
                    bytes_read: bytes.len(),
                });
            }
            Err(error) => return Err(BoundedReadError::Io(error)),
        }
    }
}
