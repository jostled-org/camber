use crate::common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, lifecycle};
#[cfg(feature = "ws")]
use camber::http::{HostRouter, WsConn};
use camber::http::{Request, Response, Router};
#[cfg(feature = "ws")]
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
#[cfg(feature = "ws")]
use std::sync::Arc;
#[cfg(feature = "ws")]
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

const PRESTART_SHUTDOWN_MODE: &str = "synchronous-prestart-shutdown";

const PRESTART_SHUTDOWN_MARKER: &str = "synchronous-prestart-shutdown-complete";

const PENDING_PERMIT_SHUTDOWN_MODE: &str = "synchronous-pending-permit-shutdown";

const PENDING_PERMIT_SHUTDOWN_MARKER: &str = "synchronous-pending-permit-shutdown-complete";

const SYNCHRONOUS_ISOLATION_MODE: &str = "synchronous-lifecycle-isolation";

const SYNCHRONOUS_ISOLATION_MARKER: &str = "synchronous-lifecycle-isolation-complete";

#[test]
fn connection_limit_zero_rejected() {
    let err = camber::runtime::builder()
        .connection_limit(0)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| Ok::<(), camber::RuntimeError>(()))
        .unwrap_err();

    match err {
        camber::RuntimeError::InvalidArgument(msg) => {
            assert_eq!(msg.as_ref(), "connection_limit must be at least 1");
        }
        other => panic!("expected InvalidArgument, got {other:?}"),
    }
}

fn send_request(stream: &mut impl Write, path: &str) {
    send_request_with_host(stream, path, "localhost");
}

fn send_request_with_host(stream: &mut impl Write, path: &str, host: &str) {
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    stream.write_all(req.as_bytes()).expect("write request");
}

fn read_status(stream: &mut impl Read) -> u16 {
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).expect("read response");
    let text = String::from_utf8_lossy(&buf[..n]);
    text.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0)
}

#[cfg(feature = "ws")]
fn dispatch_channel() -> (Sender<()>, Receiver<()>) {
    std::sync::mpsc::channel()
}

#[cfg(feature = "ws")]
fn direct_ws_router(dispatched: Sender<()>) -> Router {
    let mut router = Router::new();
    router.ws("/ws", |_req: &Request, mut conn: WsConn| {
        while let Some(message) = conn.recv() {
            conn.send(&message)?;
        }
        Ok(())
    });
    router.get("/second", move |_req: &Request| {
        let dispatched = dispatched.clone();
        async move {
            dispatched.send(()).expect("record second dispatch");
            Response::text(200, "second")
        }
    });
    router
}

#[cfg(feature = "ws")]
fn proxy_ws_router(backend_addr: SocketAddr, dispatched: Sender<()>) -> Router {
    let mut router = Router::new();
    router.proxy("/ws", &format!("http://{backend_addr}"));
    router.get("/second", move |_req: &Request| {
        let dispatched = dispatched.clone();
        async move {
            dispatched.send(()).expect("record second dispatch");
            Response::text(200, "second")
        }
    });
    router
}

#[cfg(feature = "ws")]
fn spawn_ws_backend() -> SocketAddr {
    let mut backend = Router::new();
    backend.ws("/echo", |_req: &Request, mut conn: WsConn| {
        while let Some(message) = conn.recv() {
            conn.send(&message)?;
        }
        Ok(())
    });
    common::spawn_server(backend)
}

#[cfg(feature = "ws")]
fn read_http_headers(stream: &mut impl Read) -> String {
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let count = stream.read(&mut byte).expect("read HTTP headers");
        match count {
            0 => break,
            _ => response.push(byte[0]),
        }
        if response.ends_with(b"\r\n\r\n") {
            break;
        }
    }
    String::from_utf8(response).expect("HTTP headers are UTF-8")
}

#[cfg(feature = "ws")]
fn upgrade_websocket(mut stream: impl Read + Write, path: &str, host: &str) -> impl Read + Write {
    // The workspace's own accepted head, addressed to this case's authority: a
    // copy here could drift from what Camber accepts, and then every limit
    // proved through it would be proved against a request no client sends.
    let request = common::ws_upgrade_request_to(host, path);
    stream
        .write_all(request.as_bytes())
        .expect("write WebSocket upgrade");
    let response = read_http_headers(&mut stream);
    assert!(
        response.starts_with("HTTP/1.1 101"),
        "expected WebSocket upgrade, got: {response}"
    );
    stream
}

#[cfg(feature = "ws")]
fn write_ws_frame(stream: &mut impl Write, opcode: u8, payload: &[u8]) {
    assert!(payload.len() <= 125, "test frame payload must be short");
    let mask = [0x12, 0x34, 0x56, 0x78];
    let mut frame = Vec::with_capacity(payload.len() + 6);
    frame.extend_from_slice(&[0x80 | opcode, 0x80 | payload.len() as u8]);
    frame.extend_from_slice(&mask);
    frame.extend(
        payload
            .iter()
            .enumerate()
            .map(|(index, byte)| byte ^ mask[index % mask.len()]),
    );
    stream.write_all(&frame).expect("write WebSocket frame");
}

#[cfg(feature = "ws")]
fn read_ws_frame(stream: &mut impl Read) -> (u8, Vec<u8>) {
    let mut header = [0u8; 2];
    stream
        .read_exact(&mut header)
        .expect("read WebSocket frame header");
    assert_eq!(header[1] & 0x80, 0, "server frames must not be masked");
    let length = match header[1] & 0x7f {
        126 => {
            let mut extended = [0u8; 2];
            stream
                .read_exact(&mut extended)
                .expect("read WebSocket frame length");
            u16::from_be_bytes(extended) as usize
        }
        127 => {
            let mut extended = [0u8; 8];
            stream
                .read_exact(&mut extended)
                .expect("read WebSocket frame length");
            usize::try_from(u64::from_be_bytes(extended)).expect("frame length fits usize")
        }
        length => length as usize,
    };
    let mut payload = vec![0u8; length];
    stream
        .read_exact(&mut payload)
        .expect("read WebSocket frame payload");
    (header[0] & 0x0f, payload)
}

#[cfg(feature = "ws")]
fn assert_ws_echo(stream: &mut (impl Read + Write)) {
    write_ws_frame(stream, 0x1, b"permit-held");
    let (opcode, payload) = read_ws_frame(stream);
    assert_eq!(opcode, 0x1, "expected WebSocket text frame");
    assert_eq!(payload, b"permit-held");
}

#[cfg(feature = "ws")]
fn complete_client_initiated_close(stream: &mut (impl Read + Write)) {
    write_ws_frame(stream, 0x8, &[]);
    let (opcode, _) = read_ws_frame(stream);
    assert_eq!(opcode, 0x8, "expected WebSocket close response");
}

#[cfg(feature = "ws")]
fn receive_server_initiated_close(stream: &mut (impl Read + Write)) {
    let (opcode, _) = read_ws_frame(stream);
    assert_eq!(opcode, 0x8, "expected graceful WebSocket close frame");
}

#[cfg(feature = "ws")]
fn acknowledge_server_close(stream: &mut impl Write) {
    write_ws_frame(stream, 0x8, &[]);
}

fn plain_stream(addr: SocketAddr) -> TcpStream {
    let stream = TcpStream::connect(addr).expect("connect TCP client");
    stream
        .set_read_timeout(Some(EVENT_TIMEOUT))
        .expect("set TCP read timeout");
    stream
        .set_write_timeout(Some(EVENT_TIMEOUT))
        .expect("set TCP write timeout");
    stream
}

#[cfg(feature = "ws")]
fn tls_stream(
    addr: SocketAddr,
    client_config: Arc<rustls::ClientConfig>,
) -> rustls::StreamOwned<rustls::ClientConnection, TcpStream> {
    let tcp = plain_stream(addr);
    let server_name = rustls::pki_types::ServerName::try_from("localhost")
        .expect("localhost is a valid server name");
    let connection = rustls::ClientConnection::new(client_config, server_name)
        .expect("create TLS client connection");
    rustls::StreamOwned::new(connection, tcp)
}

#[cfg(feature = "ws")]
fn observe_owner<F>(future: F) -> Receiver<Result<(), RuntimeError>>
where
    F: Future<Output = Result<(), RuntimeError>> + Send + 'static,
{
    let (completion_tx, completion_rx) = std::sync::mpsc::sync_channel(1);
    camber::spawn_async(async move {
        let result = future.await;
        completion_tx
            .send(result)
            .expect("report server owner completion");
    });
    completion_rx
}

fn arm_pending_permit_checkpoint(controller: &LifecycleController) {
    controller
        .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .expect("arm pending connection-permit checkpoint");
}

fn wait_for_pending_permit(controller: &LifecycleController) {
    common::block_on(
        controller.wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending),
    )
    .expect("production connection-permit acquisition returned Pending");
}

fn release_pending_permit(controller: &LifecycleController) {
    controller
        .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .expect("release pending connection-permit checkpoint");
}

#[cfg(feature = "ws")]
fn assert_not_dispatched(dispatched: &Receiver<()>) {
    assert!(
        matches!(dispatched.try_recv(), Err(TryRecvError::Empty)),
        "second client dispatched after permit acquisition returned Pending"
    );
}

#[cfg(feature = "ws")]
fn assert_owner_completed(completion: &Receiver<Result<(), RuntimeError>>) {
    let result = completion
        .recv_timeout(EVENT_TIMEOUT)
        .expect("owned server completes after WebSocket close");
    assert!(result.is_ok(), "owned server returned {result:?}");
}

#[cfg(feature = "ws")]
fn assert_dispatched(dispatched: &Receiver<()>) {
    dispatched
        .recv_timeout(EVENT_TIMEOUT)
        .expect("second client dispatches after WebSocket close");
}

/// The per-connection checkpoint every supervised server configures Hyper at.
fn synchronous_connection_checkpoint() -> LifecycleCheckpoint {
    LifecycleCheckpoint::HeaderTimeoutConfigured(Duration::from_secs(5))
}

/// Prove synchronous serving reaches the supervisor checkpoint every owned
/// server reaches.
///
/// Since the synchronous entry points were moved onto `ServerSupervisor`, this
/// is a reachability claim rather than the isolation one it replaced: one
/// supervisor owns both families, so a checkpoint the owned path pauses at is
/// one this path pauses at too. The client runs on its own thread, because the
/// pause holds its connection until this thread releases it.
fn assert_supervisor_checkpoint_reached(
    controller: &LifecycleController,
    checkpoint: LifecycleCheckpoint,
) {
    common::block_on(controller.wait_until_paused(checkpoint))
        .expect("synchronous serving never reached the supervisor checkpoint");
    controller
        .release(checkpoint)
        .expect("release the supervisor checkpoint");
}

#[test]
fn connection_limit_blocks_third_connection_until_slot_frees() {
    camber::runtime::builder()
        .connection_limit(2)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/hello", |_req| async {
                camber::http::Response::text(200, "ok")
            });

            let listener = camber::net::listen("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr").tcp().unwrap();

            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            // Open two keep-alive connections and hold them open.
            let mut conn1 = TcpStream::connect(addr).expect("connect 1");
            conn1.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let req_keepalive = "GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n";
            conn1.write_all(req_keepalive.as_bytes()).unwrap();
            let s1 = read_status(&mut conn1);
            assert_eq!(s1, 200);

            let mut conn2 = TcpStream::connect(addr).expect("connect 2");
            conn2.set_read_timeout(Some(Duration::from_secs(5))).ok();
            conn2.write_all(req_keepalive.as_bytes()).unwrap();
            let s2 = read_status(&mut conn2);
            assert_eq!(s2, 200);

            // Third connection — should block because both slots are occupied.
            let mut conn3 = TcpStream::connect(addr).expect("connect 3");
            conn3
                .set_read_timeout(Some(Duration::from_millis(300)))
                .ok();
            send_request(&mut conn3, "/hello");
            let result = {
                let mut buf = [0u8; 1];
                conn3.read(&mut buf)
            };
            // Should time out because no permit is available.
            assert!(
                result.is_err(),
                "third connection should block while two slots are occupied"
            );

            // Free a slot by closing the first connection.
            drop(conn1);

            // Now the third connection should complete.
            conn3.set_read_timeout(Some(Duration::from_secs(5))).ok();
            let s3 = read_status(&mut conn3);
            assert_eq!(s3, 200);

            camber::runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn connection_limit_releases_slot_after_connection_exit() {
    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = camber::http::Router::new();
            router.get("/hello", |_req| async {
                camber::http::Response::text(200, "ok")
            });

            let listener = camber::net::listen("127.0.0.1:0").expect("bind");
            let addr = listener.local_addr().expect("addr").tcp().unwrap();

            camber::spawn(move || -> Result<(), camber::RuntimeError> {
                camber::http::serve_listener(listener, router)
            });

            // Open one connection, complete a request, then close it.
            {
                let mut conn1 = TcpStream::connect(addr).expect("connect 1");
                conn1.set_read_timeout(Some(Duration::from_secs(5))).ok();
                send_request(&mut conn1, "/hello");
                let s1 = read_status(&mut conn1);
                assert_eq!(s1, 200);
                // conn1 drops here — slot freed
            }

            // Second connection should succeed.
            let mut conn2 = TcpStream::connect(addr).expect("connect 2");
            conn2.set_read_timeout(Some(Duration::from_secs(5))).ok();
            send_request(&mut conn2, "/hello");
            let s2 = read_status(&mut conn2);
            assert_eq!(s2, 200);

            camber::runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn synchronous_serve_observes_shutdown_requested_before_start() {
    const TEST_NAME: &str =
        "connection_limits::synchronous_serve_observes_shutdown_requested_before_start";

    match common::is_private_child(PRESTART_SHUTDOWN_MODE) {
        true => {
            camber::runtime::builder()
                .shutdown_timeout(Duration::from_secs(1))
                .run(|| {
                    let listener = camber::net::listen("127.0.0.1:0")
                        .expect("bind pre-start shutdown listener");
                    let addr = listener
                        .local_addr()
                        .expect("pre-start shutdown listener address")
                        .tcp()
                        .expect("TCP listener address");
                    let (dispatched, observed) = std::sync::mpsc::sync_channel(1);
                    let mut router = Router::new();
                    router.get("/sync", move |_req: &Request| {
                        let dispatched = dispatched.clone();
                        async move {
                            dispatched.send(()).expect("record pre-start dispatch");
                            Response::text(200, "sync")
                        }
                    });

                    let mut queued = plain_stream(addr);
                    send_request(&mut queued, "/sync");
                    camber::runtime::request_shutdown();
                    camber::http::serve_listener(listener, router)
                        .expect("synchronous serve observes sticky shutdown");
                    assert!(
                        matches!(
                            observed.try_recv(),
                            Err(std::sync::mpsc::TryRecvError::Empty
                                | std::sync::mpsc::TryRecvError::Disconnected)
                        ),
                        "queued connection dispatched after shutdown"
                    );
                })
                .expect("run pre-start shutdown runtime");
            println!("{PRESTART_SHUTDOWN_MARKER}");
            return;
        }
        false => {}
    }

    let run = common::run_isolated_exact(
        TEST_NAME,
        PRESTART_SHUTDOWN_MODE,
        PRESTART_SHUTDOWN_MARKER,
        EVENT_TIMEOUT,
    )
    .expect("run isolated pre-start shutdown contract");
    assert!(
        run.success(),
        "isolated pre-start shutdown contract failed: {}",
        String::from_utf8_lossy(run.stderr())
    );
}

#[test]
fn synchronous_serve_shutdown_cancels_pending_connection_permit() {
    const TEST_NAME: &str =
        "connection_limits::synchronous_serve_shutdown_cancels_pending_connection_permit";

    match common::is_private_child(PENDING_PERMIT_SHUTDOWN_MODE) {
        true => {
            camber::runtime::builder()
                .connection_limit(1)
                .shutdown_timeout(Duration::from_secs(1))
                .run(|| {
                    let listener = camber::net::listen("127.0.0.1:0")
                        .expect("bind pending-permit shutdown listener");
                    let addr = listener
                        .local_addr()
                        .expect("pending-permit listener address")
                        .tcp()
                        .expect("TCP listener address");
                    let controller = lifecycle(addr).expect("register pending-permit listener");
                    arm_pending_permit_checkpoint(&controller);

                    let mut router = Router::new();
                    router.get("/sync", |_req: &Request| async {
                        Response::text(200, "sync")
                    });
                    let server =
                        camber::spawn(move || camber::http::serve_listener(listener, router));

                    let mut first = plain_stream(addr);
                    first
                        .write_all(b"GET /sync HTTP/1.1\r\nHost: localhost\r\n\r\n")
                        .expect("write first keepalive request");
                    assert_eq!(read_status(&mut first), 200);

                    let mut second = plain_stream(addr);
                    send_request(&mut second, "/sync");
                    wait_for_pending_permit(&controller);

                    camber::runtime::request_shutdown();
                    assert!(
                        server
                            .join()
                            .expect("join server during pending permit")
                            .is_ok(),
                        "synchronous server exits while permit acquisition is pending"
                    );
                    release_pending_permit(&controller);
                })
                .expect("run pending-permit shutdown runtime");
            println!("{PENDING_PERMIT_SHUTDOWN_MARKER}");
            return;
        }
        false => {}
    }

    let run = common::run_isolated_exact(
        TEST_NAME,
        PENDING_PERMIT_SHUTDOWN_MODE,
        PENDING_PERMIT_SHUTDOWN_MARKER,
        EVENT_TIMEOUT,
    )
    .expect("run isolated pending-permit shutdown contract");
    assert!(
        run.success(),
        "isolated pending-permit shutdown contract failed: {}",
        String::from_utf8_lossy(run.stderr())
    );
}

#[test]
fn synchronous_serve_reaches_the_shared_supervisor_checkpoints() {
    const TEST_NAME: &str =
        "connection_limits::synchronous_serve_reaches_the_shared_supervisor_checkpoints";

    match common::is_private_child(SYNCHRONOUS_ISOLATION_MODE) {
        true => {}
        false => {
            let run = common::run_isolated_exact(
                TEST_NAME,
                SYNCHRONOUS_ISOLATION_MODE,
                SYNCHRONOUS_ISOLATION_MARKER,
                Duration::from_secs(15),
            )
            .expect("run isolated synchronous lifecycle contract");
            assert!(
                run.success(),
                "isolated synchronous lifecycle contract failed: {}",
                String::from_utf8_lossy(run.stderr())
            );
            return;
        }
    }

    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(move || {
            let listener =
                camber::net::listen("127.0.0.1:0").expect("bind synchronous isolation listener");
            let addr = listener
                .local_addr()
                .expect("synchronous isolation listener address")
                .tcp()
                .expect("TCP listener address");
            let controller =
                lifecycle(addr).expect("register synchronous isolation listener address");
            let checkpoint = synchronous_connection_checkpoint();
            controller
                .pause_once(checkpoint)
                .expect("arm the shared supervisor checkpoint");

            let mut router = Router::new();
            router.get("/sync", |_req: &Request| async {
                Response::text(200, "sync")
            });
            let server = camber::spawn(move || camber::http::serve_listener(listener, router));

            let client = std::thread::spawn(move || {
                let mut client = plain_stream(addr);
                send_request(&mut client, "/sync");
                read_status(&mut client)
            });
            assert_supervisor_checkpoint_reached(&controller, checkpoint);
            assert_eq!(
                client.join().expect("the synchronous client thread joined"),
                200
            );
            drop(controller);

            camber::runtime::request_shutdown();
            assert!(
                server
                    .join()
                    .expect("join synchronous isolation server")
                    .is_ok(),
                "synchronous isolation server returns successfully"
            );
        })
        .unwrap();
    println!("{SYNCHRONOUS_ISOLATION_MARKER}");
}

#[cfg(feature = "ws")]
#[test]
fn synchronous_sse_serves_under_the_shared_supervisor() {
    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let listener =
                camber::net::listen("127.0.0.1:0").expect("bind synchronous SSE listener");
            let addr = listener
                .local_addr()
                .expect("synchronous SSE listener address")
                .tcp()
                .expect("TCP listener address");
            let controller = lifecycle(addr).expect("register synchronous SSE listener address");

            let mut router = Router::new();
            router.get_sse("/events", |_req: &Request, writer| {
                writer.event("message", "synchronous")
            });
            let server = camber::spawn(move || camber::http::serve_listener(listener, router));

            let mut client = plain_stream(addr);
            send_request(&mut client, "/events");
            let mut response = String::new();
            client
                .read_to_string(&mut response)
                .expect("read synchronous SSE response");
            assert!(response.starts_with("HTTP/1.1 200"), "{response}");
            assert!(response.contains("text/event-stream"), "{response}");
            assert!(response.contains("data: synchronous"), "{response}");
            drop(controller);

            camber::runtime::request_shutdown();
            assert!(
                server.join().expect("join synchronous SSE server").is_ok(),
                "synchronous SSE server returns successfully"
            );
        })
        .unwrap();
}

#[cfg(feature = "ws")]
#[test]
fn synchronous_direct_websocket_serves_under_the_shared_supervisor() {
    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let listener = camber::net::listen("127.0.0.1:0")
                .expect("bind synchronous direct WebSocket listener");
            let addr = listener
                .local_addr()
                .expect("synchronous direct WebSocket listener address")
                .tcp()
                .expect("TCP listener address");
            let controller =
                lifecycle(addr).expect("register synchronous direct WebSocket listener address");
            let (dispatched, _) = dispatch_channel();
            let server = camber::spawn(move || {
                camber::http::serve_listener(listener, direct_ws_router(dispatched))
            });

            let mut websocket = upgrade_websocket(plain_stream(addr), "/ws", "localhost");
            assert_ws_echo(&mut websocket);
            complete_client_initiated_close(&mut websocket);
            drop(controller);

            camber::runtime::request_shutdown();
            assert!(
                server
                    .join()
                    .expect("join synchronous direct WebSocket server")
                    .is_ok(),
                "synchronous direct WebSocket server returns successfully"
            );
        })
        .unwrap();
}

#[cfg(feature = "ws")]
#[test]
fn synchronous_proxy_websocket_serves_under_the_shared_supervisor() {
    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let backend_addr = spawn_ws_backend();
            let listener = camber::net::listen("127.0.0.1:0")
                .expect("bind synchronous proxy WebSocket listener");
            let addr = listener
                .local_addr()
                .expect("synchronous proxy WebSocket listener address")
                .tcp()
                .expect("TCP listener address");
            let controller =
                lifecycle(addr).expect("register synchronous proxy WebSocket listener address");
            let (dispatched, _) = dispatch_channel();
            let server = camber::spawn(move || {
                camber::http::serve_listener(listener, proxy_ws_router(backend_addr, dispatched))
            });

            let mut websocket = upgrade_websocket(plain_stream(addr), "/ws/echo", "localhost");
            assert_ws_echo(&mut websocket);
            complete_client_initiated_close(&mut websocket);
            drop(controller);

            camber::runtime::request_shutdown();
            assert!(
                server
                    .join()
                    .expect("join synchronous proxy WebSocket server")
                    .is_ok(),
                "synchronous proxy WebSocket server returns successfully"
            );
        })
        .unwrap();
}

#[cfg(feature = "ws")]
#[test]
fn serve_async_direct_websocket_holds_permit_until_owned_bridge_finishes() {
    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let (dispatched_tx, dispatched_rx) = dispatch_channel();
            let router = direct_ws_router(dispatched_tx);
            let listener = common::block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .expect("bind owned listener");
            let addr = listener.local_addr().expect("owned listener address");
            let controller = lifecycle(addr).expect("install owned listener controller");
            let completion = observe_owner(
                camber::http::serve_async(listener, router)
                    .expect("owned server requires a Tokio runtime"),
            );

            let mut websocket = upgrade_websocket(plain_stream(addr), "/ws", "localhost");
            assert_ws_echo(&mut websocket);

            arm_pending_permit_checkpoint(&controller);
            let mut second = plain_stream(addr);
            send_request(&mut second, "/second");
            wait_for_pending_permit(&controller);
            assert_not_dispatched(&dispatched_rx);

            camber::runtime::request_shutdown();
            release_pending_permit(&controller);
            receive_server_initiated_close(&mut websocket);
            acknowledge_server_close(&mut websocket);
            assert_owner_completed(&completion);
            assert!(
                dispatched_rx.try_recv().is_err(),
                "queued client dispatched during owned shutdown"
            );
        })
        .unwrap();
}

#[cfg(feature = "ws")]
#[test]
fn serve_async_hosts_tls_proxy_websocket_holds_permit_until_owned_bridge_finishes() {
    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let backend_addr = spawn_ws_backend();
            let (dispatched_tx, dispatched_rx) = dispatch_channel();
            let proxy = proxy_ws_router(backend_addr, dispatched_tx);
            let mut hosts = HostRouter::new();
            hosts.add("matrix.test", proxy);

            let (cert_pem, key_pem) = common::generate_self_signed_cert();
            let server_config = common::server_tls_config(&cert_pem, &key_pem);
            let client_config = Arc::new(common::tls_client_config(&[&cert_pem]));
            let listener = common::block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .expect("bind owned TLS listener");
            let addr = listener.local_addr().expect("owned TLS listener address");
            let controller = lifecycle(addr).expect("install owned TLS listener controller");
            let completion = observe_owner(
                camber::http::serve_async_hosts_tls(listener, hosts, server_config)
                    .expect("owned server requires a Tokio runtime"),
            );

            let mut websocket = upgrade_websocket(
                tls_stream(addr, Arc::clone(&client_config)),
                "/ws/echo",
                "matrix.test",
            );
            assert_ws_echo(&mut websocket);

            arm_pending_permit_checkpoint(&controller);
            let (attempted_tx, attempted_rx) = std::sync::mpsc::sync_channel(1);
            let second_config = Arc::clone(&client_config);
            let second = std::thread::spawn(move || {
                let mut stream = tls_stream(addr, second_config);
                attempted_tx
                    .send(())
                    .expect("report second TLS connection attempt");
                let request =
                    "GET /second HTTP/1.1\r\nHost: matrix.test\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(request.as_bytes());
                let mut byte = [0u8; 1];
                let _ = stream.read(&mut byte);
            });
            attempted_rx
                .recv_timeout(EVENT_TIMEOUT)
                .expect("second TLS client connects");
            wait_for_pending_permit(&controller);
            assert_not_dispatched(&dispatched_rx);

            camber::runtime::request_shutdown();
            release_pending_permit(&controller);
            receive_server_initiated_close(&mut websocket);
            acknowledge_server_close(&mut websocket);
            assert_owner_completed(&completion);
            second.join().expect("join second TLS client");
            assert!(
                dispatched_rx.try_recv().is_err(),
                "queued TLS client dispatched during owned shutdown"
            );
        })
        .unwrap();
}

#[cfg(feature = "ws")]
#[test]
fn serve_background_hosts_direct_websocket_holds_permit_until_owned_bridge_finishes() {
    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let (dispatched_tx, dispatched_rx) = dispatch_channel();
            let mut hosts = HostRouter::new();
            hosts.add("matrix.test", direct_ws_router(dispatched_tx));
            let listener = common::block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .expect("bind owned host listener");
            let addr = listener.local_addr().expect("owned host listener address");
            let controller = lifecycle(addr).expect("install owned host listener controller");
            let handle = camber::http::serve_background_hosts(listener, hosts)
                .expect("owned server requires a Tokio runtime");
            let completion = observe_owner(handle.into_future());

            let mut websocket = upgrade_websocket(plain_stream(addr), "/ws", "matrix.test");
            assert_ws_echo(&mut websocket);

            arm_pending_permit_checkpoint(&controller);
            let mut second = plain_stream(addr);
            send_request_with_host(&mut second, "/second", "matrix.test");
            wait_for_pending_permit(&controller);
            assert_not_dispatched(&dispatched_rx);

            camber::runtime::request_shutdown();
            release_pending_permit(&controller);
            receive_server_initiated_close(&mut websocket);
            acknowledge_server_close(&mut websocket);
            assert_owner_completed(&completion);
            assert!(
                dispatched_rx.try_recv().is_err(),
                "queued host client dispatched during owned shutdown"
            );
        })
        .unwrap();
}

#[cfg(feature = "ws")]
#[test]
fn serve_background_tls_proxy_websocket_holds_permit_until_owned_bridge_finishes() {
    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let backend_addr = spawn_ws_backend();
            let (dispatched_tx, dispatched_rx) = dispatch_channel();
            let proxy = proxy_ws_router(backend_addr, dispatched_tx);

            let (cert_pem, key_pem) = common::generate_self_signed_cert();
            let server_config = common::server_tls_config(&cert_pem, &key_pem);
            let client_config = Arc::new(common::tls_client_config(&[&cert_pem]));
            let listener = common::block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .expect("bind background TLS listener");
            let addr = listener
                .local_addr()
                .expect("background TLS listener address");
            let controller = lifecycle(addr).expect("install background TLS controller");
            let handle = camber::http::serve_background_tls(listener, proxy, server_config)
                .expect("owned server requires a Tokio runtime");
            let completion = observe_owner(handle.into_future());

            let mut websocket = upgrade_websocket(
                tls_stream(addr, Arc::clone(&client_config)),
                "/ws/echo",
                "localhost",
            );
            assert_ws_echo(&mut websocket);

            arm_pending_permit_checkpoint(&controller);
            let (attempted_tx, attempted_rx) = std::sync::mpsc::sync_channel(1);
            let second_config = Arc::clone(&client_config);
            let second = std::thread::spawn(move || {
                let mut stream = tls_stream(addr, second_config);
                attempted_tx
                    .send(())
                    .expect("report second TLS connection attempt");
                let request =
                    "GET /second HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
                let _ = stream.write_all(request.as_bytes());
                let mut byte = [0u8; 1];
                let _ = stream.read(&mut byte);
            });
            attempted_rx
                .recv_timeout(EVENT_TIMEOUT)
                .expect("second TLS client connects");
            wait_for_pending_permit(&controller);
            assert_not_dispatched(&dispatched_rx);

            camber::runtime::request_shutdown();
            release_pending_permit(&controller);
            receive_server_initiated_close(&mut websocket);
            acknowledge_server_close(&mut websocket);
            assert_owner_completed(&completion);
            second.join().expect("join second TLS client");
            assert!(
                dispatched_rx.try_recv().is_err(),
                "queued TLS client dispatched during owned shutdown"
            );
        })
        .unwrap();
}

#[cfg(feature = "ws")]
#[test]
fn serve_listener_direct_websocket_releases_permit_after_bridge_transport() {
    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let (dispatched_tx, dispatched_rx) = dispatch_channel();
            let listener = camber::net::listen("127.0.0.1:0").expect("bind sync listener");
            let addr = listener
                .local_addr()
                .expect("sync listener address")
                .tcp()
                .expect("TCP listener address");
            let controller = lifecycle(addr).expect("register sync listener address");
            let server = camber::spawn(move || {
                camber::http::serve_listener(listener, direct_ws_router(dispatched_tx))
            });

            let mut websocket = upgrade_websocket(plain_stream(addr), "/ws", "localhost");
            assert_ws_echo(&mut websocket);

            arm_pending_permit_checkpoint(&controller);
            let mut second = plain_stream(addr);
            send_request(&mut second, "/second");
            wait_for_pending_permit(&controller);
            assert_not_dispatched(&dispatched_rx);

            complete_client_initiated_close(&mut websocket);
            release_pending_permit(&controller);
            assert_dispatched(&dispatched_rx);
            assert_eq!(read_status(&mut second), 200);

            camber::runtime::request_shutdown();
            assert!(
                server.join().expect("join synchronous server task").is_ok(),
                "synchronous server returns successfully"
            );
        })
        .unwrap();
}

#[cfg(feature = "ws")]
#[test]
fn serve_listener_proxy_websocket_releases_permit_after_bridge_transport() {
    camber::runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let backend_addr = spawn_ws_backend();
            let (dispatched_tx, dispatched_rx) = dispatch_channel();
            let listener = camber::net::listen("127.0.0.1:0").expect("bind sync proxy listener");
            let proxy_addr = listener
                .local_addr()
                .expect("sync proxy listener address")
                .tcp()
                .expect("TCP listener address");
            let controller = lifecycle(proxy_addr).expect("register sync proxy listener address");
            let server = camber::spawn(move || {
                camber::http::serve_listener(listener, proxy_ws_router(backend_addr, dispatched_tx))
            });

            let mut websocket =
                upgrade_websocket(plain_stream(proxy_addr), "/ws/echo", "localhost");
            assert_ws_echo(&mut websocket);

            arm_pending_permit_checkpoint(&controller);
            let mut second = plain_stream(proxy_addr);
            send_request(&mut second, "/second");
            wait_for_pending_permit(&controller);
            assert_not_dispatched(&dispatched_rx);

            complete_client_initiated_close(&mut websocket);
            release_pending_permit(&controller);
            assert_dispatched(&dispatched_rx);
            assert_eq!(read_status(&mut second), 200);

            camber::runtime::request_shutdown();
            assert!(
                server.join().expect("join synchronous proxy task").is_ok(),
                "synchronous proxy server returns successfully"
            );
        })
        .unwrap();
}
