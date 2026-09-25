//! A scripted raw HTTP/1.1 upstream for retry rows that run under paused time.
//!
//! Each accepted connection is one client attempt, answered by the script entry
//! at its accept index. The upstream reports every phase it reaches as a
//! [`PeerEvent`], so a row advances the Tokio clock only after the peer has
//! acknowledged the phase the client is parked in. Real socket readiness still
//! takes real time, so a [`RunnableDriver`] keeps the paused runtime busy and
//! stops it from auto-advancing onto an unrelated deadline while a row waits.

use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use camber::RuntimeError;
use camber::http::{ClientBuilder, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::halt::{HaltableListener, send_report, unless_halted, until_peer_closed};

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

/// A complete final answer.
const ANSWER: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok";

/// How much of a request body an [`Answer::Ambiguous`] attempt reads before
/// closing. One byte is enough to prove the payload reached the peer.
const AMBIGUOUS_BODY_PREFIX: usize = 1;

/// Every method the public client offers, the safe ones first.
pub const CLIENT_METHODS: [&str; 7] = ["GET", "HEAD", "OPTIONS", "POST", "PUT", "PATCH", "DELETE"];

/// How many leading [`CLIENT_METHODS`] a replay cannot duplicate an effect of.
pub const SAFE_METHOD_COUNT: usize = 3;

/// The methods a replay could duplicate, in the order rows drive them.
pub const UNSAFE_METHODS: &[&str] = CLIENT_METHODS.split_at(SAFE_METHOD_COUNT).1;

/// Send one of [`CLIENT_METHODS`] through the public client.
pub async fn send_method(
    client: &ClientBuilder,
    method: &str,
    url: &str,
) -> Result<Response, RuntimeError> {
    match method {
        "GET" => client.get(url).await,
        "HEAD" => client.head(url).await,
        "OPTIONS" => client.options(url).await,
        "POST" => client.post(url, "post").await,
        "PUT" => client.put(url, "put").await,
        "PATCH" => client.patch(url, "patch").await,
        "DELETE" => client.delete(url).await,
        other => panic!("{other} is not a method the public client offers"),
    }
}

/// One client call running as its own task.
pub type Call = JoinHandle<Result<Response, RuntimeError>>;

/// Start one GET through the public client as its own task.
///
/// The task owns the client and the URL, so dropping the task drops the whole
/// call and nothing the row keeps.
pub fn spawn_get(client: ClientBuilder, url: String) -> Call {
    tokio::spawn(async move { client.get(&url).await })
}

/// The call's result, captured without asserting it.
///
/// `None` means the call never settled within the fixture watchdog.
pub async fn settled(call: Call) -> Option<Result<Response, RuntimeError>> {
    match within_watchdog(call).await {
        Some(Ok(result)) => Some(result),
        Some(Err(joined)) => panic!("the client task did not complete: {joined}"),
        None => None,
    }
}

/// A settled call's result in one line, for a failure message.
pub fn describe(result: &Option<Result<Response, RuntimeError>>) -> Box<str> {
    match result {
        None => "no result".into(),
        Some(Ok(response)) => format!("status {}", response.status()).into_boxed_str(),
        Some(Err(error)) => format!("{error:?}").into_boxed_str(),
    }
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
    /// The client closed a transient answer's connection.
    Disposed(usize),
    /// The attempt is parked at its scripted stall.
    Stalled(usize),
    /// A complete answer was written.
    Answered(usize),
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
        match within_watchdog(self.listener.recv()).await {
            Some(Some(Ok(event))) if event == expected => Ok(()),
            Some(Some(Ok(event))) => {
                Err(format!("the upstream reported {event:?}, not {expected:?}"))
            }
            Some(Some(Err(fault))) => {
                Err(format!("the upstream faulted before {expected:?}: {fault}"))
            }
            Some(None) => Err(format!("the upstream stopped before {expected:?}")),
            None => Err(format!("the upstream never reported {expected:?}")),
        }
    }

    /// Stop accepting, close every held peer, join the accept loop, stop the
    /// row's driver, prove the address is free, require every report consumed,
    /// and report the request starts the upstream saw.
    ///
    /// The driver stops before the address check because that check waits on
    /// the Tokio clock, which the paused runtime must be free to advance.
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
        | Answer::Complete => usize::MAX,
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
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(0) => return Ok(false),
            Ok(_) => {}
            Err(error) => return client_gone(&error, "reading the request head").map(|()| false),
        }
        if head.is_empty() {
            starts.fetch_add(1, Ordering::SeqCst);
        }
        head.push(byte[0]);
    }
    let mut body = vec![0_u8; declared_content_length(&head)?.min(body_limit)];
    match stream.read_exact(&mut body).await {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(error) => client_gone(&error, "reading the request body").map(|()| false),
    }
}

/// `Ok` for a client that went away, and the fault for any other transport
/// error.
fn client_gone(error: &std::io::Error, operation: &str) -> Result<(), Box<str>> {
    match crate::http::is_closed_connection_error(error) {
        true => Ok(()),
        false => Err(format!("the scripted upstream failed {operation}: {error}").into_boxed_str()),
    }
}

/// The byte count a request head declares, or zero when it declares none.
fn declared_content_length(head: &[u8]) -> Result<usize, Box<str>> {
    let head = std::str::from_utf8(head)
        .map_err(|error| format!("the request head is not UTF-8: {error}"))?;
    crate::http::header_values(head, "content-length")
        .next()
        .map_or(Ok(0), |value| {
            value
                .parse()
                .map_err(|error| format!("Content-Length {value:?} is unreadable: {error}").into())
        })
}

/// Require the call to have answered with the scripted complete body.
fn assert_answered(result: &Option<Result<Response, RuntimeError>>, context: &str) {
    match result {
        Some(Ok(response)) => {
            assert_eq!(response.status(), 200, "{context}: final status");
            assert_eq!(response.body(), "ok", "{context}: final body");
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
