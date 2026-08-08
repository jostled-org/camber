//! Proxy refusals, against upstreams that really are there or really are not.
//!
//! Every row drives a live Camber proxy over TCP and reads the bytes it wrote
//! back. The upstreams are local servers and scripted raw listeners, so what a
//! row proves about a pre-header failure is what actually happened on a socket.

use crate::common;
use crate::common::{
    COLLAPSED_STATUS, Collapsed, Established, Journal, Trail, assert_classification,
    assert_collapsed, assert_established, collapsing_mapper, drain, mark, marking, only, take,
};

use camber::http::{
    Next, RejectionKind, RejectionProtocol, Request, Response, Router, StreamResponse,
};
use camber::{RuntimeError, runtime};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

/// The suite every observation in this module is recorded under.
const ORIGIN: &str = "acceptance_proxy";

/// The bound every transport leg in this module runs under.
///
/// Named apart from the support module's `WIRE_TIMEOUT` rather than shadowing
/// it: this is a deliberate override, because a stalled upstream is answered by
/// the proxy's own deadline and that deadline outlasts any ordinary exchange.
/// One name carrying two numbers would let either value read as the other's.
const PROXY_WIRE_TIMEOUT: Duration = Duration::from_secs(45);

/// The prefix the buffered proxy routes answer under.
const BUFFERED: &str = "/buffered";

/// The prefix the streaming proxy routes answer under.
const STREAMING: &str = "/streaming";

/// The route whose handler fails on its own rather than upstream.
const FAULTING: &str = "/faulting";

/// The prefix whose buffered backend is declared unhealthy for the whole module.
const UNHEALTHY: &str = "/unhealthy";

/// The prefix whose streaming backend is declared unhealthy for the whole module.
const UNHEALTHY_STREAM: &str = "/unhealthy-stream";

// ── Talking to the proxy ───────────────────────────────────────────

/// Open a peer and send one request, leaving the answer on the socket.
///
/// The head is the suite's, so what a proxied request looks like on the wire is
/// stated in one place. The read bound belongs to whichever reader takes the
/// answer: both of them arm a frame deadline of their own before every syscall,
/// and a socket bound set here would be overwritten by the one that ran.
fn send(addr: SocketAddr, path: &str) -> TcpStream {
    let mut peer = common::connect(addr).expect("the proxy peer could not connect");
    common::write_request(&mut peer, "GET", path, &[], b"").expect("the request could not be sent");
    peer
}

/// Send one request and read the whole answer off the socket.
///
/// Read against this module's own deadline rather than the shared bounded form,
/// which arms the suite's five seconds: a refusal this module waits for is the
/// proxy's own upstream deadline, and that outlasts an ordinary exchange.
fn get(addr: SocketAddr, path: &str) -> common::HttpResponse {
    let mut peer = send(addr, path);
    common::read_http_response(&mut peer, Some(Instant::now() + PROXY_WIRE_TIMEOUT))
        .expect("no answer to the proxied request")
}

/// Send one request and read every byte the proxy wrote before it closed.
///
/// Used where the answer outlives the framed reader's own deadline, and where
/// what the peer got is the whole transport rather than one parsed message.
fn get_until_closed(addr: SocketAddr, path: &str) -> String {
    let mut peer = send(addr, path);
    common::drain_to_close(&mut peer, PROXY_WIRE_TIMEOUT).expect("the proxied request never ended")
}

/// A loopback address no listener can hold, so connecting to it is refused.
///
/// Port 1 rather than a bound-and-released ephemeral port. Releasing a port the
/// fixture then depends on staying free leaves a window in which another
/// fixture in this binary is handed it, and the rows that claim the proxy could
/// not reach its upstream would dial a live server instead. Ephemeral
/// allocation never reaches port 1, and binding it needs root, so nothing in
/// this binary can occupy it.
fn closed_backend() -> String {
    "http://127.0.0.1:1".into()
}

// ── Scripted raw upstreams ─────────────────────────────────────────

/// What one scripted upstream does with the connection it accepts.
#[derive(Clone, Copy)]
enum UpstreamScript {
    /// Accept and never answer, so the proxy's own deadline decides.
    Stall,
    /// Answer with a head that promises more body than it will send.
    TruncatedBody,
}

/// A raw upstream that owns its listener and reports when it is finished.
///
/// Held as one value so a row cannot keep the address and forget the thread:
/// dropping this joins the server, and the join is what proves the upstream
/// released its socket rather than being abandoned mid-test.
struct ScriptedUpstream {
    addr: SocketAddr,
    /// The reservation the served thread is parked on, held here as well.
    ///
    /// `try_clone` duplicates one socket rather than binding a second, so the
    /// port stays bound for as long as this value lives and a connection dialled
    /// from `Drop` reaches the thread's own `accept`. Handing the thread the
    /// only handle is what left nothing outside able to release it.
    _reservation: TcpListener,
    truncate: Option<mpsc::SyncSender<()>>,
    finished: mpsc::Receiver<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ScriptedUpstream {
    fn backend(&self) -> String {
        format!("http://{}", self.addr)
    }

    fn truncate_body(&mut self) {
        self.truncate
            .take()
            .expect("the truncated-body script has no release control")
            .send(())
            .expect("the scripted upstream stopped before body truncation");
    }
}

impl Drop for ScriptedUpstream {
    fn drop(&mut self) {
        drop(self.truncate.take());
        // Bounded on the report rather than on the join: the report is sent on
        // every exit the served connection can take, so a thread that finished
        // has already sent it. The teardown claim is asserted only when nothing
        // is already failing, so a drop during an unwind cannot replace the
        // row's own failure.
        let served = self.finished.recv_timeout(PROXY_WIRE_TIMEOUT).is_ok();
        // A thread that never reported is still parked in `accept`, and only a
        // connection can release it. One dial does that, so the join below waits
        // on a thread that is on its way out instead of parking this drop — and
        // the whole binary with it — for good. Dialled only when the report is
        // missing, so the claim above is never satisfied by this fixture's own
        // connection.
        if !served {
            drop(TcpStream::connect(self.addr));
        }
        let joined = self
            .thread
            .take()
            .is_some_and(|thread| thread.join().is_ok());
        if !std::thread::panicking() {
            assert!(
                served,
                "the scripted upstream never finished its connection"
            );
            assert!(joined, "the scripted upstream thread did not join");
        }
    }
}

/// Serve one connection the way the script says, then release the listener.
fn scripted_upstream(script: UpstreamScript) -> ScriptedUpstream {
    let reservation = TcpListener::bind("127.0.0.1:0").expect("no ephemeral port for the upstream");
    let addr = reservation
        .local_addr()
        .expect("the upstream has no address");
    let accepting = reservation
        .try_clone()
        .expect("the upstream reservation could not be shared with its thread");
    let (truncate, truncate_on) = match script {
        UpstreamScript::TruncatedBody => {
            let (release, wait) = mpsc::sync_channel(0);
            (Some(release), Some(wait))
        }
        UpstreamScript::Stall => (None, None),
    };
    let (report, finished) = mpsc::sync_channel(1);
    let thread = std::thread::spawn(move || {
        // Reported rather than skipped: an accept that fails is the fixture
        // breaking, and passing it off as a finished connection would blame the
        // proxy for what the upstream never did.
        match accepting.accept() {
            Ok((stream, _)) => serve_scripted(stream, script, truncate_on),
            Err(error) => panic!("the scripted upstream could not accept: {error}"),
        }
        let _ = report.send(());
    });
    ScriptedUpstream {
        addr,
        _reservation: reservation,
        truncate,
        finished,
        thread: Some(thread),
    }
}

/// Run one script against the connection the upstream accepted.
fn serve_scripted(
    mut stream: TcpStream,
    script: UpstreamScript,
    truncate_on: Option<mpsc::Receiver<()>>,
) {
    stream
        .set_read_timeout(Some(PROXY_WIRE_TIMEOUT))
        .expect("the upstream read bound could not be set");
    common::read_head(&mut stream, PROXY_WIRE_TIMEOUT)
        .expect("the scripted upstream did not receive a complete request head");
    match script {
        UpstreamScript::TruncatedBody => {
            write_truncated_head(
                &mut stream,
                truncate_on.expect("the truncated-body script has no release signal"),
            );
        }
        UpstreamScript::Stall => hold_until_closed(&mut stream),
    }
}

/// Promise a body length, then send less than it and stop.
///
/// The write is the whole premise of the row this serves, so neither leg of it
/// is discarded: a head that never arrives would leave the proxy waiting out its
/// own deadline and the failure would name the proxy for what the fixture did.
fn write_truncated_head(stream: &mut TcpStream, truncate_on: mpsc::Receiver<()>) {
    stream
        .write_all(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 64\r\n\r\nshort",
        )
        .expect("the scripted upstream could not write its truncated head");
    stream
        .flush()
        .expect("the scripted upstream could not flush its truncated head");
    match truncate_on.recv() {
        Ok(()) | Err(_) => {}
    }
}

/// Answer nothing, and hold the connection until the proxy gives up on it.
///
/// The proxy's own upstream deadline is what ends this read, which is the only
/// thing that decides the refusal these bytes never arrive for.
fn hold_until_closed(stream: &mut TcpStream) {
    let mut scratch = [0_u8; 1024];
    while matches!(stream.read(&mut scratch), Ok(count) if count > 0) {}
}

// ── The router every row is answered by ────────────────────────────

/// The path whose gate refuses before the rest of the chain is entered.
const GATE_REFUSES: &str = "/streaming/refused";

/// The path whose gate answers deliberately with its own response.
const GATE_ANSWERS: &str = "/streaming/answered";

/// The status a deliberate gate response carries.
const DELIBERATE_STATUS: u16 = 401;

/// A gate frame that records entry and unwind, and may refuse or answer.
fn ordering_gate(router: &mut Router, trail: &Trail) {
    let trail = Arc::clone(trail);
    router.use_middleware(move |req: &Request, next: Next| {
        let trail = Arc::clone(&trail);
        mark(&trail, "gate:enter");
        let path: Box<str> = req.path().into();
        let short_circuits = &*path == GATE_REFUSES || &*path == GATE_ANSWERS;
        let inner = (!short_circuits).then(|| next.call(req));
        async move {
            let response = match inner {
                None if &*path == GATE_REFUSES => {
                    return Err(RuntimeError::BadRequest("gate refused".into()));
                }
                None => Response::text(DELIBERATE_STATUS, "gated")?,
                Some(inner) => inner.await,
            };
            mark(&trail, "gate:exit");
            Ok(response)
        }
    });
}

/// The proxy every row in this module is answered by.
///
/// One router with every producer on it, so a row's category comes from the
/// producer that raised it rather than from which server answered.
fn proxy_router(journal: &Journal, trail: &Trail, backend: &str) -> Router {
    let mut router = Router::new();
    ordering_gate(&mut router, trail);
    router.proxy(BUFFERED, backend);
    router.proxy_stream(STREAMING, backend);
    router.proxy_checked(UNHEALTHY, backend, Arc::new(AtomicBool::new(false)));
    router.proxy_checked_stream(UNHEALTHY_STREAM, backend, Arc::new(AtomicBool::new(false)));
    router.get(FAULTING, |_req: &Request| async {
        Err::<Response, RuntimeError>(RuntimeError::Http("the route could not be served".into()))
    });
    router.rejection_mapper(marking(
        trail,
        "mapper",
        collapsing_mapper(journal, ORIGIN, COLLAPSED_STATUS),
    ))
}

/// One live proxy, with the journal and trail its policy records through.
///
/// Every case in this module opens the same three lines — a fresh journal, a
/// fresh trail, and a server built from both over the backend the case names —
/// and the three are one fixture: a case that built the router from a journal it
/// did not go on to read would assert against an empty record. Local rather
/// than shared, because the router it spawns is this module's proxy and nothing
/// else's.
fn proxy_fixture(backend: &str) -> (Journal, Trail, SocketAddr) {
    let journal = Journal::default();
    let trail: Trail = Arc::new(Mutex::new(Vec::new()));
    let addr = common::spawn_server(proxy_router(&journal, &trail, backend));
    (journal, trail, addr)
}

// ── The specialized matrix ─────────────────────────────────────────

/// One producer the matrix drives, and the classification it must keep.
struct MatrixRow {
    label: &'static str,
    path: &'static str,
    route: &'static str,
    kind: RejectionKind,
    status: u16,
    message: &'static str,
    protocol: RejectionProtocol,
    /// The stages this row's refusal passes through, in order.
    ///
    /// Declared per row because the three orders are what distinguish the
    /// producers: a terminal the chain wraps is mapped inside it, a gated class
    /// is mapped after the chain returned, and a route refused before dispatch
    /// never enters the chain at all. Collected and dropped, this table proved
    /// none of them.
    trail: &'static [&'static str],
}

const MATRIX_ROWS: [MatrixRow; 5] = [
    MatrixRow {
        label: "buffered proxy cannot reach its upstream",
        path: "/buffered/anything",
        route: "/buffered/*proxy_path",
        kind: RejectionKind::Proxy,
        status: 502,
        message: "bad gateway",
        protocol: RejectionProtocol::Proxy,
        // The buffered forwarder is the chain's own terminal, so its failure is
        // mapped there and the mapped answer unwinds through the gate.
        trail: &["gate:enter", "mapper", "gate:exit"],
    },
    MatrixRow {
        label: "streaming proxy cannot reach its upstream",
        path: "/streaming/anything",
        route: "/streaming/*proxy_path",
        kind: RejectionKind::Proxy,
        status: 502,
        message: "bad gateway",
        protocol: RejectionProtocol::Proxy,
        // A streaming class is gated first and forwarded afterwards, so the
        // chain has already returned when the upstream fails.
        trail: &["gate:enter", "gate:exit", "mapper"],
    },
    MatrixRow {
        label: "no admissible backend",
        path: "/unhealthy/anything",
        route: "/unhealthy/*proxy_path",
        kind: RejectionKind::Proxy,
        status: 503,
        message: "service unavailable",
        protocol: RejectionProtocol::Proxy,
        // Refused on admissibility before either proxy kind is selected, so no
        // gate frame is ever entered.
        trail: &["mapper"],
    },
    MatrixRow {
        label: "no admissible backend behind a streaming route",
        path: "/unhealthy-stream/anything",
        route: "/unhealthy-stream/*proxy_path",
        kind: RejectionKind::Proxy,
        status: 503,
        message: "service unavailable",
        protocol: RejectionProtocol::Proxy,
        // The admissibility refusal precedes the kind, so the streaming route
        // takes the same trail its buffered sibling does.
        trail: &["mapper"],
    },
    MatrixRow {
        label: "the service itself could not answer",
        path: FAULTING,
        route: FAULTING,
        kind: RejectionKind::InternalService,
        status: 500,
        message: "internal server error",
        protocol: RejectionProtocol::OrdinaryHttp,
        // A handler is the chain's terminal, so its failure is mapped where the
        // buffered forwarder's is.
        trail: &["gate:enter", "mapper", "gate:exit"],
    },
];

/// A table with no rows would run no producer and still report success.
///
/// Stated as a compile-time claim, which is the only honest place for it: the
/// length is a literal, so a runtime check of it could never have failed.
const _: () = assert!(!MATRIX_ROWS.is_empty());

#[test]
fn specialized_rejection_matrix_keeps_kind_context_and_stage_order() {
    common::test_runtime()
        .run(|| {
            let (journal, trail, addr) = proxy_fixture(&closed_backend());

            // Every row ran is proved by the rows themselves: `assert_collapsed`
            // drains the journal and requires exactly the one observation its row
            // caused, so a row that never reached the production path fails
            // there.
            for row in &MATRIX_ROWS {
                let answer = get(addr, row.path);
                let label = row.label;
                let seen = assert_collapsed(
                    &journal,
                    &answer,
                    label,
                    &Collapsed {
                        kind: row.kind,
                        status: row.status,
                        message: row.message,
                    },
                );
                assert_established(
                    &seen,
                    &Established {
                        method: "GET",
                        raw_path: row.path,
                        route: Some(row.route),
                        protocol: Some(row.protocol),
                        content_type: None,
                    },
                    label,
                );
                assert_eq!(take(&trail).as_ref(), row.trail, "{label}: stage order");
            }

            let kinds: std::collections::BTreeSet<RejectionKind> =
                MATRIX_ROWS.iter().map(|row| row.kind).collect();
            assert_eq!(
                kinds.len(),
                2,
                "the proxy and internal-service categories stay distinct at one status"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── Gate ordering around a specialized route ───────────────────────

/// One gate-ordering row: what it asks for, and the markers it must produce.
struct OrderRow {
    label: &'static str,
    path: &'static str,
    status: u16,
    mapped: usize,
    trail: &'static [&'static str],
}

const ORDER_ROWS: [OrderRow; 3] = [
    OrderRow {
        label: "the gate refuses before the upstream is dialled",
        path: GATE_REFUSES,
        status: COLLAPSED_STATUS,
        mapped: 1,
        trail: &["gate:enter", "mapper"],
    },
    OrderRow {
        label: "the upstream fails after the gate completed",
        path: "/streaming/anything",
        status: COLLAPSED_STATUS,
        mapped: 1,
        trail: &["gate:enter", "gate:exit", "mapper"],
    },
    OrderRow {
        label: "the gate answers deliberately",
        path: GATE_ANSWERS,
        status: DELIBERATE_STATUS,
        mapped: 0,
        trail: &["gate:enter", "gate:exit"],
    },
];

/// A table with no rows would drive no gate and still report success.
const _: () = assert!(!ORDER_ROWS.is_empty());

#[test]
fn specialized_gate_rejections_are_not_replayed_around_a_later_failure() {
    common::test_runtime()
        .run(|| {
            let (journal, trail, addr) = proxy_fixture(&closed_backend());

            // Every row ran is proved by the trail each row takes: a row that
            // never reached the production path takes an empty trail and fails
            // its own stage-order assertion.
            for row in &ORDER_ROWS {
                let answer = get(addr, row.path);
                let label = row.label;
                assert_eq!(answer.status, row.status, "{label}: wire status");
                assert_eq!(take(&trail).as_ref(), row.trail, "{label}: stage order");
                assert_eq!(
                    drain(&journal).len(),
                    row.mapped,
                    "{label}: mapper invocations"
                );
            }

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── Where the mapper's authority ends ──────────────────────────────

/// The head a truncated upstream promised, before it stopped writing.
const TRUNCATED_STATUS: u16 = 200;

#[test]
fn proxy_preheader_failures_map_but_committed_stream_failure_does_not() {
    common::test_runtime()
        .run(|| {
            let mut upstream = scripted_upstream(UpstreamScript::TruncatedBody);
            let (journal, trail, addr) = proxy_fixture(&upstream.backend());

            // The pre-header half of this case's own name, and what the zero
            // below is measured against. Asserted on its own, that zero would
            // hold just as well for a server whose policy was never registered.
            // The unhealthy route refuses on admissibility before anything is
            // dialled, so it leaves the scripted upstream's single `accept` for
            // the committed request.
            let refused = get(addr, "/unhealthy/data");
            assert_collapsed(
                &journal,
                &refused,
                "pre-header",
                &Collapsed {
                    kind: RejectionKind::Proxy,
                    status: 503,
                    message: "service unavailable",
                },
            );
            drop(take(&trail));

            // The upstream committed a response head, so what fails afterwards
            // belongs to the stream that is already on the wire. Nothing can
            // replace a status the peer has read.
            let mut peer = send(addr, "/streaming/data");
            let head = common::read_head(&mut peer, PROXY_WIRE_TIMEOUT)
                .expect("the upstream's committed status never reached the peer");
            let head = String::from_utf8_lossy(&head).into_owned();

            assert!(
                head.starts_with(&format!("HTTP/1.1 {TRUNCATED_STATUS}")),
                "the upstream's committed status reaches the peer: {head}"
            );
            upstream.truncate_body();
            let tail = common::drain_to_close(&mut peer, PROXY_WIRE_TIMEOUT)
                .expect("the truncated proxied response never ended");
            let answered = format!("{head}{tail}");
            assert_eq!(
                answered.matches("HTTP/1.1 ").count(),
                1,
                "a failure after header commitment produces no replacement response"
            );
            assert!(
                drain(&journal).is_empty(),
                "a failure after header commitment claims no mapper execution"
            );
            assert_eq!(
                take(&trail).as_ref(),
                ["gate:enter", "gate:exit"].as_slice(),
                "the completed gate is not replayed around the committed stream"
            );

            runtime::request_shutdown();
            // The upstream is dropped where this closure's body ends, which is
            // before `run` returns, and its drop joins the served thread under
            // the module's bound.
        })
        .expect("the fixture runtime ran to completion");
}

/// The category and status a stalled upstream's deadline produces.
///
/// Its own case because it is the one row that waits: the proxy's upstream
/// deadline is what decides it, so the upstream must be there and must never
/// answer. Nothing here races — the refusal cannot arrive by any other route —
/// but the wait is real, and putting it in the matrix would make every other
/// row pay for it.
#[test]
fn proxy_preheader_deadline_maps_as_a_gateway_timeout() {
    common::test_runtime()
        .run(|| {
            let upstream = scripted_upstream(UpstreamScript::Stall);
            let (journal, _trail, addr) = proxy_fixture(&upstream.backend());

            let answered = get_until_closed(addr, "/buffered/slow");
            assert!(
                answered.starts_with(&format!("HTTP/1.1 {COLLAPSED_STATUS}")),
                "collapsed wire status: {answered}"
            );
            let seen = only(&journal, "pre-header deadline");
            assert_classification(
                &seen,
                &Collapsed {
                    kind: RejectionKind::Proxy,
                    status: 504,
                    message: "gateway timeout",
                },
                "pre-header deadline",
            );
            assert_eq!(
                seen.protocol,
                Some(RejectionProtocol::Proxy),
                "the selected dispatch class is established"
            );

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}

// ── No handler runs behind a refused proxy route ───────────────────

#[test]
fn refused_proxy_routes_never_reach_an_application_handler() {
    common::test_runtime()
        .run(|| {
            let entries = Arc::new(AtomicUsize::new(0));

            let mut upstream = Router::new();
            let counted = Arc::clone(&entries);
            upstream.get_stream("/data", move |_req: &Request| {
                counted.fetch_add(1, Ordering::SeqCst);
                Box::pin(async {
                    let (response, _sender) = StreamResponse::new(200);
                    response
                })
            });
            let upstream_addr = common::spawn_server(upstream);
            let (journal, _trail, addr) = proxy_fixture(&format!("http://{upstream_addr}"));

            // The counted handler is proved live first. Without it the refusal's
            // zero would read the same way if the upstream had never started, if
            // the counted route were misspelled, or if nothing incremented the
            // counter at all — `spawn_server`'s readiness probe asks for `/`, so
            // it says nothing about this route. The streaming prefix is stripped
            // before the upstream is dialled, so this lands on its `/data`.
            let served = get(addr, "/streaming/data");
            assert_eq!(served.status, 200, "the healthy route reaches its upstream");
            assert_eq!(
                entries.load(Ordering::SeqCst),
                1,
                "the counted upstream handler ran for the healthy route"
            );
            assert!(
                drain(&journal).is_empty(),
                "a proxied request the upstream answered invokes no mapper"
            );

            let refused = get(addr, "/unhealthy/data");
            assert_eq!(
                refused.status, COLLAPSED_STATUS,
                "the unhealthy route is refused"
            );
            assert_eq!(
                entries.load(Ordering::SeqCst),
                1,
                "a refused proxy route never reaches its upstream handler"
            );
            assert_eq!(only(&journal, "unhealthy").kind, RejectionKind::Proxy);

            runtime::request_shutdown();
        })
        .expect("the fixture runtime ran to completion");
}
