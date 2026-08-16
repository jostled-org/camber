//! The deadlines one admitted request runs under, over real transports.
//!
//! 6.T1 keeps the header boundary pre-head: it is Hyper's, it precedes every
//! operation, and no request budget, connection budget, or shutdown budget
//! reaches it. 6.T2 and 6.T4 own the two deadlines the admitted operation
//! carries, the single mapper call each may make, and the transport disposition
//! their protocol owns.

use crate::common;
use crate::http as http_support;

use camber::http::mock::{InboundTerminal, LifecycleCheckpoint};
use camber::http::{
    BodyAdmission, BodyAdmissionContext, Method, MultipartLimits, MultipartStream, Rejection,
    RejectionContext, RejectionKind, Request, RequestBudget, Response, Router, ServerPolicy,
    StreamResponse,
};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How long a live peer has to observe a transport that must close.
const CLOSE_BOUND: Duration = Duration::from_secs(5);
/// How long a bounded fixture teardown may take.
const SHUTDOWN_BOUND: Duration = Duration::from_secs(5);
/// The header boundary every 6.T1 listener is configured with.
const HEADER_BOUNDARY: Duration = Duration::from_millis(200);
/// The quiet interval an admitted body may leave between data frames.
const BODY_IDLE: Duration = Duration::from_millis(300);
/// The lifetime an admitted request has from head to committed response head.
const REQUEST_TOTAL: Duration = Duration::from_millis(600);
/// A deadline no row is meant to reach.
const UNREACHED: Duration = Duration::from_secs(120);

/// What one mapper call recorded.
///
/// Written by the production mapper this route registered, so a row asserting
/// "at most one mapper call" reads the framework's own invocations rather than
/// a count a helper kept beside them.
#[derive(Default)]
struct MapperLog {
    calls: AtomicUsize,
    body_timeouts: AtomicUsize,
    request_timeouts: AtomicUsize,
    body_limits: AtomicUsize,
}

impl MapperLog {
    fn record(&self, kind: RejectionKind) {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let counted = match kind {
            RejectionKind::BodyTimeout => &self.body_timeouts,
            RejectionKind::RequestTimeout => &self.request_timeouts,
            RejectionKind::BodyLimit => &self.body_limits,
            RejectionKind::Routing
            | RejectionKind::MethodSelection
            | RejectionKind::BodyAdmission
            | RejectionKind::BodyUnreadable
            | RejectionKind::MalformedBody
            | RejectionKind::Multipart
            | RejectionKind::InvalidHeader
            | RejectionKind::Application
            | RejectionKind::Middleware
            | RejectionKind::WebSocketHandshake
            | RejectionKind::Proxy
            | RejectionKind::InternalService => return,
        };
        counted.fetch_add(1, Ordering::SeqCst);
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

/// Attach the counting mapper to a router, and hand back what it writes.
fn counted_mapper(router: Router) -> (Router, Arc<MapperLog>) {
    let log = Arc::new(MapperLog::default());
    let observed = Arc::clone(&log);
    let router = router.rejection_mapper(move |rejection: &Rejection, _: &RejectionContext| {
        observed.record(rejection.kind());
        Response::text(rejection.status(), rejection.message())
    });
    (router, log)
}

/// The routes every deadline row is served through.
fn deadline_routes() -> Router {
    let mut router = Router::new();
    router.post("/echo", |req: &Request| {
        let len = req.body().len();
        async move { Response::text(200, &format!("echoed {len}")) }
    });
    router.get("/quick", |_req: &Request| async {
        Response::text(200, "quick")
    });
    router.get("/slow-handler", |_req: &Request| async {
        tokio::time::sleep(UNREACHED).await;
        Response::text(200, "never")
    });
    // A response whose head commits at once and whose body then outlives the
    // request total several times over. The total ends at the committed head,
    // so the time this body spends belongs to the download and not to the
    // request; a total that kept running would cut it off.
    router.get_stream("/slow-stream", |_req: &Request| {
        Box::pin(async move {
            let (streamed, sender) = StreamResponse::new(200);
            tokio::spawn(async move {
                let _committed = sender.send(COMMITTED_CHUNK).await;
                tokio::time::sleep(REQUEST_TOTAL * 3).await;
                let _outlived = sender.send(OUTLIVING_CHUNK).await;
            });
            streamed
        })
    });
    router
}

/// The frame the streamed row commits its head with.
const COMMITTED_CHUNK: &str = "committed";
/// The frame it produces long after its request total would have expired.
const OUTLIVING_CHUNK: &str = "outlived";

/// The policy every admitted-deadline row serves under.
fn deadline_policy() -> ServerPolicy {
    ServerPolicy::default()
        .header_timeout(UNREACHED)
        .expect("a header boundary no admitted-deadline row reaches")
        .request_budget(
            RequestBudget::bounded(BODY_IDLE, REQUEST_TOTAL).expect("the row's request budget"),
        )
        .shutdown_timeout(SHUTDOWN_BOUND)
        .expect("the row's shutdown deadline")
}

/// 6.T1
///
/// The header boundary belongs to Hyper's parser. A peer that never finishes a
/// request head is closed on it while the request, connection, and shutdown
/// budgets beside it are two orders of magnitude longer — and nothing about
/// that closure mints an operation, calls a mapper, or leaks the listener's
/// connection permit, because no request ever existed to own one.
#[test]
fn header_timeout_is_prehead_and_independent_of_operation_and_shutdown_budgets() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_partial_head_closes_plain().await;
                assert_partial_head_closes_tls().await;
            });
        })
        .expect("the header-boundary runtime ran");
}

/// The header boundary over a plain listener, and the same listener afterwards.
async fn assert_partial_head_closes_plain() {
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let (router, log) = counted_mapper(deadline_routes());
    let server = port.serve_with_policy(router, prehead_policy());
    let addr = server.addr();

    let closed = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the partial-head peer connected");
        peer.write_all(b"GET /quick HTTP/1.1\r\nHost: localhost\r\n")
            .expect("write a head that is never finished");
        peer.flush().expect("flush the partial head");
        read_to_close(&mut peer)
    })
    .await
    .expect("the partial-head peer settled");

    assert_eq!(
        closed.len(),
        0,
        "a head Hyper never admitted must be answered with nothing at all",
    );
    let observed = controller.operations_observed();
    assert_eq!(
        observed.admitted, 0,
        "a head that never became a request must mint no operation envelope",
    );
    assert_eq!(
        log.calls(),
        0,
        "a pre-head closure has no route policy to map through",
    );

    // The listener that closed one transport still admits the next, so the
    // permit that transport held was released rather than lost.
    let answered = tokio::task::spawn_blocking(move || {
        http_support::request(addr, "GET", "/quick", &[], b"", CLOSE_BOUND)
    })
    .await
    .expect("the follow-up peer settled")
    .expect("the follow-up request was answered");
    assert_eq!(answered.status, 200, "{}", answered.text());

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the plain header-boundary fixture tore down");
}

/// The header boundary over TLS: the same pre-head owner, past a real handshake.
async fn assert_partial_head_closes_tls() {
    let (server_config, connector) = common::self_signed_server_and_connector();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the TLS header-boundary listener");
    let addr = listener.local_addr().expect("the TLS listener's address");
    let (router, log) = counted_mapper(deadline_routes());
    let handle = camber::http::server(router)
        .policy(prehead_policy())
        .tls(server_config)
        .serve_background(listener)
        .expect("owned TLS serving requires a Tokio runtime");
    let server = http_support::ReadyServer::adopt(addr, handle);

    let stream = tokio::net::TcpStream::connect(addr)
        .await
        .expect("the TLS peer connected");
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a server name");
    let mut tls = connector
        .connect(name, stream)
        .await
        .expect("the TLS handshake completed");
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tls.write_all(b"GET /quick HTTP/1.1\r\nHost: localhost\r\n")
        .await
        .expect("write a TLS head that is never finished");
    tls.flush().await.expect("flush the partial TLS head");

    let mut answered = Vec::new();
    let read = tokio::time::timeout(CLOSE_BOUND, tls.read_to_end(&mut answered)).await;
    assert!(
        read.is_ok(),
        "the TLS transport must close on its header boundary",
    );
    assert_eq!(
        answered.len(),
        0,
        "a TLS head Hyper never admitted must be answered with nothing at all",
    );
    assert_eq!(
        log.calls(),
        0,
        "a pre-head TLS closure has no route policy to map through",
    );

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the TLS header-boundary fixture tore down");
}

/// The policy the pre-head rows serve under: a short header boundary beside
/// request and shutdown budgets far longer than it.
fn prehead_policy() -> ServerPolicy {
    ServerPolicy::default()
        .header_timeout(HEADER_BOUNDARY)
        .expect("the pre-head boundary")
        .request_budget(RequestBudget::bounded(UNREACHED, UNREACHED).expect("unreached budgets"))
        .shutdown_timeout(SHUTDOWN_BOUND)
        .expect("the pre-head row's shutdown deadline")
        .connection_limit(8)
        .expect("a finite connection limit the pre-head row must not consume")
}

/// Read until the peer closes, returning every byte it was given first.
fn read_to_close(peer: &mut TcpStream) -> Box<[u8]> {
    peer.set_read_timeout(Some(CLOSE_BOUND))
        .expect("arm the close observation");
    let mut answered = Vec::new();
    peer.read_to_end(&mut answered)
        .expect("the transport must close rather than hang on its header boundary");
    answered.into_boxed_slice()
}

/// 6.T2
///
/// The two carried deadlines each end a request once, under the category that
/// names the bound they are, and each applies the disposition its protocol
/// owns: HTTP/1 closes a connection whose payload was left unread, while HTTP/2
/// confines the failure to its own stream and leaves the connection able to
/// carry another.
#[test]
fn body_idle_and_request_total_map_once_and_apply_protocol_transport_disposition() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_http1_body_idle_closes_and_maps_once().await;
                assert_http1_paced_body_renews_its_quiet_interval().await;
                assert_bodyless_handler_spends_the_request_total().await;
                assert_response_body_outlives_the_request_total().await;
                assert_http2_stalled_body_is_stream_local().await;
            });
        })
        .expect("the admitted-deadline runtime ran");
}

/// A body that stops arriving is a body-idle expiry, mapped exactly once, and
/// the connection that owes unread payload cannot frame another request.
async fn assert_http1_body_idle_closes_and_maps_once() {
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let (router, log) = counted_mapper(deadline_routes());
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();

    let answered = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the stalled peer connected");
        http_support::write_stalled_body(&mut peer, None, "POST", "/echo")
            .expect("write a head whose payload never finishes");
        let answered =
            http_support::read_http_response_bounded(&mut peer).expect("the stall was answered");
        http_support::assert_connection_closed(&mut peer, "an unread body-idle payload");
        answered
    })
    .await
    .expect("the stalled peer settled");

    assert_eq!(answered.status, 408, "{}", answered.text());
    assert_eq!(
        log.body_timeouts.load(Ordering::SeqCst),
        1,
        "a quiet body must be answered as the idle interval it crossed",
    );
    assert_eq!(
        log.calls(),
        1,
        "exactly one mapper call per refused request"
    );
    assert_eq!(
        controller.operations_observed().admitted,
        1,
        "one admitted head mints one envelope",
    );

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the body-idle fixture tore down");
}

/// A body that keeps delivering payload renews its quiet interval, so a request
/// far longer than the idle bound still completes.
///
/// Its total is deliberately generous: the claim is that the quiet interval
/// restarts per delivered frame, and a total short enough to end the request
/// would prove that instead.
async fn assert_http1_paced_body_renews_its_quiet_interval() {
    let port = http_support::reserve_observed();
    let (router, log) = counted_mapper(deadline_routes());
    let policy = deadline_policy().request_budget(
        RequestBudget::bounded(BODY_IDLE, UNREACHED).expect("the paced row's request budget"),
    );
    let server = port.serve_with_policy(router, policy);
    let addr = server.addr();

    let answered = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the paced peer connected");
        http_support::write_chunked_head(&mut peer, "close", "POST", "/echo", "localhost")
            .expect("write the paced request head");
        for _ in 0..4 {
            std::thread::sleep(BODY_IDLE / 2);
            http_support::write_chunk(&mut peer, b"payload").expect("write a paced chunk");
        }
        http_support::write_chunked_end(&mut peer).expect("end the paced body");
        http_support::read_http_response_bounded(&mut peer).expect("the paced body was answered")
    })
    .await
    .expect("the paced peer settled");

    assert_eq!(answered.status, 200, "{}", answered.text());
    assert_eq!(
        answered.text().as_ref(),
        "echoed 28",
        "every paced frame must be retained",
    );
    assert_eq!(
        log.calls(),
        0,
        "a body inside its quiet interval is refused by nothing",
    );

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the paced-body fixture tore down");
}

/// A request with no body at all still spends the request total, which covers
/// handler execution and response production.
async fn assert_bodyless_handler_spends_the_request_total() {
    let port = http_support::reserve_observed();
    let (router, log) = counted_mapper(deadline_routes());
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();

    let answered = tokio::task::spawn_blocking(move || {
        http_support::request(addr, "GET", "/slow-handler", &[], b"", CLOSE_BOUND)
    })
    .await
    .expect("the stalled-handler peer settled")
    .expect("the stalled handler was answered");

    assert_eq!(answered.status, 408, "{}", answered.text());
    assert_eq!(
        log.request_timeouts.load(Ordering::SeqCst),
        1,
        "a bodyless request that outlives its total is a request-total expiry, \
         not a body one",
    );
    assert_eq!(
        log.calls(),
        1,
        "exactly one mapper call per refused request"
    );

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the stalled-handler fixture tore down");
}

/// The request total ends where the response head commits, so a response body
/// that goes on producing frames long after the total would have expired still
/// completes, and nothing refuses it.
///
/// It is the boundary the two expiry rows above cannot state. Both of them end
/// before a head exists, so a total that never stopped running would answer them
/// exactly the same way; only a body produced past the total can tell a total
/// that ended at commitment from one that outlived it.
async fn assert_response_body_outlives_the_request_total() {
    let port = http_support::reserve_observed();
    let (router, log) = counted_mapper(deadline_routes());
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();

    let answered = tokio::task::spawn_blocking(move || {
        http_support::request(addr, "GET", "/slow-stream", &[], b"", CLOSE_BOUND)
    })
    .await
    .expect("the streamed peer settled")
    .expect("the streamed response was read to its end");

    assert_eq!(answered.status, 200, "{}", answered.text());
    assert_eq!(
        answered.text().as_ref(),
        format!("{COMMITTED_CHUNK}{OUTLIVING_CHUNK}"),
        "a committed head's body must be delivered whole, however long it takes",
    );
    assert_eq!(
        log.calls(),
        0,
        "response-body duration cannot spend a total that ended at the head",
    );

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the outliving-body fixture tore down");
}

/// An HTTP/2 stream whose body stalls is ended on its own stream, and the
/// connection under it still carries another request.
async fn assert_http2_stalled_body_is_stream_local() {
    let port = http_support::reserve_observed();
    let (router, log) = counted_mapper(deadline_routes());
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();

    let mut client = common::PersistentH2Client::connect(addr, CLOSE_BOUND).await;
    let mut stalled = client.open_paced("POST", "/echo", "localhost", &[]).await;
    let refused = stalled.answer().await;
    assert_eq!(refused.status, 408, "{}", refused.text());
    assert_eq!(
        log.body_timeouts.load(Ordering::SeqCst),
        1,
        "a stalled HTTP/2 body is the same idle expiry HTTP/1 reports",
    );

    let reused = client
        .send_complete("GET", "/quick", "localhost", &[], b"")
        .await;
    assert_eq!(
        reused.status,
        200,
        "an HTTP/2 failure must stay on its own stream: {}",
        reused.text()
    );
    assert_eq!(log.calls(), 1, "the reused stream is refused by nothing");
    client.close().await;

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the HTTP/2 stream-local fixture tore down");
}

/// 6.T4
///
/// Deadlines travel with the route's own admission rather than replacing it.
/// The route byte ceiling stays the only counter of request payload, the
/// application permit is released exactly once however the request ends, the
/// frame that crosses the ceiling is never retained, and no deadline owner
/// re-classifies the route that admitted the work.
#[test]
fn request_deadlines_retain_route_body_byte_and_permit_authority() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                assert_byte_terminal_keeps_route_authority().await;
                assert_idle_terminal_releases_one_permit().await;
                assert_multipart_terminal_releases_one_permit().await;
                assert_peer_disconnect_releases_one_permit_without_mapping().await;
                assert_shutdown_terminal_releases_one_permit().await;
                assert_streaming_proxy_terminal_keeps_route_authority().await;
            });
        })
        .expect("the byte-and-permit runtime ran");
}

/// The route byte ceiling ends an oversized body, drops the crossing frame, and
/// releases exactly one application permit.
async fn assert_byte_terminal_keeps_route_authority() {
    let released = Arc::new(AtomicUsize::new(0));
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let (router, log) = counted_mapper(admitting_routes(&released));
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();

    let oversized: Box<[u8]> = vec![b'x'; ADMITTED_CEILING * 4].into_boxed_slice();
    let answered = tokio::task::spawn_blocking(move || {
        http_support::request(addr, "POST", "/admitted", &[], &oversized, CLOSE_BOUND)
    })
    .await
    .expect("the oversized peer settled")
    .expect("the oversized body was answered");

    assert_eq!(answered.status, 413, "{}", answered.text());
    assert_eq!(
        log.body_limits.load(Ordering::SeqCst),
        1,
        "the route's own byte ceiling stays the category a deadline owner \
         cannot re-classify",
    );
    assert!(
        controller.body_peak_retained_bytes() <= ADMITTED_CEILING,
        "the crossing frame must never be retained: peak {} exceeded {ADMITTED_CEILING}",
        controller.body_peak_retained_bytes(),
    );
    http_support::assert_released(&released, 1, "an oversized body");
    http_support::assert_owners_released(&controller, 1, "an oversized body");

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the byte-terminal fixture tore down");
}

/// A body-idle terminal releases the same one permit the route admitted, and
/// answers under the deadline's own category rather than the ceiling's.
async fn assert_idle_terminal_releases_one_permit() {
    let released = Arc::new(AtomicUsize::new(0));
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let (router, log) = counted_mapper(admitting_routes(&released));
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();

    let answered = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the stalled admitted peer connected");
        http_support::write_stalled_body(&mut peer, None, "POST", "/admitted")
            .expect("write a head whose admitted payload never finishes");
        http_support::read_http_response_bounded(&mut peer).expect("the stall was answered")
    })
    .await
    .expect("the stalled admitted peer settled");

    assert_eq!(answered.status, 408, "{}", answered.text());
    assert_eq!(
        log.body_timeouts.load(Ordering::SeqCst),
        1,
        "a deadline terminal answers under the deadline it crossed",
    );
    assert_eq!(
        log.body_limits.load(Ordering::SeqCst),
        0,
        "a quiet body is not an oversized one",
    );
    http_support::assert_released(&released, 1, "a stalled admitted body");
    http_support::assert_owners_released(&controller, 1, "a stalled admitted body");

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the idle-terminal fixture tore down");
}

/// A streaming multipart session reads its payload after the handler starts, so
/// its request total is the deadline that ends a peer who stops sending. The
/// session's own admission still owns the permit, and it is released once.
async fn assert_multipart_terminal_releases_one_permit() {
    let released = Arc::new(AtomicUsize::new(0));
    let port = http_support::reserve_observed();
    let (router, log) = counted_mapper(multipart_routes(&released));
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();

    let answered = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the stalled multipart peer connected");
        peer.write_all(MULTIPART_HEAD.as_bytes())
            .expect("write a multipart head whose payload never arrives");
        peer.flush().expect("flush the multipart head");
        http_support::read_http_response_bounded(&mut peer)
            .expect("the stalled multipart session was answered")
    })
    .await
    .expect("the stalled multipart peer settled");

    assert_eq!(answered.status, 408, "{}", answered.text());
    assert_eq!(
        log.calls(),
        1,
        "a stalled multipart session is refused exactly once",
    );
    assert_eq!(
        log.body_limits.load(Ordering::SeqCst),
        0,
        "a stalled session is not an oversized one",
    );
    http_support::assert_released(&released, 1, "a stalled multipart session");

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the multipart-terminal fixture tore down");
}

/// A peer that leaves while its admitted body is incomplete reaches the
/// connection-owned disconnect terminal before the body source can map its EOF.
/// No response remains possible, but the route's permit still releases once.
async fn assert_peer_disconnect_releases_one_permit_without_mapping() {
    let row = "a disconnected admitted body";
    let released = Arc::new(AtomicUsize::new(0));
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let (router, log) = counted_mapper(admitting_routes(&released));
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();
    let selected = LifecycleCheckpoint::InboundTerminalSelected(InboundTerminal::Disconnect);
    controller
        .pause_once(selected)
        .expect("arm the disconnect-terminal observation");

    let peer = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the disconnecting peer connected");
        http_support::write_stalled_body(&mut peer, None, "POST", "/admitted")
            .expect("write the incomplete admitted body");
    });

    peer.await.expect("the disconnecting peer settled");
    http_support::wait_until_paused_bounded(&controller, selected, row).await;
    controller
        .release(selected)
        .expect("release the disconnect-terminal observation");

    assert_eq!(
        log.calls(),
        0,
        "a peer that cannot receive a response must not invoke the mapper",
    );
    http_support::assert_released(&released, 1, row);
    http_support::assert_owners_released(&controller, 1, row);

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the disconnect-terminal fixture tore down");
}

/// The aggregate shutdown deadline is a terminal the admitted body reaches like
/// any other, and it changes nothing about who owns the request's bytes: the
/// route's permit is released exactly once, the route is not reclassified, and
/// the deadline that has no refusal to give calls no mapper at all.
///
/// The supervisor is held at the control transition it selected, so the abort it
/// would otherwise apply at the same deadline cannot take this connection's task
/// away before the body's own owner has answered. Holding it stages nothing
/// about the terminal: the deadline the coordinator weighs is the one production
/// minted from the policy this listener serves under.
async fn assert_shutdown_terminal_releases_one_permit() {
    let row = "a shutdown deadline";
    let released = Arc::new(AtomicUsize::new(0));
    let port = http_support::reserve_observed();
    let (listener, addr, controller) = port.into_owned_parts();
    let (router, log) = counted_mapper(admitting_routes(&released));
    let handle = camber::http::server(router)
        .policy(shutdown_policy())
        .serve_background(listener)
        .expect("owned serving requires a Tokio runtime");

    let supervisor = LifecycleCheckpoint::SupervisorSelectedControl;
    let held = LifecycleCheckpoint::BeforeInboundTerminalSelection;
    let selected = LifecycleCheckpoint::InboundTerminalSelected(InboundTerminal::ShutdownDeadline);
    controller
        .pause_once(supervisor)
        .expect("arm the supervisor's control observation");
    controller
        .pause_once(held)
        .expect("arm the pre-selection checkpoint");
    controller
        .pause_once(selected)
        .expect("arm the selected-terminal observation");

    let peer = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the shutdown peer connected");
        http_support::write_stalled_body(&mut peer, None, "POST", "/admitted")
            .expect("write a head whose admitted payload never finishes");
        http_support::read_http_response_bounded(&mut peer)
            .expect("the shutdown terminal was answered")
    });

    // Published while the coordinator is held, so the deadline it mints is
    // minted in the turn this release begins.
    http_support::wait_until_paused_bounded(&controller, held, row).await;
    handle.shutdown();
    http_support::wait_until_paused_bounded(&controller, supervisor, row).await;
    controller
        .release(held)
        .expect("release the pre-selection checkpoint");
    http_support::wait_until_paused_bounded(&controller, selected, row).await;
    controller
        .release(selected)
        .expect("release the selected-terminal observation");

    let answered = peer.await.expect("the shutdown peer settled");
    assert_eq!(answered.status, 503, "{}", answered.text());
    assert_eq!(
        log.calls(),
        0,
        "a shutdown deadline has no refusal for a route's mapper to shape",
    );
    assert_eq!(
        log.body_limits.load(Ordering::SeqCst),
        0,
        "a shutdown deadline is not an oversized body",
    );
    http_support::assert_released(&released, 1, row);
    http_support::assert_owners_released(&controller, 1, row);

    controller
        .release(supervisor)
        .expect("release the supervisor's control observation");
    http_support::ReadyServer::adopt(addr, handle)
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the shutdown-terminal fixture tore down");
}

/// The policy the shutdown-terminal row serves under.
///
/// Its request deadlines are two orders of magnitude longer than its aggregate
/// shutdown deadline, so the shutdown deadline is the only source that can end
/// the admitted body and the terminal the row reads cannot be another one
/// wearing its name.
fn shutdown_policy() -> ServerPolicy {
    ServerPolicy::default()
        .header_timeout(UNREACHED)
        .expect("a header boundary the shutdown row does not reach")
        .request_budget(
            RequestBudget::bounded(UNREACHED, UNREACHED)
                .expect("request deadlines the shutdown row does not reach"),
        )
        .shutdown_timeout(SHUTDOWN_DEADLINE)
        .expect("the aggregate deadline the shutdown row ends on")
}

/// The aggregate shutdown deadline the shutdown-terminal row carries.
const SHUTDOWN_DEADLINE: Duration = Duration::from_millis(200);

/// A streaming-proxy consumer answers to the same route-aware admission every
/// buffered one does, and the frame that crosses its ceiling reaches no upstream.
///
/// The admitted control is what makes the zero mean anything. A leg that was
/// never dialled and a leg that dropped its crossing frame both leave an
/// upstream with nothing, so the row forwards a payload inside the ceiling first
/// and reads it back out of the upstream's own counter.
async fn assert_streaming_proxy_terminal_keeps_route_authority() {
    let row = "a proxied oversized body";
    let forwarded = Arc::new(AtomicUsize::new(0));
    let upstream = recording_upstream(&forwarded);
    let backend = format!("http://{}", upstream.local_addr());
    let released = Arc::new(AtomicUsize::new(0));
    let port = http_support::reserve_observed();
    let controller = port.controller();
    let (router, log) = counted_mapper(proxying_routes(&released, &backend));
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();

    let admitted = proxied_upload(addr, &ADMITTED_PAYLOAD.to_vec(), "the admitted proxy leg").await;
    assert_eq!(admitted, 200, "an admitted upload must reach its upstream");
    assert_eq!(
        forwarded.load(Ordering::SeqCst),
        ADMITTED_PAYLOAD.len(),
        "the upstream must be given every byte the route admitted",
    );

    let oversized = vec![b'x'; ADMITTED_CEILING * 4];
    let refused = proxied_upload(addr, &oversized, "the oversized proxy leg").await;
    assert_eq!(
        refused, 413,
        "the route's own byte ceiling refuses the upload"
    );
    assert_eq!(
        log.body_limits.load(Ordering::SeqCst),
        1,
        "a proxied upload is refused under the ceiling's category, once",
    );
    assert_eq!(
        forwarded.load(Ordering::SeqCst),
        ADMITTED_PAYLOAD.len(),
        "the crossing frame must reach no upstream",
    );
    assert!(
        controller.body_peak_retained_bytes() <= ADMITTED_CEILING,
        "a forwarded upload retains nothing past the ceiling: peak {}",
        controller.body_peak_retained_bytes(),
    );
    http_support::assert_released(&released, 2, row);

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the streaming-proxy fixture tore down");
    upstream
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the streaming-proxy upstream tore down");
}

/// The payload the proxy row's admitted control forwards.
const ADMITTED_PAYLOAD: &[u8] = b"forwarded";

/// Forward one chunked upload through the proxy route and report its status.
async fn proxied_upload(addr: std::net::SocketAddr, payload: &[u8], leg: &str) -> u16 {
    let payload: Box<[u8]> = payload.into();
    let leg: Box<str> = leg.into();
    tokio::task::spawn_blocking(move || {
        http_support::send_chunked(
            addr,
            "close",
            "POST",
            "/proxied/sink",
            "localhost",
            &[payload.as_ref()],
        )
        .unwrap_or_else(|error| panic!("{leg} did not complete: {error}"))
        .0
        .status
    })
    .await
    .expect("the proxy peer settled")
}

/// A streaming-proxy route whose admission grants one counted permit under the
/// same finite ceiling the buffered rows are registered with.
fn proxying_routes(released: &Arc<AtomicUsize>, upstream: &str) -> Router {
    let released = Arc::clone(released);
    let mut router = Router::new();
    router.proxy_stream("/proxied", upstream);
    router.body_admission(move |_context: &BodyAdmissionContext<'_>| {
        Ok(BodyAdmission::with_permit(
            ADMITTED_CEILING,
            http_support::permit_probe(&released),
        ))
    })
}

/// An upstream that counts every payload byte it was actually given.
fn recording_upstream(received: &Arc<AtomicUsize>) -> http_support::ReadyServer {
    let received = Arc::clone(received);
    let mut upstream = Router::new();
    upstream.post("/sink", move |req: &Request| {
        received.fetch_add(req.body().len(), Ordering::SeqCst);
        async move { Response::text(200, "sunk") }
    });
    http_support::spawn_server_ready(upstream, CLOSE_BOUND)
        .expect("the proxy row's upstream answered")
}

/// A well-formed multipart head whose chunked payload never arrives.
///
/// Written out rather than framed by a helper because the claim is the deadline
/// and not the grammar: the session has to open successfully, which means a
/// real boundary declaration, and then wait on a body the peer never sends.
const MULTIPART_HEAD: &str = concat!(
    "POST /upload HTTP/1.1\r\n",
    "Host: localhost\r\n",
    "Connection: close\r\n",
    "Content-Type: multipart/form-data; boundary=camber\r\n",
    "Transfer-Encoding: chunked\r\n\r\n",
);

/// A streaming multipart route whose admission grants one counted permit.
fn multipart_routes(released: &Arc<AtomicUsize>) -> Router {
    let released = Arc::clone(released);
    let mut router = Router::new();
    router.multipart(
        Method::Post,
        "/upload",
        MultipartLimits::builder()
            .build()
            .expect("the multipart row's limits"),
        |_req: &Request, fields: MultipartStream| async move {
            let mut fields = fields;
            while let Some(field) = fields.next_field().await? {
                field.discard().await?;
            }
            Response::text(200, "uploaded")
        },
    );
    router.body_admission(move |_context: &BodyAdmissionContext<'_>| {
        Ok(BodyAdmission::with_permit(
            8 * 1024,
            http_support::permit_probe(&released),
        ))
    })
}

/// The byte ceiling the admitted route is registered with.
const ADMITTED_CEILING: usize = 64;

/// A route whose admission grants one counted permit under a finite ceiling.
fn admitting_routes(released: &Arc<AtomicUsize>) -> Router {
    let released = Arc::clone(released);
    let mut router = Router::new();
    router.post("/admitted", |req: &Request| {
        let len = req.body().len();
        async move { Response::text(200, &format!("admitted {len}")) }
    });
    router.body_admission(move |_context: &BodyAdmissionContext<'_>| {
        Ok(BodyAdmission::with_permit(
            ADMITTED_CEILING,
            http_support::permit_probe(&released),
        ))
    })
}
