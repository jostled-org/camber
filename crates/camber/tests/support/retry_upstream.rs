//! A scripted raw HTTP/1.1 upstream for retry rows that run under paused time.
//!
//! Each accepted connection is one client attempt, answered by the script entry
//! at its accept index. The upstream reports every phase it reaches as a
//! [`PeerEvent`], so a row advances the Tokio clock only after the peer has
//! acknowledged the phase the client is parked in. Real socket readiness still
//! takes real time, so a [`RunnableDriver`] keeps the paused runtime busy and
//! stops it from auto-advancing onto an unrelated deadline while a row waits.
//!
//! A [`RecordingUpstream`] is the second peer here. It keeps what each request
//! sent, for rows that assert the exact request a client wrote. It answers
//! every request the same way, or chooses each answer from the request it
//! read.

use std::borrow::Cow;
use std::future::Future;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use camber::RuntimeError;
use camber::http::Client;
use camber::http::ClientResponse;
use camber::http::mock::ScopedTransferOwner;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::halt::{HaltableListener, send_report, unless_halted, until_peer_closed};

/// Tokio rounds timer deadlines up to a millisecond; a row that steps onto a
/// deadline crosses that tick too.
pub const TIMER_ROUNDING: Duration = Duration::from_millis(1);

/// The real-time bound one fixture wait settles within.
///
/// It bounds fixture failure only. Virtual time never moves it, and its expiry
/// proves nothing about the client's policy.
pub const WATCHDOG: Duration = Duration::from_secs(10);

/// The head of a transient answer whose declared body is never sent, with an
/// optional `Retry-After` value.
///
/// The connection stays alive until the client disposes the response, so the
/// peer's EOF is the evidence of that disposal.
fn transient_head(retry_after: Option<&str>) -> Vec<u8> {
    let retry_after =
        retry_after.map_or_else(String::new, |value| format!("Retry-After: {value}\r\n"));
    format!(
        "HTTP/1.1 503 Service Unavailable\r\n{retry_after}Content-Length: 100\r\nConnection: keep-alive\r\n\r\n"
    )
    .into_bytes()
}

/// A final answer's head and a prefix of the body it declares.
const STALLED_BODY_PREFIX: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\n\r\npartial";

/// A final answer's head and a prefix of the body it declares, sent before the
/// peer closes with the rest unsent.
const TRUNCATED_BODY_PREFIX: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\npartial";

/// A complete final answer.
const ANSWER: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

/// How much of a request body an [`Answer::Ambiguous`] attempt reads before
/// closing. One byte is enough to prove the payload reached the peer.
const AMBIGUOUS_BODY_PREFIX: usize = 1;

/// Every method the public client offers a verb convenience for.
pub const CLIENT_METHODS: [&str; 7] = ["GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH", "DELETE"];

/// The body a row sends with `method`. Empty means the request has no body.
///
/// The verb conveniences send the same bytes, so a row reads one expected
/// body whichever ingress it used.
pub fn payload(method: &str) -> &'static str {
    match method {
        "POST" => "post",
        "PUT" => "put",
        "PATCH" => "patch",
        "PURGE" => "purge",
        _ => "",
    }
}

/// Send one of [`CLIENT_METHODS`] through the verb convenience `$sender`
/// names: a client value, or the free functions.
macro_rules! send_verb {
    ($method:expr, $url:expr, $($sender:tt)+) => {
        match $method {
            "GET" => $($sender)+ get($url).await,
            "HEAD" => $($sender)+ head($url).await,
            "OPTIONS" => $($sender)+ options($url).await,
            "POST" => $($sender)+ post($url, payload("POST")).await,
            "PUT" => $($sender)+ put($url, payload("PUT")).await,
            "PATCH" => $($sender)+ patch($url, payload("PATCH")).await,
            "DELETE" => $($sender)+ delete($url).await,
            other => panic!("{other} is not a method the public client offers"),
        }
    };
}

/// Send one of [`CLIENT_METHODS`] through the public client.
pub async fn send_method(
    client: &Client,
    method: &str,
    url: &str,
) -> Result<ClientResponse, RuntimeError> {
    send_verb!(method, url, client.)
}

/// Send one of [`CLIENT_METHODS`] through the free verb functions.
pub async fn send_free_method(method: &str, url: &str) -> Result<ClientResponse, RuntimeError> {
    send_verb!(method, url, camber::http::)
}

/// One client call running as its own task.
pub type Call = JoinHandle<Result<ClientResponse, RuntimeError>>;

/// Start one GET through the public client as its own task.
///
/// The task owns the client and the URL, so dropping the task drops the whole
/// call and nothing the row keeps.
pub fn spawn_get(client: Client, url: String) -> Call {
    tokio::spawn(async move { client.get(&url).await })
}

/// The call's result, captured without asserting it.
///
/// `None` means the call never settled within the fixture watchdog.
pub async fn settled(call: Call) -> Option<Result<ClientResponse, RuntimeError>> {
    match within_watchdog(call).await {
        Some(Ok(result)) => Some(result),
        Some(Err(joined)) => panic!("the client task did not complete: {joined}"),
        None => None,
    }
}

/// A settled call's result in one line, for a failure message.
pub fn describe(result: &Option<Result<ClientResponse, RuntimeError>>) -> Box<str> {
    result
        .as_ref()
        .map_or_else(|| "no result".into(), describe_result)
}

/// One call's result in one line, for a failure message.
pub fn describe_result(result: &Result<ClientResponse, RuntimeError>) -> Box<str> {
    match result {
        Ok(response) => format!("status {}", response.status()).into_boxed_str(),
        Err(error) => format!("{error:?}").into_boxed_str(),
    }
}

/// The value a row's call returned, or a panic naming its failure.
pub fn answered<T>(row: &str, result: Result<T, RuntimeError>) -> T {
    result.unwrap_or_else(|error| panic!("{row}: the call failed: {error:?}"))
}

/// The failure a row's call returned, or a panic naming the answer it got.
pub fn failure(row: &str, result: Result<ClientResponse, RuntimeError>) -> RuntimeError {
    match result {
        Err(error) => error,
        Ok(response) => panic!("{row}: the call answered {}", response.status()),
    }
}

/// A complete HTTP/1.1 answer that leaves its connection open.
pub fn answer(status_line: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut wire = format!("HTTP/1.1 {status_line}\r\n").into_bytes();
    for (name, value) in headers {
        wire.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
    wire.extend_from_slice(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes());
    wire.extend_from_slice(body);
    wire
}

/// Require the client to have closed a held attempt's transport, as captured
/// by [`ScriptedUpstream::saw`] before the fixture was released.
pub fn assert_released(released: Result<(), String>, context: &str) {
    if let Err(observed) = released {
        panic!("{context}: the held attempt kept its transport: {observed}");
    }
}

/// What the upstream does with one attempt once it has read the whole request.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Answer {
    /// Answer 503 with an unsent body, then wait for the client to dispose it.
    Transient,
    /// Answer as [`Answer::Transient`] does, with this `Retry-After` value in
    /// the head.
    TransientRetryAfter(&'static str),
    /// Send nothing and hold the connection open.
    StallHead,
    /// Send a final head and a body prefix, then hold the connection open.
    StallBody,
    /// Send a complete 200 answer and close.
    Complete,
    /// Send exactly these bytes as a complete answer and close.
    ///
    /// The row writes the whole wire image, head included, so it can put
    /// bytes in a header value or a body that no text literal holds.
    Exact(&'static [u8]),
    /// Send exactly these bytes as a head whose declared body is never sent,
    /// then wait for the client to dispose it.
    ///
    /// The row writes the head, so it can be a redirect. The peer's EOF is the
    /// evidence of the disposal, as it is for [`Answer::Transient`].
    Held(&'static [u8]),
    /// Send a final head and a body prefix, then close with the rest unsent.
    Truncate,
    /// Read the head and a body prefix, then close without answering.
    ///
    /// The peer may already have acted on what it read, and the client cannot
    /// tell, which is the whole ambiguity.
    Ambiguous,
}

/// A phase the upstream reached, tagged with its zero-based attempt index.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PeerEvent {
    /// The request of this attempt was read, as far as its answer reads it.
    Started(usize),
    /// The client closed the connection of a transient or held answer.
    Disposed(usize),
    /// The attempt is parked at its scripted stall.
    Stalled(usize),
    /// A complete answer was written.
    Answered(usize),
    /// A final head and a body prefix were written before the peer closed.
    Truncated(usize),
    /// The client closed a stalled attempt's connection.
    Released(usize),
}

/// A task that is always runnable while a row waits on real socket readiness.
///
/// Paused Tokio time advances on its own whenever the runtime would park with
/// nothing to do. This task makes sure there is always something to do, so the
/// clock moves only when a row advances it.
pub struct RunnableDriver {
    handle: JoinHandle<()>,
}

impl RunnableDriver {
    pub fn start() -> Self {
        let handle = tokio::spawn(async {
            loop {
                tokio::task::yield_now().await;
            }
        });
        Self { handle }
    }

    /// Stop the driver and join it.
    pub async fn stop(mut self) {
        self.handle.abort();
        let joined = (&mut self.handle).await;
        assert!(
            joined.is_err_and(|error| error.is_cancelled()),
            "the runnable driver ended on its own"
        );
    }
}

impl Drop for RunnableDriver {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Run `future` to completion, or report `None` once [`WATCHDOG`] of real time
/// has passed.
///
/// The bound is measured on a thread, not on the Tokio clock, because the rows
/// that use it hold that clock still.
pub async fn within_watchdog<T>(future: impl Future<Output = T>) -> Option<T> {
    let (expired_tx, expired) = oneshot::channel::<()>();
    let (cancel, cancelled) = std::sync::mpsc::channel::<()>();
    let timer = std::thread::spawn(move || {
        if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = cancelled.recv_timeout(WATCHDOG) {
            // A dropped receiver means the future already won the race.
            let _ = expired_tx.send(());
        }
    });
    let outcome = tokio::select! {
        biased;
        value = future => Some(value),
        _ = expired => None,
    };
    drop(cancel);
    timer.join().unwrap();
    outcome
}

/// A runtime for the peers of a row whose client runs on a Camber runtime.
///
/// The runtime a row shuts down then owns nothing of the peer that observes
/// it.
pub fn peer_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("the peer runtime could not be built")
}

/// Run one peer-side wait on the peer runtime.
///
/// The wait runs from a thread of its own. The caller may stand inside the
/// Camber runtime's closure, and a thread inside one runtime cannot block on
/// another.
pub fn on_peers<T: Send>(
    peers: &tokio::runtime::Runtime,
    wait: impl Future<Output = T> + Send,
) -> T {
    let joined = std::thread::scope(|scope| scope.spawn(|| peers.block_on(wait)).join());
    match joined {
        Ok(output) => output,
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Wait for each phase in order, and name the first the upstream did not
/// report.
pub async fn saw_each(upstream: &mut ScriptedUpstream, events: &[PeerEvent]) -> Result<(), String> {
    for event in events {
        upstream.saw(*event).await?;
    }
    Ok(())
}

/// Advance the paused clock onto `deadline`, and not past it.
pub async fn advance_to(deadline: Instant) {
    tokio::time::advance(deadline.saturating_duration_since(Instant::now())).await;
}

/// Let every task woken by the last clock step take its turn.
///
/// The test runtime is current-thread, so each yield hands the thread to the
/// tasks already runnable. It observes; it moves no clock.
pub async fn settle_wakeups() {
    for _ in 0..16 {
        tokio::task::yield_now().await;
    }
}

/// Observe actual collection before a row acts on a body-phase operation.
/// The watchdog bounds a failed observation; it does not advance virtual time.
pub async fn wait_for_body_collection(observed: &ScopedTransferOwner) -> Result<(), String> {
    within_watchdog(async {
        while observed.collected_chunks_polled() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .ok_or_else(|| "the client never collected the body prefix".to_owned())
}

/// One upstream report: a phase an attempt reached, or a fault of the fixture
/// itself, named by its attempt.
type PeerReport = Result<PeerEvent, Box<str>>;

/// The report for a fault of the accept loop or of one attempt's task.
fn fixture_fault(attempt: usize, detail: Box<str>) -> PeerReport {
    Err(format!("attempt {attempt}: {detail}").into_boxed_str())
}

/// A raw upstream that answers each attempt from a script.
///
/// It owns a [`HaltableListener`], which owns the accepted sockets and the
/// accept loop that holds them. Attempts past the end of the script stall
/// their head, so an attempt the client should never have started is still
/// counted and held, and its unconsumed reports fail [`Self::finish`].
pub struct ScriptedUpstream {
    listener: HaltableListener<PeerReport>,
    starts: Arc<AtomicU32>,
}

impl ScriptedUpstream {
    pub async fn bind(script: &[Answer]) -> Self {
        let starts = Arc::new(AtomicU32::new(0));
        let counted = Arc::clone(&starts);
        let script: Box<[Answer]> = script.into();
        let listener = HaltableListener::bind(
            "the scripted upstream",
            move |_: &mpsc::UnboundedSender<PeerReport>| {
                move |stream, attempt, events, halt| {
                    let answer = script.get(attempt).copied().unwrap_or(Answer::StallHead);
                    serve_attempt(stream, attempt, answer, Arc::clone(&counted), events, halt)
                }
            },
            fixture_fault,
        )
        .await;
        Self { listener, starts }
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.listener.addr()
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.listener.addr())
    }

    /// Wait for the next phase the upstream reports and require it.
    pub async fn expect(&mut self, expected: PeerEvent, context: &str) {
        if let Err(observed) = self.saw(expected).await {
            panic!("{context}: {observed}");
        }
    }

    /// Wait for each phase in order and require every one.
    pub async fn expect_each(&mut self, expected: &[PeerEvent], context: &str) {
        for event in expected {
            self.expect(*event, context).await;
        }
    }

    /// The next phase the upstream reports, required to be `expected`.
    ///
    /// The captured form of [`Self::expect`], for evidence a row asserts only
    /// after its fixture has been released. The error names what the upstream
    /// did instead.
    pub async fn saw(&mut self, expected: PeerEvent) -> Result<(), String> {
        match self.next_event(&expected).await? {
            event if event == expected => Ok(()),
            event => Err(format!("the upstream reported {event:?}, not {expected:?}")),
        }
    }

    /// The next phase the upstream reports, or what it did instead of
    /// reporting the `awaited` one.
    async fn next_event(
        &mut self,
        awaited: &(dyn std::fmt::Debug + Sync),
    ) -> Result<PeerEvent, String> {
        match within_watchdog(self.listener.recv()).await {
            Some(Some(Ok(event))) => Ok(event),
            Some(Some(Err(fault))) => {
                Err(format!("the upstream faulted before {awaited:?}: {fault}"))
            }
            Some(None) => Err(format!("the upstream stopped before {awaited:?}")),
            None => Err(format!("the upstream never reported {awaited:?}")),
        }
    }

    /// The next phases the upstream reports, required to be `expected` in any
    /// order.
    ///
    /// For phases two connections reach with no edge between them. The peer
    /// cannot order a close on one socket against a request on another.
    pub async fn saw_each_unordered(&mut self, expected: &[PeerEvent]) -> Result<(), String> {
        let mut pending = expected.to_vec();
        while !pending.is_empty() {
            let event = self.next_event(&pending).await?;
            let Some(found) = pending.iter().position(|awaited| *awaited == event) else {
                return Err(format!(
                    "the upstream reported {event:?}, not one of {pending:?}"
                ));
            };
            pending.swap_remove(found);
        }
        Ok(())
    }

    /// Stop accepting, close every held peer, join the accept loop, stop the
    /// row's driver, prove the address is free, require every report consumed,
    /// and report the request starts the upstream saw.
    ///
    /// The driver holds the virtual clock until every peer has joined. The
    /// address check then waits on real time, independently of that clock.
    pub async fn finish(self, driver: RunnableDriver, context: &str) -> u32 {
        let Self { listener, starts } = self;
        listener
            .finish(context, |accepts| async move {
                let joined = within_watchdog(accepts).await;
                driver.stop().await;
                joined
            })
            .await;
        starts.load(Ordering::SeqCst)
    }

    /// Require attempt `attempt` to start and answer completely, settle the
    /// call, finish the fixture, and require the answer after exactly
    /// `attempt + 1` request starts.
    pub async fn finish_answered(
        mut self,
        attempt: usize,
        call: Call,
        driver: RunnableDriver,
        context: &str,
    ) {
        self.expect_each(
            &[PeerEvent::Started(attempt), PeerEvent::Answered(attempt)],
            context,
        )
        .await;
        let result = settled(call).await;
        let starts = self.finish(driver, context).await;

        assert_answered(&result, context);
        assert_eq!(
            usize::try_from(starts).unwrap(),
            attempt + 1,
            "{context}: the call started {starts} requests"
        );
    }
}

async fn serve_attempt(
    mut stream: TcpStream,
    attempt: usize,
    answer: Answer,
    starts: Arc<AtomicU32>,
    events: mpsc::UnboundedSender<PeerReport>,
    mut halt: watch::Receiver<bool>,
) {
    let served = answer_attempt(&mut stream, attempt, answer, &starts, &events, &mut halt).await;
    if let Err(fault) = served {
        send_report(&events, fixture_fault(attempt, fault));
    }
    // Dropping the stream closes whatever the reply left open.
}

/// Read one attempt's request, write its scripted reply, and report each phase
/// it reaches; a transport fault that is not the client leaving is returned.
async fn answer_attempt(
    stream: &mut TcpStream,
    attempt: usize,
    answer: Answer,
    starts: &AtomicU32,
    events: &mpsc::UnboundedSender<PeerReport>,
    halt: &mut watch::Receiver<bool>,
) -> Result<(), Box<str>> {
    let body_limit = match answer {
        Answer::Ambiguous => AMBIGUOUS_BODY_PREFIX,
        Answer::Transient
        | Answer::TransientRetryAfter(_)
        | Answer::StallHead
        | Answer::StallBody
        | Answer::Complete
        | Answer::Exact(_)
        | Answer::Held(_)
        | Answer::Truncate => usize::MAX,
    };
    if !read_request(stream, starts, body_limit).await? {
        return Ok(());
    }
    send_report(events, Ok(PeerEvent::Started(attempt)));
    let reply = Reply::to(answer, attempt);
    stream
        .write_all(&reply.bytes)
        .await
        .map_err(|error| format!("writing the reply failed: {error}"))?;
    if let Some(reached) = reply.reached {
        send_report(events, Ok(reached));
    }
    match reply.closed {
        Some(closed) => report_client_close(stream, halt, events, closed).await,
        None => Ok(()),
    }
}

/// What one attempt writes once its request is read, and what it reports.
struct Reply {
    /// The bytes written back; empty for an attempt that sends nothing.
    bytes: Cow<'static, [u8]>,
    /// The phase reported once the bytes are written.
    reached: Option<PeerEvent>,
    /// The phase reported if the client closes the held connection; `None`
    /// closes the connection at once.
    closed: Option<PeerEvent>,
}

impl Reply {
    fn to(answer: Answer, attempt: usize) -> Self {
        let (bytes, reached, closed) = match answer {
            Answer::Transient => (
                Cow::Owned(transient_head(None)),
                None,
                Some(PeerEvent::Disposed(attempt)),
            ),
            Answer::TransientRetryAfter(value) => (
                Cow::Owned(transient_head(Some(value))),
                None,
                Some(PeerEvent::Disposed(attempt)),
            ),
            Answer::StallHead => (
                Cow::Borrowed(&[][..]),
                Some(PeerEvent::Stalled(attempt)),
                Some(PeerEvent::Released(attempt)),
            ),
            Answer::StallBody => (
                Cow::Borrowed(STALLED_BODY_PREFIX),
                Some(PeerEvent::Stalled(attempt)),
                Some(PeerEvent::Released(attempt)),
            ),
            Answer::Complete => (
                Cow::Borrowed(ANSWER),
                Some(PeerEvent::Answered(attempt)),
                None,
            ),
            Answer::Exact(wire) => (
                Cow::Borrowed(wire),
                Some(PeerEvent::Answered(attempt)),
                None,
            ),
            Answer::Held(head) => (
                Cow::Borrowed(head),
                None,
                Some(PeerEvent::Disposed(attempt)),
            ),
            Answer::Truncate => (
                Cow::Borrowed(TRUNCATED_BODY_PREFIX),
                Some(PeerEvent::Truncated(attempt)),
                None,
            ),
            Answer::Ambiguous => (Cow::Borrowed(&[][..]), None, None),
        };
        Self {
            bytes,
            reached,
            closed,
        }
    }
}

/// Read one request head and at most `body_limit` bytes of the body it
/// declares, counting its start at the first byte.
///
/// Reports whether that much arrived; a client that closed first leaves
/// nothing to answer. Any other transport fault is returned.
async fn read_request(
    stream: &mut TcpStream,
    starts: &AtomicU32,
    body_limit: usize,
) -> Result<bool, Box<str>> {
    let Some(head) = read_request_head(stream, starts).await? else {
        return Ok(false);
    };
    let length = declared_content_length(&head)?.min(body_limit);
    Ok(read_request_body(stream, length).await?.is_some())
}

/// Read one request head through its blank line, counting its start at the
/// first byte.
///
/// `None` means the client closed first. Any other transport fault is
/// returned.
async fn read_request_head(
    stream: &mut TcpStream,
    starts: &AtomicU32,
) -> Result<Option<Vec<u8>>, Box<str>> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(0) => return Ok(None),
            Ok(_) => {}
            Err(error) => return client_gone(&error, "reading the request head").map(|()| None),
        }
        if head.is_empty() {
            starts.fetch_add(1, Ordering::SeqCst);
        }
        head.push(byte[0]);
    }
    Ok(Some(head))
}

/// Read exactly `length` bytes of a request body.
///
/// `None` means the client closed first. Any other transport fault is
/// returned.
async fn read_request_body(
    stream: &mut TcpStream,
    length: usize,
) -> Result<Option<Vec<u8>>, Box<str>> {
    let mut body = vec![0_u8; length];
    match stream.read_exact(&mut body).await {
        Ok(_) => Ok(Some(body)),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(None),
        Err(error) => client_gone(&error, "reading the request body").map(|()| None),
    }
}

/// `Ok` for a client that went away, and the fault for any other transport
/// error.
fn client_gone(error: &std::io::Error, operation: &str) -> Result<(), Box<str>> {
    match super::http::is_closed_connection_error(error) {
        true => Ok(()),
        false => Err(format!("the scripted upstream failed {operation}: {error}").into_boxed_str()),
    }
}

/// The byte count a request head declares, or zero when it declares none.
fn declared_content_length(head: &[u8]) -> Result<usize, Box<str>> {
    let head = std::str::from_utf8(head)
        .map_err(|error| format!("the request head is not UTF-8: {error}"))?;
    super::http::header_values(head, "content-length")
        .next()
        .map_or(Ok(0), |value| {
            value
                .parse()
                .map_err(|error| format!("Content-Length {value:?} is unreadable: {error}").into())
        })
}

/// Require the call to have answered with the scripted complete body.
fn assert_answered(result: &Option<Result<ClientResponse, RuntimeError>>, context: &str) {
    match result {
        Some(Ok(response)) => {
            assert_eq!(response.status(), 200, "{context}: final status");
            assert_eq!(response.body_bytes(), b"ok", "{context}: final body");
        }
        other => panic!("{context}: returned {}", describe(other)),
    }
}

/// Hold the connection until the client closes it or the row halts the
/// upstream, and report `closed` only when the client closed it.
async fn report_client_close(
    stream: &mut TcpStream,
    halt: &mut watch::Receiver<bool>,
    events: &mpsc::UnboundedSender<PeerReport>,
    closed: PeerEvent,
) -> Result<(), Box<str>> {
    let Some(held) = unless_halted(halt, until_peer_closed(stream)).await else {
        return Ok(());
    };
    held.map_err(|error| format!("the scripted upstream failed holding the connection: {error}"))?;
    send_report(events, Ok(closed));
    Ok(())
}

/// One request a [`RecordingUpstream`] read whole.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Recorded {
    /// The accept index of the connection the request arrived on.
    pub connection: usize,
    /// The request head, through its blank line.
    pub head: Box<str>,
    /// The body the head declared.
    pub body: Box<[u8]>,
}

impl Recorded {
    /// One field of the request line: the method, the target, then the version.
    fn request_line_field(&self, index: usize) -> &str {
        self.head
            .split("\r\n")
            .next()
            .and_then(|line| line.split(' ').nth(index))
            .unwrap_or_default()
    }

    /// The method token, exactly as it was sent.
    pub fn method(&self) -> &str {
        self.request_line_field(0)
    }

    /// The request target, exactly as it was sent.
    pub fn target(&self) -> &str {
        self.request_line_field(1)
    }

    /// Every value sent for the header `name`, in the order sent.
    pub fn header_values<'a>(&'a self, name: &'a str) -> Box<[&'a str]> {
        super::http::header_values(&self.head, name).collect()
    }

    /// Every header name sent, in lowercase, once per line and in the order
    /// sent.
    pub fn header_names(&self) -> Box<[String]> {
        self.head
            .split("\r\n")
            .skip(1)
            .take_while(|line| !line.is_empty())
            .filter_map(|line| line.split_once(':'))
            .map(|(name, _)| name.trim().to_ascii_lowercase())
            .collect()
    }
}

/// What a recording upstream has read so far.
#[derive(Default)]
struct Seen {
    connections: AtomicUsize,
    starts: AtomicU32,
    requests: Mutex<Vec<Recorded>>,
}

/// Everything a [`RecordingUpstream`] read before it was stopped and joined.
#[derive(Debug)]
pub struct Recording {
    /// How many connections the upstream accepted.
    pub connections: usize,
    /// How many requests sent at least one byte.
    pub starts: u32,
    /// Every request read whole, in the order its last byte arrived.
    pub requests: Box<[Recorded]>,
}

impl Recording {
    /// Every request the peer read whole for `target`, in arrival order.
    pub fn sent_to(&self, target: &str) -> Box<[&Recorded]> {
        self.requests
            .iter()
            .filter(|request| request.target() == target)
            .collect()
    }

    /// The one request the peer read for `target`.
    pub fn only_sent_to(&self, row: &str, target: &str) -> &Recorded {
        match *self.sent_to(target) {
            [only] => only,
            [] => panic!("{row}: the peer read no request for {target}"),
            [..] => panic!("{row}: the peer read {target} more than once"),
        }
    }
}

/// The report for a fault of the accept loop or of one connection's task.
fn recording_fault(connection: usize, detail: Box<str>) -> Box<str> {
    format!("connection {connection}: {detail}").into_boxed_str()
}

/// How a recording upstream chooses the bytes it answers one request with.
type Responder = Arc<dyn Fn(&Recorded) -> Vec<u8> + Send + Sync>;

/// A raw upstream that keeps every request it reads and answers each one.
///
/// A connection stays open after its answer, so a client that reuses its
/// transport sends its next request on the same accept index. The reply is
/// written whole to every request. A row that sends HEAD therefore binds a
/// reply that declares no body.
///
/// It owns a [`HaltableListener`], which owns the accepted sockets. The only
/// reports it makes are faults, so [`Self::finish`] fails on any of them.
pub struct RecordingUpstream {
    listener: HaltableListener<Box<str>>,
    seen: Arc<Seen>,
}

impl RecordingUpstream {
    /// Bind an upstream that answers every request with `reply`.
    pub async fn bind(reply: Vec<u8>) -> Self {
        Self::bind_responder(move |_| reply.clone()).await
    }

    /// Bind an upstream that answers each request with the bytes `respond`
    /// returns for it.
    ///
    /// `respond` reads the whole request, so a row can script a redirect by
    /// target and point its `Location` at the `Host` the request named.
    pub async fn bind_responder(
        respond: impl Fn(&Recorded) -> Vec<u8> + Send + Sync + 'static,
    ) -> Self {
        let seen = Arc::new(Seen::default());
        let shared = Arc::clone(&seen);
        let respond: Responder = Arc::new(respond);
        let listener = HaltableListener::bind(
            "the recording upstream",
            move |_: &mpsc::UnboundedSender<Box<str>>| {
                move |stream, connection, events, halt| {
                    record_connection(
                        stream,
                        connection,
                        Arc::clone(&respond),
                        Arc::clone(&shared),
                        events,
                        halt,
                    )
                }
            },
            recording_fault,
        )
        .await;
        Self { listener, seen }
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.listener.addr()
    }

    pub fn url(&self, path: &str) -> String {
        format!("http://{}{path}", self.listener.addr())
    }

    /// Stop accepting, close every held connection, join the accept loop,
    /// prove the address is free, and return what the upstream read.
    ///
    /// The join runs first, so the returned record is complete: no connection
    /// task is left that could add to it.
    pub async fn finish(self, context: &str) -> Recording {
        let Self { listener, seen } = self;
        listener.finish(context, within_watchdog).await;
        let requests =
            std::mem::take(&mut *seen.requests.lock().unwrap_or_else(PoisonError::into_inner));
        Recording {
            connections: seen.connections.load(Ordering::SeqCst),
            starts: seen.starts.load(Ordering::SeqCst),
            requests: requests.into_boxed_slice(),
        }
    }
}

async fn record_connection(
    mut stream: TcpStream,
    connection: usize,
    respond: Responder,
    seen: Arc<Seen>,
    events: mpsc::UnboundedSender<Box<str>>,
    mut halt: watch::Receiver<bool>,
) {
    seen.connections.fetch_add(1, Ordering::SeqCst);
    let served = unless_halted(
        &mut halt,
        record_requests(&mut stream, connection, &*respond, &seen),
    )
    .await;
    if let Some(Err(fault)) = served {
        send_report(&events, recording_fault(connection, fault));
    }
    // Dropping the stream closes the connection the client may have pooled.
}

/// Record and answer every request one connection carries, until the client
/// closes it.
async fn record_requests(
    stream: &mut TcpStream,
    connection: usize,
    respond: &(dyn Fn(&Recorded) -> Vec<u8> + Send + Sync),
    seen: &Seen,
) -> Result<(), Box<str>> {
    while let Some(request) = read_recorded(stream, connection, &seen.starts).await? {
        let reply = respond(&request);
        seen.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request);
        if let Err(error) = stream.write_all(&reply).await {
            return client_gone(&error, "writing the reply");
        }
    }
    Ok(())
}

/// Read one whole request: its head and the body that head declares.
///
/// `None` means the client closed before the request ended.
async fn read_recorded(
    stream: &mut TcpStream,
    connection: usize,
    starts: &AtomicU32,
) -> Result<Option<Recorded>, Box<str>> {
    let Some(head) = read_request_head(stream, starts).await? else {
        return Ok(None);
    };
    let length = declared_content_length(&head)?;
    let Some(body) = read_request_body(stream, length).await? else {
        return Ok(None);
    };
    let head = String::from_utf8(head)
        .map_err(|error| format!("the request head is not UTF-8: {error}"))?;
    Ok(Some(Recorded {
        connection,
        head: head.into_boxed_str(),
        body: body.into_boxed_slice(),
    }))
}
