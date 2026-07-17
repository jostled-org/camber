mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

#[cfg(feature = "ws")]
use camber::RuntimeError;
#[cfg(feature = "ws")]
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, LifecycleFault, lifecycle};
#[cfg(feature = "ws")]
use camber::http::{HostRouter, Request, Response, Router, WsConn};
#[cfg(feature = "ws")]
use std::future::{Future, IntoFuture};
#[cfg(feature = "ws")]
use std::net::SocketAddr;
#[cfg(feature = "ws")]
use std::sync::Arc;
#[cfg(feature = "ws")]
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

#[cfg(feature = "ws")]
const EVENT_TIMEOUT: Duration = Duration::from_secs(5);

#[test]
fn connection_limit_zero_rejected() {
    let err = camber::runtime::builder()
        .connection_limit(0)
        .keepalive_timeout(Duration::from_secs(5))
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
    let request = format!(
        "GET {path} HTTP/1.1\r\n\
         Host: {host}\r\n\
         Upgrade: websocket\r\n\
         Connection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
         Sec-WebSocket-Version: 13\r\n\
         \r\n"
    );
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

#[cfg(feature = "ws")]
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

#[cfg(feature = "ws")]
fn arm_pending_permit_checkpoint(controller: &LifecycleController) {
    controller
        .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
        .expect("arm pending connection-permit checkpoint");
}

#[cfg(feature = "ws")]
fn wait_for_pending_permit(controller: &LifecycleController) {
    common::block_on(
        controller.wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending),
    )
    .expect("production connection-permit acquisition returned Pending");
}

#[cfg(feature = "ws")]
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
fn assert_owner_pending(completion: &Receiver<Result<(), RuntimeError>>) {
    match completion.try_recv() {
        Err(TryRecvError::Empty) => {}
        Ok(result) => panic!(
            "owned server completed before the client acknowledged its WebSocket close: {result:?}"
        ),
        Err(TryRecvError::Disconnected) => {
            panic!("owned server completion observer disconnected")
        }
    }
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

#[cfg(feature = "ws")]
fn synchronous_normal_checkpoints() -> [LifecycleCheckpoint; 11] {
    [
        LifecycleCheckpoint::BeforeSupervisorSelect,
        LifecycleCheckpoint::SupervisorSelectedDeadline,
        LifecycleCheckpoint::SupervisorSelectedControl,
        LifecycleCheckpoint::SupervisorSelectedRuntime,
        LifecycleCheckpoint::SupervisorSelectedAccept,
        LifecycleCheckpoint::SupervisorSelectedPermit,
        LifecycleCheckpoint::SupervisorSelectedRegistration,
        LifecycleCheckpoint::SupervisorSelectedTask,
        LifecycleCheckpoint::AfterSupervisorResultSend,
        LifecycleCheckpoint::AfterPermit,
        LifecycleCheckpoint::BeforeRuntimeWait,
    ]
}

#[cfg(feature = "ws")]
fn synchronous_sse_checkpoints() -> [LifecycleCheckpoint; 3] {
    [
        LifecycleCheckpoint::AfterPermit,
        LifecycleCheckpoint::BeforeRuntimeWait,
        LifecycleCheckpoint::SseBufferConfigured(32),
    ]
}

#[cfg(feature = "ws")]
fn synchronous_websocket_checkpoints() -> [LifecycleCheckpoint; 6] {
    [
        LifecycleCheckpoint::AfterPermit,
        LifecycleCheckpoint::BeforeRuntimeWait,
        LifecycleCheckpoint::AfterUpgradeTicketSubmitted,
        LifecycleCheckpoint::BeforeUpgradeAcknowledge,
        LifecycleCheckpoint::WebSocketOutgoingBufferConfigured(32),
        LifecycleCheckpoint::WebSocketIncomingBufferConfigured(32),
    ]
}

#[cfg(feature = "ws")]
fn arm_checkpoints(controller: &LifecycleController, checkpoints: &[LifecycleCheckpoint]) {
    for checkpoint in checkpoints {
        controller
            .pause_once(*checkpoint)
            .expect("arm synchronous-forbidden checkpoint");
    }
}

#[cfg(feature = "ws")]
fn assert_checkpoints_unconsumed(
    controller: &LifecycleController,
    checkpoints: &[LifecycleCheckpoint],
) {
    for checkpoint in checkpoints {
        assert_invalid(controller.release(*checkpoint));
    }
}

#[cfg(feature = "ws")]
fn lifecycle_faults() -> [LifecycleFault; 5] {
    [
        LifecycleFault::Accept(std::io::ErrorKind::Other),
        LifecycleFault::PanicNextOwnedTask,
        LifecycleFault::PanicNextOwnedTaskOpaque,
        LifecycleFault::CancelNextOwnedTask,
        LifecycleFault::PanicSupervisorCore,
    ]
}

#[cfg(feature = "ws")]
fn assert_invalid<T>(result: Result<T, RuntimeError>) {
    assert!(
        matches!(result, Err(RuntimeError::InvalidArgument(_))),
        "expected InvalidArgument"
    );
}

#[test]
fn connection_limit_blocks_third_connection_until_slot_frees() {
    camber::runtime::builder()
        .connection_limit(2)
        .keepalive_timeout(Duration::from_secs(5))
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
        .keepalive_timeout(Duration::from_millis(200))
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

#[cfg(feature = "ws")]
#[test]
fn synchronous_serve_cannot_consume_supervisor_checkpoints_or_faults() {
    for fault in lifecycle_faults() {
        camber::runtime::builder()
            .connection_limit(1)
            .keepalive_timeout(Duration::from_secs(5))
            .shutdown_timeout(Duration::from_secs(2))
            .run(move || {
                let listener = camber::net::listen("127.0.0.1:0")
                    .expect("bind synchronous isolation listener");
                let addr = listener
                    .local_addr()
                    .expect("synchronous isolation listener address")
                    .tcp()
                    .expect("TCP listener address");
                let controller =
                    lifecycle(addr).expect("register synchronous isolation listener address");
                let checkpoints = synchronous_normal_checkpoints();
                arm_checkpoints(&controller, &checkpoints);
                controller
                    .inject_once(fault)
                    .expect("inject owned-path-only fault");

                let mut router = Router::new();
                router.get("/sync", |_req: &Request| async {
                    Response::text(200, "sync")
                });
                let server = camber::spawn(move || camber::http::serve_listener(listener, router));

                let mut client = plain_stream(addr);
                send_request(&mut client, "/sync");
                assert_eq!(read_status(&mut client), 200);

                assert_checkpoints_unconsumed(&controller, &checkpoints);
                assert_invalid(controller.inject_once(LifecycleFault::PanicSupervisorCore));
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
    }
}

#[cfg(feature = "ws")]
#[test]
fn synchronous_sse_cannot_consume_owned_buffer_checkpoint() {
    camber::runtime::builder()
        .connection_limit(1)
        .keepalive_timeout(Duration::from_secs(5))
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
            let checkpoints = synchronous_sse_checkpoints();
            arm_checkpoints(&controller, &checkpoints);

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
            assert_checkpoints_unconsumed(&controller, &checkpoints);
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
fn synchronous_direct_websocket_cannot_consume_owned_upgrade_or_buffer_checkpoints() {
    camber::runtime::builder()
        .connection_limit(1)
        .keepalive_timeout(Duration::from_secs(5))
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
            let checkpoints = synchronous_websocket_checkpoints();
            arm_checkpoints(&controller, &checkpoints);
            let (dispatched, _) = dispatch_channel();
            let server = camber::spawn(move || {
                camber::http::serve_listener(listener, direct_ws_router(dispatched))
            });

            let mut websocket = upgrade_websocket(plain_stream(addr), "/ws", "localhost");
            assert_ws_echo(&mut websocket);
            assert_checkpoints_unconsumed(&controller, &checkpoints);
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
fn synchronous_proxy_websocket_cannot_consume_owned_upgrade_or_buffer_checkpoints() {
    camber::runtime::builder()
        .connection_limit(1)
        .keepalive_timeout(Duration::from_secs(5))
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
            let checkpoints = synchronous_websocket_checkpoints();
            arm_checkpoints(&controller, &checkpoints);
            let (dispatched, _) = dispatch_channel();
            let server = camber::spawn(move || {
                camber::http::serve_listener(listener, proxy_ws_router(backend_addr, dispatched))
            });

            let mut websocket = upgrade_websocket(plain_stream(addr), "/ws/echo", "localhost");
            assert_ws_echo(&mut websocket);
            assert_checkpoints_unconsumed(&controller, &checkpoints);
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
        .keepalive_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let (dispatched_tx, dispatched_rx) = dispatch_channel();
            let router = direct_ws_router(dispatched_tx);
            let listener = common::block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .expect("bind owned listener");
            let addr = listener.local_addr().expect("owned listener address");
            let controller = lifecycle(addr).expect("install owned listener controller");
            let completion = observe_owner(camber::http::serve_async(listener, router));

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
            assert_owner_pending(&completion);
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
        .keepalive_timeout(Duration::from_secs(5))
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
            let completion = observe_owner(camber::http::serve_async_hosts_tls(
                listener,
                hosts,
                server_config,
            ));

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
            assert_owner_pending(&completion);
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
        .keepalive_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let (dispatched_tx, dispatched_rx) = dispatch_channel();
            let mut hosts = HostRouter::new();
            hosts.add("matrix.test", direct_ws_router(dispatched_tx));
            let listener = common::block_on(tokio::net::TcpListener::bind("127.0.0.1:0"))
                .expect("bind owned host listener");
            let addr = listener.local_addr().expect("owned host listener address");
            let controller = lifecycle(addr).expect("install owned host listener controller");
            let handle = camber::http::serve_background_hosts(listener, hosts);
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
            assert_owner_pending(&completion);
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
        .keepalive_timeout(Duration::from_secs(5))
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
            let handle = camber::http::serve_background_tls(listener, proxy, server_config);
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
            assert_owner_pending(&completion);
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
        .keepalive_timeout(Duration::from_secs(5))
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
        .keepalive_timeout(Duration::from_secs(5))
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
