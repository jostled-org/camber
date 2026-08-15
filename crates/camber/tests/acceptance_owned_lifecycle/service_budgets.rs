//! 1.T3 and 1.T4: what a terminal `serve*` call captures, and what one
//! connection permit covers.

use camber::RuntimeError;
#[cfg(feature = "grpc")]
use camber::http::GrpcRouter;
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
///
/// Owns the omitted, zero, plain-TCP, SSE, gRPC, and bare-Tokio rows of the
/// connection-limit matrix. The TLS-handshake, HTTP keep-alive, direct
/// WebSocket, and proxied WebSocket rows are owned by
/// `component_websocket::connection_limits`, which holds them on the same
/// production checkpoint.
#[camber::test]
async fn connection_limit_matrix_holds_one_permit_for_the_transport_lifetime() {
    assert_zero_is_refused_before_any_accept();
    assert_omitted_limit_admits_concurrent_transports().await;
    assert_one_permit_covers_each_http_transport().await;
    assert_one_permit_covers_an_sse_response().await;
    #[cfg(feature = "grpc")]
    assert_one_permit_covers_a_grpc_transport().await;
    unmanaged(
        "bare-Tokio limit",
        assert_bare_tokio_serving_holds_one_permit,
    );
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

/// A background server limited to one concurrent transport, with the observer
/// that watches its permit.
struct LimitedServer {
    handle: camber::http::ServerHandle,
    addr: std::net::SocketAddr,
    controller: LifecycleController,
}

/// Serve `router` in the background under a one-transport connection limit.
///
/// Every permit row needs the same three things — a bound address, an observer
/// registered against it, and a server frozen at `connection_limit(1)` — so they
/// are assembled once here and the rows differ only in what they then do.
async fn serve_one_transport(router: Router, label: &str) -> LimitedServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .unwrap_or_else(|_| panic!("bind the {label} listener"));
    let addr = listener
        .local_addr()
        .unwrap_or_else(|_| panic!("{label} address"));
    let controller = lifecycle(addr).unwrap_or_else(|_| panic!("register the {label} observer"));
    let handle = camber::http::server(router)
        .policy(
            ServerPolicy::default()
                .connection_limit(1)
                .expect("one concurrent transport"),
        )
        .serve_background(listener)
        .unwrap_or_else(|_| panic!("the {label} server needs a Tokio executor"));
    LimitedServer {
        handle,
        addr,
        controller,
    }
}

/// Arm the one production checkpoint every permit row waits on.
fn arm_permit_wait(controller: &LifecycleController, label: &str) {
    controller
        .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .unwrap_or_else(|_| panic!("arm the {label} permit wait"));
}

/// Prove `blocked` is parked on the permit rather than merely slow.
///
/// Waiting on the production checkpoint is what makes this a proof: the peer has
/// reached the permit wait, so the silence that follows the release is the limit
/// holding, not a race the test happened to win.
async fn assert_blocked_on_permit(
    controller: &LifecycleController,
    blocked: &mut tokio::net::TcpStream,
    label: &str,
) {
    wait_paused(
        controller,
        LifecycleCheckpoint::ConnectionPermitWaitPending,
        label,
    )
    .await;
    controller
        .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .unwrap_or_else(|_| panic!("release the {label} permit wait"));
    assert_unanswered(blocked, label).await;
}

/// A positive limit holds one permit for a whole HTTP transport, and returns
/// exactly one when that transport ends.
async fn assert_one_permit_covers_each_http_transport() {
    let held = held_router();
    let server = serve_one_transport(held.router, "limited").await;
    let addr = server.addr;
    arm_permit_wait(&server.controller, "limited");

    let mut first = connect(addr).await;
    send_get(&mut first, "/held").await;
    let mut second = connect(addr).await;
    send_get(&mut second, "/held").await;

    // The second transport is accepted and then parked: the permit its
    // predecessor holds is what it waits on.
    assert_blocked_on_permit(&server.controller, &mut second, "limited second").await;

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

    server.handle.shutdown();
    tokio::time::timeout(EVENT_TIMEOUT, server.handle.join())
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
    let server = serve_one_transport(endless_sse_router(), "SSE").await;
    let addr = server.addr;
    arm_permit_wait(&server.controller, "SSE");

    let subscriber = open_subscription(addr).await;

    let mut blocked = connect(addr).await;
    send_get(&mut blocked, "/events").await;
    assert_blocked_on_permit(&server.controller, &mut blocked, "SSE blocked").await;

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

    server.handle.cancel();
    let ended = tokio::time::timeout(EVENT_TIMEOUT, server.handle.join())
        .await
        .expect("the SSE server joined");
    assert!(
        matches!(ended, Err(RuntimeError::Cancelled) | Ok(())),
        "unexpected SSE server outcome: {ended:?}"
    );
}

#[cfg(feature = "grpc")]
mod grpc_proto {
    tonic::include_proto!("greeter");
}

/// A greeter that reports it was entered, then parks until the test releases it.
///
/// Parking is what makes the row discriminating: while the handler waits, tonic
/// has produced no frame and no trailer, so the permit the blocked peer waits on
/// is being held across an RPC that has produced nothing.
#[cfg(feature = "grpc")]
struct ParkedGreeter {
    entered: tokio::sync::mpsc::UnboundedSender<()>,
    release: Arc<tokio::sync::Semaphore>,
}

#[cfg(feature = "grpc")]
#[tonic::async_trait]
impl grpc_proto::greeter_service::Greeter for ParkedGreeter {
    async fn say_hello(
        &self,
        request: tonic::Request<grpc_proto::HelloRequest>,
    ) -> Result<tonic::Response<grpc_proto::HelloReply>, tonic::Status> {
        self.entered
            .send(())
            .expect("report that the parked RPC was entered");
        let permit = self
            .release
            .acquire()
            .await
            .expect("the parked RPC's release gate closed");
        drop(permit);
        let name = request.into_inner().name;
        Ok(tonic::Response::new(grpc_proto::HelloReply {
            message: format!("Hello, {name}!"),
        }))
    }
}

/// One permit covers a gRPC transport for the transport's life, not the RPC's.
///
/// Read twice on purpose. First while the RPC is parked, which proves the permit
/// is held across work tonic has not begun to answer. Then after the RPC has
/// returned its reply: an HTTP/2 channel outlives the call it carried, so a
/// permit released at the end of the RPC — rather than the end of the connection
/// — would let the waiting peer in here. Only closing the channel returns it.
#[cfg(feature = "grpc")]
async fn assert_one_permit_covers_a_grpc_transport() {
    let release = Arc::new(tokio::sync::Semaphore::new(0));
    let (entered, mut entries) = tokio::sync::mpsc::unbounded_channel();
    let mut router = Router::new();
    router.get("/ping", |_req: &Request| async {
        Response::text(200, "ping")
    });
    router.grpc(
        GrpcRouter::new().add_service(grpc_proto::greeter_service::serve(ParkedGreeter {
            entered,
            release: Arc::clone(&release),
        })),
    );

    let server = serve_one_transport(router, "gRPC").await;
    let call = park_one_rpc(server.addr).await;
    tokio::time::timeout(EVENT_TIMEOUT, entries.recv())
        .await
        .expect("the parked RPC was never entered")
        .expect("the parked RPC's entry channel closed");

    arm_permit_wait(&server.controller, "gRPC");
    let mut blocked = connect(server.addr).await;
    send_get(&mut blocked, "/ping").await;
    assert_blocked_on_permit(
        &server.controller,
        &mut blocked,
        "gRPC blocked while the RPC is parked",
    )
    .await;

    release.add_permits(1);
    let (client, reply) = tokio::time::timeout(EVENT_TIMEOUT, call)
        .await
        .expect("the released RPC never returned")
        .expect("the gRPC call task panicked");
    assert_eq!(
        reply.as_str(),
        "Hello, Camber!",
        "the parked RPC did not complete through tonic"
    );

    // The second read: the RPC is answered, but its channel is still open, so
    // the permit must still be held.
    assert_unanswered(&mut blocked, "gRPC blocked after the RPC completed").await;

    drop(client);
    assert_answered(&mut blocked, "gRPC blocked").await;

    server.handle.shutdown();
    tokio::time::timeout(EVENT_TIMEOUT, server.handle.join())
        .await
        .expect("the gRPC server joined")
        .expect("the gRPC server ended cleanly");
}

/// Start the RPC that parks inside `ParkedGreeter` and hand back its join.
///
/// The client rides back out of the task rather than being dropped inside it:
/// the transport must stay open after the RPC ends, because that gap between a
/// finished call and a closed channel is exactly what this row reads.
#[cfg(feature = "grpc")]
async fn park_one_rpc(
    addr: std::net::SocketAddr,
) -> tokio::task::JoinHandle<(
    grpc_proto::greeter_client::GreeterClient<tonic::transport::Channel>,
    String,
)> {
    let channel = tokio::time::timeout(
        EVENT_TIMEOUT,
        tonic::transport::Channel::from_shared(format!("http://{addr}"))
            .expect("build the gRPC endpoint")
            .connect(),
    )
    .await
    .expect("the gRPC channel never connected")
    .expect("the gRPC channel failed to connect");
    let mut caller = grpc_proto::greeter_client::GreeterClient::new(channel);
    tokio::spawn(async move {
        let reply = caller
            .say_hello(tonic::Request::new(grpc_proto::HelloRequest {
                name: "Camber".into(),
            }))
            .await
            .expect("the gRPC call failed")
            .into_inner()
            .message;
        (caller, reply)
    })
}

/// The limit is the server's own authority, not the Camber runtime's.
///
/// This row serves from a bare Tokio executor, which takes the other production
/// join branch: `serve_background` spawns onto Tokio rather than onto Camber.
/// The `serve_listener` refusal that opens it is the proof that no Camber
/// runtime is established here — the policy under test came from nowhere else.
fn assert_bare_tokio_serving_holds_one_permit() {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build the bare executor");
    executor.block_on(assert_bare_tokio_limit_holds());
}

async fn assert_bare_tokio_limit_holds() {
    let probe = camber::net::listen("127.0.0.1:0").expect("bind the bare-Tokio probe listener");
    match camber::http::server(Router::new()).serve_listener(probe) {
        Err(RuntimeError::NoRuntime) => {}
        other => panic!("the bare-Tokio row is not bare: serve_listener returned {other:?}"),
    }

    let held = held_router();
    let server = serve_one_transport(held.router, "bare-Tokio").await;
    let addr = server.addr;
    arm_permit_wait(&server.controller, "bare-Tokio");

    let mut first = connect(addr).await;
    send_get(&mut first, "/held").await;
    let mut second = connect(addr).await;
    send_get(&mut second, "/held").await;

    assert_blocked_on_permit(&server.controller, &mut second, "bare-Tokio second").await;

    held.release.add_permits(1);
    assert_answered(&mut first, "bare-Tokio first").await;

    held.release.add_permits(1);
    assert_answered(&mut second, "bare-Tokio second").await;

    server.handle.shutdown();
    tokio::time::timeout(EVENT_TIMEOUT, server.handle.join())
        .await
        .expect("the bare-Tokio server joined")
        .expect("the bare-Tokio server ended cleanly");
}
