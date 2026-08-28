use std::future::{Future, IntoFuture};
use std::io::Write;
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camber::RuntimeError;
use camber::http::mock::{
    self as mock, ConnectionFault, ConnectionOwnerController, ConnectionOwnerEdge,
    ScopedFaultedSelection, ScopedServerStop, ScopedSupervisorSelection, ServerStopController,
    ServerStopEdge, ServerTaskFault, SupervisorJoinProbe,
    supervisor_join_probe,
};
#[cfg(feature = "ws")]
use camber::http::mock::{ScopedSupervisedRegistration, UpgradeOwnerController, UpgradeOwnerEdge};
use camber::http::{Request, Response, Router, ServerHandle, ServerHandleFuture, SseWriter};
use camber::runtime;
use camber::runtime_test_support;
use futures_util::FutureExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

use crate::common::{OwnerPoint, Owns};

const HTTP_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
const CLOSE_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
const PROBE_PANIC: &str = "supervisor join probe panic";
const OWNED_TASK_PANIC: &str = "injected owned HTTP task panic";
const OPAQUE_PANIC_PATH: &str = "/opaque-panic";
const OPAQUE_PANIC_PAYLOAD: usize = 7;
#[cfg(feature = "ws")]
const SUPERVISOR_PANIC: &str = "injected server supervisor panic";
// Every observation in this file carries this bound, so an event production
// never reaches fails the case instead of parking the binary on it.
const OBSERVATION_DEADLINE: Duration = Duration::from_secs(5);

fn ok_router() -> Router {
    let mut router = Router::new();
    router.get("/", |_req: &Request| async { Response::text(200, "ok") });
    router
}

fn counting_router(counter: Arc<AtomicUsize>) -> Router {
    let mut router = Router::new();
    router.get("/", move |_req: &Request| {
        counter.fetch_add(1, Ordering::AcqRel);
        async { Response::text(200, "ok") }
    });
    router
}

struct RequestDropProbe(Arc<AtomicBool>);

impl Drop for RequestDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

// The dispatch drop reports itself: the probe rides inside the router the
// supervisor owns, so its Drop is the exact moment the supervisor released the
// dispatch. A signal rather than a flag keeps the observer off a poll.
struct DispatchDropProbe(Option<tokio::sync::oneshot::Sender<()>>);

impl Drop for DispatchDropProbe {
    fn drop(&mut self) {
        if let Some(sender) = self.0.take() {
            let _ = sender.send(());
        }
    }
}

fn dispatch_drop_router(dropped: tokio::sync::oneshot::Sender<()>) -> Router {
    let probe = DispatchDropProbe(Some(dropped));
    let mut router = Router::new();
    router.get("/", move |_req: &Request| {
        let _ = &probe;
        async { Response::text(200, "ok") }
    });
    router
}

/// The policy every fixture that holds a request open serves under.
///
/// The request budget is explicitly unbounded because these rows are about the
/// supervisor's own deadline: a held request that its own request total ended
/// would settle the claim before the boundary under test could reach it. Under
/// paused time that is not a remote possibility — the clock advances to the
/// next timer whenever the runtime idles, which is exactly while these fixtures
/// wait on the socket I/O their observations are built on.
fn held_request_policy() -> camber::http::ServerPolicy {
    camber::http::ServerPolicy::default().request_budget(camber::http::RequestBudget::unbounded())
}

/// Serve a held-request fixture in the background under that policy.
fn serve_held(listener: tokio::net::TcpListener, router: Router) -> ServerHandle {
    camber::http::server(router)
        .policy(held_request_policy())
        .serve_background(listener)
        .expect("owned server requires a Tokio runtime")
}

/// The same, over TLS.
fn serve_held_tls(
    listener: tokio::net::TcpListener,
    router: Router,
    tls: Arc<rustls::ServerConfig>,
) -> ServerHandle {
    camber::http::server(router)
        .policy(held_request_policy())
        .tls(tls)
        .serve_background(listener)
        .expect("owned server requires a Tokio runtime")
}

fn held_router(
    entered: tokio::sync::oneshot::Sender<()>,
    release: Arc<tokio::sync::Semaphore>,
    dropped: Arc<AtomicBool>,
) -> Router {
    let entered = Arc::new(Mutex::new(Some(entered)));
    let mut router = Router::new();
    router.get("/", move |_req: &Request| {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let probe = RequestDropProbe(Arc::clone(&dropped));
        async move {
            if let Some(sender) = entered.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = sender.send(());
            }
            let _probe = probe;
            let permit = release.acquire().await;
            drop(permit);
            Response::text(200, "released")
        }
    });
    router
}

/// Add a held route whose release unwinds the request that entered it.
///
/// The unwind carries out through the connection future, so the supervisor
/// joins exactly what an opaque owned-task fault gives it: a panic whose
/// payload no reader can name. Unlike that fault, which is spent when the task
/// is spawned, this one happens when the holder of the release says so.
fn add_opaque_panic_route(
    router: &mut Router,
    entered: tokio::sync::oneshot::Sender<()>,
    release: Arc<tokio::sync::Semaphore>,
) {
    let entered = Arc::new(Mutex::new(Some(entered)));
    router.get(OPAQUE_PANIC_PATH, move |_req: &Request| {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        async move {
            if let Some(sender) = entered.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = sender.send(());
            }
            let permit = release.acquire().await;
            drop(permit);
            opaque_handler_panic()
        }
    });
}

/// Unwind one held request with a payload that is neither `&str` nor `String`.
///
/// `resume_unwind` rather than `panic!`, for the same reason production's
/// injected faults use it: it runs no panic hook, so a case that expects the
/// unwind does not print one.
fn opaque_handler_panic() -> Result<Response, RuntimeError> {
    std::panic::resume_unwind(Box::new(OPAQUE_PANIC_PAYLOAD))
}

fn named_held_router(
    entered: tokio::sync::oneshot::Sender<()>,
    release: Arc<tokio::sync::Semaphore>,
    dropped: Arc<AtomicBool>,
    next_requests: Arc<AtomicUsize>,
) -> Router {
    let entered = Arc::new(Mutex::new(Some(entered)));
    let mut router = Router::new();
    router.get("/active", move |_req: &Request| {
        let entered = Arc::clone(&entered);
        let release = Arc::clone(&release);
        let probe = RequestDropProbe(Arc::clone(&dropped));
        async move {
            if let Some(sender) = entered.lock().unwrap_or_else(|e| e.into_inner()).take() {
                let _ = sender.send(());
            }
            let _probe = probe;
            let permit = release.acquire().await;
            drop(permit);
            Response::text(200, "active")
        }
    });
    router.get("/next", move |_req: &Request| {
        next_requests.fetch_add(1, Ordering::AcqRel);
        async { Response::text(200, "next") }
    });
    router.get("/ready", |_req: &Request| async {
        Response::text(200, "ready")
    });
    router
}

// Paused Tokio time auto-advances whenever the runtime goes idle, and the
// runtime goes idle exactly while it waits on the real socket I/O these
// observations are waiting for — a `tokio::time::timeout` would expire before
// the I/O it bounds could ever land. So the bound comes off the real clock,
// leaving the paused clock free for the tests that advance it deliberately.
async fn bounded<F>(observation: F, context: &str) -> F::Output
where
    F: Future,
{
    let (expired, expiry) = tokio::sync::oneshot::channel();
    let _deadline = ObservationDeadline::start(expired);
    tokio::select! {
        observed = observation => observed,
        _ = expiry => panic!("{context}"),
    }
}

// Dropping the handle cancels the deadline, so a met observation leaves no
// thread sleeping out the remainder of the bound.
struct ObservationDeadline {
    _cancel: std::sync::mpsc::Sender<()>,
}

impl ObservationDeadline {
    fn start(expired: tokio::sync::oneshot::Sender<()>) -> Self {
        let (cancel, cancelled) = std::sync::mpsc::channel();
        std::thread::Builder::new()
            .name("observation-deadline".to_owned())
            .spawn(move || {
                if matches!(
                    cancelled.recv_timeout(OBSERVATION_DEADLINE),
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout)
                ) {
                    let _ = expired.send(());
                }
            })
            .unwrap();
        Self { _cancel: cancel }
    }
}

// Handler entry is fixture readiness for every held-request case, so it carries
// the same bound as the observations that follow it.
async fn await_handler_entry(entered: tokio::sync::oneshot::Receiver<()>, context: &str) {
    bounded(entered, context)
        .await
        .unwrap_or_else(|_| panic!("{context}: the handler dropped its entry signal"));
}

async fn connect_request_path(addr: SocketAddr, path: &str) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

async fn write_request(stream: &mut tokio::net::TcpStream) {
    stream.write_all(HTTP_REQUEST).await.unwrap();
}

// The deadline lives here rather than at the call site, so no reader in this
// file can be unbounded. `read_http_head_bounded` only names the site.
async fn read_http_head<S>(stream: &mut S) -> Box<str>
where
    S: AsyncRead + Unpin,
{
    read_http_head_bounded(stream, "HTTP response head timed out").await
}

async fn read_http_head_bounded<S>(stream: &mut S, context: &str) -> Box<str>
where
    S: AsyncRead + Unpin,
{
    bounded(read_http_head_unbounded(stream), context).await
}

async fn read_http_head_unbounded<S>(stream: &mut S) -> Box<str>
where
    S: AsyncRead + Unpin,
{
    crate::common::read_async_head_unbounded(stream, "the HTTP response head").await
}

async fn assert_eof<S>(stream: &mut S)
where
    S: AsyncRead + Unpin,
{
    let mut byte = [0u8; 1];
    let read = bounded(stream.read(&mut byte), "timed out waiting for peer EOF")
        .await
        .expect("failed while waiting for peer EOF");
    assert_eq!(read, 0, "expected EOF, received transport data");
}

fn assert_eof_with_socket_deadline(stream: tokio::net::TcpStream) {
    let stream = stream.into_std().unwrap();
    let read = peek_with_socket_deadline(stream).expect("failed while waiting for peer EOF");
    assert_eq!(read, 0, "expected EOF, received transport data");
}

// The same claim for a case whose bound must stay off the paused Tokio clock.
//
// The ordinary form is `common::assert_refusal_body_then_eof`, shared with the
// proxy and WebSocket roots. It bounds on the Tokio clock, which the paused-time
// case has stopped, so that case observes on a thread of its own instead. What
// the body must be is `common::assert_refusal_body`'s claim in both forms, so
// the two readers cannot come to disagree about what a refused peer is owed.
#[cfg(feature = "ws")]
async fn assert_refusal_body_then_eof_with_independent_deadline(
    stream: tokio::net::TcpStream,
    expected: &'static str,
) {
    tokio::task::spawn_blocking(move || {
        let stream = stream.into_std().unwrap();
        let rest = read_rest_with_socket_deadline(stream)
            .expect("failed while reading the refused upgrade body");
        assert_refusal_body(&rest, expected, "the refused upgrade");
    })
    .await
    .expect("socket refusal-body observer panicked");
}

// Everything one peer is owed for an upgrade the supervisor would not own.
//
// Six cases in this binary claim it: the mapped `503`, no commitment anywhere in
// the head, the transport-owned close, and the client-safe body through the
// peer's end of stream. Six copies were six places those four claims could come
// apart, and four of them had already lost one or two — one read no body at all,
// so it could not say what the refused peer was told.
//
// `subject` names the case, so an expired bound or a failed comparison reports
// which refusal it was reading.
#[cfg(feature = "ws")]
async fn assert_refused_upgrade_wire(client: &mut tokio::net::TcpStream, subject: &str) {
    let response =
        read_http_head_bounded(client, &format!("{subject}: the refusal head timed out")).await;
    assert!(
        response.starts_with("HTTP/1.1 503"),
        "{subject}: answers with the mapped status: {response}"
    );
    // Searched rather than tested at the head's start: the line above has just
    // required a `503` there, so a `starts_with` for `101` could not have failed
    // whatever the server did. The claim is that no commitment reached this peer
    // at all, which is a claim about the whole transport it was sent.
    assert!(
        !response.contains("HTTP/1.1 101"),
        "{subject}: a refused registration never commits its upgrade: {response}"
    );
    assert!(
        response.to_ascii_lowercase().contains("connection: close"),
        "{subject}: the transport-owned close survives mapping: {response}"
    );
    // Read to EOF rather than peeking for it: the mapped refusal carries a body,
    // so the peer's end-of-transport is the end of that body — and reading it is
    // what proves the body is the safe message and nothing else.
    common::assert_refusal_body_then_eof(client, UNAVAILABLE_BODY, subject).await;
}

async fn assert_connection_closed_with_independent_deadline(stream: tokio::net::TcpStream) {
    tokio::task::spawn_blocking(move || assert_connection_closed_with_socket_deadline(stream))
        .await
        .expect("socket closure observer panicked");
}

fn assert_connection_closed_with_socket_deadline(stream: tokio::net::TcpStream) {
    let stream = stream.into_std().unwrap();
    assert_blocking_connection_closed(stream);
}

// This observer must stay independent of paused Tokio time, so the bound runs
// against the real clock. It bounds the OBSERVATION rather than the socket: a
// peer-reset socket still answers `peek` at once, but macOS refuses to accept
// SO_RCVTIMEO on one, and a socket bound that cannot be armed would park the
// binary on the very case it exists to report.
//
// The socket is shared with the observer rather than moved into it, because a
// bound that expires leaves a thread parked in `peek` on a socket nothing else
// could reach. Shutting it down from here ends that read, so the observer is
// joinable on both paths and teardown owns neither the thread nor the socket.
fn peek_with_socket_deadline(stream: std::net::TcpStream) -> std::io::Result<usize> {
    observe_with_socket_deadline(stream, peek_blocking)
}

// The same independent observation, for a peer whose answer has a body: the
// remaining bytes through the close, rather than the first one.
fn read_rest_with_socket_deadline(stream: std::net::TcpStream) -> std::io::Result<Vec<u8>> {
    observe_with_socket_deadline(stream, read_rest_blocking)
}

// The observer both blocking observations run under, written once. The two
// differ only in what they ask of the socket; the thread, the bound, the
// shutdown that ends a parked read, and the join are the same either way.
fn observe_with_socket_deadline<T>(
    stream: std::net::TcpStream,
    observe: fn(&std::net::TcpStream) -> std::io::Result<T>,
) -> std::io::Result<T>
where
    T: Send + 'static,
{
    let stream = Arc::new(stream);
    let observed_stream = Arc::clone(&stream);
    let (observed, observation) = std::sync::mpsc::sync_channel(1);
    let mut observer = Some(
        std::thread::Builder::new()
            .name("peer-closure-observer".to_owned())
            .spawn(move || {
                let _ = observed.send(observe(&observed_stream));
            })
            .unwrap(),
    );
    let read = observation
        .recv_timeout(OBSERVATION_DEADLINE)
        .unwrap_or_else(|_| {
            let _ = stream.shutdown(std::net::Shutdown::Both);
            Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "timed out waiting for peer closure",
            ))
        });
    common::join_thread_bounded(&mut observer, OBSERVATION_DEADLINE)
        .expect("the peer-closure observer did not exit after its socket was shut down");
    read
}

// A Tokio stream converted back to std is still non-blocking, so restore
// blocking mode and let the observer's own deadline carry the bound.
fn peek_blocking(stream: &std::net::TcpStream) -> std::io::Result<usize> {
    stream.set_nonblocking(false)?;
    let mut byte = [0u8; 1];
    stream.peek(&mut byte)
}

// Everything the peer still owes, through its close.
fn read_rest_blocking(stream: &std::net::TcpStream) -> std::io::Result<Vec<u8>> {
    use std::io::Read;

    stream.set_nonblocking(false)?;
    let mut rest = Vec::new();
    (&mut &*stream).read_to_end(&mut rest)?;
    Ok(rest)
}

fn assert_blocking_connection_closed(stream: std::net::TcpStream) {
    match peek_with_socket_deadline(stream) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        Ok(read) => panic!("expected peer closure, received {read} byte(s)"),
        Err(error) => panic!("failed while waiting for peer closure: {error}"),
    }
}

async fn assert_http_body<S>(stream: &mut S, expected: &[u8])
where
    S: AsyncRead + Unpin,
{
    assert_http_body_bounded(stream, expected, "HTTP response body timed out").await;
}

async fn assert_http_body_bounded<S>(stream: &mut S, expected: &[u8], context: &str)
where
    S: AsyncRead + Unpin,
{
    bounded(assert_http_body_unbounded(stream, expected), context).await;
}

async fn assert_http_body_unbounded<S>(stream: &mut S, expected: &[u8])
where
    S: AsyncRead + Unpin,
{
    let mut body = vec![0u8; expected.len()];
    stream.read_exact(&mut body).await.unwrap();
    assert_eq!(body, expected);
}

async fn assert_connection_closed<S>(stream: &mut S)
where
    S: AsyncRead + Unpin,
{
    let mut byte = [0u8; 1];
    let result = bounded(stream.read(&mut byte), "timed out waiting for peer closure").await;
    match result {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        Ok(read) => panic!("expected peer closure, received {read} byte(s)"),
        Err(error) => panic!("failed while waiting for peer closure: {error}"),
    }
}

async fn connect_request(addr: SocketAddr) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    write_request(&mut stream).await;
    stream
}

async fn assert_ok_request(addr: SocketAddr) {
    let mut stream = connect_request(addr).await;
    let response = read_http_head(&mut stream).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected response: {response}"
    );
}

// Releasing a retained peer is one observation, not four: the held response
// completes with 200, carries its body, and the server closes the transport.
// Every caller checks the status the discarded head used to hide.
async fn drain_peer<S>(peer: &mut S, body: &[u8])
where
    S: AsyncRead + Unpin,
{
    let response = read_http_head(peer).await;
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "unexpected retained peer response: {response}"
    );
    assert_http_body(peer, body).await;
    assert_eof(peer).await;
}

async fn release_and_drain_peer<S>(release: &tokio::sync::Semaphore, peer: &mut S, body: &[u8])
where
    S: AsyncRead + Unpin,
{
    release.add_permits(1);
    drain_peer(peer, body).await;
}

fn assert_invalid<T>(result: Result<T, RuntimeError>) {
    assert!(
        matches!(result, Err(RuntimeError::InvalidArgument(_))),
        "expected InvalidArgument"
    );
}

fn assert_cancelled(result: Result<(), RuntimeError>) {
    assert!(matches!(result, Err(RuntimeError::Cancelled)));
}

fn assert_timeout(result: Result<(), RuntimeError>) {
    assert!(matches!(result, Err(RuntimeError::Timeout)));
}

fn assert_task_panicked(result: Result<(), RuntimeError>, expected: &str) {
    match result {
        Err(RuntimeError::TaskPanicked(message)) => assert_eq!(message.as_ref(), expected),
        other => panic!("expected TaskPanicked({expected:?}), got {other:?}"),
    }
}

fn assert_io_kind(result: Result<(), RuntimeError>, expected: std::io::ErrorKind) {
    match result {
        Err(RuntimeError::Io(error)) => assert_eq!(error.kind(), expected),
        other => panic!("expected Io({expected:?}), got {other:?}"),
    }
}

async fn release_after_shutdown_race(checkpoint: ConnectionOwnerEdge, forced: bool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervisor_selection(addr).unwrap();
    owners.connections
        .pause_once(checkpoint)
        .unwrap();
    let handle = camber::http::serve_background(listener, counting_router(Arc::clone(&counter)))
        .expect("owned server requires a Tokio runtime");

    let mut client = connect_request(addr).await;
    wait_until_paused_bounded(
        &owners,
        checkpoint,
        "shutdown race checkpoint timed out",
    )
    .await;
    match forced {
        true => {
            handle.cancel();
            owners.connections.release(checkpoint).unwrap();
            let completion = handle.into_future();
            let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                tokio::join!(completion, assert_connection_closed(&mut client))
            })
            .await
            .expect("forced server completion and socket rejection timed out");
            assert_eq!(counter.load(Ordering::Acquire), 0);
            assert_cancelled(result);
        }
        false => {
            owners.stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
                .unwrap();
            runtime::request_shutdown();
            owners.connections.release(checkpoint).unwrap();
            wait_until_paused_bounded(
                &owners,
                ServerStopEdge::BeforeSupervisorSelect,
                "graceful shutdown race supervisor boundary timed out",
            )
            .await;
            assert_connection_closed(&mut client).await;
            assert_eq!(counter.load(Ordering::Acquire), 0);
            owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
                .unwrap();
            let result = tokio::time::timeout(Duration::from_secs(5), handle.into_future())
                .await
                .expect("graceful server completion timed out");
            assert!(result.is_ok());
        }
    }
}

// 1.T1
#[camber::test]
async fn lifecycle_controller_is_listener_scoped_and_fail_closed() {
    fn assert_traits<T: Copy + Clone + std::fmt::Debug + Eq + PartialEq>() {}
    assert_traits::<ServerStopEdge>();
    assert_traits::<ConnectionOwnerEdge>();
    #[cfg(feature = "ws")]
    assert_traits::<UpgradeOwnerEdge>();
    assert_traits::<ConnectionFault>();
    assert_traits::<ServerTaskFault>();
    assert_traits::<SupervisorJoinProbe>();

    let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let third = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_addr = first.local_addr().unwrap();
    let second_addr = second.local_addr().unwrap();
    let third_addr = third.local_addr().unwrap();
    let first_connections = mock::connection_owner(first_addr).unwrap();
    let second_connections = mock::connection_owner(second_addr).unwrap();
    assert_invalid(mock::connection_owner(first_addr));
    first_connections
        .pause_once(ConnectionOwnerEdge::AfterAccept)
        .unwrap();
    second_connections
        .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();

    let first_handle = camber::http::serve_background(first, ok_router())
        .expect("owned server requires a Tokio runtime");
    let second_handle = camber::http::serve_background(second, ok_router())
        .expect("owned server requires a Tokio runtime");
    let third_handle = camber::http::serve_background(third, ok_router())
        .expect("owned server requires a Tokio runtime");
    let mut first_client = connect_request(first_addr).await;
    wait_until_paused_bounded(
        &first_connections,
        ConnectionOwnerEdge::AfterAccept,
        "listener-scoped accept pause timed out",
    )
    .await;

    // Joining the failed server proves that its owned listener was dropped.
    // Do not reconnect to `second_addr` after that point: the operating system
    // may already have reassigned the released ephemeral port to another test.
    assert_io_kind(second_handle.await, std::io::ErrorKind::Other);
    assert_ok_request(third_addr).await;

    first_connections
        .release(ConnectionOwnerEdge::AfterAccept)
        .unwrap();
    let response = read_http_head(&mut first_client).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    first_handle.cancel();
    third_handle.cancel();
    assert_cancelled(first_handle.await);
    assert_cancelled(third_handle.await);
}

// 1.T1
#[tokio::test]
async fn lifecycle_controller_rejects_invalid_script_operations() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let owners = mock::armed_faults(listener.local_addr().unwrap()).unwrap();
    assert_invalid(
        owners.connections.wait_until_paused(ConnectionOwnerEdge::AfterAccept)
            .await,
    );
    assert_invalid(owners.connections.release(ConnectionOwnerEdge::AfterAccept));
    owners.connections.pause_once(ConnectionOwnerEdge::AfterAccept)
        .unwrap();
    assert_invalid(owners.connections.pause_once(ConnectionOwnerEdge::AfterAccept));
    owners.connections
        .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    assert_invalid(owners.tasks.inject_once(ServerTaskFault::PanicNextOwnedTask));
}

// 1.T1
#[camber::test]
async fn dropping_controller_releases_waiter_and_allows_address_reuse() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let connections = mock::connection_owner(addr).unwrap();
    connections.pause_once(ConnectionOwnerEdge::AfterAccept)
        .unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    let mut client = connect_request(addr).await;
    wait_until_paused_bounded(
        &connections,
        ConnectionOwnerEdge::AfterAccept,
        "observer drop accept pause timed out",
    )
    .await;
    drop(connections);
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200"));

    handle.cancel();
    assert_cancelled(handle.await);
    drop(client);
    // The shared bounded rebind, because this peer is closing as the address is
    // asked for: a single ask reports its teardown as an address still held.
    let listener = rebind_within(addr, SETTLE_BOUND)
        .await
        .expect("the cancelled server's address was never bindable again");
    let replacement = mock::connection_owner(listener.local_addr().unwrap()).unwrap();
    drop(replacement);
}

// 1.T2 — the Camber probes spawn through the root scope, so this case runs
// inside a real runtime rather than a bare Tokio one.
#[camber::test]
async fn supervisor_join_maps_every_source_result() {
    assert_cancelled(supervisor_join_probe(SupervisorJoinProbe::CamberCancelled).await);
    assert_task_panicked(
        supervisor_join_probe(SupervisorJoinProbe::CamberStringPanic).await,
        PROBE_PANIC,
    );
    assert_task_panicked(
        supervisor_join_probe(SupervisorJoinProbe::CamberOpaquePanic).await,
        "unknown panic",
    );
    assert_task_panicked(
        supervisor_join_probe(SupervisorJoinProbe::CamberChannelClosed).await,
        "task channel closed",
    );
    assert!(
        supervisor_join_probe(SupervisorJoinProbe::TokioSuccess)
            .await
            .is_ok()
    );
    assert_task_panicked(
        supervisor_join_probe(SupervisorJoinProbe::TokioCancelled).await,
        "server supervisor cancelled unexpectedly",
    );
    assert_task_panicked(
        supervisor_join_probe(SupervisorJoinProbe::TokioStringPanic).await,
        PROBE_PANIC,
    );
    assert_task_panicked(
        supervisor_join_probe(SupervisorJoinProbe::TokioOpaquePanic).await,
        "unknown panic",
    );
}

// 1.T2
#[camber::test]
async fn camber_background_join_flattens_success() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stop = mock::server_stop(listener.local_addr().unwrap()).unwrap();
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "background join supervisor boundary timed out",
    )
    .await;
    runtime::request_shutdown();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    let result: Result<(), RuntimeError> = handle.await;
    assert!(result.is_ok());
}

// 1.T3
#[camber::test]
async fn graceful_shutdown_after_accept_never_dispatches() {
    release_after_shutdown_race(ConnectionOwnerEdge::AfterAccept, false).await;
}

// 1.T3
#[camber::test]
async fn forced_shutdown_after_accept_never_dispatches() {
    release_after_shutdown_race(ConnectionOwnerEdge::AfterAccept, true).await;
}

// 1.T3
#[camber::test]
async fn graceful_shutdown_after_permit_never_dispatches() {
    release_after_shutdown_race(ConnectionOwnerEdge::AfterPermit, false).await;
}

// 1.T3
#[camber::test]
async fn forced_shutdown_after_permit_never_dispatches() {
    release_after_shutdown_race(ConnectionOwnerEdge::AfterPermit, true).await;
}

// 1.T3
#[camber::test]
async fn admitted_plain_transport_keeps_owner_pending_until_release() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = serve_held(
        listener,
        held_router(entered_tx, Arc::clone(&release), Arc::clone(&dropped)),
    );
    let mut client = connect_request(addr).await;
    await_handler_entry(
        entered_rx,
        "plain transport request did not enter the handler",
    )
    .await;
    runtime::request_shutdown();
    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    assert!(!dropped.load(Ordering::Acquire));
    release_and_drain_peer(&release, &mut client, b"released").await;
    assert!(completion.await.is_ok());
    assert!(dropped.load(Ordering::Acquire));
}

// 1.T3
#[camber::test]
async fn admitted_tls_transport_keeps_owner_pending_until_release() {
    let (tls_config, connector) = common::self_signed_server_and_connector();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = serve_held_tls(
        listener,
        held_router(entered_tx, Arc::clone(&release), Arc::clone(&dropped)),
        tls_config,
    );
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut client = connector.connect(server_name, tcp).await.unwrap();
    client.write_all(HTTP_REQUEST).await.unwrap();
    await_handler_entry(
        entered_rx,
        "TLS transport request did not enter the handler",
    )
    .await;
    runtime::request_shutdown();
    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    release_and_drain_peer(&release, &mut client, b"released").await;
    assert!(completion.await.is_ok());
    assert!(dropped.load(Ordering::Acquire));
}

#[cfg(feature = "ws")]
fn attach_drain_ws(router: &mut Router) {
    use camber::http::WsConn;

    router.ws("/ws", |_request: &Request, mut connection: WsConn| {
        while connection.recv().is_some() {}
        Ok(())
    });
}

#[cfg(feature = "ws")]
fn websocket_router() -> Router {
    let mut router = Router::new();
    attach_drain_ws(&mut router);
    router
}

// 1.T3
#[cfg(feature = "ws")]
#[camber::test]
async fn admitted_websocket_bridge_keeps_owner_pending_until_transport_closes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(listener, websocket_router())
        .expect("owned server requires a Tokio runtime");
    let mut client = common::upgraded_ws_peer(addr, "/ws", "the admitted bridge").await;
    runtime::request_shutdown();
    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    common::assert_graceful_close_then_eof(&mut client, "admitted bridge").await;
    let result = completion.await;
    assert!(result.is_ok(), "unexpected server result: {result:?}");
}

async fn enter_default_grace_deadline(
    owners: &impl Owns<ServerStopController>,
    addr: SocketAddr,
    server: &tokio::task::JoinHandle<Result<(), RuntimeError>>,
    dropped: &AtomicBool,
) {
    let stop = owners.owner();
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    stop.pause_once(ServerStopEdge::SupervisorSelectedRuntime)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "initial supervisor boundary timed out",
    )
    .await;
    runtime::request_shutdown();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedRuntime,
        "runtime shutdown selection timed out",
    )
    .await;
    apply_selected(
        owners,
        ServerStopEdge::SupervisorSelectedRuntime,
        "graceful supervisor boundary timed out",
    )
    .await;
    let connect_result = std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5));
    assert!(
        connect_result.is_err(),
        "listener accepted a connection after graceful shutdown"
    );
    tokio::time::advance(Duration::from_secs(30) - Duration::from_millis(1)).await;
    assert!(
        !server.is_finished(),
        "outer serve task finished before grace"
    );
    assert!(
        !dropped.load(Ordering::Acquire),
        "request future dropped before the grace deadline"
    );
}

// 1.T5
#[tokio::test(start_paused = true)]
async fn default_grace_deadline_aborts_joins_and_releases_direct_transport() {
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = mock::server_stop(addr).unwrap();
    let server = tokio::spawn(
        camber::http::server(held_router(entered_tx, release, Arc::clone(&dropped)))
            .policy(held_request_policy())
            .serve_async(listener)
            .expect("owned server requires a Tokio runtime"),
    );
    let client = connect_request(addr).await;
    await_handler_entry(
        entered_rx,
        "grace deadline request did not enter the handler",
    )
    .await;

    enter_default_grace_deadline(&stop, addr, &server, &dropped).await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedDeadline)
        .unwrap();
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    tokio::time::advance(Duration::from_millis(1)).await;
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedDeadline,
        "grace deadline selection timed out",
    )
    .await;
    stop.release(ServerStopEdge::SupervisorSelectedDeadline)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::AfterSupervisorResultSend,
        "owned task drain timed out",
    )
    .await;
    // Reaching finish proves the aborted owned-task set was drained and joined.
    assert!(
        !server.is_finished(),
        "outer serve task bypassed the final checkpoint"
    );
    assert!(
        dropped.load(Ordering::Acquire),
        "aborted request future was not dropped before its task joined"
    );
    // The OS socket deadline stays independent from paused Tokio time.
    assert_eof_with_socket_deadline(client);
    stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .expect("serve_async finalization timed out")
        .unwrap();
    assert_timeout(result);
}

// 1.T5
#[test]
fn configured_grace_deadline_is_used_by_background_owner() {
    let dropped = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&dropped);
    runtime::builder()
        .shutdown_timeout(Duration::from_millis(150))
        .run(|| {
            runtime::block_on(async {
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let handle = serve_held(
                    listener,
                    held_router(
                        entered_tx,
                        Arc::new(tokio::sync::Semaphore::new(0)),
                        Arc::clone(&observed),
                    ),
                );
                let mut client = connect_request(addr).await;
                await_handler_entry(
                    entered_rx,
                    "configured grace request did not enter the handler",
                )
                .await;
                let started = Instant::now();
                runtime::request_shutdown();
                let mut completion = Box::pin(handle.into_future());
                let result = tokio::time::timeout(Duration::from_secs(2), &mut completion)
                    .await
                    .expect("configured grace owner completion timed out");
                let elapsed = started.elapsed();
                assert_timeout(result);
                assert!(elapsed >= Duration::from_millis(100));
                assert!(elapsed < Duration::from_secs(2));
                assert_eof(&mut client).await;
                assert!(tokio::net::TcpStream::connect(addr).await.is_err());
            });
        })
        .unwrap();
    assert!(dropped.load(Ordering::Acquire));
}

// 1.T6
#[test]
fn shutdown_while_waiting_for_permit_closes_unadmitted_socket() {
    runtime::builder()
        .connection_limit(1)
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let counter = Arc::new(AtomicUsize::new(0));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let owners = mock::supervisor_selection(addr).unwrap();
                let handle =
                    camber::http::serve_background(listener, counting_router(Arc::clone(&counter)))
                        .expect("owned server requires a Tokio runtime");
                let mut first = connect_request(addr).await;
                let response =
                    read_http_head_bounded(&mut first, "first admitted response timed out").await;
                assert!(response.starts_with("HTTP/1.1 200"));
                assert_eq!(counter.load(Ordering::Acquire), 1);

                owners.connections.pause_once(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                let mut second =
                    tokio::time::timeout(Duration::from_secs(5), connect_request(addr))
                        .await
                        .expect("second connection timed out");
                wait_until_paused_bounded(
                    &owners,
                    ConnectionOwnerEdge::PermitWaitPending,
                    "pending permit checkpoint timed out",
                )
                .await;
                owners.stop.pause_once(ServerStopEdge::SupervisorSelectedRuntime)
                    .unwrap();
                runtime::request_shutdown();
                wait_until_paused_bounded(
                    &owners,
                    ServerStopEdge::SupervisorSelectedRuntime,
                    "runtime selection checkpoint timed out",
                )
                .await;
                owners.connections.release(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                drop(first);
                owners.stop.release(ServerStopEdge::SupervisorSelectedRuntime)
                    .unwrap();

                let completion = handle.into_future();
                let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
                    tokio::join!(completion, assert_connection_closed(&mut second))
                })
                .await
                .expect("server completion and second socket closure timed out");
                assert_eq!(counter.load(Ordering::Acquire), 1);
                assert!(result.is_ok(), "unexpected server result: {result:?}");
            });
        })
        .unwrap();
}

// 1.T7
#[camber::test]
async fn shutdown_joins_incomplete_tls_handshake() {
    let (cert, key) = common::generate_self_signed_cert();
    let tls_config = common::server_tls_config(&cert, &key);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervisor_selection(addr).unwrap();
    owners.connections.pause_once(ConnectionOwnerEdge::AfterAccept)
        .unwrap();
    owners.connections.pause_once(ConnectionOwnerEdge::AfterPermit)
        .unwrap();
    let handle = camber::http::serve_background_tls(listener, ok_router(), tls_config)
        .expect("owned server requires a Tokio runtime");
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    wait_until_paused_bounded(
        &owners,
        ConnectionOwnerEdge::AfterAccept,
        "TLS handshake accept pause timed out",
    )
    .await;
    owners.connections.release(ConnectionOwnerEdge::AfterAccept)
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ConnectionOwnerEdge::AfterPermit,
        "TLS handshake permit pause timed out",
    )
    .await;
    apply_selected(
        &owners,
        ConnectionOwnerEdge::AfterPermit,
        "TLS handshake supervisor boundary timed out",
    )
    .await;
    runtime::request_shutdown();
    owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(5), handle.into_future())
        .await
        .expect("TLS server owner completion timed out");
    assert!(result.is_ok());
    assert_eof(&mut client).await;
}

/// Serve `router` in the background and hold one request inside its handler.
///
/// The retained fixtures differ only in the router they serve, the path they
/// hold, and which owners their case reads, so binding, registering, connecting,
/// and waiting for handler entry are written here once. `observe` is the
/// registration itself: each case passes the factory naming the owner families
/// it reads, so the fixture widens nothing on its callers' behalf.
async fn serve_with_held_request<Observer>(
    observe: impl FnOnce(SocketAddr) -> Result<Observer, RuntimeError>,
    router: Router,
    path: &str,
    entered: tokio::sync::oneshot::Receiver<()>,
    context: &str,
) -> (Observer, SocketAddr, ServerHandle, tokio::net::TcpStream) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = observe(addr).unwrap();
    let handle = serve_held(listener, router);
    let client = hold_request(addr, path, entered, context).await;
    (owners, addr, handle, client)
}

/// Offer one request to a served fixture and wait until its handler holds it.
///
/// Admission is deterministic here because nothing else the supervisor selects
/// is ready: it waits on the listener for as long as the kernel takes to finish
/// the handshake, rather than racing it against work already in hand.
async fn hold_request(
    addr: SocketAddr,
    path: &str,
    entered: tokio::sync::oneshot::Receiver<()>,
    context: &str,
) -> tokio::net::TcpStream {
    let client = connect_request_path(addr, path).await;
    await_handler_entry(entered, context).await;
    client
}

async fn retained_server<Observer>(
    observe: impl FnOnce(SocketAddr) -> Result<Observer, RuntimeError>,
) -> (
    Observer,
    SocketAddr,
    ServerHandle,
    tokio::net::TcpStream,
    Arc<tokio::sync::Semaphore>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let router = held_router(
        entered_tx,
        Arc::clone(&release),
        Arc::new(AtomicBool::new(false)),
    );
    let (owners, addr, handle, client) = serve_with_held_request(
        observe,
        router,
        "/",
        entered_rx,
        "retained fixture request did not enter the handler",
    )
    .await;
    (owners, addr, handle, client, release)
}

/// A retained server that also holds a request whose release is an opaque
/// panic.
///
/// A second owned-task panic cannot come from a second faulted connection.
/// Reaping the first panic closes admission, so a connection offered after that
/// reap is refused. Offering it before leaves the accept racing a completion
/// the supervisor already holds, and the kernel decides that race: a peer whose
/// connect has returned is not yet certain to be on the listener's queue, and a
/// select that answers the completion first takes the listener away for good.
/// A request admitted while nothing else is ready and released after the first
/// reap settles both. Its task exists before that reap and panics after it.
struct RetainedPanicServer {
    owners: ScopedFaultedSelection,
    addr: SocketAddr,
    handle: ServerHandle,
    peer: tokio::net::TcpStream,
    release: Arc<tokio::sync::Semaphore>,
    panicking_peer: tokio::net::TcpStream,
    panicking_release: Arc<tokio::sync::Semaphore>,
}

async fn retained_panic_server() -> RetainedPanicServer {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (panic_entered_tx, panic_entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let panicking_release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut router = held_router(
        entered_tx,
        Arc::clone(&release),
        Arc::new(AtomicBool::new(false)),
    );
    add_opaque_panic_route(
        &mut router,
        panic_entered_tx,
        Arc::clone(&panicking_release),
    );
    let (owners, addr, handle, peer) = serve_with_held_request(
        mock::faulted_selection,
        router,
        "/",
        entered_rx,
        "retained fixture request did not enter the handler",
    )
    .await;
    let panicking_peer = hold_request(
        addr,
        OPAQUE_PANIC_PATH,
        panic_entered_rx,
        "retained panic request did not enter the handler",
    )
    .await;
    RetainedPanicServer {
        owners,
        addr,
        handle,
        peer,
        release,
        panicking_peer,
        panicking_release,
    }
}

async fn retained_owner_server() -> (
    ScopedServerStop,
    SocketAddr,
    ServerHandle,
    tokio::net::TcpStream,
    Arc<tokio::sync::Semaphore>,
    Arc<AtomicBool>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let router = named_held_router(
        entered_tx,
        Arc::clone(&release),
        Arc::clone(&dropped),
        Arc::new(AtomicUsize::new(0)),
    );
    let (owners, addr, handle, client) = serve_with_held_request(
        mock::server_stop,
        router,
        "/active",
        entered_rx,
        "retained owner fixture request did not enter the handler",
    )
    .await;
    (owners, addr, handle, client, release, dropped)
}

async fn assert_graceful_owner_waits<F>(
    owners: &impl Owns<ServerStopController>,
    completion: F,
    addr: SocketAddr,
    mut client: tokio::net::TcpStream,
    release: &tokio::sync::Semaphore,
    dropped: &AtomicBool,
) where
    F: Future<Output = Result<(), RuntimeError>>,
{
    let mut completion = Box::pin(completion);
    assert!(completion.as_mut().now_or_never().is_none());
    assert!(!dropped.load(Ordering::Acquire));
    tokio::time::timeout(Duration::from_secs(5), async {
        common::assert_admission_closed(addr, OBSERVATION_DEADLINE).await;
        release.add_permits(1);
        let response = read_http_head_bounded(&mut client, "active response head timed out").await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert_http_body_bounded(&mut client, b"active", "active response body timed out").await;
        assert_eof(&mut client).await;
        assert!(dropped.load(Ordering::Acquire));
    })
    .await
    .expect("graceful transport observations timed out");
    wait_and_release_bounded(
        owners,
        ServerStopEdge::AfterSupervisorResultSend,
        "graceful owner result send timed out",
    )
    .await;
    let result = tokio::time::timeout(Duration::from_secs(5), &mut completion)
        .await
        .expect("graceful owner completion timed out");
    assert!(
        result.is_ok(),
        "unexpected graceful owner result: {result:?}"
    );
}

async fn assert_forced_owner_joins<F>(
    completion: F,
    mut client: tokio::net::TcpStream,
    dropped: &AtomicBool,
) where
    F: Future<Output = Result<(), RuntimeError>>,
{
    let (result, ()) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(completion, assert_connection_closed(&mut client))
    })
    .await
    .expect("forced owner completion and transport closure timed out");
    assert!(dropped.load(Ordering::Acquire));
    assert_cancelled(result);
}

// 1.T11
#[camber::test]
async fn fatal_accept_drains_peer_before_returning_io() {
    let (owners, addr, handle, mut peer, release) = retained_server(mock::supervisor_selection).await;
    owners.stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::BeforeSupervisorSelect,
        "fatal accept drain supervisor boundary timed out",
    )
    .await;
    owners.connections
        .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "fatal accept selection timed out",
    )
    .await;
    owners.stop.release(ServerStopEdge::SupervisorSelectedAccept)
        .unwrap();

    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    runtime::request_shutdown();
    common::assert_admission_closed(addr, OBSERVATION_DEADLINE).await;
    release_and_drain_peer(&release, &mut peer, b"released").await;
    assert_io_kind(completion.await, std::io::ErrorKind::Other);
}

// 1.T11
#[camber::test]
async fn owned_task_panic_drains_retained_peer_before_returning() {
    let (owners, addr, handle, mut peer, release) = retained_server(mock::server_task).await;
    owners
        .inject_once(ServerTaskFault::PanicNextOwnedTask)
        .unwrap();
    let mut faulted = connect_request(addr).await;
    assert_connection_closed(&mut faulted).await;
    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    common::assert_admission_closed(addr, OBSERVATION_DEADLINE).await;
    release_and_drain_peer(&release, &mut peer, b"released").await;
    assert_task_panicked(completion.await, OWNED_TASK_PANIC);
}

// A fatal accept error installs a provisional outcome and starts the grace
// deadline. This leaves the supervisor held at its select boundary with both
// already applied, which is the state both escalation rows start from.
async fn pause_after_fatal_accept_starts_grace(owners: &ScopedSupervisorSelection) {
    owners.stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        owners,
        ServerStopEdge::BeforeSupervisorSelect,
        "fatal accept initial supervisor boundary timed out",
    )
    .await;
    owners.connections
        .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    select_next(
        owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "fatal accept selection timed out",
    )
    .await;
    apply_selected(
        owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "fatal accept grace boundary timed out",
    )
    .await;
}

// 1.T11, 1.T12
#[tokio::test(start_paused = true)]
async fn fatal_outcome_is_replaced_by_timeout_after_peer_drain_escalation() {
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let (owners, _addr, handle, peer, _release) = retained_server(mock::supervisor_selection).await;
    pause_after_fatal_accept_starts_grace(&owners).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    select_deadline(&owners).await;
    release_deadline_and_wait_for_drained_result(&owners).await;
    assert_eof_with_socket_deadline(peer);
    owners.stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    assert_timeout(handle.await);
}

// 1.T11, 1.T12
#[camber::test]
async fn explicit_cancel_replaces_provisional_fatal_outcome() {
    let (owners, _addr, handle, mut peer, _release) = retained_server(mock::supervisor_selection).await;
    pause_after_fatal_accept_starts_grace(&owners).await;
    handle.cancel();
    owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    assert_eof(&mut peer).await;
    assert_cancelled(handle.await);
}

// Wait until production is held at `point`, on whichever owner holds it.
//
// `owners` is asked for that one owner and nothing else, so a case that
// registered a stop owner and a connection owner lends exactly the one the
// point belongs to.
async fn wait_until_paused_bounded<P: OwnerPoint>(
    owners: &impl Owns<P::Owner>,
    point: P,
    context: &str,
) {
    let held = owners.owner();
    bounded(point.paused_at(&held), context).await.unwrap();
}

async fn wait_and_release_bounded<P: OwnerPoint>(
    owners: &impl Owns<P::Owner>,
    point: P,
    context: &str,
) {
    wait_until_paused_bounded(owners, point, context).await;
    point.release_at(&owners.owner()).unwrap();
}

// Step the supervisor from its select boundary to the branch it selects.
//
// The order is what makes the step observable: `selected` is armed while the
// boundary still holds the loop, so releasing the boundary cannot run the
// iteration past the observation. `context` names the step, so an expired bound
// reports which selection never arrived.
//
// Every branch a supervisor selects is one of its own edges, so this asks for
// the stop owner alone.
async fn select_next(
    owners: &impl Owns<ServerStopController>,
    selected: ServerStopEdge,
    context: &str,
) {
    let stop = owners.owner();
    selected.arm_at(&stop).unwrap();
    stop.release(ServerStopEdge::BeforeSupervisorSelect).unwrap();
    wait_until_paused_bounded(&stop, selected, context).await;
}

// Step the supervisor from a selected branch back to its select boundary.
//
// [`select_next`] mirrored, and armed in the same order for the same reason:
// the boundary is armed before `selected` is released, so the loop cannot reach
// its next iteration unobserved. What is released is not always the supervisor's
// own edge — a connection held after its permit is applied the same way — so the
// released point names its own owner and the boundary names the stop owner.
async fn apply_selected<P: OwnerPoint>(
    owners: &(impl Owns<ServerStopController> + Owns<P::Owner>),
    selected: P,
    context: &str,
) {
    let stop = Owns::<ServerStopController>::owner(owners);
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    selected
        .release_at(&Owns::<P::Owner>::owner(owners))
        .unwrap();
    wait_until_paused_bounded(&stop, ServerStopEdge::BeforeSupervisorSelect, context).await;
}

#[cfg(feature = "ws")]
async fn apply_selected_event_then_release_transfer<P: OwnerPoint>(
    owners: &(impl Owns<ServerStopController> + Owns<UpgradeOwnerController> + Owns<P::Owner>),
    selected: P,
    context: &str,
) {
    apply_selected(owners, selected, context).await;
    release_transfer_edge_and_resume(owners);
}

/// Let a held transfer answer, and let the supervisor take its next event.
///
/// Both releases belong to one step: the connection cannot answer while it is
/// held, and the supervisor cannot reap the connection that answers while its
/// own select boundary is held.
#[cfg(feature = "ws")]
fn release_transfer_edge_and_resume(
    owners: &(impl Owns<ServerStopController> + Owns<UpgradeOwnerController>),
) {
    Owns::<UpgradeOwnerController>::owner(owners)
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .unwrap();
    Owns::<ServerStopController>::owner(owners)
        .release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
}


async fn prepare_completed_task(
    owners: &(impl Owns<ServerStopController> + Owns<ConnectionOwnerController>),
    addr: SocketAddr,
) {
    Owns::<ServerStopController>::owner(owners)
        .pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    Owns::<ConnectionOwnerController>::owner(owners)
        .pause_once(ConnectionOwnerEdge::AfterConnectionFutureCompleted)
        .unwrap();
    wait_until_paused_bounded(
        &Owns::<ServerStopController>::owner(owners),
        ServerStopEdge::BeforeSupervisorSelect,
        "completed-task initial supervisor boundary timed out",
    )
    .await;
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(CLOSE_REQUEST).await.unwrap();
    select_next(
        owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "completed-task accept selection timed out",
    )
    .await;
    apply_selected(
        owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "completed-task post-accept boundary timed out",
    )
    .await;
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert_http_body(&mut client, b"ok").await;
    assert_eof(&mut client).await;
    wait_and_release_bounded(
        owners,
        ConnectionOwnerEdge::AfterConnectionFutureCompleted,
        "completed task did not reach its terminal boundary",
    )
    .await;
}

async fn observe_deferred_task_reap(
    owners: &impl Owns<ServerStopController>,
    selected_winner: ServerStopEdge,
) {
    apply_selected(
        owners,
        selected_winner,
        "deferred reap supervisor boundary timed out",
    )
    .await;
    select_next(
        owners,
        ServerStopEdge::SupervisorSelectedTask,
        "deferred task reap timed out",
    )
    .await;
    Owns::<ServerStopController>::owner(owners)
        .release(ServerStopEdge::SupervisorSelectedTask)
        .unwrap();
}

async fn owned_task_fault_result(fault: ServerTaskFault, expected: &str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::faulted_selection(addr).unwrap();
    owners.connections.pause_once(ConnectionOwnerEdge::AfterPermit)
        .unwrap();
    owners.tasks.inject_once(fault).unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    let mut client = connect_request(addr).await;
    wait_until_paused_bounded(
        &owners,
        ConnectionOwnerEdge::AfterPermit,
        "owned-task fault permit pause timed out",
    )
    .await;
    apply_selected(
        &owners,
        ConnectionOwnerEdge::AfterPermit,
        "owned-task fault supervisor boundary timed out",
    )
    .await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "owned-task fault selection timed out",
    )
    .await;
    owners.stop.release(ServerStopEdge::SupervisorSelectedTask)
        .unwrap();
    assert_connection_closed(&mut client).await;
    assert_task_panicked(handle.await, expected);
}

async fn prepare_faulted_task(
    owners: &ScopedFaultedSelection,
    addr: SocketAddr,
    fault: ServerTaskFault,
) {
    owners.connections.pause_once(ConnectionOwnerEdge::AfterPermit)
        .unwrap();
    owners.tasks.inject_once(fault).unwrap();
    let client = connect_request(addr).await;
    wait_until_paused_bounded(
        owners,
        ConnectionOwnerEdge::AfterPermit,
        "faulted-task permit pause timed out",
    )
    .await;
    apply_selected(
        owners,
        ConnectionOwnerEdge::AfterPermit,
        "faulted-task supervisor boundary timed out",
    )
    .await;
    assert_connection_closed_with_independent_deadline(client).await;
}

// 1.T12: real inserted-task panic join rows in Running.
#[camber::test]
async fn owned_task_string_panic_maps_exactly() {
    owned_task_fault_result(ServerTaskFault::PanicNextOwnedTask, OWNED_TASK_PANIC).await;
}

// 1.T12
#[camber::test]
async fn owned_task_opaque_panic_maps_to_unknown_panic() {
    owned_task_fault_result(ServerTaskFault::PanicNextOwnedTaskOpaque, "unknown panic").await;
}

// 1.T12: graceful without a candidate then panic installs the panic.
#[camber::test]
async fn graceful_then_owned_task_panic_installs_candidate() {
    let (owners, addr, handle, mut peer, release) = retained_server(mock::faulted_selection).await;
    prepare_faulted_task(&owners, addr, ServerTaskFault::PanicNextOwnedTask).await;
    owners.stop.pause_once(ServerStopEdge::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::SupervisorSelectedRuntime,
        "graceful-then-panic runtime selection timed out",
    )
    .await;
    observe_deferred_task_reap(&owners, ServerStopEdge::SupervisorSelectedRuntime).await;
    release_and_drain_peer(&release, &mut peer, b"released").await;
    assert_task_panicked(handle.await, OWNED_TASK_PANIC);
}

// 1.T12: IO selected first remains the candidate when a panic is ready.
#[camber::test]
async fn io_then_owned_task_panic_retains_io() {
    let (owners, addr, handle, mut peer, release) = retained_server(mock::faulted_selection).await;
    prepare_faulted_task(&owners, addr, ServerTaskFault::PanicNextOwnedTask).await;
    owners.connections
        .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "io-then-panic accept selection timed out",
    )
    .await;
    observe_deferred_task_reap(&owners, ServerStopEdge::SupervisorSelectedAccept).await;
    release_and_drain_peer(&release, &mut peer, b"released").await;
    assert_io_kind(handle.await, std::io::ErrorKind::Other);
}

// 1.T12: panic selected first remains the candidate after repeated graceful.
#[camber::test]
async fn owned_task_panic_then_graceful_retains_panic() {
    let (owners, addr, handle, mut peer, release) = retained_server(mock::faulted_selection).await;
    prepare_faulted_task(&owners, addr, ServerTaskFault::PanicNextOwnedTask).await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "panic-then-graceful task selection timed out",
    )
    .await;
    owners.stop.release(ServerStopEdge::SupervisorSelectedTask)
        .unwrap();
    runtime::request_shutdown();
    release_and_drain_peer(&release, &mut peer, b"released").await;
    assert_task_panicked(handle.await, OWNED_TASK_PANIC);
}

// 1.T12: one owned-task panic then another retains the first payload.
#[camber::test]
async fn owned_task_panic_then_opaque_panic_retains_first_payload() {
    let RetainedPanicServer {
        owners,
        addr,
        handle,
        mut peer,
        release,
        mut panicking_peer,
        panicking_release,
    } = retained_panic_server().await;
    prepare_faulted_task(&owners, addr, ServerTaskFault::PanicNextOwnedTask).await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "faulted task selection timed out",
    )
    .await;
    apply_selected(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "faulted task reap boundary timed out",
    )
    .await;
    // What proves the second completion was a panic. A connection future that
    // returned instead of unwinding would answer its peer and reach the
    // terminal boundary armed here. Either one leaves the supervisor no second
    // candidate to keep out, and the retention below would pass on nothing.
    owners.connections.pause_once(ConnectionOwnerEdge::AfterConnectionFutureCompleted)
        .unwrap();
    panicking_release.add_permits(1);
    assert_connection_closed(&mut panicking_peer).await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "held panic task selection timed out",
    )
    .await;
    assert_eq!(
        owners.connections.polls(ConnectionOwnerEdge::AfterConnectionFutureCompleted)
            .unwrap(),
        0,
        "the panicking handler's connection future reached its terminal boundary"
    );
    owners.stop.release(ServerStopEdge::SupervisorSelectedTask)
        .unwrap();
    release.add_permits(1);
    wait_and_release_bounded(
        &owners,
        ConnectionOwnerEdge::AfterConnectionFutureCompleted,
        "retained peer terminal boundary timed out",
    )
    .await;
    drain_peer(&mut peer, b"released").await;
    assert_task_panicked(handle.await, OWNED_TASK_PANIC);
}

// 1.T12: unexpected cancellation while Running is fatal.
#[camber::test]
async fn unexpected_owned_task_cancellation_while_running_is_fatal() {
    owned_task_fault_result(
        ServerTaskFault::CancelNextOwnedTask,
        "owned HTTP task cancelled unexpectedly",
    )
    .await;
}

// 1.T12: runtime graceful starts one deadline; repeated graceful does not restart it.
#[tokio::test(start_paused = true)]
async fn runtime_graceful_and_repeated_graceful_share_one_deadline() {
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let (stop, _addr, handle, peer, _release) = retained_server(mock::server_stop).await;
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "runtime deadline initial boundary timed out",
    )
    .await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedRuntime,
        "initial runtime graceful selection timed out",
    )
    .await;
    apply_selected(
        &stop,
        ServerStopEdge::SupervisorSelectedRuntime,
        "post-runtime-graceful boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(20)).await;
    runtime::request_shutdown();
    stop.pause_once(ServerStopEdge::SupervisorSelectedDeadline)
        .unwrap();
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedDeadline,
        "original runtime deadline was restarted",
    )
    .await;
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::AfterSupervisorResultSend,
        "runtime deadline owned-task drain timed out",
    )
    .await;
    // This checkpoint proves the server-side transport owner was joined. The
    // blocking socket deadline observes the independent OS peer notification.
    assert_eof_with_socket_deadline(peer);
    stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("runtime deadline owner completion timed out");
    assert_timeout(result);
}

// 1.T12: cancel in Running wins over runtime, accept error, and a completed task.
#[camber::test]
async fn control_branch_wins_and_completed_task_is_reaped_next_iteration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervisor_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    prepare_completed_task(&owners, addr).await;
    owners.connections
        .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    runtime::request_shutdown();
    handle.cancel();
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedControl,
        "control selection over accept error and task timed out",
    )
    .await;
    observe_deferred_task_reap(&owners, ServerStopEdge::SupervisorSelectedControl).await;
    assert_cancelled(handle.await);
}

// 1.T12: runtime wins over accept error and completed task; the accept error is dropped.
#[camber::test]
async fn runtime_branch_drops_losing_accept_error_and_reaps_task_next() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervisor_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    prepare_completed_task(&owners, addr).await;
    owners.connections
        .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    runtime::request_shutdown();
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedRuntime,
        "runtime selection over accept error and task timed out",
    )
    .await;
    observe_deferred_task_reap(&owners, ServerStopEdge::SupervisorSelectedRuntime).await;
    assert!(handle.await.is_ok());
}

// 1.T12: a real accept beats an already-completed owned task.
#[camber::test]
async fn accept_branch_wins_over_task_then_reaps_task() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervisor_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    prepare_completed_task(&owners, addr).await;
    let mut second = connect_request(addr).await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "accept selection over completed task timed out",
    )
    .await;
    observe_deferred_task_reap(&owners, ServerStopEdge::SupervisorSelectedAccept).await;
    let response = read_http_head(&mut second).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    handle.cancel();
    assert_cancelled(handle.await);
}

// 1.T12: task completion is selected when it is the sole ready branch.
#[camber::test]
async fn task_branch_is_selected_when_task_is_only_ready_work() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervisor_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    prepare_completed_task(&owners, addr).await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "sole-ready task selection timed out",
    )
    .await;
    owners.stop.release(ServerStopEdge::SupervisorSelectedTask)
        .unwrap();
    handle.cancel();
    assert_cancelled(handle.await);
}

async fn shutdown_wins_over_ready_accept(forced: bool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = mock::server_stop(addr).unwrap();
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    let handle = camber::http::serve_background(listener, counting_router(Arc::clone(&counter)))
        .expect("owned server requires a Tokio runtime");
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "shutdown-over-accept supervisor boundary timed out",
    )
    .await;
    let mut client = connect_request(addr).await;
    let selected = match forced {
        true => ServerStopEdge::SupervisorSelectedControl,
        false => ServerStopEdge::SupervisorSelectedRuntime,
    };
    selected.arm_at(&stop).unwrap();
    match forced {
        true => handle.cancel(),
        false => runtime::request_shutdown(),
    }
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &stop,
        selected,
        "shutdown-over-accept selection timed out",
    )
    .await;
    assert_connection_closed(&mut client).await;
    assert_eq!(counter.load(Ordering::Acquire), 0);
    match forced {
        true => assert_cancelled(handle.await),
        false => assert!(handle.await.is_ok()),
    }
}

// 1.T12: control beats a real successful accept.
#[camber::test]
async fn control_branch_wins_over_ready_accept_success() {
    shutdown_wins_over_ready_accept(true).await;
}

// 1.T12: runtime shutdown beats a real successful accept.
#[camber::test]
async fn runtime_branch_wins_over_ready_accept_success() {
    shutdown_wins_over_ready_accept(false).await;
}

// 1.T12: forced control beats a permit that became ready after a real wait.
#[test]
fn control_branch_wins_over_ready_permit() {
    runtime::builder()
        .connection_limit(1)
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let counter = Arc::new(AtomicUsize::new(0));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let owners = mock::supervisor_selection(addr).unwrap();
                let handle =
                    camber::http::serve_background(listener, counting_router(Arc::clone(&counter)))
                        .expect("owned server requires a Tokio runtime");
                let mut first = connect_request(addr).await;
                assert!(read_http_head(&mut first).await.starts_with("HTTP/1.1 200"));
                owners.connections.pause_once(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                let mut second = connect_request(addr).await;
                wait_until_paused_bounded(
                    &owners,
                    ConnectionOwnerEdge::PermitWaitPending,
                    "second connection did not wait for a permit",
                )
                .await;
                owners.stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
                    .unwrap();
                handle.cancel();
                drop(first);
                owners.connections.release(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                wait_and_release_bounded(
                    &owners,
                    ServerStopEdge::SupervisorSelectedControl,
                    "control selection over a ready permit timed out",
                )
                .await;
                assert_connection_closed(&mut second).await;
                assert_eq!(counter.load(Ordering::Acquire), 1);
                assert_cancelled(handle.await);
            });
        })
        .unwrap();
}

// 1.T12: an already-fixed timeout is immutable when cancel is equal-ready.
#[tokio::test(start_paused = true)]
async fn deadline_branch_wins_over_cancel_and_remains_timeout() {
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let (stop, _addr, handle, peer, _release) = retained_server(mock::server_stop).await;
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "deadline-over-cancel initial boundary timed out",
    )
    .await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedRuntime,
        "deadline-over-cancel runtime selection timed out",
    )
    .await;
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "deadline-over-cancel post-graceful boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(30)).await;
    select_next(
        &stop,
        ServerStopEdge::SupervisorSelectedDeadline,
        "deadline-over-cancel deadline selection timed out",
    )
    .await;
    // The barrier this row is about. Time passing is not a committed timeout,
    // so the cancel is held until the expiry has actually committed; only then
    // is "a committed timeout is not rewritten" a claim about commit order
    // rather than about which ready branch the executor happened to poll.
    stop.pause_once(ServerStopEdge::AfterCommit).unwrap();
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    stop.release(ServerStopEdge::SupervisorSelectedDeadline)
        .unwrap();
    bounded(
        stop.wait_until_paused(ServerStopEdge::AfterCommit),
        "deadline-over-cancel timeout commit timed out",
    )
    .await
    .unwrap();
    assert_eq!(stop.observed().phase, "deadline-expired");
    handle.cancel();
    assert_eq!(
        stop.observed().phase,
        "deadline-expired",
        "a cancellation committed after the timeout must not move the phase"
    );
    stop.release(ServerStopEdge::AfterCommit).unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::AfterSupervisorResultSend,
        "deadline owned-task drain timed out",
    )
    .await;
    assert_eof_with_socket_deadline(peer);
    stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    assert_timeout(handle.await);
}

async fn pause_after_owned_panic_starts_grace() -> (
    ScopedFaultedSelection,
    SocketAddr,
    ServerHandle,
    tokio::net::TcpStream,
    Arc<tokio::sync::Semaphore>,
) {
    let (owners, addr, handle, peer, release) = retained_server(mock::faulted_selection).await;
    prepare_faulted_task(&owners, addr, ServerTaskFault::PanicNextOwnedTask).await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "owned panic task selection timed out",
    )
    .await;
    apply_selected(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "owned panic grace boundary timed out",
    )
    .await;
    (owners, addr, handle, peer, release)
}

async fn select_deadline(owners: &impl Owns<ServerStopController>) {
    select_next(
        owners,
        ServerStopEdge::SupervisorSelectedDeadline,
        "grace deadline selection timed out",
    )
    .await;
}

async fn release_deadline_and_wait_for_drained_result(owners: &impl Owns<ServerStopController>) {
    let stop = owners.owner();
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    stop.release(ServerStopEdge::SupervisorSelectedDeadline)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::AfterSupervisorResultSend,
        "deadline owned-task drain timed out",
    )
    .await;
}

// 1.T12: deadline beats newly-ready captured runtime shutdown.
#[tokio::test(start_paused = true)]
async fn deadline_branch_wins_over_runtime() {
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let (owners, _addr, handle, peer, _release) = pause_after_owned_panic_starts_grace().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    runtime::request_shutdown();
    select_deadline(&owners).await;
    release_deadline_and_wait_for_drained_result(&owners).await;
    assert_eof_with_socket_deadline(peer);
    owners.stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    assert_timeout(handle.await);
}

// 1.T12: deadline beats a real successful accept waiting in the listener.
#[tokio::test(start_paused = true)]
async fn deadline_branch_wins_over_accept_success() {
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let (owners, addr, handle, peer, _release) = retained_server(mock::faulted_selection).await;
    prepare_faulted_task(&owners, addr, ServerTaskFault::PanicNextOwnedTask).await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "deadline-over-accept task selection timed out",
    )
    .await;
    let waiting = connect_request(addr).await;
    apply_selected(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "deadline-over-accept grace boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(30)).await;
    select_deadline(&owners).await;
    release_deadline_and_wait_for_drained_result(&owners).await;
    assert_connection_closed_with_socket_deadline(waiting);
    assert_eof_with_socket_deadline(peer);
    owners.stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    assert_timeout(handle.await);
}

// 1.T12: deadline beats an injected accept error, which remains unobserved.
#[tokio::test(start_paused = true)]
async fn deadline_branch_wins_over_accept_error() {
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let (owners, _addr, handle, peer, _release) = pause_after_owned_panic_starts_grace().await;
    owners.connections
        .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    select_deadline(&owners).await;
    release_deadline_and_wait_for_drained_result(&owners).await;
    assert_eof_with_socket_deadline(peer);
    owners.stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    assert_timeout(handle.await);
}

// 1.T12: deadline beats a completed owned task and that handle is reaped next.
#[tokio::test(start_paused = true)]
async fn deadline_branch_wins_over_completed_task_then_reaps_it() {
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let (owners, _addr, handle, mut peer, release) =
        pause_after_owned_panic_starts_grace().await;
    release_and_drain_peer(&release, &mut peer, b"released").await;
    tokio::time::advance(Duration::from_secs(30)).await;
    select_deadline(&owners).await;
    observe_deferred_task_reap(&owners, ServerStopEdge::SupervisorSelectedDeadline).await;
    assert_timeout(handle.await);
}

// 1.T12: deadline beats a pending permit when the semaphore becomes ready.
#[test]
fn deadline_branch_wins_over_pending_permit_becoming_ready() {
    runtime::builder()
        .connection_limit(1)
        .shutdown_timeout(Duration::ZERO)
        .run(|| {
            runtime::block_on(async {
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let release = Arc::new(tokio::sync::Semaphore::new(0));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let owners = mock::supervisor_selection(addr).unwrap();
                let handle = serve_held(
                    listener,
                    held_router(
                        entered_tx,
                        Arc::clone(&release),
                        Arc::new(AtomicBool::new(false)),
                    ),
                );
                let mut first = connect_request(addr).await;
                await_handler_entry(entered_rx, "first request did not enter the handler").await;
                owners.connections.pause_once(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                let mut waiting = connect_request(addr).await;
                wait_until_paused_bounded(
                    &owners,
                    ConnectionOwnerEdge::PermitWaitPending,
                    "second request did not reach the pending permit wait",
                )
                .await;
                owners.stop.pause_once(ServerStopEdge::SupervisorSelectedRuntime)
                    .unwrap();
                runtime::request_shutdown();
                owners.connections.release(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                wait_until_paused_bounded(
                    &owners,
                    ServerStopEdge::SupervisorSelectedRuntime,
                    "supervisor did not select runtime shutdown",
                )
                .await;
                apply_selected(
                    &owners,
                    ServerStopEdge::SupervisorSelectedRuntime,
                    "supervisor did not reach the deadline/permit selection boundary",
                )
                .await;
                select_next(
                    &owners,
                    ServerStopEdge::SupervisorSelectedDeadline,
                    "supervisor did not select the configured deadline",
                )
                .await;
                // Make the permit ready while the selected deadline branch is
                // paused but before that branch applies abort.
                release_and_drain_peer(&release, &mut first, b"released").await;
                owners.stop.release(ServerStopEdge::SupervisorSelectedDeadline)
                    .unwrap();
                assert_connection_closed(&mut waiting).await;
                assert_timeout(handle.await);
            });
        })
        .unwrap();
}

// 1.T12: deadline beats a submitted registration, which is rejected and joined.
#[cfg(feature = "ws")]
#[tokio::test(start_paused = true)]
async fn deadline_branch_wins_over_submitted_registration_and_joins_it() {
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::registration_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, websocket_router())
        .expect("owned server requires a Tokio runtime");
    let mut client = prepare_submitted_upgrade(&owners, addr).await;
    owners.stop.pause_once(ServerStopEdge::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::SupervisorSelectedRuntime,
        "registration deadline runtime selection timed out",
    )
    .await;
    apply_selected(
        &owners,
        ServerStopEdge::SupervisorSelectedRuntime,
        "registration deadline grace boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(30)).await;
    select_deadline(&owners).await;
    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    apply_selected_event_then_release_transfer(
        &owners,
        ServerStopEdge::SupervisorSelectedDeadline,
        "deadline was not applied before releasing the submitted upgrade",
    )
    .await;
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 503"));
    assert!(response.to_ascii_lowercase().contains("connection: close"));
    assert_refusal_body_then_eof_with_independent_deadline(client, UNAVAILABLE_BODY).await;
    assert_timeout(completion.await);
}

// 1.T12: requests after the terminal send cannot replace the sent result.
#[camber::test]
async fn control_after_result_send_does_not_replace_io_result() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervisor_selection(addr).unwrap();
    owners.stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    owners.stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    owners.connections
        .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::BeforeSupervisorSelect,
        "post-result-send initial boundary timed out",
    )
    .await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "post-result-send accept selection timed out",
    )
    .await;
    owners.stop.release(ServerStopEdge::SupervisorSelectedAccept)
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::AfterSupervisorResultSend,
        "post-result-send terminal send timed out",
    )
    .await;
    handle.cancel();
    handle.cancel();
    runtime::request_shutdown();
    owners.stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    assert_io_kind(handle.await, std::io::ErrorKind::Other);
}

// 1.T12: Ok task completion while Graceful remains a successful server result.
#[camber::test]
async fn completed_task_in_graceful_mode_is_reaped_before_success() {
    let (stop, _addr, handle, mut peer, release) = retained_server(mock::server_stop).await;
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "graceful reap initial boundary timed out",
    )
    .await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedRuntime,
        "graceful reap runtime selection timed out",
    )
    .await;
    apply_selected(
        &stop,
        ServerStopEdge::SupervisorSelectedRuntime,
        "graceful reap post-runtime boundary timed out",
    )
    .await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedTask)
        .unwrap();
    release.add_permits(1);
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedTask,
        "graceful reap task selection timed out",
    )
    .await;
    drain_peer(&mut peer, b"released").await;
    assert!(handle.await.is_ok());
}

// 1.T12: CancelNextOwnedTask remains unexpected after runtime Graceful.
#[camber::test]
async fn unexpected_owned_task_cancellation_while_graceful_is_fatal() {
    let (owners, addr, handle, mut peer, release) = retained_server(mock::faulted_selection).await;
    prepare_faulted_task(&owners, addr, ServerTaskFault::CancelNextOwnedTask).await;
    owners.stop.pause_once(ServerStopEdge::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::SupervisorSelectedRuntime,
        "graceful cancellation runtime selection timed out",
    )
    .await;
    observe_deferred_task_reap(&owners, ServerStopEdge::SupervisorSelectedRuntime).await;
    release_and_drain_peer(&release, &mut peer, b"released").await;
    assert_task_panicked(handle.await, "owned HTTP task cancelled unexpectedly");
}

// The upgrade ticket is armed, written, and carried through the accept branch
// to the supervisor's select boundary. Only what happens to the permit after
// that separates the two callers.
#[cfg(feature = "ws")]
async fn submit_upgrade_through_accept(
    owners: &(impl Owns<ServerStopController> + Owns<UpgradeOwnerController>),
    addr: SocketAddr,
) -> tokio::net::TcpStream {
    submit_offered_upgrade_through_accept(
        owners,
        addr,
        &common::ws_upgrade_request("/ws"),
        UpgradeOwnerEdge::BeforeTransferAcknowledge,
    )
    .await
}

// The same carry, for a case whose claim is about what the handshake offered.
//
// Both the request and the edge the offer is held at are parameters rather than
// second copies of this sequence: an offer only changes what negotiation settles
// on, and the edge only changes which side of the answer a case reads, while how
// the connection reaches the offer never changes. A copy is where those could
// drift apart.
#[cfg(feature = "ws")]
async fn submit_offered_upgrade_through_accept(
    owners: &(impl Owns<ServerStopController> + Owns<UpgradeOwnerController>),
    addr: SocketAddr,
    request: &str,
    held: UpgradeOwnerEdge,
) -> tokio::net::TcpStream {
    let stop = Owns::<ServerStopController>::owner(owners);
    stop.pause_once(ServerStopEdge::SupervisorSelectedAccept)
        .unwrap();
    Owns::<UpgradeOwnerController>::owner(owners)
        .pause_once(held)
        .unwrap();
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(request.as_bytes()).await.unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedAccept,
        "upgrade accept selection timed out",
    )
    .await;
    apply_selected(
        owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "upgrade post-accept boundary timed out",
    )
    .await;
    client
}

#[cfg(feature = "ws")]
async fn prepare_submitted_upgrade(
    owners: &(impl Owns<ServerStopController> + Owns<UpgradeOwnerController>),
    addr: SocketAddr,
) -> tokio::net::TcpStream {
    prepare_offered_submitted_upgrade(owners, addr, &common::ws_upgrade_request("/ws")).await
}

#[cfg(feature = "ws")]
async fn prepare_offered_submitted_upgrade(
    owners: &(impl Owns<ServerStopController> + Owns<UpgradeOwnerController>),
    addr: SocketAddr,
    request: &str,
) -> tokio::net::TcpStream {
    let client = submit_offered_upgrade_through_accept(
        owners,
        addr,
        request,
        UpgradeOwnerEdge::BeforeTransferAcknowledge,
    )
    .await;
    wait_until_paused_bounded(
        owners,
        UpgradeOwnerEdge::BeforeTransferAcknowledge,
        "offered upgrade transfer edge timed out",
    )
    .await;
    client
}

#[cfg(feature = "ws")]
async fn prepare_submitted_upgrade_with_limit(
    owners: &(impl Owns<ServerStopController> + Owns<UpgradeOwnerController>),
    addr: SocketAddr,
) -> tokio::net::TcpStream {
    let client = submit_upgrade_through_accept(owners, addr).await;
    select_next(
        owners,
        ServerStopEdge::SupervisorSelectedPermit,
        "limited upgrade permit selection timed out",
    )
    .await;
    apply_selected(
        owners,
        ServerStopEdge::SupervisorSelectedPermit,
        "limited upgrade post-permit boundary timed out",
    )
    .await;
    wait_until_paused_bounded(
        owners,
        UpgradeOwnerEdge::BeforeTransferAcknowledge,
        "limited upgrade transfer edge timed out",
    )
    .await;
    client
}

// 1.T12: submitted pending-registration cancellation is connection-local.
#[cfg(feature = "ws")]
#[camber::test]
async fn pending_registration_cancellation_is_expected_and_joined() {
    let mut router = websocket_router();
    router.get("/ok", |_request: &Request| async {
        Response::text(200, "ok")
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::registration_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, router)
        .expect("owned server requires a Tokio runtime");
    let client = prepare_submitted_upgrade(&owners, addr).await;
    drop(client);
    owners.upgrades
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .unwrap();
    owners.stop
        .release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    // The connection that offered the child is the only owner that can end it,
    // and a server still serving is the proof that it did so without taking
    // anything else down with it.
    assert_ok_request_path(addr, "/ok").await;
    runtime::request_shutdown();
    assert!(handle.await.is_ok());
}

// 1.T12: control wins over a real buffered registration and rejects it with 503.
#[cfg(feature = "ws")]
#[camber::test]
async fn control_wins_over_submitted_registration_and_joins_wrapper() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::registration_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, websocket_router())
        .expect("owned server requires a Tokio runtime");
    let mut client = prepare_submitted_upgrade(&owners, addr).await;
    // The command commits its phase before it returns, so the answer this
    // connection gives after the release reads a server that has already
    // stopped admitting.
    handle.cancel();
    release_transfer_edge_and_resume(&owners);
    assert_refused_upgrade_wire(&mut client, "the control-refused upgrade").await;
    assert_cancelled(handle.await);
}

// 1.T12: runtime wins over a real buffered registration and rejects it with 503.
#[cfg(feature = "ws")]
#[camber::test]
async fn runtime_wins_over_submitted_registration_and_joins_wrapper() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::registration_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, websocket_router())
        .expect("owned server requires a Tokio runtime");
    let mut client = prepare_submitted_upgrade(&owners, addr).await;
    owners.stop
        .pause_once(ServerStopEdge::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    owners.stop
        .release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::SupervisorSelectedRuntime,
        "runtime shutdown selection timed out",
    )
    .await;
    apply_selected_event_then_release_transfer(
        &owners,
        ServerStopEdge::SupervisorSelectedRuntime,
        "runtime shutdown was not applied before releasing the offered upgrade",
    )
    .await;
    assert_refused_upgrade_wire(&mut client, "the runtime-refused upgrade").await;
    assert!(handle.await.is_ok());
}

// 1.T12: a real successful accept wins over a submitted registration ticket.
#[cfg(feature = "ws")]
#[camber::test]
async fn accept_branch_wins_over_submitted_registration() {
    let mut router = websocket_router();
    router.get("/", |_request: &Request| async {
        Response::text(200, "accepted")
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::registration_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, router)
        .expect("owned server requires a Tokio runtime");
    let mut upgrade = prepare_submitted_upgrade(&owners, addr).await;
    let mut ordinary = connect_request(addr).await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "ordinary accept selection over registration timed out",
    )
    .await;
    apply_selected(
        &owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "ordinary accept post-selection boundary timed out",
    )
    .await;
    let response = read_http_head(&mut ordinary).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    owners.stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    handle.cancel();
    owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::SupervisorSelectedControl,
        "registration forced control selection timed out",
    )
    .await;
    apply_selected_event_then_release_transfer(
        &owners,
        ServerStopEdge::SupervisorSelectedControl,
        "forced control was not applied before releasing the deferred upgrade",
    )
    .await;
    assert_refused_upgrade_wire(&mut upgrade, "the accept-deferred refused upgrade").await;
    assert_cancelled(handle.await);
}

#[cfg(feature = "ws")]
struct PermitRegistrationFixture {
    owners: ScopedSupervisedRegistration,
    handle: ServerHandle,
    release: Arc<tokio::sync::Semaphore>,
    ordinary: tokio::net::TcpStream,
    upgrade: tokio::net::TcpStream,
    waiting: tokio::net::TcpStream,
}

#[cfg(feature = "ws")]
async fn permit_registration_fixture() -> PermitRegistrationFixture {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut router = held_router(
        entered_tx,
        Arc::clone(&release),
        Arc::new(AtomicBool::new(false)),
    );
    attach_drain_ws(&mut router);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervised_registration(addr).unwrap();
    let handle = serve_held(listener, router);
    let mut ordinary =
        tokio::time::timeout(Duration::from_secs(5), tokio::net::TcpStream::connect(addr))
            .await
            .expect("ordinary connection timed out")
            .unwrap();
    ordinary.write_all(CLOSE_REQUEST).await.unwrap();
    await_handler_entry(entered_rx, "ordinary request dispatch timed out").await;
    let upgrade = tokio::time::timeout(
        Duration::from_secs(5),
        prepare_submitted_upgrade_with_limit(&owners, addr),
    )
    .await
    .expect("upgrade ticket submission timed out");
    let waiting = tokio::time::timeout(Duration::from_secs(5), connect_request(addr))
        .await
        .expect("permit-waiting connection timed out");
    PermitRegistrationFixture {
        owners,
        handle,
        release,
        ordinary,
        upgrade,
        waiting,
    }
}

#[cfg(feature = "ws")]
async fn select_permit_over_registration(fixture: &mut PermitRegistrationFixture) {
    select_next(
        &fixture.owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "permit-waiting accept selection timed out",
    )
    .await;
    apply_selected(
        &fixture.owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "pre-permit supervisor boundary timed out",
    )
    .await;
    wait_until_paused_bounded(
        &fixture.owners,
        UpgradeOwnerEdge::BeforeTransferAcknowledge,
        "permit-waiting upgrade transfer edge timed out",
    )
    .await;
    fixture
        .owners.stop.pause_once(ServerStopEdge::SupervisorSelectedPermit)
        .unwrap();
    fixture
        .owners.connections.pause_once(ConnectionOwnerEdge::AfterConnectionFutureCompleted)
        .unwrap();
    release_and_drain_peer(&fixture.release, &mut fixture.ordinary, b"released").await;
    wait_until_paused_bounded(
        &fixture.owners,
        ConnectionOwnerEdge::AfterConnectionFutureCompleted,
        "ordinary connection completion timed out",
    )
    .await;
    fixture
        .owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &fixture.owners,
        ServerStopEdge::SupervisorSelectedPermit,
        "permit selection timed out",
    )
    .await;
    fixture
        .owners.connections.release(ConnectionOwnerEdge::AfterConnectionFutureCompleted)
        .unwrap();
    apply_selected(
        &fixture.owners,
        ServerStopEdge::SupervisorSelectedPermit,
        "post-permit supervisor boundary timed out",
    )
    .await;
    fixture.release.add_permits(1);
    assert!(
        read_http_head_bounded(&mut fixture.waiting, "permit-waiting response timed out")
            .await
            .starts_with("HTTP/1.1 200")
    );
}

#[cfg(feature = "ws")]
async fn reject_deferred_upgrade(mut fixture: PermitRegistrationFixture) {
    fixture
        .owners.stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    fixture.handle.cancel();
    fixture
        .owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &fixture.owners,
        ServerStopEdge::SupervisorSelectedControl,
        "control selection timed out",
    )
    .await;
    apply_selected_event_then_release_transfer(
        &fixture.owners,
        ServerStopEdge::SupervisorSelectedControl,
        "forced control was not applied before releasing the deferred upgrade",
    )
    .await;
    assert_refused_upgrade_wire(&mut fixture.upgrade, "the permit-deferred refused upgrade").await;
    let result = tokio::time::timeout(Duration::from_secs(5), fixture.handle.into_future())
        .await
        .expect("permit fixture owner finalization timed out");
    assert_cancelled(result);
}

// 1.T12: a real permit becoming ready wins over a submitted registration.
#[cfg(feature = "ws")]
#[test]
fn permit_branch_wins_over_submitted_registration() {
    runtime::builder()
        .connection_limit(2)
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let mut fixture = permit_registration_fixture().await;
                select_permit_over_registration(&mut fixture).await;
                reject_deferred_upgrade(fixture).await;
            });
        })
        .unwrap();
}

// 1.T12: a ready permit beats completion of the task that released it.
#[test]
fn permit_branch_wins_over_completed_task() {
    runtime::builder()
        .connection_limit(1)
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let owners = mock::supervisor_selection(addr).unwrap();
                let handle = camber::http::serve_background(listener, ok_router())
                    .expect("owned server requires a Tokio runtime");
                let mut first = connect_request(addr).await;
                assert!(read_http_head(&mut first).await.starts_with("HTTP/1.1 200"));
                owners.connections.pause_once(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                let mut second = connect_request(addr).await;
                wait_until_paused_bounded(
                    &owners,
                    ConnectionOwnerEdge::PermitWaitPending,
                    "second connection did not wait for a permit",
                )
                .await;
                owners.stop.pause_once(ServerStopEdge::SupervisorSelectedPermit)
                    .unwrap();
                drop(first);
                owners.connections.release(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                wait_and_release_bounded(
                    &owners,
                    ServerStopEdge::SupervisorSelectedPermit,
                    "permit selection over a completed task timed out",
                )
                .await;
                assert!(
                    read_http_head(&mut second)
                        .await
                        .starts_with("HTTP/1.1 200")
                );
                handle.cancel();
                assert_cancelled(handle.await);
            });
        })
        .unwrap();
}

#[cfg(feature = "ws")]
async fn submit_transfer_over_completed_task(
    owners: &(impl Owns<ServerStopController> + Owns<UpgradeOwnerController>),
    addr: SocketAddr,
) -> tokio::net::TcpStream {
    let stop = Owns::<ServerStopController>::owner(owners);
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(common::ws_upgrade_request("/ws").as_bytes())
        .await
        .unwrap();
    stop.pause_once(ServerStopEdge::SupervisorSelectedAccept)
        .unwrap();
    Owns::<UpgradeOwnerController>::owner(owners)
        .pause_once(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .unwrap();
    stop.release(ServerStopEdge::BeforeSupervisorSelect).unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedAccept,
        "transfer-over-task accept selection timed out",
    )
    .await;
    apply_selected(
        owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "transfer-over-task post-accept boundary timed out",
    )
    .await;
    wait_until_paused_bounded(
        owners,
        UpgradeOwnerEdge::BeforeTransferAcknowledge,
        "transfer-over-task transfer edge timed out",
    )
    .await;
    client
}

// 1.T12: a connection's own transfer proceeds while a finished connection waits
// to be reaped.
#[cfg(feature = "ws")]
#[camber::test]
async fn connection_transfer_proceeds_over_completed_connection() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervised_registration(addr).unwrap();
    let mut router = websocket_router();
    router.get("/", |_request: &Request| async {
        Response::text(200, "ok")
    });
    let handle = camber::http::serve_background(listener, router)
        .expect("owned server requires a Tokio runtime");
    prepare_completed_task(&owners, addr).await;

    let mut client = submit_transfer_over_completed_task(&owners, addr).await;
    // The transfer is the connection's own act, so it answers while the
    // supervisor is still holding a finished connection it has not reaped. The
    // reap that follows takes the completed owner and leaves the transferring
    // one alone.
    owners
        .upgrades
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .unwrap();
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedTask,
        "transfer-over-task reap selection timed out",
    )
    .await;
    owners
        .stop
        .release(ServerStopEdge::SupervisorSelectedTask)
        .unwrap();
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 101"));
    drop(client);
    runtime::request_shutdown();
    assert!(handle.await.is_ok());
}

// 1.T2, 1.T13
#[test]
fn background_constructor_checks_tokio_before_stale_camber_marker() {
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    std_listener.set_nonblocking(true).unwrap();
    let tokio_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    // The stale Camber marker: a context established on this thread that
    // outlives the Tokio runtime it was established under.
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let listener =
        tokio_runtime.block_on(async { tokio::net::TcpListener::from_std(std_listener).unwrap() });
    drop(tokio_runtime);

    // The absent executor has its own variant, and the terminal answers it
    // synchronously: a stale Camber marker must not turn a missing Tokio
    // runtime into an argument complaint, and no owner is handed back for a
    // server that never started.
    match camber::http::serve_background(listener, ok_router()) {
        Err(RuntimeError::NoRuntime) => {}
        Ok(_) => panic!("the terminal handed back an owner with no Tokio runtime"),
        Err(other) => panic!("expected NoRuntime, got {other:?}"),
    }
}

async fn capture_standalone_default_keepalive(
    owners: &impl Owns<ConnectionOwnerController>,
    addr: SocketAddr,
) {
    let connections = owners.owner();
    connections
        .pause_once(ConnectionOwnerEdge::AfterAccept)
        .unwrap();
    connections
        .pause_once(ConnectionOwnerEdge::AfterPermit)
        .unwrap();
    let keepalive_checkpoint =
        ConnectionOwnerEdge::HeaderTimeoutConfigured(Duration::from_secs(60));
    connections.pause_once(keepalive_checkpoint).unwrap();
    let mut partial = tokio::net::TcpStream::connect(addr).await.unwrap();
    partial.write_all(b"GET / HTTP/1.1\r\nHost:").await.unwrap();
    wait_until_paused_bounded(
        &connections,
        ConnectionOwnerEdge::AfterAccept,
        "standalone default accept pause timed out",
    )
    .await;
    connections
        .release(ConnectionOwnerEdge::AfterAccept)
        .unwrap();
    wait_until_paused_bounded(
        &connections,
        ConnectionOwnerEdge::AfterPermit,
        "standalone default permit pause timed out",
    )
    .await;
    connections
        .release(ConnectionOwnerEdge::AfterPermit)
        .unwrap();
    wait_and_release_bounded(
        &connections,
        keepalive_checkpoint,
        "standalone default keepalive timeout was not captured",
    )
    .await;
    drop(partial);
}

// 1.T13
#[tokio::test(start_paused = true)]
async fn standalone_background_ignores_unrelated_camber_shutdown_and_uses_defaults() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervisor_selection(addr).unwrap();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut router = held_router(entered_tx, release, Arc::new(AtomicBool::new(false)));
    router.get("/second", |_req: &Request| async {
        Response::text(200, "second")
    });
    let handle = serve_held(listener, router);
    let _first = connect_request(addr).await;
    await_handler_entry(
        entered_rx,
        "standalone default request did not enter the handler",
    )
    .await;

    std::thread::spawn(runtime::request_shutdown)
        .join()
        .unwrap();
    assert_ok_request_path(addr, "/second").await;
    let health = reqwest::get(format!("http://{addr}/health")).await.unwrap();
    assert_eq!(health.status(), 404);
    drop(health);
    let metrics = reqwest::get(format!("http://{addr}/metrics"))
        .await
        .unwrap();
    assert_eq!(metrics.status(), 404);
    drop(metrics);
    #[cfg(feature = "profiling")]
    {
        let profiling = reqwest::get(format!("http://{addr}/debug/pprof/cpu?seconds=0"))
            .await
            .unwrap();
        assert_eq!(profiling.status(), 404);
    }

    capture_standalone_default_keepalive(&owners, addr).await;

    // The checkpoints order the injected accept error against the supervisor's
    // observation of it, so the default grace deadline is already installed
    // before the clock moves.
    owners
        .stop
        .pause_once(ServerStopEdge::SupervisorSelectedAccept)
        .unwrap();
    owners
        .connections
        .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "standalone default supervisor did not select the injected accept error",
    )
    .await;
    apply_selected(
        &owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "standalone default grace deadline was not installed",
    )
    .await;
    let mut completion = Box::pin(handle.into_future());
    owners
        .stop
        .release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    tokio::time::advance(Duration::from_secs(30) - Duration::from_millis(1)).await;
    assert!(completion.as_mut().now_or_never().is_none());
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_timeout(completion.await);
}

async fn assert_ok_request_path(addr: SocketAddr, path: &str) {
    let response = reqwest::get(format!("http://{addr}{path}")).await.unwrap();
    assert_eq!(response.status(), 200);
}

struct HealthyResource;

impl camber::Resource for HealthyResource {
    fn name(&self) -> &str {
        "owned-lifecycle"
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        Ok(())
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        Ok(())
    }
}

// 1.T13
#[test]
fn configured_background_participates_in_runtime_task_accounting() {
    let run_returned = Arc::new(AtomicBool::new(false));
    let supervisor_was_paused = Arc::new(AtomicBool::new(false));
    let release_thread = Arc::new(Mutex::new(None));
    runtime::builder()
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            let listener = runtime::block_on(tokio::net::TcpListener::bind("127.0.0.1:0")).unwrap();
            let owners = mock::supervisor_selection(listener.local_addr().unwrap()).unwrap();
            owners
                .stop
                .pause_once(ServerStopEdge::BeforeSupervisorSelect)
                .unwrap();
            owners
                .connections
                .inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
                .unwrap();
            let _handle = camber::http::serve_background(listener, ok_router())
                .expect("owned server requires a Tokio runtime");
            let returned = Arc::clone(&run_returned);
            let paused = Arc::clone(&supervisor_was_paused);
            let thread = std::thread::spawn(move || {
                let tokio_runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                tokio_runtime.block_on(async {
                    wait_until_paused_bounded(
                        &owners,
                        ServerStopEdge::BeforeSupervisorSelect,
                        "task-accounting supervisor boundary timed out",
                    )
                    .await;
                    assert!(!returned.load(Ordering::Acquire));
                    paused.store(true, Ordering::Release);
                    owners
                        .stop
                        .release(ServerStopEdge::BeforeSupervisorSelect)
                        .unwrap();
                });
            });
            *release_thread
                .lock()
                .unwrap_or_else(|error| error.into_inner()) = Some(thread);
        })
        .unwrap();
    run_returned.store(true, Ordering::Release);
    release_thread
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .unwrap()
        .join()
        .unwrap();
    assert!(supervisor_was_paused.load(Ordering::Acquire));
}

// 1.T13
#[test]
fn configured_background_captures_limit_keepalive_health_metrics_and_shutdown() {
    runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_millis(150))
        .shutdown_timeout(Duration::from_secs(1))
        .with_metrics()
        .resource(HealthyResource)
        .run(|| {
            runtime::block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let connections = mock::connection_owner(addr).unwrap();
                let handle = camber::http::serve_background(listener, ok_router())
                    .expect("owned server requires a Tokio runtime");
                let health = reqwest::get(format!("http://{addr}/health")).await.unwrap();
                assert_eq!(health.status(), 200);
                drop(health);
                let metrics = reqwest::get(format!("http://{addr}/metrics"))
                    .await
                    .unwrap();
                assert_eq!(metrics.status(), 200);
                drop(metrics);

                connections
                    .pause_once(ConnectionOwnerEdge::AfterPermit)
                    .unwrap();
                let mut partial = tokio::net::TcpStream::connect(addr).await.unwrap();
                wait_until_paused_bounded(
                    &connections,
                    ConnectionOwnerEdge::AfterPermit,
                    "captured keepalive permit pause timed out",
                )
                .await;
                connections
                    .release(ConnectionOwnerEdge::AfterPermit)
                    .unwrap();
                partial.write_all(b"GET / HTTP/1.1\r\nHost:").await.unwrap();
                connections
                    .pause_once(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                connections
                    .pause_once(ConnectionOwnerEdge::AfterPermit)
                    .unwrap();
                let mut blocked = connect_request(addr).await;
                // The pending-permit checkpoint separates "held by the captured
                // connection limit" from "the response has merely not arrived".
                wait_until_paused_bounded(
                    &connections,
                    ConnectionOwnerEdge::PermitWaitPending,
                    "connection limit did not block a second dispatch",
                )
                .await;
                connections
                    .release(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                tokio::time::timeout(Duration::from_secs(1), assert_eof(&mut partial))
                    .await
                    .expect("configured keepalive timeout was not captured");
                wait_and_release_bounded(
                    &connections,
                    ConnectionOwnerEdge::AfterPermit,
                    "the released permit did not admit the blocked connection",
                )
                .await;
                let response = read_http_head(&mut blocked).await;
                assert!(response.starts_with("HTTP/1.1 200"));

                runtime::request_shutdown();
                assert!(handle.await.is_ok());
            });
        })
        .unwrap();
}

// 1.T13
#[test]
fn configured_background_captures_router_body_buffers() {
    runtime::builder()
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let mut router = Router::new().max_request_body(4);
                router.post("/", |_req: &Request| async {
                    Response::text(200, "unexpected")
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let handle = camber::http::serve_background(listener, router)
                    .expect("owned server requires a Tokio runtime");
                let response = reqwest::Client::new()
                    .post(format!("http://{addr}/"))
                    .body("12345")
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status(), 413);
                runtime::request_shutdown();
                assert!(handle.await.is_ok());
            });
        })
        .unwrap();
}

async fn assert_configured_sse_buffer(
    owners: &impl Owns<ConnectionOwnerController>,
    addr: SocketAddr,
) -> tokio::net::TcpStream {
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .unwrap();
    wait_and_release_bounded(
        owners,
        ConnectionOwnerEdge::SseBufferConfigured(3),
        "captured SSE buffer size timed out",
    )
    .await;
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    let event = tokio::time::timeout(Duration::from_secs(5), async {
        let mut event = Vec::new();
        let mut chunk = [0u8; 128];
        loop {
            let read = client.read(&mut chunk).await.unwrap();
            assert_ne!(read, 0);
            event.extend_from_slice(&chunk[..read]);
            if event
                .windows(b"data: sse-ok".len())
                .any(|part| part == b"data: sse-ok")
            {
                return event;
            }
        }
    })
    .await
    .expect("SSE event exchange timed out");
    let event = String::from_utf8_lossy(&event);
    assert!(event.contains("event: configured"));
    client
}

// 1.T13
#[test]
fn configured_background_captures_sse_and_websocket_buffers() {
    runtime::builder()
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let mut router = Router::new().sse_buffer_size(3);
                router.get_sse("/events", |_request: &Request, writer: &mut SseWriter| {
                    writer.event("configured", "sse-ok")
                });
                #[cfg(feature = "ws")]
                {
                    use camber::http::WsConn;

                    router = router.ws_buffer_size(5);
                    router.ws(
                        "/buffered-ws",
                        |_request: &Request, mut connection: WsConn| {
                            if let Some(message) = connection.recv() {
                                connection.send(&message)?;
                            }
                            Ok(())
                        },
                    );
                }

                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let connections = mock::connection_owner(addr).unwrap();
                connections
                    .pause_once(ConnectionOwnerEdge::SseBufferConfigured(3))
                    .unwrap();
                #[cfg(feature = "ws")]
                {
                    connections
                        .pause_once(ConnectionOwnerEdge::WebSocketOutgoingBufferConfigured(5))
                        .unwrap();
                    connections
                        .pause_once(ConnectionOwnerEdge::WebSocketIncomingBufferConfigured(5))
                        .unwrap();
                }
                let handle = camber::http::serve_background(listener, router)
                    .expect("owned server requires a Tokio runtime");
                let sse_client = assert_configured_sse_buffer(&connections, addr).await;

                #[cfg(feature = "ws")]
                {
                    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
                    client
                        .write_all(common::ws_upgrade_request("/buffered-ws").as_bytes())
                        .await
                        .unwrap();
                    wait_and_release_bounded(
                        &connections,
                        ConnectionOwnerEdge::WebSocketOutgoingBufferConfigured(5),
                        "captured outgoing WebSocket buffer size timed out",
                    )
                    .await;
                    wait_and_release_bounded(
                        &connections,
                        ConnectionOwnerEdge::WebSocketIncomingBufferConfigured(5),
                        "captured incoming WebSocket buffer size timed out",
                    )
                    .await;
                    let response = read_http_head(&mut client).await;
                    assert!(response.starts_with("HTTP/1.1 101"));
                    let echo = "the buffered bridge's echo";
                    common::write_async_ws_frame(&mut client, 0x01, b"ws-ok", echo).await;
                    let framed = common::read_async_ws_frame_or_eof(&mut client, echo)
                        .await
                        .expect("the buffered bridge gave its transport up without answering");
                    assert_eq!(framed, (0x01, Box::from(*b"ws-ok")));
                }
                drop(sse_client);
                runtime::request_shutdown();
                assert!(handle.await.is_ok());
            });
        })
        .unwrap();
}

// 1.T13
#[test]
fn configured_tls_background_marks_requests_as_tls() {
    let (tls_config, connector) = common::self_signed_server_and_connector();
    runtime::builder()
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let mut backend_router = Router::new();
                backend_router.get("/marker", |request: &Request| {
                    let forwarded_proto = request
                        .header("x-forwarded-proto")
                        .unwrap_or("missing")
                        .to_owned();
                    async move { Response::text(200, &forwarded_proto) }
                });
                let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let backend_addr = backend_listener.local_addr().unwrap();
                let backend = tokio::spawn(camber::http::serve_async(
                    backend_listener,
                    backend_router,
                ).expect("owned server requires a Tokio runtime"));

                let mut router = Router::new();
                router.proxy("/proxy", &format!("http://{backend_addr}"));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let handle = camber::http::serve_background_tls(listener, router, tls_config).expect("owned server requires a Tokio runtime");
                let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
                let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
                let mut stream = connector.connect(name, tcp).await.unwrap();
                stream
                    .write_all(
                        b"GET /proxy/marker HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .await
                    .unwrap();
                let response = read_http_head(&mut stream).await;
                assert!(response.starts_with("HTTP/1.1 200"));
                let mut body = [0u8; 5];
                stream.read_exact(&mut body).await.unwrap();
                assert_eq!(&body, b"https");
                runtime::request_shutdown();
                assert!(handle.await.is_ok());
                // The backend captured this runtime when `serve_async` returned
                // its owner, so the same shutdown request ends it.
                assert!(
                    tokio::time::timeout(Duration::from_secs(5), backend)
                        .await
                        .expect("the captured backend never ended on runtime shutdown")
                        .expect("the backend task failed")
                        .is_ok()
                );
            });
        })
        .unwrap();
}

// 1.T13
#[tokio::test]
async fn standalone_tls_background_marks_proxy_scheme_as_https() {
    let (tls_config, connector) = common::self_signed_server_and_connector();
    let mut backend_router = Router::new();
    backend_router.get("/marker", |request: &Request| {
        let forwarded_proto = request
            .header("x-forwarded-proto")
            .unwrap_or("missing")
            .to_owned();
        async move { Response::text(200, &forwarded_proto) }
    });
    let backend_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend_listener.local_addr().unwrap();
    let backend = tokio::spawn(
        camber::http::serve_async(backend_listener, backend_router)
            .expect("owned server requires a Tokio runtime"),
    );
    let mut router = Router::new();
    router.proxy("/proxy", &format!("http://{backend_addr}"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = mock::server_stop(addr).unwrap();
    let handle = camber::http::serve_background_tls(listener, router, tls_config)
        .expect("owned server requires a Tokio runtime");
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut stream = connector.connect(name, tcp).await.unwrap();
    stream
        .write_all(b"GET /proxy/marker HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    let response = read_http_head(&mut stream).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    let mut body = [0u8; 5];
    stream.read_exact(&mut body).await.unwrap();
    assert_eq!(&body, b"https");
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "standalone TLS supervisor boundary timed out",
    )
    .await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    handle.cancel();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedControl,
        "standalone TLS control selection timed out",
    )
    .await;
    assert_cancelled(handle.await);
    backend.abort();
    assert!(backend.await.unwrap_err().is_cancelled());
}

// 1.T13
#[test]
fn configured_background_captures_request_tracing() {
    // Two standing interests instead of one transcript: the binary's single
    // global subscriber is the shared capture bus, so each route is read on
    // its own name and a parallel test's events cannot answer for it.
    let standalone = common::capture_events("standalone-untraced");
    let configured = common::capture_events("captured-lifecycle");

    let standalone_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    standalone_runtime.block_on(async {
        let mut router = Router::new();
        router.get("/standalone-untraced", |_request: &Request| async {
            Response::text(200, "not traced")
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let stop = mock::server_stop(addr).unwrap();
        let handle = camber::http::serve_background(listener, router)
            .expect("owned server requires a Tokio runtime");
        assert_ok_request_path(addr, "/standalone-untraced").await;
        stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
            .unwrap();
        wait_until_paused_bounded(
            &stop,
            ServerStopEdge::BeforeSupervisorSelect,
            "untraced standalone supervisor boundary timed out",
        )
        .await;
        handle.cancel();
        stop.release(ServerStopEdge::BeforeSupervisorSelect)
            .unwrap();
        assert_cancelled(handle.await);
    });
    drop(standalone_runtime);

    runtime::builder()
        .with_tracing()
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let mut router = Router::new();
                router.get("/captured-lifecycle", |_request: &Request| async {
                    Response::text(200, "traced")
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let handle = camber::http::serve_background(listener, router)
                    .expect("owned server requires a Tokio runtime");
                assert_ok_request_path(addr, "/captured-lifecycle").await;
                runtime::request_shutdown();
                assert!(handle.await.is_ok());
            });
        })
        .unwrap();

    assert!(
        standalone.is_empty(),
        "a server outside the configured runtime recorded request tracing: {:?}",
        standalone.events()
    );
    assert!(
        configured.recorded(&["method=GET", "path=/captured-lifecycle", "status=200"]),
        "the configured runtime did not record its request: {:?}",
        configured.events()
    );
}

// 1.T13
#[cfg(feature = "profiling")]
#[test]
fn configured_background_captures_profiling_route() {
    runtime::builder()
        .with_profiling()
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let handle = camber::http::serve_background(listener, Router::new())
                    .expect("owned server requires a Tokio runtime");
                let response = reqwest::get(format!("http://{addr}/debug/pprof/cpu?seconds=1"))
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200);
                // Read the flamegraph out before asking for shutdown. A body
                // left unread sits in the socket, and a flamegraph large
                // enough to fill it holds the response mid-write against a
                // client that will never read again — the drain then spends
                // its whole budget on a stall this test did not set out to
                // measure.
                response.bytes().await.unwrap();
                runtime::request_shutdown();
                assert!(handle.await.is_ok());
            });
        })
        .unwrap();
}

// 1.T16
#[camber::test]
async fn request_shutdown_cannot_be_missed_after_runtime_wait_registration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stop = mock::server_stop(listener.local_addr().unwrap()).unwrap();
    stop.pause_once(ServerStopEdge::BeforeRuntimeWait)
        .unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeRuntimeWait,
        "request-shutdown runtime wait registration timed out",
    )
    .await;
    runtime::request_shutdown();
    stop.release(ServerStopEdge::BeforeRuntimeWait)
        .unwrap();
    assert!(handle.await.is_ok());
}

// 1.T16
#[camber::test]
async fn on_cancel_cannot_be_missed_after_runtime_wait_registration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stop = mock::server_stop(listener.local_addr().unwrap()).unwrap();
    stop.pause_once(ServerStopEdge::BeforeRuntimeWait)
        .unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeRuntimeWait,
        "on-cancel runtime wait registration timed out",
    )
    .await;
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    runtime::on_cancel(async move {
        let _ = cancel_rx.await;
    });
    cancel_tx.send(()).unwrap();
    stop.release(ServerStopEdge::BeforeRuntimeWait)
        .unwrap();
    assert!(handle.await.is_ok());
}

#[cfg(unix)]
const CHILD_PROTOCOL_ENV: &str = "CAMBER_OWNED_SERVER_CHILD_PROTOCOL";

#[cfg(unix)]
const PROTOCOL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
/// Spawn one protocol child, guarded.
///
/// The child is this same test binary re-entered at `lifecycle_signal_child`,
/// which is `#[ignore]`d, so the filter has to say so. Everything past that —
/// the piped streams, the line reader, the bounded reap, and the `Drop` that
/// runs both on the unwind path — is [`ChildGuard`]'s, and this suite keeps no
/// second copy of it.
///
/// The guard captures the child's stderr rather than letting it through to this
/// process's own. Every failure below quotes it, because a child that panicked
/// has no other way to say so from here.
fn spawn_protocol_child(protocol: &str) -> ChildGuard {
    let mut command = Command::new(std::env::current_exe().expect("this test binary has a path"));
    command
        .arg("--exact")
        .arg("lifecycle_signal_child")
        .arg("--ignored")
        .arg("--nocapture")
        .env(CHILD_PROTOCOL_ENV, protocol)
        .stdin(Stdio::null());
    ChildGuard::spawn(command, PROTOCOL_CLEANUP_TIMEOUT).expect("the protocol child could not start")
}

/// Take the reap probe and the identity it will report, before either is spent.
///
/// The identity has to be read here: a guard that has reaped its child has given
/// the handle up, and `id()` answers `0` from then on.
#[cfg(unix)]
fn protocol_reap_probe(child: &mut ChildGuard) -> (ReapProbe, u32) {
    let child_id = child.id();
    let probe = child
        .take_reap_probe()
        .expect("a freshly spawned guard owns its reap probe");
    (probe, child_id)
}

/// Wait for the child's `prefix` checkpoint, and hand back the line it printed.
///
/// A checkpoint that never comes ends the child first. The guard fills its
/// captured streams during that shutdown, so shutting down is what makes the
/// child's own words available to quote — reporting only the expired wait would
/// render a child that panicked as a child that would not start.
#[cfg(unix)]
fn protocol_line(child: &mut ChildGuard, prefix: &str, timeout: Duration) -> Box<str> {
    match child.await_line(prefix, timeout) {
        Ok(line) => line,
        Err(error) => {
            let _ = child.shutdown();
            panic!(
                "the protocol child never printed {prefix}: {error}\n{}",
                String::from_utf8_lossy(child.stderr())
            )
        }
    }
}

/// Wait out the child, and require the exit status it reported to be a success.
#[cfg(unix)]
fn assert_protocol_exit(child: &mut ChildGuard, protocol: &str, timeout: Duration) {
    let status = match child.wait_bounded(timeout) {
        Ok(status) => status,
        Err(error) => panic!(
            "{protocol} child did not exit: {error}\n{}",
            String::from_utf8_lossy(child.stderr())
        ),
    };
    assert!(
        status.success(),
        "{protocol} child failed: {status}\n{}",
        String::from_utf8_lossy(child.stderr())
    );
}

#[cfg(unix)]
fn parse_address(line: &str) -> SocketAddr {
    line.split_whitespace().last().unwrap().parse().unwrap()
}

#[cfg(unix)]
fn connect_control(addr: SocketAddr, token: &[u8], timeout: Duration) {
    let mut control = std::net::TcpStream::connect_timeout(&addr, timeout).unwrap();
    control.set_write_timeout(Some(timeout)).unwrap();
    control.write_all(token).unwrap();
}

#[cfg(unix)]
fn bounded_raw_request(addr: SocketAddr, timeout: Duration) -> Box<str> {
    let mut stream = std::net::TcpStream::connect_timeout(&addr, timeout).unwrap();
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream.set_write_timeout(Some(timeout)).unwrap();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut response = String::new();
    std::io::Read::read_to_string(&mut stream, &mut response).unwrap();
    response.into_boxed_str()
}

// 1.T16
#[cfg(unix)]
#[test]
fn active_runtime_signal_drains_owned_server_before_child_exit() {
    let mut child = spawn_protocol_child("active-signal");
    let (probe, child_id) = protocol_reap_probe(&mut child);
    let ready = protocol_line(&mut child, "SERVER_READY", Duration::from_secs(5));
    let control_addr = parse_address(&ready);
    connect_control(control_addr, b"RAISE", Duration::from_secs(5));
    protocol_line(&mut child, "DRAINED", Duration::from_secs(5));
    assert_protocol_exit(&mut child, "active-signal", Duration::from_secs(5));
    assert_protocol_cleanup(probe, child_id);
}

// 1.T16
#[cfg(unix)]
#[test]
fn closure_return_does_not_transition_owned_server() {
    let mut child = spawn_protocol_child("closure-return");
    let (probe, child_id) = protocol_reap_probe(&mut child);
    let returning = protocol_line(&mut child, "CLOSURE_RETURNING", Duration::from_secs(2));
    let mut fields = returning.split_whitespace();
    assert_eq!(fields.next(), Some("CLOSURE_RETURNING"));
    let http_addr: SocketAddr = fields.next().unwrap().parse().unwrap();
    let control_addr: SocketAddr = fields.next().unwrap().parse().unwrap();
    let response = bounded_raw_request(http_addr, Duration::from_secs(2));
    assert_eq!(common::status_from_raw(&response), 200);
    connect_control(control_addr, b"CANCEL", Duration::from_secs(2));
    protocol_line(&mut child, "AFTER_RUN", Duration::from_secs(2));
    assert_protocol_exit(&mut child, "closure-return", Duration::from_secs(2));
    assert_protocol_cleanup(probe, child_id);
}

// 1.T16
#[cfg(unix)]
#[test]
fn signal_after_watcher_teardown_only_guarantees_process_survival() {
    let mut child = spawn_protocol_child("post-watcher");
    let (probe, child_id) = protocol_reap_probe(&mut child);
    protocol_line(&mut child, "WATCHER_GONE", Duration::from_secs(5));
    protocol_line(&mut child, "SURVIVED", Duration::from_secs(5));
    assert_protocol_exit(&mut child, "post-watcher", Duration::from_secs(5));
    assert_protocol_cleanup(probe, child_id);
}

/// Require the guard to have reaped the child it was given, and no other.
///
/// One reading rather than the four this suite used to take. The guard sends its
/// reap only after joining both output readers, so a probe that answers at all
/// says the process was reaped, the readers were joined, and their streams
/// reached end of file; a cleanup that failed anywhere in that sequence arrives
/// here as the error instead.
#[cfg(unix)]
fn assert_protocol_cleanup(probe: ReapProbe, child_id: u32) {
    let reaped = probe
        .wait(PROTOCOL_CLEANUP_TIMEOUT)
        .expect("the protocol child's reap did not complete");
    assert_eq!(
        reaped.child_id(),
        child_id,
        "the reap probe reported a different child"
    );
}

#[cfg(unix)]
#[test]
fn protocol_child_timeout_reaps_process_and_joins_stdout() {
    let mut child = spawn_protocol_child("cleanup-hold");
    let (probe, child_id) = protocol_reap_probe(&mut child);
    protocol_line(&mut child, "CLEANUP_READY", Duration::from_secs(5));

    let result = child.wait_bounded(Duration::ZERO);
    assert!(
        matches!(result, Err(ProcessError::ExitTimeout { .. })),
        "a child that never exits owes the bound it missed: {result:?}"
    );
    assert_protocol_cleanup(probe, child_id);
}

#[cfg(unix)]
#[test]
fn protocol_child_assertion_unwind_reaps_process_and_joins_stdout() {
    let mut child = spawn_protocol_child("cleanup-hold");
    let (probe, child_id) = protocol_reap_probe(&mut child);
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut child = child;
        protocol_line(&mut child, "CLEANUP_READY", Duration::from_secs(5));
        panic!("intentional protocol assertion failure");
    }));

    assert!(unwind.is_err(), "protocol assertion did not unwind");
    assert_protocol_cleanup(probe, child_id);
}

// Not a case of its own: this is the body the protocol children run, selected
// by exact name under `--ignored`. Ignoring it keeps an ordinary run from
// reporting a pass for a body that asserts nothing, and the missing protocol
// variable is a harness fault rather than a reason to return quietly.
#[cfg(unix)]
#[test]
#[ignore = "protocol child body; the protocol cases spawn it by exact name"]
fn lifecycle_signal_child() {
    let protocol = std::env::var(CHILD_PROTOCOL_ENV)
        .expect("lifecycle_signal_child runs only as a spawned protocol child");
    match protocol.as_str() {
        "active-signal" => run_active_signal_child(),
        "closure-return" => run_closure_return_child(),
        "post-watcher" => run_post_watcher_child(),
        "cleanup-hold" => run_cleanup_hold_child(),
        other => panic!("unknown child protocol {other}"),
    }
}

#[cfg(unix)]
fn run_active_signal_child() {
    runtime::builder()
        .worker_threads(1)
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
            runtime::block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let http_addr = listener.local_addr().unwrap();
                let stop = mock::server_stop(http_addr).unwrap();
                stop.pause_once(ServerStopEdge::BeforeRuntimeWait)
                    .unwrap();
                let handle = camber::http::serve_background(listener, ok_router())
                    .expect("owned server requires a Tokio runtime");
                let mut retained =
                    tokio::time::timeout(Duration::from_secs(5), connect_request(http_addr))
                        .await
                        .expect("initial child request timed out");
                let response =
                    read_http_head_bounded(&mut retained, "initial child response timed out").await;
                assert!(response.starts_with("HTTP/1.1 200"));
                assert_http_body(&mut retained, b"ok").await;
                let control = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let control_addr = control.local_addr().unwrap();
                wait_until_paused_bounded(
                    &stop,
                    ServerStopEdge::BeforeRuntimeWait,
                    "runtime waiter checkpoint timed out",
                )
                .await;
                camber::spawn_async(async move {
                    let (mut command, _) =
                        tokio::time::timeout(Duration::from_secs(5), control.accept())
                            .await
                            .expect("active child control accept timed out")
                            .unwrap();
                    let mut token = [0u8; 5];
                    tokio::time::timeout(Duration::from_secs(5), command.read_exact(&mut token))
                        .await
                        .expect("active child token read timed out")
                        .unwrap();
                    assert_eq!(&token, b"RAISE");
                    signal_hook::low_level::raise(signal_hook::consts::SIGTERM).unwrap();
                    stop.release(ServerStopEdge::BeforeRuntimeWait)
                        .unwrap();
                    tokio::time::timeout(Duration::from_secs(5), assert_eof(&mut retained))
                        .await
                        .expect("active child retained EOF timed out");
                    let result = tokio::time::timeout(Duration::from_secs(5), handle.into_future())
                        .await
                        .expect("active child server join timed out");
                    assert!(result.is_ok());
                    println!("DRAINED");
                    std::io::stdout().flush().unwrap();
                    completion_tx.send(()).unwrap();
                });
                println!("SERVER_READY {control_addr}");
                std::io::stdout().flush().unwrap();
            });
            runtime::block_on(tokio::time::timeout(Duration::from_secs(5), completion_rx))
                .expect("active child completion timed out")
                .unwrap();
        })
        .unwrap();
}

#[cfg(unix)]
fn run_closure_return_child() {
    runtime::builder()
        .worker_threads(1)
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let http_addr = listener.local_addr().unwrap();
                let handle = camber::http::serve_background(listener, ok_router())
                    .expect("owned server requires a Tokio runtime");
                tokio::time::timeout(Duration::from_secs(2), assert_ok_request(http_addr))
                    .await
                    .expect("closure child initial request timed out");
                let control = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let control_addr = control.local_addr().unwrap();
                camber::spawn_async(async move {
                    let (mut command, _) =
                        tokio::time::timeout(Duration::from_secs(2), control.accept())
                            .await
                            .expect("closure child control accept timed out")
                            .unwrap();
                    let mut token = [0u8; 6];
                    tokio::time::timeout(Duration::from_secs(2), command.read_exact(&mut token))
                        .await
                        .expect("closure child token read timed out")
                        .unwrap();
                    assert_eq!(&token, b"CANCEL");
                    handle.cancel();
                    let result = tokio::time::timeout(Duration::from_secs(2), handle.into_future())
                        .await
                        .expect("closure child server join timed out");
                    assert_cancelled(result);
                });
                println!("CLOSURE_RETURNING {http_addr} {control_addr}");
                std::io::stdout().flush().unwrap();
            });
        })
        .unwrap();
    println!("AFTER_RUN");
    std::io::stdout().flush().unwrap();
}

#[cfg(unix)]
fn run_post_watcher_child() {
    runtime::builder().worker_threads(1).run(|| {}).unwrap();
    println!("WATCHER_GONE");
    std::io::stdout().flush().unwrap();
    signal_hook::low_level::raise(signal_hook::consts::SIGTERM).unwrap();
    println!("SURVIVED");
    std::io::stdout().flush().unwrap();
}

#[cfg(unix)]
fn run_cleanup_hold_child() {
    println!("CLEANUP_READY");
    std::io::stdout().flush().unwrap();
    std::thread::park();
}

// 1.T19
#[camber::test]
async fn malformed_and_abrupt_http_peers_remain_connection_local() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");

    let mut malformed = tokio::net::TcpStream::connect(addr).await.unwrap();
    malformed.write_all(b"not http\r\n\r\n").await.unwrap();
    let response = read_http_head(&mut malformed).await;
    assert_eq!(
        common::status_from_raw(&response),
        400,
        "unexpected malformed-request response: {response}"
    );
    assert_eof(&mut malformed).await;
    assert_ok_request(addr).await;

    let mut reset = abortive_tcp_socket().connect(addr).await.unwrap();
    reset
        .write_all(b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 4096\r\n\r\npartial")
        .await
        .unwrap();
    drop(reset);
    assert_ok_request(addr).await;

    runtime::request_shutdown();
    assert!(handle.await.is_ok());
}

// 1.T19
#[cfg(feature = "ws")]
#[camber::test]
async fn unclean_websocket_peer_close_remains_connection_local() {
    let mut router = websocket_router();
    router.get("/ok", |_req: &Request| async { Response::text(200, "ok") });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(listener, router)
        .expect("owned server requires a Tokio runtime");
    let websocket = common::upgraded_ws_peer(addr, "/ws", "the unclean peer").await;
    drop(websocket);
    assert_ok_request_path(addr, "/ok").await;
    runtime::request_shutdown();
    assert!(handle.await.is_ok());
}

// 1.T21, owned-server portion
#[cfg(feature = "ws")]
#[camber::test]
async fn supervisor_unwind_joins_acknowledged_and_buffered_upgrades() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::faulted_registration(addr).unwrap();
    let handle = camber::http::serve_background(listener, websocket_router())
        .expect("owned server requires a Tokio runtime");
    let mut acknowledged =
        common::upgraded_ws_peer(addr, "/ws", "the acknowledged upgrade").await;

    owners.upgrades.pause_once(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .unwrap();
    owners.upgrades.pause_once(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .unwrap();
    let mut pending = tokio::net::TcpStream::connect(addr).await.unwrap();
    pending
        .write_all(common::ws_upgrade_request("/ws").as_bytes())
        .await
        .unwrap();
    wait_and_release_bounded(
        &owners,
        UpgradeOwnerEdge::AfterHandoffSubmitted,
        "buffered upgrade ticket submission timed out",
    )
    .await;
    wait_until_paused_bounded(
        &owners,
        UpgradeOwnerEdge::BeforeTransferAcknowledge,
        "acknowledged upgrade boundary timed out",
    )
    .await;
    crate::common::unwind_the_supervisor(&owners, "the unwinding supervisor").await;
    // Released only once the unwind has committed its forced phase, so the
    // connection's answer reads a server that has already stopped admitting
    // rather than racing the panic it is meant to follow.
    owners.upgrades.release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .unwrap();

    assert_eof(&mut acknowledged).await;
    // A refusal rather than an internal failure: the connection that holds the
    // offer reads the forced phase the unwinding supervisor committed, so it
    // knows the server stopped admitting rather than only that its owner went
    // away.
    let pending_response = read_http_head(&mut pending).await;
    assert!(
        pending_response.starts_with("HTTP/1.1 503"),
        "pending upgrade committed an unexpected response: {pending_response}"
    );
    assert!(
        pending_response
            .to_ascii_lowercase()
            .contains("connection: close"),
        "the refused pending upgrade did not close the connection: {pending_response}"
    );
    common::assert_refusal_body_then_eof(
        &mut pending,
        common::UNAVAILABLE_BODY,
        "the unwound pending upgrade",
    )
    .await;
    assert_task_panicked(handle.await, SUPERVISOR_PANIC);
}

// 2.T1
#[tokio::test]
async fn every_owner_form_waits_until_owned_tasks_are_empty() {
    let (stop, addr, handle, client, release, dropped) = retained_owner_server().await;
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    handle.shutdown();
    assert_graceful_owner_waits(
        &stop,
        handle.into_future(),
        addr,
        client,
        &release,
        &dropped,
    )
    .await;

    let (stop, addr, handle, client, release, dropped) = retained_owner_server().await;
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    let future = handle.join();
    future.shutdown();
    assert_graceful_owner_waits(&stop, future, addr, client, &release, &dropped).await;

    let (stop, addr, handle, client, release, dropped) = retained_owner_server().await;
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    let future = handle.shutdown_and_join();
    assert_graceful_owner_waits(&stop, future, addr, client, &release, &dropped).await;
}

// 2.T2
#[tokio::test]
async fn join_transfers_control_without_stopping_admission() {
    let counter = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = mock::server_stop(addr).unwrap();
    let handle = camber::http::serve_background(listener, counting_router(Arc::clone(&counter)))
        .expect("owned server requires a Tokio runtime");
    let mut future = Box::pin(handle.join());
    assert!(future.as_mut().now_or_never().is_none());
    tokio::time::timeout(Duration::from_secs(5), assert_ok_request(addr))
        .await
        .expect("admission after join timed out");
    assert_eq!(counter.load(Ordering::Acquire), 1);

    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "join admission boundary timed out",
    )
    .await;
    let mut waiting = tokio::time::timeout(Duration::from_secs(5), connect_request(addr))
        .await
        .expect("equal-ready accepted connection timed out");
    stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    future.as_ref().get_ref().shutdown();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedControl,
        "joined future graceful selection timed out",
    )
    .await;
    assert_connection_closed(&mut waiting).await;
    assert_eq!(counter.load(Ordering::Acquire), 1);
    let result = tokio::time::timeout(Duration::from_secs(5), &mut future)
        .await
        .expect("joined future completion timed out");
    assert!(
        result.is_ok(),
        "unexpected joined future result: {result:?}"
    );
}

// 2.T2: explicit owner graceful wins over runtime, accept error, and task completion.
#[camber::test]
async fn owner_graceful_wins_over_runtime_accept_error_and_completed_task() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::supervisor_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, ok_router())
        .expect("owned server requires a Tokio runtime");
    let mut future = Box::pin(handle.join());
    tokio::time::timeout(
        Duration::from_secs(5),
        prepare_completed_task(&owners, addr),
    )
    .await
    .expect("completed-task preparation timed out");
    owners.connections.inject_once(ConnectionFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    runtime::request_shutdown();
    owners.stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    future.as_ref().get_ref().shutdown();
    owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::SupervisorSelectedControl,
        "owner graceful selection timed out",
    )
    .await;
    apply_selected(
        &owners,
        ServerStopEdge::SupervisorSelectedControl,
        "post-owner-graceful boundary timed out",
    )
    .await;
    select_next(
        &owners,
        ServerStopEdge::SupervisorSelectedRuntime,
        "equal-ready runtime selection timed out",
    )
    .await;
    tokio::time::timeout(
        Duration::from_secs(5),
        observe_deferred_task_reap(&owners, ServerStopEdge::SupervisorSelectedRuntime),
    )
    .await
    .expect("deferred completed-task reap timed out");
    let result = tokio::time::timeout(Duration::from_secs(5), &mut future)
        .await
        .expect("owner precedence completion timed out");
    assert!(
        result.is_ok(),
        "unexpected owner precedence result: {result:?}"
    );
}

// 2.T2: explicit owner graceful wins over a real ready permit.
#[test]
fn owner_graceful_wins_over_ready_permit() {
    runtime::builder()
        .connection_limit(1)
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(tokio::time::timeout(Duration::from_secs(10), async {
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let release = Arc::new(tokio::sync::Semaphore::new(0));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let stop = mock::server_stop(addr).unwrap();
                let handle = serve_held(
                    listener,
                    held_router(
                        entered_tx,
                        Arc::clone(&release),
                        Arc::new(AtomicBool::new(false)),
                    ),
                );
                let mut first = tokio::net::TcpStream::connect(addr).await.unwrap();
                first.write_all(CLOSE_REQUEST).await.unwrap();
                await_handler_entry(entered_rx, "owner permit request did not enter the handler")
                    .await;
                stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
                    .unwrap();
                wait_until_paused_bounded(
                    &stop,
                    ServerStopEdge::BeforeSupervisorSelect,
                    "owner permit initial boundary timed out",
                )
                .await;
                let mut waiting = connect_request(addr).await;
                select_next(
                    &stop,
                    ServerStopEdge::SupervisorSelectedAccept,
                    "owner permit accept selection timed out",
                )
                .await;
                apply_selected(
                    &stop,
                    ServerStopEdge::SupervisorSelectedAccept,
                    "owner permit post-accept boundary timed out",
                )
                .await;
                let mut future = Box::pin(handle.join());
                stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
                    .unwrap();
                release_and_drain_peer(&release, &mut first, b"released").await;
                future.as_ref().get_ref().shutdown();
                stop.release(ServerStopEdge::BeforeSupervisorSelect)
                    .unwrap();
                wait_and_release_bounded(
                    &stop,
                    ServerStopEdge::SupervisorSelectedControl,
                    "owner permit control selection timed out",
                )
                .await;
                assert_connection_closed(&mut waiting).await;
                let result = tokio::time::timeout(Duration::from_secs(5), &mut future)
                    .await
                    .expect("owner permit completion timed out");
                assert!(result.is_ok(), "unexpected owner permit result: {result:?}");
            }))
            .expect("owner permit precedence row timed out");
        })
        .unwrap();
}

// 2.T2: explicit owner graceful wins over a real submitted registration.
#[cfg(feature = "ws")]
#[camber::test]
async fn owner_graceful_wins_over_submitted_registration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = mock::registration_selection(addr).unwrap();
    let handle = camber::http::serve_background(listener, websocket_router())
        .expect("owned server requires a Tokio runtime");
    let mut future = Box::pin(handle.join());
    let mut client = tokio::time::timeout(
        Duration::from_secs(5),
        prepare_submitted_upgrade(&owners, addr),
    )
    .await
    .expect("submitted-registration preparation timed out");
    owners.stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    future.as_ref().get_ref().shutdown();
    owners.stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &owners,
        ServerStopEdge::SupervisorSelectedControl,
        "owner registration precedence selection timed out",
    )
    .await;
    apply_selected_event_then_release_transfer(
        &owners,
        ServerStopEdge::SupervisorSelectedControl,
        "owner graceful shutdown was not applied before releasing the submitted upgrade",
    )
    .await;
    assert_refused_upgrade_wire(&mut client, "the owner-graceful refused upgrade").await;
    let result = tokio::time::timeout(Duration::from_secs(5), &mut future)
        .await
        .expect("owner registration completion timed out");
    assert!(
        result.is_ok(),
        "unexpected owner registration result: {result:?}"
    );
}

// 2.T2: repeated runtime graceful does not restart the owner deadline.
#[tokio::test(start_paused = true)]
async fn owner_graceful_and_runtime_graceful_share_one_deadline() {
    let _context = runtime_test_support::install_runtime_context_without_request_deadlines();
    let (stop, _addr, handle, peer, _release) = retained_server(mock::server_stop).await;
    let mut future = Box::pin(handle.join());
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "owner deadline initial boundary timed out",
    )
    .await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    future.as_ref().get_ref().shutdown();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedControl,
        "owner deadline graceful selection timed out",
    )
    .await;
    apply_selected(
        &stop,
        ServerStopEdge::SupervisorSelectedControl,
        "owner deadline post-graceful boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(10)).await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedRuntime,
        "owner deadline runtime selection timed out",
    )
    .await;
    apply_selected(
        &stop,
        ServerStopEdge::SupervisorSelectedRuntime,
        "owner deadline final boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(20)).await;
    select_next(
        &stop,
        ServerStopEdge::SupervisorSelectedDeadline,
        "original owner deadline was restarted",
    )
    .await;
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    stop.release(ServerStopEdge::SupervisorSelectedDeadline)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::AfterSupervisorResultSend,
        "owner deadline task drain timed out",
    )
    .await;
    assert_eof_with_socket_deadline(peer);
    stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(5), &mut future)
        .await
        .expect("owner deadline completion timed out");
    assert_timeout(result);
}

// 2.T3
#[tokio::test]
async fn handle_and_future_cancel_abort_join_then_return_cancelled() {
    let (_controller, _addr, handle, client, _release, dropped) = retained_owner_server().await;
    handle.cancel();
    assert_forced_owner_joins(handle.into_future(), client, &dropped).await;

    let (_controller, _addr, handle, client, _release, dropped) = retained_owner_server().await;
    let future = handle.join();
    future.cancel();
    assert_forced_owner_joins(future, client, &dropped).await;
}

async fn assert_dropped_owner_continues_abort(
    owners: &impl Owns<ServerStopController>,
    addr: SocketAddr,
    client: tokio::net::TcpStream,
    dropped: &AtomicBool,
) {
    let stop = owners.owner();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedControl,
        "dropped owner abort selection timed out",
    )
    .await;
    apply_selected(
        &stop,
        ServerStopEdge::SupervisorSelectedControl,
        "dropped owner abort application timed out",
    )
    .await;
    common::assert_admission_closed_blocking(addr, OBSERVATION_DEADLINE);
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::AfterSupervisorResultSend,
        "dropped owner result send timed out",
    )
    .await;
    assert!(
        dropped.load(Ordering::Acquire),
        "dropped owner reached finish before releasing transport state"
    );
    assert_connection_closed_with_socket_deadline(client);
    stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
}

async fn pause_after_owner_graceful<F>(
    owners: &impl Owns<ServerStopController>,
    request: F,
    context: &str,
) where
    F: FnOnce(),
{
    let stop = owners.owner();
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(&stop, ServerStopEdge::BeforeSupervisorSelect, context).await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    request();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedControl,
        "owner graceful selection timed out",
    )
    .await;
    apply_selected(
        &stop,
        ServerStopEdge::SupervisorSelectedControl,
        "post-owner-graceful boundary timed out",
    )
    .await;
}

async fn pause_after_success_result_send<F>(owners: &impl Owns<ServerStopController>, request: F)
where
    F: FnOnce(),
{
    let stop = owners.owner();
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "post-result initial boundary timed out",
    )
    .await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    request();
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedControl,
        "post-result graceful selection timed out",
    )
    .await;
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::AfterSupervisorResultSend,
        "successful result send timed out",
    )
    .await;
}

async fn wait_for_dispatch_drop(dropped: tokio::sync::oneshot::Receiver<()>, context: &str) {
    bounded(dropped, context)
        .await
        .unwrap_or_else(|_| panic!("{context}: the dispatch probe never reported"));
}

#[derive(Clone, Copy)]
enum OwnerForm {
    Handle,
    Future,
}

impl OwnerForm {
    fn label(self) -> &'static str {
        match self {
            Self::Handle => "handle",
            Self::Future => "future",
        }
    }
}

enum PendingOwner {
    Handle(ServerHandle),
    Future(std::pin::Pin<Box<ServerHandleFuture>>),
}

impl PendingOwner {
    fn new(handle: ServerHandle, form: OwnerForm) -> Self {
        match form {
            OwnerForm::Handle => Self::Handle(handle),
            OwnerForm::Future => {
                let mut future = Box::pin(handle.join());
                assert!(future.as_mut().now_or_never().is_none());
                Self::Future(future)
            }
        }
    }

    fn shutdown(&self) {
        match self {
            Self::Handle(handle) => handle.shutdown(),
            Self::Future(future) => future.as_ref().get_ref().shutdown(),
        }
    }
}

async fn assert_owner_drop_forces_abort(form: OwnerForm) {
    let (stop, addr, handle, client, _release, dropped) = retained_owner_server().await;
    let owner = PendingOwner::new(handle, form);
    stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    drop(owner);
    assert_dropped_owner_continues_abort(&stop, addr, client, &dropped).await;
}

async fn assert_owner_drop_after_graceful_forces_abort(form: OwnerForm) {
    let (stop, addr, handle, client, _release, dropped) = retained_owner_server().await;
    let owner = PendingOwner::new(handle, form);
    let graceful_context = format!("{} graceful boundary timed out", form.label());
    pause_after_owner_graceful(&stop, || owner.shutdown(), &graceful_context).await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    drop(owner);
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    assert_dropped_owner_continues_abort(&stop, addr, client, &dropped).await;
}

async fn assert_owner_deadline_beats_drop(form: OwnerForm) {
    let (stop, _addr, handle, client, _release, dropped) = retained_owner_server().await;
    let owner = PendingOwner::new(handle, form);
    let graceful_context = format!("timeout {} graceful boundary timed out", form.label());
    pause_after_owner_graceful(&stop, || owner.shutdown(), &graceful_context).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    stop.pause_once(ServerStopEdge::SupervisorSelectedDeadline)
        .unwrap();
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    drop(owner);
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &stop,
        ServerStopEdge::SupervisorSelectedDeadline,
        &format!(
            "equal-ready {} deadline did not beat Drop abort",
            form.label()
        ),
    )
    .await;
    wait_until_paused_bounded(
        &stop,
        ServerStopEdge::AfterSupervisorResultSend,
        &format!("fixed {} timeout result send timed out", form.label()),
    )
    .await;
    assert_connection_closed_with_socket_deadline(client);
    assert!(dropped.load(Ordering::Acquire));
    stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
}

async fn assert_post_result_owner_drop_continues(form: OwnerForm) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let stop = mock::server_stop(listener.local_addr().unwrap()).unwrap();
    let (dispatch_tx, mut dispatch_rx) = tokio::sync::oneshot::channel();
    let handle = camber::http::serve_background(listener, dispatch_drop_router(dispatch_tx))
        .expect("owned server requires a Tokio runtime");
    let owner = PendingOwner::new(handle, form);
    pause_after_success_result_send(&stop, || owner.shutdown()).await;
    drop(owner);
    assert!(
        (&mut dispatch_rx).now_or_never().is_none(),
        "{} Drop destroyed the supervisor while result send was paused",
        form.label()
    );
    stop.release(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    let exit_context = format!("{} post-result supervisor exit timed out", form.label());
    wait_for_dispatch_drop(dispatch_rx, &exit_context).await;
}

// 2.T4
#[tokio::test(start_paused = true)]
async fn dropping_handle_or_pending_future_forces_continuing_supervisor() {
    assert_owner_drop_forces_abort(OwnerForm::Handle).await;
    assert_owner_drop_forces_abort(OwnerForm::Future).await;
    assert_owner_drop_after_graceful_forces_abort(OwnerForm::Handle).await;
    assert_owner_drop_after_graceful_forces_abort(OwnerForm::Future).await;
    assert_owner_deadline_beats_drop(OwnerForm::Handle).await;
    assert_owner_deadline_beats_drop(OwnerForm::Future).await;
    assert_post_result_owner_drop_continues(OwnerForm::Handle).await;
    assert_post_result_owner_drop_continues(OwnerForm::Future).await;
}

// 2.T7
#[tokio::test]
async fn graceful_http1_finishes_current_response_then_closes_admission() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let next_requests = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let stop = mock::server_stop(addr).unwrap();
    let handle = serve_held(
        listener,
        named_held_router(
            entered_tx,
            Arc::clone(&release),
            Arc::clone(&dropped),
            Arc::clone(&next_requests),
        ),
    );
    let mut client = connect_request_path(addr, "/active").await;
    await_handler_entry(
        entered_rx,
        "active HTTP/1 request did not enter the handler",
    )
    .await;
    let mut future = Box::pin(handle.join());
    stop.pause_once(ServerStopEdge::AfterSupervisorResultSend)
        .unwrap();
    future.as_ref().get_ref().shutdown();
    assert!(future.as_mut().now_or_never().is_none());
    tokio::time::timeout(Duration::from_secs(5), async {
        common::assert_admission_closed(addr, OBSERVATION_DEADLINE).await;
        release.add_permits(1);
        let response =
            read_http_head_bounded(&mut client, "active HTTP/1 response head timed out").await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert_http_body_bounded(
            &mut client,
            b"active",
            "active HTTP/1 response body timed out",
        )
        .await;
        client
            .write_all(b"GET /next HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await
            .unwrap();
        assert_connection_closed(&mut client).await;
        assert_eq!(next_requests.load(Ordering::Acquire), 0);
        assert!(dropped.load(Ordering::Acquire));
    })
    .await
    .expect("graceful HTTP/1 observations timed out");
    wait_and_release_bounded(
        &stop,
        ServerStopEdge::AfterSupervisorResultSend,
        "graceful HTTP/1 result send timed out",
    )
    .await;
    let result = tokio::time::timeout(Duration::from_secs(5), &mut future)
        .await
        .expect("graceful HTTP/1 owner completion timed out");
    assert!(
        result.is_ok(),
        "unexpected graceful HTTP/1 result: {result:?}"
    );
}
