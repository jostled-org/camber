#![cfg(feature = "ws")]

use crate::common;
use crate::deterministic;

use crate::handshake::{
    Header, LOCAL_HOST, accepted, accepted_plus, assert_handshake_rejected,
    assert_websocket_switch_accepting, complete_server_close, handshake_request,
    perform_raw_ws_handshake,
};

use crate::common::{
    ASYNC_EVENT_TIMEOUT, assert_http_ok, assert_optional_close_then_eof,
    assert_refusal_body_then_eof, lifecycle_event, read_async_http_head, read_ws_binary_frame,
    read_ws_text_frame, registered_connections, status_from_raw, transferred_upgrades,
    write_ws_binary_frame, write_ws_close_frame, write_ws_text_frame,
};
use camber::RuntimeError;
use camber::http::mock::{
    ConnectionOwnershipEvent, ConnectionOwnershipObservation, ScopedConnectionOwner,
    UpgradeOwnerController, UpgradeOwnerEdge, connection_owner, faulted_registration,
    registration_selection, upgrade_owner,
};
use camber::http::{Request, Response, Router, WsConn, WsMessage};
use camber::runtime;
use std::future::IntoFuture;
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

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

/// Hold one upgrade child either side of the acknowledgement it is waiting for.
///
/// The upgrade owner and nothing else, taken as a loan: three rows below hold a
/// child this way and each names a different second family beside it, so the
/// pair of arms is written once against the one family both moves belong to.
fn arm_unacknowledged_upgrade(owners: &impl common::Owns<UpgradeOwnerController>) {
    let upgrades = owners.owner();
    upgrades
        .pause_once(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("pause after upgrade-ticket submission");
    upgrades
        .pause_once(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .expect("pause before upgrade acknowledgement");
}

async fn wait_for_unacknowledged_upgrade(owners: &impl common::Owns<UpgradeOwnerController>) {
    let upgrades = owners.owner();
    lifecycle_event(
        "upgrade ticket reaches the production registration channel",
        upgrades.wait_until_paused(UpgradeOwnerEdge::AfterHandoffSubmitted),
    )
    .await
    .expect("upgrade ticket reaches the production registration channel");
    upgrades
        .release(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("release submitted upgrade ticket");
    lifecycle_event(
        "submitted upgrade reaches acknowledgement checkpoint",
        upgrades.wait_until_paused(UpgradeOwnerEdge::BeforeTransferAcknowledge),
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

/// The switch every accepted upgrade answers with, for the RFC 6455 key.
///
/// The accept value is the RFC's own literal rather than one derived here, so
/// these rows prove the derivation instead of restating it. The
/// `websocket_upgrade_excludes_body_policy_and_refuses_a_declared_payload` row
/// owns the one request that could earn a different answer.
fn assert_websocket_switch(head: &common::HttpResponse, context: &str) {
    assert_websocket_switch_accepting(head, VALID_WEBSOCKET_ACCEPT, context);
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

/// Read the probe's one frame, then finish the close its return started.
fn assert_connected_frame(stream: &mut TcpStream, context: &str) {
    assert_eq!(&*read_ws_text_frame(stream), "connected", "{context}");
    complete_server_close(stream, context);
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

/// What every origin rule may draw on: the case index and the values drawn for it.
struct OriginInput<'a> {
    index: u64,
    zone: &'a str,
    host: &'a str,
    mixed_host: &'a str,
    path: &'a str,
    port: &'a str,
}

/// One origin category: the rule that builds its case. The table's length is
/// the modulus, so a category cannot be added without joining the cycle.
type OriginRule = fn(&OriginInput<'_>) -> GeneratedOrigin;

const ORIGIN_RULES: [OriginRule; 11] = [
    |input| GeneratedOrigin {
        label: "normalized scheme and authority case",
        origins: one_origin(format!("HtTp://{}", input.mixed_host)),
        accepted: true,
    },
    |input| GeneratedOrigin {
        label: "normalized HTTP default port",
        origins: one_origin(format!("HTTP://{}:80", input.mixed_host)),
        accepted: true,
    },
    |input| GeneratedOrigin {
        label: "normalized HTTPS default port",
        origins: one_origin(format!("hTtPs://{}:443", input.mixed_host)),
        accepted: true,
    },
    |_| GeneratedOrigin {
        label: "null origin",
        origins: one_origin("null".to_owned()),
        accepted: false,
    },
    |input| GeneratedOrigin {
        label: "wrong authority",
        origins: one_origin(format!(
            "http://attacker-{}.{}.test",
            input.index, input.zone
        )),
        accepted: false,
    },
    |input| GeneratedOrigin {
        label: "wrong port",
        origins: one_origin(format!("http://{}:{}", input.host, input.port)),
        accepted: false,
    },
    |input| GeneratedOrigin {
        label: "userinfo",
        origins: one_origin(format!("http://attacker@{}", input.host)),
        accepted: false,
    },
    |input| GeneratedOrigin {
        label: "path",
        origins: one_origin(format!("http://{}{}", input.host, input.path)),
        accepted: false,
    },
    |input| GeneratedOrigin {
        label: "query",
        origins: one_origin(format!("http://{}?case={}", input.host, input.index)),
        accepted: false,
    },
    |input| GeneratedOrigin {
        label: "fragment",
        origins: one_origin(format!("http://{}#case-{}", input.host, input.index)),
        accepted: false,
    },
    |input| GeneratedOrigin {
        label: "multiple origins",
        origins: Box::new([
            format!("http://{}", input.host).into_boxed_str(),
            format!("http://attacker-{}.{}.test", input.index, input.zone).into_boxed_str(),
        ]),
        accepted: false,
    },
];

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

    let input = OriginInput {
        index,
        zone,
        host: &host,
        mixed_host: &mixed_host,
        path: generated_path,
        port: generated_port,
    };
    let slot = usize::try_from(index).expect("a case index fits") % ORIGIN_RULES.len();
    let origin = ORIGIN_RULES[slot](&input);
    (host.into_boxed_str(), origin)
}

#[test]
fn websocket_rejects_invalid_version_and_key() {
    common::test_runtime()
        .header_timeout(Duration::from_millis(200))
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

/// One offer a direct route selects from, and the token it must echo.
struct SubprotocolOffer {
    label: &'static str,
    /// Each entry is one `Sec-WebSocket-Protocol` field, in the order sent.
    fields: &'static [&'static str],
    selected: Option<&'static str>,
}

/// Direct selection is the first offered token, read across repeated fields in
/// the order the client sent them. No offer selects nothing.
const SUBPROTOCOL_OFFERS: [SubprotocolOffer; 4] = [
    SubprotocolOffer {
        label: "one field with two offers",
        fields: &["chat, superchat"],
        selected: Some("chat"),
    },
    SubprotocolOffer {
        label: "repeated fields keep their order",
        fields: &["superchat", "chat, v2.json"],
        selected: Some("superchat"),
    },
    SubprotocolOffer {
        label: "optional whitespace around offers",
        fields: &[" \tv2.json ,chat"],
        selected: Some("v2.json"),
    },
    SubprotocolOffer {
        label: "no offers",
        fields: &[],
        selected: None,
    },
];

#[test]
fn websocket_selects_one_offered_subprotocol() {
    common::test_runtime()
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let dispatch_count = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(websocket_probe_router(Arc::clone(&dispatch_count)));

            SUBPROTOCOL_OFFERS
                .iter()
                .enumerate()
                .for_each(|(dispatched, offer)| {
                    let fields: Box<[Header<'_>]> = offer
                        .fields
                        .iter()
                        .map(|value| ("Sec-WebSocket-Protocol", *value))
                        .collect();
                    let request = handshake_request("/ws", &accepted_plus(LOCAL_HOST, &fields));
                    let (mut stream, head) = perform_raw_ws_handshake(addr, &request);

                    assert_websocket_switch(&head, offer.label);
                    assert_eq!(
                        *head.header_values("sec-websocket-protocol"),
                        *offer.selected.as_slice(),
                        "{}: server must echo exactly the first offer: {head:?}",
                        offer.label
                    );
                    assert_connected_frame(&mut stream, offer.label);
                    assert_eq!(
                        dispatch_count.load(Ordering::Acquire),
                        dispatched + 1,
                        "{}: selection and handler dispatch diverged",
                        offer.label
                    );
                });

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn generated_websocket_origins_normalize_or_reject() {
    const GENERATED_CASES: u64 = 44;

    common::test_runtime()
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let mut router = Router::new();
            router.ws("/ws", common::echo_ws);

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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
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
        .header_timeout(Duration::from_millis(200))
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
            router.ws("/chat", common::echo_ws);

            let addr = common::spawn_server(router);

            let (_, head) = ws_connect(addr, "/chat", &[]);
            assert_handshake_rejected(&head, 401, "unauthenticated WebSocket");

            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn websocket_upgrade_excludes_body_policy_and_refuses_a_declared_payload() {
    common::test_runtime()
        .header_timeout(Duration::from_millis(200))
        .shutdown_timeout(Duration::from_secs(2))
        .run(|| {
            let asked = Arc::new(AtomicUsize::new(0));
            let addr = common::spawn_server(body_limit_ws_router(&asked));

            // A handshake under a 10-byte route limit still earns its switch:
            // head-only dispatch never asks the body policy, so the limit has
            // nothing to refuse and no `413` is reachable here. The switch is
            // asserted whole — a `101` that does not say `Connection: Upgrade`
            // is one RFC 6455 §4.1 makes a conforming client fail.
            let (mut stream, head) = ws_connect(addr, "/ws", &[]);
            assert_websocket_switch(&head, "body-limit handshake");

            let msg = read_ws_text_frame(&mut stream);
            assert_eq!(&*msg, "connected");

            write_ws_close_frame(&mut stream);

            // A handshake declaring a payload is refused at the head instead.
            // The `101` would hand the transport to the bridge with those bytes
            // unframed, and Hyper answers such a response by marking the
            // connection to close — the one answer a conforming client cannot
            // accept. Both serving families refuse alike, because since the
            // synchronous entry points moved onto the shared supervisor there
            // is only one family.
            let (_declared, declared_head) =
                ws_connect(addr, "/ws", &[("Content-Length", "99999")]);
            assert_handshake_rejected(&declared_head, 400, "declared-payload handshake");

            // The refusal reads the declared length, not the header's presence:
            // `Content-Length: 0` declares no payload, leaves the transport
            // framed, and upgrades like any other handshake.
            let (mut empty, empty_head) = ws_connect(addr, "/ws", &[("Content-Length", "0")]);
            assert_websocket_switch(&empty_head, "zero-length handshake");
            assert_eq!(&*read_ws_text_frame(&mut empty), "connected");
            write_ws_close_frame(&mut empty);

            // Both rows again over the observed listener, because the body
            // counters are wired on the owned server path alone. The refusal
            // has to reach the same head-only exclusion the accepted upgrade
            // does: a `400` produced by reading the payload would be the body
            // policy answering after all.
            let port = common::reserve_request_body_owner();
            let observed = port.serve(body_limit_ws_router(&asked));
            let (mut watched, watched_head) = ws_connect(observed.addr(), "/ws", &[]);
            assert_websocket_switch(&watched_head, "observed handshake");
            assert_eq!(&*read_ws_text_frame(&mut watched), "connected");
            write_ws_close_frame(&mut watched);

            let (_watched_declared, watched_declared_head) =
                ws_connect(observed.addr(), "/ws", &[("Content-Length", "99999")]);
            assert_handshake_rejected(
                &watched_declared_head,
                400,
                "observed declared-payload handshake",
            );

            assert_eq!(
                asked.load(Ordering::SeqCst),
                0,
                "a direct WebSocket upgrade is bodyless, so no body policy is asked about it"
            );
            let body = observed.controller().observed();
            assert_eq!(body.frames_polled, 0);
            assert_eq!(body.peak_retained_bytes, 0);
            assert_eq!(body.permit_owners_dropped, 0);
            runtime::request_shutdown();
        })
        .unwrap();
}

#[test]
fn auth_middleware_allows_authenticated_websocket() {
    common::test_runtime()
        .header_timeout(Duration::from_millis(200))
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
                common::echo_until_closed(&mut conn);
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

async fn pending_direct_upgrade_shutdown_is_rejected(forced: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind pending-upgrade listener");
    let addr = listener.local_addr().expect("pending listener address");
    let controller =
        registration_selection(addr).expect("register the pending upgrade's stop and children");
    arm_unacknowledged_upgrade(&controller);
    let handle = camber::http::serve_background(listener, lifecycle_websocket_router())
        .expect("owned server requires a Tokio runtime");
    let mut pending = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect pending WebSocket peer");
    async_ws_request(&mut pending, "/ws").await;
    wait_for_unacknowledged_upgrade(&controller).await;
    match forced {
        true => handle.cancel(),
        false => runtime::request_shutdown(),
    }
    // Released only once this server's own stop state has committed. A
    // cancellation commits before the command returns; a runtime shutdown
    // commits when the supervisor takes the signal, and the answer this
    // connection gives has to be on the far side of whichever it was.
    common::await_committed_stop(&controller, "the pending direct upgrade").await;
    controller
        .upgrades
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
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
    let controller = upgrade_owner(addr).expect("register the cancelled upgrade's children");
    arm_unacknowledged_upgrade(&controller);
    controller
        .pause_once(UpgradeOwnerEdge::PeerClosed)
        .expect("pause after direct peer closure is observed");
    let handle = camber::http::serve_background(listener, router)
        .expect("owned server requires a Tokio runtime");
    let mut pending = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect cancellable WebSocket peer");
    async_ws_request(&mut pending, "/ws").await;
    wait_for_unacknowledged_upgrade(&controller).await;
    drop(pending);
    lifecycle_event(
        "owned reader observation of direct peer closure",
        controller.wait_until_paused(UpgradeOwnerEdge::PeerClosed),
    )
    .await
    .expect("owned reader observes direct peer closure");
    controller
        .release(UpgradeOwnerEdge::PeerClosed)
        .expect("release observed direct peer closure");
    controller
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
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
    let controller =
        faulted_registration(addr).expect("register the unwound supervisor's children");
    let handle = camber::http::serve_background(listener, lifecycle_websocket_router())
        .expect("owned server requires a Tokio runtime");
    let mut acknowledged = connect_async_websocket(addr, "/ws").await;

    arm_unacknowledged_upgrade(&controller);
    let mut pending = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect pending direct upgrade");
    async_ws_request(&mut pending, "/ws").await;
    wait_for_unacknowledged_upgrade(&controller).await;
    common::unwind_the_supervisor(&controller, "the unwinding supervisor").await;
    // Released only once the unwind has committed its forced phase, so the
    // connection's answer reads a server that has already stopped admitting
    // rather than racing the panic it is meant to follow.
    controller
        .upgrades
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .expect("release the held transfer edge");

    let mut owner = Box::pin(handle.into_future());
    assert_optional_close_then_eof(&mut acknowledged, "unwound direct").await;
    let pending_response =
        read_async_http_head(&mut pending, "the unwound direct-upgrade response").await;
    // A refusal rather than an internal failure: the connection holding the
    // offer reads the forced phase the unwinding supervisor committed, so it
    // knows the server stopped admitting rather than only that an owner went
    // away.
    assert_eq!(
        status_from_raw(&pending_response),
        503,
        "the refused direct upgrade did not return 503: {pending_response}"
    );
    assert!(
        pending_response
            .to_ascii_lowercase()
            .contains("connection: close"),
        "pending unwind response omitted Connection: close: {pending_response}"
    );
    assert_refusal_body_then_eof(
        &mut pending,
        "service unavailable",
        "unwound pending transport EOF",
    )
    .await;
    match lifecycle_event("supervisor unwind drain", owner.as_mut()).await {
        Err(RuntimeError::TaskPanicked(message)) => assert!(!message.is_empty()),
        other => panic!("expected TaskPanicked after upgrade drain, got {other:?}"),
    }
}

/// A router whose one upgrade route holds its bridge until the peer closes.
fn owner_tree_router() -> Router {
    let mut router = Router::new();
    router.get("/ok", |_request: &Request| async {
        Response::text(200, "ok")
    });
    router.ws("/ws", |_request: &Request, mut conn: WsConn| {
        while conn.recv().is_some() {}
        Ok(())
    });
    router
}

/// Fail if the record names any upgrade beside a connection rather than beneath
/// one.
///
/// The prohibited vocabulary is checked by shape rather than by identity: no
/// server-scope upgrade event may appear at all, whatever it names.
fn assert_no_sideways_upgrade(observed: &ConnectionOwnershipObservation, context: &str) {
    for event in observed.events.iter() {
        assert!(
            !matches!(
                event,
                ConnectionOwnershipEvent::ServerUpgradeRegistered { .. }
                    | ConnectionOwnershipEvent::ServerUpgradeSettled { .. }
            ),
            "{context}: an upgrade registered beside its connection: {event:?}"
        );
    }
}

// 2.T1 — Invariant 5 substrate: each request and upgrade has exactly one parent,
// and no upgrade registers beside its connection.
//
// The barrier is the protocol's: the `101` has been read, so the connection has
// already produced its response head and transferred what it produced. The
// record is then read for the whole sequence under one connection identity.
#[camber::test]
async fn connection_owner_transfers_request_to_upgrade_without_sideways_registration() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let controller = connection_owner(addr).expect("register the owner-tree observer");
    let handle = camber::http::serve_background(listener, owner_tree_router())
        .expect("owned server requires a Tokio runtime");

    let mut peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    async_ws_request(&mut peer, "/ws").await;
    let response = read_async_http_head(&mut peer, "the owner-tree upgrade response").await;
    assert_eq!(status_from_raw(&response), 101, "unexpected: {response}");

    let observed = controller.observed();
    assert_no_sideways_upgrade(&observed, "the transferred upgrade");
    let connections = registered_connections(&observed);
    assert_eq!(
        connections.len(),
        1,
        "one peer registered {} connection owners: {:?}",
        connections.len(),
        observed.events
    );
    let connection = connections[0];
    let request = observed
        .events
        .iter()
        .find_map(|event| match event {
            ConnectionOwnershipEvent::ConnectionRequestAdmitted {
                connection: parent,
                request,
            } if *parent == connection => Some(*request),
            _ => None,
        })
        .expect("the handshake request was never admitted under its connection");
    assert!(
        observed.contains(ConnectionOwnershipEvent::ConnectionRequestSettled {
            connection,
            request,
        }),
        "the handshake request never settled under its connection: {:?}",
        observed.events
    );
    let upgrade = transferred_upgrades(&observed)
        .iter()
        .find_map(|(parent, upgrade)| (*parent == connection).then_some(*upgrade))
        .expect("the upgrade was never transferred to its connection");
    assert_ne!(
        upgrade, request,
        "the upgrade reused its request's identity, so the tree names one owner twice"
    );

    // Closing the peer settles the child, and the child settles under the same
    // parent it was transferred to.
    drop(peer);
    let settled = ConnectionOwnershipEvent::ConnectionUpgradeSettled {
        connection,
        upgrade,
    };
    lifecycle_event(
        "the transferred upgrade never settled under its connection",
        await_ownership_event(&controller, settled),
    )
    .await;
    assert_no_sideways_upgrade(&controller.observed(), "the settled upgrade");

    runtime::request_shutdown();
    assert!(handle.await.is_ok());
}

/// Wait until the owner tree has recorded `event`.
///
/// The record is written by production at the mutation it names, so this only
/// reads: a case waiting here cannot make the event happen, and the bound its
/// caller applies is what turns a settlement that never arrives into a failure.
async fn await_ownership_event(
    controller: &ScopedConnectionOwner,
    event: ConnectionOwnershipEvent,
) {
    while !controller.observed().contains(event) {
        tokio::task::yield_now().await;
    }
}
