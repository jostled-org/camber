//! Issue 53: effective policy inspection, the intent rule, and diagnostics.
//!
//! A defaulted value never narrows an explicit setting. Each case below reads
//! the frozen report and observes the bound production enforces.

use crate::trace_capture::capture_events;
use camber::http::{
    self, BufferedResponseLimit, EffectivePolicies, HealthRoute, HostRouter, PolicyDimension,
    PolicyLayer, PolicyLayers, PolicyValue, RequestBudget, Response, Router, ServerBuilder,
    ServerHandleFuture, ServerPolicy,
};
use camber::runtime::{self, RuntimeBuilder};
use std::future::Future;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// Past every documented thirty-second default.
const PAST_DEFAULT: Duration = Duration::from_secs(31);
/// How long a live step may take before the case fails.
const LIVE_BOUND: Duration = Duration::from_secs(5);
/// An explicit bound shorter than [`PAST_DEFAULT`], for negative controls.
const NARROW: Duration = Duration::from_secs(10);
const DEFAULT_DEADLINE: Duration = Duration::from_secs(30);
const LONG_DRAIN: Duration = Duration::from_secs(120);

fn router() -> Router {
    let mut routes = Router::new();
    routes.get("/reports/:id", |_| async { Response::text(200, "ready") });
    routes
}

/// One expected diagnostic: layer, dimension, configured, effective, and the
/// limiting layers with whether each used its default.
type Expected = (
    PolicyLayer,
    PolicyDimension,
    PolicyValue,
    PolicyValue,
    Vec<(PolicyLayer, bool)>,
);

/// Assert the report carries exactly `expected`, in resolution order.
fn assert_diagnostics(report: &EffectivePolicies, expected: &[Expected]) {
    let actual: Vec<Expected> = report
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

fn layers(sources: PolicyLayers) -> Vec<(PolicyLayer, bool)> {
    sources
        .iter()
        .map(|source| (source.layer(), source.is_default()))
        .collect()
}

fn seconds(value: Duration) -> PolicyValue {
    PolicyValue::Duration(Some(value))
}

/// Serve under a Camber runtime, on an executor whose clock this case controls.
///
/// Tokio pauses only a current-thread clock, and `RuntimeBuilder::run` drives a
/// multi-thread executor. `serve_async` captures the runtime and its policy at
/// the call. The returned future, and every connection task it spawns, then
/// runs on the executor that polls it.
fn serve_on_paused_clock<F, Fut>(runtime: RuntimeBuilder, server: ServerBuilder, journey: F)
where
    F: FnOnce(ServerHandleFuture, SocketAddr) -> Fut + Send + 'static,
    Fut: Future<Output = ()>,
{
    runtime
        .run(move || {
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let (served, address) = {
                let _context = executor.enter();
                let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
                listener.set_nonblocking(true).unwrap();
                let address = listener.local_addr().unwrap();
                let listener = tokio::net::TcpListener::from_std(listener).unwrap();
                (server.serve_async(listener).unwrap(), address)
            };
            std::thread::spawn(move || executor.block_on(journey(served, address)))
                .join()
                .unwrap();
        })
        .unwrap();
}

/// The two signals a held route raises and waits on.
struct Held {
    entered: Arc<tokio::sync::Notify>,
    released: Arc<tokio::sync::Notify>,
}

/// Add `/slow`, which holds until released, and `/stop`, which stops the runtime.
fn held(mut routes: Router) -> (Router, Held) {
    let held = Held {
        entered: Arc::new(tokio::sync::Notify::new()),
        released: Arc::new(tokio::sync::Notify::new()),
    };
    let entered = Arc::clone(&held.entered);
    let released = Arc::clone(&held.released);
    routes.get("/slow", move |_| {
        let entered = Arc::clone(&entered);
        let released = Arc::clone(&released);
        async move {
            entered.notify_one();
            released.notified().await;
            Response::text(200, "held past the default")
        }
    });
    routes.get("/stop", |_| async {
        runtime::request_shutdown();
        Response::text(200, "stopping")
    });
    (routes, held)
}

async fn advance_past_default() {
    tokio::time::pause();
    tokio::time::advance(PAST_DEFAULT).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
}

/// Hold one request across [`PAST_DEFAULT`], optionally while the server drains.
async fn hold_past_default(
    address: SocketAddr,
    held: &Held,
    drain: bool,
) -> Result<reqwest::Response, reqwest::Error> {
    let response = tokio::spawn(async move {
        reqwest::Client::new()
            .get(format!("http://{address}/slow"))
            .send()
            .await
    });
    tokio::time::timeout(LIVE_BOUND, held.entered.notified())
        .await
        .unwrap();
    if drain {
        stop_admission(address).await;
    }
    advance_past_default().await;
    held.released.notify_one();
    tokio::time::timeout(LIVE_BOUND, response)
        .await
        .unwrap()
        .unwrap()
}

/// Stop the runtime, and wait until the server stops admitting connections.
async fn stop_admission(address: SocketAddr) {
    let stopped = reqwest::get(format!("http://{address}/stop"))
        .await
        .unwrap();
    assert_eq!(stopped.status(), 200);
    tokio::time::timeout(LIVE_BOUND, async {
        while tokio::net::TcpStream::connect(address).await.is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}

/// Hold a request across the default deadline and return its status.
fn held_status(runtime: RuntimeBuilder, policy: ServerPolicy, routes: Router) -> u16 {
    let (routes, held) = held(routes);
    let status = Arc::new(Mutex::new(None));
    let observed = Arc::clone(&status);
    serve_on_paused_clock(
        runtime,
        http::server(routes).policy(policy),
        move |served, address| async move {
            let server = tokio::spawn(served);
            let response = hold_past_default(address, &held, false).await.unwrap();
            *observed.lock().unwrap() = Some(response.status().as_u16());
            stop_admission(address).await;
            server.await.unwrap().unwrap();
        },
    );
    status.lock().unwrap().take().unwrap()
}

/// Resolve `server` under `runtime` without serving it.
fn resolved_under(runtime: RuntimeBuilder, server: ServerBuilder) -> EffectivePolicies {
    runtime.run(|| runtime::block_on(resolved(server))).unwrap()
}

/// Resolve `server` under whatever runtime is current, without serving it.
async fn resolved(server: ServerBuilder) -> EffectivePolicies {
    let captured = Arc::new(Mutex::new(None));
    let capture = Arc::clone(&captured);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let refused = server
        .on_policy_resolved(move |report| {
            *capture.lock().unwrap() = Some(report.clone());
            Err(camber::RuntimeError::InvalidArgument("inspect only".into()))
        })
        .serve_background(listener);
    assert!(refused.is_err());
    captured.lock().unwrap().take().unwrap()
}

// ── #53 verification bullet 1 ──────────────────────────────────────────────

/// A default runtime and server yield to an explicitly unbounded router total.
#[test]
fn explicit_router_total_outlives_default_runtime_and_server() {
    let unbounded = || router().request_budget(RequestBudget::unbounded());
    let report = resolved_under(runtime::builder(), http::server(unbounded()));
    let total = report.routes()[0].request_budget().total();
    assert_eq!(total.value(), None);
    assert_eq!(layers(total.sources()), [(PolicyLayer::Router, false)]);
    assert_diagnostics(&report, &[]);
    assert_eq!(
        held_status(runtime::builder(), ServerPolicy::default(), unbounded()),
        200
    );

    // Negative control: a narrower explicit router total still times out.
    let narrow = RequestBudget::unbounded().with_total(NARROW).unwrap();
    assert_eq!(
        held_status(
            runtime::builder(),
            ServerPolicy::default(),
            Router::new().request_budget(narrow)
        ),
        408
    );
}

// ── #53 verification bullet 2 ──────────────────────────────────────────────

/// An explicitly unbounded total at every containing layer serves past the
/// default deadline. A narrower explicit router total is the negative control.
#[test]
fn explicit_unbounded_total_at_every_layer_serves_past_the_default() {
    let unbounded = ServerPolicy::default().request_budget(RequestBudget::unbounded());
    let runtime = || runtime::builder().server_policy(unbounded);
    let report = resolved_under(runtime(), http::server(router()).policy(unbounded));
    let total = report.routes()[0].request_budget().total();
    assert_eq!(total.value(), None);
    assert_eq!(
        layers(total.sources()),
        [(PolicyLayer::Runtime, false), (PolicyLayer::Server, false)]
    );
    assert_eq!(held_status(runtime(), unbounded, Router::new()), 200);

    let narrow = RequestBudget::unbounded().with_total(NARROW).unwrap();
    assert_eq!(
        held_status(runtime(), unbounded, Router::new().request_budget(narrow)),
        408
    );
}

// ── #53 verification bullet 3 ──────────────────────────────────────────────

/// Send a declared body, stall it past the default idle deadline, then finish.
async fn stall_body_past_default(address: SocketAddr, held: &Held) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut peer = tokio::net::TcpStream::connect(address).await.unwrap();
    peer.write_all(
        b"POST /upload HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: 5\r\n\r\n",
    )
    .await
    .unwrap();
    tokio::time::timeout(LIVE_BOUND, held.entered.notified())
        .await
        .unwrap();
    advance_past_default().await;
    let _ = peer.write_all(b"hello").await;
    let mut answer = Vec::new();
    tokio::time::timeout(LIVE_BOUND, peer.read_to_end(&mut answer))
        .await
        .unwrap()
        .unwrap();
    String::from_utf8_lossy(&answer).into_owned()
}

/// An upload route whose admission callback signals that body reading starts.
fn idle_routes(budget: RequestBudget) -> (Router, Held) {
    let (routes, held) = held(Router::new());
    let entered = Arc::clone(&held.entered);
    let mut routes = routes.request_budget(budget).body_admission(move |_| {
        entered.notify_one();
        Ok(http::BodyAdmission::new(64))
    });
    routes.post("/upload", |request| {
        let length = request.body().len();
        async move { Response::text(200, &length.to_string()) }
    });
    (routes, held)
}

/// Stall one upload past the default idle deadline and return the answer.
fn stalled_answer(budget: RequestBudget) -> (EffectivePolicies, String) {
    let (routes, held) = idle_routes(budget);
    let outcome = Arc::new(Mutex::new(None));
    let observed = Arc::clone(&outcome);
    serve_on_paused_clock(
        runtime::builder(),
        http::server(routes),
        move |served, address| async move {
            let report = served.effective_policies().clone();
            let server = tokio::spawn(served);
            let answer = stall_body_past_default(address, &held).await;
            *observed.lock().unwrap() = Some((report, answer));
            stop_admission(address).await;
            server.await.unwrap().unwrap();
        },
    );
    outcome.lock().unwrap().take().unwrap()
}

/// A default runtime and server yield to a 60-second router body idle.
#[test]
fn explicit_router_body_idle_outlives_default_runtime_and_server() {
    let budget = RequestBudget::unbounded()
        .with_body_idle(Duration::from_secs(60))
        .unwrap();
    let (report, answer) = stalled_answer(budget);
    assert!(answer.starts_with("HTTP/1.1 200"), "{answer}");
    let idle = report
        .routes()
        .iter()
        .find(|route| route.route() == "/upload")
        .unwrap()
        .request_budget()
        .body_idle();
    assert_eq!(idle.value(), Some(Duration::from_secs(60)));
    assert_eq!(layers(idle.sources()), [(PolicyLayer::Router, false)]);
    assert_diagnostics(&report, &[]);

    // Negative control: a narrower explicit router idle still times out.
    let narrow = RequestBudget::unbounded().with_body_idle(NARROW).unwrap();
    let (_, answer) = stalled_answer(narrow);
    assert!(answer.starts_with("HTTP/1.1 408"), "{answer}");
    assert!(answer.ends_with("request body timed out"), "{answer}");
}

// ── Shutdown: the runtime deadline is a hard bound ─────────────────────────

/// Hold one request while the runtime stops, and report whether it was answered.
fn drained_status(
    runtime: RuntimeBuilder,
    policy: ServerPolicy,
) -> (EffectivePolicies, Option<u16>) {
    let (routes, held) = held(Router::new().request_budget(RequestBudget::unbounded()));
    let outcome = Arc::new(Mutex::new(None));
    let observed = Arc::clone(&outcome);
    serve_on_paused_clock(
        runtime,
        http::server(routes).policy(policy),
        move |served, address| async move {
            let report = served.effective_policies().clone();
            let server = tokio::spawn(served);
            let response = hold_past_default(address, &held, true).await;
            let status = response.ok().map(|response| response.status().as_u16());
            *observed.lock().unwrap() = Some((report, status));
            let _ = server.await.unwrap();
        },
    );
    outcome.lock().unwrap().take().unwrap()
}

/// Explicit or default, runtime and server, for the drain deadline.
#[test]
fn drain_deadline_matrix_reports_what_the_runtime_enforces() {
    let explicit_runtime = || runtime::builder().shutdown_timeout(LONG_DRAIN);
    let explicit_server = |drain| ServerPolicy::default().shutdown_timeout(drain).unwrap();

    // Explicit runtime, default server: the default drain yields (#53).
    let (report, status) = drained_status(explicit_runtime(), ServerPolicy::default());
    let drain = report.server().shutdown_timeout();
    assert_eq!(drain.value(), LONG_DRAIN);
    assert_eq!(layers(drain.sources()), [(PolicyLayer::Runtime, false)]);
    assert_eq!(report.server().runtime_shutdown_timeout(), Some(LONG_DRAIN));
    assert_eq!(report.server().local_shutdown_timeout(), DEFAULT_DEADLINE);
    assert_diagnostics(&report, &[]);
    assert_eq!(status, Some(200));

    // Explicit both: the narrower server drain applies.
    let (report, status) = drained_status(explicit_runtime(), explicit_server(NARROW));
    assert_eq!(report.server().shutdown_timeout().value(), NARROW);
    assert_diagnostics(&report, &[]);
    assert_eq!(status, None);

    // Default runtime, explicit server: the runtime deadline still applies.
    let (report, status) = drained_status(runtime::builder(), explicit_server(LONG_DRAIN));
    let drain = report.server().shutdown_timeout();
    assert_eq!(drain.value(), DEFAULT_DEADLINE);
    assert_eq!(layers(drain.sources()), [(PolicyLayer::Runtime, true)]);
    assert_diagnostics(
        &report,
        &[(
            PolicyLayer::Server,
            PolicyDimension::ShutdownTimeout,
            seconds(LONG_DRAIN),
            seconds(DEFAULT_DEADLINE),
            vec![(PolicyLayer::Runtime, true)],
        )],
    );
    assert_eq!(status, None);

    // Default both: the shared default applies.
    let (report, status) = drained_status(runtime::builder(), ServerPolicy::default());
    let drain = report.server().shutdown_timeout();
    assert_eq!(drain.value(), DEFAULT_DEADLINE);
    assert_eq!(
        layers(drain.sources()),
        [(PolicyLayer::Runtime, true), (PolicyLayer::Server, true)]
    );
    assert_eq!(status, None);
}

/// #53 verification bullet 4: a longer runtime shutdown and a shorter server
/// drain expose both values and the effective server bound.
#[test]
fn longer_runtime_shutdown_and_shorter_drain_expose_both() {
    let report = resolved_under(
        runtime::builder().shutdown_timeout(Duration::from_secs(90)),
        http::server(router()).policy(
            ServerPolicy::default()
                .shutdown_timeout(Duration::from_secs(45))
                .unwrap(),
        ),
    );
    let server = report.server();
    assert_eq!(
        server.runtime_shutdown_timeout(),
        Some(Duration::from_secs(90))
    );
    assert_eq!(server.local_shutdown_timeout(), Duration::from_secs(45));
    assert_eq!(server.shutdown_timeout().value(), Duration::from_secs(45));
    assert_eq!(
        layers(server.shutdown_timeout().sources()),
        [(PolicyLayer::Server, false)]
    );
}

// ── #53 verification bullet 5 and the standalone report ────────────────────

#[tokio::test]
async fn standalone_policy_report_is_frozen_before_accept_and_survives_join_conversion() {
    let observed = Arc::new(Mutex::new(None));
    let capture = Arc::clone(&observed);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let handle = http::server(router())
        .on_policy_resolved(move |report| {
            *capture.lock().unwrap() = Some(report.clone());
            Ok(())
        })
        .serve_background(listener)
        .unwrap();
    let report = observed.lock().unwrap().take().unwrap();
    assert_eq!(report, *handle.effective_policies());
    let server = report.server();
    let standalone = [(PolicyLayer::Server, true)];
    assert_eq!(
        server.request_budget().total().value(),
        Some(DEFAULT_DEADLINE)
    );
    assert_eq!(
        server.request_budget().body_idle().value(),
        Some(DEFAULT_DEADLINE)
    );
    assert_eq!(
        layers(server.request_budget().total().sources()),
        standalone
    );
    assert_eq!(server.header_timeout().value(), Duration::from_secs(60));
    assert_eq!(server.shutdown_timeout().value(), DEFAULT_DEADLINE);
    assert_eq!(server.request_body_hard_cap().value(), 256 * 1024 * 1024);
    assert_eq!(server.connection_limit().value(), None);
    assert_eq!(server.runtime_shutdown_timeout(), None);
    assert_diagnostics(&report, &[]);
    assert_eq!(report.routes().len(), 1);
    assert_eq!(report.routes()[0].route(), "/reports/:id");
    assert_eq!(report.routes()[0].method(), http::Method::Get);
    let future = handle.shutdown_and_join();
    assert_eq!(report, *future.effective_policies());
    future.await.unwrap();
}

/// `serve(addr)` builds a runtime from the server's own policy. That runtime is
/// not a layer the caller configured, so the report does not name it.
#[test]
fn serve_reports_its_own_runtime_as_no_policy_layer() {
    let observed = Arc::new(Mutex::new(None));
    let capture = Arc::clone(&observed);
    let refused = std::thread::spawn(move || {
        http::server(router())
            .on_policy_resolved(move |report| {
                *capture.lock().unwrap() = Some(report.clone());
                Err(camber::RuntimeError::InvalidArgument("inspect only".into()))
            })
            .serve("127.0.0.1:0")
    })
    .join()
    .unwrap();
    assert!(matches!(
        refused,
        Err(camber::RuntimeError::InvalidArgument(_))
    ));
    let report = observed.lock().unwrap().take().unwrap();
    let server = report.server();
    assert_eq!(server.runtime_shutdown_timeout(), None);
    assert_eq!(
        layers(server.shutdown_timeout().sources()),
        [(PolicyLayer::Server, true)]
    );
    assert_eq!(
        layers(report.routes()[0].request_budget().total().sources()),
        [(PolicyLayer::Server, true)]
    );
}

// ── Startup refusal through every owned terminal ───────────────────────────

fn refusing(server: ServerBuilder) -> ServerBuilder {
    server.on_policy_resolved(|_| {
        Err(camber::RuntimeError::InvalidArgument(
            "policy assertion".into(),
        ))
    })
}

#[tokio::test]
async fn startup_policy_refusal_prevents_background_and_async_serving() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let result = refusing(http::server(router())).serve_background(listener);
    assert!(matches!(
        result,
        Err(camber::RuntimeError::InvalidArgument(_))
    ));
    assert!(tokio::net::TcpStream::connect(address).await.is_err());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let result = refusing(http::server(router())).serve_async(listener);
    assert!(matches!(
        result,
        Err(camber::RuntimeError::InvalidArgument(_))
    ));
    assert!(tokio::net::TcpStream::connect(address).await.is_err());
}

#[test]
fn startup_policy_refusal_prevents_listener_serving() {
    runtime::builder()
        .run(|| {
            let listener = camber::net::listen("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap().tcp().unwrap();
            let result = refusing(http::server(router())).serve_listener(listener);
            assert!(matches!(
                result,
                Err(camber::RuntimeError::InvalidArgument(_))
            ));
            assert!(std::net::TcpStream::connect(address).is_err());
        })
        .unwrap();
}

// ── Reports and diagnostics ────────────────────────────────────────────────

#[tokio::test]
async fn omitted_specialized_policies_do_not_emit_explicit_narrowing() {
    let mut routes = Router::new();
    routes.get_sse("/events", |_, _| Ok(()));
    routes.proxy_stream("/stream", "http://127.0.0.1:9");
    let policy = ServerPolicy::default().download_budget(
        http::TransferBudget::unbounded()
            .with_max_bytes(64)
            .unwrap(),
    );
    let report = resolved(http::server(routes).policy(policy)).await;
    assert_diagnostics(&report, &[]);
}

#[tokio::test]
async fn proxy_reports_name_their_actual_response_limits() {
    let mut routes = Router::new();
    let policy = http::ProxyPolicy::default()
        .buffered_response_limit(512)
        .unwrap()
        .upstream_idle_timeout(Duration::from_secs(9))
        .unwrap()
        .download_budget(http::TransferBudget::unbounded().with_max_bytes(1).unwrap());
    routes.proxy_with_policy("/buffered", "http://127.0.0.1:9", policy);
    routes.proxy_with_policy(
        "/unbounded",
        "http://127.0.0.1:9",
        http::ProxyPolicy::default().unbounded_buffered_response(),
    );
    routes.proxy_stream("/streamed", "http://127.0.0.1:9");
    let report = resolved(http::server(routes)).await;
    let limit = |prefix: &str| {
        report
            .routes()
            .iter()
            .find(|route| route.route() == prefix)
            .unwrap()
            .proxy()
            .unwrap()
            .buffered_response_limit()
    };
    assert_eq!(limit("/buffered"), BufferedResponseLimit::Bytes(512));
    assert_eq!(limit("/unbounded"), BufferedResponseLimit::Unbounded);
    assert_eq!(limit("/streamed"), BufferedResponseLimit::Streamed);
    let buffered = &report.routes()[0];
    assert_eq!(buffered.download_budget().max_bytes().value(), None);
    assert_eq!(
        buffered.proxy().unwrap().upstream_idle_timeout(),
        Duration::from_secs(9)
    );
}

/// An explicit outer value narrows an explicit inner one, and the warning
/// names each part of that narrowing.
#[test]
fn explicit_narrowing_reports_and_logs_every_part() {
    let capture = capture_events("name=camber.http.policy.narrowed");
    let server = ServerPolicy::default()
        .request_budget(RequestBudget::unbounded().with_total(NARROW).unwrap());
    let report = resolved_under(
        runtime::builder(),
        http::server(router().request_budget(RequestBudget::unbounded())).policy(server),
    );
    assert_diagnostics(
        &report,
        &[(
            PolicyLayer::Router,
            PolicyDimension::RequestTotal,
            PolicyValue::Duration(None),
            seconds(NARROW),
            vec![(PolicyLayer::Server, false)],
        )],
    );
    assert!(capture.recorded(&[
        "name=camber.http.policy.narrowed",
        "layer=router",
        "dimension=request.total",
        "configured=unbounded",
        "effective=10s",
        "limiting_layers=server",
    ]));
}

#[test]
fn host_and_router_sources_survive_exact_route_enumeration() {
    let outer = ServerPolicy::default().request_budget(RequestBudget::unbounded());
    let budget = RequestBudget::bounded(Duration::from_secs(7), Duration::from_secs(11)).unwrap();
    let mut hosts = HostRouter::new().request_budget(budget);
    hosts.add("reports.example", router().request_budget(budget));
    hosts.set_default(router());
    let report = resolved_under(
        runtime::builder().server_policy(outer),
        http::server_hosts(hosts),
    );
    let reports = report.routes();
    assert_eq!(reports.len(), 2);
    let named = reports
        .iter()
        .find(|route| route.host() == Some("reports.example"))
        .unwrap();
    assert_eq!(
        named.request_budget().total().value(),
        Some(Duration::from_secs(11))
    );
    assert_eq!(
        layers(named.request_budget().total().sources()),
        [(PolicyLayer::Host, false), (PolicyLayer::Router, false)]
    );
    let fallback = reports.iter().find(|route| route.host().is_none()).unwrap();
    assert_eq!(
        layers(fallback.request_budget().total().sources()),
        [(PolicyLayer::Host, false)]
    );
    assert_diagnostics(&report, &[]);
}

/// Configuration equality records intent, and a values comparison does not.
#[test]
fn policy_equality_includes_intent_and_bounds_compare_values() {
    let restated = ServerPolicy::default()
        .shutdown_timeout(DEFAULT_DEADLINE)
        .unwrap();
    assert_ne!(ServerPolicy::default(), restated);
    let default = resolved_under(runtime::builder(), http::server(router()));
    let explicit = resolved_under(runtime::builder(), http::server(router()).policy(restated));
    assert_ne!(default, explicit);
    assert!(default.same_bounds(&explicit));
    assert!(default.server().same_bounds(explicit.server()));
    assert!(default.routes()[0].same_bounds(&explicit.routes()[0]));
    let shorter = resolved_under(
        runtime::builder(),
        http::server(router()).policy(ServerPolicy::default().shutdown_timeout(NARROW).unwrap()),
    );
    assert!(!default.same_bounds(&shorter));
}

/// The health route keeps its exposure rule: either side disabling it wins.
#[test]
fn health_route_uses_its_own_containment() {
    let ready = || {
        ServerPolicy::default()
            .health_route(HealthRoute::Path("/ready"))
            .unwrap()
    };
    let disabled = ServerPolicy::default()
        .health_route(HealthRoute::Disabled)
        .unwrap();
    let report = resolved_under(
        runtime::builder().server_policy(disabled),
        http::server(router()).policy(ready()),
    );
    let health = report.server().health_route();
    assert_eq!(health.value(), HealthRoute::Disabled);
    assert_eq!(layers(health.sources()), [(PolicyLayer::Runtime, false)]);
    assert_diagnostics(
        &report,
        &[(
            PolicyLayer::Server,
            PolicyDimension::HealthRoute,
            PolicyValue::Health(HealthRoute::Path("/ready")),
            PolicyValue::Health(HealthRoute::Disabled),
            vec![(PolicyLayer::Runtime, false)],
        )],
    );

    // A default runtime yields to the server's chosen path.
    let report = resolved_under(runtime::builder(), http::server(router()).policy(ready()));
    assert_eq!(
        report.server().health_route().value(),
        HealthRoute::Path("/ready")
    );
    assert_diagnostics(&report, &[]);
}

/// Every public report type formats as names and values, not raw bits.
#[test]
fn report_types_format_readably() {
    let policy = ServerPolicy::default()
        .header_timeout(NARROW)
        .unwrap()
        .request_budget(RequestBudget::unbounded());
    let debug = format!("{policy:?}");
    assert!(
        debug.contains(r#"explicit: {"request.body_idle", "request.total", "header_timeout"}"#),
        "{debug}"
    );
    let report = resolved_under(
        runtime::builder().server_policy(ServerPolicy::default().header_timeout(NARROW).unwrap()),
        http::server(router()).policy(
            ServerPolicy::default()
                .header_timeout(Duration::from_secs(20))
                .unwrap(),
        ),
    );
    let diagnostic = report.diagnostics()[0];
    assert_eq!(diagnostic.dimension().to_string(), "header_timeout");
    assert_eq!(diagnostic.layer().to_string(), "server");
    assert_eq!(diagnostic.configured().to_string(), "20s");
    assert_eq!(diagnostic.sources().to_string(), "runtime");
    assert_eq!(format!("{:?}", diagnostic.sources()), r#"["runtime"]"#);
    let total = report.server().request_budget().total().sources();
    assert_eq!(total.to_string(), "runtime (default), server (default)");
    assert!(!total.is_empty());
    assert_eq!(PolicyValue::Bytes(None).to_string(), "unbounded");
    assert_eq!(
        PolicyValue::Health(HealthRoute::Default).to_string(),
        "/health"
    );
}
