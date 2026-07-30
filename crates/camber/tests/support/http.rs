use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camber::RuntimeError;
use camber::http::{self, Router, ServerHandle};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The gap between attempts in every bounded poll the suite runs.
///
/// Five milliseconds, the finer of the two intervals the harness used before
/// this was stated once. The interval is a retry granularity, not a budget:
/// every wait built on it clamps its sleep to what is left of the caller's
/// deadline, so a shorter interval only costs wakeups, while the ten-millisecond
/// one lost most of its resolution against the twenty-five-millisecond bounds
/// several fixtures assert with.
pub const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// The bound on one readiness attempt, which is how long a `connect` that hangs
/// can stall the poll before the next attempt is made.
const PROBE_ATTEMPT: Duration = Duration::from_millis(100);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = MAX_HEADER_BYTES + MAX_BODY_BYTES + MAX_HEADER_BYTES;
const SERVER_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

/// How much of `deadline` is left, saturating at zero.
///
/// Lets a multi-leg wait share one deadline instead of giving each leg its own
/// bound, so its worst case is that deadline rather than a multiple of it.
///
/// Stated in this module rather than beside the runtime-scope waits that also
/// need it, because this is the one support module every harness root mounts:
/// the socket readers, the stream reader, the process guard, and the drain
/// helpers all reach it from here, and only some of those roots mount the
/// others.
pub fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// Poll `attempt` every [`POLL_INTERVAL`] until it produces a value, giving up
/// at `bound`.
///
/// The one bounded wait the whole harness is written from: a rendezvous that
/// never arrives fails the test at `bound` instead of parking the waiter on it.
/// `attempt` is always tried at least once, so a leg handed what is left of a
/// shared deadline still gets its answer rather than an automatic refusal.
pub fn poll_value<T>(bound: Duration, mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + bound;
    loop {
        match (attempt(), Instant::now() < deadline) {
            (Some(value), _) => return Some(value),
            (None, false) => return None,
            // Clamped, so the last retry cannot overshoot the bound by a whole
            // interval and report a failure the caller's budget still allowed.
            (None, true) => std::thread::sleep(POLL_INTERVAL.min(remaining(deadline))),
        }
    }
}

/// Poll `ready` until it reports success, giving up at `bound`.
///
/// [`poll_value`] for a wait whose answer is the arrival itself rather than a
/// value it carries.
pub fn poll_until(bound: Duration, mut ready: impl FnMut() -> bool) -> bool {
    poll_value(bound, || ready().then_some(())).is_some()
}

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("fixture I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("fixture runtime failed: {0}")]
    Runtime(#[from] RuntimeError),
    /// `cause` carries the last transport or parse failure the readiness poll
    /// saw. Without it a refused connection, a malformed response, and a server
    /// that never bound all report the same sentence.
    #[error("server did not return a valid HTTP readiness response before {timeout:?}: {cause}")]
    ReadinessTimeout { timeout: Duration, cause: Box<str> },
    #[error("server shutdown did not complete before {timeout:?}")]
    ShutdownTimeout { timeout: Duration },
    /// No ambient Tokio runtime, so the bounded join has nothing to drive the
    /// server task to completion on.
    #[error("no Tokio runtime was available to join the fixture server")]
    NoJoinRuntime,
    /// A runtime that cannot host a blocking wait. Reported rather than
    /// asserted on, because the guard's `Drop` reaches this too.
    #[error("a {flavor} Tokio runtime cannot host the fixture server's bounded join")]
    UnjoinableRuntime { flavor: Box<str> },
}

pub struct BoundListener {
    listener: std::net::TcpListener,
    local_addr: SocketAddr,
}

impl BoundListener {
    pub fn bind_tcp(addr: &str) -> Result<Self, io::Error> {
        let listener = std::net::TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Hand the reservation to Tokio without unbinding it.
    ///
    /// Exposed so a fixture that must own the raw [`ServerHandle`] serves from
    /// the same reservation the rest of the suite does, rather than binding a
    /// second listener of its own.
    pub(crate) fn into_tokio(self) -> Result<tokio::net::TcpListener, io::Error> {
        tokio::net::TcpListener::from_std(self.listener)
    }
}

#[derive(Default)]
struct ServerCleanupState {
    joined: std::sync::atomic::AtomicBool,
    error: Mutex<Option<Box<str>>>,
}

pub struct ServerCleanupProbe(Arc<ServerCleanupState>);

impl ServerCleanupProbe {
    pub fn joined(&self) -> bool {
        self.0.joined.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The cleanup fault the guard recorded, if it recorded one.
    ///
    /// Cloned, not taken: the accessor borrows, so reading it twice — a test
    /// that logs the fault and then asserts on it, or two probes cloned from
    /// one server — must give the same answer both times. Draining it here
    /// would report "no cleanup fault" for a server that had one.
    pub fn cleanup_error(&self) -> Option<Box<str>> {
        self.0
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

pub struct ReadyServer {
    local_addr: SocketAddr,
    handle: Option<ServerHandle>,
    readiness: HttpResponse,
    cleanup: Arc<ServerCleanupState>,
}

impl ReadyServer {
    pub fn start(
        listener: BoundListener,
        router: Router,
        timeout: Duration,
    ) -> Result<Self, FixtureError> {
        let local_addr = listener.local_addr();
        let handle = http::serve_background(listener.into_tokio()?, router);
        let cleanup = Arc::new(ServerCleanupState::default());
        let readiness = match wait_for_http_response(local_addr, timeout) {
            Ok(response) => response,
            Err(error) => {
                return Err(FixtureError::ReadinessTimeout {
                    timeout,
                    cause: cancel_unready(handle, &error),
                });
            }
        };
        Ok(Self {
            local_addr,
            handle: Some(handle),
            readiness,
            cleanup,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub fn readiness_response(&self) -> &HttpResponse {
        &self.readiness
    }

    pub fn cleanup_probe(&self) -> ServerCleanupProbe {
        ServerCleanupProbe(Arc::clone(&self.cleanup))
    }

    pub fn shutdown_bounded(mut self, timeout: Duration) -> Result<(), FixtureError> {
        self.shutdown_and_join(timeout)
    }

    /// Give up the guard and keep the server handle.
    ///
    /// For a fixture whose subject IS the handle's disposal: dropping the
    /// handle is the teardown it measures, so the guard that would cancel and
    /// join on its own behalf has to step aside. Taking the handle disarms the
    /// `Drop` arm, which then has nothing left to join.
    pub fn into_handle(mut self) -> ServerHandle {
        self.handle
            .take()
            .expect("a ready server always owns its handle until it is given up")
    }

    fn shutdown_and_join(&mut self, timeout: Duration) -> Result<(), FixtureError> {
        let handle = match self.handle.take() {
            Some(handle) => handle,
            None => return Ok(()),
        };
        handle.shutdown();
        self.join(handle, timeout)
    }

    fn cancel_and_join(&mut self, timeout: Duration) -> Result<(), FixtureError> {
        let handle = match self.handle.take() {
            Some(handle) => handle,
            None => return Ok(()),
        };
        handle.cancel();
        self.join(handle, timeout)
    }

    fn join(&self, handle: ServerHandle, timeout: Duration) -> Result<(), FixtureError> {
        let joined = join_bounded(handle, timeout);
        if joined.is_ok() {
            self.cleanup
                .joined
                .store(true, std::sync::atomic::Ordering::Release);
        }
        joined
    }

    fn record_cleanup_error(&self, error: &FixtureError) {
        let mut cleanup_error = self
            .cleanup
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cleanup_error = Some(error.to_string().into_boxed_str());
    }
}

impl Drop for ReadyServer {
    fn drop(&mut self) {
        if let Err(error) = self.cancel_and_join(SERVER_CLEANUP_TIMEOUT) {
            self.record_cleanup_error(&error);
        }
    }
}

/// Wait for one server task to finish, bounded, without ever panicking.
///
/// A cancelled server ended because it was told to, so that is a completed join
/// and not a failure. Stated once, because a guard's teardown and a failed
/// start's cleanup read the same three outcomes.
///
/// Every current caller reaches this from an assertion-unwind path, one of them
/// through [`ReadyServer`]'s `Drop`. A panic during unwind aborts the process
/// and destroys the whole binary's assertion output, so the two conditions the
/// blocking wait needs are reported rather than asserted on: `Handle::current`
/// panics with no ambient runtime, and `block_in_place` panics on a
/// current-thread runtime. `Drop` records either through the cleanup probe, and
/// a test that reads the probe still gets its own failure.
///
/// Multi-thread is the only flavor that can host this wait: `block_in_place`
/// hands the worker's remaining tasks to a sibling thread first, and a
/// current-thread runtime has no sibling to hand them to.
fn join_bounded(handle: ServerHandle, timeout: Duration) -> Result<(), FixtureError> {
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(runtime) => runtime,
        Err(_) => return Err(FixtureError::NoJoinRuntime),
    };
    let join = tokio::time::timeout(timeout, handle.join());
    let result = match runtime.runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| runtime.block_on(join))
        }
        flavor => {
            return Err(FixtureError::UnjoinableRuntime {
                flavor: format!("{flavor:?}").into_boxed_str(),
            });
        }
    };
    match result {
        Ok(Ok(())) | Ok(Err(RuntimeError::Cancelled)) => Ok(()),
        Ok(Err(error)) => Err(FixtureError::Runtime(error)),
        Err(_) => Err(FixtureError::ShutdownTimeout { timeout }),
    }
}

/// Cancel a server that never answered, and report both faults as one cause.
///
/// A failed [`ReadyServer::start`] hands back no guard, so it can hand out no
/// cleanup probe either: a cleanup fault recorded there is written where nothing
/// can ever read it. It joins the readiness diagnosis in the returned error
/// instead, so one error carries both — and neither displaces the other.
fn cancel_unready(handle: ServerHandle, readiness_error: &io::Error) -> Box<str> {
    handle.cancel();
    match join_bounded(handle, SERVER_CLEANUP_TIMEOUT) {
        Ok(()) => readiness_error.to_string().into_boxed_str(),
        Err(cleanup_error) => format!(
            "{readiness_error}; the unready server also failed to shut down: {cleanup_error}"
        )
        .into_boxed_str(),
    }
}

pub fn spawn_server_ready(router: Router, timeout: Duration) -> Result<ReadyServer, FixtureError> {
    let listener = BoundListener::bind_tcp("127.0.0.1:0")?;
    ReadyServer::start(listener, router, timeout)
}

/// Serve `router` on an already-bound reservation and hand back the raw handle
/// once the server answers.
///
/// The readiness wait is [`ReadyServer::start`]'s, so a fixture that owns its
/// own teardown does not carry a second copy of it. A server that never answers
/// is cancelled and joined before the error returns, exactly as the guarded
/// form does.
pub fn serve_background_ready(
    listener: BoundListener,
    router: Router,
    timeout: Duration,
) -> Result<ServerHandle, FixtureError> {
    ReadyServer::start(listener, router, timeout).map(ReadyServer::into_handle)
}

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Box<[(Box<str>, Box<str>)]>,
    pub body: Box<[u8]>,
    raw: Box<[u8]>,
}

impl HttpResponse {
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_ref())
    }
}

pub fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> io::Result<HttpResponse> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_write_timeout(Some(timeout))?;
    write_request(&mut stream, method, path, headers, body)?;
    with_read_deadline(&mut stream, timeout, |stream, deadline| {
        read_http_response(stream, Some(deadline))
    })
}

pub fn wait_for_http_response(addr: SocketAddr, timeout: Duration) -> io::Result<HttpResponse> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "readiness deadline overflowed")
    })?;
    // Carried in the closure rather than returned by it, because the poll keeps
    // only the successful answer: without this, a readiness bound that expires
    // reports the expiry and loses the refusal, malformed response, or unbound
    // listener that actually caused it.
    let mut last_error = io::Error::from(io::ErrorKind::TimedOut);
    let probed = poll_value(timeout, || {
        let attempt = remaining(deadline).min(PROBE_ATTEMPT);
        // The budget is spent and the poll is about to end anyway. A zero connect
        // bound is refused outright, and that refusal would displace the
        // diagnosis this wait exists to carry out.
        if attempt.is_zero() {
            return None;
        }
        match probe_transport(addr, attempt) {
            Ok(response) => Some(response),
            Err(error) => {
                last_error = error;
                None
            }
        }
    });
    probed.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("HTTP readiness timed out; last error: {last_error}"),
        )
    })
}

fn probe_transport(addr: SocketAddr, timeout: Duration) -> io::Result<HttpResponse> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: invalid\r\n\r\n")?;
    stream.flush()?;
    with_read_deadline(&mut stream, timeout, |stream, deadline| {
        read_http_response(stream, Some(deadline))
    })
}

/// One request's whole raw response text.
///
/// Sealed: the caller reads a status off it or searches it, and nothing appends
/// to it, so re-opening the text into a `String` would buy a spare capacity
/// field nothing uses.
pub fn raw_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Box<str> {
    raw_request_with_body(addr, method, path, headers, &[])
}

pub fn raw_request_with_body(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Box<str> {
    let mut stream = connect(addr).unwrap();
    write_request(&mut stream, method, path, headers, body).unwrap();
    let response = with_read_deadline(&mut stream, IO_TIMEOUT, |stream, deadline| {
        read_http_response(stream, Some(deadline))
    })
    .unwrap();
    String::from_utf8_lossy(response.raw())
        .into_owned()
        .into_boxed_str()
}

/// The status code off a raw response head.
///
/// A head with no readable status fails here with the text it was given. The
/// sentinel this used to return read as status 0, which failed the caller's
/// status assertion for the wrong reason and hid the malformed response that
/// caused it.
pub fn status_from_raw(raw: &str) -> u16 {
    raw.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("the response head carried no readable status: {raw:?}"))
}

pub fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

/// The request an admission probe sends once it has a connection.
const ADMISSION_PROBE_REQUEST: &[u8] =
    b"GET /retained HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

/// What a read that never answered reports, whether the bound was the socket's
/// or the caller's own timer.
const ADMISSION_READ_TIMED_OUT: &str = "timed out waiting for closed admission";

/// What one stage of an admission probe established.
///
/// The triage itself, held apart from the transport that produced it, so the
/// async and blocking probes classify the same outcomes the same way instead of
/// each deciding for itself what counts as proof.
enum AdmissionStage {
    /// Nothing is accepting, or the peer is already gone. That is what closed
    /// admission means, and the probe is over.
    Closed,
    /// The stage passed without settling the question. Go on to the next one.
    Continue,
    /// The probe cannot answer the question, and why.
    Inconclusive(Box<str>),
}

/// End one admission probe.
///
/// A stage that settled the question returns; one that could not fails the test
/// with what it saw. `Continue` cannot reach here — every caller consumes it —
/// and reaching it would mean a probe that ran out of stages without a verdict.
fn settle(stage: AdmissionStage) {
    match stage {
        AdmissionStage::Closed => {}
        AdmissionStage::Continue => {
            panic!("the admission probe ran out of stages without reaching a verdict")
        }
        AdmissionStage::Inconclusive(reason) => panic!("{reason}"),
    }
}

/// Read a failed connect.
fn classify_connect_error(error: &io::Error, timeout: Duration) -> AdmissionStage {
    match error.kind() {
        // Refused, or answered by a listener already gone: nothing is accepting,
        // which is what closed admission means.
        io::ErrorKind::ConnectionRefused => AdmissionStage::Closed,
        _ if is_closed_connection_error(error) => AdmissionStage::Closed,
        // Neither completed nor refused within the bound. That is a listener
        // still bound with a saturated backlog — admission is open — so it
        // cannot be read as proof that it is closed.
        _ if is_deadline_expiry(error) => {
            AdmissionStage::Inconclusive(connect_never_settled(timeout))
        }
        // Any other connect failure is the fixture's own transport breaking —
        // an exhausted descriptor table, an unreachable route — and says
        // nothing about whether the server is still admitting.
        kind => AdmissionStage::Inconclusive(
            format!("the connect failed as {kind:?}, which is not proof that admission is closed: {error}")
                .into_boxed_str(),
        ),
    }
}

/// What a connect that neither completed nor was refused reports.
fn connect_never_settled(timeout: Duration) -> Box<str> {
    format!("the connect neither completed nor was refused within {timeout:?}").into_boxed_str()
}

/// Read the probe request's write.
fn classify_write(result: io::Result<()>) -> AdmissionStage {
    match result {
        Ok(()) => AdmissionStage::Continue,
        Err(error) if is_closed_connection_error(&error) => AdmissionStage::Closed,
        Err(error) => AdmissionStage::Inconclusive(
            format!("failed while probing closed admission: {error}").into_boxed_str(),
        ),
    }
}

/// Read the one response byte a still-admitting server would produce.
fn classify_read(result: io::Result<usize>) -> AdmissionStage {
    match result {
        Ok(0) => AdmissionStage::Closed,
        Err(error) if is_closed_connection_error(&error) => AdmissionStage::Closed,
        Err(error) if is_deadline_expiry(&error) => {
            AdmissionStage::Inconclusive(ADMISSION_READ_TIMED_OUT.into())
        }
        Ok(read) => AdmissionStage::Inconclusive(
            format!("closed admission produced {read} response byte(s)").into_boxed_str(),
        ),
        Err(error) => AdmissionStage::Inconclusive(
            format!("failed while waiting for closed admission: {error}").into_boxed_str(),
        ),
    }
}

pub async fn assert_admission_closed(addr: SocketAddr, timeout: Duration) {
    let mut stream = match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => return settle(classify_connect_error(&error, timeout)),
        Err(_) => {
            return settle(AdmissionStage::Inconclusive(connect_never_settled(timeout)));
        }
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    match classify_write(stream.write_all(ADMISSION_PROBE_REQUEST).await) {
        AdmissionStage::Continue => {}
        stage => return settle(stage),
    }
    let mut byte = [0_u8; 1];
    let read = match tokio::time::timeout(timeout, stream.read(&mut byte)).await {
        Ok(read) => read,
        Err(_) => {
            return settle(AdmissionStage::Inconclusive(
                ADMISSION_READ_TIMED_OUT.into(),
            ));
        }
    };
    settle(classify_read(read));
}

/// [`assert_admission_closed`] for a caller with no runtime to await on.
///
/// The same triage, driven over a blocking socket: a sync harness that
/// hand-rolled its own would decide for itself what counts as proof that
/// admission is closed, and a connect failure that only means the fixture's own
/// transport broke would then pass as that proof.
pub fn assert_admission_closed_blocking(addr: SocketAddr, timeout: Duration) {
    let mut stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(stream) => stream,
        Err(error) => return settle(classify_connect_error(&error, timeout)),
    };
    let armed = tolerate_dead_socket(stream.set_read_timeout(Some(timeout)))
        .and_then(|()| tolerate_dead_socket(stream.set_write_timeout(Some(timeout))));
    match classify_write(armed.and_then(|()| stream.write_all(ADMISSION_PROBE_REQUEST))) {
        AdmissionStage::Continue => {}
        stage => return settle(stage),
    }
    let mut byte = [0_u8; 1];
    settle(classify_read(stream.read(&mut byte)));
}

pub fn write_request(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        body.len()
    );
    headers.iter().for_each(|(name, value)| {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    });
    request.push_str("\r\n");
    stream.write_all(request.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// Read the response head: every byte through the blank line that ends it.
///
/// A streaming response never reaches end of body while its producer holds the
/// sender, so the whole-response readers cannot be used to prove its head was
/// produced — and one `read` returns whatever a single syscall produced, which
/// TCP is free to split anywhere inside that head.
pub fn read_head(stream: &mut TcpStream, timeout: Duration) -> io::Result<Box<[u8]>> {
    read_delimited(stream, b"\r\n\r\n", MAX_HEADER_BYTES, timeout)
}

/// Read through `delimiter`, returning every byte up to and including it.
///
/// `timeout` is a deadline over the whole frame, not a bound on one syscall:
/// the read is byte-at-a-time, so a per-read timeout would give every byte a
/// fresh full budget and a peer dribbling bytes under it would never expire.
/// The socket's prior read timeout is restored, so the bound set here cannot
/// leak into the rest of a test's use of the connection.
pub fn read_delimited(
    stream: &mut TcpStream,
    delimiter: &[u8],
    limit: usize,
    timeout: Duration,
) -> io::Result<Box<[u8]>> {
    with_read_deadline(stream, timeout, |stream, deadline| {
        let mut bytes = Vec::new();
        let end = read_through(
            stream,
            &mut bytes,
            0,
            delimiter,
            limit,
            "framed read",
            Some(deadline),
        )?;
        Ok(bytes[..end].into())
    })
}

/// Read a stream to closure, returning everything that arrived before it.
///
/// An abortive close is a close: a peer that ends with `Connection: close` and
/// an RST reaches the reader as one of the gone-peer kinds — which one depends
/// on the platform — and treating any of them as a failure would fail a correct
/// teardown. One statement of that rule, bounded by one deadline over the whole
/// read.
pub fn read_until_closed(stream: &mut TcpStream, timeout: Duration) -> io::Result<Box<[u8]>> {
    with_read_deadline(stream, timeout, |stream, deadline| {
        let mut bytes = Vec::new();
        match read_to_eof(stream, &mut bytes, MAX_RESPONSE_BYTES, Some(deadline)) {
            Err(error) if is_closed_connection_error(&error) => Ok(()),
            result => result,
        }?;
        Ok(bytes.into_boxed_slice())
    })
}

/// One direction's socket timeout, as the pair of accessors that reads it and
/// arms it.
///
/// `TcpStream` states the read and write bounds as two identical method pairs,
/// so every helper that has to save, arm, and restore one of them would
/// otherwise be written once per direction. Naming the pair as a value leaves
/// one such helper for the whole suite.
#[derive(Clone, Copy)]
pub struct SocketTimeout {
    current: fn(&TcpStream) -> io::Result<Option<Duration>>,
    arm: fn(&TcpStream, Option<Duration>) -> io::Result<()>,
}

/// The read direction's timeout accessors.
pub const READ_TIMEOUT: SocketTimeout = SocketTimeout {
    current: TcpStream::read_timeout,
    arm: TcpStream::set_read_timeout,
};

/// The write direction's timeout accessors.
pub const WRITE_TIMEOUT: SocketTimeout = SocketTimeout {
    current: TcpStream::write_timeout,
    arm: TcpStream::set_write_timeout,
};

/// Run `operation` with `armed` in force on one direction of `stream`,
/// restoring the socket's prior bound however it ended.
///
/// The restore is what keeps a bound set for one exchange from leaking into the
/// rest of a test's use of the connection, and it has to run on the failure path
/// too — so it is stated once here rather than once per reader and writer. A
/// restore that itself fails is reported, but never in place of the operation's
/// own error. A caller driving a multi-read frame arms the whole frame's budget
/// here and narrows it per read from [`apply_deadline`].
pub fn with_socket_timeout<T, E>(
    stream: &mut TcpStream,
    timeout: SocketTimeout,
    armed: Option<Duration>,
    operation: impl FnOnce(&mut TcpStream) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<io::Error>,
{
    let previous = (timeout.current)(stream)?;
    tolerate_dead_socket((timeout.arm)(stream, armed))?;
    let result = operation(stream);
    let restore = tolerate_dead_socket((timeout.arm)(stream, previous));
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(E::from(error)),
    }
}

/// Run one framed read against a deadline computed from `timeout`.
///
/// The whole frame's budget is armed once so no read can outlast it even if a
/// reader forgets to narrow it, and [`apply_deadline`] then hands each read only
/// what is left.
pub(crate) fn with_read_deadline<T>(
    stream: &mut TcpStream,
    timeout: Duration,
    read: impl FnOnce(&mut TcpStream, Instant) -> io::Result<T>,
) -> io::Result<T> {
    let deadline = Instant::now() + timeout;
    with_socket_timeout(stream, READ_TIMEOUT, Some(timeout), |stream| {
        read(stream, deadline)
    })
}

/// Whether `error` is a peer that has already gone away.
///
/// The four kinds a closed connection reaches a writer or reader as, named
/// once: every probe that treats "the peer is gone" as its expected outcome
/// reads the same set.
pub fn is_closed_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
    )
}

/// Accept a socket deadline that could not be armed or restored because the
/// socket is already dead, and report every other failure.
///
/// macOS refuses `setsockopt` with `EINVAL` once both directions of a socket
/// are shut down, so a peer that goes away mid-exchange turns the next arm or
/// restore into an error on that platform and on no other. Reporting it would
/// fail a probe for the very outcome it expects. Dropping the bound is safe
/// where the bound is: a dead socket answers immediately, so the I/O this
/// guards cannot hang, and its own closed-connection error stays the verdict
/// the caller reads.
pub fn tolerate_dead_socket(result: io::Result<()>) -> io::Result<()> {
    /// `EINVAL`, named rather than depending on `libc` for one integer.
    const INVALID_ARGUMENT: i32 = 22;
    match result {
        Err(error) if error.raw_os_error() == Some(INVALID_ARGUMENT) => Ok(()),
        result => result,
    }
}

/// Read one whole HTTP response, bounded by `deadline`.
///
/// `deadline` covers the head, the body, and every chunk between them, because
/// a per-read bound would give each byte a fresh full budget: a peer that stalls
/// mid-chunk under one would park the calling test with nothing to expire. A
/// caller arms it through [`with_read_deadline`], which restores the socket's
/// prior bound afterwards. `None` reads with whatever bound the socket already
/// carries, for a caller that owns its own.
///
/// Most callers want [`read_http_response_bounded`] instead: it arms the same
/// budget [`connect`] already put on the socket, so the frame deadline and the
/// socket bound stay one number rather than two that can drift apart.
pub fn read_http_response(
    stream: &mut TcpStream,
    deadline: Option<Instant>,
) -> io::Result<HttpResponse> {
    let mut raw = Vec::new();
    let header_end = read_through(
        stream,
        &mut raw,
        0,
        b"\r\n\r\n",
        MAX_HEADER_BYTES,
        "response headers",
        deadline,
    )?;
    let (status, headers) = parse_head(&raw[..header_end])?;
    let body: Box<[u8]> = match response_body_kind(status, &headers)? {
        BodyKind::None => Vec::new().into_boxed_slice(),
        BodyKind::Length(length) => {
            let body_end = header_end
                .checked_add(length)
                .ok_or_else(|| invalid_data("response length overflowed"))?;
            read_to_length(
                stream,
                &mut raw,
                body_end,
                MAX_RESPONSE_BYTES,
                "response body",
                deadline,
            )?;
            raw[header_end..body_end].into()
        }
        BodyKind::Chunked => read_chunked_body(stream, &mut raw, header_end, deadline)?,
        BodyKind::Eof => {
            read_to_eof(stream, &mut raw, MAX_RESPONSE_BYTES, deadline)?;
            raw[header_end..].into()
        }
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
        raw: raw.into_boxed_slice(),
    })
}

/// Read one whole response under the same budget [`connect`] arms on the socket.
///
/// The frame deadline and the socket's own read timeout are the same number, so
/// stating it once is what keeps them from drifting. Every caller that connects
/// through this module's helpers wants this form; the explicit-deadline
/// [`read_http_response`] is for a caller holding a budget of its own, such as a
/// fixture measuring one step of a longer journey.
pub fn read_http_response_bounded(stream: &mut TcpStream) -> io::Result<HttpResponse> {
    read_http_response(stream, Some(Instant::now() + IO_TIMEOUT))
}

enum BodyKind {
    None,
    Length(usize),
    Chunked,
    Eof,
}

fn response_body_kind(status: u16, headers: &[(Box<str>, Box<str>)]) -> io::Result<BodyKind> {
    if (100..200).contains(&status) || matches!(status, 204 | 304) {
        return Ok(BodyKind::None);
    }
    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .split(',')
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
    });
    if chunked {
        return Ok(BodyKind::Chunked);
    }
    let lengths = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| invalid_data(format!("invalid content length: {error}")))?;
    match lengths.as_slice() {
        [] => Ok(BodyKind::Eof),
        [length, rest @ ..] if rest.iter().all(|candidate| candidate == length) => {
            validated_body_length(*length)
        }
        _ => Err(invalid_data(
            "response contained conflicting content lengths",
        )),
    }
}

fn validated_body_length(length: usize) -> io::Result<BodyKind> {
    match length <= MAX_BODY_BYTES {
        true => Ok(BodyKind::Length(length)),
        false => Err(invalid_data("response body exceeded size limit")),
    }
}

fn parse_head(bytes: &[u8]) -> io::Result<(u16, Box<[(Box<str>, Box<str>)]>)> {
    let head = std::str::from_utf8(bytes)
        .map_err(|error| invalid_data(format!("response head was not UTF-8: {error}")))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| invalid_data("response did not contain a valid status"))?;
    let headers = lines
        .take_while(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| invalid_data("response contained a malformed header"))?;
            Ok((name.into(), value.trim().into()))
        })
        .collect::<io::Result<Vec<_>>>()?
        .into_boxed_slice();
    Ok((status, headers))
}

fn read_chunked_body(
    stream: &mut TcpStream,
    raw: &mut Vec<u8>,
    mut cursor: usize,
    deadline: Option<Instant>,
) -> io::Result<Box<[u8]>> {
    let mut body = Vec::new();
    loop {
        let line_end = read_through(
            stream,
            raw,
            cursor,
            b"\r\n",
            MAX_RESPONSE_BYTES,
            "chunk size line",
            deadline,
        )?;
        let size_end = line_end
            .checked_sub(2)
            .ok_or_else(|| invalid_data("chunk size framing underflowed"))?;
        let size_line = std::str::from_utf8(&raw[cursor..size_end])
            .map_err(|error| invalid_data(format!("chunk size was not UTF-8: {error}")))?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or_default(), 16)
            .map_err(|error| invalid_data(format!("invalid chunk size: {error}")))?;
        cursor = line_end;
        if size == 0 {
            read_chunk_trailers(stream, raw, cursor, deadline)?;
            return Ok(body.into_boxed_slice());
        }
        let body_end = body
            .len()
            .checked_add(size)
            .ok_or_else(|| invalid_data("chunk body length overflowed"))?;
        if body_end > MAX_BODY_BYTES {
            return Err(invalid_data("response body exceeded size limit"));
        }
        let payload_end = cursor
            .checked_add(size)
            .ok_or_else(|| invalid_data("chunk payload length overflowed"))?;
        let framed_end = payload_end
            .checked_add(2)
            .ok_or_else(|| invalid_data("chunk framing length overflowed"))?;
        read_to_length(
            stream,
            raw,
            framed_end,
            MAX_RESPONSE_BYTES,
            "chunk payload",
            deadline,
        )?;
        body.extend_from_slice(&raw[cursor..payload_end]);
        if &raw[payload_end..framed_end] != b"\r\n" {
            return Err(invalid_data("chunk did not end with CRLF"));
        }
        cursor = framed_end;
    }
}

fn read_chunk_trailers(
    stream: &mut TcpStream,
    raw: &mut Vec<u8>,
    mut cursor: usize,
    deadline: Option<Instant>,
) -> io::Result<()> {
    loop {
        let trailer_end = read_through(
            stream,
            raw,
            cursor,
            b"\r\n",
            MAX_RESPONSE_BYTES,
            "chunk trailer",
            deadline,
        )?;
        let empty_line_end = cursor
            .checked_add(2)
            .ok_or_else(|| invalid_data("chunk trailer length overflowed"))?;
        cursor = trailer_end;
        if trailer_end == empty_line_end {
            return Ok(());
        }
    }
}

/// Read from `start` until `delimiter` appears, naming what is being read in
/// every failure.
///
/// `subject` is what the caller was framing — response headers, a chunk size
/// line — so a size or overflow failure reports the read that actually hit it
/// rather than the one this helper was first written for. `start` is where the
/// frame begins in a buffer the caller is filling across several frames: a
/// chunked body reads one line after another out of the same `Vec`, and a
/// delimiter left behind by the previous frame is not this one's.
fn read_through(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    start: usize,
    delimiter: &[u8],
    limit: usize,
    subject: &str,
    deadline: Option<Instant>,
) -> io::Result<usize> {
    // Where the last search ended. `read_one` appends one byte, so rescanning
    // the whole frame per byte would cost the square of it; resuming one
    // delimiter short of the end covers a delimiter that straddles the previous
    // read and nothing more.
    let overlap = delimiter.len().saturating_sub(1);
    let mut scanned = start;
    loop {
        let from = scanned.saturating_sub(overlap).max(start);
        if let Some(position) = bytes[from..]
            .windows(delimiter.len())
            .position(|part| part == delimiter)
        {
            return from
                .checked_add(position)
                .and_then(|end| end.checked_add(delimiter.len()))
                .ok_or_else(|| invalid_data(format!("{subject} length overflowed")));
        }
        scanned = bytes.len().max(start);
        if bytes.len() >= limit {
            return Err(invalid_data(format!(
                "{subject} exceeded the {limit}-byte size limit"
            )));
        }
        read_one(stream, bytes, deadline)?;
    }
}

/// Fill `bytes` until it reaches `expected`, bounded by `deadline`.
///
/// The counted read every framed protocol is built from: an HTTP body of known
/// length, one chunk and its CRLF, a WebSocket header, mask key, or payload.
/// `deadline` bounds the whole fill rather than one syscall, so a peer dribbling
/// bytes under a per-read timeout still expires.
pub(crate) fn read_to_length(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    expected: usize,
    limit: usize,
    subject: &str,
    deadline: Option<Instant>,
) -> io::Result<()> {
    if expected > limit {
        return Err(invalid_data(format!(
            "{subject} exceeded the {limit}-byte size limit"
        )));
    }
    while bytes.len() < expected {
        let remaining = expected - bytes.len();
        let mut chunk = [0_u8; 4096];
        let read_limit = remaining.min(chunk.len());
        apply_deadline(stream, deadline)?;
        let count = attribute_deadline(stream.read(&mut chunk[..read_limit]), deadline)?;
        if count == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(())
}

/// Read until the peer closes, bounded by `deadline` and capped at `limit`.
///
/// The drain-to-close every reader that takes "everything the peer sent" is
/// built from: a body with no length, a whole response after `Connection:
/// close`, a stream read to its end. The only `InvalidData` it produces is the
/// size cap, which is what lets a caller with its own error type tell an
/// overflow apart from a transport failure.
pub(crate) fn read_to_eof(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    limit: usize,
    deadline: Option<Instant>,
) -> io::Result<()> {
    let mut chunk = [0_u8; 4096];
    loop {
        apply_deadline(stream, deadline)?;
        match attribute_deadline(stream.read(&mut chunk), deadline)? {
            0 => return Ok(()),
            count
                if bytes
                    .len()
                    .checked_add(count)
                    .is_some_and(|length| length <= limit) =>
            {
                bytes.extend_from_slice(&chunk[..count]);
            }
            _ => return Err(invalid_data("response exceeded size limit")),
        }
    }
}

fn read_one(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    deadline: Option<Instant>,
) -> io::Result<()> {
    apply_deadline(stream, deadline)?;
    let mut byte = [0_u8; 1];
    match attribute_deadline(stream.read(&mut byte), deadline)? {
        0 => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        _ => {
            bytes.push(byte[0]);
            Ok(())
        }
    }
}

/// Give the next read only what is left of `deadline`.
///
/// This is what makes a multi-read frame bounded as a whole: the socket's own
/// timeout applies per syscall, so it is recomputed before each one. A deadline
/// already spent fails here rather than arming a zero timeout, which the
/// platform reads as no timeout at all.
fn apply_deadline(stream: &mut TcpStream, deadline: Option<Instant>) -> io::Result<()> {
    let left = match deadline {
        None => return Ok(()),
        Some(deadline) => remaining(deadline),
    };
    match left.is_zero() {
        true => Err(deadline_expired()),
        false => tolerate_dead_socket(stream.set_read_timeout(Some(left))),
    }
}

/// Whether `error` is a read that ran out of time rather than a broken
/// transport.
///
/// A read the socket's own bound cut short surfaces as `WouldBlock` on Unix and
/// `TimedOut` on Windows, and a deadline this module reports as spent uses the
/// second of those. One statement of the pair, so every probe that has to tell
/// its own bound apart from a peer that broke reads the same set.
pub fn is_deadline_expiry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Report a read the socket's own bound cut short as the frame's deadline
/// expiring.
///
/// [`apply_deadline`] arms exactly what is left of the frame's deadline, so a
/// read that expires at the socket level expired against that deadline — and the
/// platform names that `WouldBlock` on Unix and `TimedOut` on Windows. Both are
/// reported as the deadline's own failure, so one cause reaches the caller as
/// one verdict on every platform. A read taken without a deadline keeps whatever
/// the platform said.
fn attribute_deadline(result: io::Result<usize>, deadline: Option<Instant>) -> io::Result<usize> {
    match result {
        Err(error) if deadline.is_some() && is_deadline_expiry(&error) => Err(deadline_expired()),
        result => result,
    }
}

/// The one verdict a framed read that ran out of time reports.
fn deadline_expired() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "framed read exceeded its deadline")
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
