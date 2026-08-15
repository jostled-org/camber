//! 1.T3 and 1.T4: what a terminal `serve*` call captures, and what one
//! connection permit covers.

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, lifecycle};
use camber::http::{Request, Response, Router, ServerPolicy, SseWriter};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a blocked peer is given to prove it stays blocked.
const BLOCKED_BOUND: Duration = Duration::from_millis(300);
/// The header timeout the `#[camber::test]` runtime establishes.
const RUNTIME_HEADER_TIMEOUT: Duration = Duration::from_millis(100);

/// A route that holds its request until the test releases it.
struct HeldRoute {
    router: Router,
    release: Arc<tokio::sync::Semaphore>,
}

fn held_router() -> HeldRoute {
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let held = Arc::clone(&release);
    let mut router = Router::new();
    router.get("/held", move |_req: &Request| {
        let held = Arc::clone(&held);
        async move {
            let permit = held.acquire().await;
            drop(permit);
            Response::text(200, "held")
        }
    });
    HeldRoute { router, release }
}

async fn connect(addr: std::net::SocketAddr) -> tokio::net::TcpStream {
    tokio::time::timeout(EVENT_TIMEOUT, tokio::net::TcpStream::connect(addr))
        .await
        .expect("peer connect timed out")
        .expect("peer connect failed")
}

async fn send_get(stream: &mut tokio::net::TcpStream, path: &str) {
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write request");
}

/// Read one response, or report that the peer was never answered.
async fn read_response(stream: &mut tokio::net::TcpStream, bound: Duration) -> Option<String> {
    let mut buffer = Vec::new();
    match tokio::time::timeout(bound, stream.read_to_end(&mut buffer)).await {
        Ok(Ok(_)) if buffer.is_empty() => None,
        Ok(Ok(_)) => Some(String::from_utf8_lossy(&buffer).into_owned()),
        Ok(Err(_)) => None,
        Err(_) => None,
    }
}

async fn assert_answered(stream: &mut tokio::net::TcpStream, label: &str) {
    let response = read_response(stream, EVENT_TIMEOUT)
        .await
        .unwrap_or_else(|| panic!("{label}: peer was never answered"));
    assert!(
        response.starts_with("HTTP/1.1 200"),
        "{label}: unexpected answer {response:?}"
    );
}

async fn assert_unanswered(stream: &mut tokio::net::TcpStream, label: &str) {
    let mut byte = [0u8; 1];
    match tokio::time::timeout(BLOCKED_BOUND, stream.read(&mut byte)).await {
        Err(_) => {}
        Ok(Ok(0)) => panic!("{label}: transport closed instead of waiting for a permit"),
        Ok(Ok(_)) => panic!("{label}: peer was answered while the limit was held"),
        Ok(Err(error)) => panic!("{label}: peer failed while waiting: {error}"),
    }
}

async fn wait_paused(controller: &LifecycleController, checkpoint: LifecycleCheckpoint, at: &str) {
    tokio::time::timeout(EVENT_TIMEOUT, controller.wait_until_paused(checkpoint))
        .await
        .unwrap_or_else(|_| panic!("{at}: {checkpoint:?} was never reached"))
        .unwrap_or_else(|error| panic!("{at}: waiting for {checkpoint:?} failed: {error}"));
}

/// 1.T3
#[camber::test]
async fn server_builder_freezes_context_and_policy_at_terminal_call() {
    // The rows that must observe an ABSENT runtime run on threads of their own:
    // a case body inside `#[camber::test]` is already on a Tokio worker, which
    // is the context they exist to prove Camber refuses to assume.
    unmanaged(
        "owned terminals",
        assert_owned_terminals_refuse_a_missing_executor,
    );
    unmanaged(
        "listener terminal",
        assert_listener_terminal_requires_a_camber_runtime,
    );
    assert_moved_future_keeps_its_captured_policy().await;
    unmanaged(
        "blocking terminal",
        assert_blocking_serve_establishes_a_runtime,
    );
}

/// Run one row on a thread that has entered no runtime at all.
fn unmanaged(row: &str, assertion: impl FnOnce() + Send + 'static) {
    std::thread::spawn(assertion)
        .join()
        .unwrap_or_else(|_| panic!("{row}: the unmanaged row panicked"));
}

/// Both owned terminals answer an absent Tokio executor synchronously, without
/// handing back an owner for a server that never started.
fn assert_owned_terminals_refuse_a_missing_executor() {
    let tokio_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build a throwaway executor for the listener");
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("non-blocking");
    let background_listener =
        tokio_runtime.block_on(async { tokio::net::TcpListener::from_std(std_listener).unwrap() });
    let std_listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
    std_listener.set_nonblocking(true).expect("non-blocking");
    let async_listener =
        tokio_runtime.block_on(async { tokio::net::TcpListener::from_std(std_listener).unwrap() });
    drop(tokio_runtime);

    match camber::http::server(Router::new()).serve_background(background_listener) {
        Err(RuntimeError::NoRuntime) => {}
        Ok(_) => panic!("serve_background admitted a missing Tokio executor"),
        Err(other) => panic!("expected NoRuntime, got {other:?}"),
    }
    match camber::http::server(Router::new()).serve_async(async_listener) {
        Err(RuntimeError::NoRuntime) => {}
        Ok(_) => panic!("serve_async admitted a missing Tokio executor"),
        Err(other) => panic!("expected NoRuntime, got {other:?}"),
    }
}

/// The listener terminal serves on the current Camber runtime rather than
/// creating one, so its absence is refused before the listener is polled.
fn assert_listener_terminal_requires_a_camber_runtime() {
    // Bound through a bare Tokio runtime, because binding needs a reactor and
    // the claim is about the Camber runtime the listener terminal requires.
    let executor = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build a bare executor for the listener");
    let listener = executor
        .block_on(async { camber::net::listen("127.0.0.1:0") })
        .expect("bind an unmanaged listener");
    let refusal = camber::http::server(Router::new()).serve_listener(listener);
    match refusal {
        Err(RuntimeError::NoRuntime) => {}
        Ok(()) => panic!("serve_listener served with no Camber runtime established"),
        Err(other) => panic!("expected NoRuntime, got {other:?}"),
    }
}

/// A `serve_async` owner built inside a Camber runtime keeps that runtime's
/// policy when it is polled on a foreign executor.
///
/// The server's own policy leaves every dimension at its default, so a capture
/// that happened at first poll — on an executor with no Camber runtime — would
/// configure Hyper with the 60-second default instead of the runtime's
/// 100 milliseconds, and the armed observation would never be reached.
async fn assert_moved_future_keeps_its_captured_policy() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the moved-future listener");
    let addr = listener.local_addr().expect("moved-future address");
    let controller = lifecycle(addr).expect("register the moved-future observer");
    controller
        .pause_once(LifecycleCheckpoint::HeaderTimeoutConfigured(
            RUNTIME_HEADER_TIMEOUT,
        ))
        .expect("arm the captured header timeout");

    let mut router = Router::new();
    router.get("/moved", |_req: &Request| async {
        Response::text(200, "moved")
    });
    let served = camber::http::server(router)
        .policy(ServerPolicy::default())
        .serve_async(listener)
        .expect("the owned future is built inside the Camber runtime");

    let (stop, stopped) = tokio::sync::oneshot::channel::<()>();
    let foreign = std::thread::spawn(move || {
        let executor = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build the foreign executor");
        executor.block_on(async move {
            tokio::select! {
                result = served => {
                    let _ = result;
                }
                _ = stopped => {}
            }
        });
    });

    let mut peer = connect(addr).await;
    send_get(&mut peer, "/moved").await;
    wait_paused(
        &controller,
        LifecycleCheckpoint::HeaderTimeoutConfigured(RUNTIME_HEADER_TIMEOUT),
        "moved future",
    )
    .await;
    controller
        .release(LifecycleCheckpoint::HeaderTimeoutConfigured(
            RUNTIME_HEADER_TIMEOUT,
        ))
        .expect("release the captured header timeout");
    assert_answered(&mut peer, "moved future").await;

    stop.send(()).expect("stop the foreign executor");
    foreign.join().expect("the foreign executor thread joined");
}

/// The blocking terminal establishes a runtime when none exists.
///
/// The served route requests shutdown, which is a no-op with no runtime
/// established. So the server stopping is itself the proof: a `serve` that had
/// not created a runtime would refuse before ever listening — the refusal
/// `assert_listener_terminal_requires_a_camber_runtime` reads — and one that
/// created none for its handlers would never see the request.
fn assert_blocking_serve_establishes_a_runtime() {
    let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("reserve a port");
    let addr = probe.local_addr().expect("reserved address");
    drop(probe);

    let mut router = Router::new();
    router.get("/stop", |_req: &Request| async {
        camber::runtime::request_shutdown();
        Response::text(200, "stopping")
    });
    let (report, reported) = std::sync::mpsc::channel();
    let serving = std::thread::spawn(move || {
        let result = camber::http::server(router)
            .policy(
                ServerPolicy::default()
                    .shutdown_timeout(Duration::from_secs(1))
                    .expect("a short shutdown deadline"),
            )
            .serve(&addr.to_string());
        let _ = report.send(result);
    });

    crate::common::wait_for_http_response(addr, EVENT_TIMEOUT)
        .expect("the blocking server answered its readiness probe");

    let stopped = crate::common::request_to_host(addr, "GET", "/stop", "localhost", &[])
        .expect("the blocking server answered the stop request");
    assert_eq!(stopped.status, 200, "the stop route was not served");

    let outcome = reported
        .recv_timeout(EVENT_TIMEOUT)
        .expect("the blocking terminal never returned, so it established no runtime to stop");
    outcome.expect("the blocking terminal ended with an error");
    serving.join().expect("the serving thread joined");
}

/// 1.T4
#[camber::test]
async fn connection_limit_matrix_holds_one_permit_for_the_transport_lifetime() {
    assert_zero_is_refused_before_any_accept();
    assert_omitted_limit_admits_concurrent_transports().await;
    assert_one_permit_covers_each_http_transport().await;
    assert_one_permit_covers_an_sse_response().await;
}

/// Zero never reaches a listener: the policy constructor refuses it, so no
/// server can be built with one.
fn assert_zero_is_refused_before_any_accept() {
    match ServerPolicy::default().connection_limit(0) {
        Err(RuntimeError::InvalidArgument(message)) => assert!(
            message.contains("connection_limit"),
            "expected the connection limit named, got {message}"
        ),
        other => panic!("expected a refused zero limit, got {other:?}"),
    }
}

/// An omitted limit is unbounded: two transports are served at once.
async fn assert_omitted_limit_admits_concurrent_transports() {
    let held = held_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the unbounded listener");
    let addr = listener.local_addr().expect("unbounded address");
    let handle = camber::http::server(held.router)
        .serve_background(listener)
        .expect("owned server requires a Tokio runtime");

    let mut first = connect(addr).await;
    send_get(&mut first, "/held").await;
    let mut second = connect(addr).await;
    send_get(&mut second, "/held").await;

    held.release.add_permits(2);
    assert_answered(&mut first, "unbounded first").await;
    assert_answered(&mut second, "unbounded second").await;

    handle.shutdown();
    tokio::time::timeout(EVENT_TIMEOUT, handle.join())
        .await
        .expect("the unbounded server joined")
        .expect("the unbounded server ended cleanly");
}

/// A positive limit holds one permit for a whole HTTP transport, and returns
/// exactly one when that transport ends.
async fn assert_one_permit_covers_each_http_transport() {
    let held = held_router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the limited listener");
    let addr = listener.local_addr().expect("limited address");
    let controller = lifecycle(addr).expect("register the limited observer");
    let handle = camber::http::server(held.router)
        .policy(
            ServerPolicy::default()
                .connection_limit(1)
                .expect("one concurrent transport"),
        )
        .serve_background(listener)
        .expect("owned server requires a Tokio runtime");

    controller
        .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .expect("arm the permit wait");

    let mut first = connect(addr).await;
    send_get(&mut first, "/held").await;
    let mut second = connect(addr).await;
    send_get(&mut second, "/held").await;

    // The second transport is accepted and then parked: the permit its
    // predecessor holds is what it waits on.
    wait_paused(
        &controller,
        LifecycleCheckpoint::ConnectionPermitWaitPending,
        "limited second",
    )
    .await;
    controller
        .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .expect("release the permit wait");
    assert_unanswered(&mut second, "limited second").await;

    // Releasing the first request ends its transport, which returns its permit.
    held.release.add_permits(1);
    assert_answered(&mut first, "limited first").await;

    // Exactly one permit came back: the second is served, and a third that
    // arrives while the second holds the limit is still refused entry.
    held.release.add_permits(1);
    assert_answered(&mut second, "limited second").await;

    let mut third = connect(addr).await;
    send_get(&mut third, "/held").await;
    held.release.add_permits(1);
    assert_answered(&mut third, "limited third").await;

    handle.shutdown();
    tokio::time::timeout(EVENT_TIMEOUT, handle.join())
        .await
        .expect("the limited server joined")
        .expect("the limited server ended cleanly");
}

/// An SSE response holds its permit for the response's lifetime, not just the
/// request head that started it.
/// A route whose SSE response stays open until its peer goes away.
///
/// Held open on purpose: the permit's lifetime is the transport's, not the
/// response head's, and a producer that ended on its own would not show that.
fn endless_sse_router() -> Router {
    let mut router = Router::new();
    router.get_sse("/events", move |_req: &Request, writer: &mut SseWriter| {
        while writer.event("tick", "tick").is_ok() {
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    });
    router
}

/// Open one SSE subscription and read its head, so the permit is held.
async fn open_subscription(addr: std::net::SocketAddr) -> tokio::net::TcpStream {
    let mut subscriber = connect(addr).await;
    subscriber
        .write_all(b"GET /events HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await
        .expect("write the SSE request");
    let mut head = [0u8; 64];
    tokio::time::timeout(EVENT_TIMEOUT, subscriber.read(&mut head))
        .await
        .expect("the SSE head arrived")
        .expect("the SSE head was readable");
    subscriber
}

async fn assert_one_permit_covers_an_sse_response() {
    let router = endless_sse_router();

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the SSE listener");
    let addr = listener.local_addr().expect("SSE address");
    let controller = lifecycle(addr).expect("register the SSE observer");
    let handle = camber::http::server(router)
        .policy(
            ServerPolicy::default()
                .connection_limit(1)
                .expect("one concurrent transport"),
        )
        .serve_background(listener)
        .expect("owned server requires a Tokio runtime");

    controller
        .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .expect("arm the SSE permit wait");

    let subscriber = open_subscription(addr).await;

    let mut blocked = connect(addr).await;
    send_get(&mut blocked, "/events").await;
    wait_paused(
        &controller,
        LifecycleCheckpoint::ConnectionPermitWaitPending,
        "SSE blocked",
    )
    .await;
    controller
        .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .expect("release the SSE permit wait");
    assert_unanswered(&mut blocked, "SSE blocked").await;

    // Closing the subscriber ends the streaming response and returns its one
    // permit; the waiting peer then enters.
    drop(subscriber);
    // The waiting peer is itself asking for an endless stream, so entering is
    // the first byte it reads rather than a complete response.
    let mut entered = [0u8; 32];
    let read = tokio::time::timeout(EVENT_TIMEOUT, blocked.read(&mut entered))
        .await
        .expect("the waiting peer never entered after the SSE response released its permit")
        .expect("the waiting peer's transport failed");
    assert!(
        read > 0,
        "the waiting peer was closed instead of being admitted"
    );

    handle.cancel();
    let ended = tokio::time::timeout(EVENT_TIMEOUT, handle.join())
        .await
        .expect("the SSE server joined");
    assert!(
        matches!(ended, Err(RuntimeError::Cancelled) | Ok(())),
        "unexpected SSE server outcome: {ended:?}"
    );
}
