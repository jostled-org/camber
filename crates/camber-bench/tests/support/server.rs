use super::FixtureError;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError, TrySendError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

const IO_TIMEOUT: Duration = Duration::from_millis(250);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);
const CLEANUP_TIMEOUT: Duration = Duration::from_secs(1);
const MAX_REQUEST_BYTES: usize = 8192;

#[derive(Clone, Copy)]
enum Behavior {
    Ok,
    StallAfterHeaders,
    OversizedResponse,
    OverflowingContentLength,
    PanicAfterReady,
    ReadinessFailure,
}

#[derive(Debug)]
enum WorkerTerminal {
    Completed,
    Panicked,
}

enum HeaderRead {
    Complete,
    Closed,
    Stop,
}

#[derive(Debug)]
pub struct OwnedHttpServer {
    addr: SocketAddr,
    stop: Option<SyncSender<()>>,
    done: Receiver<WorkerTerminal>,
    thread: Option<JoinHandle<()>>,
    cleanup_complete: Arc<AtomicBool>,
}

impl OwnedHttpServer {
    pub fn ok() -> Result<Self, FixtureError> {
        Self::start(Behavior::Ok)
    }

    pub fn stall_after_headers() -> Result<Self, FixtureError> {
        Self::start(Behavior::StallAfterHeaders)
    }

    pub fn oversized_response() -> Result<Self, FixtureError> {
        Self::start(Behavior::OversizedResponse)
    }

    pub fn overflowing_content_length() -> Result<Self, FixtureError> {
        Self::start(Behavior::OverflowingContentLength)
    }

    pub fn panic_after_ready() -> Result<Self, FixtureError> {
        Self::start(Behavior::PanicAfterReady)
    }

    pub fn readiness_failure() -> Result<Self, FixtureError> {
        Self::start(Behavior::ReadinessFailure)
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(&mut self, timeout: Duration) -> Result<(), FixtureError> {
        if self.thread.is_none() {
            return Ok(());
        }
        signal_stop(self.stop.take());
        let deadline = cleanup_deadline(timeout);
        let terminal = wait_for_terminal(&self.done, deadline);
        join_completed(self.thread.take(), deadline);
        match terminal {
            WorkerTerminal::Completed => Ok(()),
            WorkerTerminal::Panicked => Err(FixtureError::new("owned server thread panicked")),
        }
    }

    pub fn is_joined(&self) -> bool {
        self.thread.is_none()
    }

    pub fn cleanup_witness(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cleanup_complete)
    }

    fn start(behavior: Behavior) -> Result<Self, FixtureError> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let (stop_tx, stop_rx) = mpsc::sync_channel(1);
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);
        let cleanup_complete = Arc::new(AtomicBool::new(false));
        let thread = std::thread::Builder::new()
            .name("camber-bench-owned-http".into())
            .spawn(move || run_owned(listener, behavior, stop_rx, ready_tx, done_tx))?;
        match ready_rx.recv_timeout(CLEANUP_TIMEOUT) {
            Ok(()) => Ok(Self {
                addr,
                stop: Some(stop_tx),
                done: done_rx,
                thread: Some(thread),
                cleanup_complete,
            }),
            Err(_) => {
                signal_stop(Some(stop_tx));
                let deadline = cleanup_deadline(CLEANUP_TIMEOUT);
                let terminal = wait_for_terminal(&done_rx, deadline);
                join_completed(Some(thread), deadline);
                Err(readiness_error(terminal))
            }
        }
    }
}

fn readiness_error(terminal: WorkerTerminal) -> FixtureError {
    match terminal {
        WorkerTerminal::Completed => FixtureError::new("owned server readiness failed"),
        WorkerTerminal::Panicked => {
            FixtureError::new("owned server readiness failed after worker panic")
        }
    }
}

impl Drop for OwnedHttpServer {
    fn drop(&mut self) {
        if self.thread.is_some() && self.shutdown(CLEANUP_TIMEOUT).is_err() && self.thread.is_some()
        {
            abort_cleanup();
        }
        self.cleanup_complete.store(true, Ordering::Release);
    }
}

fn run_owned(
    listener: TcpListener,
    behavior: Behavior,
    stop: Receiver<()>,
    ready: SyncSender<()>,
    done: SyncSender<WorkerTerminal>,
) {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run(listener, behavior, stop, ready)
    }));
    let terminal = match result {
        Ok(()) => WorkerTerminal::Completed,
        Err(_) => WorkerTerminal::Panicked,
    };
    let _ = done.send(terminal);
}

fn run(listener: TcpListener, behavior: Behavior, stop: Receiver<()>, ready: SyncSender<()>) {
    if matches!(behavior, Behavior::ReadinessFailure) {
        return;
    }
    if ready.send(()).is_err() {
        return;
    }
    assert!(!matches!(behavior, Behavior::PanicAfterReady));
    loop {
        if stop.try_recv().is_ok() {
            return;
        }
        if handle_accept(listener.accept(), behavior, &stop) {
            return;
        }
    }
}

fn handle_accept(
    result: std::io::Result<(TcpStream, SocketAddr)>,
    behavior: Behavior,
    stop: &Receiver<()>,
) -> bool {
    match result {
        Ok((stream, _)) => handle_connection(stream, behavior, stop),
        Err(error) if error.kind() == ErrorKind::WouldBlock => {
            stop.recv_timeout(Duration::from_millis(5)).is_ok()
        }
        Err(_) => true,
    }
}

fn handle_connection(mut stream: TcpStream, behavior: Behavior, stop: &Receiver<()>) -> bool {
    let _ = stream.set_read_timeout(Some(IO_TIMEOUT));
    let _ = stream.set_write_timeout(Some(IO_TIMEOUT));
    match read_headers(&mut stream, stop) {
        HeaderRead::Complete => {}
        HeaderRead::Closed => return false,
        HeaderRead::Stop => return true,
    }
    match behavior {
        Behavior::Ok => {
            let _ = stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok");
            false
        }
        Behavior::StallAfterHeaders => {
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\no");
            stop.recv_timeout(Duration::from_secs(2)).is_ok()
        }
        Behavior::OversizedResponse => {
            write_oversized_response(&mut stream);
            false
        }
        Behavior::OverflowingContentLength => {
            let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", usize::MAX);
            let _ = stream.write_all(header.as_bytes());
            false
        }
        Behavior::PanicAfterReady | Behavior::ReadinessFailure => false,
    }
}

fn write_oversized_response(stream: &mut TcpStream) {
    let Some(size) = crate::support::http::MAX_RESPONSE_BYTES.checked_add(1) else {
        return;
    };
    let header = format!("HTTP/1.1 200 OK\r\nContent-Length: {size}\r\n\r\n");
    if stream.write_all(header.as_bytes()).is_err() {
        return;
    }
    let bytes = vec![b'x'; size];
    let _ = stream.write_all(&bytes);
}

fn read_headers(stream: &mut TcpStream, stop: &Receiver<()>) -> HeaderRead {
    let Some(deadline) = Instant::now().checked_add(REQUEST_TIMEOUT) else {
        return HeaderRead::Closed;
    };
    let mut request = Vec::new();
    let mut buffer = [0_u8; 1024];
    while request.len() < MAX_REQUEST_BYTES {
        match stop.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => return HeaderRead::Stop,
            Err(TryRecvError::Empty) => {}
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return HeaderRead::Closed;
        };
        if stream
            .set_read_timeout(Some(remaining.min(IO_TIMEOUT)))
            .is_err()
        {
            return HeaderRead::Closed;
        }
        let remaining_capacity = MAX_REQUEST_BYTES.saturating_sub(request.len());
        let read_limit = remaining_capacity.min(buffer.len());
        match stream.read(&mut buffer[..read_limit]) {
            Ok(0) => return HeaderRead::Closed,
            Ok(read) if !append_request_chunk(&mut request, &buffer[..read]) => {
                return HeaderRead::Closed;
            }
            Ok(_) if request.windows(4).any(|window| window == b"\r\n\r\n") => {
                return HeaderRead::Complete;
            }
            Ok(_) => {}
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {}
            Err(_) => return HeaderRead::Closed,
        }
    }
    HeaderRead::Closed
}

fn append_request_chunk(request: &mut Vec<u8>, chunk: &[u8]) -> bool {
    let Some(new_len) = request.len().checked_add(chunk.len()) else {
        return false;
    };
    if new_len > MAX_REQUEST_BYTES {
        return false;
    }
    request.extend_from_slice(chunk);
    true
}

fn signal_stop(stop: Option<SyncSender<()>>) {
    let Some(stop) = stop else {
        return;
    };
    match stop.try_send(()) {
        Ok(()) | Err(TrySendError::Full(())) | Err(TrySendError::Disconnected(())) => {}
    }
}

fn wait_for_terminal(done: &Receiver<WorkerTerminal>, deadline: Instant) -> WorkerTerminal {
    let remaining = deadline.saturating_duration_since(Instant::now());
    match done.recv_timeout(remaining) {
        Ok(terminal) => terminal,
        Err(_) => abort_cleanup(),
    }
}

fn join_completed(thread: Option<JoinHandle<()>>, deadline: Instant) {
    let Some(thread) = thread else {
        return;
    };
    while !thread.is_finished() {
        if Instant::now() >= deadline {
            abort_cleanup();
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if thread.join().is_err() {
        abort_cleanup();
    }
}

fn cleanup_deadline(timeout: Duration) -> Instant {
    Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(|| abort_cleanup())
}

fn abort_cleanup() -> ! {
    std::process::abort()
}
