//! One admitted operation, one response commitment, whichever owner produced it.
//!
//! Every case here drives a real accepted HTTP request through the public
//! `Router` and reads back what that operation's own commitment settled on. The
//! observer counts what production wrote: how many producers reached the cell,
//! how many took it, and how many found it held. It selects no producer, maps no
//! refusal, and names no origin.
//!
//! The table is the producer mapping itself. Each row drives a different
//! production owner — the route handler, the middleware chain, the router's own
//! terminals, the framework's mapper, the static-file worker, a Camber-internal
//! route, and the server-sent-events handoff — and requires that owner to be the
//! one the commitment names. A row that shares a boxed handler shape with
//! another is exactly the pair a commitment inferring the origin from the shape
//! could not tell apart.
//!
//! The middleware chain appears three times because it can short-circuit three
//! different shapes, and each of them reaches the cell down its own path: a gate
//! in front of a specialized class, a frame wrapped around a buffered producer,
//! and a gate in front of a streaming forward that never reaches buffered
//! dispatch at all. One row would leave the other two paths free to name the
//! producer the chain never let run, or to commit nothing at all.
//!
//! Five tests drive these rows, at five boundaries, and every producer family
//! belongs to exactly one of them. The local one owns the producers a buffered
//! route reaches without leaving the process: the mounted handler, the chain
//! that replaced it, the router's own terminals, the served file, an internal
//! route, and the framework's mapper in the two shapes it answers a local
//! operation in. The buffered proxy one owns the three response phases a forward
//! can end in, where the registration alone cannot name the producer. The
//! streaming proxy one owns that class's two pre-head endings, which never reach
//! buffered dispatch at all. The streaming application one owns the two classes
//! whose payload outlives the head that announced it — server-sent events and
//! streaming multipart — including the two rows where the transport ends behind
//! a head the peer already holds. The protocol handoff one owns both upgrade
//! classes: the three ways a handshake is answered before a `101` exists, and
//! the `101` itself.

use crate::http as wire;
use crate::rejection_support::{Journal, drain, journal, recording_mapper};
use crate::runtime_support as common;
use crate::streaming_multipart::{BOUNDARY, DECLARED, Field, multipart_body};
use crate::temp::TempRoot;

use camber::http::mock::{
    ConnectionOwnershipEvent, InboundTerminal, ResponseCommit, ResponseOrigin,
    ScopedCommittedAnswer,
};
use camber::http::{
    BodyAdmission, BodyAdmissionContext, HostRouter, Method, MultipartLimits, MultipartStream,
    Next, ProxyPolicy, Request, RequestBudget, Response, Router, ServerPolicy, SseWriter,
};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// How long one request has to be answered.
const ANSWER_TIMEOUT: Duration = Duration::from_secs(5);

/// The payload maximum the framework row crosses.
const BODY_MAX: usize = 8;

/// The path whose response head is handed to the server-sent-events owner.
const EVENTS: &str = "/events";

/// The event-stream route whose chain outlives the request total in front of it.
///
/// The handoff commits its own head the moment dispatch reaches it, so the gate
/// is the only place this class can still be answered by somebody else. A row
/// that expires there is the framework's head for a feed that never opened.
const SLOW_EVENTS: &str = "/slow-events";

/// The event-stream route whose feed is cut by the maximum it was registered
/// under.
const CAPPED_EVENTS: &str = "/capped-events";

/// The event that feed publishes, and the frame the writer builds from it.
///
/// The frame is spelled out rather than measured, because the maximum below is
/// derived from it: the registered bound admits exactly one event and the second
/// crosses it. A bound too small for even the first would cut the body before
/// the committed head reached the wire, and the row would be proving the
/// transport's timing instead of the commitment.
const CAPPED_EVENT_DATA: &str = "cut";
const CAPPED_EVENT_FRAME: &str = "event: message\ndata: cut\n\n";

/// The payload maximum that feed is registered with.
const CAPPED_EVENT_MAX: usize = CAPPED_EVENT_FRAME.len();

/// How long that feed waits between events.
///
/// A pacing device, not a timing assertion: the admitted event and the one that
/// crosses have to reach the transport as two writes, or Hyper batches them into
/// one flush the crossing discards whole and the committed head never leaves.
/// The row's claim is what the commitment holds, and this is what puts the head
/// on the wire before the body under it ends.
const CAPPED_EVENT_PACE: Duration = Duration::from_millis(20);

/// The path the static-file root is mounted under.
const FILES: &str = "/files";

/// The file the static-file row asks for, and what it holds.
const SERVED_FILE: &str = "served.txt";
const SERVED_BODY: &str = "served from disk";

/// The Camber-internal route this table can reach without a registered
/// resource.
const INTERNAL: &str = "/metrics";

/// The path the middleware chain answers before its own producer is entered.
///
/// A specialized class, because that is where a chain runs as a gate: it either
/// admits the request or replaces the answer, and the replacement is the
/// middleware's own head rather than a frame wrapped around somebody else's.
const GATED: &str = "/gated";

/// The ordinary route the same chain answers without calling `next.call`.
///
/// A buffered class runs its chain around its producer rather than as a gate, so
/// the answer that comes back has the shape the handler's would have had. Its own
/// row, because a commitment that read the origin off the registration alone
/// names the handler here and the gate row above cannot tell.
const GATED_ROUTE: &str = "/gated-route";

/// The streaming-proxy route the same chain answers before any upstream is
/// dialed.
///
/// Its own row for the same reason the two above are separate: this class never
/// reaches the dispatch the buffered rows finish through, so a commitment wired
/// only there leaves this operation's cell empty and reports Hyper as the
/// producer of Camber's own refusal. The backend is never contacted — the chain
/// answers in front of it — so the address only has to be one the registration
/// accepts.
const GATED_PROXY: &str = "/gated-proxy";
const UNDIALED_BACKEND: &str = "http://127.0.0.1:9/";

/// The streaming-proxy route whose upstream leg fails before any head exists.
///
/// The chain admits this one, so the forward is really attempted and really
/// refused. There is no upstream head to commit, and the producer left is the
/// mapper that answers the peer — the one place this class could otherwise
/// finish with an empty cell after doing real work.
const FAILED_PROXY: &str = "/failed-proxy";

/// The two upgrade classes, each reached at the path its registration mounts.
///
/// A direct route is asked for by name; a proxied one is a prefix, so its rows
/// ask for the backend path under it. Both classes answer every phase this table
/// drives, because the producer a phase names has to be the same one whichever
/// bridge would have taken the transport.
#[cfg(feature = "ws")]
const DIRECT_WEBSOCKET: &str = "/direct-websocket";
#[cfg(feature = "ws")]
const PROXIED_WEBSOCKET: &str = "/proxied-websocket";
#[cfg(feature = "ws")]
const PROXIED_WEBSOCKET_PATH: &str = "/proxied-websocket/echo";

/// The two upgrade routes whose chain outlives the request total in front of
/// them.
///
/// Their own registrations for the reason [`SLOW_EVENTS`] has one: an upgrade
/// commits its `101` the moment negotiation produces it, so a gate the total
/// outlived is the only place either class can still be answered by somebody
/// else.
#[cfg(feature = "ws")]
const SLOW_DIRECT_WEBSOCKET: &str = "/slow-direct-websocket";
#[cfg(feature = "ws")]
const SLOW_PROXIED_WEBSOCKET: &str = "/slow-proxied-websocket";
#[cfg(feature = "ws")]
const SLOW_PROXIED_WEBSOCKET_PATH: &str = "/slow-proxied-websocket/echo";

/// The route the WebSocket backend echoes on, behind both proxied prefixes.
#[cfg(feature = "ws")]
const UPSTREAM_ECHO: &str = "/echo";

/// The message an accepted row sends through the session its `101` opened.
///
/// Echoed back, because that round trip is what makes the row a produced handoff
/// rather than a status: a bridge that never took the transport cannot return
/// it.
#[cfg(feature = "ws")]
const BRIDGED_MESSAGE: &str = "bridged";

/// The status a produced handoff answers with.
#[cfg(feature = "ws")]
const UPGRADED_STATUS: u16 = 101;

/// The status the built-in mapper gives an upstream that could not be reached.
const UNREACHABLE_STATUS: u16 = 502;

/// The buffered proxy route whose upstream is never there to answer.
const UNREACHED_PROXY: &str = "/unreached-proxy";
const UNREACHED_PROXY_PATH: &str = "/unreached-proxy/anything";

/// The buffered proxy route whose upstream answers both of its paths.
const ANSWERED_PROXY: &str = "/answered-proxy";
const ANSWERED_PROXY_PATH: &str = "/answered-proxy/answer";
const REFUSED_PROXY_PATH: &str = "/answered-proxy/refusal";

/// The buffered proxy route whose upstream answers past what it may carry.
///
/// Its own registration, because the phase it drives is decided by this route's
/// frozen buffered maximum and not by the request: the upstream produces a head
/// this proxy accepted and then a payload the route cannot carry, which is the
/// only way a buffered forward fails with an upstream head already in hand.
const OVERSIZED_PROXY: &str = "/oversized-proxy";
const OVERSIZED_PROXY_PATH: &str = "/oversized-proxy/oversized";

/// The upstream paths behind the forwarding routes, after the prefix is
/// stripped.
const UPSTREAM_ANSWER: &str = "/answer";
const UPSTREAM_REFUSAL: &str = "/refusal";
const UPSTREAM_OVERSIZED: &str = "/oversized";

/// The buffered maximum [`OVERSIZED_PROXY`] freezes, and the upstream payload
/// that crosses it.
///
/// Small enough that the upstream's declared length alone crosses it, so the
/// phase this row drives is the collection after the head and never the head
/// itself.
const BUFFERED_MAX: usize = 16;
const OVERSIZED_BODY: &str = "a payload past the maximum this route froze";

/// The status the upstream reports for itself on [`UPSTREAM_REFUSAL`].
///
/// Not one the framework's mapper produces for a proxy route, so a row reading
/// it back has read the upstream's own head and not a gateway refusal built in
/// its place.
const UPSTREAM_REFUSED_STATUS: u16 = 503;

/// The path whose handler outlives the request total its route resolved to.
const SLOW: &str = "/slow";

/// The request total the framework row crosses, and the wait that crosses it.
///
/// The wait is only ever the thing on the far side of the total, so what it owes
/// is a clear margin over it and nothing more. It is also what the bounded
/// teardown joins after the peer already holds its refusal, so every millisecond
/// past that margin is wall clock five rows pay for no added claim.
const SLOW_TOTAL: Duration = Duration::from_millis(60);
const SLOW_HANDLER: Duration = Duration::from_millis(200);

/// A payload frame that crosses [`BODY_MAX`].
const CROSSING: &[u8] = b"far-past-the-configured-maximum";

/// The one authority the host row's table claims.
const CLAIMED_HOST: &str = "claimed.invalid";

/// The authority nothing in that table claims, which is what the host row asks
/// for. Reserved by RFC 2606, so no resolver can answer it.
const UNCLAIMED_HOST: &str = "unclaimed.invalid";

/// The streaming multipart route whose session answers its own peer.
const MULTIPART: &str = "/multipart";

/// The streaming multipart route whose handler outlives its request total.
const SLOW_MULTIPART: &str = "/slow-multipart";

/// The streaming multipart route whose admission refuses the work.
///
/// Refused by the table's own admission policy rather than by a payload
/// maximum, because that is the refusal a session never sees: it is decided
/// from the head, before a boundary is read or a frame is polled.
const REFUSED_MULTIPART: &str = "/refused-multipart";

/// The status the framework gives a request its admission policy declined.
const DECLINED_STATUS: u16 = 503;

/// The streaming multipart route whose answer the peer stops taking.
const DEPARTED_MULTIPART: &str = "/departed-multipart";

/// The byte maximum this table's admission grants every route it admits.
///
/// Well past the framed payload these rows send, so the only row admission
/// answers is the one it is asked to decline.
const MULTIPART_ADMITTED_MAX: usize = 64 * 1024;

/// The payload that route answers with, in bytes.
///
/// Past what a socket buffer absorbs, so a peer that read the committed head and
/// went away leaves this answer genuinely undeliverable rather than sitting
/// complete in the kernel. The head is on the wire either way, which is what the
/// row turns on.
const DEPARTED_BODY_BYTES: usize = 512 * 1024;

/// A multipart representation that declares no boundary at all.
///
/// The route reads the declared boundary before it polls a single frame, so this
/// is the one refusal that reaches the cell with a session that never opened.
const UNFRAMED: &str = "multipart/form-data";

/// The status the built-in mapper gives a multipart request it cannot frame.
const UNFRAMED_STATUS: u16 = 400;

/// The status the built-in mapper gives a request whose total expired.
const EXPIRED_STATUS: u16 = 408;

/// One producer class, and the commitment its own route must settle on.
struct Producer {
    /// What the row is called when an assertion reports it.
    label: &'static str,
    /// The method this row asks with. A row that asks a registered path with an
    /// unregistered method is how the router's method terminal is reached.
    method: &'static str,
    /// The path this row asks for.
    path: &'static str,
    /// The body this row sends, framed so it is read after the head is
    /// admitted rather than refused from a declared length.
    chunks: &'static [&'static [u8]],
    /// The status the peer must read.
    status: u16,
    /// The fact the commitment must settle on.
    committed: ResponseCommit,
    /// How many producers must reach a commitment another owner already held.
    late: usize,
    /// How many times this row's route mapper may run.
    mapper_calls: usize,
    /// The request total this row's operation runs under.
    ///
    /// `None` for every row that answers inside the server's own defaults. A row
    /// naming one is a row whose claim is that the total outlived the producer
    /// its registration mounted, so the total has to be short enough for the
    /// handler to still be running when it expires.
    total: Option<Duration>,
}

/// The producers a buffered local route reaches without leaving the process.
///
/// Every one of them answers from inside Camber, and they come in two kinds: the
/// owners a registration mounted, and the framework answering for an owner that
/// did not. Both kinds are named below, because a row's kind is what decides
/// which of them the commitment is supposed to hold.
fn local_producers() -> Box<[Producer]> {
    mounted_local_producers()
        .into_iter()
        .chain(framework_local_producers())
        .collect()
}

/// The local producers an owner the registration mounted answered for itself.
///
/// The handler, the chain that replaced it, the router's own two terminals, the
/// file served off disk, and an internal route. Four of them are one boxed
/// future by the time dispatch holds them, so they are the rows that state the
/// claim — the producer is named by the registration that mounted it, not
/// derived from its shape.
fn mounted_local_producers() -> [Producer; 6] {
    [
        Producer {
            label: "route handler",
            method: "GET",
            path: "/handled",
            chunks: &[],
            status: 200,
            committed: ResponseCommit::Head(ResponseOrigin::Application),
            late: 0,
            mapper_calls: 0,
            total: None,
        },
        Producer {
            label: "middleware short-circuit on an ordinary route",
            method: "GET",
            path: GATED_ROUTE,
            chunks: &[],
            status: 403,
            committed: ResponseCommit::Head(ResponseOrigin::Middleware),
            late: 0,
            mapper_calls: 0,
            total: None,
        },
        Producer {
            label: "router not-found terminal",
            method: "GET",
            path: "/absent",
            chunks: &[],
            status: 404,
            committed: ResponseCommit::Head(ResponseOrigin::Router),
            late: 0,
            mapper_calls: 1,
            total: None,
        },
        Producer {
            label: "router method terminal",
            method: "DELETE",
            path: "/handled",
            chunks: &[],
            status: 405,
            committed: ResponseCommit::Head(ResponseOrigin::Router),
            late: 0,
            mapper_calls: 1,
            total: None,
        },
        Producer {
            label: "static-file worker",
            method: "GET",
            path: "/files/served.txt",
            chunks: &[],
            status: 200,
            committed: ResponseCommit::Head(ResponseOrigin::StaticFile),
            late: 0,
            mapper_calls: 0,
            total: None,
        },
        Producer {
            label: "Camber-internal route",
            method: "GET",
            path: INTERNAL,
            chunks: &[],
            status: 200,
            committed: ResponseCommit::Head(ResponseOrigin::Internal),
            late: 0,
            mapper_calls: 0,
            total: None,
        },
    ]
}

/// The two shapes the framework's own mapper answers a local operation in.
///
/// It takes the cell where a request total expired on a handler that had not
/// answered, and it arrives late behind a cause the body reader already
/// committed. The second is the set-once claim in its sharpest form: the peer
/// still gets one status, and it is the cause's.
fn framework_local_producers() -> [Producer; 2] {
    [
        Producer {
            label: "framework head at the request total",
            method: "GET",
            path: SLOW,
            chunks: &[],
            status: EXPIRED_STATUS,
            // The handler that outlived this total is answered by the mapper, so
            // the producer that commits is the framework and not the handler
            // still running behind it. A commitment taken where the producer was
            // selected, rather than where it answered, names the handler here.
            committed: ResponseCommit::Head(ResponseOrigin::Framework),
            late: 0,
            mapper_calls: 1,
            total: Some(SLOW_TOTAL),
        },
        Producer {
            label: "framework rejection behind a committed cause",
            method: "POST",
            path: "/handled",
            chunks: &[CROSSING],
            status: 413,
            // The reader committed the cause where it read the crossing frame,
            // so the framework mapper that answers it reaches a cell already
            // held.
            committed: ResponseCommit::Cause(InboundTerminal::RouteBodyLimit),
            late: 1,
            mapper_calls: 1,
            total: None,
        },
    ]
}

/// The local producer only a table of authorities can reach.
///
/// A `HostRouter` asked for an authority no child claims answers from the host
/// stage in front of the children: there is no child chain to unwind through, so
/// the routing decision itself is the producer and the host table's own mapper
/// gives it a body. Every other local row is served on a single table, where
/// that stage does not exist — so without this row `ResponseOrigin::Router` is
/// only ever read back from the two terminals a child router mints, and a
/// commitment that named the framework for an unclaimed authority would pass.
fn host_terminal_producer() -> Producer {
    Producer {
        label: "host terminal for an unclaimed authority",
        method: "GET",
        path: "/handled",
        chunks: &[],
        status: 404,
        committed: ResponseCommit::Head(ResponseOrigin::Router),
        late: 0,
        mapper_calls: 1,
        total: None,
    }
}

/// The table of authorities the host row is served on.
///
/// It claims exactly one authority, and that authority's own table answers the
/// row's path. So a request that reached the claimed child would read `200`, and
/// the `404` the row requires is available only from the stage in front of it.
fn host_commitment_routes(mapped: &Journal) -> HostRouter {
    let mut claimed = Router::new();
    claimed.get("/handled", |_req: &Request| async {
        Response::text(200, "claimed").expect("the claimed authority's answer is representable")
    });
    let mut hosts = HostRouter::new().rejection_mapper(recording_mapper(mapped, "host commitment"));
    hosts.add(CLAIMED_HOST, claimed);
    hosts
}

/// Drive the host row and read back what its operation committed.
fn assert_host_terminal_commitment() {
    let producer = host_terminal_producer();
    let mapped = journal();
    let hosts = host_commitment_routes(&mapped);
    assert_row_commitment(&producer, &mapped, hosts, |addr| {
        wire::send_to_host(addr, producer.method, producer.path, UNCLAIMED_HOST).status
    });
}

/// The table every row above is served through.
///
/// One table rather than one per row, because the claim is about the operation
/// each request mints and not about a route's registration: two rows sharing a
/// handler and differing only in what they send is exactly the pair that would
/// expose a commitment kept per route.
fn commitment_routes(mapped: &Journal, files: &Path) -> Router {
    let mut router = Router::new();
    router.get("/handled", |_req: &Request| async {
        Response::text(200, "handled").expect("the handler's answer is representable")
    });
    router.post("/handled", |_req: &Request| async {
        Response::text(200, "handled").expect("the handler's answer is representable")
    });
    router.get(GATED_ROUTE, |_req: &Request| async {
        Response::text(200, "never entered").expect("the gated handler's answer is representable")
    });
    router.get(SLOW, |_req: &Request| async {
        tokio::time::sleep(SLOW_HANDLER).await;
        Response::text(200, "too late").expect("the slow handler's answer is representable")
    });
    router.static_files(FILES, &files.to_string_lossy());
    // The chain answers only the path it is asked to refuse, so every other row
    // reaches the producer its label names rather than this one. It returns
    // without calling `next.call`, which is what a short-circuit is: the
    // producer behind it never runs, and the head is the chain's own.
    gate(&mut router, &[GATED_ROUTE]);
    router
        .max_request_body(BODY_MAX)
        .rejection_mapper(recording_mapper(mapped, "commitment"))
}

/// Put a chain in front of `routes` that refuses `refused` and admits the rest.
///
/// One gate rather than one per table: a short-circuit is the same shape
/// wherever it runs, and what each table claims with it is which producer the
/// commitment then names — not how the chain refused.
fn gate(routes: &mut Router, refused: &'static [&'static str]) {
    routes.use_middleware(move |req: &Request, next: Next| {
        let admitted = (!refused.contains(&req.path())).then(|| next.call(req));
        async move {
            match admitted {
                Some(answering) => answering.await,
                None => Response::text(403, "gated").expect("the gate's answer is representable"),
            }
        }
    });
}

/// The static-file root the served table's file row asks for.
///
/// The root is the caller's to close, because the runtime the rows run in has to
/// have stopped before the directory goes: a served file is read by a worker
/// this test only knows has finished once its server has joined.
fn served_root() -> TempRoot {
    let files = TempRoot::new().expect("a static-file root for the served row");
    std::fs::write(files.path().join(SERVED_FILE), SERVED_BODY)
        .expect("write the file the static row asks for");
    files
}

/// What one row is served on: a single table, or a table of authorities.
///
/// A host row's producer is reached by the authority its request names and not
/// by the path, so it cannot be registered on a single table at all. Naming both
/// shapes here keeps every row on the one serve, status, commitment, and
/// teardown sequence below.
enum Served {
    Routes(Router),
    Hosts(HostRouter),
}

impl From<Router> for Served {
    fn from(routes: Router) -> Self {
        Self::Routes(routes)
    }
}

impl From<HostRouter> for Served {
    fn from(hosts: HostRouter) -> Self {
        Self::Hosts(hosts)
    }
}

/// The policy a row naming a request total is served under.
fn total_policy(total: Duration) -> ServerPolicy {
    ServerPolicy::default()
        .request_budget(
            RequestBudget::unbounded()
                .with_total(total)
                .expect("the row's request total is accepted"),
        )
        .shutdown_timeout(ANSWER_TIMEOUT)
        .expect("the row's shutdown deadline is accepted")
}

/// Serve one row's table under the request total that row declares.
///
/// The tables differ — bodyless rows, framed multipart rows, refused handshakes,
/// and a table of authorities each have their own — but the policy a row needs
/// is decided the same way in all of them. A row naming no total takes the
/// server's own defaults; a row whose claim is that its total outlived its
/// producer has to name that total, and the deadline its teardown joins under
/// with it.
fn serve_under_total(
    port: wire::ObservedPort<ScopedCommittedAnswer>,
    served: Served,
    total: Option<Duration>,
) -> wire::ObservedServer<ScopedCommittedAnswer> {
    match (served, total) {
        (Served::Routes(routes), None) => port.serve(routes),
        (Served::Routes(routes), Some(total)) => {
            port.serve_with_policy(routes, total_policy(total))
        }
        (Served::Hosts(hosts), None) => port.serve_hosts(hosts),
        (Served::Hosts(hosts), Some(total)) => {
            port.serve_hosts_with_policy(hosts, total_policy(total))
        }
    }
}

/// Serve one row's table, ask it what the row asks, and read back the
/// commitment production wrote.
///
/// The tables differ and so does the request each row sends — a bodyless line,
/// a framed multipart payload, a handshake, or a forwarded path — but the serve,
/// the status check, the commitment read, and the bounded teardown are the same
/// four steps for every one of them. Stated once, because a second copy of them
/// is a second place a row's server can be left running past its own assertion.
fn assert_row_commitment(
    producer: &Producer,
    mapped: &Journal,
    served: impl Into<Served>,
    ask: impl FnOnce(SocketAddr) -> u16,
) {
    let port = wire::reserve_committed_answer();
    let controller = port.controller();
    let server = serve_under_total(port, served.into(), producer.total);

    let status = ask(server.addr());
    assert_eq!(
        status, producer.status,
        "{}: the producer this row names answered",
        producer.label
    );

    await_settled_connections(&controller, producer);
    assert_committed(&controller, producer, mapped);

    server
        .shutdown_bounded(ANSWER_TIMEOUT)
        .unwrap_or_else(|error| panic!("{}: teardown failed: {error}", producer.label));
}

/// Send one row's request and hand back the status the peer read.
///
/// A row that declares its length is refused from the head, before an operation
/// exists to hold a commitment at all. Chunked framing is what puts the crossing
/// frame in front of the body reader that owns the bound.
fn ask_producer(addr: SocketAddr, producer: &Producer) -> u16 {
    match producer.chunks {
        [] => {
            wire::send(
                addr,
                producer.method,
                producer.path,
                &[("Connection", wire::CLOSE_AFTER_RESPONSE)],
                b"",
            )
            .status
        }
        chunks => {
            wire::send_chunked(
                addr,
                wire::CLOSE_AFTER_RESPONSE,
                producer.method,
                producer.path,
                wire::DEFAULT_HOST,
                chunks,
            )
            .unwrap_or_else(|error| {
                panic!(
                    "{}: the chunked upload was answered: {error}",
                    producer.label
                )
            })
            .0
            .status
        }
    }
}

/// Drive one producer class and read back what its operation committed.
fn assert_one_commitment(producer: &Producer, files: &Path) {
    let mapped = journal();
    let routes = commitment_routes(&mapped, files);
    assert_row_commitment(producer, &mapped, routes, |addr| {
        ask_producer(addr, producer)
    });
}

/// Wait until every connection this listener accepted has settled.
///
/// The barrier every row's commitment is read behind. A status the peer holds
/// says the head was produced; it does not say the answer under that head is
/// over. The row this matters for is the one whose peer leaves with payload
/// still to be written: the write fails after the peer is gone, so a cell read
/// in front of that would report "nothing re-decided this head, and no mapper
/// ran" about a window in which the departure had not reached production yet.
///
/// The account a request stages is the wrong fact to wait on. A buffered answer
/// hands its whole payload over before a byte of it is written, so that account
/// is recorded while the departed row's write is still to come. The connection
/// owner's own settlement is the one that cannot precede the write: it is
/// published where the connection this answer travelled on finished, whether the
/// payload reached the peer or failed against a socket nobody was left holding.
///
/// The whole tree is what it waits on, rather than one named connection. This
/// fixture's own readiness probe is a settled connection too, so a row asking
/// only whether some connection had settled would be answered by the probe
/// before its own request had gone out. The row's `ask` has returned a status by
/// the time this runs, so that row's connection is registered — and requiring
/// every registered connection to have settled therefore requires that one to
/// have settled.
fn await_settled_connections(controller: &ScopedCommittedAnswer, producer: &Producer) {
    let connections = &controller.connections;
    let counted = |event: &ConnectionOwnershipEvent| {
        matches!(
            event,
            ConnectionOwnershipEvent::ServerConnectionRegistered { .. }
        )
    };
    let settled = wire::poll_until(ANSWER_TIMEOUT, || {
        let events = connections.observed().events;
        let registered = events.iter().filter(|event| counted(event)).count();
        let settled = events
            .iter()
            .filter(|event| {
                matches!(
                    event,
                    ConnectionOwnershipEvent::ServerConnectionSettled { .. }
                )
            })
            .count();
        registered > 0 && registered == settled
    });
    assert!(
        settled,
        "{}: a connection that carried this answer never settled: {:?}",
        producer.label,
        connections.observed().events
    );
}

/// Read back the commitment this row's operation settled on, and what its
/// route's mapper was asked for.
fn assert_committed(controller: &ScopedCommittedAnswer, producer: &Producer, mapped: &Journal) {
    let commitment = &controller.commitment;
    let observed = commitment.observed();
    assert_eq!(
        observed.commits, 1,
        "{}: exactly one producer took this operation's commitment: {observed:?}",
        producer.label
    );
    assert_eq!(
        observed.distinct_operations, 1,
        "{}: one admitted operation minted one commitment: {observed:?}",
        producer.label
    );
    assert_eq!(
        observed.committed,
        Some(producer.committed),
        "{}: the commitment names the producer that took it: {observed:?}",
        producer.label
    );
    assert_eq!(
        observed.late, producer.late,
        "{}: {} producer(s) reached a commitment already held: {observed:?}",
        producer.label, producer.late
    );
    assert_eq!(
        observed.attempts,
        observed.commits + observed.late,
        "{}: every producer that reached the cell is counted once: {observed:?}",
        producer.label
    );

    let refusals = drain(mapped);
    assert_eq!(
        refusals.len(),
        producer.mapper_calls,
        "{}: this producer owes {} mapper call(s): {refusals:?}",
        producer.label,
        producer.mapper_calls
    );
}

/// How one streaming row's peer takes the answer its operation committed.
///
/// A streaming class can commit a head the peer reads whole, a head whose body
/// the transport then ends, or a head written behind a peer that is already
/// leaving. The three are the same commitment claim under three transports, so
/// the row names which one it drives rather than each row carrying a reader of
/// its own.
#[derive(Clone, Copy)]
enum PeerRead {
    /// The whole framed answer, as any other row reads one.
    Framed,
    /// The committed head, and then whatever the ending names.
    HeadThen(HeadEnding),
}

/// What one post-head row's peer does once it holds the committed head.
///
/// Its own type rather than two more arms above, so the reader that takes a head
/// first cannot be handed a framed row: the two shapes it does serve are the
/// whole of this enum.
#[derive(Clone, Copy)]
enum HeadEnding {
    /// Wait out the transport's own end of the body under the head. A body cut
    /// short is not framed, so it cannot be read as one.
    Close,
    /// Go, with the payload behind the head still being written.
    Depart,
}

/// Read one committed head, and take what `ending` says the peer does next.
///
/// The head is read as bytes rather than as a framed message because neither
/// shape has a frame to read: a body the transport cut has no terminal chunk,
/// and a body nobody is left to receive has no reader. The status is the same
/// fact in both, and it is the fact each row's peer is owed.
fn read_committed_head(
    addr: SocketAddr,
    producer: &Producer,
    headers: &[(&str, &str)],
    body: &[u8],
    ending: HeadEnding,
) -> u16 {
    let label = producer.label;
    let mut peer =
        wire::connect(addr).unwrap_or_else(|error| panic!("{label}: the peer connected: {error}"));
    wire::write_request(&mut peer, producer.method, producer.path, headers, body)
        .unwrap_or_else(|error| panic!("{label}: the request was sent: {error}"));
    let head = wire::read_head(&mut peer, wire::WIRE_TIMEOUT)
        .unwrap_or_else(|error| panic!("{label}: the committed head arrived: {error}"));
    let status = wire::status_from_raw(&String::from_utf8_lossy(&head));
    match ending {
        // Drained to close, so the transfer that carried this head has really
        // ended before the commitment is read. An assertion taken while the feed
        // was still running would be reading a cell no later owner had reached.
        HeadEnding::Close => {
            wire::drain_to_close(&mut peer, wire::WIRE_TIMEOUT)
                .unwrap_or_else(|error| panic!("{label}: the cut body ended: {error}"));
        }
        // Dropped with the payload still coming, which is the row: the head is
        // on the wire and the answer behind it reaches nobody.
        HeadEnding::Depart => drop(peer),
    }
    status
}

/// One server-sent-events producer, and how its peer takes the answer.
struct SseProducer {
    /// What this row must answer with, name itself as, owe its mapper, and run
    /// under — read back by the assertion every other producer row is read back
    /// by.
    producer: Producer,
    /// How this row's peer takes the answer its operation committed.
    read: PeerRead,
}

/// The four producers one event-stream table can commit.
///
/// The class has one head of its own and three ways to be answered by somebody
/// else, and every one of them is a different owner: the chain that replaced the
/// feed before it opened, the framework mapper for a chain the request total
/// outlived, the handoff's own `200`, and that same handoff's `200` behind a body
/// the registered maximum cut. The last is the phase claim — a committed head is
/// not re-decided by what happens to the body under it.
fn sse_producers() -> [SseProducer; 4] {
    [
        SseProducer {
            producer: Producer {
                label: "middleware short-circuit at an event-stream gate",
                method: "GET",
                path: GATED,
                chunks: &[],
                status: 403,
                committed: ResponseCommit::Head(ResponseOrigin::Middleware),
                late: 0,
                mapper_calls: 0,
                total: None,
            },
            read: PeerRead::Framed,
        },
        SseProducer {
            producer: Producer {
                label: "framework head at a request total the event-stream gate outlived",
                method: "GET",
                path: SLOW_EVENTS,
                chunks: &[],
                status: EXPIRED_STATUS,
                // The handoff commits its own head where dispatch reaches it, so
                // a row that expires in front of that is the only pre-head this
                // class has. The producer is the mapper, and the feed never
                // opened.
                committed: ResponseCommit::Head(ResponseOrigin::Framework),
                late: 0,
                mapper_calls: 1,
                total: Some(SLOW_TOTAL),
            },
            read: PeerRead::Framed,
        },
        SseProducer {
            producer: Producer {
                label: "server-sent-events handoff",
                method: "GET",
                path: EVENTS,
                chunks: &[],
                status: 200,
                committed: ResponseCommit::Head(ResponseOrigin::ServerSentEvents),
                late: 0,
                mapper_calls: 0,
                total: None,
            },
            read: PeerRead::Framed,
        },
        SseProducer {
            producer: Producer {
                label: "event-stream feed cut by its registered payload maximum",
                method: "GET",
                path: CAPPED_EVENTS,
                chunks: &[],
                // The status the handoff already committed, which the peer holds
                // before a single event is charged. A transfer that ends the body
                // under it may not replace it, and no mapper may build a second
                // one.
                status: 200,
                committed: ResponseCommit::Head(ResponseOrigin::ServerSentEvents),
                late: 0,
                mapper_calls: 0,
                total: None,
            },
            read: PeerRead::HeadThen(HeadEnding::Close),
        },
    ]
}

/// The event-stream table every row above is served through.
///
/// Its own table rather than rows on the local one, because two of these routes
/// exist only to be answered by somebody other than the handoff: one is refused
/// by the chain in front of it and one outlives the total that chain runs under.
fn sse_routes(mapped: &Journal) -> Router {
    let mut router = Router::new();
    router.get_sse(GATED, |_req: &Request, writer: &mut SseWriter| {
        writer.event("message", "never published")
    });
    router.get_sse(SLOW_EVENTS, |_req: &Request, writer: &mut SseWriter| {
        writer.event("message", "never reached")
    });
    router.get_sse(EVENTS, |_req: &Request, writer: &mut SseWriter| {
        writer.event("message", "committed")
    });
    router.get_sse_with_budget(
        CAPPED_EVENTS,
        camber::http::TransferBudget::unbounded()
            .with_max_bytes(CAPPED_EVENT_MAX)
            .expect("the capped feed's payload maximum is accepted"),
        |_req: &Request, writer: &mut SseWriter| {
            // Published until the owner that charges these bytes stops reading
            // them, which is the crossing this row drives. The send fails once
            // the transfer has ended, and the feed returns on that failure.
            loop {
                writer.event("message", CAPPED_EVENT_DATA)?;
                std::thread::sleep(CAPPED_EVENT_PACE);
            }
        },
    );
    gate(&mut router, &[GATED]);
    stall(&mut router, &[SLOW_EVENTS]);
    router.rejection_mapper(recording_mapper(mapped, "sse-commitment"))
}

/// Put a chain in front of `routes` that outlives the request total on every
/// path in `stalled`.
///
/// Its own frame rather than a second behaviour inside [`gate`]: a chain that
/// refuses and a chain that never returns in time are two different producers of
/// the head, and a helper that did both would decide which by a flag. One frame
/// over a list rather than one frame per path, for [`gate`]'s reason: a table
/// stalling two classes would otherwise wrap two chains around every request it
/// admits.
fn stall(routes: &mut Router, stalled: &'static [&'static str]) {
    routes.use_middleware(move |req: &Request, next: Next| {
        let admitted = (!stalled.contains(&req.path())).then(|| next.call(req));
        async move {
            match admitted {
                Some(answering) => answering.await,
                None => {
                    tokio::time::sleep(SLOW_HANDLER).await;
                    Response::text(200, "too late")
                        .expect("the stalled chain's answer is representable")
                }
            }
        }
    });
}

/// Drive one event-stream producer and read back what its operation committed.
fn assert_sse_commitment(row: &SseProducer) {
    let mapped = journal();
    let routes = sse_routes(&mapped);
    let producer = &row.producer;
    assert_row_commitment(producer, &mapped, routes, |addr| match row.read {
        PeerRead::Framed => ask_producer(addr, producer),
        PeerRead::HeadThen(ending) => read_committed_head(addr, producer, &[], b"", ending),
    });
}

/// The backend both proxied registrations forward their sessions to.
///
/// A real echoing peer rather than an address nothing answers on, because the
/// accepted proxied row's claim is that a `101` was produced and the bridge
/// behind it took the transport. A backend that could not be reached would leave
/// the same status on the wire with nothing behind it.
#[cfg(feature = "ws")]
fn websocket_backend() -> Router {
    let mut backend = Router::new();
    backend.ws(UPSTREAM_ECHO, echo_until_closed);
    backend
}

/// Echo every message back until the peer closes the session.
///
/// Shared by the direct routes and the backend behind the proxied ones, so both
/// classes' accepted rows exercise the same session rather than one echoing and
/// one hanging up.
#[cfg(feature = "ws")]
fn echo_until_closed(
    _req: &Request,
    mut conn: camber::http::WsConn,
) -> Result<(), camber::RuntimeError> {
    while let Some(message) = conn.recv() {
        match conn.send(&message) {
            Ok(()) => {}
            Err(_gone) => break,
        }
    }
    Ok(())
}

/// The four upgrade registrations every row below is served through.
///
/// One table over both classes, because what separates the rows is the phase
/// each handshake reaches and not where it was pointed: two routes are asked for
/// with a head negotiation refuses, two are held behind a chain the request total
/// outlives, and two complete.
#[cfg(feature = "ws")]
fn websocket_routes(mapped: &Journal, backend: SocketAddr) -> Router {
    let upstream = format!("http://{backend}").into_boxed_str();
    let mut router = Router::new();
    router.ws(DIRECT_WEBSOCKET, echo_until_closed);
    router.ws(SLOW_DIRECT_WEBSOCKET, echo_until_closed);
    router.proxy_stream(PROXIED_WEBSOCKET, &upstream);
    router.proxy_stream(SLOW_PROXIED_WEBSOCKET, &upstream);
    stall(
        &mut router,
        &[SLOW_DIRECT_WEBSOCKET, SLOW_PROXIED_WEBSOCKET_PATH],
    );
    router.rejection_mapper(recording_mapper(mapped, "websocket-commitment"))
}

/// One upgrade producer: the handshake it sends, and what its peer is left
/// holding.
#[cfg(feature = "ws")]
struct UpgradeProducer {
    /// What this row must answer with, name itself as, owe its mapper, and run
    /// under — read back by the assertion every other producer row is read back
    /// by.
    producer: Producer,
    /// The handshake head this row sends to the path its producer names.
    request: fn(&str) -> Box<str>,
    /// What the answer to that handshake leaves the peer holding.
    answer: UpgradeAnswer,
}

/// What one upgrade row's peer is left holding.
///
/// The whole of the class: an upgrade is either refused in front of the `101` —
/// leaving an ordinary response head and no bridge — or produced, leaving a
/// session the peer owns until it closes it. A row names which, rather than each
/// row carrying a reader of its own.
#[cfg(feature = "ws")]
#[derive(Clone, Copy)]
enum UpgradeAnswer {
    /// The framework answered in front of the handoff.
    Refused,
    /// The handoff produced its `101` and a bridge took the transport.
    Bridged,
}

/// A handshake declaring a version Camber does not speak.
#[cfg(feature = "ws")]
fn unsupported_upgrade(path: &str) -> Box<str> {
    crate::ws_support::ws_upgrade_request(path)
        .replace("Sec-WebSocket-Version: 13", "Sec-WebSocket-Version: 8")
        .into_boxed_str()
}

/// A conforming handshake declaring an origin this server does not admit.
#[cfg(feature = "ws")]
fn cross_origin_upgrade(path: &str) -> Box<str> {
    crate::ws_support::ws_upgrade_request_with(path, &[("Origin", "http://elsewhere.invalid")])
}

/// The eight producers the two upgrade classes can commit.
///
/// Four phases, both classes, and no phase is registration-specific: a
/// handshake refused on its declared version, a handshake refused on its
/// declared origin, a request total that outlived the chain in front of the
/// handoff, and a handoff that produced its `101`. The first three are the
/// framework answering for an upgrade that never happened, and only the last is
/// the upgrade itself.
///
/// Both classes carry every phase because their dispatch shapes differ. A direct
/// upgrade is a mounted route and a proxied one is a forward with a prefix, so a
/// commitment wired to one of them names nothing for the other — and an
/// operation that ends with an empty cell is a Camber-produced answer the
/// completion finalizer records as Hyper's.
#[cfg(feature = "ws")]
fn upgrade_producers() -> [UpgradeProducer; 8] {
    [
        refused_upgrade(
            "invalid direct WebSocket handshake",
            DIRECT_WEBSOCKET,
            426,
            unsupported_upgrade,
        ),
        refused_upgrade(
            "invalid proxied WebSocket handshake",
            PROXIED_WEBSOCKET_PATH,
            426,
            unsupported_upgrade,
        ),
        refused_upgrade(
            "cross-origin direct WebSocket handshake",
            DIRECT_WEBSOCKET,
            403,
            cross_origin_upgrade,
        ),
        refused_upgrade(
            "cross-origin proxied WebSocket handshake",
            PROXIED_WEBSOCKET_PATH,
            403,
            cross_origin_upgrade,
        ),
        expired_upgrade(
            "framework head at a request total the direct handshake's gate outlived",
            SLOW_DIRECT_WEBSOCKET,
        ),
        expired_upgrade(
            "framework head at a request total the proxied handshake's gate outlived",
            SLOW_PROXIED_WEBSOCKET_PATH,
        ),
        bridged_upgrade("direct WebSocket handoff", DIRECT_WEBSOCKET),
        bridged_upgrade("proxied WebSocket handoff", PROXIED_WEBSOCKET_PATH),
    ]
}

/// One row whose handshake negotiation refused before a `101` could exist.
///
/// The mapper answers, so the framework is the producer and owes exactly one
/// call. Nothing may commit `WebSocket` here: no bridge was offered the
/// transport, and a cell holding the handoff would credit an owner that never
/// ran.
#[cfg(feature = "ws")]
const fn refused_upgrade(
    label: &'static str,
    path: &'static str,
    status: u16,
    request: fn(&str) -> Box<str>,
) -> UpgradeProducer {
    UpgradeProducer {
        producer: Producer {
            label,
            method: "GET",
            path,
            chunks: &[],
            status,
            committed: ResponseCommit::Head(ResponseOrigin::Framework),
            late: 0,
            mapper_calls: 1,
            total: None,
        },
        request,
        answer: UpgradeAnswer::Refused,
    }
}

/// One row whose chain outlived the request total in front of the handoff.
///
/// The only pre-head this class has that is not a refused negotiation: the
/// handshake was conforming and the upgrade would have been produced, and the
/// total expired before dispatch reached it. The producer is the mapper, and the
/// bridge never opened.
#[cfg(feature = "ws")]
const fn expired_upgrade(label: &'static str, path: &'static str) -> UpgradeProducer {
    UpgradeProducer {
        producer: Producer {
            label,
            method: "GET",
            path,
            chunks: &[],
            status: EXPIRED_STATUS,
            committed: ResponseCommit::Head(ResponseOrigin::Framework),
            late: 0,
            mapper_calls: 1,
            total: Some(SLOW_TOTAL),
        },
        request: crate::ws_support::ws_upgrade_request,
        answer: UpgradeAnswer::Refused,
    }
}

/// One row whose handoff produced its `101` and handed the transport over.
///
/// The one phase that commits `WebSocket`, and the one that owes its mapper
/// nothing: the peer holds the status before a frame is exchanged, and the
/// session ending afterwards is the transport's own business rather than a
/// second answer to this request.
#[cfg(feature = "ws")]
const fn bridged_upgrade(label: &'static str, path: &'static str) -> UpgradeProducer {
    UpgradeProducer {
        producer: Producer {
            label,
            method: "GET",
            path,
            chunks: &[],
            status: UPGRADED_STATUS,
            committed: ResponseCommit::Head(ResponseOrigin::WebSocket),
            late: 0,
            mapper_calls: 0,
            total: None,
        },
        request: crate::ws_support::ws_upgrade_request,
        answer: UpgradeAnswer::Bridged,
    }
}

/// Send one row's handshake and hand back the status the peer read.
#[cfg(feature = "ws")]
fn ask_upgrade(addr: SocketAddr, row: &UpgradeProducer) -> u16 {
    let producer = &row.producer;
    let label = producer.label;
    let head = (row.request)(producer.path);
    let mut peer = crate::ws_support::start_upgrade_with(addr, &head);
    match row.answer {
        UpgradeAnswer::Refused => {
            wire::read_http_response_bounded(&mut peer)
                .unwrap_or_else(|error| panic!("{label}: the refusal was not answered: {error}"))
                .status
        }
        UpgradeAnswer::Bridged => bridged_session(&mut peer, label),
    }
}

/// Take the `101` this row's handshake produced, use the session, and close it.
///
/// The status is read off the head rather than through the HTTP reader, because
/// a `101` frames no body for that reader to end on. The round trip after it is
/// what makes the row a produced handoff: a bridge that never took the transport
/// cannot return the frame. The close is the peer's own, so the connection this
/// answer travelled on ends on a completed session rather than on a socket
/// dropped mid-frame.
///
/// A status that is not the handoff's ends here untouched. The row's caller is
/// what reports it, and exchanging frames on a socket carrying an ordinary
/// refusal would panic on the read instead of naming the answer.
#[cfg(feature = "ws")]
fn bridged_session(peer: &mut std::net::TcpStream, label: &str) -> u16 {
    let head = crate::ws_support::read_until_double_crlf(peer);
    let status = wire::status_from_raw(&head);
    match status {
        UPGRADED_STATUS => {
            crate::ws_support::write_ws_text_frame(peer, BRIDGED_MESSAGE);
            let echoed = crate::ws_support::read_ws_text_frame(peer);
            assert_eq!(
                &*echoed, BRIDGED_MESSAGE,
                "{label}: the session behind this committed `101` carried a frame",
            );
            crate::ws_support::write_ws_close_frame(peer);
        }
        _ => {}
    }
    status
}

/// Drive one upgrade producer and read back what its operation committed.
#[cfg(feature = "ws")]
fn assert_upgrade_commitment(row: &UpgradeProducer, backend: SocketAddr) {
    let mapped = journal();
    let routes = websocket_routes(&mapped, backend);
    let producer = &row.producer;
    assert_row_commitment(producer, &mapped, routes, |addr| ask_upgrade(addr, row));
}

/// One streaming multipart producer, and the commitment its own class settles
/// on.
///
/// The class has five commit sites and no other, so it has five rows: the
/// admission that declined the work before a boundary was read, the declared
/// boundary the route could not read, the request total that outlived a session
/// which had opened, the session's own settled answer, and that same settled
/// answer whose payload the peer left before taking. Each writes the cell from a
/// different place in the route, and a row missing here is a producer the
/// completion finalizer would report as a head Hyper wrote.
struct MultipartProducer {
    /// What this row must answer with, name itself as, owe its mapper, and run
    /// its session under — read back by the assertion every other producer row
    /// is read back by.
    producer: Producer,
    /// The representation this row declares, which is what decides whether the
    /// route can frame a session at all.
    content_type: &'static str,
    /// How this row's peer takes the answer its operation committed.
    read: PeerRead,
}

/// The five producers one streaming multipart table can commit.
///
/// Two kinds, named apart below: the sessions a handler answered for itself,
/// and the requests the framework answered for a handler that never got the
/// chance. A row's kind is what decides which owner its cell is supposed to
/// hold.
fn multipart_producers() -> Box<[MultipartProducer]> {
    settled_multipart_producers()
        .into_iter()
        .chain(framework_multipart_producers())
        .collect()
}

/// The multipart rows a mounted handler answered for itself.
///
/// One session settles and its answer is read whole; the other settles the same
/// way and its payload is left behind by a peer that took the head and went. The
/// pair is the phase claim for this class: the head is the handler's in both,
/// and what happens to the payload under it changes neither the origin nor the
/// count.
fn settled_multipart_producers() -> [MultipartProducer; 2] {
    [
        MultipartProducer {
            producer: Producer {
                label: "streaming multipart session",
                method: "POST",
                path: MULTIPART,
                chunks: &[],
                status: 200,
                committed: ResponseCommit::Head(ResponseOrigin::Application),
                late: 0,
                mapper_calls: 0,
                total: None,
            },
            content_type: DECLARED,
            read: PeerRead::Framed,
        },
        MultipartProducer {
            producer: Producer {
                label: "multipart answer whose payload its peer left before taking",
                method: "POST",
                path: DEPARTED_MULTIPART,
                chunks: &[],
                // The handler's own status, read by this peer before it left.
                // What happens to the payload behind it is the transport's, and
                // the answer the operation committed is not re-decided by it.
                status: 200,
                committed: ResponseCommit::Head(ResponseOrigin::Application),
                late: 0,
                mapper_calls: 0,
                total: None,
            },
            content_type: DECLARED,
            read: PeerRead::HeadThen(HeadEnding::Depart),
        },
    ]
}

/// The three multipart rows the framework's own mapper answered.
///
/// Admission declined the work before a boundary was read; a boundary the route
/// could not read left no session to answer; and a request total outlived a
/// session that had opened. Each reaches the cell from a different place in the
/// route, and an empty cell in any of them is Camber refusing a request and
/// reporting the refusal as Hyper's own.
fn framework_multipart_producers() -> [MultipartProducer; 3] {
    [
        MultipartProducer {
            producer: Producer {
                label: "multipart work this table's admission declined",
                method: "POST",
                path: REFUSED_MULTIPART,
                chunks: &[],
                status: DECLINED_STATUS,
                committed: ResponseCommit::Head(ResponseOrigin::Framework),
                late: 0,
                mapper_calls: 1,
                total: None,
            },
            content_type: DECLARED,
            read: PeerRead::Framed,
        },
        MultipartProducer {
            producer: Producer {
                label: "multipart boundary the route could not read",
                method: "POST",
                path: MULTIPART,
                chunks: &[],
                status: UNFRAMED_STATUS,
                committed: ResponseCommit::Head(ResponseOrigin::Framework),
                late: 0,
                mapper_calls: 1,
                total: None,
            },
            content_type: UNFRAMED,
            read: PeerRead::Framed,
        },
        MultipartProducer {
            producer: Producer {
                label: "multipart session outlived by its request total",
                method: "POST",
                path: SLOW_MULTIPART,
                chunks: &[],
                status: EXPIRED_STATUS,
                // The handler was still holding this session when its total
                // expired, so the head belongs to the framework mapper and not
                // to the handler the registration mounted.
                committed: ResponseCommit::Head(ResponseOrigin::Framework),
                late: 0,
                mapper_calls: 1,
                total: Some(SLOW_TOTAL),
            },
            content_type: DECLARED,
            read: PeerRead::Framed,
        },
    ]
}

/// The multipart table every row above is served through.
///
/// Its own table rather than a pair of routes on [`commitment_routes`], because
/// these rows send a framed payload under a declared representation and that
/// table's rows are bodyless under a payload maximum of [`BODY_MAX`].
fn multipart_routes(mapped: &Journal) -> Router {
    let mut router = Router::new();
    router.multipart(
        Method::Post,
        MULTIPART,
        MultipartLimits::builder()
            .build()
            .expect("the multipart row's limits are accepted"),
        |_req: &Request, fields: MultipartStream| async move {
            drained(fields).await?;
            Response::text(200, "uploaded")
        },
    );
    router.multipart(
        Method::Post,
        REFUSED_MULTIPART,
        MultipartLimits::builder()
            .build()
            .expect("the declined row's limits are accepted"),
        |_req: &Request, fields: MultipartStream| async move {
            drained(fields).await?;
            Response::text(200, "never entered")
        },
    );
    router.multipart(
        Method::Post,
        DEPARTED_MULTIPART,
        MultipartLimits::builder()
            .build()
            .expect("the departed row's limits are accepted"),
        |_req: &Request, fields: MultipartStream| async move {
            drained(fields).await?;
            Response::text(200, &"?".repeat(DEPARTED_BODY_BYTES))
        },
    );
    router.multipart(
        Method::Post,
        SLOW_MULTIPART,
        MultipartLimits::builder()
            .build()
            .expect("the slow multipart row's limits are accepted"),
        |_req: &Request, fields: MultipartStream| async move {
            drained(fields).await?;
            // Still holding the session when the total expires, which is what
            // makes the framework's head the one this request gets.
            tokio::time::sleep(SLOW_HANDLER).await;
            Response::text(200, "too late")
        },
    );
    // One policy over the whole table, declining exactly the route whose row
    // turns on it. A refusal registered on its own table would be a second
    // server, and the claim is about the operation this request minted rather
    // than about which registration answered it.
    router
        .body_admission(|context: &BodyAdmissionContext<'_>| {
            match context.route() == REFUSED_MULTIPART {
                true => Err(camber::RuntimeError::InvalidArgument(
                    "this table declines the work behind the refused route".into(),
                )),
                false => Ok(BodyAdmission::new(MULTIPART_ADMITTED_MAX)),
            }
        })
        .rejection_mapper(recording_mapper(mapped, "multipart-commitment"))
}

/// Read every field one multipart session offers.
async fn drained(mut fields: MultipartStream) -> Result<(), camber::RuntimeError> {
    while let Some(field) = fields.next_field().await? {
        field.discard().await?;
    }
    Ok(())
}

/// Drive one multipart producer class and read back what its operation
/// committed.
fn assert_multipart_commitment(row: &MultipartProducer) {
    let mapped = journal();
    let routes = multipart_routes(&mapped);
    let producer = &row.producer;
    let headers = [
        ("Connection", wire::CLOSE_AFTER_RESPONSE),
        ("Content-Type", row.content_type),
    ];
    let body = multipart_body(BOUNDARY, &[Field::text("note", "hello")]);
    assert_row_commitment(producer, &mapped, routes, |addr| match row.read {
        PeerRead::Framed => {
            wire::send(addr, producer.method, producer.path, &headers, &body).status
        }
        PeerRead::HeadThen(ending) => read_committed_head(addr, producer, &headers, &body, ending),
    });
}

/// One buffered proxy phase, and how far its forward got before it ended.
struct BufferedProxyPhase {
    /// What this row must answer with, name itself as, and owe its mapper —
    /// read back by the assertion every other producer row is read back by.
    producer: Producer,
    /// How many response heads the upstream produced for this row.
    ///
    /// What separates the two failing rows. Both are answered `502` by the same
    /// mapper and both commit the framework, so the status, the origin, and the
    /// mapper count together still cannot say whether the forward ended before
    /// an upstream head existed or after one this proxy had already accepted.
    /// Only the upstream's own record of what it answered says which phase ran,
    /// and without it the post-head row is the pre-head row again under a
    /// second name.
    upstream_heads: usize,
}

/// The phases one buffered forward can end in.
///
/// A buffered proxy is one producer registration and four different answers, and
/// the registration cannot tell them apart: the route names an upstream, and
/// whether an upstream head ever reached this operation — and whether the
/// payload behind it was one this route could carry — is decided inside the
/// forward. Each row drives one phase over the same three registrations.
///
/// The rows are the three response phases this class has. Two of them fail: one
/// before any upstream head exists and one after a head this proxy accepted, and
/// they are not the same fact. The first has no upstream answer to keep; the
/// second has one and still does not give it to the peer, because a buffered
/// forward puts nothing on the wire until the whole payload is in hand. What
/// separates them from the two the upstream answered is who produced the head
/// the peer read.
fn buffered_proxy_producers() -> [BufferedProxyPhase; 4] {
    [
        BufferedProxyPhase {
            producer: Producer {
                label: "buffered forward that never reached an upstream head",
                method: "GET",
                path: UNREACHED_PROXY_PATH,
                chunks: &[],
                status: UNREACHABLE_STATUS,
                // Nothing was dialled, so no upstream produced anything. The
                // framework's mapper answered this peer, and a cell naming the
                // upstream here credits a head that never existed.
                committed: ResponseCommit::Head(ResponseOrigin::Framework),
                late: 0,
                mapper_calls: 1,
                total: None,
            },
            upstream_heads: 0,
        },
        BufferedProxyPhase {
            producer: Producer {
                label: "buffered forward that carried an upstream head",
                method: "GET",
                path: ANSWERED_PROXY_PATH,
                chunks: &[],
                status: 200,
                committed: ResponseCommit::Head(ResponseOrigin::Upstream),
                late: 0,
                mapper_calls: 0,
                total: None,
            },
            upstream_heads: 1,
        },
        BufferedProxyPhase {
            producer: Producer {
                label: "buffered forward whose upstream head reported its own failure",
                method: "GET",
                path: REFUSED_PROXY_PATH,
                chunks: &[],
                // The upstream's own status, not the gateway status the first
                // row maps. Past the head this forward carried, the route's
                // mapper has no authority left: a failure the upstream reported
                // is the answer, and remapping it would give this peer a second
                // one.
                status: UPSTREAM_REFUSED_STATUS,
                committed: ResponseCommit::Head(ResponseOrigin::Upstream),
                late: 0,
                mapper_calls: 0,
                total: None,
            },
            upstream_heads: 1,
        },
        BufferedProxyPhase {
            producer: Producer {
                label: "buffered forward that failed after its upstream head arrived",
                method: "GET",
                path: OVERSIZED_PROXY_PATH,
                chunks: &[],
                // The gateway status, not the `200` the upstream stated. A
                // buffered forward holds its answer until the whole payload is
                // collected, so a payload this route may not carry leaves the
                // accepted head undeliverable and the peer is told what actually
                // happened to its request.
                status: UNREACHABLE_STATUS,
                // The framework's, and the row that says why: an upstream head
                // arrived, so a commitment written where the head was received
                // names the upstream — and then this peer reads a `502` no
                // upstream sent, under the one origin that is supposed to mean
                // it did. The producer of the head this operation ended on is
                // the mapper.
                committed: ResponseCommit::Head(ResponseOrigin::Framework),
                late: 0,
                mapper_calls: 1,
                total: None,
            },
            upstream_heads: 1,
        },
    ]
}

/// The upstream the answering rows forward to.
///
/// A served Camber router rather than a raw socket, because these rows turn on
/// the head the forward carried and not on how it was framed: one path answers,
/// one reports its own failure, one answers and then states more payload than
/// the route asking for it may carry, and the proxy has to keep them apart.
///
/// Every path counts its own head into `heads`, which is the only place a row
/// can learn that this upstream answered at all. The proxy's own answer cannot
/// say so: the gateway status a forward that never dialled is mapped to and the
/// one a collected payload is refused with are the same status, built by the
/// same mapper, over the same commitment.
fn buffered_proxy_upstream(heads: &Arc<AtomicUsize>) -> Router {
    let mut router = Router::new();
    let answered = Arc::clone(heads);
    router.get(UPSTREAM_ANSWER, move |_req: &Request| {
        answered.fetch_add(1, Ordering::SeqCst);
        async { Response::text(200, "forwarded").expect("the upstream's answer is representable") }
    });
    let refused = Arc::clone(heads);
    router.get(UPSTREAM_REFUSAL, move |_req: &Request| {
        refused.fetch_add(1, Ordering::SeqCst);
        async {
            Response::text(UPSTREAM_REFUSED_STATUS, "the upstream refused this request")
                .expect("the upstream's refusal is representable")
        }
    });
    let oversized = Arc::clone(heads);
    router.get(UPSTREAM_OVERSIZED, move |_req: &Request| {
        oversized.fetch_add(1, Ordering::SeqCst);
        // A perfectly good answer, which is the point: the head is one this
        // proxy accepts, and only the payload behind it is past what the route
        // that asked for it froze.
        async {
            Response::text(200, OVERSIZED_BODY)
                .expect("the upstream's oversized answer is representable")
        }
    });
    router
}

/// The three buffered proxy registrations every row above is served through.
///
/// One table, because the claim is about the forward each request makes and not
/// about the registration: every route is `Router::proxy` over the same
/// upstream leg, and what differs is whether an upstream is there to answer and
/// whether its payload is one the asking route may carry.
fn buffered_proxy_routes(mapped: &Journal, upstream: SocketAddr) -> Router {
    let backend = format!("http://{upstream}").into_boxed_str();
    let mut router = Router::new();
    router.proxy(UNREACHED_PROXY, UNDIALED_BACKEND);
    router.proxy(ANSWERED_PROXY, &backend);
    router.proxy_with_policy(
        OVERSIZED_PROXY,
        &backend,
        ProxyPolicy::default()
            .buffered_response_limit(BUFFERED_MAX)
            .expect("the oversized row's buffered maximum is accepted"),
    );
    router.rejection_mapper(recording_mapper(mapped, "buffered-proxy-commitment"))
}

/// Drive one buffered proxy phase and read back what its operation committed.
///
/// The upstream's own count is read after the row's server has joined, so what
/// it reports is everything this row's forward asked of it and nothing a later
/// row will.
fn assert_buffered_proxy_commitment(
    phase: &BufferedProxyPhase,
    upstream: SocketAddr,
    heads: &Arc<AtomicUsize>,
) {
    let producer = &phase.producer;
    heads.store(0, Ordering::SeqCst);
    let mapped = journal();
    let routes = buffered_proxy_routes(&mapped, upstream);
    assert_row_commitment(producer, &mapped, routes, |addr| {
        ask_producer(addr, producer)
    });
    assert_eq!(
        heads.load(Ordering::SeqCst),
        phase.upstream_heads,
        "{}: the upstream produced {} head(s) for this row",
        producer.label,
        phase.upstream_heads
    );
}

/// Invariants 9 and 10, over the buffered proxy's own three response phases
///
/// A buffered forward is one registration with four endings across the three
/// phases a response has, and the producer that answers is different in each. A
/// forward that never reached an upstream head was answered by the framework's
/// mapper; a forward that carried one was answered by the upstream; a head that
/// reported the upstream's own failure is still the upstream's answer, which the
/// route's mapper may not replace; and a forward that failed after its upstream
/// head arrived is the framework's again, because a buffered answer is not on
/// the wire until its payload is collected and this one never got there.
///
/// The last two rows are what the phase count buys. Both have an upstream head
/// in hand and only one of them gives the upstream the commitment, so a proxy
/// naming its producer from the point the head was received passes the refusal
/// row and credits the upstream for a gateway status it never sent.
///
/// The registration cannot decide this on its own. It names an upstream because
/// that is what the route forwards to, and a commitment written from the
/// registration credits the upstream for the gateway refusal Camber built when
/// no upstream was ever reached — which is the defect this row set is written
/// against.
///
/// Every row reads the cell production wrote, requires exactly one producer to
/// have taken it, and requires the mapper to run once for each forward the
/// framework answered and not at all for the two the upstream answered. The
/// mapper count is what separates the rows a status alone cannot: a `503` the
/// upstream sent and a `502` the mapper built are both failures on the wire, and
/// only the record of who was asked tells them apart.
///
/// What it does not claim: nothing here is the streaming proxy's. That class
/// answers its pre-head rows in
/// [`streaming_proxy_producers_name_actual_producer`], commits its head at its
/// own barrier, and orders that head against an upload's crossing, which
/// `acceptance_proxy::body_admission` and `acceptance_proxy::transfer_budgets`
/// own.
#[test]
fn buffered_proxy_failure_and_success_name_actual_producer() {
    common::test_runtime()
        .with_metrics()
        .run(|| {
            let heads = Arc::new(AtomicUsize::new(0));
            let upstream = common::spawn_server(buffered_proxy_upstream(&heads));
            for phase in &buffered_proxy_producers() {
                assert_buffered_proxy_commitment(phase, upstream, &heads);
            }
            // The upstream is the runtime's own child, and the rows above own
            // only the servers they forwarded through. Asked to stop here so it
            // drains inside this runtime's shutdown deadline rather than being
            // outstanding when the scope is joined.
            camber::runtime::request_shutdown();
        })
        .expect("the buffered proxy runtime ran");
}

/// The producers a streaming forward reaches before any upstream head exists.
///
/// Two rows, because a streaming forward has two ways to end without one and
/// they name different owners. A chain in front of the forward answers from
/// Camber and never dials at all; a chain that admitted it leaves a forward that
/// really ran and really failed, and the only producer left is the mapper that
/// answers the peer.
///
/// Neither of them is the buffered class's. A streaming registration never
/// reaches the buffered dispatch those rows finish through, so a commitment
/// wired only there leaves this operation's cell empty and reports Hyper as the
/// producer of Camber's own refusal.
fn streaming_proxy_producers() -> [Producer; 2] {
    [
        Producer {
            label: "middleware short-circuit on a streaming forward",
            method: "GET",
            path: GATED_PROXY,
            chunks: &[],
            status: 403,
            committed: ResponseCommit::Head(ResponseOrigin::Middleware),
            late: 0,
            mapper_calls: 0,
            total: None,
        },
        Producer {
            label: "framework rejection of an unreachable upstream",
            method: "GET",
            path: FAILED_PROXY,
            chunks: &[],
            status: UNREACHABLE_STATUS,
            // The chain admitted this one and the forward really ran, so the row
            // is the class doing its work and still finding no upstream head to
            // commit. The mapper that answers is then the producer, and a cell
            // left empty here is a Camber-produced refusal the completion
            // finalizer would record as Hyper's.
            committed: ResponseCommit::Head(ResponseOrigin::Framework),
            late: 0,
            mapper_calls: 1,
            total: None,
        },
    ]
}

/// The two streaming proxy registrations both rows above are served through.
///
/// One table over one unreachable backend, because what separates the rows is
/// how far each request got and not where it was pointed: the chain refuses one
/// of the two paths, and the other is really dialled and really fails.
fn streaming_proxy_routes(mapped: &Journal) -> Router {
    let mut router = Router::new();
    router.proxy_stream(GATED_PROXY, UNDIALED_BACKEND);
    router.proxy_stream(FAILED_PROXY, UNDIALED_BACKEND);
    gate(&mut router, &[GATED_PROXY]);
    router.rejection_mapper(recording_mapper(mapped, "streaming-proxy-commitment"))
}

/// Drive one streaming proxy producer and read back what its operation
/// committed.
fn assert_streaming_proxy_commitment(producer: &Producer) {
    let mapped = journal();
    let routes = streaming_proxy_routes(&mapped);
    assert_row_commitment(producer, &mapped, routes, |addr| {
        ask_producer(addr, producer)
    });
}

/// Invariants 9 and 10, over the streaming proxy's pre-head producers
///
/// A streaming forward commits its upstream head at its own barrier, and these
/// are the rows that end before that barrier is reached. The chain that
/// short-circuited in front of the forward produced its own head; the forward
/// that was admitted, dialled, and refused has no upstream head to commit, so
/// the framework's mapper produced the one the peer read.
///
/// The class is why they are here rather than beside the buffered rows. A
/// streaming registration is dispatched down its own path and finishes nowhere
/// near buffered dispatch, so a commitment wired only where a buffered forward
/// ends names nothing at all for either of these — and an operation that ends
/// with an empty cell is a Camber-produced refusal the completion finalizer
/// records as Hyper's.
///
/// Each row reads the cell production wrote, requires exactly one producer to
/// have taken it, and requires the mapper to run only for the row whose answer
/// the framework built. The mapper count is what separates the two: both peers
/// read a status Camber decided, and only the record of who was asked says
/// which owner decided it.
///
/// What it does not claim: no committed streaming head is here. That fact is
/// taken at the upstream-head barrier and ordered against an upload's crossing,
/// which `acceptance_proxy::body_admission` and
/// `acceptance_proxy::transfer_budgets` own, and its post-head behaviour is
/// `acceptance_proxy::framework_rejections`'.
#[test]
fn streaming_proxy_producers_name_actual_producer() {
    common::test_runtime()
        .with_metrics()
        .run(|| {
            for producer in &streaming_proxy_producers() {
                assert_streaming_proxy_commitment(producer);
            }
        })
        .expect("the streaming proxy runtime ran");
}

/// Invariants 9 and 10, over the streaming application producers
///
/// Server-sent events and streaming multipart are the two classes whose payload
/// outlives the head that announced it, in opposite directions: the feed streams
/// its answer out and the session streams its request in. Both reach the one
/// commitment their operation has, and both have producers a registration cannot
/// name on its own.
///
/// The event-stream rows are the four owners this class can be answered by. The
/// chain that replaced the feed before it opened is the middleware's own head;
/// the chain the request total outlived is the framework mapper's, and it is the
/// only pre-head this class has, because the handoff commits where dispatch
/// reaches it; the feed that opened is the handoff's own `200`; and the feed cut
/// by its registered payload maximum is that same `200`, still, with the body
/// under it ended.
///
/// The multipart rows are the five its route can commit. Admission declined the
/// work before a boundary was read; a boundary the route could not read left no
/// session to answer; a request total outlived a session that had opened; the
/// settled session produced the handler's own head; and that same head stands
/// over a payload its peer left before taking.
///
/// The two post-head rows are what the phase count buys. Both hold a committed
/// head the peer is already holding, and in both the transport then ends — one
/// because the feed crossed the maximum it was registered under, one because
/// there is nobody left to write to. Neither may re-decide the answer, and
/// neither may reach a mapper: a commitment written where the failure was
/// noticed rather than where the head was produced would replace a status the
/// peer has already read.
///
/// Every row reads the cell production wrote, requires exactly one producer to
/// have taken it, requires each later producer to be counted without replacing
/// it, and requires the mapper to run once for each head the framework built and
/// not at all for the heads its owners produced.
///
/// What it does not claim: nothing here is the proxy's or the protocol
/// handoff's. Those families take their own boundaries, and the transfer
/// ordering these rows sit under is
/// `component_streaming::transfer_budgets::transfer_first_commit_survives_later_higher_rank_event`'s.
#[test]
fn sse_and_multipart_producers_commit_once() {
    common::test_runtime()
        .with_metrics()
        .run(|| {
            for row in &sse_producers() {
                assert_sse_commitment(row);
            }
            for row in &multipart_producers() {
                assert_multipart_commitment(row);
            }
        })
        .expect("the streaming application runtime ran");
}

/// Invariants 9 and 10, over both protocol-handoff upgrade classes
///
/// An upgrade has one commit site and three ways to be answered in front of it,
/// and the two classes reach all four down different dispatch paths. A direct
/// upgrade is a mounted route; a proxied one is a forward under a prefix. Both
/// are driven here through the same four phases, because a commitment wired to
/// one shape names nothing for the other.
///
/// The three refusals are the framework's own heads. A handshake declaring a
/// version Camber does not speak and one declaring an origin this server does
/// not admit are refused by negotiation; a conforming handshake behind a chain
/// the request total outlived never reaches negotiation at all. None of them may
/// commit `WebSocket`: no bridge was offered the transport, so a cell naming the
/// handoff would credit an owner that never ran. Each row requires exactly one
/// producer to have reached the cell, which is what states that absence rather
/// than asserting it — a `WebSocket` attempt beaten to the cell would still be
/// counted.
///
/// The two accepted rows are the only phase that commits `WebSocket`. Each reads
/// the `101` its peer holds, exchanges a frame through the session behind it,
/// and closes. The frame is what separates a produced handoff from a status: a
/// bridge that never took the transport cannot return it. The session ending
/// afterwards reaches no mapper, because a committed head is not re-decided by
/// what the transport under it does next.
///
/// What it does not claim: nothing here is gRPC's. That handoff commits at
/// tonic's head and orders it against this operation's carried budgets, which
/// `acceptance_e2e::cross_protocol_service_operation` owns.
#[cfg(feature = "ws")]
#[test]
fn websocket_handoffs_and_refusals_name_actual_producer() {
    common::test_runtime()
        .with_metrics()
        .run(|| {
            let backend = common::spawn_server(websocket_backend());
            for row in &upgrade_producers() {
                assert_upgrade_commitment(row, backend);
            }
            // The backend is the runtime's own child, and the rows above own only
            // the servers they upgraded through. Asked to stop here so it drains
            // inside this runtime's shutdown deadline rather than being
            // outstanding when the scope is joined.
            camber::runtime::request_shutdown();
        })
        .expect("the protocol handoff runtime ran");
}

/// Invariant 9, over the producers a local buffered route can reach
///
/// The set-once claim at the boundary that owns it. Every producer here answers
/// from inside Camber, without an upstream, a handoff, or a transfer between the
/// registration and the peer: the mounted handler, the chain that replaced it,
/// the router's not-found and method terminals, the file served off disk, an
/// internal route, the framework's head at an expired request total, and its
/// mapper arriving late behind a cause the body reader already committed.
///
/// The last row is served on a table of authorities rather than a single table,
/// because the producer it names exists only there: an authority no child claims
/// is answered by the host stage in front of the children, and no single-table
/// row can reach that stage at all.
///
/// Each row reads the cell production wrote, requires the origin to be the owner
/// that actually produced the head, and requires every later producer to be
/// counted without replacing what is held. The counting is what makes it a
/// set-once claim: a row asserting only the origin would pass on a cell that
/// took every writer in turn and happened to end on the right one.
///
/// The rows are also the local half of the producer table. A router terminal, a
/// served file, an internal route, and an application handler are one boxed
/// future by the time dispatch holds them, so a commitment reading the origin off
/// the dispatch shape answers `application` for all four, and this test names
/// each of them separately.
///
/// What it does not claim: no proxy, streaming, or protocol-handoff producer is
/// here. Those families take their own boundaries, and a local matrix that
/// reached into them would be the broad test again under a narrower name.
#[test]
fn local_response_producers_commit_once() {
    let files = served_root();
    let root = files.path();
    common::test_runtime()
        .with_metrics()
        .run(|| {
            for producer in &local_producers() {
                assert_one_commitment(producer, root);
            }
            assert_host_terminal_commitment();
        })
        .expect("the local producer runtime ran");
    files.close().expect("the static-file root was removed");
}
