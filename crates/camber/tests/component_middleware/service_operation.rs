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
const MULTIPART_PATH: &str = "/upload";
#[cfg(feature = "ws")]
const WS_PATH: &str = "/ws";
#[cfg(feature = "ws")]
const WS_PROXY_PATH: &str = "/wsproxy/ws";

/// The multipart payload every upload row sends, and the boundary framing it.
const MULTIPART_BOUNDARY: &str = "CamberProjection";
const MULTIPART_FIELD: &str = "payload";

/// A declaration naming no boundary at all.
///
/// The one refusal a multipart route raises after its chain has already passed:
/// the head it answers with is the framework's, and what the chain stated over
/// the provisional head has to reach that one too.
const MULTIPART_WITHOUT_BOUNDARY: &str = "multipart/form-data";

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

/// What ran behind the one frame, split by who owns the head it ran under.
///
/// Two counters rather than one total, because the two halves of the matrix
/// make opposite claims. A gated class produces nothing until the chain has
/// passed, so a refusal row's zero is the whole proof. A wrapped class has no
/// gate at all — its middleware wraps the answer its handler already produced —
/// so the same refusal reaches it only after that handler ran, and a shared
/// total would make each half's claim unreadable.
#[derive(Default)]
struct Work {
    /// Producers, sessions, upgrades, forwarded legs, and services that begin
    /// only once a gate has admitted the request.
    gated: AtomicUsize,
    /// Handlers and forwarded legs whose answer the chain wraps.
    wrapped: AtomicUsize,
}

impl Work {
    fn gated(&self) -> usize {
        self.gated.load(Ordering::SeqCst)
    }

    fn wrapped(&self) -> usize {
        self.wrapped.load(Ordering::SeqCst)
    }

    fn enter_gated(&self) {
        self.gated.fetch_add(1, Ordering::SeqCst);
    }

    fn enter_wrapped(&self) {
        self.wrapped.fetch_add(1, Ordering::SeqCst);
    }
}

/// One served fixture: the classes under test, and what actually ran.
struct Fixture {
    addr: SocketAddr,
    /// Every specialized producer, handler, upstream route, and gRPC service
    /// this fixture serves records itself here as it is entered.
    work: Arc<Work>,
    journal: Journal,
}

impl Fixture {
    /// Serve every class under one router carrying one frame.
    ///
    /// Two upstreams, not one: the buffered proxy's forwarded leg is wrapped
    /// work and the streaming proxy's is gated, and a single upstream route
    /// could not tell a reader which class had entered it.
    fn serve(policy: Policy) -> Self {
        let work = Arc::new(Work::default());
        let journal = observed::journal();
        let wrapped_upstream = common::spawn_server(wrapped_upstream_router(&work));
        let gated_upstream = common::spawn_server(gated_upstream_router(&work));

        let mut router = Router::new();
        project(&mut router, policy);
        register_classes(&mut router, &work, wrapped_upstream, gated_upstream);
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

    /// How much gated work — producers, sessions, upgrades, services — ran.
    fn gated(&self) -> usize {
        self.work.gated()
    }

    /// How much wrapped work — the two classes with no gate — ran.
    fn wrapped(&self) -> usize {
        self.work.wrapped()
    }

    /// The category the mapper recorded for this row's one refusal.
    fn refused_kind(&self, label: &str) -> camber::http::RejectionKind {
        observed::only(&self.journal, label).kind
    }
}

/// The upstream the buffered proxy forwards to, whose leg is wrapped work.
fn wrapped_upstream_router(work: &Arc<Work>) -> Router {
    let mut upstream = Router::new();
    let entered = Arc::clone(work);
    upstream.get("/echo", move |_req: &Request| {
        entered.enter_wrapped();
        async { Response::text(200, "upstream").map(|resp| resp.with_content_type(UPSTREAM_TYPE)) }
    });
    upstream
}

/// The upstream the streaming proxy and the proxied upgrade forward to.
fn gated_upstream_router(work: &Arc<Work>) -> Router {
    let mut upstream = Router::new();
    let entered = Arc::clone(work);
    upstream.get("/echo", move |_req: &Request| {
        entered.enter_gated();
        async { Response::text(200, "upstream").map(|resp| resp.with_content_type(UPSTREAM_TYPE)) }
    });
    register_upstream_upgrade(&mut upstream, work);
    upstream
}

/// Register the upgrade the proxied WebSocket class forwards to.
#[cfg(feature = "ws")]
fn register_upstream_upgrade(upstream: &mut Router, work: &Arc<Work>) {
    let entered = Arc::clone(work);
    upstream.ws("/ws", move |_req: &Request, _conn: camber::http::WsConn| {
        entered.enter_gated();
        Ok(())
    });
}

/// Without the feature, no upgrade class is served and none is registered.
#[cfg(not(feature = "ws"))]
fn register_upstream_upgrade(_upstream: &mut Router, _work: &Arc<Work>) {}

/// Register every class the projection matrix covers.
fn register_classes(
    router: &mut Router,
    work: &Arc<Work>,
    wrapped_upstream: SocketAddr,
    gated_upstream: SocketAddr,
) {
    let wrapped_backend = format!("http://{wrapped_upstream}");
    let gated_backend = format!("http://{gated_upstream}");
    let entered = Arc::clone(work);
    router.get(HTTP_PATH, move |_req: &Request| {
        entered.enter_wrapped();
        async { Response::text(200, "buffered") }
    });
    register_stream(router, work);
    register_sse(router, work);
    register_multipart(router, work);
    register_upgrades(router, work, &gated_backend);
    register_grpc(router, work);
    router.proxy("/buffered", &wrapped_backend);
    router.proxy_stream("/streamed", &gated_backend);
}

/// Register the multipart class, whose head its own session settles on.
fn register_multipart(router: &mut Router, work: &Arc<Work>) {
    let entered = Arc::clone(work);
    router.multipart(
        camber::http::Method::Post,
        MULTIPART_PATH,
        camber::http::MultipartLimits::default(),
        move |_req: &Request, mut stream: camber::http::MultipartStream| {
            let entered = Arc::clone(&entered);
            async move {
                entered.enter_gated();
                let mut received = 0;
                while let Some(mut field) = stream.next_field().await? {
                    while let Some(chunk) = field.next_chunk().await? {
                        received += chunk.len();
                    }
                }
                Response::text(200, &format!("received {received}"))
            }
        },
    );
}

/// Register the generic streaming class, which states a representation of its
/// own for the merge to leave alone.
fn register_stream(router: &mut Router, work: &Arc<Work>) {
    let entered = Arc::clone(work);
    router.get_stream(STREAM_PATH, move |_req: &Request| {
        let entered = Arc::clone(&entered);
        Box::pin(async move {
            entered.enter_gated();
            let (response, sender) = StreamResponse::new(200);
            tokio::spawn(async move {
                let _sent = sender.send("streamed").await;
            });
            response.with_header("Content-Type", STREAM_TYPE)
        }) as Pin<Box<dyn std::future::Future<Output = StreamResponse> + Send>>
    });
}

/// Register the SSE class, whose representation the protocol owns outright.
fn register_sse(router: &mut Router, work: &Arc<Work>) {
    let entered = Arc::clone(work);
    router.get_sse(SSE_PATH, move |_req: &Request, writer: &mut SseWriter| {
        entered.enter_gated();
        writer.event("tick", "event")
    });
}

/// Register the direct and proxied upgrade classes.
#[cfg(feature = "ws")]
fn register_upgrades(router: &mut Router, work: &Arc<Work>, backend: &str) {
    let entered = Arc::clone(work);
    router.ws(
        WS_PATH,
        move |_req: &Request, _conn: camber::http::WsConn| {
            entered.enter_gated();
            Ok(())
        },
    );
    router.proxy("/wsproxy", backend);
}

/// Without the feature, neither upgrade class exists.
#[cfg(not(feature = "ws"))]
fn register_upgrades(_router: &mut Router, _work: &Arc<Work>, _backend: &str) {}

/// Register the gRPC class, whose head tonic owns.
#[cfg(feature = "grpc")]
fn register_grpc(router: &mut Router, work: &Arc<Work>) {
    router.grpc(
        camber::http::GrpcRouter::new().add_service(proto::greeter_service::serve(Greeter {
            entered: Arc::clone(work),
        })),
    );
}

/// Without the feature, no gRPC class is served.
#[cfg(not(feature = "grpc"))]
fn register_grpc(_router: &mut Router, _work: &Arc<Work>) {}

/// The unary service the gRPC class dispatches to.
#[cfg(feature = "grpc")]
struct Greeter {
    entered: Arc<Work>,
}

#[cfg(feature = "grpc")]
#[tonic::async_trait]
impl proto::greeter_service::Greeter for Greeter {
    async fn say_hello(
        &self,
        request: tonic::Request<proto::HelloRequest>,
    ) -> Result<tonic::Response<proto::HelloReply>, tonic::Status> {
        self.entered.enter_gated();
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

/// The body one multipart upload declares and sends.
fn multipart_body() -> Box<[u8]> {
    format!(
        "--{MULTIPART_BOUNDARY}\r\nContent-Disposition: form-data; \
         name=\"field\"\r\n\r\n{MULTIPART_FIELD}\r\n--{MULTIPART_BOUNDARY}--\r\n"
    )
    .into_bytes()
    .into_boxed_slice()
}

/// Drive one multipart upload under `declared`, and read the head it answered
/// with.
///
/// The declaration is a parameter because the two multipart rows differ in
/// exactly that: a well-framed upload reaches the session that settles its own
/// head, and one naming no boundary is refused after the same chain has already
/// passed.
fn upload(addr: SocketAddr, declared: &str) -> wire::HttpResponse {
    wire::request(
        addr,
        "POST",
        MULTIPART_PATH,
        &[("Content-Type", declared)],
        &multipart_body(),
        BOUND,
    )
    .expect("the fixture answered the upload")
}

/// The declaration a well-framed upload sends.
fn multipart_declaration() -> Box<str> {
    format!("multipart/form-data; boundary={MULTIPART_BOUNDARY}").into()
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
    for (label, class) in driven_classes() {
        let head = drive(&fixture, class);
        assert_eq!(
            head.status, REFUSED_STATUS,
            "{label}: a short-circuit is the answer the peer gets",
        );
        assert_unprojected(&head, label);
    }
    assert!(
        observed::drain(&fixture.journal).is_empty(),
        "short-circuit: a chain that answered for itself reached no rejection mapper",
    );
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

    assert_multipart_heads_projected(&fixture);
    assert_upgrades_projected(&fixture);
    assert_grpc_projected(&fixture);
    assert!(
        fixture.gated() >= EXPECTED_GATED,
        "pass-through: only {} of the {EXPECTED_GATED} gated producers this row drives ran",
        fixture.gated(),
    );
    assert_eq!(
        fixture.wrapped(),
        EXPECTED_WRAPPED,
        "pass-through: the two classes with no gate each answered exactly once",
    );
}

/// How much gated work — producers, sessions, upgrades, forwarded legs, and
/// services — a passing row enters.
///
/// The floor rather than the exact count: it is what makes the refusal rows'
/// zero load-bearing, because a matrix that quietly stopped driving a class
/// would satisfy "nothing ran" for free.
#[cfg(all(feature = "ws", feature = "grpc"))]
const EXPECTED_GATED: usize = 7;
#[cfg(all(feature = "ws", not(feature = "grpc")))]
const EXPECTED_GATED: usize = 6;
#[cfg(all(not(feature = "ws"), feature = "grpc"))]
const EXPECTED_GATED: usize = 5;
#[cfg(all(not(feature = "ws"), not(feature = "grpc")))]
const EXPECTED_GATED: usize = 4;

/// The ordinary HTTP handler and the buffered proxy's forwarded leg: the two
/// classes whose answer the chain wraps rather than gates.
const EXPECTED_WRAPPED: usize = 2;

/// Both heads a multipart route can answer with carry the projected metadata.
///
/// A multipart request has two of them, and the chain passes before either
/// exists: the head its own session settles on, and the framework refusal a
/// declaration naming no boundary earns after that same chain has passed. A
/// merge wired onto only one of the two leaves the other bare.
fn assert_multipart_heads_projected(fixture: &Fixture) {
    let settled = upload(fixture.addr, &multipart_declaration());
    assert_eq!(settled.status, 200, "multipart: wire status");
    assert_projected(&settled, "multipart");
    assert_cookies(&settled, "multipart");

    let entered_before = fixture.gated();
    let refused = upload(fixture.addr, MULTIPART_WITHOUT_BOUNDARY);
    assert_eq!(
        refused.status, COLLAPSED_STATUS,
        "multipart refusal: the mapper answers a declaration naming no boundary",
    );
    assert_projected(&refused, "multipart refusal");
    assert_eq!(
        fixture.refused_kind("multipart refusal"),
        camber::http::RejectionKind::Multipart,
        "multipart refusal: the category names the framing that could not be read",
    );
    assert_eq!(
        fixture.gated(),
        entered_before,
        "multipart refusal: no session ran for a body that could never be framed",
    );
}

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

/// A frame that fails on the unwind answers through the mapper for every class,
/// and the terminal it passed through authorized no specialized work.
fn assert_unwound_failure_answers_without_specialized_work() {
    assert_every_class_refused_by_its_mapper(
        Policy::Mapped,
        "unwound failure",
        camber::http::RejectionKind::Middleware,
    );
}

/// Metadata the wire cannot carry is refused where it was stated, before any
/// specialized work begins, for every class.
fn assert_unrepresentable_projection_is_refused_before_work() {
    assert_every_class_refused_by_its_mapper(
        Policy::Unrepresentable,
        "unrepresentable projection",
        camber::http::RejectionKind::InvalidHeader,
    );
}

/// Drive every class under one refusing policy and read what each answered.
///
/// Class by class rather than in one sweep, because the mapper claim is a
/// per-request one: each class refuses once, under the category that names what
/// went wrong, and nothing behind any of them runs.
fn assert_every_class_refused_by_its_mapper(
    policy: Policy,
    label: &str,
    kind: camber::http::RejectionKind,
) {
    let fixture = Fixture::serve(policy);
    for (class, driven) in driven_classes() {
        let head = drive(&fixture, driven);
        assert_eq!(
            head.status, COLLAPSED_STATUS,
            "{label}: {class}: the mapper answers",
        );
        assert_unprojected(&head, class);
        assert_eq!(
            fixture.refused_kind(class),
            kind,
            "{label}: {class}: the category the refusal was raised under",
        );
    }
    assert_grpc_refused(&fixture, label);
    assert_eq!(
        fixture.gated(),
        0,
        "{label}: a producer, an upstream, an upgrade, or a service ran behind a refused chain",
    );
    // The contrast, asserted rather than left implicit: a frame that reached
    // its terminal ran the two ungated handlers, and the refusal it raised on
    // the way back out is still what the peer was given.
    assert_eq!(
        fixture.wrapped(),
        EXPECTED_WRAPPED,
        "{label}: the classes with no gate answered their handler before the frame refused",
    );
}

/// How a row drives one class.
#[derive(Clone, Copy, Debug)]
enum Driven {
    /// A GET whose answer is framed as ordinary HTTP.
    Get(&'static str),
    /// A well-framed multipart upload.
    Upload,
    /// An upgrade handshake.
    #[cfg(feature = "ws")]
    Handshake(&'static str),
}

/// Every class this fixture serves over a real transport, and its label.
///
/// gRPC is absent because it is the one class no HTTP framing reads: it is
/// driven through tonic's own client beside every row that uses this table.
fn driven_classes() -> Box<[(&'static str, Driven)]> {
    [
        ("ordinary http", Driven::Get(HTTP_PATH)),
        ("generic stream", Driven::Get(STREAM_PATH)),
        ("sse", Driven::Get(SSE_PATH)),
        ("buffered proxy", Driven::Get(BUFFERED_PATH)),
        ("streaming proxy", Driven::Get(STREAMED_PATH)),
        ("multipart", Driven::Upload),
    ]
    .into_iter()
    .chain(driven_upgrades())
    .collect()
}

/// Both upgrade classes, as this table drives them.
#[cfg(feature = "ws")]
fn driven_upgrades() -> Box<[(&'static str, Driven)]> {
    Box::new([
        ("direct upgrade", Driven::Handshake(WS_PATH)),
        ("proxied upgrade", Driven::Handshake(WS_PROXY_PATH)),
    ])
}

#[cfg(not(feature = "ws"))]
fn driven_upgrades() -> Box<[(&'static str, Driven)]> {
    Box::new([])
}

/// Drive one class and read the head it answered with.
fn drive(fixture: &Fixture, class: Driven) -> wire::HttpResponse {
    match class {
        Driven::Get(path) => exchange(fixture.addr, path),
        Driven::Upload => upload(fixture.addr, &multipart_declaration()),
        #[cfg(feature = "ws")]
        Driven::Handshake(path) => handshake(fixture.addr, path),
    }
}

/// Assert nothing behind any gate ran for this fixture.
fn assert_no_specialized_work(fixture: &Fixture, label: &str) {
    assert_eq!(
        (fixture.gated(), fixture.wrapped()),
        (0, 0),
        "{label}: a producer, an upstream, a handler, or an upgrade ran behind a refused chain",
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
        fixture.gated(),
        0,
        "{label}: the gRPC service ran behind a refused chain",
    );
}

#[cfg(not(feature = "grpc"))]
fn assert_grpc_refused(_fixture: &Fixture, _label: &str) {}
