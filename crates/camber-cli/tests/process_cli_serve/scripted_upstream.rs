//! A loopback upstream whose health endpoint answers from a script.
//!
//! Every probe the upstream answers is acknowledged after its response is on
//! the wire. The acknowledgement says only that the peer answered: the health
//! authority in the serve child commits its own state after reading the
//! response, so a row that needs the committed state must observe it through
//! the server, not through this channel.
//!
//! Every other request is a forwarded request. It is answered `200` with a body
//! that names its method and target, and reported in arrival order.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::support::FixtureError;
use crate::support::http::{accept, find, header_value, invalid_data, reason_phrase};

const IO_TIMEOUT: Duration = Duration::from_secs(2);
const ACCEPT_SLICE: Duration = Duration::from_millis(10);
const TEARDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_SIZE: usize = 64 * 1024;

/// One request the upstream answered as a forwarded request.
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct ForwardedRequest {
    pub method: Box<str>,
    pub target: Box<str>,
}

impl ForwardedRequest {
    pub fn new(method: &str, target: &str) -> Self {
        Self {
            method: method.into(),
            target: target.into(),
        }
    }
}

/// The status one answered probe carried.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ProbeAck {
    pub status: u16,
}

enum Observed {
    Probe(ProbeAck),
    Forwarded(ForwardedRequest),
}

pub struct ScriptedUpstream {
    addr: SocketAddr,
    healthy: Arc<AtomicBool>,
    stop: Sender<()>,
    observed: Receiver<Observed>,
    forwarded: Vec<ForwardedRequest>,
    worker: Option<JoinHandle<io::Result<()>>>,
}

impl ScriptedUpstream {
    /// Bind a loopback upstream that answers probes of `probe_target` as
    /// `healthy` until a script changes it.
    pub fn start(probe_target: &str, healthy: bool) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        listener.set_nonblocking(true)?;
        let health = Arc::new(AtomicBool::new(healthy));
        let (stop_sender, stop_receiver) = mpsc::channel();
        let (observed_sender, observed_receiver) = mpsc::channel();
        let worker_health = Arc::clone(&health);
        let probe_target: Box<str> = probe_target.into();
        let worker = std::thread::spawn(move || {
            serve(
                &listener,
                &probe_target,
                &worker_health,
                &stop_receiver,
                &observed_sender,
            )
        });
        Ok(Self {
            addr,
            healthy: health,
            stop: stop_sender,
            observed: observed_receiver,
            forwarded: Vec::new(),
            worker: Some(worker),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Answer every later probe with `200` when `healthy`, `500` otherwise.
    pub fn script_health(&self, healthy: bool) {
        self.healthy.store(healthy, Ordering::Release);
    }

    /// Wait for the upstream to answer a probe with `status`.
    ///
    /// Probes with any other status are skipped: a probe already
    /// in flight when the script changed answers under the old script.
    pub fn wait_for_probe(&mut self, status: u16, bound: Duration) -> Result<(), FixtureError> {
        let deadline = Instant::now() + bound;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.observed.recv_timeout(remaining) {
                Ok(Observed::Probe(ack)) if ack.status == status => return Ok(()),
                Ok(Observed::Probe(_)) => {}
                Ok(Observed::Forwarded(request)) => self.forwarded.push(request),
                Err(RecvTimeoutError::Timeout) => {
                    return Err(FixtureError::new(format!(
                        "upstream {} answered no probe with status {status} within {bound:?}",
                        self.addr
                    )));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err(FixtureError::new(format!(
                        "upstream {} worker ended before a probe with status {status}",
                        self.addr
                    )));
                }
            }
        }
    }

    /// Every forwarded request the upstream has answered so far, in order.
    pub fn forwarded(&mut self) -> &[ForwardedRequest] {
        self.drain_observed();
        &self.forwarded
    }

    fn drain_observed(&mut self) {
        loop {
            match self.observed.try_recv() {
                Ok(Observed::Probe(_)) => {}
                Ok(Observed::Forwarded(request)) => self.forwarded.push(request),
                Err(TryRecvError::Empty | TryRecvError::Disconnected) => return,
            }
        }
    }

    /// Stop the worker, join it within the teardown bound, and return every
    /// forwarded request it answered.
    pub fn finish(mut self) -> Result<Box<[ForwardedRequest]>, FixtureError> {
        let served = self.stop_and_join()?;
        self.drain_observed();
        served
            .map(|()| std::mem::take(&mut self.forwarded).into_boxed_slice())
            .map_err(|error| FixtureError::new(format!("scripted upstream failed: {error}")))
    }

    /// Stop and join the worker. The outer error is a worker that could not be
    /// joined; the inner result is what the joined worker reported.
    fn stop_and_join(&mut self) -> Result<io::Result<()>, FixtureError> {
        let _ = self.stop.send(());
        let worker = match self.worker.take() {
            Some(worker) => worker,
            None => return Ok(Ok(())),
        };
        let deadline = Instant::now() + TEARDOWN_TIMEOUT;
        while !worker.is_finished() {
            match Instant::now() < deadline {
                true => std::thread::sleep(ACCEPT_SLICE),
                false => {
                    return Err(FixtureError::new(
                        "scripted upstream did not stop before teardown deadline",
                    ));
                }
            }
        }
        worker
            .join()
            .map_err(|_| FixtureError::new("scripted upstream worker panicked"))
    }
}

impl Drop for ScriptedUpstream {
    fn drop(&mut self) {
        if self.worker.is_some() && self.stop_and_join().is_err() {
            std::process::abort();
        }
    }
}

fn serve(
    listener: &TcpListener,
    probe_target: &str,
    healthy: &AtomicBool,
    stop: &Receiver<()>,
    observed: &Sender<Observed>,
) -> io::Result<()> {
    while let Some(stream) = accept(listener, stop)? {
        tolerate_peer_left(answer(stream, probe_target, healthy, observed))?;
    }
    Ok(())
}

fn answer(
    mut stream: TcpStream,
    probe_target: &str,
    healthy: &AtomicBool,
    observed: &Sender<Observed>,
) -> io::Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let (method, target) = read_request(&mut stream)?;
    match &*target == probe_target {
        true => {
            let status = probe_status(healthy);
            write_response(&mut stream, &method, status, "probe")?;
            let _ = observed.send(Observed::Probe(ProbeAck { status }));
        }
        false => {
            let body = format!("upstream {method} {target}");
            write_response(&mut stream, &method, 200, &body)?;
            let _ = observed.send(Observed::Forwarded(ForwardedRequest { method, target }));
        }
    }
    Ok(())
}

/// A probe in flight when the serve child is killed leaves mid-request. That
/// peer is gone, not malformed.
fn tolerate_peer_left(answered: io::Result<()>) -> io::Result<()> {
    match answered {
        Err(error) if peer_left(&error) => Ok(()),
        other => other,
    }
}

fn probe_status(healthy: &AtomicBool) -> u16 {
    match healthy.load(Ordering::Acquire) {
        true => 200,
        false => 500,
    }
}

fn read_request(stream: &mut TcpStream) -> io::Result<(Box<str>, Box<str>)> {
    let mut bytes = Vec::new();
    let header_end = read_until(stream, &mut bytes, |bytes| find(bytes, b"\r\n\r\n"))?;
    let head = std::str::from_utf8(&bytes[..header_end])
        .map_err(|error| invalid_data(format!("upstream request was not UTF-8: {error}")))?;
    let mut request_line = head.lines().next().unwrap_or("").split_whitespace();
    let method: Box<str> = request_line
        .next()
        .ok_or_else(|| invalid_data("upstream request had no method"))?
        .into();
    let target: Box<str> = request_line
        .next()
        .ok_or_else(|| invalid_data("upstream request had no target"))?
        .into();
    let framing = request_framing(head)?;
    let body_start = header_end + 4;
    match framing {
        Framing::Length(length) => {
            let end = body_start + length;
            read_until(stream, &mut bytes, |bytes| {
                (bytes.len() >= end).then_some(end)
            })?;
        }
        Framing::Chunked => {
            read_until(stream, &mut bytes, |bytes| {
                bytes[body_start.min(bytes.len())..]
                    .ends_with(b"0\r\n\r\n")
                    .then_some(bytes.len())
            })?;
        }
        Framing::None => {}
    }
    Ok((method, target))
}

enum Framing {
    None,
    Length(usize),
    Chunked,
}

fn request_framing(head: &str) -> io::Result<Framing> {
    match (
        header_value(head, "transfer-encoding"),
        header_value(head, "content-length"),
    ) {
        (Some(encoding), _) if encoding.eq_ignore_ascii_case("chunked") => Ok(Framing::Chunked),
        (_, Some(length)) => length
            .parse::<usize>()
            .map(Framing::Length)
            .map_err(|error| invalid_data(format!("upstream request length: {error}"))),
        _ => Ok(Framing::None),
    }
}

fn read_until(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    complete: impl Fn(&[u8]) -> Option<usize>,
) -> io::Result<usize> {
    loop {
        match complete(bytes) {
            Some(end) => return Ok(end),
            None => {}
        }
        match bytes.len() < MAX_REQUEST_SIZE {
            true => {}
            false => return Err(invalid_data("upstream request exceeded size limit")),
        }
        let mut chunk = [0_u8; 1024];
        match stream.read(&mut chunk)? {
            0 => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
            count => bytes.extend_from_slice(&chunk[..count]),
        }
    }
}

fn write_response(stream: &mut TcpStream, method: &str, status: u16, body: &str) -> io::Result<()> {
    let reason = reason_phrase(status);
    let head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: text/plain\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    match method {
        "HEAD" => {}
        _ => stream.write_all(body.as_bytes())?,
    }
    stream.flush()
}

fn peer_left(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::UnexpectedEof
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::BrokenPipe
    )
}
