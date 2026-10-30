//! Issue 52: the configurable request-body hard cap, and the request-body
//! limits it contains.

use crate::streaming_multipart::{BOUNDARY, Field, content_type, drain_fields, multipart_body};
use crate::{http as wire, runtime_support as common};
use camber::http::{
    self, BodyAdmission, EffectivePolicies, HostRouter, PolicyDimension, PolicyLayer, PolicyLayers,
    PolicyValue, Response, Router, ServerPolicy,
};
use camber::runtime::RuntimeBuilder;
use camber::{RuntimeError, runtime};
use std::io::Write;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

const MIB: usize = 1024 * 1024;
const GIB: usize = 1024 * MIB;
/// The status a body-admission callback's own refusal maps to.
const APPLICATION_REFUSAL: u16 = 503;

fn layers(sources: PolicyLayers) -> Vec<(PolicyLayer, bool)> {
    sources
        .iter()
        .map(|source| (source.layer(), source.is_default()))
        .collect()
}

/// One expected request-body diagnostic: layer, configured, effective, and the
/// limiting layers with whether each used its default.
type Expected = (
    PolicyLayer,
    PolicyDimension,
    usize,
    usize,
    Vec<(PolicyLayer, bool)>,
);

/// Assert the report carries exactly `expected`, in resolution order.
fn assert_diagnostics(report: &EffectivePolicies, expected: &[Expected]) {
    let expected: Vec<_> = expected
        .iter()
        .map(|(layer, dimension, configured, effective, sources)| {
            (
                *layer,
                *dimension,
                PolicyValue::Bytes(Some(*configured)),
                PolicyValue::Bytes(Some(*effective)),
                sources.clone(),
            )
        })
        .collect();
    let actual: Vec<_> = report
        .diagnostics()
        .iter()
        .map(|diagnostic| {
            (
                diagnostic.layer(),
                diagnostic.dimension(),
                diagnostic.configured(),
                diagnostic.effective(),
                layers(diagnostic.sources()),
            )
        })
        .collect();
    assert_eq!(actual, expected);
}

fn echo_router() -> Router {
    let mut router = Router::new();
    router.post("/body", |request| {
        let bytes = request.body_shared();
        async move { Response::bytes(200, bytes) }
    });
    router
}

/// An echo route whose admission callback counts calls and then refuses.
fn refusing_router(called: &Arc<AtomicUsize>) -> Router {
    let observed = Arc::clone(called);
    echo_router().body_admission(move |_| {
        observed.fetch_add(1, Ordering::SeqCst);
        Err(RuntimeError::InvalidArgument("application refusal".into()))
    })
}

/// Declare `bytes` of body, send none of it, and return the answer's status.
fn declared_status(address: SocketAddr, bytes: usize) -> u16 {
    let (answer, peer) = wire::send_withheld_declaration(
        address,
        wire::CLOSE_AFTER_RESPONSE,
        "POST",
        "/body",
        &bytes.to_string(),
    )
    .unwrap();
    drop(peer);
    answer.status
}

/// Send a chunked request whose data frames are exactly `chunks`.
fn chunked_status(
    address: SocketAddr,
    path: &str,
    headers: &[(&str, &str)],
    chunks: &[&[u8]],
) -> u16 {
    let mut stream = wire::connect(address).unwrap();
    let head = wire::framed_chunked_request_head(wire::CLOSE_AFTER_RESPONSE, "POST", path, headers);
    stream.write_all(&head).unwrap();
    let written = chunks
        .iter()
        .try_for_each(|chunk| wire::write_chunk(&mut stream, chunk))
        .and_then(|()| wire::write_chunked_end(&mut stream));
    wire::tolerate_answered_peer(written).unwrap();
    wire::read_http_response_bounded(&mut stream)
        .unwrap()
        .status
}

#[tokio::test]
async fn multipart_report_keeps_byte_admission_separate_from_upload_deadlines() {
    let outer =
        http::TransferBudget::bounded(2, Duration::from_secs(9), Duration::from_secs(17)).unwrap();
    let local =
        http::TransferBudget::bounded(1, Duration::from_secs(6), Duration::from_secs(11)).unwrap();
    let mut router = Router::new().upload_budget(local);
    router.multipart(
        http::Method::Post,
        "/multipart",
        http::MultipartLimits::default(),
        |_, mut stream| async move {
            let count = drain_fields(&mut stream).await?;
            Response::text(200, &count.to_string())
        },
    );
    router.proxy_stream("/proxy", "http://127.0.0.1:9");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let handle = http::server(router)
        .policy(ServerPolicy::default().upload_budget(outer))
        .serve_background(listener)
        .unwrap();
    let body = multipart_body(BOUNDARY, &[Field::text("field", "hello")]);
    let response = reqwest::Client::new()
        .post(format!("http://{address}/multipart"))
        .header("Content-Type", &*content_type(BOUNDARY))
        .body(body)
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.text().await.unwrap(), "5");

    let routes = handle.effective_policies().routes();
    let find = |prefix: &str| routes.iter().find(|route| route.route() == prefix).unwrap();
    for route in [find("/multipart"), find("/proxy")] {
        let upload = route.upload_budget();
        assert_eq!(upload.max_bytes().value(), None);
        assert!(upload.max_bytes().sources().is_empty());
        assert_eq!(upload.idle().value(), local.idle());
        assert_eq!(
            layers(upload.idle().sources()),
            [(PolicyLayer::Router, false)]
        );
        assert_eq!(upload.total().value(), local.total());
        assert_eq!(route.request_body_limit().unwrap().value(), 8 * MIB);
    }
    handle.shutdown_and_join().await.unwrap();
}

#[tokio::test]
async fn specialized_upload_cap_reports_narrowing_for_both_proxy_modes() {
    let mut router = Router::new();
    let proxy = http::ProxyPolicy::default().upload_budget(
        http::TransferBudget::unbounded()
            .with_max_bytes(32)
            .unwrap(),
    );
    router.proxy_with_policy("/buffered", "http://127.0.0.1:9", proxy);
    router.proxy_stream_with_policy("/streamed", "http://127.0.0.1:9", proxy);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handle = http::server(router)
        .policy(ServerPolicy::default().request_body_hard_cap(4).unwrap())
        .serve_background(listener)
        .unwrap();
    let report = handle.effective_policies();
    assert!(
        report
            .routes()
            .iter()
            .all(|route| route.request_body_limit().unwrap().value() == 4)
    );
    let narrowed = (
        PolicyLayer::Route,
        PolicyDimension::RequestBodyLimit,
        32,
        4,
        vec![(PolicyLayer::Server, false)],
    );
    assert_diagnostics(report, &vec![narrowed; report.routes().len()]);
    handle.shutdown_and_join().await.unwrap();
}

#[test]
fn cap_refuses_streaming_consumers_before_polling() {
    common::test_runtime()
        .run(|| {
            let mut router = Router::new();
            router.proxy("/buffered", "http://127.0.0.1:9");
            router.proxy_stream("/streamed", "http://127.0.0.1:9");
            router.multipart(
                http::Method::Post,
                "/multipart",
                http::MultipartLimits::default(),
                |_, _| async { Response::text(200, "unreachable") },
            );
            let server = wire::reserve_request_body_owner().serve_with_policy(
                router,
                ServerPolicy::default().request_body_hard_cap(4).unwrap(),
            );
            for path in ["/buffered", "/streamed", "/multipart"] {
                let response = wire::send(
                    server.addr(),
                    "POST",
                    path,
                    &[("Content-Type", "multipart/form-data; boundary=cap")],
                    b"12345",
                );
                assert_eq!(response.status, 413, "{path}");
                assert_eq!(server.controller().observed().frames_polled, 0, "{path}");
            }
            runtime::request_shutdown();
        })
        .unwrap();
}

#[tokio::test]
async fn cap_validation_defaults_and_requested_router_limits() {
    assert!(ServerPolicy::default().request_body_hard_cap(0).is_err());
    assert!(
        ServerPolicy::default()
            .request_body_hard_cap(usize::MAX)
            .is_ok()
    );
    for (requested, cap, expected) in [
        (None, None, 8 * MIB),
        (None, Some(512 * MIB), 8 * MIB),
        (Some(400 * MIB), None, 256 * MIB),
        (Some(400 * MIB), Some(512 * MIB), 400 * MIB),
        (Some(0), Some(512 * MIB), 0),
    ] {
        assert_report_row(requested, cap, expected).await;
    }
}

async fn assert_report_row(requested: Option<usize>, cap: Option<usize>, expected: usize) {
    let router = match requested {
        Some(bytes) => echo_router().max_request_body(bytes),
        None => echo_router(),
    };
    let policy = match cap {
        Some(bytes) => ServerPolicy::default()
            .request_body_hard_cap(bytes)
            .unwrap(),
        None => ServerPolicy::default(),
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handle = http::server(router)
        .policy(policy)
        .serve_background(listener)
        .unwrap();
    let report = handle.effective_policies();
    assert_eq!(
        report.server().request_body_hard_cap().value(),
        cap.unwrap_or(256 * MIB)
    );
    assert_eq!(
        report.routes()[0].request_body_limit().unwrap().value(),
        expected
    );
    assert_eq!(
        report.routes()[0].configured_request_body_limit(),
        requested
    );
    // Only the default cap narrowing an explicit 400 MiB limit is diagnosed.
    let narrowed: Vec<Expected> = match (requested, cap) {
        (Some(bytes), None) if bytes > expected => vec![(
            PolicyLayer::Router,
            PolicyDimension::RequestBodyLimit,
            bytes,
            expected,
            vec![(PolicyLayer::Server, true)],
        )],
        _ => Vec::new(),
    };
    assert_diagnostics(report, &narrowed);
    handle.shutdown_and_join().await.unwrap();
}

#[test]
fn runtime_cap_contains_server_host_and_router_and_retains_ties() {
    common::test_runtime()
        .server_policy(ServerPolicy::default().request_body_hard_cap(16).unwrap())
        .run(|| runtime::block_on(assert_nested_cap()))
        .unwrap();
}

async fn assert_nested_cap() {
    let mut hosts = HostRouter::new().max_request_body(16);
    hosts.set_default(echo_router().max_request_body(64));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handle = http::server_hosts(hosts)
        .policy(ServerPolicy::default().request_body_hard_cap(32).unwrap())
        .serve_background(listener)
        .unwrap();
    let report = handle.effective_policies();
    let limit = report.routes()[0].request_body_limit().unwrap();
    assert_eq!(limit.value(), 16);
    assert_eq!(
        layers(limit.sources()),
        [(PolicyLayer::Runtime, false), (PolicyLayer::Host, false)]
    );
    assert_diagnostics(
        report,
        &[
            (
                PolicyLayer::Server,
                PolicyDimension::RequestBodyHardCap,
                32,
                16,
                vec![(PolicyLayer::Runtime, false)],
            ),
            (
                PolicyLayer::Router,
                PolicyDimension::RequestBodyLimit,
                64,
                16,
                vec![(PolicyLayer::Runtime, false), (PolicyLayer::Host, false)],
            ),
        ],
    );
    handle.shutdown_and_join().await.unwrap();
    runtime::request_shutdown();
}

#[test]
fn raised_cap_admits_large_declaration_without_allocating_its_payload() {
    common::test_runtime()
        .server_policy(
            ServerPolicy::default()
                .request_body_hard_cap(512 * MIB)
                .unwrap(),
        )
        .run(|| {
            let called = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&called);
            let router = echo_router()
                .max_request_body(400 * MIB)
                .body_admission(move |context| {
                    assert_eq!(context.declared_length(), Some((300 * MIB) as u64));
                    observed.fetch_add(1, Ordering::SeqCst);
                    Err(RuntimeError::InvalidArgument("application refusal".into()))
                });
            let server = wire::reserve_request_body_owner()
                .serve_with_policy(router, ServerPolicy::default());
            assert_eq!(
                declared_status(server.addr(), 300 * MIB),
                APPLICATION_REFUSAL
            );
            assert_eq!(called.load(Ordering::SeqCst), 1);
            assert_eq!(server.controller().observed().frames_polled, 0);
            runtime::request_shutdown();
        })
        .unwrap();
}

/// Each body consumer stops reading at the frame that crosses the cap.
#[test]
fn cap_refuses_declarations_and_stops_every_consumer_at_the_crossing_frame() {
    common::test_runtime()
        .run(|| {
            let upstream = wire::spawn_server_ready(echo_router(), Duration::from_secs(5)).unwrap();
            let called = Arc::new(AtomicUsize::new(0));
            let observed = Arc::clone(&called);
            let mut router = echo_router().max_request_body(64).body_admission(move |_| {
                observed.fetch_add(1, Ordering::SeqCst);
                Ok(BodyAdmission::new(64))
            });
            router.multipart(
                http::Method::Post,
                "/multipart",
                http::MultipartLimits::default(),
                |_, mut stream| async move {
                    let count = drain_fields(&mut stream).await?;
                    Response::text(200, &count.to_string())
                },
            );
            router.proxy_stream("/streamed", &format!("http://{}", upstream.local_addr()));
            let server = wire::reserve_multipart_body().serve_with_policy(
                router,
                ServerPolicy::default().request_body_hard_cap(4).unwrap(),
            );
            // Admission counts buffered and proxied frames; a multipart session
            // counts the frames it parses.
            let polled = || {
                let owners = server.controller();
                owners.bodies.observed().frames_polled
                    + owners.sessions.observed().body_frames_polled()
            };

            assert_eq!(declared_status(server.addr(), 5), 413);
            assert_eq!(called.load(Ordering::SeqCst), 0);
            assert_eq!(polled(), 0);

            let crossing: &[&[u8]] = &[b"1234", b"5", b"6"];
            let multipart = content_type(BOUNDARY);
            for (path, headers, label) in [
                ("/body", vec![], "buffered"),
                (
                    "/multipart",
                    vec![("Content-Type", &*multipart)],
                    "multipart",
                ),
                ("/streamed/body", vec![], "streaming proxy"),
            ] {
                let before = polled();
                let status = chunked_status(server.addr(), path, &headers, crossing);
                assert_eq!(status, 413, "{label}");
                assert_eq!(
                    polled() - before,
                    2,
                    "{label}: stopped at the crossing frame"
                );
            }
            // Admission ran once for each chunked request, and never for the
            // declaration the cap refused.
            assert_eq!(called.load(Ordering::SeqCst), 3);
            runtime::request_shutdown();
        })
        .unwrap();
}

/// A streaming proxy reports the payload limit body admission enforces. The
/// router's upload maximum is not one: admission counts these bytes.
#[test]
fn streaming_proxy_report_matches_the_payload_bytes_admission_enforces() {
    common::test_runtime()
        .run(|| {
            let upstream = wire::spawn_server_ready(echo_router(), Duration::from_secs(5)).unwrap();
            let backend = format!("http://{}", upstream.local_addr());
            let one_byte = http::TransferBudget::unbounded().with_max_bytes(1).unwrap();
            let mut router = Router::new().upload_budget(one_byte);
            router.proxy_stream("/router", &backend);
            router.proxy_stream_with_policy(
                "/route",
                &backend,
                http::ProxyPolicy::default().upload_budget(one_byte),
            );
            let server = wire::reserve_request_body_owner().serve(router);
            let address = server.addr();
            let handle = server.into_handle();
            let limit = |prefix: &str| {
                let route = handle
                    .effective_policies()
                    .routes()
                    .iter()
                    .find(|route| route.route() == prefix)
                    .unwrap();
                (
                    route.upload_budget().max_bytes().value(),
                    route.request_body_limit().unwrap().value(),
                )
            };
            assert_eq!(limit("/router"), (None, 8 * MIB));
            assert_eq!(limit("/route"), (None, 1));
            let send = |path| wire::send(address, "POST", path, &[], b"12").status;
            assert_eq!(send("/router/body"), 200);
            assert_eq!(send("/route/body"), 413);
            handle.shutdown();
            runtime::request_shutdown();
        })
        .unwrap();
}

// ── The intent rule at the runtime → server cap edge ───────────────────────

fn cap_runtime(cap: Option<usize>) -> RuntimeBuilder {
    match cap {
        Some(bytes) => runtime::builder().server_policy(
            ServerPolicy::default()
                .request_body_hard_cap(bytes)
                .unwrap(),
        ),
        None => runtime::builder(),
    }
}

fn cap_policy(cap: Option<usize>) -> ServerPolicy {
    match cap {
        Some(bytes) => ServerPolicy::default()
            .request_body_hard_cap(bytes)
            .unwrap(),
        None => ServerPolicy::default(),
    }
}

/// Serve a 1 GiB router limit and declare a 300 MiB body.
///
/// Returns the report, the answer status, and how many times admission ran.
fn cap_row(
    runtime_cap: Option<usize>,
    server_cap: Option<usize>,
) -> (EffectivePolicies, u16, usize) {
    cap_runtime(runtime_cap)
        .run(|| {
            let called = Arc::new(AtomicUsize::new(0));
            let server = wire::reserve_request_body_owner().serve_with_policy(
                refusing_router(&called).max_request_body(GIB),
                cap_policy(server_cap),
            );
            let status = declared_status(server.addr(), 300 * MIB);
            assert_eq!(server.controller().observed().frames_polled, 0);
            let handle = server.into_handle();
            let report = handle.effective_policies().clone();
            handle.shutdown();
            runtime::request_shutdown();
            (report, status, called.load(Ordering::SeqCst))
        })
        .unwrap()
}

#[test]
fn cap_matrix_follows_the_intent_rule() {
    let router_narrowed = |effective, sources| {
        (
            PolicyLayer::Router,
            PolicyDimension::RequestBodyLimit,
            GIB,
            effective,
            sources,
        )
    };

    // Explicit both: the narrower runtime cap applies.
    let (report, status, called) = cap_row(Some(16), Some(32));
    let cap = report.server().request_body_hard_cap();
    assert_eq!(cap.value(), 16);
    assert_eq!(layers(cap.sources()), [(PolicyLayer::Runtime, false)]);
    assert_diagnostics(
        &report,
        &[
            (
                PolicyLayer::Server,
                PolicyDimension::RequestBodyHardCap,
                32,
                16,
                vec![(PolicyLayer::Runtime, false)],
            ),
            router_narrowed(16, vec![(PolicyLayer::Runtime, false)]),
        ],
    );
    assert_eq!((status, called), (413, 0));

    // Default runtime, explicit server: the #52 example raises the cap.
    let (report, status, called) = cap_row(None, Some(GIB));
    let cap = report.server().request_body_hard_cap();
    assert_eq!(cap.value(), GIB);
    assert_eq!(layers(cap.sources()), [(PolicyLayer::Server, false)]);
    assert_diagnostics(&report, &[]);
    assert_eq!((status, called), (APPLICATION_REFUSAL, 1));

    // Explicit runtime, default server: the default cap yields.
    let (report, status, called) = cap_row(Some(512 * MIB), None);
    let cap = report.server().request_body_hard_cap();
    assert_eq!(cap.value(), 512 * MIB);
    assert_eq!(layers(cap.sources()), [(PolicyLayer::Runtime, false)]);
    assert_diagnostics(
        &report,
        &[router_narrowed(
            512 * MIB,
            vec![(PolicyLayer::Runtime, false)],
        )],
    );
    assert_eq!((status, called), (APPLICATION_REFUSAL, 1));

    // Default both: the default cap still contains the explicit router limit,
    // and the diagnostic says so.
    let (report, status, called) = cap_row(None, None);
    let defaults = vec![(PolicyLayer::Runtime, true), (PolicyLayer::Server, true)];
    let cap = report.server().request_body_hard_cap();
    assert_eq!(cap.value(), 256 * MIB);
    assert_eq!(layers(cap.sources()), defaults);
    assert_diagnostics(&report, &[router_narrowed(256 * MIB, defaults.clone())]);
    assert_eq!((status, called), (413, 0));
}

// ── The intent rule at the host → router request-body edge ─────────────────

/// Serve one child under a host router and declare `declared` bytes.
fn host_row(
    host: Option<usize>,
    child: Option<usize>,
    declared: usize,
) -> (EffectivePolicies, u16, usize) {
    common::test_runtime()
        .run(|| {
            let called = Arc::new(AtomicUsize::new(0));
            let router = refusing_router(&called);
            let router = match child {
                Some(bytes) => router.max_request_body(bytes),
                None => router,
            };
            let mut hosts = match host {
                Some(bytes) => HostRouter::new().max_request_body(bytes),
                None => HostRouter::new(),
            };
            hosts.set_default(router);
            let server = wire::reserve_request_body_owner()
                .serve_hosts_with_policy(hosts, ServerPolicy::default());
            let status = declared_status(server.addr(), declared);
            let handle = server.into_handle();
            let report = handle.effective_policies().clone();
            handle.shutdown();
            runtime::request_shutdown();
            (report, status, called.load(Ordering::SeqCst))
        })
        .unwrap()
}

#[test]
fn host_and_router_body_limits_follow_the_intent_rule() {
    let limit = |report: &EffectivePolicies| {
        let bound = report.routes()[0].request_body_limit().unwrap();
        (bound.value(), layers(bound.sources()))
    };

    // Explicit both: the narrower host limit applies.
    let (report, status, called) = host_row(Some(4 * MIB), Some(16 * MIB), 6 * MIB);
    assert_eq!(limit(&report), (4 * MIB, vec![(PolicyLayer::Host, false)]));
    assert_diagnostics(
        &report,
        &[(
            PolicyLayer::Router,
            PolicyDimension::RequestBodyLimit,
            16 * MIB,
            4 * MIB,
            vec![(PolicyLayer::Host, false)],
        )],
    );
    assert_eq!((status, called), (413, 0));

    // Default host, explicit child: the host's 8 MiB default yields.
    let (report, status, called) = host_row(None, Some(16 * MIB), 12 * MIB);
    assert_eq!(
        limit(&report),
        (16 * MIB, vec![(PolicyLayer::Router, false)])
    );
    assert_diagnostics(&report, &[]);
    assert_eq!((status, called), (APPLICATION_REFUSAL, 1));

    // Explicit host, unset child: the child takes the host's limit.
    let (report, status, called) = host_row(Some(16 * MIB), None, 12 * MIB);
    assert_eq!(limit(&report), (16 * MIB, vec![(PolicyLayer::Host, false)]));
    assert_diagnostics(&report, &[]);
    assert_eq!((status, called), (APPLICATION_REFUSAL, 1));

    // Default both: the 8 MiB default applies.
    let (report, status, called) = host_row(None, None, 12 * MIB);
    assert_eq!(limit(&report), (8 * MIB, vec![(PolicyLayer::Host, true)]));
    assert_diagnostics(&report, &[]);
    assert_eq!((status, called), (413, 0));
}
