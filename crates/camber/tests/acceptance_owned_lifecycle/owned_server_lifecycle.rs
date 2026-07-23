use std::future::{Future, IntoFuture};
use std::io::Write;
use std::net::SocketAddr;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camber::RuntimeError;
use camber::http::mock::{
    LifecycleCheckpoint, LifecycleController, LifecycleFault, SupervisorJoinProbe, lifecycle,
    supervisor_join_probe,
};
use camber::http::{Request, Response, Router, ServerHandle, SseWriter};
use camber::runtime;
use futures_util::FutureExt;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};

const HTTP_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
const CLOSE_REQUEST: &[u8] = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
const PROBE_PANIC: &str = "supervisor join probe panic";
const OWNED_TASK_PANIC: &str = "injected owned HTTP task panic";
const DISPATCH_POLL_INTERVAL: Duration = Duration::from_millis(10);

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

struct DispatchDropProbe(Arc<AtomicBool>);

impl Drop for DispatchDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

fn dispatch_drop_router(dropped: Arc<AtomicBool>) -> Router {
    let probe = DispatchDropProbe(dropped);
    let mut router = Router::new();
    router.get("/", move |_req: &Request| {
        let _ = &probe;
        async { Response::text(200, "ok") }
    });
    router
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

async fn connect_request_path(addr: SocketAddr, path: &str) -> tokio::net::TcpStream {
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n");
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

async fn write_request(stream: &mut tokio::net::TcpStream) {
    stream.write_all(HTTP_REQUEST).await.unwrap();
}

async fn read_http_head<S>(stream: &mut S) -> Box<str>
where
    S: AsyncRead + Unpin,
{
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") {
        let read = stream.read(&mut byte).await.unwrap();
        assert_ne!(read, 0, "peer closed before a complete HTTP response head");
        response.push(byte[0]);
    }
    String::from_utf8(response).unwrap().into_boxed_str()
}

async fn read_http_head_bounded<S>(stream: &mut S, context: &str) -> Box<str>
where
    S: AsyncRead + Unpin,
{
    tokio::time::timeout(Duration::from_secs(5), read_http_head(stream))
        .await
        .unwrap_or_else(|_| panic!("{context}"))
}

async fn assert_eof<S>(stream: &mut S)
where
    S: AsyncRead + Unpin,
{
    let mut byte = [0u8; 1];
    let read = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
        .await
        .expect("timed out waiting for peer EOF")
        .expect("failed while waiting for peer EOF");
    assert_eq!(read, 0, "expected EOF, received transport data");
}

fn assert_eof_with_socket_deadline(stream: tokio::net::TcpStream) {
    let stream = stream.into_std().unwrap();
    let read = peek_with_socket_deadline(&stream).expect("failed while waiting for peer EOF");
    assert_eq!(read, 0, "expected EOF, received transport data");
}

async fn assert_eof_with_independent_deadline(stream: tokio::net::TcpStream) {
    tokio::task::spawn_blocking(move || assert_eof_with_socket_deadline(stream))
        .await
        .expect("socket EOF observer panicked");
}

async fn assert_connection_closed_with_independent_deadline(stream: tokio::net::TcpStream) {
    tokio::task::spawn_blocking(move || assert_connection_closed_with_socket_deadline(stream))
        .await
        .expect("socket closure observer panicked");
}

fn assert_connection_closed_with_socket_deadline(stream: tokio::net::TcpStream) {
    let stream = stream.into_std().unwrap();
    assert_blocking_connection_closed(&stream);
}

fn peek_with_socket_deadline(stream: &std::net::TcpStream) -> std::io::Result<usize> {
    stream.set_nonblocking(true)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let mut byte = [0u8; 1];
    loop {
        match stream.peek(&mut byte) {
            Err(error)
                if error.kind() == std::io::ErrorKind::WouldBlock
                    && std::time::Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(1));
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::TimedOut,
                    "timed out waiting for peer closure",
                ));
            }
            result => return result,
        }
    }
}

fn assert_blocking_connection_closed(stream: &std::net::TcpStream) {
    match peek_with_socket_deadline(stream) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        Ok(read) => panic!("expected peer closure, received {read} byte(s)"),
        Err(error) => panic!("failed while waiting for peer closure: {error}"),
    }
}

fn assert_new_admission_stopped_with_socket_deadline(addr: SocketAddr) {
    match std::net::TcpStream::connect_timeout(&addr, Duration::from_secs(5)) {
        Err(_) => {}
        Ok(mut stream) => {
            stream
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            assert_closed_admission_write(&mut stream);
        }
    }
}

fn assert_closed_admission_write(stream: &mut std::net::TcpStream) {
    match stream.write_all(CLOSE_REQUEST) {
        Ok(()) => assert_blocking_connection_closed(stream),
        Err(error) if is_closed_connection_error(&error) => {}
        Err(error) => panic!("failed while probing closed admission: {error}"),
    }
}

fn is_closed_connection_error(error: &std::io::Error) -> bool {
    matches!(
        error.kind(),
        std::io::ErrorKind::BrokenPipe
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
            | std::io::ErrorKind::NotConnected
    )
}

async fn assert_http_body<S>(stream: &mut S, expected: &[u8])
where
    S: AsyncRead + Unpin,
{
    let mut body = vec![0u8; expected.len()];
    stream.read_exact(&mut body).await.unwrap();
    assert_eq!(body, expected);
}

async fn assert_http_body_bounded<S>(stream: &mut S, expected: &[u8], context: &str)
where
    S: AsyncRead + Unpin,
{
    tokio::time::timeout(Duration::from_secs(5), assert_http_body(stream, expected))
        .await
        .unwrap_or_else(|_| panic!("{context}"));
}

async fn assert_connection_closed<S>(stream: &mut S)
where
    S: AsyncRead + Unpin,
{
    let mut byte = [0u8; 1];
    let result = tokio::time::timeout(Duration::from_secs(5), stream.read(&mut byte))
        .await
        .expect("timed out waiting for peer closure");
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

async fn assert_new_admission_stopped(addr: SocketAddr) {
    let mut stream = match tokio::net::TcpStream::connect(addr).await {
        Err(_) => return,
        Ok(stream) => stream,
    };
    match stream.write_all(CLOSE_REQUEST).await {
        Ok(()) => assert_connection_closed(&mut stream).await,
        Err(error) if is_closed_connection_error(&error) => {}
        Err(error) => panic!("failed while probing closed admission: {error}"),
    }
}

#[expect(
    deprecated,
    reason = "SO_LINGER zero is required to produce a deterministic TCP reset"
)]
fn abortive_tcp_socket() -> tokio::net::TcpSocket {
    let socket = tokio::net::TcpSocket::new_v4().unwrap();
    socket.set_linger(Some(Duration::ZERO)).unwrap();
    assert_eq!(socket.linger().unwrap(), Some(Duration::ZERO));
    socket
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

async fn release_after_shutdown_race(checkpoint: LifecycleCheckpoint, forced: bool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    controller.pause_once(checkpoint).unwrap();
    let handle = camber::http::serve_background(listener, counting_router(Arc::clone(&counter)));

    let mut client = connect_request(addr).await;
    controller.wait_until_paused(checkpoint).await.unwrap();
    match forced {
        true => {
            handle.cancel();
            controller.release(checkpoint).unwrap();
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
            controller
                .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
                .unwrap();
            runtime::request_shutdown();
            controller.release(checkpoint).unwrap();
            controller
                .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
                .await
                .unwrap();
            assert_connection_closed(&mut client).await;
            assert_eq!(counter.load(Ordering::Acquire), 0);
            controller
                .release(LifecycleCheckpoint::BeforeSupervisorSelect)
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
    assert_traits::<LifecycleCheckpoint>();
    assert_traits::<LifecycleFault>();
    assert_traits::<SupervisorJoinProbe>();

    let first = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let second = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let third = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let first_addr = first.local_addr().unwrap();
    let second_addr = second.local_addr().unwrap();
    let third_addr = third.local_addr().unwrap();
    let first_controller = lifecycle(first_addr).unwrap();
    let second_controller = lifecycle(second_addr).unwrap();
    assert_invalid(lifecycle(first_addr));
    first_controller
        .pause_once(LifecycleCheckpoint::AfterAccept)
        .unwrap();
    second_controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();

    let first_handle = camber::http::serve_background(first, ok_router());
    let second_handle = camber::http::serve_background(second, ok_router());
    let third_handle = camber::http::serve_background(third, ok_router());
    let mut first_client = connect_request(first_addr).await;
    first_controller
        .wait_until_paused(LifecycleCheckpoint::AfterAccept)
        .await
        .unwrap();

    assert_io_kind(second_handle.await, std::io::ErrorKind::Other);
    assert_ok_request(third_addr).await;
    assert!(
        tokio::time::timeout(Duration::ZERO, read_http_head(&mut first_client))
            .await
            .is_err(),
        "the listener-scoped pause was not enforced"
    );
    assert!(tokio::net::TcpStream::connect(second_addr).await.is_err());

    first_controller
        .release(LifecycleCheckpoint::AfterAccept)
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
    let controller = lifecycle(listener.local_addr().unwrap()).unwrap();
    assert_invalid(
        controller
            .wait_until_paused(LifecycleCheckpoint::AfterAccept)
            .await,
    );
    assert_invalid(controller.release(LifecycleCheckpoint::AfterAccept));
    controller
        .pause_once(LifecycleCheckpoint::AfterAccept)
        .unwrap();
    assert_invalid(controller.pause_once(LifecycleCheckpoint::AfterAccept));
    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    assert_invalid(controller.inject_once(LifecycleFault::PanicNextOwnedTask));
}

// 1.T1
#[camber::test]
async fn dropping_controller_releases_waiter_and_allows_address_reuse() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterAccept)
        .unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    let mut client = connect_request(addr).await;
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterAccept)
        .await
        .unwrap();
    drop(controller);
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200"));

    handle.cancel();
    assert_cancelled(handle.await);
    drop(client);
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let replacement = lifecycle(listener.local_addr().unwrap()).unwrap();
    drop(replacement);
}

// 1.T2
#[tokio::test]
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
    let controller = lifecycle(listener.local_addr().unwrap()).unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    let result: Result<(), RuntimeError> = handle.await;
    assert!(result.is_ok());
}

// 1.T3
#[camber::test]
async fn graceful_shutdown_after_accept_never_dispatches() {
    release_after_shutdown_race(LifecycleCheckpoint::AfterAccept, false).await;
}

// 1.T3
#[camber::test]
async fn forced_shutdown_after_accept_never_dispatches() {
    release_after_shutdown_race(LifecycleCheckpoint::AfterAccept, true).await;
}

// 1.T3
#[camber::test]
async fn graceful_shutdown_after_permit_never_dispatches() {
    release_after_shutdown_race(LifecycleCheckpoint::AfterPermit, false).await;
}

// 1.T3
#[camber::test]
async fn forced_shutdown_after_permit_never_dispatches() {
    release_after_shutdown_race(LifecycleCheckpoint::AfterPermit, true).await;
}

// 1.T3
#[camber::test]
async fn admitted_plain_transport_keeps_owner_pending_until_release() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(
        listener,
        held_router(entered_tx, Arc::clone(&release), Arc::clone(&dropped)),
    );
    let mut client = connect_request(addr).await;
    entered_rx.await.unwrap();
    runtime::request_shutdown();
    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    assert!(!dropped.load(Ordering::Acquire));
    release.add_permits(1);
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert_http_body(&mut client, b"released").await;
    assert_eof(&mut client).await;
    assert!(completion.await.is_ok());
    assert!(dropped.load(Ordering::Acquire));
}

// 1.T3
#[camber::test]
async fn admitted_tls_transport_keeps_owner_pending_until_release() {
    let (cert, key) = common::generate_self_signed_cert();
    let tls_config = common::server_tls_config(&cert, &key);
    let client_config = common::tls_client_config(&[&cert]);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background_tls(
        listener,
        held_router(entered_tx, Arc::clone(&release), Arc::clone(&dropped)),
        tls_config,
    );
    let tcp = tokio::net::TcpStream::connect(addr).await.unwrap();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut client = connector.connect(server_name, tcp).await.unwrap();
    client.write_all(HTTP_REQUEST).await.unwrap();
    entered_rx.await.unwrap();
    runtime::request_shutdown();
    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    release.add_permits(1);
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert_http_body(&mut client, b"released").await;
    assert_eof(&mut client).await;
    assert!(completion.await.is_ok());
    assert!(dropped.load(Ordering::Acquire));
}

#[cfg(feature = "ws")]
fn websocket_request(path: &str) -> Box<str> {
    format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )
    .into_boxed_str()
}

#[cfg(feature = "ws")]
fn websocket_router() -> Router {
    use camber::http::WsConn;

    let mut router = Router::new();
    router.ws("/ws", |_req: &Request, mut connection: WsConn| {
        while connection.recv().is_some() {}
        Ok(())
    });
    router
}

#[cfg(feature = "ws")]
async fn websocket_client(addr: SocketAddr, path: &str) -> tokio::net::TcpStream {
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(websocket_request(path).as_bytes())
        .await
        .unwrap();
    let response = read_http_head(&mut client).await;
    assert!(
        response.starts_with("HTTP/1.1 101"),
        "unexpected response: {response}"
    );
    client
}

#[cfg(feature = "ws")]
async fn write_websocket_text(stream: &mut tokio::net::TcpStream, text: &str) {
    assert!(text.len() <= 125);
    let mask = [0x12, 0x34, 0x56, 0x78];
    let mut frame = Vec::with_capacity(text.len() + 6);
    frame.push(0x81);
    frame.push(0x80 | text.len() as u8);
    frame.extend_from_slice(&mask);
    frame.extend(
        text.as_bytes()
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    stream.write_all(&frame).await.unwrap();
}

#[cfg(feature = "ws")]
async fn read_websocket_text(stream: &mut tokio::net::TcpStream) -> Box<str> {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await.unwrap();
    assert_eq!(header[0] & 0x0f, 1);
    assert_eq!(header[1] & 0x80, 0);
    let length = (header[1] & 0x7f) as usize;
    assert!(length <= 125);
    let mut payload = vec![0u8; length];
    stream.read_exact(&mut payload).await.unwrap();
    String::from_utf8(payload).unwrap().into_boxed_str()
}

#[cfg(feature = "ws")]
async fn assert_websocket_close(stream: &mut tokio::net::TcpStream) {
    let mut header = [0u8; 2];
    stream.read_exact(&mut header).await.unwrap();
    assert_eq!(header[0] & 0x0f, 0x08, "expected a WebSocket close frame");
    assert_eq!(header[1] & 0x80, 0, "server close frame must not be masked");
    let payload_len = usize::from(header[1] & 0x7f);
    assert!(payload_len <= 125, "close frame used an extended payload");
    let mut payload = vec![0u8; payload_len];
    stream.read_exact(&mut payload).await.unwrap();
}

#[cfg(feature = "ws")]
async fn write_websocket_close(stream: &mut tokio::net::TcpStream) {
    stream
        .write_all(&[0x88, 0x80, 0x12, 0x34, 0x56, 0x78])
        .await
        .unwrap();
}

// 1.T3
#[cfg(feature = "ws")]
#[camber::test]
async fn admitted_websocket_bridge_keeps_owner_pending_until_transport_closes() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = camber::http::serve_background(listener, websocket_router());
    let mut client = websocket_client(addr, "/ws").await;
    runtime::request_shutdown();
    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    assert_websocket_close(&mut client).await;
    write_websocket_close(&mut client).await;
    assert_eof(&mut client).await;
    let result = completion.await;
    assert!(result.is_ok(), "unexpected server result: {result:?}");
}

// 1.T5
#[tokio::test(start_paused = true)]
async fn default_grace_deadline_aborts_joins_and_releases_direct_transport() {
    runtime::on_cancel(std::future::pending());
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let server = tokio::spawn(camber::http::serve_async(
        listener,
        held_router(entered_tx, release, Arc::clone(&dropped)),
    ));
    let client = connect_request(addr).await;
    entered_rx.await.unwrap();

    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "initial supervisor boundary timed out",
    )
    .await;
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedRuntime,
        "runtime shutdown selection timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
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
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    tokio::time::advance(Duration::from_millis(1)).await;
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedDeadline,
        "grace deadline selection timed out",
    )
    .await;
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::AfterSupervisorResultSend,
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
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
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
                let handle = camber::http::serve_background(
                    listener,
                    held_router(
                        entered_tx,
                        Arc::new(tokio::sync::Semaphore::new(0)),
                        Arc::clone(&observed),
                    ),
                );
                let mut client = connect_request(addr).await;
                entered_rx.await.unwrap();
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
                let controller = lifecycle(addr).unwrap();
                let handle =
                    camber::http::serve_background(listener, counting_router(Arc::clone(&counter)));
                let mut first = connect_request(addr).await;
                let response =
                    tokio::time::timeout(Duration::from_secs(5), read_http_head(&mut first))
                        .await
                        .expect("first admitted response timed out");
                assert!(response.starts_with("HTTP/1.1 200"));
                assert_eq!(counter.load(Ordering::Acquire), 1);

                controller
                    .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .unwrap();
                let mut second =
                    tokio::time::timeout(Duration::from_secs(5), connect_request(addr))
                        .await
                        .expect("second connection timed out");
                tokio::time::timeout(
                    Duration::from_secs(5),
                    controller.wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending),
                )
                .await
                .expect("pending permit checkpoint timed out")
                .unwrap();
                controller
                    .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
                    .unwrap();
                runtime::request_shutdown();
                tokio::time::timeout(
                    Duration::from_secs(5),
                    controller.wait_until_paused(LifecycleCheckpoint::SupervisorSelectedRuntime),
                )
                .await
                .expect("runtime selection checkpoint timed out")
                .unwrap();
                controller
                    .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .unwrap();
                drop(first);
                controller
                    .release(LifecycleCheckpoint::SupervisorSelectedRuntime)
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
    let controller = lifecycle(addr).unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterAccept)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterPermit)
        .unwrap();
    let handle = camber::http::serve_background_tls(listener, ok_router(), tls_config);
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterAccept)
        .await
        .unwrap();
    controller
        .release(LifecycleCheckpoint::AfterAccept)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterPermit)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::AfterPermit)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();

    let result = tokio::time::timeout(Duration::from_secs(5), handle.into_future())
        .await
        .expect("TLS server owner completion timed out");
    assert!(result.is_ok());
    assert_eof(&mut client).await;
}

async fn retained_server() -> (
    LifecycleController,
    SocketAddr,
    ServerHandle,
    tokio::net::TcpStream,
    Arc<tokio::sync::Semaphore>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(
        listener,
        held_router(
            entered_tx,
            Arc::clone(&release),
            Arc::new(AtomicBool::new(false)),
        ),
    );
    let client = connect_request(addr).await;
    entered_rx.await.unwrap();
    (controller, addr, handle, client, release)
}

async fn retained_owner_server() -> (
    LifecycleController,
    SocketAddr,
    ServerHandle,
    tokio::net::TcpStream,
    Arc<tokio::sync::Semaphore>,
    Arc<AtomicBool>,
) {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let dropped = Arc::new(AtomicBool::new(false));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(
        listener,
        named_held_router(
            entered_tx,
            Arc::clone(&release),
            Arc::clone(&dropped),
            Arc::new(AtomicUsize::new(0)),
        ),
    );
    let client = connect_request_path(addr, "/active").await;
    entered_rx.await.unwrap();
    (controller, addr, handle, client, release, dropped)
}

async fn assert_graceful_owner_waits<F>(
    controller: &LifecycleController,
    completion: F,
    addr: SocketAddr,
    mut client: tokio::net::TcpStream,
    release: Arc<tokio::sync::Semaphore>,
    dropped: Arc<AtomicBool>,
) where
    F: Future<Output = Result<(), RuntimeError>>,
{
    let mut completion = Box::pin(completion);
    assert!(completion.as_mut().now_or_never().is_none());
    assert!(!dropped.load(Ordering::Acquire));
    tokio::time::timeout(Duration::from_secs(5), async {
        assert_new_admission_stopped(addr).await;
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
        controller,
        LifecycleCheckpoint::AfterSupervisorResultSend,
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
    dropped: Arc<AtomicBool>,
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
    let (controller, addr, handle, mut peer, release) = retained_server().await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedAccept).await;

    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    runtime::request_shutdown();
    assert_new_admission_stopped(addr).await;
    release.add_permits(1);
    let _ = read_http_head(&mut peer).await;
    assert_http_body(&mut peer, b"released").await;
    assert_eof(&mut peer).await;
    assert_io_kind(completion.await, std::io::ErrorKind::Other);
}

// 1.T11
#[camber::test]
async fn owned_task_panic_drains_retained_peer_before_returning() {
    let (controller, addr, handle, mut peer, release) = retained_server().await;
    controller
        .inject_once(LifecycleFault::PanicNextOwnedTask)
        .unwrap();
    let mut faulted = connect_request(addr).await;
    assert_connection_closed(&mut faulted).await;
    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    assert_new_admission_stopped(addr).await;
    release.add_permits(1);
    let _ = read_http_head(&mut peer).await;
    assert_http_body(&mut peer, b"released").await;
    assert_eof(&mut peer).await;
    assert_task_panicked(completion.await, OWNED_TASK_PANIC);
}

// 1.T11, 1.T12
#[tokio::test(start_paused = true)]
async fn fatal_outcome_is_replaced_by_timeout_after_peer_drain_escalation() {
    runtime::on_cancel(std::future::pending());
    let (controller, _addr, handle, peer, _release) = retained_server().await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    select_deadline(&controller).await;
    release_deadline_and_wait_for_drained_result(&controller).await;
    assert_eof_with_socket_deadline(peer);
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    assert_timeout(handle.await);
}

// 1.T11, 1.T12
#[camber::test]
async fn explicit_cancel_replaces_provisional_fatal_outcome() {
    let (controller, _addr, handle, mut peer, _release) = retained_server().await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    handle.cancel();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    assert_eof(&mut peer).await;
    assert_cancelled(handle.await);
}

async fn wait_and_release(controller: &LifecycleController, checkpoint: LifecycleCheckpoint) {
    controller.wait_until_paused(checkpoint).await.unwrap();
    controller.release(checkpoint).unwrap();
}

async fn wait_until_paused_bounded(
    controller: &LifecycleController,
    checkpoint: LifecycleCheckpoint,
    context: &str,
) {
    tokio::time::timeout(
        Duration::from_secs(5),
        controller.wait_until_paused(checkpoint),
    )
    .await
    .unwrap_or_else(|_| panic!("{context}"))
    .unwrap();
}

async fn wait_and_release_bounded(
    controller: &LifecycleController,
    checkpoint: LifecycleCheckpoint,
    context: &str,
) {
    wait_until_paused_bounded(controller, checkpoint, context).await;
    controller.release(checkpoint).unwrap();
}

async fn apply_selected_event_then_release_upgrade(
    controller: &LifecycleController,
    selected: LifecycleCheckpoint,
    context: &str,
) {
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller.release(selected).unwrap();
    wait_until_paused_bounded(
        controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        context,
    )
    .await;
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
}

async fn prepare_completed_task(controller: &LifecycleController, addr: SocketAddr) {
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client.write_all(CLOSE_REQUEST).await.unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert_http_body(&mut client, b"ok").await;
    assert_eof(&mut client).await;
}

async fn observe_deferred_task_reap(
    controller: &LifecycleController,
    selected_winner: LifecycleCheckpoint,
) {
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller.release(selected_winner).unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedTask)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(controller, LifecycleCheckpoint::SupervisorSelectedTask).await;
}

async fn owned_task_fault_result(fault: LifecycleFault, expected: &str) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterPermit)
        .unwrap();
    controller.inject_once(fault).unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    let mut client = connect_request(addr).await;
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterPermit)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::AfterPermit)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedTask)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedTask).await;
    assert_connection_closed(&mut client).await;
    assert_task_panicked(handle.await, expected);
}

async fn prepare_faulted_task(
    controller: &LifecycleController,
    addr: SocketAddr,
    fault: LifecycleFault,
) {
    controller
        .pause_once(LifecycleCheckpoint::AfterPermit)
        .unwrap();
    controller.inject_once(fault).unwrap();
    let client = connect_request(addr).await;
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterPermit)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::AfterPermit)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    assert_connection_closed_with_independent_deadline(client).await;
}

// 1.T12: real inserted-task panic join rows in Running.
#[camber::test]
async fn owned_task_string_and_opaque_panics_map_exactly() {
    owned_task_fault_result(LifecycleFault::PanicNextOwnedTask, OWNED_TASK_PANIC).await;
}

// 1.T12
#[camber::test]
async fn owned_task_opaque_panic_maps_to_unknown_panic() {
    owned_task_fault_result(LifecycleFault::PanicNextOwnedTaskOpaque, "unknown panic").await;
}

// 1.T12: graceful without a candidate then panic installs the panic.
#[camber::test]
async fn graceful_then_owned_task_panic_installs_candidate() {
    let (controller, addr, handle, mut peer, release) = retained_server().await;
    prepare_faulted_task(&controller, addr, LifecycleFault::PanicNextOwnedTask).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .await
        .unwrap();
    observe_deferred_task_reap(&controller, LifecycleCheckpoint::SupervisorSelectedRuntime).await;
    release.add_permits(1);
    let _ = read_http_head(&mut peer).await;
    assert_http_body(&mut peer, b"released").await;
    assert_eof(&mut peer).await;
    assert_task_panicked(handle.await, OWNED_TASK_PANIC);
}

// 1.T12: IO selected first remains the candidate when a panic is ready.
#[camber::test]
async fn io_then_owned_task_panic_retains_io() {
    let (controller, addr, handle, mut peer, release) = retained_server().await;
    prepare_faulted_task(&controller, addr, LifecycleFault::PanicNextOwnedTask).await;
    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
        .await
        .unwrap();
    observe_deferred_task_reap(&controller, LifecycleCheckpoint::SupervisorSelectedAccept).await;
    release.add_permits(1);
    let _ = read_http_head(&mut peer).await;
    assert_http_body(&mut peer, b"released").await;
    assert_eof(&mut peer).await;
    assert_io_kind(handle.await, std::io::ErrorKind::Other);
}

// 1.T12: panic selected first remains the candidate after repeated graceful.
#[camber::test]
async fn owned_task_panic_then_graceful_retains_panic() {
    let (controller, addr, handle, mut peer, release) = retained_server().await;
    prepare_faulted_task(&controller, addr, LifecycleFault::PanicNextOwnedTask).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedTask)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedTask).await;
    runtime::request_shutdown();
    release.add_permits(1);
    let _ = read_http_head(&mut peer).await;
    assert_http_body(&mut peer, b"released").await;
    assert_eof(&mut peer).await;
    assert_task_panicked(handle.await, OWNED_TASK_PANIC);
}

// 1.T12: one owned-task panic then another retains the first payload.
#[camber::test]
async fn owned_task_panic_then_opaque_panic_retains_first_payload() {
    let (controller, addr, handle, mut peer, release) = retained_server().await;
    prepare_faulted_task(&controller, addr, LifecycleFault::PanicNextOwnedTask).await;
    controller
        .inject_once(LifecycleFault::PanicNextOwnedTaskOpaque)
        .unwrap();
    let mut second = connect_request(addr).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    assert_connection_closed(&mut second).await;
    for index in 0..2 {
        controller
            .pause_once(LifecycleCheckpoint::SupervisorSelectedTask)
            .unwrap();
        controller
            .release(LifecycleCheckpoint::BeforeSupervisorSelect)
            .unwrap();
        controller
            .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedTask)
            .await
            .unwrap();
        match index {
            0 => {
                controller
                    .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                controller
                    .release(LifecycleCheckpoint::SupervisorSelectedTask)
                    .unwrap();
                controller
                    .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .await
                    .unwrap();
            }
            _ => controller
                .release(LifecycleCheckpoint::SupervisorSelectedTask)
                .unwrap(),
        }
    }
    release.add_permits(1);
    let _ = read_http_head(&mut peer).await;
    assert_http_body(&mut peer, b"released").await;
    assert_eof(&mut peer).await;
    assert_task_panicked(handle.await, OWNED_TASK_PANIC);
}

// 1.T12: unexpected cancellation while Running is fatal.
#[camber::test]
async fn unexpected_owned_task_cancellation_while_running_is_fatal() {
    owned_task_fault_result(
        LifecycleFault::CancelNextOwnedTask,
        "owned HTTP task cancelled unexpectedly",
    )
    .await;
}

// 1.T12: runtime graceful starts one deadline; repeated graceful does not restart it.
#[tokio::test(start_paused = true)]
async fn runtime_graceful_and_repeated_graceful_share_one_deadline() {
    runtime::on_cancel(std::future::pending());
    let (controller, _addr, handle, peer, _release) = retained_server().await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "runtime deadline initial boundary timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedRuntime,
        "initial runtime graceful selection timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "post-runtime-graceful boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(20)).await;
    runtime::request_shutdown();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    tokio::time::advance(Duration::from_secs(10)).await;
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedDeadline,
        "original runtime deadline was restarted",
    )
    .await;
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::AfterSupervisorResultSend,
        "runtime deadline owned-task drain timed out",
    )
    .await;
    // This checkpoint proves the server-side transport owner was joined. The
    // blocking socket deadline observes the independent OS peer notification.
    assert_eof_with_socket_deadline(peer);
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
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
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    prepare_completed_task(&controller, addr).await;
    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    runtime::request_shutdown();
    handle.cancel();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedControl)
        .await
        .unwrap();
    observe_deferred_task_reap(&controller, LifecycleCheckpoint::SupervisorSelectedControl).await;
    assert_cancelled(handle.await);
}

// 1.T12: runtime wins over accept error and completed task; the accept error is dropped.
#[camber::test]
async fn runtime_branch_drops_losing_accept_error_and_reaps_task_next() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    prepare_completed_task(&controller, addr).await;
    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    runtime::request_shutdown();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .await
        .unwrap();
    observe_deferred_task_reap(&controller, LifecycleCheckpoint::SupervisorSelectedRuntime).await;
    assert!(handle.await.is_ok());
}

// 1.T12: a real accept beats an already-completed owned task.
#[camber::test]
async fn accept_branch_wins_over_task_then_reaps_task() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    prepare_completed_task(&controller, addr).await;
    let mut second = connect_request(addr).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
        .await
        .unwrap();
    observe_deferred_task_reap(&controller, LifecycleCheckpoint::SupervisorSelectedAccept).await;
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
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    prepare_completed_task(&controller, addr).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedTask)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedTask).await;
    handle.cancel();
    assert_cancelled(handle.await);
}

async fn shutdown_wins_over_ready_accept(forced: bool) {
    let counter = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    let handle = camber::http::serve_background(listener, counting_router(Arc::clone(&counter)));
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    let mut client = connect_request(addr).await;
    let selected = match forced {
        true => LifecycleCheckpoint::SupervisorSelectedControl,
        false => LifecycleCheckpoint::SupervisorSelectedRuntime,
    };
    controller.pause_once(selected).unwrap();
    match forced {
        true => handle.cancel(),
        false => runtime::request_shutdown(),
    }
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(&controller, selected).await;
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
                let controller = lifecycle(addr).unwrap();
                let handle =
                    camber::http::serve_background(listener, counting_router(Arc::clone(&counter)));
                let mut first = connect_request(addr).await;
                assert!(read_http_head(&mut first).await.starts_with("HTTP/1.1 200"));
                controller
                    .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .unwrap();
                let mut second = connect_request(addr).await;
                controller
                    .wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .await
                    .unwrap();
                controller
                    .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
                    .unwrap();
                handle.cancel();
                drop(first);
                controller
                    .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .unwrap();
                wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedControl).await;
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
    runtime::on_cancel(std::future::pending());
    let (controller, _addr, handle, peer, _release) = retained_server().await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedRuntime).await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    handle.cancel();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .await
        .unwrap();
    release_deadline_and_wait_for_drained_result(&controller).await;
    assert_eof_with_socket_deadline(peer);
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    assert_timeout(handle.await);
}

async fn pause_after_owned_panic_starts_grace() -> (
    LifecycleController,
    SocketAddr,
    ServerHandle,
    tokio::net::TcpStream,
    Arc<tokio::sync::Semaphore>,
) {
    let (controller, addr, handle, peer, release) = retained_server().await;
    prepare_faulted_task(&controller, addr, LifecycleFault::PanicNextOwnedTask).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedTask)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedTask)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedTask)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    (controller, addr, handle, peer, release)
}

async fn select_deadline(controller: &LifecycleController) {
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .await
        .unwrap();
}

async fn release_deadline_and_wait_for_drained_result(controller: &LifecycleController) {
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterSupervisorResultSend)
        .await
        .unwrap();
}

// 1.T12: deadline beats newly-ready captured runtime shutdown.
#[tokio::test(start_paused = true)]
async fn deadline_branch_wins_over_runtime() {
    runtime::on_cancel(std::future::pending());
    let (controller, _addr, handle, peer, _release) =
        pause_after_owned_panic_starts_grace().await;
    tokio::time::advance(Duration::from_secs(30)).await;
    runtime::request_shutdown();
    select_deadline(&controller).await;
    release_deadline_and_wait_for_drained_result(&controller).await;
    assert_eof_with_socket_deadline(peer);
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    assert_timeout(handle.await);
}

// 1.T12: deadline beats a real successful accept waiting in the listener.
#[tokio::test(start_paused = true)]
async fn deadline_branch_wins_over_accept_success() {
    runtime::on_cancel(std::future::pending());
    let (controller, addr, handle, peer, _release) = retained_server().await;
    prepare_faulted_task(&controller, addr, LifecycleFault::PanicNextOwnedTask).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedTask)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedTask)
        .await
        .unwrap();
    let waiting = connect_request(addr).await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedTask)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    select_deadline(&controller).await;
    release_deadline_and_wait_for_drained_result(&controller).await;
    assert_connection_closed_with_socket_deadline(waiting);
    assert_eof_with_socket_deadline(peer);
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    assert_timeout(handle.await);
}

// 1.T12: deadline beats an injected accept error, which remains unobserved.
#[tokio::test(start_paused = true)]
async fn deadline_branch_wins_over_accept_error() {
    runtime::on_cancel(std::future::pending());
    let (controller, _addr, handle, peer, _release) =
        pause_after_owned_panic_starts_grace().await;
    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    select_deadline(&controller).await;
    release_deadline_and_wait_for_drained_result(&controller).await;
    assert_eof_with_socket_deadline(peer);
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    assert_timeout(handle.await);
}

// 1.T12: deadline beats a completed owned task and that handle is reaped next.
#[tokio::test(start_paused = true)]
async fn deadline_branch_wins_over_completed_task_then_reaps_it() {
    runtime::on_cancel(std::future::pending());
    let (controller, _addr, handle, mut peer, release) =
        pause_after_owned_panic_starts_grace().await;
    release.add_permits(1);
    let response = read_http_head(&mut peer).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert_http_body(&mut peer, b"released").await;
    assert_eof(&mut peer).await;
    tokio::time::advance(Duration::from_secs(30)).await;
    select_deadline(&controller).await;
    observe_deferred_task_reap(&controller, LifecycleCheckpoint::SupervisorSelectedDeadline).await;
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
                let controller = lifecycle(addr).unwrap();
                let handle = camber::http::serve_background(
                    listener,
                    held_router(
                        entered_tx,
                        Arc::clone(&release),
                        Arc::new(AtomicBool::new(false)),
                    ),
                );
                let mut first = connect_request(addr).await;
                tokio::time::timeout(Duration::from_secs(5), entered_rx)
                    .await
                    .expect("first request did not enter the handler")
                    .unwrap();
                controller
                    .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .unwrap();
                let mut waiting = connect_request(addr).await;
                wait_until_paused_bounded(
                    &controller,
                    LifecycleCheckpoint::ConnectionPermitWaitPending,
                    "second request did not reach the pending permit wait",
                )
                .await;
                controller
                    .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
                    .unwrap();
                runtime::request_shutdown();
                controller
                    .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .unwrap();
                wait_until_paused_bounded(
                    &controller,
                    LifecycleCheckpoint::SupervisorSelectedRuntime,
                    "supervisor did not select runtime shutdown",
                )
                .await;
                controller
                    .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                controller
                    .release(LifecycleCheckpoint::SupervisorSelectedRuntime)
                    .unwrap();
                wait_until_paused_bounded(
                    &controller,
                    LifecycleCheckpoint::BeforeSupervisorSelect,
                    "supervisor did not reach the deadline/permit selection boundary",
                )
                .await;
                controller
                    .pause_once(LifecycleCheckpoint::SupervisorSelectedDeadline)
                    .unwrap();
                controller
                    .release(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                wait_until_paused_bounded(
                    &controller,
                    LifecycleCheckpoint::SupervisorSelectedDeadline,
                    "supervisor did not select the configured deadline",
                )
                .await;
                // Make the permit ready while the selected deadline branch is
                // paused but before that branch applies abort.
                release.add_permits(1);
                let response = read_http_head(&mut first).await;
                assert!(response.starts_with("HTTP/1.1 200"));
                assert_http_body(&mut first, b"released").await;
                assert_eof(&mut first).await;
                controller
                    .release(LifecycleCheckpoint::SupervisorSelectedDeadline)
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
    runtime::on_cancel(std::future::pending());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, websocket_router());
    let mut client = prepare_submitted_upgrade(&controller, addr).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    tokio::time::advance(Duration::from_secs(30)).await;
    select_deadline(&controller).await;
    let mut completion = Box::pin(handle.into_future());
    assert!(completion.as_mut().now_or_never().is_none());
    apply_selected_event_then_release_upgrade(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedDeadline,
        "deadline was not applied before releasing the submitted upgrade",
    )
    .await;
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 503"));
    assert!(response.to_ascii_lowercase().contains("connection: close"));
    assert_eof_with_independent_deadline(client).await;
    assert_timeout(completion.await);
}

// 1.T12: requests after the terminal send cannot replace the sent result.
#[camber::test]
async fn control_after_result_send_does_not_replace_io_result() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedAccept).await;
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterSupervisorResultSend)
        .await
        .unwrap();
    handle.cancel();
    handle.cancel();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    assert_io_kind(handle.await, std::io::ErrorKind::Other);
}

// 1.T12: Ok task completion while Graceful remains a successful server result.
#[camber::test]
async fn completed_task_in_graceful_mode_is_reaped_before_success() {
    let (controller, _addr, handle, mut peer, release) = retained_server().await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedTask)
        .unwrap();
    release.add_permits(1);
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedTask).await;
    let response = read_http_head(&mut peer).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    assert_http_body(&mut peer, b"released").await;
    assert_eof(&mut peer).await;
    assert!(handle.await.is_ok());
}

// 1.T12: CancelNextOwnedTask remains unexpected after runtime Graceful.
#[camber::test]
async fn unexpected_owned_task_cancellation_while_graceful_is_fatal() {
    let (controller, addr, handle, mut peer, release) = retained_server().await;
    prepare_faulted_task(&controller, addr, LifecycleFault::CancelNextOwnedTask).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .await
        .unwrap();
    observe_deferred_task_reap(&controller, LifecycleCheckpoint::SupervisorSelectedRuntime).await;
    release.add_permits(1);
    let _ = read_http_head(&mut peer).await;
    assert_http_body(&mut peer, b"released").await;
    assert_eof(&mut peer).await;
    assert_task_panicked(handle.await, "owned HTTP task cancelled unexpectedly");
}

#[cfg(feature = "ws")]
async fn prepare_submitted_upgrade(
    controller: &LifecycleController,
    addr: SocketAddr,
) -> tokio::net::TcpStream {
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .unwrap();
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(websocket_request("/ws").as_bytes())
        .await
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .await
        .unwrap();
    client
}

#[cfg(feature = "ws")]
async fn prepare_submitted_upgrade_with_limit(
    controller: &LifecycleController,
    addr: SocketAddr,
) -> tokio::net::TcpStream {
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .unwrap();
    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(websocket_request("/ws").as_bytes())
        .await
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedPermit)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedPermit)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedPermit)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .await
        .unwrap();
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
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, router);
    let client = prepare_submitted_upgrade(&controller, addr).await;
    drop(client);
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRegistration)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedRegistration,
    )
    .await;
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
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, websocket_router());
    let mut client = prepare_submitted_upgrade(&controller, addr).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    handle.cancel();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedControl)
        .await
        .unwrap();
    apply_selected_event_then_release_upgrade(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedControl,
        "forced control was not applied before releasing the submitted upgrade",
    )
    .await;
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 503"));
    assert!(response.to_ascii_lowercase().contains("connection: close"));
    assert_eof(&mut client).await;
    assert_cancelled(handle.await);
}

// 1.T12: runtime wins over a real buffered registration and rejects it with 503.
#[cfg(feature = "ws")]
#[camber::test]
async fn runtime_wins_over_submitted_registration_and_joins_wrapper() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, websocket_router());
    let mut client = prepare_submitted_upgrade(&controller, addr).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .await
        .unwrap();
    apply_selected_event_then_release_upgrade(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedRuntime,
        "runtime shutdown was not applied before releasing the submitted upgrade",
    )
    .await;
    let response = read_http_head(&mut client).await;
    assert!(response.starts_with("HTTP/1.1 503"));
    assert!(response.to_ascii_lowercase().contains("connection: close"));
    assert_eof(&mut client).await;
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
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, router);
    let mut upgrade = prepare_submitted_upgrade(&controller, addr).await;
    let mut ordinary = connect_request(addr).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    let response = read_http_head(&mut ordinary).await;
    assert!(response.starts_with("HTTP/1.1 200"));
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    handle.cancel();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedControl)
        .await
        .unwrap();
    apply_selected_event_then_release_upgrade(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedControl,
        "forced control was not applied before releasing the deferred upgrade",
    )
    .await;
    let response = read_http_head(&mut upgrade).await;
    assert!(response.starts_with("HTTP/1.1 503"));
    assert_eof(&mut upgrade).await;
    assert_cancelled(handle.await);
}

// 1.T12: a real permit becoming ready wins over a submitted registration.
#[cfg(feature = "ws")]
#[test]
fn permit_branch_wins_over_submitted_registration() {
    use camber::http::WsConn;

    runtime::builder()
        .connection_limit(2)
        .shutdown_timeout(Duration::from_secs(1))
        .run(|| {
            runtime::block_on(async {
                let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
                let release = Arc::new(tokio::sync::Semaphore::new(0));
                let mut router = held_router(
                    entered_tx,
                    Arc::clone(&release),
                    Arc::new(AtomicBool::new(false)),
                );
                router.ws("/ws", |_request: &Request, mut connection: WsConn| {
                    while connection.recv().is_some() {}
                    Ok(())
                });
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let controller = lifecycle(addr).unwrap();
                let handle = camber::http::serve_background(listener, router);
                let mut ordinary = tokio::time::timeout(
                    Duration::from_secs(5),
                    tokio::net::TcpStream::connect(addr),
                )
                .await
                .expect("ordinary connection timed out")
                .unwrap();
                ordinary.write_all(CLOSE_REQUEST).await.unwrap();
                tokio::time::timeout(Duration::from_secs(5), entered_rx)
                    .await
                    .expect("ordinary request dispatch timed out")
                    .unwrap();

                let mut upgrade = tokio::time::timeout(
                    Duration::from_secs(5),
                    prepare_submitted_upgrade_with_limit(&controller, addr),
                )
                .await
                .expect("upgrade ticket submission timed out");

                let mut waiting =
                    tokio::time::timeout(Duration::from_secs(5), connect_request(addr))
                        .await
                        .expect("permit-waiting connection timed out");
                controller
                    .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
                    .unwrap();
                controller
                    .release(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                wait_until_paused_bounded(
                    &controller,
                    LifecycleCheckpoint::SupervisorSelectedAccept,
                    "permit-waiting accept selection timed out",
                )
                .await;
                controller
                    .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                controller
                    .release(LifecycleCheckpoint::SupervisorSelectedAccept)
                    .unwrap();
                wait_until_paused_bounded(
                    &controller,
                    LifecycleCheckpoint::BeforeSupervisorSelect,
                    "pre-permit supervisor boundary timed out",
                )
                .await;
                wait_until_paused_bounded(
                    &controller,
                    LifecycleCheckpoint::AfterUpgradeTicketSubmitted,
                    "submitted upgrade ticket confirmation timed out",
                )
                .await;
                controller
                    .pause_once(LifecycleCheckpoint::SupervisorSelectedPermit)
                    .unwrap();
                release.add_permits(1);
                let response =
                    tokio::time::timeout(Duration::from_secs(5), read_http_head(&mut ordinary))
                        .await
                        .expect("ordinary response timed out");
                assert!(response.starts_with("HTTP/1.1 200"));
                assert_http_body(&mut ordinary, b"released").await;
                assert_eof(&mut ordinary).await;
                controller
                    .release(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                wait_until_paused_bounded(
                    &controller,
                    LifecycleCheckpoint::SupervisorSelectedPermit,
                    "permit selection timed out",
                )
                .await;
                controller
                    .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                controller
                    .release(LifecycleCheckpoint::SupervisorSelectedPermit)
                    .unwrap();
                wait_until_paused_bounded(
                    &controller,
                    LifecycleCheckpoint::BeforeSupervisorSelect,
                    "post-permit supervisor boundary timed out",
                )
                .await;
                release.add_permits(1);
                assert!(
                    tokio::time::timeout(Duration::from_secs(5), read_http_head(&mut waiting),)
                        .await
                        .expect("permit-waiting response timed out")
                        .starts_with("HTTP/1.1 200")
                );

                controller
                    .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
                    .unwrap();
                handle.cancel();
                controller
                    .release(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                wait_until_paused_bounded(
                    &controller,
                    LifecycleCheckpoint::SupervisorSelectedControl,
                    "control selection timed out",
                )
                .await;
                apply_selected_event_then_release_upgrade(
                    &controller,
                    LifecycleCheckpoint::SupervisorSelectedControl,
                    "forced control was not applied before releasing the deferred upgrade",
                )
                .await;
                let response =
                    tokio::time::timeout(Duration::from_secs(5), read_http_head(&mut upgrade))
                        .await
                        .expect("upgrade rejection response timed out");
                assert!(
                    response.starts_with("HTTP/1.1 503"),
                    "unexpected upgrade rejection response: {response}"
                );
                let result = tokio::time::timeout(Duration::from_secs(5), handle.into_future())
                    .await
                    .expect("permit fixture owner finalization timed out");
                assert_cancelled(result);
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
                let controller = lifecycle(addr).unwrap();
                let handle = camber::http::serve_background(listener, ok_router());
                let mut first = connect_request(addr).await;
                assert!(read_http_head(&mut first).await.starts_with("HTTP/1.1 200"));
                controller
                    .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .unwrap();
                let mut second = connect_request(addr).await;
                controller
                    .wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .await
                    .unwrap();
                controller
                    .pause_once(LifecycleCheckpoint::SupervisorSelectedPermit)
                    .unwrap();
                drop(first);
                controller
                    .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .unwrap();
                wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedPermit).await;
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

// 1.T12: a submitted registration beats an already-completed task.
#[cfg(feature = "ws")]
#[camber::test]
async fn registration_branch_wins_over_completed_task() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let mut router = websocket_router();
    router.get("/", |_request: &Request| async {
        Response::text(200, "ok")
    });
    let handle = camber::http::serve_background(listener, router);
    prepare_completed_task(&controller, addr).await;

    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
    client
        .write_all(websocket_request("/ws").as_bytes())
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedAccept)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRegistration)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedRegistration)
        .await
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedRegistration)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .await
        .unwrap();
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedTask)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedTask).await;
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
    let listener = tokio_runtime.block_on(async {
        runtime::on_cancel(std::future::pending());
        tokio::net::TcpListener::from_std(std_listener).unwrap()
    });
    drop(tokio_runtime);

    let result = camber::http::serve_background(listener, ok_router())
        .into_future()
        .now_or_never()
        .expect("constructor error must already be ready");
    match result {
        Err(RuntimeError::InvalidArgument(message)) => assert_eq!(
            message.as_ref(),
            "serve_background requires an active Tokio runtime"
        ),
        other => panic!("expected ready InvalidArgument, got {other:?}"),
    }
}

// 1.T13
#[tokio::test(start_paused = true)]
async fn standalone_background_ignores_unrelated_camber_shutdown_and_uses_defaults() {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let mut router = held_router(entered_tx, release, Arc::new(AtomicBool::new(false)));
    router.get("/second", |_req: &Request| async {
        Response::text(200, "second")
    });
    let handle = camber::http::serve_background(listener, router);
    let _first = connect_request(addr).await;
    entered_rx.await.unwrap();

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

    controller
        .pause_once(LifecycleCheckpoint::AfterAccept)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterPermit)
        .unwrap();
    let keepalive_checkpoint =
        LifecycleCheckpoint::KeepaliveTimeoutConfigured(Duration::from_secs(60));
    controller.pause_once(keepalive_checkpoint).unwrap();
    let mut partial = tokio::net::TcpStream::connect(addr).await.unwrap();
    partial.write_all(b"GET / HTTP/1.1\r\nHost:").await.unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterAccept)
        .await
        .unwrap();
    controller
        .release(LifecycleCheckpoint::AfterAccept)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterPermit)
        .await
        .unwrap();
    controller
        .release(LifecycleCheckpoint::AfterPermit)
        .unwrap();
    wait_and_release(&controller, keepalive_checkpoint).await;
    drop(partial);

    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    tokio::task::yield_now().await;
    let mut completion = Box::pin(handle.into_future());
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

struct CapturingLayer {
    events: Arc<Mutex<Vec<Box<str>>>>,
}

impl<S> tracing_subscriber::Layer<S> for CapturingLayer
where
    S: camber::tracing::Subscriber,
{
    fn on_event(
        &self,
        event: &camber::tracing::Event<'_>,
        _context: tracing_subscriber::layer::Context<'_, S>,
    ) {
        struct Visitor(String);

        impl camber::tracing::field::Visit for Visitor {
            fn record_debug(
                &mut self,
                field: &camber::tracing::field::Field,
                value: &dyn std::fmt::Debug,
            ) {
                self.0.push_str(field.name());
                self.0.push('=');
                self.0.push_str(&format!("{value:?}"));
                self.0.push(' ');
            }
        }

        let mut visitor = Visitor(String::new());
        event.record(&mut visitor);
        self.events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(visitor.0.into_boxed_str());
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
            let controller = lifecycle(listener.local_addr().unwrap()).unwrap();
            controller
                .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
                .unwrap();
            controller
                .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
                .unwrap();
            let _handle = camber::http::serve_background(listener, ok_router());
            let returned = Arc::clone(&run_returned);
            let paused = Arc::clone(&supervisor_was_paused);
            let thread = std::thread::spawn(move || {
                let tokio_runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap();
                tokio_runtime.block_on(async {
                    controller
                        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
                        .await
                        .unwrap();
                    assert!(!returned.load(Ordering::Acquire));
                    paused.store(true, Ordering::Release);
                    controller
                        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
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
        .keepalive_timeout(Duration::from_millis(150))
        .shutdown_timeout(Duration::from_secs(1))
        .with_metrics()
        .resource(HealthyResource)
        .run(|| {
            runtime::block_on(async {
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let controller = lifecycle(addr).unwrap();
                let handle = camber::http::serve_background(listener, ok_router());
                let health = reqwest::get(format!("http://{addr}/health")).await.unwrap();
                assert_eq!(health.status(), 200);
                drop(health);
                let metrics = reqwest::get(format!("http://{addr}/metrics"))
                    .await
                    .unwrap();
                assert_eq!(metrics.status(), 200);
                drop(metrics);

                controller
                    .pause_once(LifecycleCheckpoint::AfterPermit)
                    .unwrap();
                let mut partial = tokio::net::TcpStream::connect(addr).await.unwrap();
                controller
                    .wait_until_paused(LifecycleCheckpoint::AfterPermit)
                    .await
                    .unwrap();
                controller
                    .release(LifecycleCheckpoint::AfterPermit)
                    .unwrap();
                partial.write_all(b"GET / HTTP/1.1\r\nHost:").await.unwrap();
                controller
                    .pause_once(LifecycleCheckpoint::AfterPermit)
                    .unwrap();
                let mut blocked = connect_request(addr).await;
                let mut byte = [0u8; 1];
                assert!(
                    tokio::time::timeout(Duration::ZERO, blocked.read(&mut byte))
                        .await
                        .is_err(),
                    "connection limit did not block a second dispatch"
                );
                tokio::time::timeout(Duration::from_secs(1), assert_eof(&mut partial))
                    .await
                    .expect("configured keepalive timeout was not captured");
                controller
                    .wait_until_paused(LifecycleCheckpoint::AfterPermit)
                    .await
                    .unwrap();
                controller
                    .release(LifecycleCheckpoint::AfterPermit)
                    .unwrap();
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
                let handle = camber::http::serve_background(listener, router);
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
                let controller = lifecycle(addr).unwrap();
                controller
                    .pause_once(LifecycleCheckpoint::SseBufferConfigured(3))
                    .unwrap();
                #[cfg(feature = "ws")]
                {
                    controller
                        .pause_once(LifecycleCheckpoint::WebSocketOutgoingBufferConfigured(5))
                        .unwrap();
                    controller
                        .pause_once(LifecycleCheckpoint::WebSocketIncomingBufferConfigured(5))
                        .unwrap();
                }
                let handle = camber::http::serve_background(listener, router);
                let mut sse_client = tokio::net::TcpStream::connect(addr).await.unwrap();
                sse_client
                    .write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
                    .await
                    .unwrap();
                wait_and_release(&controller, LifecycleCheckpoint::SseBufferConfigured(3)).await;
                let response = read_http_head(&mut sse_client).await;
                assert!(response.starts_with("HTTP/1.1 200"));
                let event = tokio::time::timeout(Duration::from_secs(5), async {
                    let mut event = Vec::new();
                    let mut chunk = [0u8; 128];
                    loop {
                        let read = sse_client.read(&mut chunk).await.unwrap();
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

                #[cfg(feature = "ws")]
                {
                    let mut client = tokio::net::TcpStream::connect(addr).await.unwrap();
                    client
                        .write_all(websocket_request("/buffered-ws").as_bytes())
                        .await
                        .unwrap();
                    wait_and_release(
                        &controller,
                        LifecycleCheckpoint::WebSocketOutgoingBufferConfigured(5),
                    )
                    .await;
                    wait_and_release(
                        &controller,
                        LifecycleCheckpoint::WebSocketIncomingBufferConfigured(5),
                    )
                    .await;
                    let response = read_http_head(&mut client).await;
                    assert!(response.starts_with("HTTP/1.1 101"));
                    write_websocket_text(&mut client, "ws-ok").await;
                    assert_eq!(read_websocket_text(&mut client).await.as_ref(), "ws-ok");
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
    let (cert, key) = common::generate_self_signed_cert();
    let tls_config = common::server_tls_config(&cert, &key);
    let client_config = common::tls_client_config(&[&cert]);
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
                ));

                let mut router = Router::new();
                router.proxy("/proxy", &format!("http://{backend_addr}"));
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let handle = camber::http::serve_background_tls(listener, router, tls_config);
                let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
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
                backend.abort();
                assert!(backend.await.unwrap_err().is_cancelled());
            });
        })
        .unwrap();
}

// 1.T13
#[tokio::test]
async fn standalone_tls_background_marks_proxy_scheme_as_https() {
    let (cert, key) = common::generate_self_signed_cert();
    let tls_config = common::server_tls_config(&cert, &key);
    let client_config = common::tls_client_config(&[&cert]);
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
    let backend = tokio::spawn(camber::http::serve_async(backend_listener, backend_router));
    let mut router = Router::new();
    router.proxy("/proxy", &format!("http://{backend_addr}"));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background_tls(listener, router, tls_config);
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
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
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
        .await
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    handle.cancel();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedControl).await;
    assert_cancelled(handle.await);
    backend.abort();
    assert!(backend.await.unwrap_err().is_cancelled());
}

// 1.T13
#[test]
fn configured_background_captures_request_tracing() {
    use tracing_subscriber::layer::SubscriberExt;

    let events = Arc::new(Mutex::new(Vec::new()));
    let subscriber = tracing_subscriber::registry().with(CapturingLayer {
        events: Arc::clone(&events),
    });
    camber::tracing::subscriber::set_global_default(subscriber).unwrap();

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
        let controller = lifecycle(addr).unwrap();
        let handle = camber::http::serve_background(listener, router);
        assert_ok_request_path(addr, "/standalone-untraced").await;
        controller
            .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
            .unwrap();
        controller
            .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
            .await
            .unwrap();
        handle.cancel();
        controller
            .release(LifecycleCheckpoint::BeforeSupervisorSelect)
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
                let handle = camber::http::serve_background(listener, router);
                assert_ok_request_path(addr, "/captured-lifecycle").await;
                runtime::request_shutdown();
                assert!(handle.await.is_ok());
            });
        })
        .unwrap();

    let combined = events
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .join(" ");
    assert!(!combined.contains("standalone-untraced"));
    assert!(combined.contains("captured-lifecycle"));
    assert!(combined.contains("status=200"));
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
                let handle = camber::http::serve_background(listener, Router::new());
                let response = reqwest::get(format!("http://{addr}/debug/pprof/cpu?seconds=1"))
                    .await
                    .unwrap();
                assert_eq!(response.status(), 200);
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
    let controller = lifecycle(listener.local_addr().unwrap()).unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeRuntimeWait)
        .unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeRuntimeWait)
        .await
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeRuntimeWait)
        .unwrap();
    assert!(handle.await.is_ok());
}

// 1.T16
#[camber::test]
async fn on_cancel_cannot_be_missed_after_runtime_wait_registration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let controller = lifecycle(listener.local_addr().unwrap()).unwrap();
    controller
        .pause_once(LifecycleCheckpoint::BeforeRuntimeWait)
        .unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeRuntimeWait)
        .await
        .unwrap();
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    runtime::on_cancel(async move {
        let _ = cancel_rx.await;
    });
    cancel_tx.send(()).unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeRuntimeWait)
        .unwrap();
    assert!(handle.await.is_ok());
}

#[cfg(unix)]
const CHILD_PROTOCOL_ENV: &str = "CAMBER_OWNED_SERVER_CHILD_PROTOCOL";

#[cfg(unix)]
const PROTOCOL_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

#[cfg(unix)]
const PROTOCOL_POLL_INTERVAL: Duration = Duration::from_millis(10);

#[cfg(unix)]
#[derive(Debug, thiserror::Error)]
enum ProtocolChildError {
    #[error("protocol child did not exit before its deadline")]
    ExitTimeout,
    #[error("protocol child could not be inspected: {0:?}")]
    TryWait(std::io::ErrorKind),
    #[error("protocol child could not be terminated: {0:?}")]
    Kill(std::io::ErrorKind),
    #[error("protocol child was not reaped before its cleanup deadline")]
    ReapTimeout,
    #[error("protocol child did not print the requested checkpoint before its deadline")]
    CheckpointTimeout,
    #[error("protocol child stdout closed before the requested checkpoint")]
    CheckpointStreamClosed,
    #[error("protocol child stdout could not be read: {0:?}")]
    StdoutRead(std::io::ErrorKind),
    #[error("protocol child stdout receiver closed while the reader was active")]
    StdoutReceiverClosed,
    #[error("protocol child stdout reader did not finish before its cleanup deadline")]
    StdoutReaderTimeout,
    #[error("protocol child stdout reader panicked")]
    StdoutReaderPanicked,
}

#[cfg(unix)]
#[derive(Default)]
struct ProtocolCleanupState {
    process_reaped: AtomicBool,
    stdout_eof: AtomicBool,
    reader_joined: AtomicBool,
    error: Mutex<Option<Box<str>>>,
}

#[cfg(unix)]
struct ProtocolCleanupProbe(Arc<ProtocolCleanupState>);

#[cfg(unix)]
impl ProtocolCleanupProbe {
    fn process_reaped(&self) -> bool {
        self.0.process_reaped.load(Ordering::Acquire)
    }

    fn stdout_eof(&self) -> bool {
        self.0.stdout_eof.load(Ordering::Acquire)
    }

    fn reader_joined(&self) -> bool {
        self.0.reader_joined.load(Ordering::Acquire)
    }

    fn cleanup_error(&self) -> Option<Box<str>> {
        self.0
            .error
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

#[cfg(unix)]
struct ProtocolChild {
    child: Option<Child>,
    lines: std::sync::mpsc::Receiver<Box<str>>,
    reader: Option<std::thread::JoinHandle<Result<(), ProtocolChildError>>>,
    cleanup: Arc<ProtocolCleanupState>,
}

#[cfg(unix)]
impl ProtocolChild {
    fn cleanup_probe(&self) -> ProtocolCleanupProbe {
        ProtocolCleanupProbe(Arc::clone(&self.cleanup))
    }

    fn wait_for_line(
        &self,
        prefix: &str,
        timeout: Duration,
    ) -> Result<Box<str>, ProtocolChildError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match self.lines.recv_timeout(remaining) {
                Ok(line) if line.starts_with(prefix) => return Ok(line),
                Ok(_) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(ProtocolChildError::CheckpointTimeout);
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ProtocolChildError::CheckpointStreamClosed);
                }
            }
        }
    }

    fn wait_bounded(
        &mut self,
        timeout: Duration,
    ) -> Result<std::process::ExitStatus, ProtocolChildError> {
        match self.reap_bounded(timeout)? {
            Some(status) => {
                self.join_reader_bounded(timeout)?;
                Ok(status)
            }
            None => self.exit_timeout_after_cleanup(),
        }
    }

    fn exit_timeout_after_cleanup(
        &mut self,
    ) -> Result<std::process::ExitStatus, ProtocolChildError> {
        match self.cleanup_bounded(PROTOCOL_CLEANUP_TIMEOUT) {
            Ok(()) => Err(ProtocolChildError::ExitTimeout),
            Err(error) => Err(error),
        }
    }

    fn reap_bounded(
        &mut self,
        timeout: Duration,
    ) -> Result<Option<std::process::ExitStatus>, ProtocolChildError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.try_reap()? {
                Some(status) => return Ok(Some(status)),
                None if Instant::now() < deadline => std::thread::sleep(PROTOCOL_POLL_INTERVAL),
                None => return Ok(None),
            }
        }
    }

    fn try_reap(&mut self) -> Result<Option<std::process::ExitStatus>, ProtocolChildError> {
        let Some(child) = self.child.as_mut() else {
            return Ok(None);
        };
        let status = child
            .try_wait()
            .map_err(|error| ProtocolChildError::TryWait(error.kind()))?;
        if status.is_some() {
            self.child.take();
            self.cleanup.process_reaped.store(true, Ordering::Release);
        }
        Ok(status)
    }

    fn cleanup_bounded(&mut self, timeout: Duration) -> Result<(), ProtocolChildError> {
        self.terminate_bounded(timeout)?;
        self.join_reader_bounded(timeout)
    }

    fn terminate_bounded(&mut self, timeout: Duration) -> Result<(), ProtocolChildError> {
        match self.try_reap()? {
            Some(_) => return Ok(()),
            None if self.child.is_none() => return Ok(()),
            None => {}
        }
        let kill_result = self.child.as_mut().map(Child::kill);
        match kill_result {
            Some(Ok(())) => {}
            Some(Err(error)) => return self.resolve_kill_error(error),
            None => return Ok(()),
        }
        match self.reap_bounded(timeout)? {
            Some(_) => Ok(()),
            None => Err(ProtocolChildError::ReapTimeout),
        }
    }

    fn resolve_kill_error(&mut self, error: std::io::Error) -> Result<(), ProtocolChildError> {
        match self.try_reap()? {
            Some(_) => Ok(()),
            None => Err(ProtocolChildError::Kill(error.kind())),
        }
    }

    fn join_reader_bounded(&mut self, timeout: Duration) -> Result<(), ProtocolChildError> {
        let deadline = Instant::now() + timeout;
        loop {
            match self.reader.as_ref() {
                Some(reader) if reader.is_finished() => break,
                Some(_) if Instant::now() < deadline => std::thread::sleep(PROTOCOL_POLL_INTERVAL),
                Some(_) => return Err(ProtocolChildError::StdoutReaderTimeout),
                None => return Ok(()),
            }
        }
        let reader = self.reader.take();
        let result = match reader {
            Some(reader) => reader
                .join()
                .map_err(|_| ProtocolChildError::StdoutReaderPanicked)?,
            None => return Ok(()),
        };
        self.cleanup.reader_joined.store(true, Ordering::Release);
        result?;
        self.cleanup.stdout_eof.store(true, Ordering::Release);
        Ok(())
    }
}

#[cfg(unix)]
impl Drop for ProtocolChild {
    fn drop(&mut self) {
        if self.child.is_none() && self.reader.is_none() {
            return;
        }
        if let Err(error) = self.cleanup_bounded(PROTOCOL_CLEANUP_TIMEOUT) {
            let mut cleanup_error = self
                .cleanup
                .error
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *cleanup_error = Some(error.to_string().into_boxed_str());
        }
    }
}

#[cfg(unix)]
fn spawn_protocol_child(protocol: &str) -> ProtocolChild {
    let mut child = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("lifecycle_signal_child")
        .arg("--nocapture")
        .env(CHILD_PROTOCOL_ENV, protocol)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (sender, receiver) = std::sync::mpsc::channel();
    let reader = std::thread::spawn(move || {
        let reader = std::io::BufReader::new(stdout);
        for line in std::io::BufRead::lines(reader) {
            let line = line.map_err(|error| ProtocolChildError::StdoutRead(error.kind()))?;
            sender
                .send(line.into_boxed_str())
                .map_err(|_| ProtocolChildError::StdoutReceiverClosed)?;
        }
        Ok(())
    });
    ProtocolChild {
        child: Some(child),
        lines: receiver,
        reader: Some(reader),
        cleanup: Arc::new(ProtocolCleanupState::default()),
    }
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
    let probe = child.cleanup_probe();
    let ready = child
        .wait_for_line("SERVER_READY", Duration::from_secs(5))
        .unwrap();
    let control_addr = parse_address(&ready);
    connect_control(control_addr, b"RAISE", Duration::from_secs(5));
    child
        .wait_for_line("DRAINED", Duration::from_secs(5))
        .unwrap();
    let status = child.wait_bounded(Duration::from_secs(5)).unwrap();
    assert!(status.success(), "active-signal child failed: {status}");
    assert_protocol_cleanup(&probe);
}

// 1.T16
#[cfg(unix)]
#[test]
fn closure_return_does_not_transition_owned_server() {
    let mut child = spawn_protocol_child("closure-return");
    let probe = child.cleanup_probe();
    let returning = child
        .wait_for_line("CLOSURE_RETURNING", Duration::from_secs(2))
        .unwrap();
    let mut fields = returning.split_whitespace();
    assert_eq!(fields.next(), Some("CLOSURE_RETURNING"));
    let http_addr: SocketAddr = fields.next().unwrap().parse().unwrap();
    let control_addr: SocketAddr = fields.next().unwrap().parse().unwrap();
    let response = bounded_raw_request(http_addr, Duration::from_secs(2));
    assert_eq!(common::status_from_raw(&response), 200);
    connect_control(control_addr, b"CANCEL", Duration::from_secs(2));
    child
        .wait_for_line("AFTER_RUN", Duration::from_secs(2))
        .unwrap();
    let status = child.wait_bounded(Duration::from_secs(2)).unwrap();
    assert!(status.success(), "closure-return child failed: {status}");
    assert_protocol_cleanup(&probe);
}

// 1.T16
#[cfg(unix)]
#[test]
fn signal_after_watcher_teardown_only_guarantees_process_survival() {
    let mut child = spawn_protocol_child("post-watcher");
    let probe = child.cleanup_probe();
    child
        .wait_for_line("WATCHER_GONE", Duration::from_secs(5))
        .unwrap();
    child
        .wait_for_line("SURVIVED", Duration::from_secs(5))
        .unwrap();
    let status = child.wait_bounded(Duration::from_secs(5)).unwrap();
    assert!(status.success(), "post-watcher child failed: {status}");
    assert_protocol_cleanup(&probe);
}

#[cfg(unix)]
fn assert_protocol_cleanup(probe: &ProtocolCleanupProbe) {
    assert!(probe.process_reaped(), "protocol child was not reaped");
    assert!(
        probe.stdout_eof(),
        "protocol child stdout did not reach EOF"
    );
    assert!(
        probe.reader_joined(),
        "protocol stdout reader was not joined"
    );
    assert_eq!(probe.cleanup_error(), None, "protocol cleanup failed");
}

#[cfg(unix)]
#[test]
fn protocol_child_timeout_reaps_process_and_joins_stdout() {
    let mut child = spawn_protocol_child("cleanup-hold");
    let probe = child.cleanup_probe();
    child
        .wait_for_line("CLEANUP_READY", Duration::from_secs(5))
        .unwrap();

    let result = child.wait_bounded(Duration::ZERO);
    assert!(matches!(result, Err(ProtocolChildError::ExitTimeout)));
    assert_protocol_cleanup(&probe);
}

#[cfg(unix)]
#[test]
fn protocol_child_assertion_unwind_reaps_process_and_joins_stdout() {
    let child = spawn_protocol_child("cleanup-hold");
    let probe = child.cleanup_probe();
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let child = child;
        child
            .wait_for_line("CLEANUP_READY", Duration::from_secs(5))
            .unwrap();
        assert!(false, "intentional protocol assertion failure");
    }));

    assert!(unwind.is_err(), "protocol assertion did not unwind");
    assert_protocol_cleanup(&probe);
}

#[cfg(unix)]
#[test]
fn protocol_child_lifecycle_stress_iteration() {
    active_runtime_signal_drains_owned_server_before_child_exit();
    closure_return_does_not_transition_owned_server();
    signal_after_watcher_teardown_only_guarantees_process_survival();
    protocol_child_timeout_reaps_process_and_joins_stdout();
    protocol_child_assertion_unwind_reaps_process_and_joins_stdout();
}

#[cfg(unix)]
#[test]
fn lifecycle_signal_child() {
    let Ok(protocol) = std::env::var(CHILD_PROTOCOL_ENV) else {
        return;
    };
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
                let controller = lifecycle(http_addr).unwrap();
                controller
                    .pause_once(LifecycleCheckpoint::BeforeRuntimeWait)
                    .unwrap();
                let handle = camber::http::serve_background(listener, ok_router());
                let mut retained =
                    tokio::time::timeout(Duration::from_secs(5), connect_request(http_addr))
                        .await
                        .expect("initial child request timed out");
                let response =
                    tokio::time::timeout(Duration::from_secs(5), read_http_head(&mut retained))
                        .await
                        .expect("initial child response timed out");
                assert!(response.starts_with("HTTP/1.1 200"));
                assert_http_body(&mut retained, b"ok").await;
                let control = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let control_addr = control.local_addr().unwrap();
                tokio::task::yield_now().await;
                tokio::time::timeout(
                    Duration::from_secs(5),
                    controller.wait_until_paused(LifecycleCheckpoint::BeforeRuntimeWait),
                )
                .await
                .expect("runtime waiter checkpoint timed out")
                .unwrap();
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
                    controller
                        .release(LifecycleCheckpoint::BeforeRuntimeWait)
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
                let handle = camber::http::serve_background(listener, ok_router());
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
    let handle = camber::http::serve_background(listener, ok_router());

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
    let handle = camber::http::serve_background(listener, router);
    let websocket = websocket_client(addr, "/ws").await;
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
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, websocket_router());
    let mut acknowledged = websocket_client(addr, "/ws").await;

    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .unwrap();
    let mut pending = tokio::net::TcpStream::connect(addr).await.unwrap();
    pending
        .write_all(websocket_request("/ws").as_bytes())
        .await
        .unwrap();
    wait_and_release(
        &controller,
        LifecycleCheckpoint::AfterUpgradeTicketSubmitted,
    )
    .await;
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .await
        .unwrap();
    controller
        .inject_once(LifecycleFault::PanicSupervisorCore)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .unwrap();

    assert_eof(&mut acknowledged).await;
    let pending_response = read_http_head(&mut pending).await;
    assert!(
        pending_response.starts_with("HTTP/1.1 500"),
        "pending upgrade committed an unexpected response: {pending_response}"
    );
    assert!(
        pending_response
            .to_ascii_lowercase()
            .contains("connection: close"),
        "supervisor-unavailable response did not close the connection: {pending_response}"
    );
    assert_eof(&mut pending).await;
    match handle.await {
        Err(RuntimeError::TaskPanicked(message)) => assert!(!message.is_empty()),
        other => panic!("expected supervisor panic after draining upgrades, got {other:?}"),
    }
}

// 2.T1
#[tokio::test]
async fn every_owner_form_waits_until_owned_tasks_are_empty() {
    let (controller, addr, handle, client, release, dropped) = retained_owner_server().await;
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    handle.shutdown();
    assert_graceful_owner_waits(
        &controller,
        handle.into_future(),
        addr,
        client,
        release,
        dropped,
    )
    .await;

    let (controller, addr, handle, client, release, dropped) = retained_owner_server().await;
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    let future = handle.join();
    future.shutdown();
    assert_graceful_owner_waits(&controller, future, addr, client, release, dropped).await;

    let (controller, addr, handle, client, release, dropped) = retained_owner_server().await;
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    let future = handle.shutdown_and_join();
    assert_graceful_owner_waits(&controller, future, addr, client, release, dropped).await;
}

// 2.T2
#[tokio::test]
async fn join_transfers_control_without_stopping_admission() {
    let counter = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, counting_router(Arc::clone(&counter)));
    let mut future = Box::pin(handle.join());
    assert!(future.as_mut().now_or_never().is_none());
    tokio::time::timeout(Duration::from_secs(5), assert_ok_request(addr))
        .await
        .expect("admission after join timed out");
    assert_eq!(counter.load(Ordering::Acquire), 1);

    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "join admission boundary timed out",
    )
    .await;
    let mut waiting = tokio::time::timeout(Duration::from_secs(5), connect_request(addr))
        .await
        .expect("equal-ready accepted connection timed out");
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    future.as_ref().get_ref().shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedControl,
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
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, ok_router());
    let mut future = Box::pin(handle.join());
    tokio::time::timeout(
        Duration::from_secs(5),
        prepare_completed_task(&controller, addr),
    )
    .await
    .expect("completed-task preparation timed out");
    controller
        .inject_once(LifecycleFault::Accept(std::io::ErrorKind::Other))
        .unwrap();
    runtime::request_shutdown();
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    future.as_ref().get_ref().shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedControl,
        "owner graceful selection timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "post-owner-graceful boundary timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedRuntime,
        "equal-ready runtime selection timed out",
    )
    .await;
    tokio::time::timeout(
        Duration::from_secs(5),
        observe_deferred_task_reap(&controller, LifecycleCheckpoint::SupervisorSelectedRuntime),
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
                let controller = lifecycle(addr).unwrap();
                let handle = camber::http::serve_background(
                    listener,
                    held_router(
                        entered_tx,
                        Arc::clone(&release),
                        Arc::new(AtomicBool::new(false)),
                    ),
                );
                let mut first = tokio::net::TcpStream::connect(addr).await.unwrap();
                first.write_all(CLOSE_REQUEST).await.unwrap();
                entered_rx.await.unwrap();
                controller
                    .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                controller
                    .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .await
                    .unwrap();
                let mut waiting = connect_request(addr).await;
                controller
                    .pause_once(LifecycleCheckpoint::SupervisorSelectedAccept)
                    .unwrap();
                controller
                    .release(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                controller
                    .wait_until_paused(LifecycleCheckpoint::SupervisorSelectedAccept)
                    .await
                    .unwrap();
                controller
                    .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                controller
                    .release(LifecycleCheckpoint::SupervisorSelectedAccept)
                    .unwrap();
                controller
                    .wait_until_paused(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .await
                    .unwrap();
                let mut future = Box::pin(handle.join());
                controller
                    .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
                    .unwrap();
                release.add_permits(1);
                let response = read_http_head(&mut first).await;
                assert!(response.starts_with("HTTP/1.1 200"));
                assert_http_body(&mut first, b"released").await;
                assert_eof(&mut first).await;
                future.as_ref().get_ref().shutdown();
                controller
                    .release(LifecycleCheckpoint::BeforeSupervisorSelect)
                    .unwrap();
                wait_and_release(&controller, LifecycleCheckpoint::SupervisorSelectedControl).await;
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
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(listener, websocket_router());
    let mut future = Box::pin(handle.join());
    let mut client = tokio::time::timeout(
        Duration::from_secs(5),
        prepare_submitted_upgrade(&controller, addr),
    )
    .await
    .expect("submitted-registration preparation timed out");
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    future.as_ref().get_ref().shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedControl,
        "owner registration precedence selection timed out",
    )
    .await;
    apply_selected_event_then_release_upgrade(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedControl,
        "owner graceful shutdown was not applied before releasing the submitted upgrade",
    )
    .await;
    let response = read_http_head_bounded(
        &mut client,
        "owner registration rejection response timed out",
    )
    .await;
    assert!(response.starts_with("HTTP/1.1 503"));
    assert!(response.to_ascii_lowercase().contains("connection: close"));
    assert_eof(&mut client).await;
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
    runtime::on_cancel(std::future::pending());
    let (controller, _addr, handle, peer, _release) = retained_server().await;
    let mut future = Box::pin(handle.join());
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "owner deadline initial boundary timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    future.as_ref().get_ref().shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedControl,
        "owner deadline graceful selection timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "owner deadline post-graceful boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(10)).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    runtime::request_shutdown();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedRuntime,
        "owner deadline runtime selection timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedRuntime)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "owner deadline final boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(20)).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedDeadline,
        "original owner deadline was restarted",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .unwrap();
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::AfterSupervisorResultSend,
        "owner deadline task drain timed out",
    )
    .await;
    assert_eof_with_socket_deadline(peer);
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
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
    assert_forced_owner_joins(handle.into_future(), client, dropped).await;

    let (_controller, _addr, handle, client, _release, dropped) = retained_owner_server().await;
    let future = handle.join();
    future.cancel();
    assert_forced_owner_joins(future, client, dropped).await;
}

async fn assert_dropped_owner_continues_abort(
    controller: &LifecycleController,
    addr: SocketAddr,
    client: tokio::net::TcpStream,
    dropped: &AtomicBool,
) {
    wait_until_paused_bounded(
        controller,
        LifecycleCheckpoint::SupervisorSelectedControl,
        "dropped owner abort selection timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    wait_until_paused_bounded(
        controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "dropped owner abort application timed out",
    )
    .await;
    assert_new_admission_stopped_with_socket_deadline(addr);
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        controller,
        LifecycleCheckpoint::AfterSupervisorResultSend,
        "dropped owner result send timed out",
    )
    .await;
    assert!(
        dropped.load(Ordering::Acquire),
        "dropped owner reached finish before releasing transport state"
    );
    assert_connection_closed_with_socket_deadline(client);
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
}

async fn pause_after_owner_graceful<F>(controller: &LifecycleController, request: F, context: &str)
where
    F: FnOnce(),
{
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        context,
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    request();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        controller,
        LifecycleCheckpoint::SupervisorSelectedControl,
        "owner graceful selection timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    controller
        .release(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    wait_until_paused_bounded(
        controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "post-owner-graceful boundary timed out",
    )
    .await;
}

async fn pause_after_success_result_send<F>(controller: &LifecycleController, request: F)
where
    F: FnOnce(),
{
    controller
        .pause_once(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_until_paused_bounded(
        controller,
        LifecycleCheckpoint::BeforeSupervisorSelect,
        "post-result initial boundary timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    request();
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        controller,
        LifecycleCheckpoint::SupervisorSelectedControl,
        "post-result graceful selection timed out",
    )
    .await;
    wait_until_paused_bounded(
        controller,
        LifecycleCheckpoint::AfterSupervisorResultSend,
        "successful result send timed out",
    )
    .await;
}

async fn wait_for_dispatch_drop(dropped: &AtomicBool, context: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !dropped.load(Ordering::Acquire) {
            tokio::time::sleep(DISPATCH_POLL_INTERVAL).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{context}"));
}

// 2.T4
#[tokio::test(start_paused = true)]
async fn dropping_handle_or_pending_future_forces_continuing_supervisor() {
    let (controller, addr, handle, client, _release, dropped) = retained_owner_server().await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    drop(handle);
    assert_dropped_owner_continues_abort(&controller, addr, client, &dropped).await;

    let (controller, addr, handle, client, _release, dropped) = retained_owner_server().await;
    let mut future = Box::pin(handle.join());
    assert!(future.as_mut().now_or_never().is_none());
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    drop(future);
    assert_dropped_owner_continues_abort(&controller, addr, client, &dropped).await;

    let (controller, addr, handle, client, _release, dropped) = retained_owner_server().await;
    pause_after_owner_graceful(
        &controller,
        || handle.shutdown(),
        "handle graceful boundary timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    drop(handle);
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    assert_dropped_owner_continues_abort(&controller, addr, client, &dropped).await;

    let (controller, addr, handle, client, _release, dropped) = retained_owner_server().await;
    let mut future = Box::pin(handle.join());
    assert!(future.as_mut().now_or_never().is_none());
    pause_after_owner_graceful(
        &controller,
        || future.as_ref().get_ref().shutdown(),
        "future graceful boundary timed out",
    )
    .await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedControl)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    drop(future);
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    assert_dropped_owner_continues_abort(&controller, addr, client, &dropped).await;

    let (controller, _addr, handle, client, _release, dropped) = retained_owner_server().await;
    pause_after_owner_graceful(
        &controller,
        || handle.shutdown(),
        "timeout handle graceful boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(30)).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    drop(handle);
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedDeadline,
        "equal-ready handle deadline did not beat Drop abort",
    )
    .await;
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::AfterSupervisorResultSend,
        "fixed handle timeout result send timed out",
    )
    .await;
    assert_connection_closed_with_socket_deadline(client);
    assert!(dropped.load(Ordering::Acquire));
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();

    let (controller, _addr, handle, client, _release, dropped) = retained_owner_server().await;
    let mut future = Box::pin(handle.join());
    assert!(future.as_mut().now_or_never().is_none());
    pause_after_owner_graceful(
        &controller,
        || future.as_ref().get_ref().shutdown(),
        "timeout future graceful boundary timed out",
    )
    .await;
    tokio::time::advance(Duration::from_secs(30)).await;
    controller
        .pause_once(LifecycleCheckpoint::SupervisorSelectedDeadline)
        .unwrap();
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    drop(future);
    controller
        .release(LifecycleCheckpoint::BeforeSupervisorSelect)
        .unwrap();
    wait_and_release_bounded(
        &controller,
        LifecycleCheckpoint::SupervisorSelectedDeadline,
        "equal-ready future deadline did not beat Drop abort",
    )
    .await;
    wait_until_paused_bounded(
        &controller,
        LifecycleCheckpoint::AfterSupervisorResultSend,
        "fixed future timeout result send timed out",
    )
    .await;
    assert_connection_closed_with_socket_deadline(client);
    assert!(dropped.load(Ordering::Acquire));
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let controller = lifecycle(listener.local_addr().unwrap()).unwrap();
    let dispatch_dropped = Arc::new(AtomicBool::new(false));
    let handle = camber::http::serve_background(
        listener,
        dispatch_drop_router(Arc::clone(&dispatch_dropped)),
    );
    pause_after_success_result_send(&controller, || handle.shutdown()).await;
    drop(handle);
    assert!(
        !dispatch_dropped.load(Ordering::Acquire),
        "handle Drop destroyed the supervisor while result send was paused"
    );
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    wait_for_dispatch_drop(
        &dispatch_dropped,
        "handle post-result supervisor exit timed out",
    )
    .await;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let controller = lifecycle(listener.local_addr().unwrap()).unwrap();
    let dispatch_dropped = Arc::new(AtomicBool::new(false));
    let handle = camber::http::serve_background(
        listener,
        dispatch_drop_router(Arc::clone(&dispatch_dropped)),
    );
    let mut future = Box::pin(handle.join());
    assert!(future.as_mut().now_or_never().is_none());
    pause_after_success_result_send(&controller, || {
        future.as_ref().get_ref().shutdown();
    })
    .await;
    drop(future);
    assert!(
        !dispatch_dropped.load(Ordering::Acquire),
        "future Drop destroyed the supervisor while result send was paused"
    );
    controller
        .release(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    wait_for_dispatch_drop(
        &dispatch_dropped,
        "future post-result supervisor exit timed out",
    )
    .await;
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
    let controller = lifecycle(addr).unwrap();
    let handle = camber::http::serve_background(
        listener,
        named_held_router(
            entered_tx,
            Arc::clone(&release),
            Arc::clone(&dropped),
            Arc::clone(&next_requests),
        ),
    );
    let mut client = connect_request_path(addr, "/active").await;
    entered_rx.await.unwrap();
    let mut future = Box::pin(handle.join());
    controller
        .pause_once(LifecycleCheckpoint::AfterSupervisorResultSend)
        .unwrap();
    future.as_ref().get_ref().shutdown();
    assert!(future.as_mut().now_or_never().is_none());
    tokio::time::timeout(Duration::from_secs(5), async {
        assert_new_admission_stopped(addr).await;
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
        &controller,
        LifecycleCheckpoint::AfterSupervisorResultSend,
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
