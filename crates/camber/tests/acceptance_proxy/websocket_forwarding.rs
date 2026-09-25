#![cfg(feature = "ws")]

use crate::backend_negotiation::{
    BACKEND_REFUSED, BUFFERED, NegotiatingProxy, assert_mapped_before_upgrade, offer, route_of,
};
use crate::buffered_forwarding::FORWARDING_METADATA_LEAK;
use crate::common;
use crate::common::{
    Collapsed, EXPIRING_STOP, assert_classification, assert_graceful_close_then_eof,
    assert_http_ok, assert_optional_close_then_eof, assert_refusal_body_then_eof,
    assert_transport_eof, assert_within_one_deadline, attach_dispatch_probe, lifecycle_event,
    read_async_head_unbounded, read_async_http_head, read_async_ws_frame_or_eof,
    read_until_double_crlf, read_ws_text_frame, status_from_raw, unless_halted,
    write_async_ws_frame, write_ws_close_frame, write_ws_text_frame,
};
use crate::header_properties::CONNECTION_NAMED_LEAK;
use crate::retry_upstream::{RunnableDriver, settle_wakeups, within_watchdog};
use crate::ws_backend_script::{BackendEvent, BackendScript, ScriptedWsBackend};
use camber::RuntimeError;
use camber::http::mock::{
    ConnectionOwnerEdge, ScopedFaultedRegistration, ScopedUpgradeOwner, UpgradeOwnerEdge,
    connection_owner, faulted_registration, registration_selection, upgrade_owner,
};
use camber::http::{
    self, DisconnectCause, DisconnectSignal, Next, ProxyPolicy, RejectionKind, Request,
    RequestBudget, Response, Router, ServerHandleFuture, WsConn,
};
use camber::runtime;
use futures_util::{SinkExt, StreamExt};
use std::future::IntoFuture;
use std::io::Write;
use std::net::TcpStream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// The peer-supplied fields a proxied offer must never carry to its backend.
///
/// One from each rule the offer perimeter is built from: a field Camber
/// replaces on an ordinary request, a field only the whole `X-Forwarded-`
/// family covers, and a field the peer's own `Connection` named.
const STOPPED_AT_THE_WS_PROXY: [&str; 3] =
    ["x-forwarded-for", "x-forwarded-prefix", "authorization"];

/// What a backend saw of the fields that had to stop at the proxy.
///
/// Names only: every row that reads this report offers a credential, so what
/// crosses the bridge and lands in a failure message is the field's name.
fn ws_leak_report(leaked: &[&str]) -> Box<str> {
    match leaked.is_empty() {
        true => "none".into(),
        false => leaked.join(",").into_boxed_str(),
    }
}

/// Check both leak families without hiding either failure or printing credentials.
fn assert_ws_header_report(report: &str) {
    let (leaked, control) = report.split_once('|').expect("header report has a control");
    let failures = [
        (
            leaked.split(',').any(|name| name == "authorization"),
            CONNECTION_NAMED_LEAK,
        ),
        (
            leaked
                .split(',')
                .any(|name| name.starts_with("x-forwarded-")),
            FORWARDING_METADATA_LEAK,
        ),
        (
            control != "preserved",
            "unnamed end-to-end control did not reach backend",
        ),
    ]
    .into_iter()
    .filter_map(|(failed, diagnostic)| failed.then_some(diagnostic))
    .collect::<Box<[_]>>();
    assert!(failures.is_empty(), "{failures:?}; header names: {leaked}");
}

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

async fn assert_owned_proxy_close_contract(
    websocket: &mut tokio::net::TcpStream,
    owner: Pin<&mut ServerHandleFuture>,
) {
    let (opcode, _) = read_async_ws_frame_or_eof(websocket, "the permit-holding proxy close")
        .await
        .expect("graceful proxy shutdown sends a close frame");
    assert_eq!(opcode, 0x8, "expected graceful proxied close frame");
    assert!(
        tokio::time::timeout(UNRESOLVED_WINDOW, owner)
            .await
            .is_err(),
        "owner completed while the proxy bridge still owned its transport"
    );
    write_async_ws_frame(websocket, 0x8, &[], "the permit-holding proxy close reply").await;
    assert_transport_eof(websocket, "the permit-holding proxy transport").await;
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

/// What one lifecycle backend saw across every connection it accepted.
///
/// A proxied upgrade negotiates its backend before the `101` is offered for
/// registration, so a refused handoff still reaches the backend. What it must
/// never do is bridge it: the tally tells a negotiated backend the refusal
/// released apart from one a bridge carried frames over.
#[derive(Clone)]
struct BackendTally {
    /// Handshakes the backend completed.
    negotiated: tokio::sync::watch::Sender<usize>,
    /// Negotiated connections the proxy released before any frame crossed.
    released_unbridged: tokio::sync::watch::Sender<usize>,
}

impl BackendTally {
    fn new() -> Self {
        Self {
            negotiated: tokio::sync::watch::Sender::new(0),
            released_unbridged: tokio::sync::watch::Sender::new(0),
        }
    }

    fn negotiated(&self) -> usize {
        *self.negotiated.borrow()
    }

    async fn require_negotiated(&self, count: usize) {
        let mut negotiated = self.negotiated.subscribe();
        lifecycle_event(
            "refused handoff skipped backend negotiation",
            negotiated.wait_for(|seen| *seen >= count),
        )
        .await
        .expect("the lifecycle backend tally outlives its readers");
        assert_eq!(
            self.negotiated(),
            count,
            "refused handoff skipped backend negotiation"
        );
    }

    /// Wait until `count` negotiated connections were released unbridged.
    async fn await_released_unbridged(&self, count: usize, context: &str) {
        let mut released = self.released_unbridged.subscribe();
        lifecycle_event(context, released.wait_for(|seen| *seen >= count))
            .await
            .expect("the lifecycle backend tally outlives its readers");
    }
}

/// An echo backend that serves every connection it accepts until shut down.
async fn spawn_lifecycle_ws_backend(tally: BackendTally) -> LifecycleWsBackend {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind lifecycle WebSocket backend");
    let addr = listener
        .local_addr()
        .expect("lifecycle WebSocket backend address");
    let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (halt, halted) = tokio::sync::watch::channel(false);
        let mut peers = tokio::task::JoinSet::new();
        loop {
            let accepted = tokio::select! {
                biased;
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => accepted.expect("accept lifecycle backend peer"),
            };
            peers.spawn(serve_lifecycle_peer(
                accepted.0,
                tally.clone(),
                halted.clone(),
            ));
        }
        drop(listener);
        halt.send_replace(true);
        while let Some(joined) = peers.join_next().await {
            joined.expect("a lifecycle backend peer joined");
        }
    });
    LifecycleWsBackend {
        addr,
        shutdown,
        task,
    }
}

/// Echo one negotiated connection until it ends, and tally how it ended.
async fn serve_lifecycle_peer(
    stream: tokio::net::TcpStream,
    tally: BackendTally,
    mut halt: tokio::sync::watch::Receiver<bool>,
) {
    let mut websocket =
        match unless_halted(&mut halt, tokio_tungstenite::accept_async(stream)).await {
            Some(handshake) => handshake.expect("accept lifecycle backend WebSocket"),
            None => return,
        };
    tally.negotiated.send_modify(|negotiated| *negotiated += 1);

    let mut carried = false;
    loop {
        let message = match unless_halted(&mut halt, websocket.next()).await {
            Some(Some(Ok(message))) => message,
            Some(Some(Err(_)) | None) => break,
            None => return,
        };
        carried = true;
        let closes = message.is_close();
        let sent = match unless_halted(&mut halt, websocket.send(message)).await {
            Some(sent) => sent,
            None => return,
        };
        if sent.is_err() || closes {
            break;
        }
    }
    match carried {
        true => {}
        false => tally
            .released_unbridged
            .send_modify(|released| *released += 1),
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
    let backend = spawn_lifecycle_ws_backend(BackendTally::new()).await;
    let backend_addr = backend.addr;
    let mut proxy = lifecycle_proxy_router(backend_addr);
    let mut captured = capture_proxy_signals(&mut proxy);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy handoff listener");
    let proxy_addr = listener.local_addr().expect("proxy handoff address");
    let controller = upgrade_owner(proxy_addr).expect("install proxy handoff controller");
    controller
        .pause_once(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .expect("pause proxy handoff acknowledgement");
    let handle = camber::http::serve_background(listener, proxy)
        .expect("owned server requires a Tokio runtime");

    let mut peer = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect proxy handoff peer");
    send_async_proxy_upgrade(&mut peer).await;
    lifecycle_event(
        "proxy handoff acknowledgement checkpoint",
        controller.wait_until_paused(UpgradeOwnerEdge::BeforeTransferAcknowledge),
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
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
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
    backend.ws("/echo", common::echo_ws);
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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: sends 3 messages then waits
            let mut backend = Router::new();
            backend.ws("/chat", |_: &Request, mut conn: WsConn| {
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
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: serves both HTTP and WebSocket
            let mut backend = echo_ws_backend();
            backend.get("/hello", |_: &Request| async {
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
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            // Backend: WebSocket echo server
            let mut backend = Router::new();
            backend.ws("/echo", |_: &Request, conn: WsConn| {
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
fn ws_proxy_strips_spoofed_forwarded_headers() {
    let report = common::test_runtime()
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut backend = Router::new();
            backend.ws("/echo", |req: &Request, conn: WsConn| {
                let leaked = req
                    .headers()
                    .filter_map(|(name, _)| {
                        STOPPED_AT_THE_WS_PROXY
                            .into_iter()
                            .find(|stopped| name.eq_ignore_ascii_case(stopped))
                    })
                    .collect::<Box<[&str]>>();
                let control = req
                    .headers()
                    .find(|(name, _)| name.eq_ignore_ascii_case("x-end-to-end"))
                    .map_or("missing", |(_, value)| value);
                conn.send(&format!("{}|{control}", ws_leak_report(&leaked)))?;
                Ok(())
            });
            let backend_addr = common::spawn_server(backend);

            let proxy_addr = common::spawn_server(lifecycle_proxy_router(backend_addr));

            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            // Three peer-supplied fields a proxied offer must not carry: one
            // Camber replaces on an ordinary request, one that only the
            // `X-Forwarded-` family covers, and one the peer itself marked
            // hop-by-hop through a repeated `Connection` value.
            let upgrade_req = common::ws_upgrade_request_with(
                "/ws/echo",
                &[
                    ("X-Forwarded-For", "6.6.6.6"),
                    ("X-Forwarded-Prefix", "/spoofed"),
                    ("Connection", "Authorization"),
                    ("Authorization", "Bearer connection-named-credential"),
                    ("X-End-To-End", "preserved"),
                ],
            );
            stream.write_all(upgrade_req.as_bytes()).unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(status_from_raw(&resp), 101, "response: {resp}");

            let msg = read_ws_text_frame(&mut stream);
            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
            msg
        })
        .unwrap();
    assert_ws_header_report(&report);
}

#[test]
fn websocket_proxy_rejects_invalid_backend_scheme() {
    common::test_runtime()
        .header_timeout(Duration::from_millis(200))
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
fn websocket_proxy_stream_upgrade_excludes_body_policy_and_refuses_a_declared_payload() {
    common::test_runtime()
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let backend_addr = common::spawn_server(echo_ws_backend());

            let port = common::reserve_request_body_owner();
            let asked = Arc::new(AtomicUsize::new(0));
            let mut proxy = Router::new().max_request_body(10);
            proxy.proxy_stream("/ws", &format!("http://{backend_addr}"));
            let proxy = proxy.body_admission(common::refusing_body_admission(&asked));
            let server = port.serve(proxy);
            let proxy_addr = server.addr();

            // A proxied handshake under a 10-byte route limit still upgrades:
            // it is bodyless, not a streaming upload, so the limit has nothing
            // to refuse and the body policy is never asked.
            let mut stream = TcpStream::connect(proxy_addr).unwrap();
            stream
                .write_all(common::ws_upgrade_request("/ws/echo").as_bytes())
                .unwrap();

            let resp = read_until_double_crlf(&mut stream);
            assert_eq!(status_from_raw(&resp), 101, "response: {resp}");

            write_ws_text_frame(&mut stream, "hello");
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(&*msg, "hello");

            write_ws_close_frame(&mut stream);

            // A handshake declaring a payload is refused at the head, the same
            // as a direct one: the `101` would hand the transport to the bridge
            // with those bytes unframed, and the reply RFC 6455 §4.1 makes a
            // conforming client fail is the only one Hyper could then write.
            // One handshake owner validates for both bridges, so the proxied
            // path answers exactly as the direct one does.
            let mut declared = TcpStream::connect(proxy_addr).unwrap();
            declared
                .write_all(
                    common::ws_upgrade_request_with("/ws/echo", &[("Content-Length", "99999")])
                        .as_bytes(),
                )
                .unwrap();
            let declared_resp = read_until_double_crlf(&mut declared);
            assert_eq!(
                status_from_raw(&declared_resp),
                400,
                "declared-payload handshake: {declared_resp}"
            );

            assert_eq!(
                asked.load(Ordering::SeqCst),
                0,
                "a proxied WebSocket upgrade is bodyless, not a streaming upload"
            );
            let body = server.controller().observed();
            assert_eq!(body.frames_polled, 0);
            assert_eq!(body.peak_retained_bytes, 0);
            assert_eq!(body.permit_owners_dropped, 0);
            runtime::request_shutdown();
        })
        .unwrap();
}

// 1.T9, proxied WebSocket portion.
#[test]
fn proxied_websocket_bridge_holds_permit_and_finishes_before_owned_completion() {
    runtime::builder()
        .connection_limit(1)
        .header_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            runtime::block_on(async {
                let tally = BackendTally::new();
                let backend = spawn_lifecycle_ws_backend(tally.clone()).await;
                let mut proxy = lifecycle_proxy_router(backend.addr);
                let mut dispatched = attach_dispatch_probe(&mut proxy);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind owned proxy listener");
                let proxy_addr = listener.local_addr().expect("owned proxy listener address");
                let controller = connection_owner(proxy_addr).expect("install proxy controller");
                let handle = camber::http::serve_background(listener, proxy)
                    .expect("owned server requires a Tokio runtime");
                let mut websocket = connect_async_proxy_websocket(proxy_addr).await;
                assert_proxy_echo(&mut websocket).await;
                assert_eq!(tally.negotiated(), 1);
                controller
                    .pause_once(ConnectionOwnerEdge::PermitWaitPending)
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
                    .wait_until_paused(ConnectionOwnerEdge::PermitWaitPending)
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
                    .release(ConnectionOwnerEdge::PermitWaitPending)
                    .expect("release pending proxy permit wait into shutdown");
                let mut owner = Box::pin(handle.into_future());
                assert_owned_proxy_close_contract(&mut websocket, owner.as_mut()).await;
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
    let backend = spawn_lifecycle_ws_backend(BackendTally::new()).await;
    let backend_addr = backend.addr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind graceful proxy listener");
    let proxy_addr = listener.local_addr().expect("graceful proxy address");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend_addr))
        .expect("owned server requires a Tokio runtime");
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
    let backend = spawn_lifecycle_ws_backend(BackendTally::new()).await;
    let backend_addr = backend.addr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind forced proxy listener");
    let proxy_addr = listener.local_addr().expect("forced proxy address");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend_addr))
        .expect("owned server requires a Tokio runtime");
    let mut websocket = connect_async_proxy_websocket(proxy_addr).await;
    assert_proxy_echo(&mut websocket).await;

    handle.cancel();
    let mut owner = Box::pin(handle.into_future());
    assert_optional_close_then_eof(&mut websocket, "forced proxy").await;
    assert_cancelled(lifecycle_event("forced proxy bridge join", owner.as_mut()).await);
    backend.shutdown().await;
}

/// A backend that completes the WebSocket handshake and then answers nothing.
///
/// Not an option on [`spawn_lifecycle_ws_backend`]: that backend is a live peer
/// whose replies every other proxy row depends on, and this one exists only to
/// never reply — including to the close a graceful stop sends it.
async fn spawn_silent_ws_backend() -> LifecycleWsBackend {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind silent WebSocket backend");
    let addr = listener
        .local_addr()
        .expect("silent WebSocket backend address");
    let (shutdown, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let accepted = tokio::select! {
            biased;
            _ = &mut shutdown_rx => return,
            accepted = listener.accept() => accepted.expect("accept silent backend peer"),
        };
        let websocket = tokio_tungstenite::accept_async(accepted.0)
            .await
            .expect("accept silent backend WebSocket");
        // Held and never polled: the proxy's close arrives on this socket and
        // nothing here ever reads it, let alone answers.
        let _ = shutdown_rx.await;
        drop(websocket);
    });
    LifecycleWsBackend {
        addr,
        shutdown,
        task,
    }
}

// 2.T8, the bound a proxied bridge is still ended under.
//
// A proxied bridge is registered, so a server that aborts leaves it to settle
// the abort itself. This row is the case where it cannot: the graceful stop
// sends a close to each side, and the backend never answers. The bridge has to
// hear the abort where it waits, so this server ends within the one deadline
// its stop was given rather than the two a second armed deadline would cost.
#[test]
fn forced_proxy_abort_bounds_a_backend_that_never_answers_its_close() {
    runtime::builder()
        .shutdown_timeout(EXPIRING_STOP)
        .run(|| {
            runtime::block_on(async {
                let backend = spawn_silent_ws_backend().await;
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind silent-backend proxy listener");
                let proxy_addr = listener.local_addr().expect("silent-backend proxy address");
                let handle =
                    camber::http::serve_background(listener, lifecycle_proxy_router(backend.addr))
                        .expect("owned server requires a Tokio runtime");
                let mut websocket = connect_async_proxy_websocket(proxy_addr).await;

                let requested = tokio::time::Instant::now();
                handle.shutdown();
                let mut owner = Box::pin(handle.into_future());
                let (opcode, _) =
                    read_async_ws_frame_or_eof(&mut websocket, "the silent-backend proxy close")
                        .await
                        .expect("graceful proxy shutdown sends a close frame");
                assert_eq!(opcode, 0x8, "expected graceful proxied close frame");
                // Neither peer answers from here: the client holds its socket
                // open and the backend has never read a byte.
                let completed = lifecycle_event("silent-backend proxy join", owner.as_mut()).await;
                assert!(
                    matches!(completed, Err(RuntimeError::Timeout)),
                    "a proxy stop neither peer answered completed as {completed:?}"
                );
                assert_within_one_deadline(requested, "a proxy stop neither peer answered");
                backend.shutdown().await;
            });
        })
        .unwrap();
}

/// The deadline the route-deadline row's proxy freezes for its backend.
const DIAL_DEADLINE: Duration = Duration::from_millis(300);

/// The inbound request total the inbound-total row's proxy serves under.
///
/// Shorter than [`DIAL_DEADLINE`], so it is the one of the two that ends the
/// same held handshake.
const INBOUND_TOTAL: Duration = Duration::from_millis(100);

/// A bound no row in this case reaches on the paused clock.
const UNREACHED: Duration = Duration::from_secs(3600);

/// One backend offer held open under a deadline, and what refusing it earns.
struct HeldOfferRow {
    label: &'static str,
    router: fn(&str) -> Router,
    /// How far the paused clock is stepped once the backend holds the offer.
    step: Duration,
    /// The whole classification the refusal keeps: category, status, and the
    /// client-safe message its producer fixed.
    refused: Collapsed<'static>,
}

/// The route-deadline row's proxy: its own upstream deadline, no shorter total.
fn route_deadline_router(backend: &str) -> Router {
    let mut proxy = Router::new();
    proxy.proxy_with_policy(
        BUFFERED,
        backend,
        ProxyPolicy::default()
            .request_timeout(DIAL_DEADLINE)
            .expect("a short upstream deadline"),
    );
    proxy
}

/// The inbound-total row's proxy: a request total shorter than any upstream
/// deadline it serves under.
fn inbound_total_router(backend: &str) -> Router {
    let mut proxy = Router::new().request_budget(
        RequestBudget::bounded(UNREACHED, INBOUND_TOTAL).expect("a short inbound request total"),
    );
    proxy.proxy_with_policy(
        BUFFERED,
        backend,
        ProxyPolicy::default()
            .request_timeout(UNREACHED)
            .expect("an upstream deadline the row never reaches"),
    );
    proxy
}

const HELD_OFFER_ROWS: [HeldOfferRow; 2] = [
    HeldOfferRow {
        label: "the inbound request total ends the held handshake",
        router: inbound_total_router,
        step: INBOUND_TOTAL,
        refused: Collapsed {
            kind: RejectionKind::RequestTimeout,
            status: 408,
            message: "request timed out",
        },
    },
    HeldOfferRow {
        label: "the route deadline ends the held handshake",
        router: route_deadline_router,
        step: DIAL_DEADLINE,
        refused: Collapsed {
            kind: RejectionKind::Proxy,
            status: 504,
            message: "gateway timeout",
        },
    },
];

/// 6.T4
///
/// The deadline a proxied upgrade runs under ends its backend handshake before
/// the peer is told anything. The backend acknowledges the whole offer and
/// then holds it; stepping the paused clock onto the deadline refuses the peer
/// with a mapped head, never a `101`, and releases the backend. The inbound
/// request total cancels the same handshake when it is the shorter bound.
///
/// A backend behind `https` whose certificate nothing trusts is refused the
/// same way, before any `101`, and it only ever sees a TLS hello: a TLS
/// failure is never retried as plaintext.
#[tokio::test(start_paused = true)]
async fn proxied_upgrade_bounds_its_backend_dial_by_the_route_deadline() {
    let context = camber::runtime_test_support::install_runtime_context();
    for row in &HELD_OFFER_ROWS {
        assert_held_offer_refused(row).await;
    }
    assert_untrusted_backend_refused().await;
    drop(context);
}

/// One held-offer row, driven onto its deadline on the paused clock.
async fn assert_held_offer_refused(row: &HeldOfferRow) {
    let label = row.label;
    let driver = RunnableDriver::start();
    let mut backend = ScriptedWsBackend::bind(Box::new([BackendScript::Hold])).await;
    let proxy = NegotiatingProxy::serve((row.router)(&backend.http_url())).await;

    let mut peer = offer(proxy.addr(), BUFFERED, &[]).await;
    backend.offered(0, label).await;
    tokio::time::advance(row.step).await;
    settle_wakeups().await;
    let head = read_refused_head(&mut peer, "the held downstream head", label).await;
    let seen = assert_mapped_before_upgrade(&head, &proxy, &route_of(BUFFERED), label);
    assert_classification(&seen, &row.refused, label);
    backend.expect(BackendEvent::Released(0), label).await;

    settle_refused_row(peer, driver, proxy, backend, label).await;
}

/// Read the head a refused paused-clock row's peer was answered with.
///
/// Bounded by the watchdog's wall clock: a tokio timeout would fire on the
/// paused clock the moment every task parks.
async fn read_refused_head(peer: &mut tokio::net::TcpStream, what: &str, label: &str) -> Box<str> {
    within_watchdog(read_async_head_unbounded(peer, what))
        .await
        .unwrap_or_else(|| panic!("{label}: the peer was never answered"))
}

/// Tear one refused paused-clock row down: the peer, the driver, the proxy's
/// clean join, and a backend that saw nothing no row consumed.
async fn settle_refused_row(
    peer: tokio::net::TcpStream,
    driver: RunnableDriver,
    proxy: NegotiatingProxy,
    backend: ScriptedWsBackend,
    label: &str,
) {
    drop(peer);
    driver.stop().await;
    let stopped = within_watchdog(proxy.stop())
        .await
        .unwrap_or_else(|| panic!("{label}: the proxy never joined"));
    assert!(stopped.is_ok(), "{label}: the proxy stopped as {stopped:?}");
    backend.finish(label).await;
}

/// The untrusted-TLS row: refused before `101`, with no plaintext fallback.
async fn assert_untrusted_backend_refused() {
    let label = "an untrusted TLS backend";
    let (cert_pem, key_pem) = common::generate_cert_with_san("127.0.0.1");
    let config = common::build_server_config(&cert_pem, &key_pem);
    let driver = RunnableDriver::start();
    let mut backend =
        ScriptedWsBackend::bind(Box::new([BackendScript::UntrustedTls(config)])).await;
    let mut router = Router::new();
    router.proxy(BUFFERED, &backend.https_url());
    let proxy = NegotiatingProxy::serve(router).await;

    let mut peer = offer(proxy.addr(), BUFFERED, &[]).await;
    let head = read_refused_head(&mut peer, "the untrusted downstream head", label).await;
    let seen = assert_mapped_before_upgrade(&head, &proxy, &route_of(BUFFERED), label);
    assert_classification(&seen, &BACKEND_REFUSED, label);
    backend.expect(BackendEvent::TlsRefused(0), label).await;

    settle_refused_row(peer, driver, proxy, backend, label).await;
}

async fn pending_proxy_upgrade_shutdown_is_rejected(forced: bool) {
    let tally = BackendTally::new();
    let backend = spawn_lifecycle_ws_backend(tally.clone()).await;
    let backend_addr = backend.addr;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind pending proxy-upgrade listener");
    let proxy_addr = listener.local_addr().expect("pending proxy address");
    let controller = registration_selection(proxy_addr).expect("install pending proxy controller");
    controller
        .upgrades
        .pause_once(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("pause after pending proxy ticket submission");
    controller
        .upgrades
        .pause_once(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .expect("pause pending proxy upgrade");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend_addr))
        .expect("owned server requires a Tokio runtime");
    let mut pending = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect pending proxied WebSocket peer");
    send_async_proxy_upgrade(&mut pending).await;
    controller
        .upgrades
        .wait_until_paused(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .await
        .expect("pending proxy ticket reaches the supervisor channel");
    controller
        .upgrades
        .release(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("release submitted pending proxy ticket");
    controller
        .upgrades
        .wait_until_paused(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .await
        .expect("proxy upgrade reaches acknowledgement checkpoint");
    match forced {
        true => handle.cancel(),
        false => runtime::request_shutdown(),
    }
    // Released only once this server's own stop state has committed. A
    // cancellation commits before the command returns; a runtime shutdown
    // commits when the supervisor takes the signal, and the answer this
    // connection gives has to be on the far side of whichever it was.
    common::await_committed_stop(&controller, "the pending proxy upgrade").await;
    controller
        .upgrades
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .expect("release pending proxy upgrade into shutdown");
    let mut owner = Box::pin(handle.into_future());
    let response = read_async_http_head(&mut pending, "the rejected proxy-upgrade response").await;
    assert_proxy_shutdown_head(&response);
    // The backend is negotiated before the upgrade is offered for
    // registration, so the refusal reaches it — and must release it unbridged.
    tally.require_negotiated(1).await;
    tally
        .await_released_unbridged(1, "the rejected proxy upgrade's backend release")
        .await;
    assert_refusal_body_then_eof(
        &mut pending,
        "service unavailable",
        "the rejected proxy-upgrade transport",
    )
    .await;
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
    let tally = BackendTally::new();
    let backend = spawn_lifecycle_ws_backend(tally.clone()).await;
    let backend_addr = backend.addr;
    let mut proxy = lifecycle_proxy_router(backend_addr);
    proxy.get("/ok", |_: &Request| async { Response::text(200, "ok") });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy cancellation listener");
    let proxy_addr = listener.local_addr().expect("proxy cancellation address");
    let controller = upgrade_owner(proxy_addr).expect("install proxy cancellation controller");
    arm_cancellable_proxy_upgrade(&controller);
    let handle = camber::http::serve_background(listener, proxy)
        .expect("owned server requires a Tokio runtime");
    let mut pending = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect cancellable proxy peer");
    send_async_proxy_upgrade(&mut pending).await;
    hold_proxy_upgrade_at_transfer_edge(&controller).await;
    drop(pending);
    release_proxy_upgrade_after_peer_close(&controller).await;

    assert_http_ok(
        proxy_addr,
        "/ok",
        "the proxy listener after registrar cancellation",
    )
    .await;
    tally.require_negotiated(1).await;
    tally
        .await_released_unbridged(1, "the cancelled proxy upgrade's backend release")
        .await;
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
    tally: BackendTally,
    /// Held for the whole case, not just its setup.
    ///
    /// Dropping the controller closes its script and unregisters the address, so
    /// a controller left behind in the setup would retire the injected fault
    /// before the unwind it arms is observed. The case owns the fault until it
    /// has read every verdict that depends on it.
    controller: ScopedFaultedRegistration,
    handle: camber::http::ServerHandle,
    acknowledged: tokio::net::TcpStream,
    pending: tokio::net::TcpStream,
}

async fn start_proxy_unwind_scenario() -> ProxyUnwindScenario {
    let tally = BackendTally::new();
    let backend = spawn_lifecycle_ws_backend(tally.clone()).await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind proxy unwind listener");
    let proxy_addr = listener.local_addr().expect("proxy unwind address");
    let controller = faulted_registration(proxy_addr).expect("install proxy unwind controller");
    let handle = camber::http::serve_background(listener, lifecycle_proxy_router(backend.addr))
        .expect("owned server requires a Tokio runtime");
    let mut acknowledged = connect_async_proxy_websocket(proxy_addr).await;
    assert_proxy_echo(&mut acknowledged).await;
    assert_eq!(tally.negotiated(), 1);
    controller
        .upgrades
        .pause_once(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("pause after second proxy ticket submission");
    controller
        .upgrades
        .pause_once(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .expect("pause second proxy upgrade");
    let mut pending = tokio::net::TcpStream::connect(proxy_addr)
        .await
        .expect("connect pending proxy upgrade");
    send_async_proxy_upgrade(&mut pending).await;
    controller
        .upgrades
        .wait_until_paused(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .await
        .expect("second proxy ticket reaches the supervisor channel");
    controller
        .upgrades
        .release(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("release submitted second proxy ticket");
    controller
        .upgrades
        .wait_until_paused(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .await
        .expect("second proxy upgrade reaches acknowledgement checkpoint");
    common::unwind_the_supervisor(&controller, "the unwinding proxy supervisor").await;
    // Released only once the unwind has committed its forced phase, so the
    // connection's answer reads a server that has already stopped admitting
    // rather than racing the panic it is meant to follow.
    controller
        .upgrades
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .expect("release the held proxy transfer edge");
    ProxyUnwindScenario {
        backend,
        tally,
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
    // A refusal rather than an internal failure: the connection holding the
    // offer reads the forced phase the unwinding supervisor committed, so it
    // knows the server stopped admitting rather than only that an owner went
    // away.
    assert_proxy_shutdown_head(&pending_response);
    // The acknowledged bridge and the pending offer both negotiated their
    // backend; only the pending one is released without a frame crossing.
    scenario.tally.require_negotiated(2).await;
    scenario
        .tally
        .await_released_unbridged(1, "the unwound pending upgrade's backend release")
        .await;
    assert_refusal_body_then_eof(
        &mut scenario.pending,
        "service unavailable",
        "the unwound pending proxy transport",
    )
    .await;
    match lifecycle_event("proxy supervisor unwind drain", owner.as_mut()).await {
        Err(RuntimeError::TaskPanicked(message)) => assert!(!message.is_empty()),
        other => panic!("expected TaskPanicked after proxy upgrade drain, got {other:?}"),
    }
    scenario.backend.shutdown().await;
    drop(scenario.controller);
}

fn assert_proxy_shutdown_head(response: &str) {
    assert_eq!(
        status_from_raw(response),
        503,
        "shutdown committed an unexpected proxy upgrade response: {response}"
    );
    assert!(
        response.to_ascii_lowercase().contains("connection: close"),
        "proxy upgrade rejection omitted Connection: close: {response}"
    );
}

// 1.T21, proxied WebSocket supervisor-unwind portion.
#[camber::test]
async fn supervisor_unwind_joins_acknowledged_and_pending_proxy_upgrades() {
    let scenario = start_proxy_unwind_scenario().await;
    finish_proxy_unwind_scenario(scenario).await;
}

/// Arm the three edges a cancelled proxy upgrade is walked through.
///
/// Armed together because the walk needs all three before the peer connects: an
/// edge armed later would be armed after production had already run past it.
fn arm_cancellable_proxy_upgrade(controller: &ScopedUpgradeOwner) {
    controller
        .pause_once(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("pause after the cancellable proxy offer is submitted");
    controller
        .pause_once(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .expect("pause the cancellable proxy upgrade at its transfer edge");
    controller
        .pause_once(UpgradeOwnerEdge::PeerClosed)
        .expect("pause after proxy peer closure is observed");
}

/// Take the offered proxy upgrade as far as its connection's transfer edge.
async fn hold_proxy_upgrade_at_transfer_edge(controller: &ScopedUpgradeOwner) {
    controller
        .wait_until_paused(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .await
        .expect("the cancellable proxy offer reaches its connection");
    controller
        .release(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("release the submitted cancellable proxy offer");
    controller
        .wait_until_paused(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .await
        .expect("the cancellable proxy upgrade reaches its transfer edge");
}

/// Let the held transfer answer, but only after the peer close is observed.
///
/// The order is the whole claim: the connection decides against a peer it
/// already knows has gone, so the cancellation is its own rather than a race.
async fn release_proxy_upgrade_after_peer_close(controller: &ScopedUpgradeOwner) {
    lifecycle_event(
        "owned reader observation of proxy peer closure",
        controller.wait_until_paused(UpgradeOwnerEdge::PeerClosed),
    )
    .await
    .expect("owned reader observes proxy peer closure");
    controller
        .release(UpgradeOwnerEdge::PeerClosed)
        .expect("release observed proxy peer closure");
    controller
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .expect("release the cancelled proxy transfer");
}
