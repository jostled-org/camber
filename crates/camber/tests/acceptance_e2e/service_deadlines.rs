//! The deadlines one admitted request runs under, over real transports.
//!
//! 6.T1 keeps the header boundary pre-head: it is Hyper's, it precedes every
//! operation, and no request budget, connection budget, or shutdown budget
//! reaches it. 6.T2 and 6.T4 own the two deadlines the admitted operation
//! carries, the single mapper call each may make, and the transport disposition
//! their protocol owns.

use crate::common;
use crate::http as http_support;
use crate::stream::{Streamed, open_streaming, read_cut, read_streamed, read_streaming_head};

#[cfg(feature = "ws")]
use camber::http::mock::UpgradeOwnerEdge;
use camber::http::mock::{InboundTerminal, ResponseCommitmentEdge, ServerStopEdge};
/// The refused-handoff row's own vocabulary, which only the upgrade rows name.
#[cfg(feature = "ws")]
use camber::http::mock::{ResponseCommit, ResponseCommitmentController, ResponseOrigin};
use camber::http::{
    BodyAdmission, BodyAdmissionContext, Method, MultipartLimits, MultipartStream, Rejection,
    RejectionContext, RejectionKind, Request, RequestBudget, Response, Router, ServerPolicy,
    StreamResponse, TransferBudget,
};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
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
    // The shape no other row has: a handler that stalls while producing its
    // head rather than while producing a buffered response. Nothing about this
    // request's body can end it — a GET carries none — so the only deadline
    // that can answer it is the total, applied to head production on the
    // streaming arm.
    router.get_stream("/stalled-stream", |_req: &Request| {
        Box::pin(async move {
            tokio::time::sleep(UNREACHED).await;
            StreamResponse::new(200).0
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
    let port = http_support::reserve_response_commitment();
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
                assert_http1_stalled_response_head_closes_and_maps_once().await;
                assert_http2_stalled_response_head_is_stream_local().await;
                #[cfg(feature = "ws")]
                {
                    assert_websocket_handoff_spends_the_request_total("/ws", "the direct upgrade")
                        .await;
                    assert_websocket_handoff_spends_the_request_total(
                        "/ws-proxy/session",
                        "the proxied upgrade",
                    )
                    .await;
                }
                assert_middleware_gate_spends_the_request_total().await;
                assert_response_body_outlives_the_request_total().await;
                assert_http2_stalled_body_is_stream_local().await;
            });
        })
        .expect("the admitted-deadline runtime ran");
}

/// A body that stops arriving is a body-idle expiry, mapped exactly once, and
/// the connection that owes unread payload cannot frame another request.
async fn assert_http1_body_idle_closes_and_maps_once() {
    let port = http_support::reserve_response_commitment();
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
    let port = http_support::reserve_unwatched();
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
    let port = http_support::reserve_unwatched();
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

/// A streaming route's head is produced under the same total a buffered one
/// spends, and the HTTP/1 connection that never received it cannot frame
/// another request.
///
/// The row the two rows above cannot state. Both of theirs end while the total
/// is still collecting a body or running a buffered handler, so a total wired to
/// body collection alone answers them exactly the same way. This one carries no
/// body at all and stalls after dispatch has chosen the streaming arm, so only a
/// total applied to head production can end it.
async fn assert_http1_stalled_response_head_closes_and_maps_once() {
    let port = http_support::reserve_response_commitment();
    let controller = port.controller();
    let (router, log) = counted_mapper(deadline_routes());
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();

    let answered = tokio::task::spawn_blocking(move || {
        let mut peer = http_support::connect(addr).expect("the stalled-head peer connected");
        // Keep-alive, so the close below is the framework's disposition and not
        // the preference this peer asked for.
        http_support::write_request_with_connection(
            &mut peer,
            "keep-alive",
            "GET",
            "/stalled-stream",
            &[],
            b"",
        )
        .expect("write the stalled-head request");
        let answered =
            http_support::read_http_response_bounded(&mut peer).expect("the stall was answered");
        http_support::assert_connection_closed(&mut peer, "a refused response head");
        answered
    })
    .await
    .expect("the stalled-head peer settled");

    assert_eq!(answered.status, 408, "{}", answered.text());
    assert_eq!(
        answered.header("connection"),
        Some("close"),
        "an HTTP/1 refusal must state the disposition it enacts",
    );
    assert_eq!(
        log.request_timeouts.load(Ordering::SeqCst),
        1,
        "a head that outlives its total is a request-total expiry, not a body one",
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
        .expect("the stalled-head fixture tore down");
}

/// The same stalled head over HTTP/2 is ended on its own stream, and the
/// connection under it still carries another request.
async fn assert_http2_stalled_response_head_is_stream_local() {
    let port = http_support::reserve_unwatched();
    let (router, log) = counted_mapper(deadline_routes());
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();

    let mut client = common::PersistentH2Client::connect(addr, CLOSE_BOUND).await;
    let refused = client
        .send_complete("GET", "/stalled-stream", "localhost", &[], b"")
        .await;
    assert_eq!(refused.status, 408, "{}", refused.text());
    assert_eq!(
        refused.header("connection"),
        None,
        "an HTTP/2 refusal carries no connection-specific header",
    );
    assert_eq!(
        log.request_timeouts.load(Ordering::SeqCst),
        1,
        "a stalled HTTP/2 head is the same total expiry HTTP/1 reports",
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
        .expect("the HTTP/2 stalled-head fixture tore down");
}

/// The request total ends at a successful WebSocket handoff, so an upgrade the
/// handoff never completes is refused on it like any other admitted request.
///
/// Held at the production checkpoint the handoff itself pauses at — after the
/// registrar has the bridge and before the `101` is committed — so the deadline
/// is weighed against the one await the upgrade arms make. Nothing is released
/// before the total expires: the peer is answered with a refusal rather than the
/// handshake it asked for, and the session that would have followed a `101`
/// never starts.
///
/// Both upgrade kinds run here. A direct upgrade and a proxied one reach the
/// same handoff through different dispatch arms, and a total wired to one arm
/// alone would leave the other unbounded.
#[cfg(feature = "ws")]
async fn assert_websocket_handoff_spends_the_request_total(path: &str, upgrade: &str) {
    let port = http_support::reserve_handoff_commitment();
    let controller = port.controller();
    let (router, log) = counted_mapper(upgrade_routes());
    let server = port.serve_with_policy(router, upgrade_policy());
    let addr = server.addr();

    controller
        .upgrades
        .pause_once(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("the upgrade handoff was armed");
    let requested = path.to_owned();
    // Both rows read through the same helper, so the row is named in what a
    // failure prints: an unbounded arm and a bounded one are told apart by
    // which upgrade never answered.
    let named = upgrade.to_owned();
    let answered = tokio::task::spawn_blocking(move || {
        let mut peer = common::start_upgrade(addr, &requested);
        let answered = http_support::read_http_response_bounded(&mut peer)
            .unwrap_or_else(|error| panic!("{named} was never answered: {error}"));
        http_support::assert_connection_closed(&mut peer, &format!("{named}, refused"));
        answered
    });
    // Proves the refusal below is the handoff's and not the handshake's: the
    // ticket reached the registrar, so negotiation had already succeeded.
    http_support::wait_until_paused_bounded(
        &controller,
        UpgradeOwnerEdge::AfterHandoffSubmitted,
        upgrade,
    )
    .await;
    let answered = answered.await.expect("the upgrading peer settled");

    assert_handoff_refusal_is_the_frameworks(&controller.commitment, &log, &answered, upgrade);
    controller
        .upgrades
        .release(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("the held handoff was released");
    assert_refused_handoff_returns_its_permit(addr, &log, upgrade).await;

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the upgrade-handoff fixture tore down");
}

/// The refusal an expired total wrote, read off the wire and off the cell.
///
/// The status alone leaves the producer open: a `408` a bridge mapped for itself
/// reads the same on this socket. What tells them apart is that the framework
/// took this operation's one commitment, and took it before a `101` could exist.
#[cfg(feature = "ws")]
fn assert_handoff_refusal_is_the_frameworks(
    controller: &ResponseCommitmentController,
    log: &MapperLog,
    answered: &http_support::HttpResponse,
    upgrade: &str,
) {
    assert_eq!(answered.status, 408, "{upgrade}: {}", answered.text());
    assert_eq!(
        log.request_timeouts.load(Ordering::SeqCst),
        1,
        "{upgrade} outlived its total, so the total is what must answer it",
    );
    assert_eq!(log.calls(), 1, "{upgrade} was mapped exactly once");
    let commitment = controller.observed();
    assert_eq!(
        commitment.committed,
        Some(ResponseCommit::Head(ResponseOrigin::Framework)),
        "{upgrade} was answered by the framework before a 101 existed: {commitment:?}",
    );
    assert_eq!(
        (commitment.attempts, commitment.commits, commitment.late),
        (1, 1, 0),
        "{upgrade} reached the commitment once at its real producer: {commitment:?}",
    );
    assert_eq!(
        controller.operations_observed().admitted,
        1,
        "{upgrade} minted one envelope",
    );
}

/// Serve a second peer through the slot the refused handoff held.
///
/// The refusal drops the registrar's future while the supervisor already holds
/// the ticket, so the bridge that owns this connection's permit is reaped by the
/// cancellation the supervisor answers with — a terminal no other row reaches.
/// Under `connection_limit(1)` a permit that outlived the refusal leaves nothing
/// here that can be accepted, and the probe runs against a live listener, where
/// teardown cannot mask it.
#[cfg(feature = "ws")]
async fn assert_refused_handoff_returns_its_permit(
    addr: SocketAddr,
    log: &MapperLog,
    upgrade: &str,
) {
    let served = tokio::task::spawn_blocking(move || {
        http_support::wait_for_http_response(addr, CLOSE_BOUND)
            .expect("no connection was accepted after the refused upgrade closed");
        http_support::request(addr, "GET", "/quick", &[], b"", CLOSE_BOUND)
            .expect("the request after the refused upgrade was answered")
    })
    .await
    .expect("the permit probe settled");
    assert_eq!(
        served.status,
        200,
        "{upgrade} must hand back the one connection permit it held: {}",
        served.text()
    );
    assert_eq!(
        log.calls(),
        1,
        "{upgrade}: the permit probe is refused by nothing",
    );
}

/// The policy the upgrade-handoff rows serve under.
///
/// One connection at a time, so the permit the cancelled bridge held is
/// something the row can read: a refusal that left the slot taken leaves the
/// probe after it with nothing the listener can accept.
#[cfg(feature = "ws")]
fn upgrade_policy() -> ServerPolicy {
    deadline_policy()
        .connection_limit(1)
        .expect("the upgrade row's connection limit")
}

/// The routes the handoff rows are served through: both upgrade kinds, and the
/// ordinary route the permit probe reads back.
///
/// The proxied route's upstream is never dialled: the bridge that would reach it
/// is spawned behind a gate the handoff opens, and no row here opens it. It
/// exists so the route dispatches as a proxied upgrade rather than a direct one.
#[cfg(feature = "ws")]
fn upgrade_routes() -> Router {
    let mut router = Router::new();
    router.ws("/ws", |_req: &Request, mut conn: camber::http::WsConn| {
        while let Some(message) = conn.recv() {
            if conn.send(&message).is_err() {
                break;
            }
        }
        Ok(())
    });
    router.proxy("/ws-proxy", UNDIALLED_UPSTREAM);
    router.get("/quick", |_req: &Request| async {
        Response::text(200, "quick")
    });
    router
}

/// The upstream the proxied rows name and no row reaches.
///
/// The proxied upgrade's bridge is spawned behind a gate the handoff opens, and
/// the stalled-gate proxy row never returns from its chain, so neither leg is
/// ever dialled.
const UNDIALLED_UPSTREAM: &str = "http://127.0.0.1:1";

/// A middleware chain is admitted work under the request total, whatever class
/// runs it.
///
/// The buffered arm cannot state this. Dispatch builds its chain into the very
/// future the buffered producer is wrapped in, so a total applied to producers
/// alone answers a stalled buffered chain exactly the same way it answers a
/// bounded one. Every class here runs its chain at a gate of its own instead —
/// the streaming and upgrade classes at the shared dispatch gate, multipart and
/// the streaming proxy at theirs — and only a total that covers the gate can end
/// them.
///
/// One fixture per class, because the claim is per gate: a shared mapper counter
/// would let one bounded gate cover for another that is not.
async fn assert_middleware_gate_spends_the_request_total() {
    assert_gated_class_spends_the_total("a streaming route's chain", |addr| {
        let mut peer = http_support::connect(addr).expect("the stalled-gate stream peer connected");
        // Keep-alive, so the close below is the framework's disposition and not
        // the preference this peer asked for.
        http_support::write_request_with_connection(
            &mut peer,
            "keep-alive",
            "GET",
            "/gated-stream",
            &[],
            b"",
        )
        .expect("write the stalled-gate stream request");
        let answered = http_support::read_http_response_bounded(&mut peer)
            .expect("the stalled stream gate was answered");
        http_support::assert_connection_closed(&mut peer, "a stream refused in middleware");
        answered
    })
    .await;

    #[cfg(feature = "ws")]
    assert_gated_class_spends_the_total("an upgrade's chain", |addr| {
        let mut peer = common::start_upgrade(addr, "/gated-ws");
        http_support::read_http_response_bounded(&mut peer)
            .expect("the stalled upgrade gate was answered")
    })
    .await;

    assert_gated_class_spends_the_total("a multipart session's chain", |addr| {
        let mut peer =
            http_support::connect(addr).expect("the stalled-gate multipart peer connected");
        peer.write_all(MULTIPART_HEAD.as_bytes())
            .expect("write a multipart head whose chain never returns");
        peer.flush().expect("flush the stalled-gate multipart head");
        http_support::read_http_response_bounded(&mut peer)
            .expect("the stalled multipart gate was answered")
    })
    .await;

    assert_gated_class_spends_the_total("a streaming proxy's chain", |addr| {
        let mut peer = http_support::connect(addr).expect("the stalled-gate proxy peer connected");
        http_support::write_stalled_body(&mut peer, None, "POST", "/gated-proxy/sink")
            .expect("write a proxied head whose chain never returns");
        http_support::read_http_response_bounded(&mut peer)
            .expect("the stalled proxy gate was answered")
    })
    .await;
}

/// Drive one gated class through the stalling chain and assert its total is what
/// answered it.
///
/// The peer routine is the caller's because each class asks for its own head —
/// an upgrade handshake, a multipart boundary declaration, a chunked upload —
/// and what every one of them shares is that the head is complete and the chain
/// is the only thing left holding the request.
async fn assert_gated_class_spends_the_total(
    class: &str,
    drive: impl FnOnce(SocketAddr) -> http_support::HttpResponse + Send + 'static,
) {
    let port = http_support::reserve_response_commitment();
    let controller = port.controller();
    let (router, log) = counted_mapper(stalled_gate_routes());
    let server = port.serve_with_policy(router, gate_policy());
    let addr = server.addr();

    let answered = tokio::task::spawn_blocking(move || drive(addr))
        .await
        .unwrap_or_else(|error| panic!("{class}: the stalled-gate peer did not settle: {error}"));

    assert_eq!(answered.status, 408, "{class}: {}", answered.text());
    assert_eq!(
        answered.header("connection"),
        Some("close"),
        "{class}: an HTTP/1 refusal must state the disposition it enacts",
    );
    assert_eq!(
        log.request_timeouts.load(Ordering::SeqCst),
        1,
        "{class} outlives the total it was admitted under, so the total must answer it",
    );
    assert_eq!(log.calls(), 1, "{class} was mapped exactly once");
    assert_eq!(
        controller.operations_observed().admitted,
        1,
        "{class} minted one envelope",
    );

    server
        .shutdown_bounded(SHUTDOWN_BOUND)
        .expect("the stalled-gate fixture tore down");
}

/// The gated classes a stalled chain is driven through: one route for each
/// production gate a request can reach outside the buffered dispatch that builds
/// its own.
///
/// No handler here is ever entered. The chain below never passes a request
/// through, so each route only has to make its request dispatch as the class it
/// names.
fn stalled_gate_routes() -> Router {
    let mut router = Router::new();
    router.get_stream("/gated-stream", |_req: &Request| {
        Box::pin(async move { StreamResponse::new(200).0 })
    });
    #[cfg(feature = "ws")]
    router.ws(
        "/gated-ws",
        |_req: &Request, _conn: camber::http::WsConn| Ok(()),
    );
    router.proxy_stream("/gated-proxy", UNDIALLED_UPSTREAM);
    router.multipart(
        Method::Post,
        "/upload",
        MultipartLimits::builder()
            .build()
            .expect("the stalled-gate row's limits"),
        |_req: &Request, _fields: MultipartStream| async move { Response::text(200, "never") },
    );
    router.use_middleware(|_req: &Request, _next: camber::http::Next| async move {
        tokio::time::sleep(UNREACHED).await;
        Response::text(200, "never")
    });
    router
}

/// The policy the stalled-gate row serves under.
///
/// Its quiet interval is out of reach on purpose. None of these classes has read
/// a payload byte by the time its chain runs, so the total is the only deadline
/// that can answer them — and the category the mapper records is what says which
/// one did.
fn gate_policy() -> ServerPolicy {
    ServerPolicy::default()
        .header_timeout(UNREACHED)
        .expect("a header boundary the stalled-gate row does not reach")
        .request_budget(
            RequestBudget::bounded(UNREACHED, REQUEST_TOTAL)
                .expect("the stalled-gate row's request budget"),
        )
        .shutdown_timeout(SHUTDOWN_BOUND)
        .expect("the stalled-gate row's shutdown deadline")
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
    let port = http_support::reserve_unwatched();
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
    let port = http_support::reserve_unwatched();
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
    let port = http_support::reserve_request_body_owner();
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
    let peak = controller.observed().peak_retained_bytes;
    assert!(
        peak <= ADMITTED_CEILING,
        "the crossing frame must never be retained: peak {peak} exceeded {ADMITTED_CEILING}",
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
    let port = http_support::reserve_request_body_owner();
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
    let port = http_support::reserve_unwatched();
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
    let port = http_support::reserve_admitted_commitment();
    let controller = port.controller();
    let (router, log) = counted_mapper(admitting_routes(&released));
    let server = port.serve_with_policy(router, deadline_policy());
    let addr = server.addr();
    let selected = ResponseCommitmentEdge::CauseCommitted(InboundTerminal::Disconnect);
    controller
        .commitment
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
        .commitment
        .release(selected)
        .expect("release the disconnect-terminal observation");

    assert_eq!(
        log.calls(),
        0,
        "a peer that cannot receive a response must not invoke the mapper",
    );
    http_support::assert_released(&released, 1, row);
    http_support::assert_owners_released(&controller.bodies, 1, row);

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
    let port = http_support::reserve_staged_commitment();
    let (listener, addr, controller) = port.into_owned_parts();
    let (router, log) = counted_mapper(admitting_routes(&released));
    let handle = camber::http::server(router)
        .policy(shutdown_policy())
        .serve_background(listener)
        .expect("owned serving requires a Tokio runtime");

    let supervisor = ServerStopEdge::SupervisorSelectedControl;
    let held = ResponseCommitmentEdge::BeforeResponseCommit;
    let selected = ResponseCommitmentEdge::CauseCommitted(InboundTerminal::ShutdownDeadline);
    controller
        .stop
        .pause_once(supervisor)
        .expect("arm the supervisor's control observation");
    controller
        .commitment
        .pause_once(held)
        .expect("arm the pre-selection checkpoint");
    controller
        .commitment
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
        .commitment
        .release(held)
        .expect("release the pre-selection checkpoint");
    http_support::wait_until_paused_bounded(&controller, selected, row).await;
    controller
        .commitment
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
    http_support::assert_owners_released(&controller.bodies, 1, row);

    controller
        .stop
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
    let port = http_support::reserve_request_body_owner();
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
    let peak = controller.observed().peak_retained_bytes;
    assert!(
        peak <= ADMITTED_CEILING,
        "a forwarded upload retains nothing past the ceiling: peak {peak}",
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
async fn proxied_upload(addr: SocketAddr, payload: &[u8], leg: &str) -> u16 {
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

/// The payload maximum every post-commit byte row is bounded by.
const TRANSFER_MAX_BYTES: usize = 24;
/// One frame of a post-commit row's payload. Three reach the maximum exactly.
const TRANSFER_FRAME: &[u8] = b"12345678";
/// The quiet interval a post-commit row that means to cross one configures.
const TRANSFER_IDLE: Duration = Duration::from_millis(150);
/// The lifetime a post-commit row that means to cross one configures.
const TRANSFER_TOTAL: Duration = Duration::from_millis(400);
/// How long a paced post-commit producer waits between frames.
///
/// Well under every crossed bound, and long enough that each frame reaches the
/// peer as its own write rather than as part of one buffer a cut would discard.
const TRANSFER_PACE: Duration = Duration::from_millis(20);

/// Which post-commit download terminal one row triggers.
///
/// Named as a closed set rather than staged ad hoc, because the claim is that
/// every one of them ends the same way: the committed status stands, no mapper
/// runs, and the transport applies its own disposition.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PostCommit {
    /// The download's own payload maximum.
    Bytes,
    /// The download's quiet interval.
    Idle,
    /// The download's lifetime.
    Total,
    /// The body's source failed mid-stream.
    Source,
    /// The peer stopped reading and went away.
    Disconnect,
    /// The one aggregate shutdown deadline expired.
    Shutdown,
}

impl PostCommit {
    /// The route this terminal is triggered on.
    const fn path(self) -> &'static str {
        match self {
            Self::Bytes => "/post-bytes",
            Self::Idle => "/post-idle",
            Self::Total => "/post-total",
            Self::Source => "/post-source",
            Self::Disconnect | Self::Shutdown => "/post-paced",
        }
    }

    /// Payload bytes this row's peer receives before the terminal ends it.
    ///
    /// `None` is a row whose delivered prefix is not fixed: a peer that goes away
    /// and a server that stops both cut at whatever the producer had reached.
    const fn delivered(self) -> Option<usize> {
        match self {
            Self::Bytes => Some(TRANSFER_MAX_BYTES),
            Self::Idle => Some(TRANSFER_FRAME.len()),
            Self::Source | Self::Total | Self::Disconnect | Self::Shutdown => None,
        }
    }

    /// Whether `observed` is a terminal this row's cause can produce.
    ///
    /// One cause per row, except the stop: a graceful stop that reaches its own
    /// deadline escalates to cancellation, and whether a turn falls between the
    /// two is the supervisor's timing rather than this row's claim. Both are
    /// silent rows that end the body without touching the committed status, which
    /// is what the row is about.
    const fn accepts(self, observed: InboundTerminal) -> bool {
        match self {
            Self::Bytes => matches!(observed, InboundTerminal::TransferBytes),
            Self::Idle => matches!(observed, InboundTerminal::TransferIdle),
            Self::Total => matches!(observed, InboundTerminal::TransferTotal),
            Self::Source => matches!(observed, InboundTerminal::SourceFailure),
            Self::Disconnect => matches!(observed, InboundTerminal::Disconnect),
            Self::Shutdown => matches!(
                observed,
                InboundTerminal::ShutdownDeadline | InboundTerminal::ForcedCancellation
            ),
        }
    }
}

/// Invariant 10
///
/// Only a pre-commit producer can map its cause into a response. A committed
/// response head is never replaced: the status crossed to the peer before the
/// transfer failed, so what a post-commit failure changes is the transport, not
/// the answer. Every post-commit download terminal — the payload maximum, the quiet interval, the lifetime, a failed
/// source, a departed peer, and the aggregate shutdown deadline — ends its body
/// and nothing else: the peer keeps the `200` it was already given, the route's
/// mapper is never called, HTTP/1 closes the connection whose framing cannot
/// continue, and HTTP/2 resets the one affected stream while another stream on
/// the same connection still answers. The aggregate deadline is the exception
/// both transports share: the server itself is going, so it cuts the connection
/// rather than one stream on it, and there is no neighbour left to ask.
#[test]
fn postcommit_failure_preserves_wire_status_and_ends_transport() {
    camber::runtime::builder()
        .run(|| {
            camber::runtime::block_on(async {
                for row in [
                    PostCommit::Bytes,
                    PostCommit::Idle,
                    PostCommit::Total,
                    PostCommit::Source,
                    PostCommit::Disconnect,
                    PostCommit::Shutdown,
                ] {
                    assert_postcommit_http1(row).await;
                    assert_postcommit_http2(row).await;
                }
            });
        })
        .expect("the post-commit transfer runtime ran");
}

/// One row over HTTP/1: the head stands, the body is cut, the connection closes.
async fn assert_postcommit_http1(row: PostCommit) {
    let label = format!("{row:?} over HTTP/1");
    let upstream = truncating_upstream(row);
    let port = http_support::reserve_transfer_owner();
    let controller = port.controller();
    let (router, log) = counted_mapper(postcommit_routes(upstream.as_ref()));
    let (listener, addr, _) = port.into_owned_parts();
    let handle = camber::http::server(router)
        .policy(postcommit_policy(row))
        .serve_background(listener)
        .expect("the owned server requires a Tokio runtime");

    let mut peer = open_streaming(addr, row.path(), CLOSE_BOUND).await;
    let delivered = match row {
        // The peer is the cause: it reads its committed head and then goes away
        // with the body still coming.
        PostCommit::Disconnect => {
            let status = read_streaming_head(&mut peer, CLOSE_BOUND).await;
            assert_eq!(
                status, 200,
                "{label}: the head committed before the peer left"
            );
            drop(peer);
            None
        }
        // The server is the cause, and it can only be applied to a head already
        // written: the status is read first, and what is left to establish is how
        // the body under it ended.
        PostCommit::Shutdown => {
            let status = read_streaming_head(&mut peer, CLOSE_BOUND).await;
            assert_eq!(status, 200, "{label}: the head committed before the stop");
            handle.shutdown();
            assert!(
                read_cut(&mut peer).await,
                "{label}: the body is cut under its committed head rather than ended"
            );
            None
        }
        _ => Some(read_streamed(&mut peer).await),
    };
    assert_postcommit_wire(row, &label, delivered.as_ref());
    assert_postcommit_owner(&controller, row, &label).await;
    assert_eq!(
        log.calls(),
        0,
        "{label}: a post-commit terminal reaches no mapper"
    );

    handle.cancel();
    assert!(
        tokio::time::timeout(SHUTDOWN_BOUND, handle).await.is_ok(),
        "{label}: the fixture server joined"
    );
}

/// One row over HTTP/2: the stream resets, and its neighbour still answers.
async fn assert_postcommit_http2(row: PostCommit) {
    let label = format!("{row:?} over HTTP/2");
    let upstream = truncating_upstream(row);
    let port = http_support::reserve_transfer_owner();
    let controller = port.controller();
    let (router, log) = counted_mapper(postcommit_routes(upstream.as_ref()));
    let (listener, addr, _) = port.into_owned_parts();
    let handle = camber::http::server(router)
        .policy(postcommit_policy(row))
        .serve_background(listener)
        .expect("the owned server requires a Tokio runtime");

    let mut client = common::PersistentH2Client::connect(addr, CLOSE_BOUND).await;
    let mut download = client.open_download(row.path()).await;
    let streamed = match row {
        PostCommit::Disconnect => {
            download.head().await;
            download.reset();
            None
        }
        PostCommit::Shutdown => {
            download.head().await;
            handle.shutdown();
            Some(download.drain().await)
        }
        _ => Some(download.drain().await),
    };
    assert_postcommit_stream(row, &label, streamed.as_ref());
    assert_postcommit_owner(&controller, row, &label).await;
    assert_eq!(
        log.calls(),
        0,
        "{label}: a post-commit terminal reaches no mapper"
    );
    // The claim HTTP/2 alone can make: the connection under a reset stream is
    // still a connection. A shutdown row has closed admission by now, so it is
    // the one row with no neighbour left to ask.
    match row {
        PostCommit::Shutdown => {}
        _ => {
            let reused = client
                .send_complete("GET", "/quick", "localhost", &[], b"")
                .await;
            assert_eq!(
                reused.status,
                200,
                "{label}: another stream on the same connection still answers: {}",
                reused.text()
            );
        }
    }
    client.close().await;

    handle.cancel();
    assert!(
        tokio::time::timeout(SHUTDOWN_BOUND, handle).await.is_ok(),
        "{label}: the fixture server joined"
    );
}

/// Assert what one row's HTTP/2 stream was given.
///
/// The reset is the claim HTTP/2 makes that HTTP/1 cannot: the status stays what
/// was committed, and what ends is one stream rather than the transport under it.
///
/// The stop is the one row that cannot make it. Its cause is the server going
/// away, and a forced end has no graceful frame left to send, so the peer is
/// entitled to lose the connection instead — the same cut its HTTP/1 twin reads.
/// What every row shares is that the body did not finish under its committed
/// head.
fn assert_postcommit_stream(row: PostCommit, label: &str, streamed: Option<&common::H2Streamed>) {
    let Some(streamed) = streamed else {
        return;
    };
    assert_eq!(
        streamed.status, 200,
        "{label}: the committed status is not replaced"
    );
    let cut = match row {
        PostCommit::Shutdown => streamed.end != common::H2BodyEnd::Ended,
        _ => streamed.end == common::H2BodyEnd::Reset,
    };
    assert!(
        cut,
        "{label}: the affected stream is cut under its committed head rather than ended: {streamed:?}"
    );
    let Some(delivered) = row.delivered() else {
        return;
    };
    assert_eq!(
        streamed.bytes, delivered,
        "{label}: the peer receives what was admitted and no more"
    );
}

/// Assert what one row's HTTP/1 peer was given.
fn assert_postcommit_wire(row: PostCommit, label: &str, delivered: Option<&Streamed>) {
    let Some(delivered) = delivered else {
        return;
    };
    assert_eq!(
        delivered.status, 200,
        "{label}: the committed status is not replaced"
    );
    assert!(
        !delivered.complete,
        "{label}: the body is cut under its committed head rather than ended: {delivered:?}"
    );
    let Some(bytes) = row.delivered() else {
        return;
    };
    assert_eq!(
        delivered.bytes, bytes,
        "{label}: the peer receives what was admitted and no more"
    );
}

/// Assert the production owner fixed this row's terminal and released once.
async fn assert_postcommit_owner(
    controller: &camber::http::mock::TransferOwnerController,
    row: PostCommit,
    label: &str,
) {
    // A row whose cause is the peer or the server can have its transport taken
    // away before the owner weighs another turn: the response is released through
    // the same destruction either way. The four rows a producer causes have no
    // such excuse, so each must fix its own terminal.
    let transport_caused = matches!(row, PostCommit::Disconnect | PostCommit::Shutdown);
    let settled = http_support::poll_until(CLOSE_BOUND, || {
        let observed = controller.observed();
        observed.download.releases >= 1
            && (transport_caused || observed.download.terminal.is_some())
    });
    let observed = controller.observed();
    assert!(settled, "{label}: the download never settled: {observed:?}");
    assert_eq!(
        observed.download.releases, 1,
        "{label}: the download released its source once: {observed:?}"
    );
    match observed.download.terminal {
        None => assert!(
            transport_caused,
            "{label}: a producer-caused row fixes its own terminal: {observed:?}"
        ),
        Some(terminal) => assert!(
            row.accepts(terminal),
            "{label}: this row's own cause is the terminal: {observed:?}"
        ),
    }
    assert!(
        observed.download.terminals <= 1,
        "{label}: a terminal is fixed at most once: {observed:?}"
    );
}

/// The policy one post-commit row serves under.
///
/// Only the shutdown row names a short aggregate deadline: every other row is
/// answered by a bound of its own, and a deadline they could reach would decide
/// them instead.
fn postcommit_policy(row: PostCommit) -> ServerPolicy {
    let deadline = match row {
        PostCommit::Shutdown => SHUTDOWN_DEADLINE,
        _ => SHUTDOWN_BOUND,
    };
    ServerPolicy::default()
        .header_timeout(UNREACHED)
        .expect("a header boundary no post-commit row reaches")
        .request_budget(
            RequestBudget::bounded(UNREACHED, UNREACHED)
                .expect("request deadlines no post-commit row reaches"),
        )
        .shutdown_timeout(deadline)
        .expect("the row's aggregate deadline")
}

/// The routes every post-commit row is served through.
///
/// One router carries all of them: the terminal a row triggers is the route it
/// asks for, so a row cannot be answered by a bound another row configured.
fn postcommit_routes(upstream: Option<&http_support::ReadyServer>) -> Router {
    let mut router = Router::new();
    router.get("/quick", |_req: &Request| async {
        Response::text(200, "quick")
    });
    router.get_stream("/post-bytes", |_req: &Request| {
        Box::pin(async move {
            let (response, sender) = StreamResponse::with_budget(
                200,
                4,
                TransferBudget::bounded(TRANSFER_MAX_BYTES, UNREACHED, UNREACHED)
                    .expect("the byte row's budget"),
            )
            .expect("a positive stream capacity");
            spawn_paced(sender, TRANSFER_MAX_BYTES / TRANSFER_FRAME.len() + 1);
            response
        })
    });
    router.get_stream("/post-idle", |_req: &Request| {
        Box::pin(async move {
            let (response, sender) = StreamResponse::with_budget(
                200,
                4,
                TransferBudget::unbounded()
                    .with_idle(TRANSFER_IDLE)
                    .expect("the idle row's budget"),
            )
            .expect("a positive stream capacity");
            spawn_paced(sender, 1);
            response
        })
    });
    router.get_stream("/post-total", |_req: &Request| {
        Box::pin(async move {
            let (response, sender) = StreamResponse::with_budget(
                200,
                4,
                TransferBudget::unbounded()
                    .with_total(TRANSFER_TOTAL)
                    .expect("the total row's budget"),
            )
            .expect("a positive stream capacity");
            spawn_paced(sender, usize::MAX);
            response
        })
    });
    // The two rows the producer cannot cause: a peer that leaves and a server
    // that stops both cut a stream that would otherwise run on.
    router.get_stream("/post-paced", |_req: &Request| {
        Box::pin(async move {
            let (response, sender) = StreamResponse::with_budget(
                200,
                4,
                TransferBudget::unbounded()
                    .with_idle(UNREACHED)
                    .expect("the paced row's budget"),
            )
            .expect("a positive stream capacity");
            spawn_paced(sender, usize::MAX);
            response
        })
    });
    match upstream {
        Some(upstream) => {
            router.proxy_stream("/post-source", &format!("http://{}", upstream.local_addr()));
        }
        None => {}
    }
    router
}

/// Publish `frames` paced frames, then hold the stream open.
///
/// Paced so each frame reaches the peer as its own write: a producer that filled
/// the channel in one turn would leave Hyper holding every frame in one unflushed
/// buffer, and a body that then ended would take the whole of it with the
/// transport. Held open afterwards so the terminal a row asserts on is the
/// owner's and not the producer's end.
fn spawn_paced(sender: camber::http::StreamSender, frames: usize) {
    tokio::spawn(async move {
        for _ in 0..frames {
            if sender.send(TRANSFER_FRAME).await.is_err() {
                return;
            }
            tokio::time::sleep(TRANSFER_PACE).await;
        }
        std::future::pending::<()>().await;
    });
}

/// The upstream one source row proxies to, and nothing for every other row.
///
/// It is a Camber server whose own download maximum its producer crosses, so it
/// answers with a `200`, writes one frame, and then has its body cut. Read
/// through the proxy that forwarded its head, that cut is a source failure under a
/// response head Camber has already committed — which is the one download source
/// failure a served route can actually produce.
fn truncating_upstream(row: PostCommit) -> Option<http_support::ReadyServer> {
    match row {
        PostCommit::Source => {
            let mut upstream = Router::new();
            upstream.get_stream("/", |_req: &Request| {
                Box::pin(async move {
                    let (response, sender) = StreamResponse::with_budget(
                        200,
                        4,
                        TransferBudget::bounded(TRANSFER_FRAME.len(), UNREACHED, UNREACHED)
                            .expect("the upstream's own maximum"),
                    )
                    .expect("a positive stream capacity");
                    spawn_paced(sender, 2);
                    response
                })
            });
            Some(
                http_support::spawn_server_ready(upstream, CLOSE_BOUND)
                    .expect("the source row's upstream answered"),
            )
        }
        _ => None,
    }
}
