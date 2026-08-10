#![cfg(feature = "ws")]

use crate::common;
#[path = "../support/deterministic.rs"]
mod deterministic;

use crate::handshake::{Header, LOCAL_HOST, accepted, accepted_plus, handshake_request};

use crate::common::{
    ASYNC_EVENT_TIMEOUT, assert_graceful_close_then_eof, assert_http_ok,
    assert_optional_close_then_eof, assert_refusal_body_then_eof, assert_transport_eof,
    attach_dispatch_probe, lifecycle_event, read_async_http_head, read_ws_binary_frame,
    read_ws_text_frame, status_from_raw, write_ws_binary_frame, write_ws_close_frame,
    write_ws_text_frame,
};
use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, LifecycleFault, lifecycle};
use camber::http::{Request, Response, Router, WsConn, WsMessage};
use camber::runtime;
use futures_util::FutureExt;
use std::future::IntoFuture;
use std::io::Write;
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const FLAG_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// The key every accepted handshake here offers.
///
/// The workspace's own, not a copy of it: [`VALID_WEBSOCKET_ACCEPT`] below is
/// derived from this exact value, so a second spelling would leave the accept
/// value proving nothing about the key Camber was actually sent.
const VALID_WEBSOCKET_KEY: &str = common::WS_KEY;
const VALID_WEBSOCKET_ACCEPT: &str = "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=";

struct InvalidHandshakeCase {
    label: &'static str,
    connection: Option<&'static str>,
    version: Option<&'static str>,
    key: &'static str,
    expected_status: u16,
    /// The version a rejection must advertise, for the one case that owes the
    /// client one.
    ///
    /// Carried on the case rather than re-derived from `label`, which is display
    /// text: a reworded label would silently retire the `426` claim instead of
    /// failing.
    expected_version_header: Option<&'static str>,
}

const INVALID_HANDSHAKE_CASES: [InvalidHandshakeCase; 5] = [
    InvalidHandshakeCase {
        label: "missing version",
        connection: Some("Upgrade"),
        version: None,
        key: VALID_WEBSOCKET_KEY,
        expected_status: 400,
        expected_version_header: None,
    },
    InvalidHandshakeCase {
        label: "wrong version",
        connection: Some("Upgrade"),
        version: Some("12"),
        key: VALID_WEBSOCKET_KEY,
        expected_status: 426,
        expected_version_header: Some("13"),
    },
    InvalidHandshakeCase {
        label: "malformed Base64 key",
        connection: Some("Upgrade"),
        version: Some("13"),
        key: "not@@base64",
        expected_status: 400,
        expected_version_header: None,
    },
    InvalidHandshakeCase {
        label: "decoded key is not 16 bytes",
        connection: Some("Upgrade"),
        version: Some("13"),
        key: "dG9vIHNob3J0",
        expected_status: 400,
        expected_version_header: None,
    },
    InvalidHandshakeCase {
        label: "missing Connection Upgrade",
        connection: None,
        version: Some("13"),
        key: VALID_WEBSOCKET_KEY,
        expected_status: 400,
        expected_version_header: None,
    },
];

struct CallbackDropProbe(Arc<AtomicBool>);

impl Drop for CallbackDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

async fn async_ws_request(stream: &mut tokio::net::TcpStream, path: &str) {
    let request = common::ws_upgrade_request(path);
    tokio::io::AsyncWriteExt::write_all(stream, request.as_bytes())
        .await
        .expect("write WebSocket upgrade request");
}

async fn connect_async_websocket(addr: std::net::SocketAddr, path: &str) -> tokio::net::TcpStream {
    let mut stream = lifecycle_event(
        "WebSocket TCP connection",
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .expect("connect WebSocket peer");
    async_ws_request(&mut stream, path).await;
    let response = read_async_http_head(&mut stream, "the direct WebSocket handshake").await;
    assert_eq!(
        status_from_raw(&response),
        101,
        "expected WebSocket upgrade, got: {response}"
    );
    stream
}

fn lifecycle_websocket_router() -> Router {
    let mut router = Router::new();
    router.ws("/ws", |_request: &Request, mut connection: WsConn| {
        while connection.recv().is_some() {}
        Ok(())
    });
    router
}

fn assert_cancelled(result: Result<(), RuntimeError>) {
    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "expected Cancelled, got {result:?}"
    );
}

fn arm_unacknowledged_upgrade(controller: &LifecycleController) {
    controller
        .pause_once(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("pause after upgrade-ticket submission");
    controller
        .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("pause before upgrade acknowledgement");
}

/// Await a flag a callback sets while unwinding its own stack.
///
/// A callback that reports completion over a channel is still holding its
/// locals when the receiving task wakes: the send happens inside the callback
/// body, the drops happen after it returns. Reading the flag once races that
/// window and only loses under contention, so poll it to the lifecycle
/// deadline instead.
async fn wait_for_dropped_flag(flag: &AtomicBool, context: &str) {
    let deadline = Instant::now() + ASYNC_EVENT_TIMEOUT;
    while !flag.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "{context}");
        tokio::time::sleep(FLAG_POLL_INTERVAL).await;
    }
}

async fn wait_for_unacknowledged_upgrade(controller: &LifecycleController) {
    lifecycle_event(
        "upgrade ticket reaches the production registration channel",
        controller.wait_until_paused(LifecycleCheckpoint::AfterUpgradeTicketSubmitted),
    )
    .await
    .expect("upgrade ticket reaches the production registration channel");
    controller
        .release(LifecycleCheckpoint::AfterUpgradeTicketSubmitted)
        .expect("release submitted upgrade ticket");
    lifecycle_event(
        "submitted upgrade reaches acknowledgement checkpoint",
        controller.wait_until_paused(LifecycleCheckpoint::BeforeUpgradeAcknowledge),
    )
    .await
    .expect("submitted upgrade reaches acknowledgement checkpoint");
}

/// The accepted head with one case's omissions and replacements applied.
///
/// Stated as a change to the accepted list rather than as a list of its own:
/// what each case claims is that one header is missing, or carries a value
/// Camber must refuse, and that everything else is what a client sends. `None`
/// drops the header the case names; a stated value replaces it.
fn case_handshake(case: &InvalidHandshakeCase) -> Box<[Header<'static>]> {
    accepted(LOCAL_HOST)
        .into_iter()
        .filter_map(|(name, value)| match name {
            "Connection" => case.connection.map(|refused| (name, refused)),
            "Sec-WebSocket-Version" => case.version.map(|refused| (name, refused)),
            "Sec-WebSocket-Key" => Some((name, case.key)),
            _ => Some((name, value)),
        })
        .collect()
}

/// Send `request` and read the response it answers with.
///
/// The connection and its bounds come from `common::connect`, and the head is
/// parsed by the shared response reader: a handshake case owns what it sends
/// and what the head must say, not a second statement of how a socket is
/// opened or how a status line is split.
///
/// Read under the bounded form, which arms one deadline over the whole reply.
/// The socket's own timeout bounds a single syscall, so a peer dribbling one
/// byte per read would never be cut off by it.
fn perform_raw_ws_handshake(
    addr: std::net::SocketAddr,
    request: &str,
) -> (TcpStream, common::HttpResponse) {
    let mut stream = common::connect(addr).expect("connect raw WebSocket client");
    stream
        .write_all(request.as_bytes())
        .expect("write raw WebSocket handshake");
    let response = common::read_http_response_bounded(&mut stream)
        .expect("read raw WebSocket handshake reply");
    (stream, response)
}

fn assert_websocket_switch(head: &common::HttpResponse, context: &str) {
    assert_eq!(head.status, 101, "{context}: unexpected status: {head:?}");
    let upgrade = head.header_values("upgrade");
    assert_eq!(upgrade.len(), 1, "{context}: Upgrade header: {head:?}");
    assert!(
        upgrade[0].eq_ignore_ascii_case("websocket"),
        "{context}: invalid Upgrade header: {head:?}"
    );
    let connection = head.header_values("connection");
    assert_eq!(
        connection.len(),
        1,
        "{context}: Connection header: {head:?}"
    );
    assert!(
        connection[0].eq_ignore_ascii_case("upgrade"),
        "{context}: invalid Connection header: {head:?}"
    );
    assert_eq!(
        *head.header_values("sec-websocket-accept"),
        [VALID_WEBSOCKET_ACCEPT],
        "{context}: Sec-WebSocket-Accept header"
    );
}

fn assert_handshake_rejected(head: &common::HttpResponse, expected_status: u16, context: &str) {
    assert_eq!(
        head.status, expected_status,
        "{context}: unexpected rejection: {head:?}"
    );
    assert!(
        head.header_values("sec-websocket-accept").is_empty(),
        "{context}: rejection exposed Sec-WebSocket-Accept: {head:?}"
    );
    assert!(
        head.header_values("sec-websocket-protocol").is_empty(),
        "{context}: rejection selected a subprotocol: {head:?}"
    );
}

fn websocket_probe_router(dispatch_count: Arc<AtomicUsize>) -> Router {
    let mut router = Router::new();
    router.ws("/ws", move |_request: &Request, connection: WsConn| {
        dispatch_count.fetch_add(1, Ordering::AcqRel);
        connection.send("connected")?;
        Ok(())
    });
    router
}

fn assert_connected_frame(stream: &mut TcpStream, context: &str) {
    assert_eq!(&*read_ws_text_frame(stream), "connected", "{context}");
    write_ws_close_frame(stream);
}

fn assert_invalid_handshake_matrix(addr: std::net::SocketAddr, dispatch_count: &AtomicUsize) {
    INVALID_HANDSHAKE_CASES.iter().for_each(|case| {
        let request = handshake_request("/ws", &case_handshake(case));
        let (_, head) = perform_raw_ws_handshake(addr, &request);
        assert_handshake_rejected(&head, case.expected_status, case.label);
        match case.expected_version_header {
            Some(version) => assert_eq!(
                *head.header_values("sec-websocket-version"),
                [version],
                "{}: rejection must advertise the supported version",
                case.label
            ),
            None => {}
        }
        assert_eq!(
            dispatch_count.load(Ordering::Acquire),
            0,
            "{}: invalid handshake reached the WebSocket handler",
            case.label
        );
    });
}

fn assert_strict_handshake_rejections(addr: std::net::SocketAddr, dispatch_count: &AtomicUsize) {
    let invalid_protocol = handshake_request(
        "/ws",
        &accepted_plus(
            LOCAL_HOST,
            &[("Sec-WebSocket-Protocol", "chat, invalid protocol")],
        ),
    );
    let (_, head) = perform_raw_ws_handshake(addr, &invalid_protocol);
    assert_handshake_rejected(&head, 400, "malformed subprotocol offer");
    let http_10 =
        handshake_request("/ws", &accepted(LOCAL_HOST)).replacen("HTTP/1.1", "HTTP/1.0", 1);
    let (_, head) = perform_raw_ws_handshake(addr, &http_10);
    assert_handshake_rejected(&head, 400, "HTTP/1.0 upgrade");
    assert_eq!(
        dispatch_count.load(Ordering::Acquire),
        0,
        "strict handshake rejections reached the WebSocket handler"
    );
}

fn assert_valid_handshake_after_rejections(
    addr: std::net::SocketAddr,
    dispatch_count: &AtomicUsize,
) {
    let valid = handshake_request("/ws", &accepted(LOCAL_HOST));
    let (mut stream, head) = perform_raw_ws_handshake(addr, &valid);
    assert_websocket_switch(&head, "valid handshake after rejection matrix");
    assert!(
        head.header_values("sec-websocket-protocol").is_empty(),
        "unsolicited subprotocol in valid probe: {head:?}"
    );
    assert_connected_frame(&mut stream, "valid handshake did not dispatch");
    assert_eq!(dispatch_count.load(Ordering::Acquire), 1);
}

/// How many origin categories [`generated_origin`] can produce.
///
/// The modulus and the arm count are the same number, named once: a category
/// added without widening this — or the reverse — fails the generator's own
/// `unreachable!` instead of silently narrowing what the case set covers.
const ORIGIN_CATEGORIES: u64 = 11;

/// One generated case: what it offers as `Origin`, and whether Camber takes it.
///
/// The values alone, not the header lines they are sent as: every one of them
/// is an `Origin`, and a category that offers two of them is a case about a
/// handshake carrying the header twice. Written as a list rather than as a
/// spliced block so the whole request stays one header list.
#[derive(Debug)]
struct GeneratedOrigin {
    label: &'static str,
    origins: Box<[Box<str>]>,
    accepted: bool,
}

/// One offered origin, as the list a single-origin category declares.
fn one_origin(value: String) -> Box<[Box<str>]> {
    Box::new([value.into_boxed_str()])
}

fn generated_host_case(host: &str, case: &mut deterministic::DeterministicCase) -> String {
    host.chars()
        .enumerate()
        .map(
            |(index, character)| match (character.is_ascii_alphabetic(), index, case.boolean()) {
                (true, 0, _) | (true, _, true) => character.to_ascii_uppercase(),
                _ => character,
            },
        )
        .collect()
}

fn generated_origin(
    index: u64,
    case: &mut deterministic::DeterministicCase,
) -> (Box<str>, GeneratedOrigin) {
    let zone = case
        .select(&["alpha", "bravo", "charlie", "delta"])
        .copied()
        .expect("origin zone set is non-empty");
    let host = format!("ws-{index}.{zone}.test");
    let mixed_host = generated_host_case(&host, case);
    let generated_path = case
        .select(&["/", "/private", "/socket/path"])
        .copied()
        .expect("origin path set is non-empty");
    let generated_port = case
        .select(&["81", "444", "8080"])
        .copied()
        .expect("origin port set is non-empty");

    let origin = match index % ORIGIN_CATEGORIES {
        0 => GeneratedOrigin {
            label: "normalized scheme and authority case",
            origins: one_origin(format!("HtTp://{mixed_host}")),
            accepted: true,
        },
        1 => GeneratedOrigin {
            label: "normalized HTTP default port",
            origins: one_origin(format!("HTTP://{mixed_host}:80")),
            accepted: true,
        },
        2 => GeneratedOrigin {
            label: "normalized HTTPS default port",
            origins: one_origin(format!("hTtPs://{mixed_host}:443")),
            accepted: true,
        },
        3 => GeneratedOrigin {
            label: "null origin",
            origins: one_origin("null".to_owned()),
            accepted: false,
        },
        4 => GeneratedOrigin {
            label: "wrong authority",
            origins: one_origin(format!("http://attacker-{index}.{zone}.test")),
            accepted: false,
        },
        5 => GeneratedOrigin {
            label: "wrong port",
            origins: one_origin(format!("http://{host}:{generated_port}")),
            accepted: false,
        },
        6 => GeneratedOrigin {
            label: "userinfo",
            origins: one_origin(format!("http://attacker@{host}")),
            accepted: false,
        },
        7 => GeneratedOrigin {
            label: "path",
            origins: one_origin(format!("http://{host}{generated_path}")),
            accepted: false,
        },
        8 => GeneratedOrigin {
            label: "query",
            origins: one_origin(format!("http://{host}?case={index}")),
            accepted: false,
        },
        9 => GeneratedOrigin {
            label: "fragment",
            origins: one_origin(format!("http://{host}#case-{index}")),
            accepted: false,
        },
        10 => GeneratedOrigin {
            label: "multiple origins",
            origins: Box::new([
                format!("http://{host}").into_boxed_str(),
                format!("http://attacker-{index}.{zone}.test").into_boxed_str(),
            ]),
            accepted: false,
        },
        _ => unreachable!("modulo bounds origin categories"),
    };
    (host.into_boxed_str(), origin)
}

#[test]
fn websocket_rejects_invalid_version_and_key() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let dispatch_count = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(websocket_probe_router(Arc::clone(&dispatch_count)));
            assert_invalid_handshake_matrix(addr, &dispatch_count);
            assert_strict_handshake_rejections(addr, &dispatch_count);
            assert_valid_handshake_after_rejections(addr, &dispatch_count);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_selects_one_offered_subprotocol() {
    const OFFERED_AND_SUPPORTED: [&str; 2] = ["chat", "superchat"];

    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let dispatch_count = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(websocket_probe_router(Arc::clone(&dispatch_count)));
            let request = handshake_request(
                "/ws",
                &accepted_plus(LOCAL_HOST, &[("Sec-WebSocket-Protocol", "chat, superchat")]),
            );
            let (mut stream, head) = perform_raw_ws_handshake(addr, &request);

            assert_websocket_switch(&head, "subprotocol handshake");
            let selected = head.header_values("sec-websocket-protocol");
            assert_eq!(
                selected.len(),
                1,
                "server must emit one Sec-WebSocket-Protocol header: {head:?}"
            );
            assert_eq!(
                selected[0].split(',').count(),
                1,
                "server echoed the offered subprotocol list: {head:?}"
            );
            assert!(
                OFFERED_AND_SUPPORTED.contains(&selected[0]),
                "server selected an unsupported or unoffered subprotocol: {head:?}"
            );
            assert_connected_frame(&mut stream, "selected subprotocol did not dispatch");
            assert_eq!(dispatch_count.load(Ordering::Acquire), 1);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn generated_websocket_origins_normalize_or_reject() {
    const GENERATED_CASES: u64 = 44;

    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let generator = deterministic::DeterministicGenerator::stable();
            assert_eq!(generator.seed(), deterministic::STABLE_SEED);
            let dispatch_count = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(websocket_probe_router(Arc::clone(&dispatch_count)));
            let mut expected_dispatches = 0;

            (0..GENERATED_CASES).for_each(|index| {
                let mut case = generator.case(index);
                let (host, origin) = generated_origin(index, &mut case);
                let context = format!(
                    "seed={:#x} index={} category={} host={host} origins={:?}",
                    case.seed(),
                    case.index(),
                    origin.label,
                    origin.origins
                );
                let offered: Box<[Header<'_>]> = origin
                    .origins
                    .iter()
                    .map(|value| ("Origin", value.as_ref()))
                    .collect();
                let request = handshake_request("/ws", &accepted_plus(&host, &offered));
                let (mut stream, head) = perform_raw_ws_handshake(addr, &request);

                match origin.accepted {
                    true => {
                        assert_websocket_switch(&head, &context);
                        assert!(
                            head.header_values("sec-websocket-protocol").is_empty(),
                            "{context}: unsolicited subprotocol"
                        );
                        assert_connected_frame(&mut stream, &context);
                        expected_dispatches += 1;
                    }
                    false => assert_handshake_rejected(&head, 403, &context),
                }
                assert_eq!(
                    dispatch_count.load(Ordering::Acquire),
                    expected_dispatches,
                    "{context}: origin decision and handler dispatch diverged"
                );
            });

            assert_eq!(expected_dispatches, 12);
            runtime::request_shutdown();
        })
        .unwrap();
}

/// Handshake `path` with the workspace's upgrade request plus `extra` headers.
///
/// The head itself is the shared one: a copy here could drift from what Camber
/// accepts, and then every case below would be proving something about a
/// request no client sends.
/// The router both body-limit upgrade rows serve: a tight body ceiling, a
/// policy that would refuse anything it was asked about, and one WS route.
fn body_limit_ws_router(asked: &Arc<AtomicUsize>) -> Router {
    let mut router = Router::new().max_request_body(10);
    router.ws("/ws", |_req: &Request, conn: WsConn| {
        conn.send("connected")?;
        Ok(())
    });
    router.body_admission(common::refusing_body_admission(asked))
}

fn ws_connect(
    addr: std::net::SocketAddr,
    path: &str,
    extra: &[(&str, &str)],
) -> (TcpStream, common::HttpResponse) {
    perform_raw_ws_handshake(addr, &common::ws_upgrade_request_with(path, extra))
}

#[test]
fn websocket_echo() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                while let Some(msg) = conn.recv() {
                    if conn.send(&msg).is_err() {
                        break;
                    }
                }
                Ok(())
            });

            let addr = common::spawn_server(router);

            let (mut stream, head) = ws_connect(addr, "/ws", &[]);
            assert_websocket_switch(&head, "echo handshake");

            // Send a text frame with "hello"
            write_ws_text_frame(&mut stream, "hello");

            // Read the echo response frame
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(&*msg, "hello");

            // Send close frame
            write_ws_close_frame(&mut stream);

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_server_sends_multiple() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, conn: WsConn| {
                conn.send("one")?;
                conn.send("two")?;
                conn.send("three")?;
                Ok(())
            });

            let addr = common::spawn_server(router);

            let (mut stream, head) = ws_connect(addr, "/ws", &[]);
            assert_websocket_switch(&head, "multi-send handshake");

            let messages: [Box<str>; 3] = std::array::from_fn(|_| read_ws_text_frame(&mut stream));
            assert_eq!(
                [&*messages[0], &*messages[1], &*messages[2]],
                ["one", "two", "three"]
            );

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_handler_sees_request_path_and_headers() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |req: &Request, conn: WsConn| {
                conn.send(req.path())?;
                Ok(())
            });

            let addr = common::spawn_server(router);

            let (mut stream, head) = ws_connect(addr, "/ws?token=abc", &[]);
            assert_websocket_switch(&head, "request-path handshake");

            let msg = read_ws_text_frame(&mut stream);
            assert!(msg.contains("/ws"), "expected path in message: {msg}");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_send_and_recv_binary_frames() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                while let Some(data) = conn.recv_binary() {
                    if conn.send_binary(&data).is_err() {
                        break;
                    }
                }
                Ok(())
            });

            let addr = common::spawn_server(router);
            let (mut stream, head) = ws_connect(addr, "/ws", &[]);
            assert_websocket_switch(&head, "binary-frame handshake");

            let payload = b"\x00\x01\x02\xff\xfe\xfd";
            write_ws_binary_frame(&mut stream, payload);

            let received = read_ws_binary_frame(&mut stream);
            assert_eq!(received.as_ref(), payload);

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_recv_timeout_bounds_a_silent_peer() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let (reported, outcome) = std::sync::mpsc::channel();
            let mut router = Router::new();
            router.ws("/ws", move |_req: &Request, mut conn: WsConn| {
                let result = conn.recv_timeout(Duration::from_millis(50));
                reported.send(result).unwrap();
                Ok(())
            });

            let addr = common::spawn_server(router);
            let (mut stream, head) = ws_connect(addr, "/ws", &[]);
            assert_websocket_switch(&head, "silent-peer handshake");
            let result = outcome
                .recv_timeout(ASYNC_EVENT_TIMEOUT)
                .expect("the timed receive never returned");

            assert!(
                matches!(result, Err(RuntimeError::Timeout)),
                "a silent peer did not expire the receive deadline: {result:?}"
            );
            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_recv_message_returns_both_types() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                // Echo back a description of each received message type
                while let Some(msg) = conn.recv_message() {
                    let reply = match &msg {
                        WsMessage::Text(t) => format!("text:{t}"),
                        WsMessage::Binary(b) => format!("binary:{}", b.len()),
                    };
                    conn.send(&reply)?;
                }
                Ok(())
            });

            let addr = common::spawn_server(router);
            let (mut stream, head) = ws_connect(addr, "/ws", &[]);
            assert_websocket_switch(&head, "message-type handshake");

            // Send text, then binary
            write_ws_text_frame(&mut stream, "hello");
            let r1 = read_ws_text_frame(&mut stream);
            assert_eq!(&*r1, "text:hello");

            write_ws_binary_frame(&mut stream, &[0xDE, 0xAD]);
            let r2 = read_ws_text_frame(&mut stream);
            assert_eq!(&*r2, "binary:2");

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn ws_recv_binary_skips_text_frames() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, mut conn: WsConn| {
                // recv_binary should skip text frames
                if let Some(data) = conn.recv_binary() {
                    conn.send_binary(&data)?;
                }
                Ok(())
            });

            let addr = common::spawn_server(router);
            let (mut stream, head) = ws_connect(addr, "/ws", &[]);
            assert_websocket_switch(&head, "binary-skip handshake");

            // Send text first (should be skipped), then binary
            write_ws_text_frame(&mut stream, "ignored");
            write_ws_binary_frame(&mut stream, &[0xCA, 0xFE]);

            let received = read_ws_binary_frame(&mut stream);
            assert_eq!(received.as_ref(), &[0xCA, 0xFE]);

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_accepts_same_host_origin() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, conn: WsConn| {
                conn.send("connected")?;
                Ok(())
            });

            let addr = common::spawn_server(router);
            let port = addr.port();

            // Origin matches Host after normalization (both include the same port)
            let authority = format!("localhost:{port}");
            let origin = format!("http://{authority}");
            let request = handshake_request(
                "/ws",
                &accepted_plus(&authority, &[("Origin", origin.as_str())]),
            );
            let (mut stream, head) = perform_raw_ws_handshake(addr, &request);
            assert_websocket_switch(&head, "same-host origin handshake");

            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(&*msg, "connected");

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_rejects_cross_host_origin() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, conn: WsConn| {
                conn.send("should not reach")?;
                Ok(())
            });

            let addr = common::spawn_server(router);

            // Origin on a different host
            let (_, head) = ws_connect(addr, "/ws", &[("Origin", "http://evil.example.com")]);
            assert_handshake_rejected(&head, 403, "cross-host origin");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_rejects_null_origin() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", |_req: &Request, conn: WsConn| {
                conn.send("should not reach")?;
                Ok(())
            });

            let addr = common::spawn_server(router);

            let (_, head) = ws_connect(addr, "/ws", &[("Origin", "null")]);
            assert_handshake_rejected(&head, 403, "null origin");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn auth_middleware_blocks_unauthenticated_websocket() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(|req, next| {
                let has_auth = req
                    .headers()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                match has_auth {
                    true => next.call(req),
                    false => Box::pin(async {
                        Response::text(401, "unauthorized").expect("valid status")
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
                }
            });
            router.ws("/chat", |_req: &Request, mut conn: WsConn| {
                while let Some(msg) = conn.recv() {
                    if conn.send(&msg).is_err() {
                        break;
                    }
                }
                Ok(())
            });

            let addr = common::spawn_server(router);

            let (_, head) = ws_connect(addr, "/chat", &[]);
            assert_handshake_rejected(&head, 401, "unauthenticated WebSocket");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_upgrade_ignores_request_body_limit() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let asked = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(body_limit_ws_router(&asked));

            // Send WS upgrade with Content-Length exceeding the body limit.
            // Head-only dispatch skips body collection, so 413 is not returned.
            let (mut stream, head) = ws_connect(addr, "/ws", &[("Content-Length", "99999")]);
            assert_websocket_switch(&head, "body-limit handshake");

            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(&*msg, "connected");

            write_ws_close_frame(&mut stream);

            // The same upgrade over the observed listener, because the body
            // counters are wired on the owned server path alone. This row owns
            // the exclusion claim; the row above owns the handshake predicate,
            // and its connection disposition is that path's own question.
            let port = common::reserve_observed();
            let observed = port.serve(body_limit_ws_router(&asked));
            let (_watched, watched_head) =
                ws_connect(observed.addr(), "/ws", &[("Content-Length", "99999")]);
            assert_eq!(
                watched_head.status, 101,
                "observed handshake: {watched_head:?}"
            );
            assert_eq!(
                *watched_head.header_values("sec-websocket-accept"),
                [VALID_WEBSOCKET_ACCEPT],
                "observed handshake: accept key"
            );

            assert_eq!(
                asked.load(Ordering::SeqCst),
                0,
                "a direct WebSocket upgrade is bodyless, so no body policy is asked about it"
            );
            assert_eq!(observed.controller().body_frames_polled(), 0);
            assert_eq!(observed.controller().body_peak_retained_bytes(), 0);
            assert_eq!(observed.controller().body_permit_owners_dropped(), 0);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn auth_middleware_allows_authenticated_websocket() {
    common::test_runtime()
        .keepalive_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.use_middleware(|req, next| {
                let has_auth = req
                    .headers()
                    .any(|(k, _)| k.eq_ignore_ascii_case("authorization"));
                match has_auth {
                    true => next.call(req),
                    false => Box::pin(async {
                        Response::text(401, "unauthorized").expect("valid status")
                    })
                        as std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>,
                }
            });
            router.ws("/chat", |_req: &Request, mut conn: WsConn| {
                conn.send("welcome")?;
                while let Some(msg) = conn.recv() {
                    if conn.send(&msg).is_err() {
                        break;
                    }
                }
                Ok(())
            });

            let addr = common::spawn_server(router);

            let (mut stream, head) =
                ws_connect(addr, "/chat", &[("Authorization", "Bearer token")]);
            assert_websocket_switch(&head, "authenticated WebSocket handshake");

            // Verify WS works end-to-end
            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(&*msg, "welcome");

            write_ws_text_frame(&mut stream, "ping");
            let echo = read_ws_text_frame(&mut stream);
            assert_eq!(&*echo, "ping");

            write_ws_close_frame(&mut stream);
            runtime::request_shutdown();
        })
        .unwrap();
}

// 1.T9, direct WebSocket portion.
#[test]
fn direct_websocket_bridge_holds_permit_and_finishes_before_owned_completion() {
    runtime::builder()
        .connection_limit(1)
        .keepalive_timeout(Duration::from_secs(5))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            runtime::block_on(async {
                let mut router = lifecycle_websocket_router();
                let mut dispatched = attach_dispatch_probe(&mut router);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .expect("bind owned listener");
                let addr = listener.local_addr().expect("owned listener address");
                let controller = lifecycle(addr).expect("install lifecycle controller");
                let handle = camber::http::serve_background(listener, router);
                let mut websocket = connect_async_websocket(addr, "/ws").await;
                controller
                    .pause_once(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .expect("pause once the second client waits for a permit");
                let mut second = tokio::net::TcpStream::connect(addr)
                    .await
                    .expect("connect permit-waiting peer");
                tokio::io::AsyncWriteExt::write_all(
                    &mut second,
                    b"GET /second HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                )
                .await
                .expect("write permit-waiting request");
                lifecycle_event(
                    "production permit acquisition returned Pending",
                    controller.wait_until_paused(LifecycleCheckpoint::ConnectionPermitWaitPending),
                )
                .await
                .expect("production permit acquisition returned Pending");
                assert!(
                    matches!(
                        dispatched.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Empty)
                    ),
                    "second request dispatched while the direct bridge held the permit"
                );

                runtime::request_shutdown();
                controller
                    .release(LifecycleCheckpoint::ConnectionPermitWaitPending)
                    .expect("release pending permit wait for shutdown");
                let mut owner = Box::pin(handle.into_future());
                assert!(
                    owner.as_mut().now_or_never().is_none(),
                    "owner completed while the direct bridge still owned its transport"
                );
                assert_graceful_close_then_eof(&mut websocket, "permit-holding direct").await;
                assert_transport_eof(&mut second, "permit-waiting transport EOF").await;
                assert!(
                    lifecycle_event("owned direct bridge completion", owner.as_mut())
                        .await
                        .is_ok()
                );
                assert!(
                    matches!(
                        dispatched.try_recv(),
                        Err(tokio::sync::oneshot::error::TryRecvError::Closed)
                    ),
                    "permit-waiting dispatch sender remained live after owner completion"
                );
            });
        })
        .unwrap();
}

fn blocking_callback_router(
    entered_tx: tokio::sync::oneshot::Sender<()>,
    release_rx: std::sync::mpsc::Receiver<()>,
    callback_result_tx: tokio::sync::oneshot::Sender<bool>,
    callback_dropped: Arc<AtomicBool>,
) -> Router {
    let entered_tx = Arc::new(Mutex::new(Some(entered_tx)));
    let release_rx = Arc::new(Mutex::new(release_rx));
    let callback_result_tx = Arc::new(Mutex::new(Some(callback_result_tx)));
    let mut router = Router::new();
    router.ws("/ws", move |_request: &Request, connection: WsConn| {
        let _probe = CallbackDropProbe(Arc::clone(&callback_dropped));
        if let Some(sender) = entered_tx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = sender.send(());
        }
        let _ = release_rx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .recv();
        let peers_were_closed = connection.send("after owner completion").is_err();
        if let Some(sender) = callback_result_tx
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            let _ = sender.send(peers_were_closed);
        }
        Ok(())
    });
    router
}

struct BlockingCallbackScenario {
    release: std::sync::mpsc::Sender<()>,
    callback_result: tokio::sync::oneshot::Receiver<bool>,
    callback_dropped: Arc<AtomicBool>,
    handle: camber::http::ServerHandle,
    websocket: tokio::net::TcpStream,
}

async fn start_blocking_callback_scenario() -> BlockingCallbackScenario {
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (callback_result_tx, callback_result_rx) = tokio::sync::oneshot::channel();
    let callback_dropped = Arc::new(AtomicBool::new(false));
    let router = blocking_callback_router(
        entered_tx,
        release_rx,
        callback_result_tx,
        Arc::clone(&callback_dropped),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind callback-boundary listener");
    let addr = listener.local_addr().expect("callback listener address");
    let handle = camber::http::serve_background(listener, router);
    let websocket = connect_async_websocket(addr, "/ws").await;
    lifecycle_event("blocking WebSocket callback entry", entered_rx)
        .await
        .expect("blocking callback reports entry");
    BlockingCallbackScenario {
        release: release_tx,
        callback_result: callback_result_rx,
        callback_dropped,
        handle,
        websocket,
    }
}

async fn finish_blocking_callback_scenario(mut scenario: BlockingCallbackScenario) {
    runtime::request_shutdown();
    let mut owner = Box::pin(scenario.handle.into_future());
    assert_graceful_close_then_eof(&mut scenario.websocket, "callback-boundary").await;
    assert!(
        lifecycle_event("owner completion across callback boundary", owner.as_mut())
            .await
            .is_ok()
    );
    assert!(
        !scenario.callback_dropped.load(Ordering::Acquire),
        "owner completion incorrectly claimed blocking callback exit"
    );
    assert!(
        matches!(
            scenario.callback_result.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "blocking callback returned before its explicit release"
    );
    scenario
        .release
        .send(())
        .expect("release blocking callback");
    assert!(
        lifecycle_event("callback-side channel failure", scenario.callback_result)
            .await
            .expect("callback reports post-owner send result"),
        "callback-side WsConn retained a live supervisor peer after owner completion"
    );
    wait_for_dropped_flag(
        &scenario.callback_dropped,
        "blocking callback did not drop after reporting its result",
    )
    .await;
}

// 1.T15.
#[camber::test]
async fn owner_releases_direct_transport_without_claiming_blocking_callback_exit() {
    let scenario = start_blocking_callback_scenario().await;
    finish_blocking_callback_scenario(scenario).await;
}

// 1.T18, graceful direct WebSocket portion.
#[camber::test]
async fn graceful_direct_websocket_shutdown_sends_close_before_eof_and_join() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind graceful WebSocket listener");
    let addr = listener.local_addr().expect("graceful listener address");
    let handle = camber::http::serve_background(listener, lifecycle_websocket_router());
    let mut websocket = connect_async_websocket(addr, "/ws").await;

    runtime::request_shutdown();
    let mut owner = Box::pin(handle.into_future());
    assert_graceful_close_then_eof(&mut websocket, "graceful direct").await;
    assert!(
        lifecycle_event("graceful direct bridge join", owner.as_mut())
            .await
            .is_ok()
    );
}

// 1.T18, forced direct WebSocket portion.
#[camber::test]
async fn forced_direct_websocket_abort_releases_transport_before_cancelled() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind forced WebSocket listener");
    let addr = listener.local_addr().expect("forced listener address");
    let handle = camber::http::serve_background(listener, lifecycle_websocket_router());
    let mut websocket = connect_async_websocket(addr, "/ws").await;

    handle.cancel();
    let mut owner = Box::pin(handle.into_future());
    assert_optional_close_then_eof(&mut websocket, "forced direct").await;
    assert_cancelled(lifecycle_event("forced direct bridge join", owner.as_mut()).await);
}

async fn pending_direct_upgrade_shutdown_is_rejected(forced: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind pending-upgrade listener");
    let addr = listener.local_addr().expect("pending listener address");
    let controller = lifecycle(addr).expect("install lifecycle controller");
    arm_unacknowledged_upgrade(&controller);
    let handle = camber::http::serve_background(listener, lifecycle_websocket_router());
    let mut pending = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect pending WebSocket peer");
    async_ws_request(&mut pending, "/ws").await;
    wait_for_unacknowledged_upgrade(&controller).await;
    match forced {
        true => handle.cancel(),
        false => runtime::request_shutdown(),
    }
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release pending upgrade into shutdown");
    let mut owner = Box::pin(handle.into_future());
    let response = read_async_http_head(&mut pending, "the rejected direct-upgrade response").await;
    let response_lower = response.to_ascii_lowercase();
    assert_eq!(
        status_from_raw(&response),
        503,
        "shutdown committed an unexpected upgrade response: {response}"
    );
    assert!(
        response_lower.contains("connection: close"),
        "upgrade rejection omitted Connection: close: {response}"
    );
    assert_refusal_body_then_eof(
        &mut pending,
        "service unavailable",
        "pending direct-upgrade transport EOF",
    )
    .await;
    let result = lifecycle_event("pending direct-upgrade drain", owner.as_mut()).await;
    match forced {
        true => assert_cancelled(result),
        false => assert!(result.is_ok(), "graceful owner returned {result:?}"),
    }
}

// 1.T21, direct WebSocket registrar-cancellation portion.
#[camber::test]
async fn cancelled_pending_direct_upgrade_is_joined_and_connection_local() {
    let callback_count = Arc::new(AtomicUsize::new(0));
    let mut router = Router::new();
    router.ws("/ws", {
        let callback_count = Arc::clone(&callback_count);
        move |_request: &Request, _connection: WsConn| {
            callback_count.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
    });
    router.get("/ok", |_request: &Request| async {
        Response::text(200, "ok")
    });

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind cancellation listener");
    let addr = listener
        .local_addr()
        .expect("cancellation listener address");
    let controller = lifecycle(addr).expect("install lifecycle controller");
    arm_unacknowledged_upgrade(&controller);
    controller
        .pause_once(LifecycleCheckpoint::UpgradePeerClosed)
        .expect("pause after direct peer closure is observed");
    let handle = camber::http::serve_background(listener, router);
    let mut pending = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect cancellable WebSocket peer");
    async_ws_request(&mut pending, "/ws").await;
    wait_for_unacknowledged_upgrade(&controller).await;
    drop(pending);
    lifecycle_event(
        "owned reader observation of direct peer closure",
        controller.wait_until_paused(LifecycleCheckpoint::UpgradePeerClosed),
    )
    .await
    .expect("owned reader observes direct peer closure");
    controller
        .release(LifecycleCheckpoint::UpgradePeerClosed)
        .expect("release observed direct peer closure");
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release cancelled registration");

    assert_http_ok(addr, "/ok", "the listener after registrar cancellation").await;
    runtime::request_shutdown();
    assert!(
        lifecycle_event(
            "owner join after registrar cancellation",
            handle.into_future()
        )
        .await
        .is_ok()
    );
    assert_eq!(
        callback_count.load(Ordering::Acquire),
        0,
        "cancelled upgrade reached its WebSocket callback"
    );
}

// 1.T21, graceful direct WebSocket rejection portion.
#[camber::test]
async fn graceful_shutdown_rejects_unacknowledged_direct_upgrade() {
    pending_direct_upgrade_shutdown_is_rejected(false).await;
}

// 1.T21, forced direct WebSocket rejection portion.
#[camber::test]
async fn forced_shutdown_rejects_unacknowledged_direct_upgrade() {
    pending_direct_upgrade_shutdown_is_rejected(true).await;
}

// 1.T21, direct WebSocket supervisor-unwind portion.
#[camber::test]
async fn supervisor_unwind_joins_acknowledged_and_pending_direct_upgrades() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind unwind listener");
    let addr = listener.local_addr().expect("unwind listener address");
    let controller = lifecycle(addr).expect("install lifecycle controller");
    let handle = camber::http::serve_background(listener, lifecycle_websocket_router());
    let mut acknowledged = connect_async_websocket(addr, "/ws").await;

    arm_unacknowledged_upgrade(&controller);
    let mut pending = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect pending direct upgrade");
    async_ws_request(&mut pending, "/ws").await;
    wait_for_unacknowledged_upgrade(&controller).await;
    controller
        .inject_once(LifecycleFault::PanicSupervisorCore)
        .expect("inject supervisor unwind");
    controller
        .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
        .expect("release supervisor into unwind");

    let mut owner = Box::pin(handle.into_future());
    assert_optional_close_then_eof(&mut acknowledged, "unwound direct").await;
    let pending_response =
        read_async_http_head(&mut pending, "the unwound direct-upgrade response").await;
    assert_eq!(
        status_from_raw(&pending_response),
        500,
        "supervisor-unavailable direct upgrade did not return 500: {pending_response}"
    );
    assert!(
        pending_response
            .to_ascii_lowercase()
            .contains("connection: close"),
        "pending unwind response omitted Connection: close: {pending_response}"
    );
    assert_refusal_body_then_eof(
        &mut pending,
        "internal server error",
        "unwound pending transport EOF",
    )
    .await;
    match lifecycle_event("supervisor unwind drain", owner.as_mut()).await {
        Err(RuntimeError::TaskPanicked(message)) => assert!(!message.is_empty()),
        other => panic!("expected TaskPanicked after upgrade drain, got {other:?}"),
    }
}
