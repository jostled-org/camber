//! A local upstream a proxy case drives byte by byte.
//!
//! A Camber upstream answers only once it has a whole request, which is exactly
//! what a streaming-upload case cannot promise: the claims here are about a
//! payload that stops partway, an answer that arrives before it, and the order
//! those two reach each other in. This upstream records every byte the proxy
//! forwards and answers when the case says so, so both sides of that race are
//! arranged rather than waited for.

use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::http::{
    POLL_INTERVAL, fail_unless_panicking, is_closed_connection_error, is_deadline_expiry,
};

/// How long one upstream may serve before its own deadline ends it.
const UPSTREAM_BOUND: Duration = Duration::from_secs(20);

/// The most bytes one upstream retains.
const UPSTREAM_CAPTURE_LIMIT: usize = 1 << 20;

/// What ends a request head.
const HEAD_TERMINATOR: &[u8] = b"\r\n\r\n";

/// What ends a chunked payload.
const CHUNKED_TERMINATOR: &[u8] = b"0\r\n\r\n";

/// When a raw upstream writes its answer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpstreamAnswers {
    /// As soon as the request head has arrived, with the upload still running.
    OnHead,
    /// Never. The answer is withheld, so a case can settle a request that
    /// never received one.
    Withheld,
    /// Once the peer's payload has ended, or its connection has.
    OnBodyEnd,
}

/// What one upstream answers with, and everything it has observed.
struct UpstreamState {
    received: Mutex<Vec<u8>>,
    connections: AtomicUsize,
    /// Connections whose peer ended them at the transport.
    ///
    /// Separate from [`Self::connections`], because a cancelled upstream leg is
    /// only observable here: the proxy that dropped it wrote nothing to say so,
    /// and a case that read the request bytes alone cannot tell a leg still
    /// waiting apart from one that is gone.
    dropped: AtomicUsize,
    answered: AtomicUsize,
    stopped: AtomicBool,
    /// The accept this upstream stopped on, when one failed.
    ///
    /// A failed accept ends the serving loop, so every connection after it is
    /// missing for the fixture's own reason. Without this the case reads a
    /// connection count of zero and blames the subject under test for a
    /// transport failure that was never its.
    accept_failure: Mutex<Option<Box<str>>>,
    /// The exact bytes one connection is answered with.
    ///
    /// Held whole rather than assembled per answer: a case that scripts a
    /// declared length its payload does not keep, or a chunked answer whose
    /// frames are the claim, cannot describe its answer as a status and a body.
    answer: Box<[u8]>,
    answers: UpstreamAnswers,
}

impl UpstreamState {
    /// Whether this upstream may answer a connection standing at `read`.
    fn may_answer(&self, read: ConnectionRead) -> bool {
        match (self.answers, read) {
            (_, ConnectionRead::Partial) => false,
            (UpstreamAnswers::OnHead, _) => true,
            (UpstreamAnswers::Withheld, _) => false,
            (UpstreamAnswers::OnBodyEnd, read) => read == ConnectionRead::Ended,
        }
    }

    /// Record what one connection has read so far, up to this fixture's cap.
    fn record(&self, bytes: &[u8]) {
        let mut received = self
            .received
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let room = UPSTREAM_CAPTURE_LIMIT.saturating_sub(received.len());
        received.extend_from_slice(&bytes[..bytes.len().min(room)]);
    }

    /// Record the accept that stopped this upstream, keeping the first one.
    ///
    /// The first failure is the one that explains every connection the case
    /// then finds missing; the ones after it are its consequences.
    fn record_accept_failure(&self, error: &io::Error) {
        let mut failure = self
            .accept_failure
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match failure.as_ref() {
            Some(_) => {}
            None => *failure = Some(error.to_string().into_boxed_str()),
        }
    }
}

/// A bound upstream, the thread serving it, and everything it observed.
pub struct RawUpstream {
    addr: SocketAddr,
    state: Arc<UpstreamState>,
    serving: Option<std::thread::JoinHandle<()>>,
}

/// Bind a raw upstream on an ephemeral port and start serving it.
///
/// The listener is bound before the thread starts, so the address handed back
/// is already accepting: a case can register it on a proxy route without
/// waiting for a readiness probe this upstream would not answer.
pub fn raw_upstream(status: u16, body: &str, answers: UpstreamAnswers) -> RawUpstream {
    let answer = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: text/plain\r\nContent-Length: {}\r\n\r\n{body}",
        body.len(),
    );
    scripted_upstream(answer.into_bytes().into_boxed_slice(), answers)
}

/// Bind a raw upstream that answers with exactly `answer`, byte for byte.
///
/// [`raw_upstream`]'s counterpart for a case whose claim is the answer's own
/// framing: a length above what the reader will retain, or a chunked body whose
/// frames cross a ceiling one at a time. Both forms share this fixture's accept
/// loop, byte record, and bounded teardown, so a scripted answer costs one
/// spelling of the bytes and nothing else.
pub fn scripted_upstream(answer: Box<[u8]>, answers: UpstreamAnswers) -> RawUpstream {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind the raw upstream");
    let addr = listener.local_addr().expect("name the raw upstream");
    let state = Arc::new(UpstreamState {
        received: Mutex::new(Vec::new()),
        connections: AtomicUsize::new(0),
        dropped: AtomicUsize::new(0),
        answered: AtomicUsize::new(0),
        stopped: AtomicBool::new(false),
        accept_failure: Mutex::new(None),
        answer,
        answers,
    });
    let served = Arc::clone(&state);
    let serving = std::thread::spawn(move || serve_upstream(&listener, &served));
    RawUpstream {
        addr,
        state,
        serving: Some(serving),
    }
}

impl RawUpstream {
    /// The backend URL a proxy route is registered with.
    pub fn backend(&self) -> Box<str> {
        format!("http://{}", self.addr).into_boxed_str()
    }

    /// The address this upstream is bound to.
    ///
    /// What a case registers a lifecycle controller under: the production
    /// collectors publish what they read and kept against the address they read
    /// it from, so this is how a proxy case reads the collection its own
    /// upstream answered.
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Every byte this upstream has read, across every connection.
    fn received(&self) -> Box<[u8]> {
        self.state
            .received
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .into_boxed_slice()
    }

    /// Whether the bytes this upstream read contain `needle`.
    pub fn forwarded(&self, needle: &[u8]) -> bool {
        self.received()
            .windows(needle.len())
            .any(|window| window == needle)
    }

    /// How many connections this upstream has accepted.
    pub fn connections(&self) -> usize {
        self.state.connections.load(Ordering::Acquire)
    }

    /// How many answers this upstream has put on the wire.
    pub fn answered(&self) -> usize {
        self.state.answered.load(Ordering::Acquire)
    }

    /// How many of this upstream's connections their peer ended.
    pub fn dropped(&self) -> usize {
        self.state.dropped.load(Ordering::Acquire)
    }

    /// The accept failure that stopped this upstream, if one did.
    ///
    /// Cloned rather than taken: `Drop` fails on it and a case may report it
    /// first, and a read that drained it would tell the second reader this
    /// upstream served cleanly.
    pub fn accept_failure(&self) -> Option<Box<str>> {
        self.state
            .accept_failure
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }
}

impl Drop for RawUpstream {
    /// End the serving thread, wait for it, and fail on either teardown fault.
    ///
    /// The stop flag is read between reads, and the connection below is what
    /// ends the accept that is not reading anything: a case that panicked
    /// partway leaves the same bounded teardown as one that finished.
    ///
    /// The join and the accept loop are two separate faults and both are worth
    /// a failure — a thread that never returned, and an accept that stopped the
    /// fixture before the case was done with it. Neither is raised during an
    /// unwind, for the reason [`fail_unless_panicking`] states.
    fn drop(&mut self) {
        self.state.stopped.store(true, Ordering::Release);
        drop(TcpStream::connect(self.addr));
        let joined = super::stream::join_thread_bounded(&mut self.serving, UPSTREAM_BOUND);
        match (joined, self.accept_failure()) {
            (Ok(()), None) => {}
            (Ok(()), Some(failure)) => {
                fail_unless_panicking(&format!("the raw upstream stopped accepting: {failure}"));
            }
            (Err(error), _) => {
                fail_unless_panicking(&format!("the raw upstream thread ended: {error}"));
            }
        }
    }
}

/// Accept and serve connections until this fixture is stopped.
fn serve_upstream(listener: &TcpListener, state: &Arc<UpstreamState>) {
    let deadline = Instant::now() + UPSTREAM_BOUND;
    while !state.stopped.load(Ordering::Acquire) && Instant::now() < deadline {
        match listener.accept() {
            // The connection that ends a waiting accept during teardown is not
            // a request, so it is not counted or served as one.
            Ok((stream, _)) if !state.stopped.load(Ordering::Acquire) => {
                serve_connection(stream, state, deadline);
            }
            Ok(_) => {}
            Err(error) => {
                state.record_accept_failure(&error);
                state.stopped.store(true, Ordering::Release);
            }
        }
    }
}

/// Read one connection to its end, answering it when this upstream may.
fn serve_connection(mut stream: TcpStream, state: &Arc<UpstreamState>, deadline: Instant) {
    state.connections.fetch_add(1, Ordering::Release);
    let bounded = stream.set_read_timeout(Some(POLL_INTERVAL));
    assert!(bounded.is_ok(), "the raw upstream bounded its own reads");
    let mut connection = ConnectionBuffer::new();
    let mut answered = false;
    let mut buffer = [0_u8; 8192];
    while !state.stopped.load(Ordering::Acquire) && Instant::now() < deadline {
        let read = read_into(&mut stream, &mut buffer, &mut connection, state);
        answered = answer_when_due(&mut stream, state, answered, read);
        match read {
            ConnectionRead::Ended => break,
            ConnectionRead::Partial | ConnectionRead::Head => {}
        }
    }
}

/// What one connection has arriving on it so far.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ConnectionRead {
    /// Not even a whole request head has arrived.
    Partial,
    /// The head has arrived and payload is still coming.
    Head,
    /// The peer finished its request, or ended the connection.
    Ended,
}

/// How one request framed the payload behind its head.
///
/// A payload ends at the terminal chunk or at the length its head declared,
/// and this upstream is only ever sent one of those two framings by the proxy
/// under test. A head with neither is a request that promised nothing more.
#[derive(Clone, Copy)]
enum Framing {
    /// Chunked, so the payload ends at its terminal chunk.
    Chunked,
    /// Exactly this many payload bytes.
    Declared(usize),
    /// Neither, so the head promised nothing more.
    Nothing,
}

/// One request head, as the reader below has to keep it.
#[derive(Clone, Copy)]
struct RequestHead {
    /// Where the head's terminator ends, in bytes.
    end: usize,
    framing: Framing,
}

/// Everything one connection has sent, and where its head ended.
struct ConnectionBuffer {
    bytes: Vec<u8>,
    /// Found once and kept.
    ///
    /// The offset is what the payload is measured from, and it is a byte
    /// offset: a length taken off a lossy decode of these bytes counts three
    /// for every payload byte no encoding explains, so it reaches any declared
    /// length early and declares a body complete that is still arriving.
    ///
    /// Keeping it also keeps the search off the poll: re-scanning the whole
    /// accumulated buffer every five milliseconds costs the square of the
    /// upload.
    head: Option<RequestHead>,
}

impl ConnectionBuffer {
    fn new() -> Self {
        Self {
            bytes: Vec::new(),
            head: None,
        }
    }

    /// Take what one read produced and say where the request now stands.
    fn extend(&mut self, chunk: &[u8]) -> ConnectionRead {
        self.bytes.extend_from_slice(chunk);
        self.standing()
    }

    /// Where this request stands, read off the bytes that have arrived for it.
    fn standing(&mut self) -> ConnectionRead {
        match self.head() {
            None => ConnectionRead::Partial,
            Some(head) if self.payload_complete(head) => ConnectionRead::Ended,
            Some(_) => ConnectionRead::Head,
        }
    }

    /// This request's head, searched for only until it is found.
    fn head(&mut self) -> Option<RequestHead> {
        match self.head {
            Some(head) => Some(head),
            None => {
                self.head = self.locate_head();
                self.head
            }
        }
    }

    /// Find the head terminator in the raw bytes and read the framing off it.
    fn locate_head(&self) -> Option<RequestHead> {
        let terminator = self
            .bytes
            .windows(HEAD_TERMINATOR.len())
            .position(|window| window == HEAD_TERMINATOR)?;
        let end = terminator + HEAD_TERMINATOR.len();
        let text = String::from_utf8_lossy(&self.bytes[..end]);
        Some(RequestHead {
            end,
            framing: framing(&text),
        })
    }

    /// Whether the payload this head promised has fully arrived.
    fn payload_complete(&self, head: RequestHead) -> bool {
        match head.framing {
            Framing::Chunked => self.bytes.ends_with(CHUNKED_TERMINATOR),
            Framing::Declared(declared) => self.bytes.len().saturating_sub(head.end) >= declared,
            Framing::Nothing => true,
        }
    }
}

/// Take whatever this connection has ready, and say where its request stands.
fn read_into(
    stream: &mut TcpStream,
    buffer: &mut [u8],
    connection: &mut ConnectionBuffer,
    state: &Arc<UpstreamState>,
) -> ConnectionRead {
    match stream.read(buffer) {
        Ok(0) => {
            state.dropped.fetch_add(1, Ordering::Release);
            ConnectionRead::Ended
        }
        Ok(count) => {
            state.record(&buffer[..count]);
            connection.extend(&buffer[..count])
        }
        // A read that found nothing yet is the peer still sending, not the end
        // of anything: the loop above rereads its own state and comes back.
        Err(error) if is_deadline_expiry(&error) => connection.standing(),
        // A peer that reset or aborted named the same event a peer that closed
        // does, and the count exists to say the leg is gone rather than still
        // waiting. Counting only the clean close makes a case polling for it
        // fail whenever the platform delivered the reset instead.
        Err(error) if is_closed_connection_error(&error) => {
            state.dropped.fetch_add(1, Ordering::Release);
            ConnectionRead::Ended
        }
        Err(_) => ConnectionRead::Ended,
    }
}

/// How one request head framed the payload behind it.
fn framing(head: &str) -> Framing {
    match (is_chunked(head), declared_length(head)) {
        (true, _) => Framing::Chunked,
        (false, Some(declared)) => Framing::Declared(declared),
        (false, None) => Framing::Nothing,
    }
}

/// Whether one request head framed its payload in chunks.
fn is_chunked(head: &str) -> bool {
    named_header(head, "transfer-encoding").is_some_and(|value| value.contains("chunked"))
}

/// The length one request head declared, when it declared one.
fn declared_length(head: &str) -> Option<usize> {
    named_header(head, "content-length").and_then(|value| value.parse::<usize>().ok())
}

/// One header's first value, under the case-insensitive lookup HTTP requires.
fn named_header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines()
        .filter_map(|line| line.split_once(':'))
        .find(|(header, _)| header.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.trim())
}

/// Write this upstream's answer if it is due and not already given, and say
/// whether this connection has one.
///
/// One connection gets one answer. The read loop comes back every poll while a
/// payload is still arriving, and an upstream answering on the head would put a
/// second whole response on the wire at each of them.
fn answer_when_due(
    stream: &mut TcpStream,
    state: &Arc<UpstreamState>,
    answered: bool,
    read: ConnectionRead,
) -> bool {
    match (answered, state.may_answer(read)) {
        (true, _) => true,
        (false, false) => false,
        (false, true) => write_answer(stream, state),
    }
}

/// Put this upstream's fixed answer on the wire, and say whether it left.
///
/// The count is raised only by a write that completed. A discarded write result
/// counted an answer the peer never saw, and a case reading that count would
/// hold the subject responsible for the fixture's own broken socket.
fn write_answer(stream: &mut TcpStream, state: &Arc<UpstreamState>) -> bool {
    let written = stream
        .write_all(&state.answer)
        .and_then(|()| stream.flush());
    match written {
        Ok(()) => {
            state.answered.fetch_add(1, Ordering::Release);
            true
        }
        Err(_) => false,
    }
}
