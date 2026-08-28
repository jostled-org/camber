//! 7.T1 and 7.T3: the outbound client collects under one checked ceiling and
//! one stored response policy.

use camber::RuntimeError;
use camber::http::mock::{self, ScopedTransferOwner};
use camber::http::{self, ByteBoundary, Request, Response, Router, TransferBudget};
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
const CEILING: usize = 16;

/// The bytes one admitted frame carries, exactly filling [`CEILING`].
const ADMITTED: &[u8] = b"0123456789abcdef";

/// The bytes of the frame that crosses [`CEILING`].
const CROSSING: &[u8] = b"crossing";

/// What ends a chunked payload.
const CHUNKED_END: &[u8] = b"0\r\n\r\n";

/// A chunked head, which states no length at all.
const CHUNKED_HEAD: &str = "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n";

/// One client whose response ceiling is `CEILING` and whose deadlines are long
/// enough that no row can reach them.
fn bounded_client() -> http::ClientBuilder {
    http::client().response_budget(
        TransferBudget::bounded(CEILING, Duration::from_secs(30), Duration::from_secs(30))
            .expect("a finite client response budget"),
    )
}

/// The bytes of one chunked-transfer frame carrying `payload`.
fn chunk_frame(payload: &[u8]) -> Box<[u8]> {
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
struct ScriptedUpstream {
    addr: SocketAddr,
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
    async fn start(head: &'static str, body: Box<[Box<[u8]>]>, hold: bool) -> Self {
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
    async fn finish(mut self, row: &str) {
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

/// Watch one upstream's address, so a row reads what the production collector
/// published while it read that upstream's answer.
fn watch(addr: SocketAddr) -> ScopedTransferOwner {
    mock::transfer_owner(addr).expect("one transfer-owner controller for this upstream")
}

/// Run one client call under the row bound, so a poll after a terminal fails
/// the row instead of parking it.
async fn within_bound<T>(row: &str, call: impl std::future::Future<Output = T>) -> T {
    tokio::time::timeout(ROW_BOUND, call)
        .await
        .unwrap_or_else(|_| panic!("{row}: the client call never returned"))
}

/// The one typed cause a crossed client ceiling reports.
fn assert_client_limit(row: &str, result: Result<Response, RuntimeError>) {
    match result {
        Err(RuntimeError::LimitExceeded(ByteBoundary::ClientResponse)) => {}
        Err(other) => panic!("{row}: expected the client response ceiling, got {other:?}"),
        Ok(response) => panic!(
            "{row}: expected a refusal, got {} bytes",
            response.body().len()
        ),
    }
}

/// A declaration above the ceiling is refused before one frame is polled.
async fn trustworthy_declaration_is_refused_before_a_frame_is_polled() {
    let row = "declared oversize";
    let upstream = ScriptedUpstream::start(
        "HTTP/1.1 200 OK\r\nContent-Length: 64\r\nConnection: close\r\n\r\n",
        vec![vec![b'x'; 64].into_boxed_slice()].into_boxed_slice(),
        false,
    )
    .await;
    let observed = watch(upstream.addr);

    let result = within_bound(
        row,
        bounded_client().get(&format!("http://{}/declared", upstream.addr)),
    )
    .await;

    assert_client_limit(row, result);
    assert_eq!(
        observed.collected_chunks_polled(),
        0,
        "{row}: a trustworthy declaration must be refused before any frame is read",
    );
    assert_eq!(
        observed.collected_peak_retained_bytes(),
        0,
        "{row}: nothing may be retained for a refused declaration",
    );
    upstream.finish(row).await;
}

/// The frame that crosses the ceiling is dropped, and nothing is read after it.
async fn crossing_frame_is_dropped_and_ends_the_read() {
    let row = "crossing frame";
    let upstream = ScriptedUpstream::start(
        CHUNKED_HEAD,
        vec![chunk_frame(ADMITTED), chunk_frame(CROSSING)].into_boxed_slice(),
        true,
    )
    .await;
    let observed = watch(upstream.addr);

    // The upstream never terminates its payload. A collector that polled again
    // after fixing its terminal would wait here until the row bound expired.
    let result = within_bound(
        row,
        bounded_client().get(&format!("http://{}/crossing", upstream.addr)),
    )
    .await;

    assert_client_limit(row, result);
    assert!(
        observed.collected_chunks_polled() > 0,
        "{row}: the crossing frame must have been read before it was refused",
    );
    // The production collector publishes what it holds after every accounting
    // decision, refusals included, so this reads the buffer rather than the
    // total the collector was permitted. A collector that appended the
    // crossing frame and only then refused would report those bytes here.
    assert_eq!(
        observed.collected_peak_retained_bytes(),
        CEILING,
        "{row}: the admitted frame must be kept whole and the crossing frame must add nothing",
    );
    upstream.finish(row).await;
}

/// A frame that exactly fills the ceiling is retained whole.
async fn exact_boundary_frame_is_retained_whole() {
    let row = "exact boundary";
    let upstream = ScriptedUpstream::start(
        CHUNKED_HEAD,
        vec![chunk_frame(ADMITTED), CHUNKED_END.into()].into_boxed_slice(),
        false,
    )
    .await;
    let observed = watch(upstream.addr);

    let response = within_bound(
        row,
        bounded_client().get(&format!("http://{}/exact", upstream.addr)),
    )
    .await
    .expect("a body that exactly fills its ceiling is admitted");

    assert_eq!(response.status(), 200, "{row}: unexpected status");
    assert_eq!(
        response.body().as_bytes(),
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
    let observed = watch(upstream.addr);

    let response = within_bound(
        row,
        bounded_client().get(&format!("http://{}/trailers", upstream.addr)),
    )
    .await
    .expect("a trailered body at its ceiling is admitted");

    assert_eq!(
        response.body().as_bytes(),
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

/// A head declaring nine MiB: one above the eight-MiB default a caller who
/// wrote no ceiling of their own collects under. The peer never sends them.
const ABOVE_DEFAULT_HEAD: &str =
    "HTTP/1.1 200 OK\r\nContent-Length: 9437184\r\nConnection: close\r\n\r\n";

/// The named opt-out removes the ceiling and nothing else.
async fn named_opt_out_admits_a_body_above_the_ceiling() {
    let row = "named opt-out";
    let upstream = ScriptedUpstream::start(
        CHUNKED_HEAD,
        vec![chunk_frame(ABOVE_CEILING), CHUNKED_END.into()].into_boxed_slice(),
        false,
    )
    .await;
    let observed = watch(upstream.addr);

    let response = within_bound(
        row,
        bounded_client()
            .unbounded_response()
            .get(&format!("http://{}/opt-out", upstream.addr)),
    )
    .await
    .expect("the named opt-out admits a body above the configured ceiling");

    assert_eq!(
        response.body().as_bytes(),
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
    let observed = watch(upstream.addr);

    let result = within_bound(row, http::get(&format!("http://{}/default", upstream.addr))).await;

    assert_client_limit(row, result);
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
    let refused = http::client()
        .response_budget(wholesale_policy())
        .request_timeout(Duration::from_millis(50))
        .get(&paced)
        .await;
    assert!(
        matches!(refused, Err(RuntimeError::Timeout)),
        "the stored short total must end the paced call, got {refused:?}",
    );

    // The same two writes in the other order keep the wholesale total, which
    // outlasts the same route.
    let admitted = http::client()
        .request_timeout(Duration::from_millis(50))
        .response_budget(wholesale_policy())
        .get(&paced)
        .await
        .expect("the wholesale total outlasts the paced route");
    assert_eq!(admitted.body(), "paced");
}

/// The quiet interval one live row measures, short enough that a peer which
/// stops sending is refused long before any total in this file.
const QUIET_IDLE: Duration = Duration::from_millis(60);

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

/// The one typed cause a crossed client deadline reports.
fn assert_client_timeout(row: &str, result: Result<Response, RuntimeError>) {
    match result {
        Err(RuntimeError::Timeout) => {}
        Err(other) => panic!("{row}: expected a client deadline, got {other:?}"),
        Ok(response) => panic!(
            "{row}: expected a refusal, got {} bytes",
            response.body().len()
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
    let refused = within_bound(
        row,
        http::client()
            .response_budget(wholesale_policy())
            .response_idle_timeout(QUIET_IDLE)
            .get(&quiet),
    )
    .await;
    let elapsed = started.elapsed();

    assert_client_timeout(row, refused);
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
/// milliseconds would refuse far earlier than its total.
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
    let refused = within_bound(
        row,
        http::client()
            .response_idle_timeout(QUIET_IDLE)
            .response_budget(replaced)
            .get(&quiet),
    )
    .await;
    let elapsed = started.elapsed();

    assert_client_timeout(row, refused);
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
    let refused = http::client()
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
    let unsafe_refused = http::client()
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
    let opted_in = http::client()
        .response_budget(wholesale_policy())
        .retries(2)
        .backoff(backoff)
        .retry_unsafe_methods(true)
        .post(&transient, "body")
        .await
        .expect("the opt-in keeps answering after its retries");
    assert_eq!(opted_in.status(), 503);
    assert_eq!(
        attempts.load(Ordering::Relaxed),
        3,
        "the unsafe-method opt-in is unchanged",
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
