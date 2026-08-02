#![cfg(feature = "ws")]

use crate::common;
use crate::common::{
    assert_graceful_close_then_eof, assert_http_ok, assert_optional_close_then_eof,
    assert_transport_eof, attach_dispatch_probe, lifecycle_event, read_async_http_head,
    read_async_ws_frame_or_eof, read_until_double_crlf, read_ws_text_frame, status_from_raw,
    write_async_ws_frame, write_ws_close_frame, write_ws_text_frame,
};
use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, LifecycleFault, lifecycle};
use camber::http::{
    self, DisconnectCause, DisconnectSignal, Next, Request, Response, Router, WsConn,
};
use camber::runtime;
use futures_util::{SinkExt, StreamExt};
use std::future::IntoFuture;
use std::io::Write;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

async fn send_async_proxy_upgrade(stream: &mut tokio::net::TcpStream) {
    tokio::io::AsyncWriteExt::write_all(stream, common::ws_upgrade_request("/ws/echo").as_bytes())
        .await
        .expect("write proxied WebSocket upgrade request");
}

async fn connect_async_proxy_websocket(addr: std::net::SocketAddr) -> tokio::net::TcpStream {
    let mut stream = lifecycle_event(
        "proxied WebSocket TCP connection",
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .expect("connect proxied WebSocket peer");
    send_async_proxy_upgrade(&mut stream).await;
    let response = read_async_http_head(&mut stream, "the proxied WebSocket handshake").await;
    assert_eq!(
        status_from_raw(&response),
        101,
        "expected proxied WebSocket upgrade, got: {response}"
    );
    stream
}

async fn assert_proxy_echo(stream: &mut tokio::net::TcpStream) {
    write_async_ws_frame(stream, 0x1, b"bridge-live", "the proxied echo probe").await;
    let (opcode, payload) = read_async_ws_frame_or_eof(stream, "the proxied echo reply")
        .await
        .expect("proxy echo arrives before EOF");
    assert_eq!(opcode, 0x1, "expected proxied WebSocket text frame");
    assert_eq!(payload.as_ref(), b"bridge-live");
}

struct LifecycleWsBackend {
    addr: std::net::SocketAddr,
    shutdown: tokio::sync::oneshot::Sender<()>,
    task: tokio::task::JoinHandle<()>,
}

impl LifecycleWsBackend {
    async fn shutdown(self) {
        let _ = self.shutdown.send(());
        lifecycle_event("lifecycle WebSocket backend shutdown", self.task)
            .await
            .expect("join lifecycle WebSocket backend");
    }
}

async fn spawn_lifecycle_ws_backend(connection_count: Arc<AtomicUsize>) -> LifecycleWsBackend {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind lifecycle WebSocket backend");
    let addr = listener
        .local_addr()
        .expect("lifecycle WebSocket backend address");
    let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let accepted = tokio::select! {
            biased;
            _ = &mut shutdown_rx => return,
            accepted = listener.accept() => accepted.expect("accept lifecycle backend peer"),
        };
        let mut websocket = tokio_tungstenite::accept_async(accepted.0)
            .await
            .expect("accept lifecycle backend WebSocket");
        connection_count.fetch_add(1, Ordering::AcqRel);

        loop {
            let message = tokio::select! {
                biased;
                _ = &mut shutdown_rx => break,
                message = websocket.next() => message,
            };
            let message = match message {
                Some(Ok(message)) => message,
                Some(Err(_)) | None => break,
            };
            let closes = message.is_close();
            if websocket.send(message).await.is_err() || closes {
                break;
            }
        }
    });
    LifecycleWsBackend {
        addr,
        shutdown,
        task,
    }
}

fn lifecycle_proxy_router(backend_addr: std::net::SocketAddr) -> Router {
    let mut proxy = Router::new();
    proxy.proxy("/ws", &format!("http://{backend_addr}"));
    proxy
}

/// How long a signal that must NOT be resolved is watched before it counts as
/// unresolved. Short on purpose: the assertion is falsified by a resolution,
/// not by waiting longer.
const UNRESOLVED_WINDOW: Duration = Duration::from_millis(500);

/// Capture proxied requests' response-lifetime signals from production
/// middleware, which runs before the handoff those signals resolve at.
///
/// The signal itself is handed out rather than a resolved cause, so the case
/// can read the terminal cause more than once and show it does not change.
fn capture_proxy_signals(
    proxy: &mut Router,
) -> tokio::sync::mpsc::UnboundedReceiver<DisconnectSignal> {
    let (signals, captured) = tokio::sync::mpsc::unbounded_channel();
    proxy.use_middleware(move |request: &Request, next: Next| {
        match request.path().starts_with("/ws") {
            true => drop(signals.send(request.on_disconnect())),
            false => {}
        }
        // Returned as it stands: `Next::call` already hands back a boxed
        // future, which satisfies the middleware's future bound. Wrapping it in
        // an async block would add a second state machine and a second
        // allocation to every proxied request for no change in behavior.
        next.call(request)
    });
    captured
}

// 8.T2, proxied WebSocket response-lifetime handoff.
//
// The proxied path takes the same explicit handoff as the direct one: nothing
// is resolved while the registrar still holds the upgrade, and the committed
// 101 is what establishes Completed. Bridge ownership is untouched — the owner
// still joins the registered bridge at shutdown.
#[camber::test]
async fn proxied_websocket_resolves_completed_at_handoff() {
    let backend = spawn_lifecycle_ws_backend(Arc::new(AtomicUsize::new(0))).await;
    let backend_addr = backend.addr;
    let mut proxy = lifecycle_proxy_router(backend_addr);
    let mut captured = capture_proxy_signals(&mut proxy);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy handoff listener");
    let proxy_addr = listener.local_addr().expect("proxy handoff address");
    let controller = lifecycle(proxy_addr).expect("install proxy handoff controller");
    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("pause proxy handoff acknowledgement");
    let handle = camber::http::serve_background(listener, proxy);

    let mut peer = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect proxy handoff peer");
    send_async_proxy_upgrade(&mut peer).await;
    lifecycle_event(
        "proxy handoff acknowledgement checkpoint",
        controller.wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge),
    )
    .await
    .expect("proxy upgrade reaches acknowledgement checkpoint");

    let signal = lifecycle_event("proxied upgrade signal capture", captured.recv())
        .await
        .expect("the proxy middleware never captured a request signal");
    assert!(
        tokio::time::timeout(UNRESOLVED_WINDOW, signal.cancelled())
            .await
            .is_err(),
        "the proxied upgrade's signal resolved before its 101 was handed off"
    );

    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release proxy handoff acknowledgement");
    let response = read_async_http_head(&mut peer, "the proxied handoff upgrade response").await;
    assert_eq!(
        status_from_raw(&response),
        101,
        "expected a proxied WebSocket upgrade, got: {response}"
    );
    assert_eq!(
        lifecycle_event("proxied handoff completion", signal.cancelled()).await,
        DisconnectCause::Completed,
        "the proxied upgrade did not resolve Completed at its 101 handoff"
    );

    write_async_ws_frame(&mut peer, 0x8, &[], "the proxied handoff close frame").await;
    match read_async_ws_frame_or_eof(&mut peer, "the proxied handoff close reply").await {
        None => {}
        Some((0x8, _)) => assert_transport_eof(&mut peer, "the proxied handoff transport").await,
        Some((opcode, payload)) => {
            panic!("proxied close emitted opcode {opcode:#x} with payload {payload:?}")
        }
    }
    assert_eq!(
        lifecycle_event("proxied cause after peer close", signal.cancelled()).await,
        DisconnectCause::Completed,
        "the upgraded peer closing changed the cause established at the handoff"
    );

    runtime::request_shutdown();
    assert!(
        lifecycle_event("proxy handoff owner join", handle.into_future())
            .await
            .is_ok()
    );
    backend.shutdown().await;
}

/// A WebSocket echo backend the synchronous entry path can serve.
///
/// `spawn_lifecycle_ws_backend` cannot stand in here: it is an async
/// `tokio-tungstenite` server that needs an async setup and an awaited
/// shutdown, while this case runs entirely inside a synchronous
/// `runtime::run` closure. This is a router the same `common::spawn_server`
/// serves, so no handshake logic is duplicated.
fn echo_ws_backend() -> Router {
    let mut backend = Router::new();
    backend.ws("/echo", |_req: &Request, mut conn: WsConn| {
        while let Some(message) = conn.recv() {
            if conn.send(&message).is_err() {
                break;
            }
        }
        Ok(())
    });
    backend
}

// 8.T4, synchronous-entry proxied WebSocket response-lifetime handoff.
//
// `serve_listener` gives its connections no upgrade registrar, so a proxied
// upgrade served this way takes the detached-bridge branch that no owned
// fixture reaches — 8.T2 pauses at an acknowledgement this path does not have.
// Deleting the handoff from that branch makes this case report StreamReset
// while every registrar-path proof stays green.
//
// Nothing needs holding here: the handoff resolves before the response returns
// to Hyper, so a cause read once the 101 is on the wire is already past the
// point being proven.
#[test]
fn synchronous_entry_proxied_websocket_resolves_completed_at_handoff() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let backend_addr = common::spawn_server(echo_ws_backend());
            let mut proxy = lifecycle_proxy_router(backend_addr);
            let mut captured = capture_proxy_signals(&mut proxy);
            let proxy_addr = common::spawn_server(proxy);

            // No read bound is set here: every `ws` helper installs its own for
            // the length of its call, so one set on the peer would only be
            // overwritten and restored around each read.
            let mut peer = TcpStream::connect(proxy_addr).expect("connect synchronous proxy peer");
            peer.write_all(common::ws_upgrade_request("/ws/echo").as_bytes())
                .expect("write the synchronous proxied upgrade request");
            let head = read_until_double_crlf(&mut peer);
            assert_eq!(
                status_from_raw(&head),
                101,
                "expected a proxied WebSocket upgrade, got: {head}"
            );

            // Bounded like every other wait here: a middleware that never
            // captured the request must fail this case, not park it on an
            // unbounded receive.
            let signal = runtime::block_on(lifecycle_event(
                "synchronous proxied upgrade signal capture",
                captured.recv(),
            ))
            .expect("the proxy middleware never captured a request signal");
            assert_eq!(
                runtime::block_on(lifecycle_event(
                    "synchronous proxied handoff completion",
                    signal.cancelled()
                )),
                DisconnectCause::Completed,
                "a synchronous-entry proxied upgrade did not resolve Completed at its 101 handoff"
            );

            write_ws_text_frame(&mut peer, "hello");
            assert_eq!(&*read_ws_text_frame(&mut peer), "hello");
            write_ws_close_frame(&mut peer);
            assert_eq!(
                runtime::block_on(lifecycle_event(
                    "synchronous proxied cause after peer close",
                    signal.cancelled()
                )),
                DisconnectCause::Completed,
                "the upgraded peer closing changed the cause established at the handoff"
            );

            runtime::request_shutdown();
        })
        .expect("the synchronous proxy runtime did not return cleanly");
}

fn assert_cancelled(result: Result<(), RuntimeError>) {
    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}

#[test]
fn websocket_proxy_forwards_text_messages() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let backend_addr = common::spawn_server(echo_ws_backend());

            // Proxy: forward /ws/* to backend
            let proxy_addr = common::spawn_server(lifecycle_proxy_router(backend_addr));

            // Client: WebSocket handshake through proxy
            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .write_all(common::ws_upgrade_request("/ws/echo").as_bytes())
                .unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(status_from_raw(&resp), 101, "response: {resp}");

            // Send "hello", expect "hello" back
            write_ws_text_frame(&mut stream, "hello");
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(&*msg, "hello");

            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_proxy_handles_client_close() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: sends 3 messages then waits
            let mut backend = Router::new();
            backend.ws("/chat", |_req: &Request, mut conn: WsConn| {
                conn.send("one")?;
                conn.send("two")?;
                conn.send("three")?;
                // Wait for client to close
                let _ = conn.recv();
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            let proxy_addr = common::spawn_server(lifecycle_proxy_router(backend_addr));

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .write_all(common::ws_upgrade_request("/ws/chat").as_bytes())
                .unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(status_from_raw(&resp), 101, "response: {resp}");

            // Receive 3 messages
            let m1 = read_ws_text_frame(&mut stream);
            let m2 = read_ws_text_frame(&mut stream);
            let m3 = read_ws_text_frame(&mut stream);
            assert_eq!([&*m1, &*m2, &*m3], ["one", "two", "three"]);

            // Client sends close — proxy should clean up without panic
            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_proxy_coexists_with_http_proxy() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: serves both HTTP and WebSocket
            let mut backend = echo_ws_backend();
            backend.get("/hello", |_req: &Request| async {
                Response::text(200, "http-ok")
            });
            let backend_addr = common::spawn_server(backend);

            // Single proxy prefix handles both HTTP and WS
            let mut proxy = Router::new();
            proxy.proxy("/api", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            // HTTP GET through proxy
            let resp =
                common::block_on(http::get(&format!("http://{proxy_addr}/api/hello"))).unwrap();
            assert_eq!(resp.status(), 200);
            assert_eq!(resp.body(), "http-ok");

            // WebSocket upgrade through same proxy prefix
            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .write_all(common::ws_upgrade_request("/api/echo").as_bytes())
                .unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(status_from_raw(&resp), 101, "response: {resp}");

            write_ws_text_frame(&mut stream, "ping");
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(&*msg, "ping");

            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_proxy_rejects_cross_host_origin_before_upstream_upgrade() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: WebSocket echo server
            let mut backend = Router::new();
            backend.ws("/echo", |_req: &Request, conn: WsConn| {
                conn.send("should not reach")?;
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            let proxy_addr = common::spawn_server(lifecycle_proxy_router(backend_addr));

            // Send proxied WS upgrade with mismatched Origin
            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            let upgrade_req = common::ws_upgrade_request_with(
                "/ws/echo",
                &[("Origin", "http://evil.example.com")],
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(status_from_raw(&resp), 403, "response: {resp}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_proxy_forwards_sec_websocket_protocol() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: echo Sec-WebSocket-Protocol as first WS message
            let mut backend = Router::new();
            backend.ws("/echo", |req: &Request, conn: WsConn| {
                let proto = req
                    .headers()
                    .find(|(k, _)| k.eq_ignore_ascii_case("sec-websocket-protocol"))
                    .map(|(_, v)| v.to_owned())
                    .unwrap_or_else(|| "none".to_owned());
                conn.send(&proto)?;
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            let proxy_addr = common::spawn_server(lifecycle_proxy_router(backend_addr));

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            let upgrade_req = common::ws_upgrade_request_with(
                "/ws/echo",
                &[("Sec-WebSocket-Protocol", "graphql-ws, graphql-transport-ws")],
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(status_from_raw(&resp), 101, "response: {resp}");
            // Client should see the subprotocol in the 101 response
            let lower = resp.to_lowercase();
            assert!(
                lower.contains("sec-websocket-protocol: graphql-ws"),
                "expected Sec-WebSocket-Protocol in 101 response: {resp}"
            );

            // The backend receives only the protocol committed to the client.
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(
                &*msg, "graphql-ws",
                "backend should receive Sec-WebSocket-Protocol header"
            );

            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_proxy_strips_spoofed_forwarded_headers() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut backend = Router::new();
            backend.ws("/echo", |req: &Request, conn: WsConn| {
                let forwarded_for = req
                    .headers()
                    .find(|(k, _)| k.eq_ignore_ascii_case("x-forwarded-for"))
                    .map(|(_, v)| v.to_owned())
                    .unwrap_or_else(|| "none".to_owned());
                conn.send(&forwarded_for)?;
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            let proxy_addr = common::spawn_server(lifecycle_proxy_router(backend_addr));

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            let upgrade_req =
                common::ws_upgrade_request_with("/ws/echo", &[("X-Forwarded-For", "6.6.6.6")]);
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(status_from_raw(&resp), 101, "response: {resp}");

            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(&*msg, "none", "spoofed forwarding header reached backend");

            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_proxy_rejects_invalid_backend_scheme() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend configured with ftp:// — not http:// or https://, should return 502
            let mut proxy = Router::new();
            proxy.proxy("/ws", "ftp://127.0.0.1:1");
            let proxy_addr = common::spawn_server(proxy);

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .write_all(common::ws_upgrade_request("/ws/echo").as_bytes())
                .unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(status_from_raw(&resp), 502, "response: {resp}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_proxy_stream_upgrade_ignores_request_body_limit() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let backend_addr = common::spawn_server(echo_ws_backend());

            let mut proxy = Router::new().max_request_body(10);
            proxy.proxy_stream("/ws", &format!("http://{backend_addr}"));
            let proxy_addr = common::spawn_server(proxy);

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            let upgrade_req =
                common::ws_upgrade_request_with("/ws/echo", &[("Content-Length", "99999")]);
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(status_from_raw(&resp), 101, "response: {resp}");

            write_ws_text_frame(&mut stream, "hello");
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(&*msg, "hello");

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

// 1.T9, proxied WebSocket portion.
#[test]
fn proxied_websocket_bridge_holds_permit_and_finishes_before_owned_completion() {
    runtime::builder()
        .connection_limit(1)
        .keepalive_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            runtime::block_on(async {
                let backend_connections = Arc::new(AtomicUsize::new(0));
                let backend = spawn_lifecycle_ws_backend(Arc::clone(&backend_connections)).await;
                let mut proxy = lifecycle_proxy_router(backend.addr);
                let mut dispatched = attach_dispatch_probe(&mut proxy);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind owned proxy listener");
                let proxy_addr = listener.local_addr().expect("owned proxy listener address");
                let controller = lifecycle(proxy_addr).expect("install proxy controller");
                let handle = camber::http::serve_background(listener, proxy);
                let mut websocket = connect_async_proxy_websocket(proxy_addr).await;
                assert_proxy_echo(&mut websocket).await;
                assert_eq!(backend_connections.load(Ordering::Acquire), 1);
                controller
                    .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .expect("pause when the proxy permit wait becomes pending");
                let mut second = tokio::net::TcpStream::connect(proxy_addr)
                    .await
                    .expect("connect permit-waiting proxy peer");
                tokio::io::AsyncWriteExt::write_all(
                    &mut second,
                    b"GET /second HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write permit-waiting proxy request");
                controller
                    .wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .await
                    .expect("proxy semaphore acquisition returned pending");
                assert!(
                    matches!(
                        dispatched.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                    ),
                    "second request dispatched while the proxy bridge held the permit"
                );

                runtime::request_shutdown();
                controller
                    .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .expect("release pending proxy permit wait into shutdown");
                let (opcode, _) =
                    read_async_ws_frame_or_eof(&mut websocket, "the permit-holding proxy close")
                        .await
                        .expect("graceful proxy shutdown sends a close frame");
                assert_eq!(opcode, 0x8, "expected graceful proxied close frame");
                let mut owner = Box::pin(handle.into_future());
                assert!(
                    tokio::time::timeout(UNRESOLVED_WINDOW, owner.as_mut())
                        .await
                        .is_err(),
                    "owner completed while the proxy bridge still owned its transport"
                );
                write_async_ws_frame(
                    &mut websocket,
                    0x8,
                    &[],
                    "the permit-holding proxy close reply",
                )
                .await;
                assert_transport_eof(&mut websocket, "the permit-holding proxy transport").await;
                assert_transport_eof(&mut second, "the permit-waiting proxy transport").await;
                assert!(
                    lifecycle_event("owned proxy bridge completion", owner.as_mut())
                        .await
                        .is_ok()
                );
                assert!(
                    matches!(
                        dispatched.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
                    ),
                    "permit-waiting proxy dispatch sender remained live after owner completion"
                );
                backend.shutdown().await;
            });
        })
        .unwrap();
}

// 1.T18, graceful proxied WebSocket portion.
#[camber::test]
async fn graceful_proxy_websocket_shutdown_sends_close_before_eof_and_join() {
    let backend = spawn_lifecycle_ws_backend(Arc::new(AtomicUsize::new(0))).await;
    let backend_addr = backend.addr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind graceful proxy listener");
    let proxy_addr = listener.local_addr().expect("graceful proxy address");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend_addr));
    let mut websocket = connect_async_proxy_websocket(proxy_addr).await;
    assert_proxy_echo(&mut websocket).await;

    runtime::request_shutdown();
    let mut owner = Box::pin(handle.into_future());
    assert_graceful_close_then_eof(&mut websocket, "graceful proxy").await;
    assert!(
        lifecycle_event("graceful proxy bridge join", owner.as_mut())
            .await
            .is_ok()
    );
    backend.shutdown().await;
}

// 1.T18, forced proxied WebSocket portion.
#[camber::test]
async fn forced_proxy_websocket_abort_releases_transport_before_cancelled() {
    let backend = spawn_lifecycle_ws_backend(Arc::new(AtomicUsize::new(0))).await;
    let backend_addr = backend.addr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind forced proxy listener");
    let proxy_addr = listener.local_addr().expect("forced proxy address");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend_addr));
    let mut websocket = connect_async_proxy_websocket(proxy_addr).await;
    assert_proxy_echo(&mut websocket).await;

    handle.cancel();
    let mut owner = Box::pin(handle.into_future());
    assert_optional_close_then_eof(&mut websocket, "forced proxy").await;
    assert_cancelled(lifecycle_event("forced proxy bridge join", owner.as_mut()).await);
    backend.shutdown().await;
}

async fn pending_proxy_upgrade_shutdown_is_rejected(forced: bool) {
    let backend_connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_lifecycle_ws_backend(Arc::clone(&backend_connections)).await;
    let backend_addr = backend.addr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind pending proxy-upgrade listener");
    let proxy_addr = listener.local_addr().expect("pending proxy address");
    let controller = lifecycle(proxy_addr).expect("install pending proxy controller");
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("pause after pending proxy ticket submission");
    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("pause pending proxy upgrade");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend_addr));
    let mut pending = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect pending proxied WebSocket peer");
    send_async_proxy_upgrade(&mut pending).await;
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .await
        .expect("pending proxy ticket reaches the supervisor channel");
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("release submitted pending proxy ticket");
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .await
        .expect("proxy upgrade reaches acknowledgement checkpoint");
    match forced {
        true => handle.cancel(),
        false => runtime::request_shutdown(),
    }
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release pending proxy upgrade into shutdown");
    let mut owner = Box::pin(handle.into_future());
    let response = read_async_http_head(&mut pending, "the rejected proxy-upgrade response").await;
    let status = status_from_raw(&response);
    let response_lower = response.to_ascii_lowercase();
    assert_eq!(
        status, 503,
        "shutdown committed an unexpected proxy upgrade response: {response}"
    );
    assert!(
        response_lower.contains("connection: close"),
        "proxy upgrade rejection omitted Connection: close: {response}"
    );
    assert_eq!(
        backend_connections.load(Ordering::Acquire),
        0,
        "rejected proxy upgrade reached its backend"
    );
    assert_transport_eof(&mut pending, "the rejected proxy-upgrade transport").await;
    let result = lifecycle_event("pending proxy-upgrade drain", owner.as_mut()).await;
    match forced {
        true => assert_cancelled(result),
        false => assert!(result.is_ok(), "graceful proxy owner returned {result:?}"),
    }
    backend.shutdown().await;
}

// 1.T21, proxied WebSocket registrar-cancellation portion.
#[camber::test]
async fn cancelled_pending_proxy_upgrade_is_joined_and_connection_local() {
    let backend_connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_lifecycle_ws_backend(Arc::clone(&backend_connections)).await;
    let backend_addr = backend.addr;
    let mut proxy = lifecycle_proxy_router(backend_addr);
    proxy.get("/ok", |_request: &Request| async {
        Response::text(200, "ok")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy cancellation listener");
    let proxy_addr = listener.local_addr().expect("proxy cancellation address");
    let controller = lifecycle(proxy_addr).expect("install proxy cancellation controller");
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("pause after cancellable proxy ticket submission");
    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("pause cancellable proxy upgrade");
    controller
        .pause_once(LifecycleCheckpoint::UpgradePeerClosed)
        .expect("pause after proxy peer closure is observed");
    let handle = camber::http::serve_background(listener, proxy);
    let mut pending = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect cancellable proxy peer");
    send_async_proxy_upgrade(&mut pending).await;
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .await
        .expect("cancellable proxy ticket reaches the supervisor channel");
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("release submitted cancellable proxy ticket");
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .await
        .expect("cancellable proxy upgrade reaches acknowledgement checkpoint");
    drop(pending);
    lifecycle_event(
        "owned reader observation of proxy peer closure",
        controller.wait_until_paused(LifecycleCheckpoint::UpgradePeerClosed),
    )
    .await
    .expect("owned reader observes proxy peer closure");
    controller
        .release(LifecycleCheckpoint::UpgradePeerClosed)
        .expect("release observed proxy peer closure");
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release cancelled proxy registration");

    assert_http_ok(
        proxy_addr,
        "/ok",
        "the proxy listener after registrar cancellation",
    )
    .await;
    assert_eq!(
        backend_connections.load(Ordering::Acquire),
        0,
        "cancelled proxy upgrade reached its backend"
    );
    runtime::request_shutdown();
    assert!(
        lifecycle_event(
            "proxy owner join after registrar cancellation",
            handle.into_future()
        )
        .await
        .is_ok()
    );
    backend.shutdown().await;
}

// 1.T21, graceful proxied WebSocket rejection portion.
#[camber::test]
async fn graceful_shutdown_rejects_unacknowledged_proxy_upgrade() {
    pending_proxy_upgrade_shutdown_is_rejected(false).await;
}

// 1.T21, forced proxied WebSocket rejection portion.
#[camber::test]
async fn forced_shutdown_rejects_unacknowledged_proxy_upgrade() {
    pending_proxy_upgrade_shutdown_is_rejected(true).await;
}

struct ProxyUnwindScenario {
    backend: LifecycleWsBackend,
    backend_connections: Arc<AtomicUsize>,
    /// Held for the whole case, not just its setup.
    ///
    /// Dropping the controller closes its script and unregisters the address, so
    /// a controller left behind in the setup would retire the injected fault
    /// before the unwind it arms is observed. The case owns the fault until it
    /// has read every verdict that depends on it.
    controller: LifecycleController,
    handle: camber::http::ServerHandle,
    acknowledged: tokio::net::TcpStream,
    pending: tokio::net::TcpStream,
}

async fn start_proxy_unwind_scenario() -> ProxyUnwindScenario {
    let backend_connections = Arc::new(AtomicUsize::new(0));
    let backend = spawn_lifecycle_ws_backend(Arc::clone(&backend_connections)).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy unwind listener");
    let proxy_addr = listener.local_addr().expect("proxy unwind address");
    let controller = lifecycle(proxy_addr).expect("install proxy unwind controller");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend.addr));
    let mut acknowledged = connect_async_proxy_websocket(proxy_addr).await;
    assert_proxy_echo(&mut acknowledged).await;
    assert_eq!(backend_connections.load(Ordering::Acquire), 1);
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("pause after second proxy ticket submission");
    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("pause second proxy upgrade");
    let mut pending = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect pending proxy upgrade");
    send_async_proxy_upgrade(&mut pending).await;
    controller
        .wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .await
        .expect("second proxy ticket reaches the supervisor channel");
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("release submitted second proxy ticket");
    controller
        .wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .await
        .expect("second proxy upgrade reaches acknowledgement checkpoint");
    controller
        .inject_once(LifecycleFault::PanicSupervisorCore)
        .expect("inject proxy supervisor unwind");
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release proxy supervisor into unwind");
    ProxyUnwindScenario {
        backend,
        backend_connections,
        controller,
        handle,
        acknowledged,
        pending,
    }
}

async fn finish_proxy_unwind_scenario(mut scenario: ProxyUnwindScenario) {
    let mut owner = Box::pin(scenario.handle.into_future());
    assert_optional_close_then_eof(&mut scenario.acknowledged, "unwound proxy").await;
    let pending_response =
        read_async_http_head(&mut scenario.pending, "the unwound proxy-upgrade response").await;
    let pending_status = status_from_raw(&pending_response);
    assert_eq!(
        pending_status, 500,
        "pending proxy upgrade committed an unexpected response: {pending_response}"
    );
    assert!(
        pending_response
            .to_ascii_lowercase()
            .contains("connection: close"),
        "pending proxy unwind response omitted Connection: close: {pending_response}"
    );
    assert_eq!(
        scenario.backend_connections.load(Ordering::Acquire),
        1,
        "pending proxy upgrade reached its backend during unwind"
    );
    assert_transport_eof(&mut scenario.pending, "the unwound pending proxy transport").await;
    match lifecycle_event("proxy supervisor unwind drain", owner.as_mut()).await {
        Err(RuntimeError::TaskPanicked(message)) => assert!(!message.is_empty()),
        other => panic!("expected TaskPanicked after proxy upgrade drain, got {other:?}"),
    }
    scenario.backend.shutdown().await;
    drop(scenario.controller);
}

// 1.T21, proxied WebSocket supervisor-unwind portion.
#[camber::test]
async fn supervisor_unwind_joins_acknowledged_and_pending_proxy_upgrades() {
    let scenario = start_proxy_unwind_scenario().await;
    finish_proxy_unwind_scenario(scenario).await;
}
