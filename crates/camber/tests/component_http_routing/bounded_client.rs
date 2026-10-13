//! 7.T1 and 7.T3: the outbound client collects under one checked ceiling and
//! one stored response policy. Outbound 4.T3: no write to that policy removes
//! the operation total.

use crate::retry_upstream::watch_streams;

use camber::__private::client_transport_is_built;
use camber::RuntimeError;
use camber::http::Client;
use camber::http::ClientError;
use camber::http::ClientResponse;
use camber::http::ClientTimeout;
use camber::http::RetryPolicy;
use camber::http::{self, Request, Response, Router, TransferBudget};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;

/// How long any one row waits for a client call that must already have ended.
///
/// A row that refuses a body proves it stopped reading by returning while its
/// upstream still holds an unterminated payload open. Expiry here is that
/// proof failing, not a slow machine: nothing in a row is waiting on the
/// network for longer than a loopback write.
const ROW_BOUND: Duration = Duration::from_secs(5);

/// How long a row waits for its upstream owner to finish after release.
const TEARDOWN_BOUND: Duration = Duration::from_secs(5);

/// The response ceiling every retention row is measured against.
pub(crate) const CEILING: usize = 16;

/// The bytes one admitted frame carries, exactly filling [`CEILING`].
pub(crate) const ADMITTED: &[u8] = b"0123456789abcdef";

/// The bytes of the frame that crosses [`CEILING`].
pub(crate) const CROSSING: &[u8] = b"crossing";

/// What ends a chunked payload.
const CHUNKED_END: &[u8] = b"0\r\n\r\n";

/// A chunked head, which states no length at all.
pub(crate) const CHUNKED_HEAD: &str = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

/// One client whose response ceiling is `CEILING` and whose deadlines are long
/// enough that no row can reach them.
pub(crate) fn bounded_client() -> Client {
    http::client().response_budget(
        TransferBudget::bounded(CEILING, Duration::from_secs(30), Duration::from_secs(30))
            .expect("a finite client response budget"),
    )
}

/// The bytes of one chunked-transfer frame carrying `payload`.
pub(crate) fn chunk_frame(payload: &[u8]) -> Box<[u8]> {
    let mut frame = format!("{:x}\r\n", payload.len()).into_bytes();
    frame.extend_from_slice(payload);
    frame.extend_from_slice(b"\r\n");
    frame.into_boxed_slice()
}

/// A local upstream that answers one request with exactly the bytes a row names.
///
/// The whole answer is scripted rather than produced by a served route: the
/// claims here are about a declared length that outruns its ceiling, a frame
/// that crosses one, and trailers that carry no payload, and only writing the
/// wire directly puts all three in one deterministic upstream.
pub(crate) struct ScriptedUpstream {
    pub(crate) addr: SocketAddr,
    /// The signal that lets a held connection go. `None` once released.
    release: Option<oneshot::Sender<()>>,
    /// The single owner of this upstream's connection. `None` once joined.
    served: Option<tokio::task::JoinHandle<Result<(), Box<str>>>>,
}

impl ScriptedUpstream {
    /// Bind one upstream and answer the first request with `head` then `body`.
    ///
    /// `hold` keeps the connection open with the payload unterminated, so a row
    /// can prove the client stopped reading rather than that the body ended.
    pub(crate) async fn start(head: &'static str, body: Box<[Box<[u8]>]>, hold: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("an upstream listener");
        let addr = listener.local_addr().expect("an upstream address");
        let (release, released) = oneshot::channel();
        let served = tokio::spawn(answer_once(listener, head, body, hold, released));
        Self {
            addr,
            release: Some(release),
            served: Some(served),
        }
    }

    /// Let the held connection go and join the owner that holds it.
    pub(crate) async fn finish(mut self, row: &str) {
        drop(self.release.take());
        let Some(served) = self.served.take() else {
            return;
        };
        let joined = tokio::time::timeout(TEARDOWN_BOUND, served)
            .await
            .unwrap_or_else(|_| panic!("{row}: the upstream owner never returned"))
            .unwrap_or_else(|error| panic!("{row}: the upstream owner did not join: {error}"));
        assert!(joined.is_ok(), "{row}: the upstream failed: {joined:?}");
    }
}

impl Drop for ScriptedUpstream {
    fn drop(&mut self) {
        // A row that panicked before its own release still leaves no owner
        // running: this is the safety net behind `finish`, not the path.
        if let Some(served) = self.served.take() {
            served.abort();
        }
    }
}

/// Answer one request with the scripted bytes, then hold or close.
async fn answer_once(
    listener: TcpListener,
    head: &'static str,
    body: Box<[Box<[u8]>]>,
    hold: bool,
    released: oneshot::Receiver<()>,
) -> Result<(), Box<str>> {
    let (mut stream, _) = listener
        .accept()
        .await
        .map_err(|error| -> Box<str> { format!("upstream accept failed: {error}").into() })?;
    read_head(&mut stream).await?;
    write_all(&mut stream, head.as_bytes()).await?;
    for frame in body {
        write_all(&mut stream, &frame).await?;
    }
    match hold {
        true => drop(released.await),
        false => drop(stream),
    }
    Ok(())
}

/// Read one request head, so the answer is written to a peer that asked for it.
async fn read_head(stream: &mut TcpStream) -> Result<(), Box<str>> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        match stream.read(&mut byte).await {
            Ok(0) => return Err("the peer closed before its request head ended".into()),
            Ok(_) => head.push(byte[0]),
            Err(error) => {
                return Err(format!("upstream head read failed: {error}").into());
            }
        }
    }
    Ok(())
}

/// Write and flush one scripted span, so each frame reaches the wire on its own.
async fn write_all(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), Box<str>> {
    stream
        .write_all(bytes)
        .await
        .map_err(|error| -> Box<str> { format!("upstream write failed: {error}").into() })?;
    stream
        .flush()
        .await
        .map_err(|error| -> Box<str> { format!("upstream flush failed: {error}").into() })
}

/// Run one client call under the row bound, so a poll after a terminal fails
/// the row instead of parking it.
pub(crate) async fn within_bound<T>(row: &str, call: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(ROW_BOUND, call)
        .await
        .unwrap_or_else(|_| panic!("{row}: the client call never returned"))
}

/// The one typed cause a crossed client ceiling reports, naming the ceiling
/// the collection was refused under.
pub(crate) fn assert_client_limit(
    row: &str,
    limit: usize,
    result: Result<ClientResponse, RuntimeError>,
) {
    match result {
        Err(RuntimeError::Client(ClientError::ResponseTooLarge { limit: refused })) => {
            assert_eq!(refused, limit, "{row}: the refusal names another ceiling");
        }
        Err(other) => panic!("{row}: expected the client response ceiling, got {other:?}"),
        Ok(response) => panic!(
            "{row}: expected a refusal, got {} bytes",
            response.body_bytes().len()
        ),
    }
}

/// A head declaring more than [`CEILING`].
pub(crate) const DECLARED_HEAD: &str =
    "HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n";

/// The payload length [`DECLARED_HEAD`] declares.
pub(crate) const DECLARED_LENGTH: usize = 64;

/// What the production collector published while it refused one answer.
pub(crate) struct Collected {
    pub(crate) chunks_polled: usize,
    pub(crate) peak_retained: usize,
}

/// Send one GET for `path` under [`CEILING`] at a scripted answer, and return
/// the call's result with what the collector published.
///
/// The fixture is released and joined before the result is returned.
pub(crate) async fn collect_refused(
    row: &str,
    path: &str,
    head: &'static str,
    body: Box<[Box<[u8]>]>,
    hold: bool,
) -> (Result<ClientResponse, RuntimeError>, Collected, SocketAddr) {
    let upstream = ScriptedUpstream::start(head, body, hold).await;
    let addr = upstream.addr;
    let observed = watch_streams(addr);

    let result: Result<ClientResponse, RuntimeError> =
        within_bound(row, bounded_client().get(&format!("http://{addr}{path}"))).await;
    let collected = Collected {
        chunks_polled: observed.collected_chunks_polled(),
        peak_retained: observed.collected_peak_retained_bytes(),
    };
    upstream.finish(row).await;
    (result, collected, addr)
}

/// A declaration above the ceiling is refused before one frame is polled.
async fn trustworthy_declaration_is_refused_before_a_frame_is_polled() {
    let row = "declared oversize";
    let body = vec![vec![b'x'; DECLARED_LENGTH].into_boxed_slice()].into_boxed_slice();
    let (result, collected, _) =
        collect_refused(row, "/declared", DECLARED_HEAD, body, false).await;

    assert_client_limit(row, CEILING, result);
    assert_eq!(
        collected.chunks_polled, 0,
        "{row}: a trustworthy declaration must be refused before any frame is read",
    );
    assert_eq!(
        collected.peak_retained, 0,
        "{row}: nothing may be retained for a refused declaration",
    );
}

/// The frame that crosses the ceiling is dropped, and nothing is read after it.
async fn crossing_frame_is_dropped_and_ends_the_read() {
    let row = "crossing frame";
    let body = vec![chunk_frame(ADMITTED), chunk_frame(CROSSING)].into_boxed_slice();
    // The upstream never terminates its payload. A collector that polled again
    // after fixing its terminal would wait here until the row bound expired.
    let (result, collected, _) = collect_refused(row, "/crossing", CHUNKED_HEAD, body, true).await;

    assert_client_limit(row, CEILING, result);
    assert!(
        collected.chunks_polled > 0,
        "{row}: the crossing frame must have been read before it was refused",
    );
    // The production collector publishes what it holds after every accounting
    // decision, refusals included, so this reads the buffer rather than the
    // total the collector was permitted. A collector that appended the
    // crossing frame and only then refused would report those bytes here.
    assert_eq!(
        collected.peak_retained, CEILING,
        "{row}: the admitted frame must be kept whole and the crossing frame must add nothing",
    );
}

/// A frame that exactly fills the ceiling is retained whole.
async fn exact_boundary_frame_is_retained_whole() {
    let row = "exact boundary";
    let upstream = admitted_upstream().await;
    let observed = watch_streams(upstream.addr);

    let response: ClientResponse = within_bound(
        row,
        bounded_client().get(&format!("http://{}/exact", upstream.addr)),
    )
    .await
    .expect("a body that exactly fills its ceiling is admitted");

    assert_eq!(response.status(), 200, "{row}: unexpected status");
    assert_eq!(
        response.body_bytes(),
        ADMITTED,
        "{row}: the admitted payload must arrive whole",
    );
    assert_eq!(
        observed.collected_peak_retained_bytes(),
        CEILING,
        "{row}: the exact-boundary body must be retained in full",
    );
    upstream.finish(row).await;
}

/// Trailers cost no payload bytes.
async fn trailers_add_no_retained_bytes() {
    let row = "trailers";
    let trailer: Box<[u8]> = b"0\r\nx-checked: yes\r\n\r\n".as_slice().into();
    let upstream = ScriptedUpstream::start(
        CHUNKED_HEAD,
        vec![chunk_frame(ADMITTED), trailer].into_boxed_slice(),
        false,
    )
    .await;
    let observed = watch_streams(upstream.addr);

    let response: ClientResponse = within_bound(
        row,
        bounded_client().get(&format!("http://{}/trailers", upstream.addr)),
    )
    .await
    .expect("a trailered body at its ceiling is admitted");

    assert_eq!(
        response.body_bytes(),
        ADMITTED,
        "{row}: trailers must not reach the payload",
    );
    assert_eq!(
        observed.collected_peak_retained_bytes(),
        CEILING,
        "{row}: trailers must add no retained bytes",
    );
    upstream.finish(row).await;
}

/// The bytes a body carries past [`CEILING`], which only the named opt-out
/// admits.
const ABOVE_CEILING: &[u8] = b"0123456789abcdef and eight more";

/// The documented ceiling a client collects under when its caller wrote none.
pub(crate) const DEFAULT_CEILING: usize = 8 * 1024 * 1024;

/// A head declaring nine MiB: one above the eight-MiB default a caller who
/// wrote no ceiling of their own collects under. The peer never sends them.
const ABOVE_DEFAULT_HEAD: &str =
    "HTTP/1.1 200 OK\r\nContent-Length: 9437184\r\nConnection: close\r\n\r\n";

/// The named opt-out removes the ceiling and nothing else.
async fn named_opt_out_admits_a_body_above_the_ceiling() {
    let row = "named opt-out";
    let upstream = above_ceiling_upstream().await;
    let observed = watch_streams(upstream.addr);

    let response: ClientResponse = within_bound(
        row,
        bounded_client()
            .unbounded_response()
            .get(&format!("http://{}/opt-out", upstream.addr)),
    )
    .await
    .expect("the named opt-out admits a body above the configured ceiling");

    assert_eq!(
        response.body_bytes(),
        ABOVE_CEILING,
        "{row}: the opted-out body must arrive whole",
    );
    assert_eq!(
        observed.collected_peak_retained_bytes(),
        ABOVE_CEILING.len(),
        "{row}: the opted-out collection retains everything it was sent",
    );
    upstream.finish(row).await;
}

/// The free functions collect under the documented default, with no builder in
/// sight to have written one.
async fn default_ceiling_refuses_a_declaration_on_the_free_path() {
    let row = "free-function default";
    let upstream = ScriptedUpstream::start(ABOVE_DEFAULT_HEAD, Box::default(), true).await;
    let observed = watch_streams(upstream.addr);

    let result: Result<ClientResponse, RuntimeError> =
        within_bound(row, http::get(&format!("http://{}/default", upstream.addr))).await;

    assert_client_limit(row, DEFAULT_CEILING, result);
    assert_eq!(
        observed.collected_chunks_polled(),
        0,
        "{row}: the default ceiling must refuse the declaration before any frame is read",
    );
    upstream.finish(row).await;
}

/// The checked addition every retained frame passes through, including the
/// total that cannot be represented at all.
fn checked_addition_refuses_an_overflowing_total() {
    assert_eq!(
        camber::__private::checked_body_frame_total(0, ADMITTED.len(), CEILING),
        Some(CEILING),
        "a frame that exactly fills the ceiling is admitted",
    );
    assert_eq!(
        camber::__private::checked_body_frame_total(CEILING, 1, CEILING),
        None,
        "a frame past the ceiling is refused",
    );
    assert_eq!(
        camber::__private::checked_body_frame_total(usize::MAX, 1, usize::MAX),
        None,
        "an overflowing total is a limit failure, never a wrapped small count",
    );
    assert!(
        camber::__private::declared_length_exceeds_limit(u64::MAX, CEILING),
        "a declaration no machine can hold is above every ceiling",
    );
}

/// 7.T1
#[camber::test]
async fn checked_collectors_drop_the_crossing_frame_without_excess_retention() {
    trustworthy_declaration_is_refused_before_a_frame_is_polled().await;
    crossing_frame_is_dropped_and_ends_the_read().await;
    exact_boundary_frame_is_retained_whole().await;
    trailers_add_no_retained_bytes().await;
    named_opt_out_admits_a_body_above_the_ceiling().await;
    default_ceiling_refuses_a_declaration_on_the_free_path().await;
    checked_addition_refuses_an_overflowing_total();
}

/// The response ceiling one call-order row writes as a whole policy.
const WHOLESALE_CEILING: usize = 32;
/// The response-idle deadline only the wholesale policy carries.
const WHOLESALE_IDLE: Duration = Duration::from_secs(11);
/// The request-total deadline only the wholesale policy carries.
const WHOLESALE_TOTAL: Duration = Duration::from_secs(12);
/// The request-total deadline one row writes as a single field.
const FIELD_TOTAL: Duration = Duration::from_secs(13);
/// The response-idle deadline one row writes as a single field.
const FIELD_IDLE: Duration = Duration::from_secs(14);

/// The whole response policy a call-order row writes.
fn wholesale_policy() -> TransferBudget {
    TransferBudget::bounded(WHOLESALE_CEILING, WHOLESALE_IDLE, WHOLESALE_TOTAL)
        .expect("a finite wholesale response policy")
}

/// The last write to one dimension is the one the stored policy keeps.
fn call_order_decides_each_stored_dimension() {
    let wholesale = wholesale_policy();

    // A single-field write after the whole policy replaces only its own field.
    let narrowed = http::client()
        .response_budget(wholesale)
        .request_timeout(FIELD_TOTAL)
        .response_idle_timeout(FIELD_IDLE)
        .response_policy();
    assert_eq!(narrowed.max_bytes(), Some(WHOLESALE_CEILING));
    assert_eq!(narrowed.total(), Some(FIELD_TOTAL));
    assert_eq!(narrowed.idle(), Some(FIELD_IDLE));

    // The whole policy written last replaces every field the setters wrote.
    let replaced = http::client()
        .request_timeout(FIELD_TOTAL)
        .response_idle_timeout(FIELD_IDLE)
        .response_budget(wholesale)
        .response_policy();
    assert_eq!(replaced, wholesale);

    // One store: neither single-field setter touches the other's dimension or
    // the ceiling.
    let only_total = http::client()
        .request_timeout(FIELD_TOTAL)
        .response_policy();
    assert_eq!(only_total.total(), Some(FIELD_TOTAL));
    assert_eq!(only_total.idle(), http::client().response_policy().idle());
    assert_eq!(
        only_total.max_bytes(),
        http::client().response_policy().max_bytes(),
    );
}

/// Serve one route that answers slowly and one that always refuses.
fn timing_routes(attempts: &Arc<AtomicU32>) -> Router {
    let mut router = Router::new();
    router.get("/paced", |_req: &Request| async {
        tokio::time::sleep(Duration::from_millis(300)).await;
        Response::text(200, "paced")
    });
    let counted = Arc::clone(attempts);
    router.get("/transient", move |_req: &Request| {
        counted.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    let unsafe_counted = Arc::clone(attempts);
    router.post("/transient", move |_req: &Request| {
        unsafe_counted.fetch_add(1, Ordering::Relaxed);
        async { Response::empty(503) }
    });
    router
}

/// The stored request total is the one a live call is measured against.
async fn stored_total_bounds_one_live_attempt(addr: SocketAddr) {
    let paced = format!("http://{addr}/paced");

    // The single-field write lands after the whole policy, so its short total
    // is the stored one.
    let refused: Result<ClientResponse, RuntimeError> = http::client()
        .response_budget(wholesale_policy())
        .request_timeout(Duration::from_millis(50))
        .get(&paced)
        .await;
    assert!(
        matches!(
            refused,
            Err(RuntimeError::Client(ClientError::Timeout(
                ClientTimeout::Attempt
            )))
        ),
        "the stored short total must end the paced call as its attempt, got {refused:?}",
    );

    // The same two writes in the other order keep the wholesale total, which
    // outlasts the same route.
    let admitted: ClientResponse = http::client()
        .request_timeout(Duration::from_millis(50))
        .response_budget(wholesale_policy())
        .get(&paced)
        .await
        .expect("the wholesale total outlasts the paced route");
    assert_eq!(admitted.body_bytes(), b"paced");
}

/// The quiet interval a live row measures, short enough that a peer which
/// stops sending is refused long before any total written beside it. Real
/// time crosses it.
pub(crate) const QUIET_IDLE: Duration = Duration::from_millis(60);

/// The longest a call bounded by [`QUIET_IDLE`] may take. Far below every
/// request total written beside it, so a call ended by a total instead of the
/// quiet interval fails here rather than passing late.
const IDLE_PROOF_BOUND: Duration = Duration::from_secs(1);

/// The request total the reverse-order quiet row is bounded by.
const QUIET_WHOLESALE_TOTAL: Duration = Duration::from_millis(300);

/// The quiet interval that row's wholesale policy carries, long enough that
/// only the total can end a call against a silent peer.
const QUIET_WHOLESALE_IDLE: Duration = Duration::from_secs(5);

/// The shortest that row may take if the wholesale policy replaced
/// [`QUIET_IDLE`]. Between the two deadlines, so either one ending the call
/// names itself.
const QUIET_ORDER_FLOOR: Duration = Duration::from_millis(150);

/// One upstream that answers with a partial body and then stops sending.
///
/// The head and one admitted frame arrive, the payload is never terminated,
/// and the connection stays open: the peer is quiet, not gone, which is the
/// only condition the quiet interval owns.
async fn quiet_upstream() -> ScriptedUpstream {
    ScriptedUpstream::start(
        CHUNKED_HEAD,
        vec![chunk_frame(ADMITTED)].into_boxed_slice(),
        true,
    )
    .await
}

/// An upstream that answers with [`ADMITTED`], exactly filling [`CEILING`],
/// and closes.
async fn admitted_upstream() -> ScriptedUpstream {
    ScriptedUpstream::start(
        CHUNKED_HEAD,
        vec![chunk_frame(ADMITTED), CHUNKED_END.into()].into_boxed_slice(),
        false,
    )
    .await
}

/// The one typed cause a crossed client deadline reports, naming the timer
/// that crossed it.
fn assert_client_timeout(
    row: &str,
    expected: ClientTimeout,
    result: Result<ClientResponse, RuntimeError>,
) {
    match result {
        Err(RuntimeError::Client(ClientError::Timeout(timer))) if timer == expected => {}
        Err(other) => panic!("{row}: expected the {expected} deadline, got {other:?}"),
        Ok(response) => panic!(
            "{row}: expected a refusal, got {} bytes",
            response.body_bytes().len()
        ),
    }
}

/// The stored quiet interval is what a silent peer is measured against.
///
/// The single-field write lands after the whole policy, so its short interval
/// is the stored one. The total it is written beside is twelve seconds: a
/// client that never handed the interval to its transport would sit on this
/// upstream until the row bound expired.
async fn stored_idle_bounds_one_quiet_peer() {
    let row = "quiet peer";
    let upstream = quiet_upstream().await;
    let quiet = format!("http://{}/quiet", upstream.addr);

    let started = Instant::now();
    let refused: Result<ClientResponse, RuntimeError> = within_bound(
        row,
        http::client()
            .response_budget(wholesale_policy())
            .response_idle_timeout(QUIET_IDLE)
            .get(&quiet),
    )
    .await;
    let elapsed = started.elapsed();

    assert_client_timeout(row, ClientTimeout::Transport, refused);
    assert!(
        elapsed < IDLE_PROOF_BOUND,
        "{row}: the quiet interval must end the call in under {IDLE_PROOF_BOUND:?}, took {elapsed:?}",
    );
    upstream.finish(row).await;
}

/// The whole policy written last replaces the quiet interval a setter wrote.
///
/// The same silent peer, and a wholesale policy whose interval outlasts this
/// row and whose total does not. A client still holding the replaced sixty
/// milliseconds would refuse far earlier than its total. The total is the
/// attempt's, so the attempt timer is the one that names the refusal.
async fn wholesale_idle_replaces_the_field_write() {
    let row = "quiet peer, wholesale last";
    let upstream = quiet_upstream().await;
    let quiet = format!("http://{}/quiet-replaced", upstream.addr);
    let replaced = TransferBudget::bounded(
        WHOLESALE_CEILING,
        QUIET_WHOLESALE_IDLE,
        QUIET_WHOLESALE_TOTAL,
    )
    .expect("a finite wholesale policy with a short total");

    let started = Instant::now();
    let refused: Result<ClientResponse, RuntimeError> = within_bound(
        row,
        http::client()
            .response_idle_timeout(QUIET_IDLE)
            .response_budget(replaced)
            .get(&quiet),
    )
    .await;
    let elapsed = started.elapsed();

    assert_client_timeout(row, ClientTimeout::Attempt, refused);
    assert!(
        elapsed >= QUIET_ORDER_FLOOR,
        "{row}: the replaced quiet interval must not end the call, took {elapsed:?}",
    );
    upstream.finish(row).await;
}

/// Retry eligibility, count, and backoff are unchanged by the response policy.
async fn retry_policy_survives_the_response_writes(addr: SocketAddr, attempts: &Arc<AtomicU32>) {
    let transient = format!("http://{addr}/transient");
    let backoff = Duration::from_millis(20);

    attempts.store(0, Ordering::Relaxed);
    let started = Instant::now();
    let refused: ClientResponse = http::client()
        .response_budget(wholesale_policy())
        .retries(2)
        .backoff(backoff)
        .get(&transient)
        .await
        .expect("a transient status is returned after its retries");
    assert_eq!(refused.status(), 503);
    assert_eq!(
        attempts.load(Ordering::Relaxed),
        3,
        "a safe method keeps its two configured retries",
    );
    assert!(
        started.elapsed() >= backoff,
        "the configured backoff still separates attempts",
    );

    attempts.store(0, Ordering::Relaxed);
    let unsafe_refused: ClientResponse = http::client()
        .response_budget(wholesale_policy())
        .retries(2)
        .backoff(backoff)
        .post(&transient, "body")
        .await
        .expect("an unsafe method answers on its first attempt");
    assert_eq!(unsafe_refused.status(), 503);
    assert_eq!(
        attempts.load(Ordering::Relaxed),
        1,
        "an unsafe method is still not retried by default",
    );

    attempts.store(0, Ordering::Relaxed);
    let replayable_client = http::client()
        .response_budget(wholesale_policy())
        .retries(2)
        .backoff(backoff);
    let replayable: ClientResponse = replayable_client
        .request("POST", &transient)
        .expect("a valid method and target")
        .body("body")
        .retry_policy(RetryPolicy::Replayable)
        .send()
        .await
        .expect("a replayable request keeps answering after its retries");
    assert_eq!(replayable.status(), 503);
    assert_eq!(
        attempts.load(Ordering::Relaxed),
        3,
        "the request's own replay permission is unchanged",
    );
}

/// 7.T3
#[camber::test]
async fn client_transfer_policy_writes_follow_authoritative_call_order() {
    call_order_decides_each_stored_dimension();

    stored_idle_bounds_one_quiet_peer().await;
    wholesale_idle_replaces_the_field_write().await;

    let attempts = Arc::new(AtomicU32::new(0));
    let addr = crate::runtime_support::spawn_server(timing_routes(&attempts));

    stored_total_bounds_one_live_attempt(addr).await;
    retry_policy_survives_the_response_writes(addr, &attempts).await;

    camber::runtime::request_shutdown();
}

/// The deadline every client dimension starts with.
const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);

/// The base backoff a client starts with.
const DEFAULT_BACKOFF: Duration = Duration::from_millis(100);

/// The shortest duration an infallible client setter holds.
const SETTER_FLOOR: Duration = Duration::from_millis(1);

/// The longest duration an infallible client setter holds: thirty years.
const SETTER_CEILING: Duration = Duration::from_secs(86_400 * 365 * 30);

/// A duration inside the setter range, which every setter holds as written.
const SETTER_INSIDE: Duration = Duration::from_secs(7);

/// The operation total the stored-policy rows write.
const FIELD_OPERATION: Duration = Duration::from_secs(15);

/// The operation total of the live expiry row: far below every row bound, so
/// a call no total ended fails at its row bound instead of passing late.
const LIVE_OPERATION: Duration = Duration::from_millis(100);

/// Require `client` to project the duration `field` as `expected`.
///
/// The operation total, the connect bound, and the backoff have no accessor.
/// The client's `Debug` projection is where a caller reads them.
fn assert_projected(row: &str, client: &Client, field: &str, expected: Duration) {
    let projected = format!("{client:?}");
    assert!(
        projected.contains(&format!("{field}: {expected:?}")),
        "{row}: {field} is not projected as {expected:?}: {projected}",
    );
}

/// A client starts with every documented bound.
fn defaults_hold_every_documented_bound() {
    let row = "client defaults";
    let client: Client = http::client();
    let policy = client.response_policy();

    assert_projected(row, &client, "total_timeout", DEFAULT_DEADLINE);
    assert_projected(row, &client, "connect_timeout", DEFAULT_DEADLINE);
    assert_projected(row, &client, "backoff", DEFAULT_BACKOFF);
    assert_eq!(policy.total(), Some(DEFAULT_DEADLINE), "{row}: attempt");
    assert_eq!(
        policy.idle(),
        Some(DEFAULT_DEADLINE),
        "{row}: quiet interval"
    );
    assert_eq!(policy.max_bytes(), Some(DEFAULT_CEILING), "{row}: ceiling");
}

/// Where one duration setter's stored value is read back.
#[derive(Clone, Copy)]
enum Stored {
    /// In the client's `Debug` projection, under the setter's own name.
    Projected,
    /// In one dimension of the stored response policy.
    Policy(fn(&TransferBudget) -> Option<Duration>),
}

/// One infallible duration setter of the client.
struct Setter {
    name: &'static str,
    write: fn(Client, Duration) -> Client,
    stored: Stored,
}

/// Every infallible duration setter the client has.
const SETTERS: [Setter; 5] = [
    Setter {
        name: "total_timeout",
        write: |client, value| client.total_timeout(value),
        stored: Stored::Projected,
    },
    Setter {
        name: "connect_timeout",
        write: |client, value| client.connect_timeout(value),
        stored: Stored::Projected,
    },
    Setter {
        name: "backoff",
        write: |client, value| client.backoff(value),
        stored: Stored::Projected,
    },
    Setter {
        name: "request_timeout",
        write: |client, value| client.request_timeout(value),
        stored: Stored::Policy(TransferBudget::total),
    },
    Setter {
        name: "response_idle_timeout",
        write: |client, value| client.response_idle_timeout(value),
        stored: Stored::Policy(TransferBudget::idle),
    },
];

/// What each written duration is stored as: raised to the floor, lowered to
/// the ceiling, or kept.
const CLAMPS: [(Duration, Duration); 4] = [
    (Duration::ZERO, SETTER_FLOOR),
    (Duration::from_micros(500), SETTER_FLOOR),
    (Duration::MAX, SETTER_CEILING),
    (SETTER_INSIDE, SETTER_INSIDE),
];

/// Require `setter` to store `written` as `expected`.
fn assert_stored(setter: &Setter, written: Duration, expected: Duration) {
    let row = format!("{} written {written:?}", setter.name);
    let client: Client = (setter.write)(http::client(), written);
    match setter.stored {
        Stored::Projected => assert_projected(&row, &client, setter.name, expected),
        Stored::Policy(read) => assert_eq!(
            read(&client.response_policy()),
            Some(expected),
            "{row}: the stored policy holds another value",
        ),
    }
}

/// Every duration setter holds a written value inside one millisecond and
/// thirty years.
fn every_duration_setter_clamps_to_the_policy_range() {
    for setter in &SETTERS {
        for (written, expected) in CLAMPS {
            assert_stored(setter, written, expected);
        }
    }
}

/// The operation total is not a dimension of the response policy.
///
/// A write to either leaves the other as it was, in both orders. The named
/// opt-out removes the byte ceiling only. A wholly unbounded policy removes
/// the attempt and quiet bounds too, and the operation total stays finite.
fn operation_total_is_outside_the_response_policy() {
    let wholesale = wholesale_policy();

    let budget_last: Client = http::client()
        .total_timeout(FIELD_OPERATION)
        .response_budget(wholesale);
    assert_projected(
        "budget last",
        &budget_last,
        "total_timeout",
        FIELD_OPERATION,
    );
    assert_eq!(budget_last.response_policy(), wholesale, "budget last");

    let total_last: Client = http::client()
        .response_budget(wholesale)
        .total_timeout(FIELD_OPERATION);
    assert_projected("total last", &total_last, "total_timeout", FIELD_OPERATION);
    assert_eq!(total_last.response_policy(), wholesale, "total last");

    let opted_out: Client = http::client()
        .total_timeout(FIELD_OPERATION)
        .response_budget(wholesale)
        .unbounded_response();
    let byte_only = opted_out.response_policy();
    assert_projected("opt-out", &opted_out, "total_timeout", FIELD_OPERATION);
    assert_eq!(byte_only.max_bytes(), None, "opt-out: ceiling");
    assert_eq!(
        byte_only.idle(),
        Some(WHOLESALE_IDLE),
        "opt-out: quiet interval"
    );
    assert_eq!(byte_only.total(), Some(WHOLESALE_TOTAL), "opt-out: attempt");

    let unbounded: Client = http::client()
        .total_timeout(FIELD_OPERATION)
        .response_budget(TransferBudget::unbounded());
    assert_projected("unbounded", &unbounded, "total_timeout", FIELD_OPERATION);
    assert_eq!(
        unbounded.response_policy(),
        TransferBudget::unbounded(),
        "unbounded",
    );

    let defaulted: Client = http::client().response_budget(TransferBudget::unbounded());
    assert_projected(
        "unbounded, default total",
        &defaulted,
        "total_timeout",
        DEFAULT_DEADLINE,
    );
}

/// A wholly unbounded response policy leaves the operation total in force.
///
/// The peer sends a partial body and goes quiet. No attempt bound, quiet
/// interval, or ceiling is left to end the call, so only the operation total
/// can, and it names itself.
async fn unbounded_policy_keeps_the_operation_total() {
    let row = "unbounded policy, live total";
    let upstream = quiet_upstream().await;
    let quiet = format!("http://{}/unbounded", upstream.addr);

    let refused: Result<ClientResponse, RuntimeError> = within_bound(
        row,
        http::client()
            .total_timeout(LIVE_OPERATION)
            .response_budget(TransferBudget::unbounded())
            .get(&quiet),
    )
    .await;

    assert_client_timeout(row, ClientTimeout::Operation, refused);
    upstream.finish(row).await;
}

/// An upstream that answers with [`ABOVE_CEILING`] and closes.
async fn above_ceiling_upstream() -> ScriptedUpstream {
    ScriptedUpstream::start(
        CHUNKED_HEAD,
        vec![chunk_frame(ABOVE_CEILING), CHUNKED_END.into()].into_boxed_slice(),
        false,
    )
    .await
}

/// Send one GET through `client` with its own response ceiling.
async fn get_under_limit(
    client: &Client,
    url: &str,
    limit: usize,
) -> Result<ClientResponse, RuntimeError> {
    client
        .request("GET", url)?
        .response_limit(limit)?
        .send()
        .await
}

/// A request's own ceiling replaces the client's for that request only.
///
/// The same client then refuses the same body under the ceiling it stored,
/// and its stored policy is unchanged.
async fn request_limit_overrides_one_request() {
    let row = "request response limit";
    let client: Client = bounded_client().total_timeout(WHOLESALE_TOTAL);

    let admitting = above_ceiling_upstream().await;
    let admitted: Result<ClientResponse, RuntimeError> = within_bound(
        row,
        get_under_limit(
            &client,
            &format!("http://{}/override", admitting.addr),
            ABOVE_CEILING.len(),
        ),
    )
    .await;
    admitting.finish(row).await;

    let refusing = above_ceiling_upstream().await;
    let refused: Result<ClientResponse, RuntimeError> =
        within_bound(row, client.get(&format!("http://{}/stored", refusing.addr))).await;
    refusing.finish(row).await;

    let response: ClientResponse =
        admitted.expect("the request's own ceiling admits a body above the client's");
    assert_eq!(
        response.body_bytes(),
        ABOVE_CEILING,
        "{row}: the admitted body must arrive whole",
    );
    assert_client_limit(row, CEILING, refused);
    assert_eq!(
        client.response_policy().max_bytes(),
        Some(CEILING),
        "{row}: the request's ceiling reached the client's stored policy",
    );
}

/// One client that has sent once, so its transport is built and cached under
/// a quiet interval of eleven seconds.
async fn used_client(row: &str) -> Client {
    let answering = admitted_upstream().await;
    let client: Client = http::client()
        .total_timeout(WHOLESALE_TOTAL)
        .response_budget(wholesale_policy());
    let first: Result<ClientResponse, RuntimeError> =
        within_bound(row, client.get(&format!("http://{}/first", answering.addr))).await;
    answering.finish(row).await;

    let answered: ClientResponse = first.expect("the first call builds the transport and answers");
    assert_eq!(
        answered.body_bytes(),
        ADMITTED,
        "{row}: the first answer must arrive whole",
    );
    client
}

/// One write of a transport setting to a client.
struct TransportWrite {
    name: &'static str,
    write: fn(Client) -> Client,
    /// Whether a quiet peer can measure the new value. Such a write shortens
    /// the quiet interval. No local peer can hold a dial open for a connect
    /// bound.
    measured_by_quiet_peer: bool,
}

/// The whole policy a used client is rewritten with: the same short quiet
/// interval the single-field write carries, under a total that outlasts the
/// row.
fn quiet_wholesale_policy() -> TransferBudget {
    TransferBudget::bounded(WHOLESALE_CEILING, QUIET_IDLE, WHOLESALE_TOTAL)
        .expect("a finite wholesale policy with a short quiet interval")
}

/// Every setter that writes a transport setting.
const TRANSPORT_WRITES: [TransportWrite; 3] = [
    TransportWrite {
        name: "connect_timeout",
        write: |client| client.connect_timeout(SETTER_INSIDE),
        measured_by_quiet_peer: false,
    },
    TransportWrite {
        name: "response_idle_timeout",
        write: |client| client.response_idle_timeout(QUIET_IDLE),
        measured_by_quiet_peer: true,
    },
    TransportWrite {
        name: "response_budget",
        write: |client| client.response_budget(quiet_wholesale_policy()),
        measured_by_quiet_peer: true,
    },
];

/// A transport setting written to a used client discards the transport that
/// client built.
///
/// `client_transport_is_built(&client)` is the read-only view of the cache. A
/// client is built from its settings on first use, so one that kept its
/// transport across the write would send under the settings it replaced. The
/// connect bound has no other proof: the row below cannot hold a dial.
async fn transport_write_discards_the_built_transport(write: &TransportWrite) {
    let row = format!("discarded transport after {}", write.name);
    let unused = client_transport_is_built(&http::client());
    let client: Client = used_client(&row).await;
    let used = client_transport_is_built(&client);

    let client: Client = (write.write)(client);

    assert!(!unused, "{row}: a client that never sent holds a transport");
    assert!(used, "{row}: the first call left no built transport");
    assert!(
        !client_transport_is_built(&client),
        "{row}: the write kept the transport built before it",
    );
}

/// A transport setting written to a used client is the one its next call
/// runs under.
///
/// The first call builds and caches the transport under a quiet interval of
/// eleven seconds. The write then leaves sixty milliseconds. A client still
/// sending through the cached transport would sit on the quiet peer until the
/// row bound expired.
async fn reconfigured_transport_is_the_one_used(write: &TransportWrite) {
    let row = format!("reconfigured transport after {}", write.name);
    let client: Client = (write.write)(used_client(&row).await);
    let quiet = quiet_upstream().await;
    let refused: Result<ClientResponse, RuntimeError> =
        within_bound(&row, client.get(&format!("http://{}/second", quiet.addr))).await;
    quiet.finish(&row).await;

    assert_client_timeout(&row, ClientTimeout::Transport, refused);
}

/// Every transport write to a used client discards its transport, and the
/// next call runs under each value a quiet peer can measure.
async fn transport_writes_reach_a_used_client() {
    for write in &TRANSPORT_WRITES {
        transport_write_discards_the_built_transport(write).await;
    }
    for write in TRANSPORT_WRITES
        .iter()
        .filter(|write| write.measured_by_quiet_peer)
    {
        reconfigured_transport_is_the_one_used(write).await;
    }
}

/// Outbound 4.T3
#[camber::test]
async fn client_budget_writes_preserve_outer_lifetime() {
    defaults_hold_every_documented_bound();
    every_duration_setter_clamps_to_the_policy_range();
    operation_total_is_outside_the_response_policy();

    unbounded_policy_keeps_the_operation_total().await;
    request_limit_overrides_one_request().await;
    transport_writes_reach_a_used_client().await;
}
