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
use std::pin::Pin;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::Poll;
use std::time::Duration;

use bytes::Bytes;
use camber::__private::ClientBodyObservation;
use camber::RuntimeError;
use camber::http::Client;
use camber::http::ClientBody;
use camber::http::ClientError;
use camber::http::ClientResponse;
use camber::http::ClientStreamResponse;
use camber::http::mock::{self, ScopedTransferOwner};
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

/// What `send_streaming` returns.
pub type StreamHead = Result<ClientStreamResponse, RuntimeError>;

/// What one demand of a streamed body returns.
pub type StreamNext = Result<Option<Bytes>, RuntimeError>;

/// Send one GET through the streaming terminal.
pub async fn stream_get(client: &Client, url: &str) -> StreamHead {
    client.request("GET", url)?.send_streaming().await
}

/// The chunk one demand delivered, or a panic naming what it returned.
pub fn delivered_chunk(row: &str, next: StreamNext) -> Bytes {
    match next {
        Ok(Some(chunk)) => chunk,
        other => panic!("{row}: the demand delivered no chunk: {other:?}"),
    }
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

/// Start one streaming GET through the public client as its own task.
///
/// The task owns the client and the URL, as [`spawn_get`]'s does.
pub fn spawn_stream_get(client: Client, url: String) -> JoinHandle<StreamHead> {
    tokio::spawn(async move { stream_get(&client, &url).await })
}

/// The result of a call that runs as its own task, captured without
/// asserting it.
///
/// `None` means the call never settled within the fixture watchdog.
pub async fn settled<T>(call: JoinHandle<T>) -> Option<T> {
    match within_watchdog(call).await {
        Some(Ok(result)) => Some(result),
        Some(Err(joined)) => panic!("the client task did not complete: {joined}"),
        None => None,
    }
}

/// Whether the task of a call ended cancelled within the fixture watchdog.
///
/// A task that returned a result, or that never ended, was not cancelled.
pub async fn was_cancelled<T>(call: JoinHandle<T>) -> bool {
    matches!(within_watchdog(call).await, Some(Err(error)) if error.is_cancelled())
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

/// A call's result, or a panic that names the row that never settled.
///
/// Read only after the row's peers are joined.
pub fn required<T>(row: &str, result: Option<T>) -> T {
    result.unwrap_or_else(|| panic!("{row}: the call never settled"))
}

/// Whether `result` failed as this client category.
pub fn failed_as<T>(result: &Result<T, RuntimeError>, category: ClientError) -> bool {
    matches!(result, Err(RuntimeError::Client(found)) if *found == category)
}

/// The failure a row's call returned, or a panic naming the answer it got.
pub fn failure(row: &str, result: Result<ClientResponse, RuntimeError>) -> RuntimeError {
    match result {
        Err(error) => error,
        Ok(response) => panic!("{row}: the call answered {}", response.status()),
    }
}

/// Append one `name: value` line for each field.
fn push_lines(wire: &mut Vec<u8>, lines: &[(&str, &str)]) {
    for (name, value) in lines {
        wire.extend_from_slice(format!("{name}: {value}\r\n").as_bytes());
    }
}

/// A complete HTTP/1.1 answer that leaves its connection open.
pub fn answer(status_line: &str, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let mut wire = format!("HTTP/1.1 {status_line}\r\n").into_bytes();
    push_lines(&mut wire, headers);
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
    /// Hold the request for this long on the Tokio clock, then answer as
    /// [`Answer::StallBody`] does.
    ///
    /// The wait starts when the request is read. A row that holds the clock
    /// still from its call's entry therefore gets the head at entry plus this.
    StallBodyAfter(Duration),
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

/// The output of `future`, or a panic once the fixture watchdog expires.
///
/// The panic names the row and `what` did not settle.
pub async fn must_settle<F: Future>(row: &str, what: &str, future: F) -> F::Output {
    within_watchdog(future)
        .await
        .unwrap_or_else(|| panic!("{row}: {what} did not settle within the watchdog"))
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

/// Poll `future` once.
pub async fn poll_once<F: Future>(mut future: Pin<&mut F>) -> Poll<F::Output> {
    std::future::poll_fn(|context| Poll::Ready(future.as_mut().poll(context))).await
}

/// Observe actual collection before a row acts on a body-phase operation.
/// The watchdog bounds a failed observation; it does not advance virtual time.
pub async fn wait_for_body_collection(observed: &ScopedTransferOwner) -> Result<(), String> {
    yielded_until(|| observed.collected_chunks_polled() != 0)
        .await
        .ok_or_else(|| "the client never collected the body prefix".to_owned())
}

/// Yield until `ready` holds.
///
/// This observes. It moves no clock and commits nothing. `None` means the
/// fixture watchdog expired first.
pub async fn yielded_until(ready: impl Fn() -> bool) -> Option<()> {
    within_watchdog(async {
        while !ready() {
            tokio::task::yield_now().await;
        }
    })
    .await
}

/// Yield until the counts `read` returns satisfy `ready`, and return them.
///
/// This is the acknowledgement a row waits for. It observes. It moves no
/// clock and commits nothing. The error holds the counts the owner reached
/// when the fixture watchdog expired.
pub async fn counts_reached(
    read: impl Fn() -> ClientBodyObservation,
    ready: impl Fn(&ClientBodyObservation) -> bool,
) -> Result<ClientBodyObservation, ClientBodyObservation> {
    match yielded_until(|| ready(&read())).await {
        Some(()) => Ok(read()),
        None => Err(read()),
    }
}

/// Yield until the streaming owner's counts satisfy `ready`, and return them.
///
/// The error names `what` the owner never did, and the counts it reached.
pub async fn owner_reached(
    watched: &ScopedTransferOwner,
    what: &str,
    ready: impl Fn(&ClientBodyObservation) -> bool,
) -> Result<ClientBodyObservation, String> {
    counts_reached(|| watched.streamed(), ready)
        .await
        .map_err(|seen| format!("the body owner never {what}: {seen:?}"))
}

/// The counts [`owner_reached`] returns, or a panic naming the row and what
/// the owner never did.
pub async fn owner_must_reach(
    row: &str,
    watched: &ScopedTransferOwner,
    what: &str,
    ready: impl Fn(&ClientBodyObservation) -> bool,
) -> ClientBodyObservation {
    owner_reached(watched, what, ready)
        .await
        .unwrap_or_else(|unreached| panic!("{row}: {unreached}"))
}

/// Yield until the streaming owner is parked with no demand placed.
pub async fn owner_parked(watched: &ScopedTransferOwner) -> Result<ClientBodyObservation, String> {
    owner_reached(watched, "parked with no demand", |seen| {
        seen.awaiting_demand
    })
    .await
}

/// The counts of an owner parked with no demand placed, or a panic naming
/// the row.
///
/// The owner publishes this each time it parks, and withdraws it when it
/// accepts a demand. It is the acknowledgement a real-time row reads the
/// owner's counts behind. A row that reads them without it reads whatever the
/// owner had reached when the row was polled.
pub async fn parked(row: &str, watched: &ScopedTransferOwner) -> ClientBodyObservation {
    owner_parked(watched)
        .await
        .unwrap_or_else(|unreached| panic!("{row}: {unreached}"))
}

/// One demand, awaited to its result.
pub async fn demand(row: &str, body: &mut ClientBody) -> StreamNext {
    must_settle(row, "the demand", body.next()).await
}

/// Close the body and return what the close reported.
pub async fn close(row: &str, body: ClientBody) -> Result<(), RuntimeError> {
    must_settle(row, "the close", body.close()).await
}

/// Watch the body owners of the calls to the peer at `addr`.
pub fn watch_streams(addr: std::net::SocketAddr) -> ScopedTransferOwner {
    mock::transfer_owner(addr).unwrap_or_else(|error| panic!("a peer has one observer: {error:?}"))
}

/// Read demands until one delivers no chunk. Return the payload and that end.
///
/// The client delivers no empty chunk, so each one read is required nonempty.
/// One fixture watchdog bounds the whole read.
pub async fn read_out(row: &str, body: &mut ClientBody) -> (Vec<u8>, StreamNext) {
    must_settle(row, "the body", read_demands(row, body)).await
}

/// Read demands until one delivers no chunk, unbounded.
async fn read_demands(row: &str, body: &mut ClientBody) -> (Vec<u8>, StreamNext) {
    let mut payload = Vec::new();
    loop {
        match body.next().await {
            Ok(Some(chunk)) => {
                assert!(!chunk.is_empty(), "{row}: an empty chunk was delivered");
                payload.extend_from_slice(&chunk);
            }
            end => return (payload, end),
        }
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

    pub fn addr(&self) -> std::net::SocketAddr {
        self.listener.addr()
    }

    pub fn url(&self, path: &str) -> String {
        self.listener.http_url(path)
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
        | Answer::StallBodyAfter(_)
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
    if !held(reply.after, halt).await {
        return Ok(());
    }
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

/// Wait out a reply's scripted hold on the Tokio clock.
///
/// `false` means the row halted the upstream first, so nothing is written.
async fn held(after: Option<Duration>, halt: &mut watch::Receiver<bool>) -> bool {
    match after {
        Some(wait) => unless_halted(halt, tokio::time::sleep(wait))
            .await
            .is_some(),
        None => true,
    }
}

/// What one attempt writes once its request is read, and what it reports.
struct Reply {
    /// How long the attempt waits on the Tokio clock before it writes; `None`
    /// writes at once.
    after: Option<Duration>,
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
        let after = match answer {
            Answer::StallBodyAfter(wait) => Some(wait),
            Answer::Transient
            | Answer::TransientRetryAfter(_)
            | Answer::StallHead
            | Answer::StallBody
            | Answer::Complete
            | Answer::Exact(_)
            | Answer::Held(_)
            | Answer::Truncate
            | Answer::Ambiguous => None,
        };
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
            Answer::StallBody | Answer::StallBodyAfter(_) => (
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
            after,
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

/// One header a row keeps: its name and its value bytes.
pub type HeaderEntry = (Box<str>, Box<[u8]>);

/// Each header as an owned `(name, value)` entry, in the order given.
pub fn header_entries<'a>(
    headers: impl Iterator<Item = (&'a str, &'a [u8])>,
) -> Box<[HeaderEntry]> {
    headers
        .map(|(name, value)| (name.into(), value.into()))
        .collect()
}

/// Every value `headers` holds for `name`, in order. The name matches in any
/// ASCII case.
pub fn values_of<'a>(headers: &'a [HeaderEntry], name: &str) -> Box<[&'a [u8]]> {
    headers
        .iter()
        .filter(|(found, _)| found.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_ref())
        .collect()
}

/// What a recording upstream has read so far.
#[derive(Default)]
struct Seen {
    connections: AtomicUsize,
    starts: AtomicU32,
    requests: Mutex<Vec<Recorded>>,
}

impl Seen {
    /// Take what was read. The record is complete only after the accept loop
    /// and its connection tasks are joined.
    fn recording(&self) -> Recording {
        let requests =
            std::mem::take(&mut *self.requests.lock().unwrap_or_else(PoisonError::into_inner));
        Recording {
            connections: self.connections.load(Ordering::SeqCst),
            starts: self.starts.load(Ordering::SeqCst),
            requests: requests.into_boxed_slice(),
        }
    }

    /// Keep one request that was read whole.
    fn record(&self, request: Recorded) {
        self.requests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(request);
    }
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

    /// The accept index each request the peer read arrived on, in order.
    pub fn connections(&self) -> Box<[usize]> {
        self.requests.iter().map(|sent| sent.connection).collect()
    }

    /// The target of each request the peer read, in order.
    pub fn targets(&self) -> Box<[&str]> {
        self.requests.iter().map(Recorded::target).collect()
    }

    /// What the peer read before the row's own [`probe`].
    ///
    /// The probe must be the last connection the peer accepted, and its one
    /// request the only one that connection carried. A listener accepts in
    /// arrival order, so every connection made before the probe was accepted
    /// before it and is in the returned record.
    fn before_probe(self, row: &str) -> Self {
        let Self {
            connections,
            starts,
            requests,
        } = self;
        let last = connections.checked_sub(1);
        let (probes, sent): (Vec<Recorded>, Vec<Recorded>) = requests
            .into_vec()
            .into_iter()
            .partition(|request| Some(request.connection) == last);
        let targets: Box<[&str]> = probes.iter().map(Recorded::target).collect();
        assert_eq!(
            *targets,
            [PROBE_TARGET],
            "{row}: the last of {connections} connections did not carry the probe alone"
        );
        Self {
            connections: connections - 1,
            starts: starts - 1,
            requests: sent.into_boxed_slice(),
        }
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

/// How many ports [`loopback_pair`] tries before it fails the row.
const PAIR_ATTEMPTS: usize = 32;

/// One IPv4 and one IPv6 loopback listener on the same port, or `None` when
/// the IPv6 port the IPv4 bind chose is taken.
///
/// Any other IPv6 failure is a host without IPv6 loopback. A row that needs
/// both families fails there. It does not skip.
fn paired_listeners() -> Option<(std::net::TcpListener, std::net::TcpListener)> {
    let v4 = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .unwrap_or_else(|error| panic!("the IPv4 loopback bind failed: {error}"));
    let port = v4
        .local_addr()
        .unwrap_or_else(|error| panic!("the IPv4 loopback address is unreadable: {error}"))
        .port();
    match std::net::TcpListener::bind((std::net::Ipv6Addr::LOCALHOST, port)) {
        Ok(v6) => Some((v4, v6)),
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => None,
        Err(error) => panic!("IPv6 loopback is required, and its bind failed: {error}"),
    }
}

/// One IPv4 and one IPv6 loopback listener on the same port, for
/// [`RecordingUpstream::adopt_responder`].
///
/// A name has one port and any number of addresses. A row that answers one
/// name with both families therefore needs both peers on one port. Dropping
/// one half leaves a freed port, which refuses a dial at once.
pub fn loopback_pair() -> (std::net::TcpListener, std::net::TcpListener) {
    (0..PAIR_ATTEMPTS)
        .find_map(|_| paired_listeners())
        .unwrap_or_else(|| panic!("no port was free on both loopback families"))
}

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
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .unwrap_or_else(|error| panic!("the recording upstream: binding failed: {error}"));
        Self::adopt_responder(listener, respond)
    }

    /// Serve an already bound listener as [`Self::bind_responder`] serves the
    /// one it binds.
    ///
    /// For a row that chose the address: an IPv6 loopback peer, or one half of
    /// a pair that shares a port. It must be called inside a Tokio runtime,
    /// which owns the accept loop.
    pub fn adopt_responder(
        listener: std::net::TcpListener,
        respond: impl Fn(&Recorded) -> Vec<u8> + Send + Sync + 'static,
    ) -> Self {
        let seen = Arc::new(Seen::default());
        let shared = Arc::clone(&seen);
        let respond: Responder = Arc::new(respond);
        let listener = HaltableListener::adopt(
            "the recording upstream",
            listener,
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
        );
        Self { listener, seen }
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.listener.addr()
    }

    pub fn url(&self, path: &str) -> String {
        self.listener.http_url(path)
    }

    /// Stop accepting, close every held connection, join the accept loop,
    /// prove the address is free, and return what the upstream read.
    ///
    /// The join runs first, so the returned record is complete: no connection
    /// task is left that could add to it.
    pub async fn finish(self, context: &str) -> Recording {
        let Self { listener, seen } = self;
        listener.finish(context, within_watchdog).await;
        seen.recording()
    }

    /// Send the row's own [`probe`], then [`Self::finish`], and return what
    /// the upstream read before the probe.
    ///
    /// The halt wins over a pending accept, so a bare finish can discard a
    /// connection that was made and not yet accepted. The probe is answered
    /// only after every earlier connection was accepted. A count of zero in
    /// the returned record therefore means that nothing dialed this peer
    /// before the probe.
    pub async fn finish_probed(self, context: &str) -> Recording {
        let probed = within_watchdog(probe(self.addr())).await;
        let recording = self.finish(context).await;
        assert_eq!(
            probed,
            Some(Ok(())),
            "{context}: the fixture's own probe failed"
        );
        recording.before_probe(context)
    }
}

/// Count one connection, run `serving` unless the row halts the upstream
/// first, and report its fault.
async fn serve_counted(
    connection: usize,
    seen: &Seen,
    events: &mpsc::UnboundedSender<Box<str>>,
    halt: &mut watch::Receiver<bool>,
    serving: impl Future<Output = Result<(), Box<str>>>,
) {
    seen.connections.fetch_add(1, Ordering::SeqCst);
    if let Some(Err(fault)) = unless_halted(halt, serving).await {
        send_report(events, recording_fault(connection, fault));
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
    let serving = record_requests(&mut stream, connection, &*respond, &seen);
    serve_counted(connection, &seen, &events, &mut halt, serving).await;
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
        seen.record(request);
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

/// The target of the request a row itself sends with [`probe`].
pub const PROBE_TARGET: &str = "/probe";

/// Send one request to the peer at `addr` from the row, not from a client,
/// and read its answer's head.
///
/// A listener accepts in arrival order. A connection a call had dialed is
/// therefore accepted before this one, and the accept count shows it. Without
/// the probe, a dial still in the backlog at teardown would not be counted.
pub async fn probe(addr: std::net::SocketAddr) -> Result<(), String> {
    let mut stream = TcpStream::connect(addr)
        .await
        .map_err(|error| format!("the probe could not connect: {error}"))?;
    stream
        .write_all(format!("GET {PROBE_TARGET} HTTP/1.1\r\nHost: probe\r\n\r\n").as_bytes())
        .await
        .map_err(|error| format!("the probe could not write: {error}"))?;
    let mut reply = Vec::new();
    let mut byte = [0_u8; 1];
    while !reply.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(0) => return Err("the peer closed before it answered the probe".into()),
            Ok(_) => reply.push(byte[0]),
            Err(error) => return Err(format!("the probe could not read: {error}")),
        }
    }
    Ok(())
}

/// One step of the answer a [`PacedUpstream`] writes to one request.
#[derive(Clone, Debug)]
pub enum Pace {
    /// Write these bytes.
    Send(Vec<u8>),
    /// Wait until the row opens one more gate, or the client closes.
    Gate,
    /// Wait until the client closes the connection.
    UntilClosed,
    /// Close the connection.
    Close,
}

/// How far a paced upstream's connections have come, as counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Paced {
    /// Requests read whole.
    pub requests: usize,
    /// `Send` steps written whole.
    pub sends: usize,
    /// Gates a connection reached, the ones it has passed included.
    pub gates: usize,
    /// Connections the client closed while a step still held them.
    pub client_closes: usize,
}

/// The framing line of a body with no declared length.
pub const CHUNKED: (&str, &str) = ("Transfer-Encoding", "chunked");

/// A head with these header lines and no body.
///
/// The row states the framing: a `Content-Length` or a `Transfer-Encoding`
/// line, or neither.
pub fn paced_head(status_line: &str, headers: &[(&str, &str)]) -> Vec<u8> {
    let mut wire = format!("HTTP/1.1 {status_line}\r\n").into_bytes();
    push_lines(&mut wire, headers);
    wire.extend_from_slice(b"\r\n");
    wire
}

/// One chunked-transfer frame that carries `payload`.
pub fn paced_chunk(payload: &[u8]) -> Vec<u8> {
    let mut frame = format!("{:x}\r\n", payload.len()).into_bytes();
    frame.extend_from_slice(payload);
    frame.extend_from_slice(b"\r\n");
    frame
}

/// The last chunked-transfer frame, with these trailer lines.
pub fn paced_end(trailers: &[(&str, &str)]) -> Vec<u8> {
    let mut frame = b"0\r\n".to_vec();
    push_lines(&mut frame, trailers);
    frame.extend_from_slice(b"\r\n");
    frame
}

/// What the connections of one paced upstream share.
struct PacedShared {
    seen: Seen,
    progress: watch::Sender<Paced>,
    gates: tokio::sync::Semaphore,
}

impl PacedShared {
    fn advance(&self, step: impl FnOnce(&mut Paced)) {
        self.progress.send_modify(step);
    }
}

/// A raw upstream that writes each answer one step at a time.
///
/// A row that reads a streamed body needs a peer that sends the head and then
/// withholds the body, or sends one chunk and waits. Each request is answered
/// by the [`Pace`] steps the row's function returns for it. A connection
/// whose steps end without [`Pace::Close`] reads its next request.
///
/// The gates are one count for the whole upstream: [`Self::open_gate`] lets
/// one parked [`Pace::Gate`] pass, on whichever connection reaches it. A row
/// that gates therefore holds one gated answer at a time.
///
/// It reports faults only, so [`Self::finish`] fails on any of them. A row
/// reads phases from [`Self::reached`], which holds counts and no queue.
pub struct PacedUpstream {
    listener: HaltableListener<Box<str>>,
    shared: Arc<PacedShared>,
    progress: watch::Receiver<Paced>,
}

impl PacedUpstream {
    /// Bind an upstream that answers each request with the steps `pacer`
    /// returns for it.
    pub async fn bind(pacer: impl Fn(&Recorded) -> Arc<[Pace]> + Send + Sync + 'static) -> Self {
        let (progress_tx, progress) = watch::channel(Paced::default());
        let shared = Arc::new(PacedShared {
            seen: Seen::default(),
            progress: progress_tx,
            gates: tokio::sync::Semaphore::new(0),
        });
        let serving = Arc::clone(&shared);
        let pacer = Arc::new(pacer);
        let listener = HaltableListener::bind(
            "the paced upstream",
            move |_: &mpsc::UnboundedSender<Box<str>>| {
                move |stream, connection, events, halt| {
                    pace_connection(
                        stream,
                        connection,
                        Arc::clone(&serving),
                        Arc::clone(&pacer),
                        events,
                        halt,
                    )
                }
            },
            recording_fault,
        )
        .await;
        Self {
            listener,
            shared,
            progress,
        }
    }

    /// Bind an upstream that answers every request with `steps`.
    pub async fn bind_steps(steps: Vec<Pace>) -> Self {
        let steps: Arc<[Pace]> = steps.into();
        Self::bind(move |_| Arc::clone(&steps)).await
    }

    pub fn addr(&self) -> std::net::SocketAddr {
        self.listener.addr()
    }

    pub fn url(&self, path: &str) -> String {
        self.listener.http_url(path)
    }

    /// Let one [`Pace::Gate`] pass.
    pub fn open_gate(&self) {
        self.shared.gates.add_permits(1);
    }

    /// The counts, now.
    pub fn progress(&self) -> Paced {
        *self.progress.borrow()
    }

    /// Wait until the counts satisfy `ready`, and return them.
    ///
    /// The wait is bounded by the fixture watchdog. The error names `what`
    /// the upstream never did, and the counts it reached.
    pub async fn reached(
        &mut self,
        what: &str,
        ready: impl Fn(&Paced) -> bool,
    ) -> Result<Paced, String> {
        let waited = within_watchdog(self.progress.wait_for(|seen| ready(seen)))
            .await
            .map(|found| found.map(|seen| *seen).map_err(drop));
        match waited {
            Some(Ok(seen)) => Ok(seen),
            Some(Err(())) => Err(format!("the paced upstream stopped before it {what}")),
            None => Err(format!(
                "the paced upstream never {what}: {:?}",
                self.progress()
            )),
        }
    }

    /// Wait until the client has closed one held connection, and return the
    /// counts.
    pub async fn client_left(&mut self) -> Result<Paced, String> {
        self.reached("saw the client close", |seen| seen.client_closes == 1)
            .await
    }

    /// The counts [`Self::client_left`] returns, or a panic naming the row and
    /// the connection the client kept open.
    pub async fn must_see_client_left(&mut self, row: &str) -> Paced {
        self.client_left()
            .await
            .unwrap_or_else(|open| panic!("{row}: {open}"))
    }

    /// Stop accepting, close every held connection, join the accept loop,
    /// prove the address is free, and return what the upstream read.
    pub async fn finish(self, context: &str) -> Recording {
        let Self {
            listener, shared, ..
        } = self;
        listener.finish(context, within_watchdog).await;
        shared.seen.recording()
    }
}

/// Serve one connection with the steps `pacer` chooses for each request.
async fn pace_connection<P>(
    mut stream: TcpStream,
    connection: usize,
    shared: Arc<PacedShared>,
    pacer: Arc<P>,
    events: mpsc::UnboundedSender<Box<str>>,
    mut halt: watch::Receiver<bool>,
) where
    P: Fn(&Recorded) -> Arc<[Pace]> + Send + Sync + 'static,
{
    let serving = pace_requests(&mut stream, connection, &shared, &*pacer);
    serve_counted(connection, &shared.seen, &events, &mut halt, serving).await;
    // Dropping the stream closes whatever the steps left open.
}

/// Record every request one connection carries and write its steps, until a
/// step ends the connection or the client closes it.
async fn pace_requests<P>(
    stream: &mut TcpStream,
    connection: usize,
    shared: &PacedShared,
    pacer: &P,
) -> Result<(), Box<str>>
where
    P: Fn(&Recorded) -> Arc<[Pace]> + Send + Sync + 'static,
{
    while let Some(request) = read_recorded(stream, connection, &shared.seen.starts).await? {
        let steps = pacer(&request);
        shared.seen.record(request);
        shared.advance(|seen| seen.requests += 1);
        if !pace_steps(stream, &steps, shared).await? {
            return Ok(());
        }
    }
    Ok(())
}

/// Run one answer's steps in order. `false` ends the connection.
async fn pace_steps(
    stream: &mut TcpStream,
    steps: &[Pace],
    shared: &PacedShared,
) -> Result<bool, Box<str>> {
    for step in steps {
        if !pace_step(stream, step, shared).await? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Run one step. `false` ends the connection.
async fn pace_step(
    stream: &mut TcpStream,
    step: &Pace,
    shared: &PacedShared,
) -> Result<bool, Box<str>> {
    match step {
        Pace::Send(bytes) => pace_send(stream, bytes, shared).await,
        Pace::Gate => pace_gate(stream, shared).await,
        Pace::UntilClosed => client_closed(until_peer_closed(stream).await, shared),
        Pace::Close => Ok(false),
    }
}

/// Write one step's bytes. A client that left first ends the connection.
async fn pace_send(
    stream: &mut TcpStream,
    bytes: &[u8],
    shared: &PacedShared,
) -> Result<bool, Box<str>> {
    let Err(error) = stream.write_all(bytes).await else {
        shared.advance(|seen| seen.sends += 1);
        return Ok(true);
    };
    client_gone(&error, "writing a paced step")?;
    shared.advance(|seen| seen.client_closes += 1);
    Ok(false)
}

/// Park at one gate until the row opens it or the client closes.
async fn pace_gate(stream: &mut TcpStream, shared: &PacedShared) -> Result<bool, Box<str>> {
    shared.advance(|seen| seen.gates += 1);
    tokio::select! {
        permit = shared.gates.acquire() => opened_gate(permit),
        closed = until_peer_closed(stream) => client_closed(closed, shared),
    }
}

/// Spend the permit of one opened gate.
fn opened_gate(
    permit: Result<tokio::sync::SemaphorePermit<'_>, tokio::sync::AcquireError>,
) -> Result<bool, Box<str>> {
    permit
        .map_err(|_| Box::<str>::from("the paced gates were closed"))?
        .forget();
    Ok(true)
}

/// Count a client's close of a held connection, and end the connection.
fn client_closed(closed: std::io::Result<()>, shared: &PacedShared) -> Result<bool, Box<str>> {
    closed.map_err(|error| {
        format!("the paced upstream failed holding the connection: {error}").into_boxed_str()
    })?;
    shared.advance(|seen| seen.client_closes += 1);
    Ok(false)
}

/// Read either client terminal through its public API, including streamed settlement.
pub async fn client_answer(
    request: camber::http::ClientRequest<'_>,
    streaming: bool,
) -> Result<(u16, Vec<u8>), camber::RuntimeError> {
    match streaming {
        false => {
            let response = request.send().await?;
            Ok((response.status(), response.body_bytes().to_vec()))
        }
        true => {
            let response = request.send_streaming().await?;
            let status = response.status();
            let mut body = response.into_body();
            let mut bytes = Vec::new();
            while let Some(chunk) = body.next().await? {
                bytes.extend_from_slice(&chunk);
            }
            body.close().await?;
            Ok((status, bytes))
        }
    }
}
