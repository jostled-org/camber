//! The response-head projection every specialized head consumes.
//!
//! A specialized route runs its middleware as a gate: the chain is shown a
//! provisional head, because the real one does not exist until an upstream, a
//! producer, or an upgrade has committed. What a passing chain adds to that
//! provisional head is retained and merged into the real one, and the protocol's
//! own corrections are applied after it. A frame that refuses — before the
//! terminal, or on the unwind — answers instead, and no specialized work begins.
//!
//! Ordinary HTTP and the buffered proxy are the contrast the matrix exists to
//! state: their middleware wraps the real response, so what a frame sets there
//! is the answer rather than a projection under it.

use crate::http as wire;
use crate::rejection_support::{
    self as observed, COLLAPSED_STATUS, Journal, UNREPRESENTABLE_HEADER,
};
use crate::runtime_support as common;

use camber::http::{Next, Request, Response, Router, SseWriter, StreamResponse};
use camber::{RuntimeError, runtime};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

#[cfg(feature = "grpc")]
mod proto {
    tonic::include_proto!("greeter");
}

/// How long any one exchange here may take.
const BOUND: Duration = Duration::from_secs(10);

/// The header only a retained projection can put on a specialized head.
const PROJECTED: &str = "X-Camber-Projected";
const PROJECTED_VALUE: &str = "applied";

/// The cross-origin metadata a passing frame states for every class.
const ORIGIN_HEADER: &str = "Access-Control-Allow-Origin";
const ORIGIN: &str = "https://projection.test";

/// The representation a passing frame states, which every protocol owning one
/// of its own must override.
const PROJECTED_TYPE: &str = "text/x-projected";

/// The representation the generic streaming handler states for its own head.
const STREAM_TYPE: &str = "text/x-stream";

/// The representation the proxied upstream states for its own head.
const UPSTREAM_TYPE: &str = "text/x-upstream";

/// The two values a passing frame states under one header name.
const FIRST_COOKIE: &str = "first=1";
const SECOND_COOKIE: &str = "second=2";

/// The answer a frame that never reaches the terminal gives.
const REFUSED_STATUS: u16 = 401;
const REFUSED_BODY: &str = "unauthenticated";

/// What a frame that fails on the unwind failed with.
const UNWIND_CAUSE: &str = "the frame refused after the terminal";

/// The paths each class is registered under.
const HTTP_PATH: &str = "/http";
const STREAM_PATH: &str = "/stream";
const SSE_PATH: &str = "/sse";
const BUFFERED_PATH: &str = "/buffered/echo";
const STREAMED_PATH: &str = "/streamed/echo";
#[cfg(feature = "ws")]
const WS_PATH: &str = "/ws";
#[cfg(feature = "ws")]
const WS_PROXY_PATH: &str = "/wsproxy/ws";

// ---------------------------------------------------------------------------
// The one registered middleware frame
// ---------------------------------------------------------------------------

/// What the one registered frame does with the request it is given.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Policy {
    /// Refuse before the terminal, the way authentication does.
    ShortCircuit,
    /// Reach the terminal, then add successful metadata to the head.
    PassThrough,
    /// Reach the terminal, then fail: the route's mapper answers.
    Mapped,
    /// Reach the terminal, then add a header name the wire cannot represent.
    Unrepresentable,
}

/// Register the one frame this row's policy describes.
fn project(router: &mut Router, policy: Policy) {
    match policy {
        Policy::ShortCircuit => refuse_before_terminal(router),
        Policy::PassThrough => add_metadata(router, None),
        Policy::Unrepresentable => add_metadata(router, Some(UNREPRESENTABLE_HEADER)),
        Policy::Mapped => fail_on_unwind(router),
    }
}

/// A frame that answers without ever calling `Next`.
fn refuse_before_terminal(router: &mut Router) {
    router.use_middleware(|_req: &Request, _next: Next| async {
        Response::text(REFUSED_STATUS, REFUSED_BODY)
    });
}

/// A frame that passes, then states response metadata of its own.
///
/// `broken` is the one header name `with_header` accepts and the wire cannot
/// carry. A row that names it is asking what a projection does with metadata it
/// cannot validate; the rows that do not name it are asking what it does with
/// metadata it can.
fn add_metadata(router: &mut Router, broken: Option<&'static str>) {
    router.use_middleware(move |req: &Request, next: Next| {
        let entered = next.call(req);
        async move {
            let answered = entered
                .await
                .with_header(PROJECTED, PROJECTED_VALUE)
                .with_header(ORIGIN_HEADER, ORIGIN)
                .with_header("Set-Cookie", FIRST_COOKIE)
                .with_header("Set-Cookie", SECOND_COOKIE)
                .with_content_type(PROJECTED_TYPE);
            match broken {
                Some(name) => answered.with_header(name, "present"),
                None => answered,
            }
        }
    });
}

/// A frame that passes, then fails on its way back out.
fn fail_on_unwind(router: &mut Router) {
    router.use_middleware(|req: &Request, next: Next| {
        let entered = next.call(req);
        async move {
            let _reached = entered.await;
            Err::<Response, RuntimeError>(RuntimeError::BadRequest(UNWIND_CAUSE.into()))
        }
    });
}

// ---------------------------------------------------------------------------
// The served fixture
// ---------------------------------------------------------------------------

/// One served fixture: the classes under test, and what actually ran.
struct Fixture {
    addr: SocketAddr,
    /// Every specialized producer, handler, upstream route, and gRPC service
    /// this fixture serves increments this as it is entered.
    work: Arc<AtomicUsize>,
    journal: Journal,
}

impl Fixture {
    /// Serve every class under one router carrying one frame.
    fn serve(policy: Policy) -> Self {
        let work = Arc::new(AtomicUsize::new(0));
        let journal = observed::journal();
        let upstream = common::spawn_server(upstream_router(&work));

        let mut router = Router::new();
        project(&mut router, policy);
        register_classes(&mut router, &work, upstream);
        let addr = common::spawn_server(router.rejection_mapper(observed::collapsing_mapper(
            &journal,
            "projection",
            COLLAPSED_STATUS,
        )));
        Self {
            addr,
            work,
            journal,
        }
    }

    /// How many producers, handlers, upstream routes, and services have run.
    fn entered(&self) -> usize {
        self.work.load(Ordering::SeqCst)
    }

    /// The category the mapper recorded for this row's one refusal.
    fn refused_kind(&self, label: &str) -> camber::http::RejectionKind {
        observed::only(&self.journal, label).kind
    }
}

/// The upstream both proxy classes and the proxied upgrade forward to.
fn upstream_router(work: &Arc<AtomicUsize>) -> Router {
    let mut upstream = Router::new();
    let entered = Arc::clone(work);
    upstream.get("/echo", move |_req: &Request| {
        entered.fetch_add(1, Ordering::SeqCst);
        async { Response::text(200, "upstream").map(|resp| resp.with_content_type(UPSTREAM_TYPE)) }
    });
    register_upstream_upgrade(&mut upstream, work);
    upstream
}

/// Register the upgrade the proxied WebSocket class forwards to.
#[cfg(feature = "ws")]
fn register_upstream_upgrade(upstream: &mut Router, work: &Arc<AtomicUsize>) {
    let entered = Arc::clone(work);
    upstream.ws("/ws", move |_req: &Request, _conn: camber::http::WsConn| {
        entered.fetch_add(1, Ordering::SeqCst);
        Ok(())
    });
}

/// Without the feature, no upgrade class is served and none is registered.
#[cfg(not(feature = "ws"))]
fn register_upstream_upgrade(_upstream: &mut Router, _work: &Arc<AtomicUsize>) {}

/// Register every class the projection matrix covers.
fn register_classes(router: &mut Router, work: &Arc<AtomicUsize>, upstream: SocketAddr) {
    let backend = format!("http://{upstream}");
    let entered = Arc::clone(work);
    router.get(HTTP_PATH, move |_req: &Request| {
        entered.fetch_add(1, Ordering::SeqCst);
        async { Response::text(200, "buffered") }
    });
    register_stream(router, work);
    register_sse(router, work);
    register_upgrades(router, work, &backend);
    register_grpc(router, work);
    router.proxy("/buffered", &backend);
    router.proxy_stream("/streamed", &backend);
}

/// Register the generic streaming class, which states a representation of its
/// own for the merge to leave alone.
fn register_stream(router: &mut Router, work: &Arc<AtomicUsize>) {
    let entered = Arc::clone(work);
    router.get_stream(STREAM_PATH, move |_req: &Request| {
        let entered = Arc::clone(&entered);
        Box::pin(async move {
            entered.fetch_add(1, Ordering::SeqCst);
            let (response, sender) = StreamResponse::new(200);
            tokio::spawn(async move {
                let _sent = sender.send("streamed").await;
            });
            response.with_header("Content-Type", STREAM_TYPE)
        }) as Pin<Box<dyn std::future::Future<Output = StreamResponse> + Send>>
    });
}

/// Register the SSE class, whose representation the protocol owns outright.
fn register_sse(router: &mut Router, work: &Arc<AtomicUsize>) {
    let entered = Arc::clone(work);
    router.get_sse(SSE_PATH, move |_req: &Request, writer: &mut SseWriter| {
        entered.fetch_add(1, Ordering::SeqCst);
        writer.event("tick", "event")
    });
}

/// Register the direct and proxied upgrade classes.
#[cfg(feature = "ws")]
fn register_upgrades(router: &mut Router, work: &Arc<AtomicUsize>, backend: &str) {
    let entered = Arc::clone(work);
    router.ws(
        WS_PATH,
        move |_req: &Request, _conn: camber::http::WsConn| {
            entered.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );
    router.proxy("/wsproxy", backend);
}

/// Without the feature, neither upgrade class exists.
#[cfg(not(feature = "ws"))]
fn register_upgrades(_router: &mut Router, _work: &Arc<AtomicUsize>, _backend: &str) {}

/// Register the gRPC class, whose head tonic owns.
#[cfg(feature = "grpc")]
fn register_grpc(router: &mut Router, work: &Arc<AtomicUsize>) {
    router.grpc(
        camber::http::GrpcRouter::new().add_service(proto::greeter_service::serve(Greeter {
            entered: Arc::clone(work),
        })),
    );
}

/// Without the feature, no gRPC class is served.
#[cfg(not(feature = "grpc"))]
fn register_grpc(_router: &mut Router, _work: &Arc<AtomicUsize>) {}

/// The unary service the gRPC class dispatches to.
#[cfg(feature = "grpc")]
struct Greeter {
    entered: Arc<AtomicUsize>,
}

#[cfg(feature = "grpc")]
#[tonic::async_trait]
impl proto::greeter_service::Greeter for Greeter {
    async fn say_hello(
        &self,
        request: tonic::Request<proto::HelloRequest>,
    ) -> Result<tonic::Response<proto::HelloReply>, tonic::Status> {
        self.entered.fetch_add(1, Ordering::SeqCst);
        Ok(tonic::Response::new(proto::HelloReply {
            message: format!("Hello, {}!", request.into_inner().name),
        }))
    }
}

// ---------------------------------------------------------------------------
// The head each class answered with
// ---------------------------------------------------------------------------

/// Assert the successful metadata a passing chain stated reached this head.
fn assert_projected(head: &wire::HttpResponse, label: &str) {
    assert_eq!(
        head.header(PROJECTED),
        Some(PROJECTED_VALUE),
        "{label}: the custom header a passing chain added never reached the real head",
    );
    assert_eq!(
        head.header(ORIGIN_HEADER),
        Some(ORIGIN),
        "{label}: the cross-origin metadata a passing chain added never reached the real head",
    );
}

/// Assert no projected metadata reached a head no chain approved.
fn assert_unprojected(head: &wire::HttpResponse, label: &str) {
    assert_eq!(
        head.header(PROJECTED),
        None,
        "{label}: a refused chain still projected its metadata",
    );
    assert_eq!(
        head.header(ORIGIN_HEADER),
        None,
        "{label}: a refused chain still projected its metadata",
    );
}

/// Assert the representation this head states is the one `expected` names.
///
/// Every value under the name, not the first: a merge that appended beside a
/// protocol's own correction rather than standing aside reads identically
/// through a first-value lookup.
fn assert_content_type(head: &wire::HttpResponse, expected: &str, label: &str) {
    assert_eq!(
        head.header_values("Content-Type").as_ref(),
        [expected],
        "{label}: the wrong owner's representation reached the head",
    );
}

/// Assert both cookies a passing chain set reached this head, in order.
///
/// One name carrying two values is the case a merge keyed by name can lose: a
/// projection grouped for the conflict test has to stay a list where the
/// protocol claims nothing.
fn assert_cookies(head: &wire::HttpResponse, label: &str) {
    assert_eq!(
        head.header_values("Set-Cookie").as_ref(),
        [FIRST_COOKIE, SECOND_COOKIE],
        "{label}: a chain that stated two values under one name lost one",
    );
}

/// Drive one ordinary HTTP exchange and read the head it answered with.
fn exchange(addr: SocketAddr, path: &str) -> wire::HttpResponse {
    wire::request(addr, "GET", path, &[], &[], BOUND).expect("the fixture answered the exchange")
}

/// Drive one upgrade handshake and read the head it answered with.
#[cfg(feature = "ws")]
fn handshake(addr: SocketAddr, path: &str) -> wire::HttpResponse {
    let mut peer = crate::ws_support::start_upgrade(addr, path);
    let raw = crate::ws_support::read_until_double_crlf(&mut peer);
    parsed_head(&raw)
}

/// Read one handshake answer's status line and header lines.
///
/// Rebuilt as the answer every other transport here hands back, rather than as
/// a second answer type with its own header lookup: a handshake is framed
/// differently and read the same way.
#[cfg(feature = "ws")]
fn parsed_head(raw: &str) -> wire::HttpResponse {
    let mut lines = raw.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status line in the handshake answer: {raw:?}"));
    let headers = lines
        .take_while(|line| !line.is_empty())
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (Box::from(name.trim()), Box::from(value.trim())))
        .collect();
    wire::HttpResponse::from_parts(status, headers, Box::new([]))
}

/// Call the gRPC class and read the head tonic committed, or the refusal.
#[cfg(feature = "grpc")]
async fn called(addr: SocketAddr) -> Result<wire::HttpResponse, tonic::Status> {
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("a well-formed gRPC endpoint")
        .connect()
        .await
        .expect("the gRPC fixture accepted the connection");
    proto::greeter_client::GreeterClient::new(channel)
        .say_hello(tonic::Request::new(proto::HelloRequest {
            name: "projection".into(),
        }))
        .await
        .map(|response| {
            wire::HttpResponse::from_parts(200, ascii_metadata(response.metadata()), Box::new([]))
        })
}

/// The head values tonic committed, as the header pairs every answer here
/// carries.
///
/// Binary metadata is dropped rather than decoded: nothing a chain projects is
/// binary, so what a row asserts on is exactly the ASCII half.
#[cfg(feature = "grpc")]
fn ascii_metadata(metadata: &tonic::metadata::MetadataMap) -> Box<[(Box<str>, Box<str>)]> {
    metadata
        .iter()
        .filter_map(|entry| match entry {
            tonic::metadata::KeyAndValueRef::Ascii(name, value) => Some((
                Box::from(name.as_str()),
                Box::from(value.to_str().unwrap_or_default()),
            )),
            tonic::metadata::KeyAndValueRef::Binary(_, _) => None,
        })
        .collect()
}

// ---------------------------------------------------------------------------
// 14.T1
// ---------------------------------------------------------------------------

/// 14.T1
///
/// One middleware run per request decides one thing for every class: whether
/// the request may proceed. A short-circuit, a mapped failure, and metadata the
/// wire cannot carry all stop the specialized work before it begins. Metadata a
/// passing chain stated reaches the real head of every class, under the
/// protocol's own corrections.
#[test]
fn specialized_head_projection_preserves_short_circuit_and_success_metadata() {
    common::test_runtime()
        .run(|| {
            assert_short_circuit_precedes_every_class();
            assert_pass_through_metadata_reaches_every_head();
            assert_unwound_failure_answers_without_specialized_work();
            assert_unrepresentable_projection_is_refused_before_work();
            runtime::request_shutdown();
        })
        .expect("the projection fixture ran to completion");
}

/// A frame that never calls `Next` answers every class, and nothing specialized
/// runs behind it.
fn assert_short_circuit_precedes_every_class() {
    let fixture = Fixture::serve(Policy::ShortCircuit);
    for (label, head) in refusable_heads(&fixture) {
        assert_eq!(
            head.status, REFUSED_STATUS,
            "{label}: a short-circuit is the answer the peer gets",
        );
        assert_unprojected(&head, label);
    }
    assert_no_specialized_work(&fixture, "short-circuit");
    assert_grpc_refused(&fixture, "short-circuit");
}

/// A passing frame's metadata reaches every real head, and the protocol's own
/// corrections still win the names it owns.
fn assert_pass_through_metadata_reaches_every_head() {
    let fixture = Fixture::serve(Policy::PassThrough);

    let ordinary = exchange(fixture.addr, HTTP_PATH);
    assert_eq!(ordinary.status, 200, "ordinary http: wire status");
    assert_projected(&ordinary, "ordinary http");
    assert_content_type(&ordinary, PROJECTED_TYPE, "ordinary http");

    let streamed = exchange(fixture.addr, STREAM_PATH);
    assert_eq!(streamed.status, 200, "generic stream: wire status");
    assert_projected(&streamed, "generic stream");
    assert_content_type(&streamed, STREAM_TYPE, "generic stream");

    let events = exchange(fixture.addr, SSE_PATH);
    assert_eq!(events.status, 200, "sse: wire status");
    assert_projected(&events, "sse");
    assert_content_type(&events, "text/event-stream", "sse");
    assert_cookies(&events, "sse");

    let buffered = exchange(fixture.addr, BUFFERED_PATH);
    assert_eq!(buffered.status, 200, "buffered proxy: wire status");
    assert_projected(&buffered, "buffered proxy");
    assert_content_type(&buffered, PROJECTED_TYPE, "buffered proxy");

    let streaming = exchange(fixture.addr, STREAMED_PATH);
    assert_eq!(streaming.status, 200, "streaming proxy: wire status");
    assert_projected(&streaming, "streaming proxy");
    assert_content_type(&streaming, UPSTREAM_TYPE, "streaming proxy");
    assert_cookies(&streaming, "streaming proxy");

    assert_upgrades_projected(&fixture);
    assert_grpc_projected(&fixture);
    assert!(
        fixture.entered() >= EXPECTED_PRODUCERS,
        "pass-through: only {} of the {EXPECTED_PRODUCERS} producers this row drives ran",
        fixture.entered(),
    );
}

/// How many producers, upstream routes, upgrades, and services a passing row
/// enters.
///
/// The floor rather than the exact count: it is what makes the refusal rows'
/// zero load-bearing, because a matrix that quietly stopped driving a class
/// would satisfy "nothing ran" for free.
#[cfg(all(feature = "ws", feature = "grpc"))]
const EXPECTED_PRODUCERS: usize = 8;
#[cfg(all(feature = "ws", not(feature = "grpc")))]
const EXPECTED_PRODUCERS: usize = 7;
#[cfg(all(not(feature = "ws"), feature = "grpc"))]
const EXPECTED_PRODUCERS: usize = 6;
#[cfg(all(not(feature = "ws"), not(feature = "grpc")))]
const EXPECTED_PRODUCERS: usize = 5;

/// Both upgrade classes commit a `101` carrying the projected metadata beside
/// the accept value the handshake owns.
#[cfg(feature = "ws")]
fn assert_upgrades_projected(fixture: &Fixture) {
    for (label, path) in [
        ("direct upgrade", WS_PATH),
        ("proxied upgrade", WS_PROXY_PATH),
    ] {
        let head = handshake(fixture.addr, path);
        assert_eq!(head.status, 101, "{label}: wire status");
        assert_projected(&head, label);
        assert!(
            head.header("Sec-WebSocket-Accept").is_some(),
            "{label}: the handshake's own correction was lost to the merge",
        );
        assert_eq!(
            head.header("Upgrade"),
            Some("websocket"),
            "{label}: the handshake's own correction was lost to the merge",
        );
    }
}

#[cfg(not(feature = "ws"))]
fn assert_upgrades_projected(_fixture: &Fixture) {}

/// Tonic's committed head carries the projected metadata.
#[cfg(feature = "grpc")]
fn assert_grpc_projected(fixture: &Fixture) {
    let head = runtime::block_on(called(fixture.addr)).expect("the gRPC call succeeded");
    assert_projected(&head, "grpc");
}

#[cfg(not(feature = "grpc"))]
fn assert_grpc_projected(_fixture: &Fixture) {}

/// A frame that fails on the unwind answers through the mapper, and the
/// terminal it passed through authorized no specialized work.
fn assert_unwound_failure_answers_without_specialized_work() {
    let fixture = Fixture::serve(Policy::Mapped);
    let streamed = exchange(fixture.addr, STREAM_PATH);
    assert_eq!(
        streamed.status, COLLAPSED_STATUS,
        "unwound failure: the mapper answers",
    );
    assert_unprojected(&streamed, "unwound failure");
    assert_eq!(
        fixture.refused_kind("unwound failure"),
        camber::http::RejectionKind::Middleware,
        "unwound failure: the frame's own category",
    );
    assert_eq!(
        fixture.entered(),
        0,
        "unwound failure: the gate terminal is not permission to produce",
    );
}

/// Metadata the wire cannot carry is refused where it was stated, before any
/// specialized work begins.
fn assert_unrepresentable_projection_is_refused_before_work() {
    let fixture = Fixture::serve(Policy::Unrepresentable);
    let streamed = exchange(fixture.addr, STREAM_PATH);
    assert_eq!(
        streamed.status, COLLAPSED_STATUS,
        "unrepresentable projection: the mapper answers",
    );
    assert_unprojected(&streamed, "unrepresentable projection");
    assert_eq!(
        fixture.refused_kind("unrepresentable projection"),
        camber::http::RejectionKind::InvalidHeader,
        "unrepresentable projection: the category names the head that could not be built",
    );
    assert_eq!(
        fixture.entered(),
        0,
        "unrepresentable projection: no producer ran for a head that was never built",
    );
}

/// Every class that answers over ordinary HTTP framing, and its label.
fn refusable_heads(fixture: &Fixture) -> Box<[(&'static str, wire::HttpResponse)]> {
    [
        ("ordinary http", HTTP_PATH),
        ("generic stream", STREAM_PATH),
        ("sse", SSE_PATH),
        ("buffered proxy", BUFFERED_PATH),
        ("streaming proxy", STREAMED_PATH),
    ]
    .into_iter()
    .map(|(label, path)| (label, exchange(fixture.addr, path)))
    .chain(refusable_upgrades(fixture))
    .collect()
}

/// Both upgrade classes, answered as the refusals they earned.
#[cfg(feature = "ws")]
fn refusable_upgrades(fixture: &Fixture) -> Box<[(&'static str, wire::HttpResponse)]> {
    [
        ("direct upgrade", WS_PATH),
        ("proxied upgrade", WS_PROXY_PATH),
    ]
    .into_iter()
    .map(|(label, path)| (label, handshake(fixture.addr, path)))
    .collect()
}

#[cfg(not(feature = "ws"))]
fn refusable_upgrades(_fixture: &Fixture) -> Box<[(&'static str, wire::HttpResponse)]> {
    Box::new([])
}

/// Assert nothing behind any gate ran for this fixture.
fn assert_no_specialized_work(fixture: &Fixture, label: &str) {
    assert_eq!(
        fixture.entered(),
        0,
        "{label}: a producer, an upstream, or an upgrade ran behind a refused chain",
    );
}

/// The gRPC class is refused too, and its service never runs.
#[cfg(feature = "grpc")]
fn assert_grpc_refused(fixture: &Fixture, label: &str) {
    let refused = runtime::block_on(called(fixture.addr));
    assert!(
        refused.is_err(),
        "{label}: the gRPC call was answered by the service the chain refused",
    );
    assert_eq!(
        fixture.entered(),
        0,
        "{label}: the gRPC service ran behind a refused chain",
    );
}

#[cfg(not(feature = "grpc"))]
fn assert_grpc_refused(_fixture: &Fixture, _label: &str) {}
