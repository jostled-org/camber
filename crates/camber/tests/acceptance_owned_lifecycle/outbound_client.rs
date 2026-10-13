//! Outbound 4.T2: an outbound call a handler awaits inline ends with the
//! runtime's shutdown and with its inbound peer, and ends on its own without
//! either.
//!
//! The journey rows run a real Camber runtime and listener. A served handler
//! awaits the public client inline against a scripted raw upstream, and a row
//! acts only after that upstream has confirmed the phase the call is held in.
//! The upstream runs on a runtime of its own, so the runtime a row shuts down
//! owns nothing of the peer that observes it.
//!
//! The rows without a Camber runtime are separate tests: bare Tokio has no
//! shutdown source, and a thread with no executor has no timer at all.
//!
//! # Streaming 5.T3 and 5.T4
//!
//! A streamed response keeps one owner task alive after the head is returned.
//! 5.T3 proves that task belongs to the Camber runtime that first polled the
//! send. 5.T4 proves the handle owns it on bare Tokio, on one thread and on
//! several.
//!
//! The rows read `ScopedTransferOwner::streamed()`, the production body
//! owner's counts for the calls to one peer. It is the doc-hidden, read-only
//! view the streaming rows of `component_http_routing` describe. A row that
//! ends before a head has no owner view, and reads its peer.
//!
//! Two rows also hold the owner at `TransferOwnerEdge::BeforeHeadDelivery`,
//! the moment it has the final head and has not handed it over. The hold is
//! owner-local. It delivers nothing, and it chooses no terminal: each row
//! drops the waiting send itself. The 5.T3 row drops it through the inbound
//! peer's disconnect and then lets the owner go. The 5.T4 row aborts the
//! send's task and never lets the owner go, so only the handle's abort can
//! end that owner.
//!
//! | Clause | Row | Mutation |
//! |---|---|---|
//! | Shutdown ends every phase, with no second demand | `assert_held_stream_ends`, `Trigger::Shutdown` | drop root admission, or leave the body unowned |
//! | A disconnect drops the awaiting future and its handle | `assert_held_stream_ends`, `Trigger::Disconnect` | omit the handoff guard |
//! | The root joins the owner | `assert_held_stream_ends`, `assert_moved_handle_leaves_its_join_with_the_root` | give the join to the handle |
//! | A scope close with no shutdown releases a retained handle | `assert_scope_close_releases_a_retained_handle` | stop the owner on the shutdown latch alone |
//! | Closed admission sends nothing | `assert_closed_admission_sends_nothing` | spawn before admission |
//! | The first poll names the runtime | `assert_first_poll_runtime_owns_the_stream` | capture at description |
//! | A moved handle keeps its runtime | `assert_moved_handle_leaves_its_join_with_the_root` | read the ambient runtime after movement |
//! | A transferred handle is not adopted | `assert_transferred_handle_outlives_its_handler` | cancel every stream a handler opened |
//! | A failed head delivery leaves no orphan | `assert_failed_head_delivery_leaves_no_orphan` | omit the handoff guard |
//! | An awaited end or failure acknowledges release | `assert_bare_end_acknowledges_release`, `assert_bare_failure_acknowledges_release` | acknowledge before release |
//! | A close acknowledges release | `assert_bare_close_acknowledges_release` | answer the close on the committed fact |
//! | Drop releases without a caller poll | `assert_bare_drop_releases`, `assert_bare_deadline_releases_a_retained_handle` | ignore the abandonment, or stop the owner on a demand alone |
//! | A dropped bare owner aborts its task | `assert_bare_dropped_send_aborts_a_held_owner` | detach the bare handle |
//! | An abandoned demand is cancellation | `assert_bare_abandoned_demand_is_cancellation` | drop the cancellation guard |
//! | A lost owner is a transport failure | `assert_bare_task_loss_is_transport` | report EOF for a lost owner |
//! | Status conversion only requests release | `assert_bare_status_conversion_requests_release` | convert status into a second terminal, or block in it |
//! | A send dropped beside its head leaves no orphan | `assert_bare_send_dropped_beside_its_head` | omit the handoff guard |
//! | No executor is `NoRuntime` before any spawn | `assert_stream_without_an_executor_is_no_runtime`, `assert_mocked_stream_without_an_executor_is_no_runtime` | spawn before the executor check |
//!
//! The current-thread rows of 5.T4 are the proof that the owner releases its
//! source before it acknowledges. The row reads the count at the return, and
//! an owner that yields between the two is read before its release on every
//! pass. The multithread rows read the same count while the owner runs on
//! another thread. They repeat, but an owner that acknowledges and releases
//! with no await between has passed them: they do not prove the order alone.

use crate::disconnect::fixture::{BOUND, with_drained_server};
use crate::disconnect::peer::start_request;
use crate::disconnect::routes::probe_router;
use crate::disconnect::servers::with_owned_server;
use crate::retry_upstream::{
    Answer, CHUNKED, Call, Pace, Paced, PacedUpstream, PeerEvent, Recording, RunnableDriver,
    ScriptedUpstream, StreamHead, StreamNext, advance_to, answered, assert_released, close, demand,
    describe, failed_as, must_settle, on_peers, owner_must_reach, owner_parked, owner_reached,
    paced_chunk, paced_end, paced_head, parked, peer_runtime, poll_once, read_out, saw_each,
    settle_wakeups, settled, spawn_get, spawn_stream_get, stream_get, wait_for_body_collection,
    was_cancelled, watch_streams, within_watchdog,
};

use camber::__private::ClientBodyObservation;
use camber::__private::ClientDnsEdge;
use camber::__private::client_dns_edge;
use camber::http;
use camber::http::Client;
use camber::http::ClientBody;
use camber::http::ClientError;
use camber::http::ClientResponse;
use camber::http::ClientStreamResponse;
use camber::http::ClientTimeout;
use camber::http::EgressPolicy;
use camber::http::mock::{self, ScopedTransferOwner, TransferOwnerEdge};
use camber::http::{Request, Response, Router};
use camber::{RuntimeError, runtime};
use futures_util::FutureExt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::task::Poll;
use std::time::Duration;
use tokio::sync::Notify;
use tokio::time::Instant;

/// The route whose handler awaits a call the upstream holds.
const HELD_PATH: &str = "/held-call";

/// The route whose handler answers with what its call returned.
const RELAY_PATH: &str = "/relayed-call";

/// The one path every call requests of the upstream.
const UPSTREAM_PATH: &str = "/outbound-lifetime";

/// Every client bound of a journey row. No row waits this long, so only the
/// row's own shutdown or disconnect can end a held call.
const UNREACHED: Duration = Duration::from_secs(60);

/// The base backoff of journey rows that cross a delay on their way to a later
/// attempt. Real time crosses it, so it is short.
const CROSSED_BACKOFF: Duration = Duration::from_millis(5);

/// The retry count of every row that has retries to spare.
const SPARE_RETRIES: u32 = 2;

/// A client whose every bound outlasts the row that sends through it.
fn unbounded_client() -> Client {
    http::client()
        .total_timeout(UNREACHED)
        .connect_timeout(UNREACHED)
        .request_timeout(UNREACHED)
        .response_idle_timeout(UNREACHED)
}

/// A client whose total bound alone outlasts the row; every other bound is
/// the default.
fn total_only_client() -> Client {
    http::client().total_timeout(UNREACHED)
}

/// Where one journey row holds the handler's call.
#[derive(Clone, Copy, Debug)]
enum Held {
    /// The first attempt's response head, with no retries configured.
    Head,
    /// The first attempt's final body, with no retries configured.
    Body,
    /// A later attempt's response head, after a transient answer.
    LaterHead,
    /// A later attempt's final body, after a transient answer.
    LaterBody,
    /// The delay after a transient answer.
    Backoff,
}

const HELD: [Held; 5] = [
    Held::Head,
    Held::Body,
    Held::LaterHead,
    Held::LaterBody,
    Held::Backoff,
];

impl Held {
    /// The held call's script, then one complete answer for the sibling call a
    /// disconnect row sends through the same client.
    fn script(self) -> &'static [Answer] {
        match self {
            Self::Head => &[Answer::StallHead, Answer::Complete],
            Self::Body => &[Answer::StallBody, Answer::Complete],
            Self::LaterHead => &[Answer::Transient, Answer::StallHead, Answer::Complete],
            Self::LaterBody => &[Answer::Transient, Answer::StallBody, Answer::Complete],
            Self::Backoff => &[Answer::Transient, Answer::Complete],
        }
    }

    fn retries(self) -> u32 {
        match self {
            Self::Head | Self::Body => 0,
            Self::LaterHead | Self::LaterBody | Self::Backoff => SPARE_RETRIES,
        }
    }

    fn backoff(self) -> Duration {
        match self {
            Self::Backoff => UNREACHED,
            Self::Head | Self::Body | Self::LaterHead | Self::LaterBody => CROSSED_BACKOFF,
        }
    }

    /// What the upstream reports, in order, once the call is held here.
    fn reached(self) -> &'static [PeerEvent] {
        match self {
            Self::Head | Self::Body => &[PeerEvent::Started(0), PeerEvent::Stalled(0)],
            Self::LaterHead | Self::LaterBody => &[
                PeerEvent::Started(0),
                PeerEvent::Disposed(0),
                PeerEvent::Started(1),
                PeerEvent::Stalled(1),
            ],
            Self::Backoff => &[PeerEvent::Started(0), PeerEvent::Disposed(0)],
        }
    }

    /// The attempt whose transport is open while the call is held, or `None`
    /// when the call is held between attempts.
    fn in_flight(self) -> Option<usize> {
        match self {
            Self::Head | Self::Body => Some(0),
            Self::LaterHead | Self::LaterBody => Some(1),
            Self::Backoff => None,
        }
    }

    /// Every request start the held call has made, and all it may make.
    fn starts(self) -> u32 {
        match self {
            Self::Head | Self::Body | Self::Backoff => 1,
            Self::LaterHead | Self::LaterBody => 2,
        }
    }

    /// A client whose every bound outlasts the row.
    fn client(self) -> Client {
        unbounded_client()
            .retries(self.retries())
            .backoff(self.backoff())
    }
}

/// How one run of the held route's handler ended.
#[derive(Debug)]
enum HandlerEnd {
    /// The awaited call returned this status or this failure.
    Returned(Result<u16, RuntimeError>),
    /// The handler future was dropped with its call still pending.
    Dropped,
}

/// Reports how one handler run ended, from wherever that run stops.
///
/// A handler cannot report its own cancellation: the server drops its future.
/// The report is sent from `Drop`, which runs on both paths.
struct EndReport {
    ends: Sender<HandlerEnd>,
    returned: Option<Result<u16, RuntimeError>>,
}

impl EndReport {
    /// A report that reads as dropped until the handler's call returns.
    fn new(ends: &Sender<HandlerEnd>) -> Self {
        Self {
            ends: ends.clone(),
            returned: None,
        }
    }

    /// Report what the handler's call returned, then answer the inbound peer
    /// with 200 for a status and 502 for a failure.
    fn answer(mut self, returned: Result<u16, RuntimeError>) -> Result<Response, RuntimeError> {
        let status = returned.as_ref().map_or(502, |_| 200);
        self.returned = Some(returned);
        drop(self);
        Response::empty(status)
    }
}

impl Drop for EndReport {
    fn drop(&mut self) {
        let end = self
            .returned
            .take()
            .map_or(HandlerEnd::Dropped, HandlerEnd::Returned);
        // A closed receiver means the row has already finished with this route.
        let _ = self.ends.send(end);
    }
}

/// A router whose two outbound routes share one client.
///
/// The held route awaits its call inline and reports how its run ended. The
/// relay route answers its inbound peer with the status and body its own call
/// returned.
fn outbound_router(client: Client, upstream_url: String, ends: Sender<HandlerEnd>) -> Router {
    let client = Arc::new(client);
    let url: Arc<str> = upstream_url.into();
    let mut router = probe_router();

    let (held_client, held_url) = (Arc::clone(&client), Arc::clone(&url));
    router.get(HELD_PATH, move |_req: &Request| {
        let client = Arc::clone(&held_client);
        let url = Arc::clone(&held_url);
        let end = EndReport::new(&ends);
        async move {
            let result: Result<ClientResponse, RuntimeError> = client.get(&url).await;
            end.answer(result.map(|response| response.status()))
        }
    });

    router.get(RELAY_PATH, move |_req: &Request| {
        let client = Arc::clone(&client);
        let url = Arc::clone(&url);
        async move {
            let answer: ClientResponse = client.get(&url).await?;
            Response::bytes(answer.status(), answer.into_body())
        }
    });
    router
}

/// Whether the held attempt's transport was closed by the client, or what the
/// upstream reported instead.
async fn released_in_flight(held: Held, upstream: &mut ScriptedUpstream) -> Result<(), String> {
    match held.in_flight() {
        Some(attempt) => upstream.saw(PeerEvent::Released(attempt)).await,
        None => Ok(()),
    }
}

/// Release the upstream and report every request start it saw.
///
/// The driver exists only for the release: the peer runtime's clock is real.
async fn finished(upstream: ScriptedUpstream, context: &str) -> u32 {
    let driver = RunnableDriver::start();
    upstream.finish(driver, context).await
}

/// What one request through the relay route returned, and whether the
/// upstream served it.
struct Relayed {
    status: u16,
    body: Box<[u8]>,
    served: Result<(), String>,
}

/// Send one request through the relay route, and require the upstream to
/// answer it as attempt `attempt`.
fn relay(
    peers: &tokio::runtime::Runtime,
    upstream: &mut ScriptedUpstream,
    addr: SocketAddr,
    attempt: usize,
) -> Result<Relayed, String> {
    let response = crate::common::request(addr, "GET", RELAY_PATH, &[], &[], BOUND)
        .map_err(|error| format!("the relay request was never answered: {error}"))?;
    let served = on_peers(
        peers,
        saw_each(
            upstream,
            &[PeerEvent::Started(attempt), PeerEvent::Answered(attempt)],
        ),
    );
    Ok(Relayed {
        status: response.status,
        body: response.body,
        served,
    })
}

/// Require the relay route to have returned the upstream's complete answer.
fn assert_relayed(relayed: Result<Relayed, String>, context: &str) {
    let relayed = match relayed {
        Ok(relayed) => relayed,
        Err(unanswered) => panic!("{context}: {unanswered}"),
    };
    assert_eq!(relayed.status, 200, "{context}: the relayed status");
    assert_eq!(&*relayed.body, b"ok", "{context}: the relayed body");
    if let Err(observed) = relayed.served {
        panic!("{context}: the upstream did not serve the relayed call: {observed}");
    }
}

/// What ends the held call.
#[derive(Clone, Copy, Debug)]
enum Trigger {
    /// Request runtime shutdown through the public control.
    Shutdown,
    /// Close the inbound peer whose handler awaits the call.
    Disconnect,
}

impl Trigger {
    /// Whether the handler's run ended the way this trigger requires.
    ///
    /// Shutdown is a result: the call returns `Cancelled` to its handler. A
    /// disconnect is no result: the server drops the handler, and the pending
    /// call with it.
    fn ended(self, end: &Option<HandlerEnd>) -> bool {
        matches!(
            (self, end),
            (
                Self::Shutdown,
                Some(HandlerEnd::Returned(Err(RuntimeError::Cancelled)))
            ) | (Self::Disconnect, Some(HandlerEnd::Dropped))
        )
    }

    /// Apply this trigger to the inbound peer of a held handler.
    ///
    /// A shutdown returns the peer still open. The row keeps it until the
    /// handler has reported, so the peer's close cannot be what ended the
    /// handler. A disconnect closes the peer and returns nothing.
    fn apply(self, inbound: TcpStream) -> Option<TcpStream> {
        match self {
            Self::Shutdown => {
                runtime::request_shutdown();
                Some(inbound)
            }
            Self::Disconnect => {
                drop(inbound);
                None
            }
        }
    }

    /// Run `row` against a server of `router`, on the server this trigger
    /// needs.
    ///
    /// A disconnect row also reports whether the root scope drained to zero
    /// children. A shutdown row has no such report.
    fn serve<T>(self, router: Router, row: impl FnOnce(SocketAddr) -> T) -> (T, Option<bool>) {
        match self {
            Self::Shutdown => (with_owned_server(router, row), None),
            Self::Disconnect => {
                let (observed, drained) = with_drained_server(None, router, row);
                (observed, Some(drained))
            }
        }
    }
}

/// Everything one journey row observed before its runtime returned.
struct Observed {
    /// Whether the upstream confirmed the phase the call was to be held in.
    reached: Result<(), String>,
    /// How the handler's run ended, or `None` if it never reported.
    end: Option<HandlerEnd>,
    /// Whether the client closed the held attempt's transport.
    released: Result<(), String>,
    /// What a sibling call through the same client returned afterwards.
    sibling: Option<Result<Relayed, String>>,
}

/// One journey row's fixtures, as the runtime closure drives them.
struct Journey<'a> {
    peers: &'a tokio::runtime::Runtime,
    upstream: &'a mut ScriptedUpstream,
    ended: &'a Receiver<HandlerEnd>,
    held: Held,
    trigger: Trigger,
    observed: &'a ScopedTransferOwner,
}

impl Journey<'_> {
    /// Hold the handler's call, apply the trigger, and collect what followed.
    ///
    /// Nothing is asserted here. The row asserts after the runtime has
    /// returned and the upstream is released.
    fn run(self, addr: SocketAddr) -> Observed {
        let inbound = start_request(addr, HELD_PATH);
        let reached = on_peers(self.peers, async {
            saw_each(self.upstream, self.held.reached()).await?;
            if matches!(self.held, Held::Body | Held::LaterBody) {
                wait_for_body_collection(self.observed).await?;
            }
            Ok(())
        });
        match reached {
            Ok(()) => self.end(addr, inbound),
            Err(unreached) => Observed {
                reached: Err(unreached),
                end: None,
                released: Ok(()),
                sibling: None,
            },
        }
    }

    /// Apply the trigger to a held call and collect what followed.
    ///
    /// A shutdown row keeps its inbound peer open until the handler has
    /// reported, so the peer's close cannot be what ended the call.
    fn end(self, addr: SocketAddr, inbound: TcpStream) -> Observed {
        let kept_open = self.trigger.apply(inbound);
        let end = self.ended.recv_timeout(BOUND).ok();
        drop(kept_open);
        let released = on_peers(self.peers, released_in_flight(self.held, self.upstream));
        let sibling = match self.trigger {
            Trigger::Shutdown => None,
            Trigger::Disconnect => Some(relay(
                self.peers,
                self.upstream,
                addr,
                self.held.starts() as usize,
            )),
        };
        Observed {
            reached: Ok(()),
            end,
            released,
            sibling,
        }
    }
}

/// One journey row: a served handler awaits a call the upstream holds, the
/// trigger ends it, and the peer, the handler, and the runtime all settle.
///
/// A shutdown row requires `Cancelled` in the handler and a runtime that
/// returns cleanly. A disconnect row requires the handler dropped, the root
/// scope drained to zero children, and a sibling call through the same client
/// still answered. Both require the held transport closed and no request
/// start the row did not ask for.
fn assert_held_call_ends(held: Held, trigger: Trigger) {
    let context = format!("{trigger:?} with the handler's call held at {held:?}");
    let peers = peer_runtime();
    let mut upstream = on_peers(&peers, ScriptedUpstream::bind(held.script()));
    let collected = watch_streams(upstream.addr());
    let (ends, ended) = channel();
    let router = outbound_router(held.client(), upstream.url(UPSTREAM_PATH), ends);
    let journey = Journey {
        peers: &peers,
        upstream: &mut upstream,
        ended: &ended,
        held,
        trigger,
        observed: &collected,
    };

    let (observed, drained) = trigger.serve(router, |addr| journey.run(addr));
    let starts = on_peers(&peers, finished(upstream, &context));

    if let Err(unreached) = observed.reached {
        panic!("{context}: the call was never held: {unreached}");
    }
    assert!(
        trigger.ended(&observed.end),
        "{context}: the handler's run ended as {:?}",
        observed.end
    );
    assert_released(observed.released, &context);
    assert_ne!(
        drained,
        Some(false),
        "{context}: the scope drain never reached zero children"
    );
    let sibling_starts = match observed.sibling {
        Some(relayed) => {
            assert_relayed(relayed, &context);
            1
        }
        None => 0,
    };
    assert_eq!(
        starts,
        held.starts() + sibling_starts,
        "{context}: the upstream saw {starts} request starts"
    );
}

/// The wiring row: a served handler awaits the public client inline, and its
/// inbound peer receives what the upstream answered.
fn assert_handler_call_answers() {
    const CONTEXT: &str = "a handler's inline call, answered";

    let peers = peer_runtime();
    let mut upstream = on_peers(&peers, ScriptedUpstream::bind(&[Answer::Complete]));
    let (ends, _ended) = channel();
    let router = outbound_router(Held::Head.client(), upstream.url(UPSTREAM_PATH), ends);

    let relayed = with_owned_server(router, |addr| relay(&peers, &mut upstream, addr, 0));
    let starts = on_peers(&peers, finished(upstream, CONTEXT));

    assert_relayed(relayed, CONTEXT);
    assert_eq!(
        starts, 1,
        "{CONTEXT}: the upstream saw {starts} request starts"
    );
}

/// Outbound 4.T2
#[test]
fn outbound_operation_consumes_runtime_shutdown_and_disconnect() {
    assert_handler_call_answers();
    for trigger in [Trigger::Shutdown, Trigger::Disconnect] {
        for held in HELD {
            assert_held_call_ends(held, trigger);
        }
    }
}

/// The operation total of the bare-Tokio rows.
const BARE_TOTAL: Duration = Duration::from_secs(5);

/// The base backoff of the bare-Tokio rows.
const BARE_BACKOFF: Duration = Duration::from_secs(1);

/// A client whose only bound short enough to matter is the operation total.
fn bare_client(retries: u32) -> Client {
    unbounded_client()
        .total_timeout(BARE_TOTAL)
        .retries(retries)
        .backoff(BARE_BACKOFF)
}

/// One GET parked on a head the upstream never sends, under a paused clock.
struct BareRow {
    driver: RunnableDriver,
    upstream: ScriptedUpstream,
    entry: Instant,
    call: Call,
}

impl BareRow {
    async fn parked(retries: u32, context: &str) -> Self {
        let driver = RunnableDriver::start();
        let mut upstream = ScriptedUpstream::bind(&[Answer::StallHead]).await;
        let entry = Instant::now();
        let call = spawn_get(bare_client(retries), upstream.url(UPSTREAM_PATH));
        upstream
            .expect_each(&[PeerEvent::Started(0), PeerEvent::Stalled(0)], context)
            .await;
        Self {
            driver,
            upstream,
            entry,
            call,
        }
    }

    /// Step past every timer the call held, then release the fixture and
    /// report the request starts the upstream saw.
    async fn finish(driver: RunnableDriver, upstream: ScriptedUpstream, context: &str) -> u32 {
        tokio::time::advance(BARE_TOTAL + 2 * BARE_BACKOFF).await;
        settle_wakeups().await;
        upstream.finish(driver, context).await
    }
}

/// With no Camber runtime, the operation total still ends a stalled call.
async fn assert_bare_call_expires(retries: u32) {
    let context = format!("bare Tokio expiry with {retries} retries");
    let BareRow {
        driver,
        mut upstream,
        entry,
        call,
    } = BareRow::parked(retries, &context).await;

    advance_to(entry + BARE_TOTAL).await;
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    let released = upstream.saw(PeerEvent::Released(0)).await;
    let starts = BareRow::finish(driver, upstream, &context).await;

    assert!(
        matches!(
            result,
            Some(Err(RuntimeError::Client(ClientError::Timeout(
                ClientTimeout::Operation
            ))))
        ),
        "{context}: returned {}",
        describe(&result)
    );
    assert_released(released, &context);
    assert_eq!(starts, 1, "{context}: the call started {starts} requests");
}

/// With no Camber runtime, dropping a pending call still releases its
/// transport and sends nothing later.
async fn assert_bare_call_drops(retries: u32) {
    let context = format!("bare Tokio drop with {retries} retries");
    let BareRow {
        driver,
        mut upstream,
        call,
        ..
    } = BareRow::parked(retries, &context).await;

    call.abort();
    let cancelled = was_cancelled(call).await;
    let released = upstream.saw(PeerEvent::Released(0)).await;
    let starts = BareRow::finish(driver, upstream, &context).await;

    assert!(cancelled, "{context}: the dropped call was not cancelled");
    assert_released(released, &context);
    assert_eq!(starts, 1, "{context}: the call started {starts} requests");
}

/// Outbound 4.T2, bare Tokio: deadline and drop need no Camber runtime.
#[tokio::test(start_paused = true)]
async fn outbound_operation_expires_and_drops_on_bare_tokio() {
    for retries in [0, SPARE_RETRIES] {
        assert_bare_call_expires(retries).await;
        assert_bare_call_drops(retries).await;
    }
}

/// Outbound 4.T2, no executor: a call polled with no Tokio runtime returns
/// `NoRuntime`. It does not panic, and it dials nothing.
#[test]
fn outbound_send_without_an_executor_returns_no_runtime() {
    assert_unexecuted_send_is_no_runtime("a call polled with no executor", |client, url| {
        let polled: Option<Result<ClientResponse, RuntimeError>> = client.get(url).now_or_never();
        polled.map(|returned| returned.map(|response| response.status()))
    });
}

/// Poll one send at a listener with no Tokio runtime, and require `NoRuntime`
/// and no dial.
///
/// `poll` polls its terminal once and reports the status or the failure.
fn assert_unexecuted_send_is_no_runtime(
    context: &str,
    poll: impl FnOnce(&Client, &str) -> Option<Result<u16, RuntimeError>>,
) {
    let listener = crate::common::UndialedListener::bind();
    let url = format!("http://{}{UPSTREAM_PATH}", listener.addr());
    let client = total_only_client();

    let polled = poll(&client, &url);

    assert!(
        matches!(polled, Some(Err(RuntimeError::NoRuntime))),
        "{context}: returned {polled:?}"
    );
    listener.assert_undialed(context);
}

/// The canned body of the mock a call with no executor must not reach.
const MOCK_BODY: &str = "mock-answer";

/// Outbound 4.T2, no executor, mocked: a mocked call enters the operation, so
/// one polled with no Tokio runtime returns `NoRuntime` before the mock
/// answers.
///
/// The same call on an executor is then answered by the mock, so the
/// registration matches the refused call too. The mock is called once in all:
/// a call it answered with no executor would make that count two.
#[test]
fn mocked_send_without_an_executor_returns_no_runtime() {
    let url = "http://outbound-lifetime.mock.test/no-executor";
    let canned = Response::text(200, MOCK_BODY).expect("a valid mock status");
    let registered = mock::http(url).returns(canned);
    let client = total_only_client();

    let polled: Option<Result<ClientResponse, RuntimeError>> = client.get(url).now_or_never();
    let answered: Result<ClientResponse, RuntimeError> =
        Flavor::CurrentThread.runtime().block_on(client.get(url));

    assert!(
        matches!(polled, Some(Err(RuntimeError::NoRuntime))),
        "a mocked call polled with no executor returned {}",
        describe(&polled)
    );
    let response: ClientResponse = answered.expect("the mock answers a call on an executor");
    assert_eq!(
        response.body_bytes(),
        MOCK_BODY.as_bytes(),
        "the mock's own answer must arrive"
    );
    registered.assert_called_once();
}

/// The one chunk the streaming peers send before they wait.
const FIRST_CHUNK: &[u8] = b"first";

/// The name the lookup-held row resolves through a controlled lookup.
const STREAM_NAME: &str = "held.stream.test";

/// The address every streaming peer listens on.
const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

/// The origin that names the peer at `addr` as [`STREAM_NAME`].
fn stream_origin(addr: SocketAddr) -> String {
    format!("http://{STREAM_NAME}:{}", addr.port())
}

/// Where one streaming journey row holds the handler.
#[derive(Clone, Copy, Debug)]
enum StreamHeld {
    /// The lookup of the name, before any dial.
    Dns,
    /// The response head, with no retries configured.
    Headers,
    /// The delay after a transient answer.
    RetryDelay,
    /// The head was handed to the handler, which holds the unread response.
    HeadHandoff,
    /// A demand the peer does not answer.
    WaitingDemand,
    /// A chunk the owner admitted and the handler has not taken.
    UndeliveredChunk,
    /// A body the handler read one chunk of, with no demand placed.
    NoDemand,
}

const STREAM_HELD: [StreamHeld; 7] = [
    StreamHeld::Dns,
    StreamHeld::Headers,
    StreamHeld::RetryDelay,
    StreamHeld::HeadHandoff,
    StreamHeld::WaitingDemand,
    StreamHeld::UndeliveredChunk,
    StreamHeld::NoDemand,
];

impl StreamHeld {
    /// What the peer answers every request with.
    fn steps(self) -> Vec<Pace> {
        match self {
            Self::Dns | Self::Headers => vec![Pace::UntilClosed],
            Self::RetryDelay => vec![
                Pace::Send(paced_head(
                    "503 Service Unavailable",
                    &[("Content-Length", "100"), ("Connection", "keep-alive")],
                )),
                Pace::UntilClosed,
            ],
            Self::HeadHandoff | Self::WaitingDemand => held_head_steps(),
            Self::UndeliveredChunk | Self::NoDemand => held_body_steps(),
        }
    }

    /// The URL the handler requests of the peer at `addr`.
    fn url(self, addr: SocketAddr) -> String {
        match self {
            Self::Dns => format!("{}{UPSTREAM_PATH}", stream_origin(addr)),
            Self::Headers
            | Self::RetryDelay
            | Self::HeadHandoff
            | Self::WaitingDemand
            | Self::UndeliveredChunk
            | Self::NoDemand => format!("http://{addr}{UPSTREAM_PATH}"),
        }
    }

    /// A client whose every bound outlasts the row.
    ///
    /// The lookup row restricts it to the peer through the controlled edge,
    /// which is what makes the lookup one a row can hold.
    fn client(self, addr: SocketAddr, edge: &ClientDnsEdge) -> Client {
        let (retries, backoff) = match self {
            Self::RetryDelay => (SPARE_RETRIES, UNREACHED),
            Self::Dns
            | Self::Headers
            | Self::HeadHandoff
            | Self::WaitingDemand
            | Self::UndeliveredChunk
            | Self::NoDemand => (0, CROSSED_BACKOFF),
        };
        let client = unbounded_client().retries(retries).backoff(backoff);
        match self {
            Self::Dns => {
                let origin = stream_origin(addr);
                let policy = EgressPolicy::public_internet()
                    .allow_destination(&origin, LOOPBACK)
                    .unwrap_or_else(|error| panic!("the peer's exception was refused: {error:?}"));
                client_dns_edge(client.egress_policy(policy), edge)
            }
            Self::Headers
            | Self::RetryDelay
            | Self::HeadHandoff
            | Self::WaitingDemand
            | Self::UndeliveredChunk
            | Self::NoDemand => client,
        }
    }

    /// The edge that confirms the handler is held in this phase.
    fn edge(self) -> HeldEdge {
        match self {
            Self::Dns => HeldEdge::Lookup,
            Self::Headers => HeldEdge::Peer("read the request", |seen| seen.requests == 1),
            Self::RetryDelay => HeldEdge::Peer("saw its transient answer disposed", |seen| {
                seen.client_closes == 1
            }),
            Self::HeadHandoff => HeldEdge::OwnerParked,
            Self::WaitingDemand => HeldEdge::Owner("accepted a demand", |seen| seen.pending_demand),
            Self::UndeliveredChunk => HeldEdge::Owner("held a chunk for its caller", |seen| {
                seen.retained_chunks == 1
            }),
            Self::NoDemand => HeldEdge::Owner("parked behind one delivered chunk", |seen| {
                seen.admitted_chunks == 1 && seen.retained_chunks == 0 && seen.awaiting_demand
            }),
        }
    }

    /// Whether the handler holds a handle and waits to be resumed.
    fn parked(self) -> bool {
        matches!(
            self,
            Self::HeadHandoff | Self::UndeliveredChunk | Self::NoDemand
        )
    }

    /// Whether a head arrived, so the row has an owner view to read.
    fn owned(self) -> bool {
        self.parked() || matches!(self, Self::WaitingDemand)
    }

    /// Whether a transport is open while the handler is held.
    fn in_flight(self) -> bool {
        !matches!(self, Self::Dns | Self::RetryDelay)
    }

    /// Every request the peer reads, and all it may read.
    fn requests(self) -> usize {
        match self {
            Self::Dns => 0,
            Self::Headers
            | Self::RetryDelay
            | Self::HeadHandoff
            | Self::WaitingDemand
            | Self::UndeliveredChunk
            | Self::NoDemand => 1,
        }
    }
}

/// What a streaming journey row waits for before it applies its trigger.
///
/// The peer and owner edges carry what the fixture never did, for the failure
/// text, and the counts that confirm it.
enum HeldEdge {
    /// The controlled lookup holds the name.
    Lookup,
    /// The peer's counts.
    Peer(&'static str, fn(&Paced) -> bool),
    /// The owner parked with no demand.
    OwnerParked,
    /// The owner's counts.
    Owner(&'static str, fn(&ClientBodyObservation) -> bool),
}

/// Wait until the client closed its held transport, then until the owner's
/// counts satisfy `ready`.
async fn released_after_leaving(
    upstream: &mut PacedUpstream,
    watched: &ScopedTransferOwner,
    what: &str,
    ready: impl Fn(&ClientBodyObservation) -> bool,
) -> Result<ClientBodyObservation, String> {
    upstream.client_left().await?;
    owner_reached(watched, what, ready).await
}

/// Run the held route's stream as far as `held`, wait there when the phase
/// parks, and report how the body ended.
///
/// A phase before the head ends at the send. Every later phase ends at a
/// demand, so the handler reports the terminal its own handle returned.
async fn held_stream(
    client: Arc<Client>,
    url: Arc<str>,
    held: StreamHeld,
    resume: Arc<Notify>,
) -> Result<u16, RuntimeError> {
    let head: StreamHead = stream_get(&client, &url).await;
    let response: ClientStreamResponse = head?;
    let status: u16 = response.status();
    if matches!(held, StreamHeld::HeadHandoff) {
        resume.notified().await;
    }
    let mut body: ClientBody = response.into_body();
    if matches!(held, StreamHeld::NoDemand) {
        let first: StreamNext = body.next().await;
        first?;
        resume.notified().await;
    }
    let mut next = std::pin::pin!(body.next());
    let polled: Poll<StreamNext> = poll_once(next.as_mut()).await;
    if matches!(held, StreamHeld::UndeliveredChunk) {
        resume.notified().await;
    }
    let ended: StreamNext = match polled {
        Poll::Ready(ended) => ended,
        Poll::Pending => next.await,
    };
    ended.map(|_| status)
}

/// A router whose held route streams from the peer and reports how its run
/// ended.
fn streaming_router(
    client: Client,
    url: String,
    held: StreamHeld,
    resume: &Arc<Notify>,
    ends: Sender<HandlerEnd>,
) -> Router {
    let client = Arc::new(client);
    let url: Arc<str> = url.into();
    let resume = Arc::clone(resume);
    let mut router = probe_router();
    router.get(HELD_PATH, move |_req: &Request| {
        let (client, url, resume) = (Arc::clone(&client), Arc::clone(&url), Arc::clone(&resume));
        let end = EndReport::new(&ends);
        async move { end.answer(held_stream(client, url, held, resume).await) }
    });
    router
}

/// Everything one streaming journey row observed before its runtime returned.
struct StreamObserved {
    /// Whether the peer, the edge, or the owner confirmed the phase.
    reached: Result<(), String>,
    /// The owner's counts while the handler was held, before the trigger.
    before: Option<ClientBodyObservation>,
    /// The owner's counts once it released its source, read before the
    /// handler was resumed. `None` for a phase with no owner view.
    released: Result<Option<ClientBodyObservation>, String>,
    /// How the handler's run ended, or `None` if it never reported.
    end: Option<HandlerEnd>,
}

impl StreamObserved {
    /// Require the phase reached, the handler ended as the trigger requires,
    /// and the held stream released with no demand of the handler's own.
    ///
    /// `joined` is the owner's counts once the runtime returned. A phase with
    /// an owner view requires that owner's task ended by then.
    fn assert_ended(
        self,
        context: &str,
        held: StreamHeld,
        trigger: Trigger,
        joined: &ClientBodyObservation,
    ) {
        if let Err(unreached) = self.reached {
            panic!("{context}: the stream was never held: {unreached}");
        }
        assert!(
            trigger.ended(&self.end),
            "{context}: the handler's run ended as {:?}",
            self.end
        );
        let released = match self.released {
            Ok(released) => released,
            Err(kept) => panic!("{context}: the held stream was not released: {kept}"),
        };
        // A phase with an owner view read it twice. A phase without one has
        // nothing more to prove here.
        let (before, after) = match (held.owned(), self.before, released) {
            (true, Some(before), Some(after)) => (before, after),
            (false, None, None) => return,
            (owned, before, after) => panic!(
                "{context}: a phase with an owner view ({owned}) read {before:?}, then {after:?}"
            ),
        };
        assert_eq!(
            after.retained_chunks, 0,
            "{context}: the release kept a chunk: {after:?}"
        );
        // A parked handler has no demand out, so the owner has no reason to
        // poll. A pending demand's source may be polled once more on the
        // wakeup that ends it.
        assert!(
            !held.parked() || after.source_polls == before.source_polls,
            "{context}: a source with no demand was polled: {before:?} then {after:?}"
        );
        assert!(
            joined.owner_ended,
            "{context}: the runtime returned before the owner task ended: {joined:?}"
        );
    }
}

/// One streaming journey row's fixtures, as the runtime closure drives them.
struct StreamJourney<'a> {
    peers: &'a tokio::runtime::Runtime,
    upstream: &'a mut PacedUpstream,
    edge: &'a ClientDnsEdge,
    watched: &'a ScopedTransferOwner,
    ended: &'a Receiver<HandlerEnd>,
    resume: &'a Notify,
    held: StreamHeld,
    trigger: Trigger,
}

impl StreamJourney<'_> {
    /// Wait until the handler's stream is held in its phase.
    fn reached(&mut self) -> Result<(), String> {
        let (upstream, watched, edge) = (&mut *self.upstream, self.watched, self.edge);
        let held = self.held.edge();
        on_peers(self.peers, async move {
            match held {
                HeldEdge::Lookup => within_watchdog(edge.held())
                    .await
                    .ok_or_else(|| "the lookup was never held".to_owned()),
                HeldEdge::Peer(what, ready) => upstream.reached(what, ready).await.map(drop),
                HeldEdge::OwnerParked => owner_parked(watched).await.map(drop),
                HeldEdge::Owner(what, ready) => owner_reached(watched, what, ready).await.map(drop),
            }
        })
    }

    /// Wait until the trigger released what the phase held.
    ///
    /// The open transport must be closed by the client, and the owner must
    /// have dropped its source. The handler is still held while this waits.
    /// An owner view means a head arrived, so its phase has a transport open.
    fn released(&mut self) -> Result<Option<ClientBodyObservation>, String> {
        let (upstream, watched, held) = (&mut *self.upstream, self.watched, self.held);
        on_peers(self.peers, async move {
            match (held.owned(), held.in_flight()) {
                (true, true) => {
                    released_after_leaving(upstream, watched, "released its source", |seen| {
                        seen.source_released
                    })
                    .await
                    .map(Some)
                }
                (true, false) => Err(format!("{held:?} has an owner view and no transport")),
                (false, true) => upstream.client_left().await.map(|_| None),
                (false, false) => Ok(None),
            }
        })
    }

    /// Hold the handler's stream, apply the trigger, and collect what
    /// followed.
    ///
    /// Nothing is asserted here. A shutdown row keeps its inbound peer open
    /// until the handler has reported, so the peer's close cannot be what
    /// ended the stream. The handler is resumed only after the release was
    /// observed, so no demand of its own can be what released the source.
    fn run(mut self, addr: SocketAddr) -> StreamObserved {
        let inbound = start_request(addr, HELD_PATH);
        if let Err(unreached) = self.reached() {
            return StreamObserved {
                reached: Err(unreached),
                before: None,
                released: Ok(None),
                end: None,
            };
        }
        let before = self.held.owned().then(|| self.watched.streamed());
        let kept_open = self.trigger.apply(inbound);
        let released = self.released();
        self.resume.notify_one();
        let end = self.ended.recv_timeout(BOUND).ok();
        drop(kept_open);
        StreamObserved {
            reached: Ok(()),
            before,
            released,
            end,
        }
    }
}

/// One streaming journey row: a served handler streams from the peer, the
/// row holds it in one phase, the trigger ends it, and the peer, the owner,
/// the handler, and the runtime all settle.
///
/// A shutdown row requires `Cancelled` in the handler. A disconnect row
/// requires the handler dropped and the root scope drained to zero children.
/// Both require the open transport closed, the source released while the
/// handler placed no further demand, the owner task ended once the runtime
/// returned, and no request the row did not ask for.
fn assert_held_stream_ends(held: StreamHeld, trigger: Trigger) {
    let context = format!("{trigger:?} with the handler's stream held at {held:?}");
    let peers = peer_runtime();
    let mut upstream = on_peers(&peers, PacedUpstream::bind_steps(held.steps()));
    let addr = upstream.addr();
    let watched = watch_streams(addr);
    let edge = ClientDnsEdge::new();
    edge.hold();
    let resume = Arc::new(Notify::new());
    let (ends, ended) = channel();
    let router = streaming_router(
        held.client(addr, &edge),
        held.url(addr),
        held,
        &resume,
        ends,
    );
    let journey = StreamJourney {
        peers: &peers,
        upstream: &mut upstream,
        edge: &edge,
        watched: &watched,
        ended: &ended,
        resume: &resume,
        held,
        trigger,
    };

    let (observed, drained) = trigger.serve(router, |addr| journey.run(addr));
    let joined: ClientBodyObservation = watched.streamed();
    edge.release(&[LOOPBACK]);
    let recording = on_peers(&peers, upstream.finish(&context));

    observed.assert_ended(&context, held, trigger, &joined);
    assert_ne!(
        drained,
        Some(false),
        "{context}: the scope drain never reached zero children"
    );
    assert_eq!(
        recording.requests.len(),
        held.requests(),
        "{context}: the peer read {:?}",
        recording.requests
    );
    if matches!(held, StreamHeld::Dns) {
        assert_eq!(
            recording.connections, 0,
            "{context}: an ended lookup was dialed"
        );
    }
}

/// A stream whose runtime has closed admission is refused as `ScopeClosed`
/// before it dials.
fn assert_closed_admission_sends_nothing() {
    const CONTEXT: &str = "a streaming send after admission closed";

    let listener = crate::common::UndialedListener::bind();
    let url = format!("http://{}{UPSTREAM_PATH}", listener.addr());
    let client = total_only_client();

    let refused: Result<u16, RuntimeError> = runtime::builder()
        .run(move || {
            runtime::request_shutdown();
            let head: StreamHead = runtime::block_on(stream_get(&client, &url));
            head.map(|response: ClientStreamResponse| response.status())
        })
        .expect("the runtime did not return cleanly");

    assert!(
        matches!(refused, Err(RuntimeError::ScopeClosed)),
        "{CONTEXT}: returned {refused:?}"
    );
    listener.assert_undialed(CONTEXT);
}

/// The steps of a peer that sends a head, then holds.
fn held_head_steps() -> Vec<Pace> {
    vec![
        Pace::Send(paced_head("200 OK", &[CHUNKED])),
        Pace::UntilClosed,
    ]
}

/// The steps of a peer that sends a head and one chunk, then holds.
fn held_body_steps() -> Vec<Pace> {
    vec![
        Pace::Send(paced_head("200 OK", &[CHUNKED])),
        Pace::Send(paced_chunk(FIRST_CHUNK)),
        Pace::UntilClosed,
    ]
}

/// A paced peer on a runtime of its own, the view of the owners of its calls,
/// and the URL a row requests of it.
///
/// It is the peer-runtime counterpart of [`bare_stream`]. The fields drop in
/// the order a row that binds them one by one drops them: the view goes before
/// the runtime that serves the peer.
struct WatchedPeer {
    url: String,
    watched: ScopedTransferOwner,
    upstream: PacedUpstream,
    peers: tokio::runtime::Runtime,
}

impl WatchedPeer {
    /// Bind a peer that answers every request with `steps`, and watch it.
    fn bind(steps: Vec<Pace>) -> Self {
        let peers = peer_runtime();
        let upstream = on_peers(&peers, PacedUpstream::bind_steps(steps));
        let watched = watch_streams(upstream.addr());
        let url = upstream.url(UPSTREAM_PATH);
        Self {
            url,
            watched,
            upstream,
            peers,
        }
    }
}

/// Require the peer to have read one request, and no more.
fn assert_one_request(context: &str, recording: &Recording) {
    assert_eq!(
        recording.requests.len(),
        1,
        "{context}: requests the peer read"
    );
}

/// Require the peer to have accepted one connection and read one request on
/// it, and no more of either.
fn assert_one_connection_one_request(context: &str, recording: &Recording) {
    assert_eq!(
        (recording.connections, recording.requests.len()),
        (1, 1),
        "{context}: the peer read {:?}",
        recording.requests
    );
}

/// Require the peer to have seen the client close its one held transport, as
/// read before the peer was released.
fn assert_client_left(context: &str, left: Result<Paced, String>) {
    if let Err(kept_open) = left {
        panic!("{context}: {kept_open}");
    }
}

/// What a row saw of one stream its runtime's shutdown ended.
struct ShutdownSeen {
    /// The status of the head, or the failure of the send.
    status: Result<u16, RuntimeError>,
    /// Whether the owner parked with the unread response.
    held: Result<ClientBodyObservation, String>,
    /// The owner's counts once shutdown released its source.
    released: Result<ClientBodyObservation, String>,
    /// What the first demand after the shutdown returned.
    next: Option<StreamNext>,
}

/// The runtime that first polls a send owns its stream, wherever the send
/// was described.
///
/// The request is described and its terminal called on this thread, which
/// has no runtime. A Camber runtime then polls it. That runtime's shutdown
/// must release the unread response without a demand, and the next demand
/// must report `Cancelled`.
fn assert_first_poll_runtime_owns_the_stream() {
    const CONTEXT: &str = "a send described outside the runtime that first polls it";

    let mut peer = WatchedPeer::bind(held_body_steps());
    let client = total_only_client();
    let described = match client.request("GET", &peer.url) {
        Ok(request) => request.send_streaming(),
        Err(error) => panic!("{CONTEXT}: the description was refused: {error:?}"),
    };

    let seen = runtime::builder()
        .run(|| {
            let head: StreamHead = runtime::block_on(described);
            let held = on_peers(&peer.peers, owner_parked(&peer.watched));
            runtime::request_shutdown();
            let released = on_peers(
                &peer.peers,
                released_after_leaving(
                    &mut peer.upstream,
                    &peer.watched,
                    "released its source",
                    |seen| seen.source_released,
                ),
            );
            let (status, next) = match head {
                Ok(response) => {
                    let status: u16 = response.status();
                    let mut body: ClientBody = response.into_body();
                    let next: StreamNext = runtime::block_on(body.next());
                    (Ok(status), Some(next))
                }
                Err(error) => (Err(error), None),
            };
            ShutdownSeen {
                status,
                held,
                released,
                next,
            }
        })
        .expect("the runtime did not return cleanly");
    let joined: ClientBodyObservation = peer.watched.streamed();
    let recording = on_peers(&peer.peers, peer.upstream.finish(CONTEXT));

    assert!(
        matches!(seen.status, Ok(200)),
        "{CONTEXT}: the send returned {:?}",
        seen.status
    );
    if let Err(unheld) = seen.held {
        panic!("{CONTEXT}: {unheld}");
    }
    match seen.released {
        Ok(released) => assert_eq!(
            released.source_polls, 0,
            "{CONTEXT}: an unread body was polled: {released:?}"
        ),
        Err(kept) => panic!("{CONTEXT}: shutdown left the stream live: {kept}"),
    }
    assert!(
        matches!(seen.next, Some(Err(RuntimeError::Cancelled))),
        "{CONTEXT}: the demand after shutdown returned {:?}",
        seen.next
    );
    assert!(
        joined.owner_ended,
        "{CONTEXT}: the runtime returned before the owner task ended: {joined:?}"
    );
    assert_one_request(CONTEXT, &recording);
}

/// What a row saw of one stream the close of its runtime's root scope ended.
struct ScopeCloseSeen {
    /// The unread handle, or the failure of the send.
    retained: Result<ClientBody, RuntimeError>,
    /// The owner's counts while it was parked with the unread response.
    held: Result<ClientBodyObservation, String>,
}

/// The close of the root scope releases a retained unread handle, with no
/// shutdown request.
///
/// The runtime's closure returns the handle, so the closure's return is the
/// only thing that closes the scope. Every bound of the client outlasts the
/// row. The runtime's return is then the drain: it must be clean, with the
/// source released and the owner task ended. The handle was never read, so
/// its first demand afterwards must report `Cancelled` from zero source
/// polls.
///
/// An owner that stops on the shutdown latch alone is still live when the
/// drain expires. The runtime then reports the drain's timeout, and the
/// forced stop leaves the handle a lost owner in place of a cancellation.
fn assert_scope_close_releases_a_retained_handle() {
    const CONTEXT: &str = "a runtime closure that returns a retained unread handle";

    let mut peer = WatchedPeer::bind(held_head_steps());
    let client = unbounded_client();

    let closed = runtime::builder().run(|| {
        let head: StreamHead = runtime::block_on(stream_get(&client, &peer.url));
        let held = on_peers(&peer.peers, owner_parked(&peer.watched));
        ScopeCloseSeen {
            retained: head.map(ClientStreamResponse::into_body),
            held,
        }
    });
    let joined: ClientBodyObservation = peer.watched.streamed();
    let left = on_peers(&peer.peers, peer.upstream.client_left());
    let seen = match closed {
        Ok(seen) => seen,
        Err(error) => panic!("{CONTEXT}: the scope did not drain cleanly: {error:?}"),
    };
    let next: Option<StreamNext> = match seen.retained {
        Ok(mut body) => on_peers(
            &peer.peers,
            async move { within_watchdog(body.next()).await },
        ),
        Err(error) => panic!("{CONTEXT}: the send failed: {error:?}"),
    };
    let recording = on_peers(&peer.peers, peer.upstream.finish(CONTEXT));

    match seen.held {
        Ok(held) => assert!(
            !held.source_released && !held.owner_ended,
            "{CONTEXT}: the stream ended before the scope closed: {held:?}"
        ),
        Err(unheld) => panic!("{CONTEXT}: {unheld}"),
    }
    assert!(
        joined.source_released && joined.owner_ended,
        "{CONTEXT}: the runtime returned with its owner live: {joined:?}"
    );
    assert_eq!(
        joined.source_polls, 0,
        "{CONTEXT}: an unread body was polled: {joined:?}"
    );
    assert!(
        matches!(next, Some(Err(RuntimeError::Cancelled))),
        "{CONTEXT}: the demand after the scope closed returned {next:?}"
    );
    assert_client_left(CONTEXT, left);
    assert_one_request(CONTEXT, &recording);
}

/// A handle moved to another runtime still reads its body there, and its
/// owner stays with the runtime that admitted it.
///
/// The body is read on the peer runtime, which holds no Camber context. The
/// admitting runtime then shuts down and returns while the handle is still
/// alive. Its return is the join: the owner task has ended by then, and the
/// handle's next demand reports `Cancelled`.
fn assert_moved_handle_leaves_its_join_with_the_root() {
    const CONTEXT: &str = "a body handle moved to another runtime";

    let mut peer = WatchedPeer::bind(held_body_steps());
    let client = total_only_client();

    let moved: Result<(StreamNext, ClientBody), RuntimeError> = runtime::builder()
        .run(|| -> Result<(StreamNext, ClientBody), RuntimeError> {
            let head: StreamHead = runtime::block_on(stream_get(&client, &peer.url));
            let mut body: ClientBody = head?.into_body();
            let (first, body) = on_peers(&peer.peers, async move {
                let first: Option<StreamNext> = within_watchdog(body.next()).await;
                (first, body)
            });
            runtime::request_shutdown();
            let first: StreamNext =
                first.unwrap_or_else(|| panic!("{CONTEXT}: the moved demand never settled"));
            Ok((first, body))
        })
        .expect("the runtime did not return cleanly");
    let joined: ClientBodyObservation = peer.watched.streamed();
    let (first, after) = match moved {
        Ok((first, mut body)) => {
            let after: Option<StreamNext> =
                on_peers(
                    &peer.peers,
                    async move { within_watchdog(body.next()).await },
                );
            (first, after)
        }
        Err(error) => panic!("{CONTEXT}: the send failed: {error:?}"),
    };
    let left = on_peers(&peer.peers, peer.upstream.client_left());
    let recording = on_peers(&peer.peers, peer.upstream.finish(CONTEXT));

    assert!(
        matches!(&first, Ok(Some(chunk)) if &chunk[..] == FIRST_CHUNK),
        "{CONTEXT}: the moved demand returned {first:?}"
    );
    assert!(
        joined.source_released && joined.owner_ended,
        "{CONTEXT}: the admitting runtime returned with its owner live: {joined:?}"
    );
    assert!(
        matches!(after, Some(Err(RuntimeError::Cancelled))),
        "{CONTEXT}: the demand after shutdown returned {after:?}"
    );
    assert_client_left(CONTEXT, left);
    assert_one_request(CONTEXT, &recording);
}

/// The second chunk of the transferred-handle row, which its peer withholds.
const SECOND_CHUNK: &[u8] = b"second";

/// What the application task of the transferred-handle row reports: the
/// first chunk, then the rest of the payload at its clean end.
type Transferred = Result<Vec<u8>, RuntimeError>;

/// Read the body one chunk, report it, then read to the end and report the
/// rest.
///
/// A read the fixture watchdog ends panics in this task and reports nothing,
/// so the row reads a missing report.
async fn read_transferred(
    context: &'static str,
    mut body: ClientBody,
    reports: Sender<Transferred>,
) {
    let first: StreamNext = body.next().await;
    // A closed receiver means the row has already finished.
    let _ = reports.send(first.map(|chunk| chunk.map_or_else(Vec::new, |chunk| chunk.to_vec())));
    let (rest, end) = read_out(context, &mut body).await;
    let _ = reports.send(end.map(|_| rest));
}

/// A router whose held route moves its body handle into an application task
/// and then waits on a notification that never comes.
fn transferring_router(
    context: &'static str,
    client: Client,
    url: String,
    reports: Sender<Transferred>,
    ends: Sender<HandlerEnd>,
) -> Router {
    let client = Arc::new(client);
    let url: Arc<str> = url.into();
    let never = Arc::new(Notify::new());
    let mut router = probe_router();
    router.get(HELD_PATH, move |_req: &Request| {
        let (client, url, never) = (Arc::clone(&client), Arc::clone(&url), Arc::clone(&never));
        let reports = reports.clone();
        let end = EndReport::new(&ends);
        async move {
            let head: StreamHead = stream_get(&client, &url).await;
            let body: ClientBody = match head {
                Ok(response) => response.into_body(),
                Err(error) => return end.answer(Err(error)),
            };
            drop(camber::spawn_async(read_transferred(
                context, body, reports,
            )));
            never.notified().await;
            drop(end);
            Response::empty(200)
        }
    });
    router
}

/// What the transferred-handle row observed before its runtime returned.
struct TransferSeen {
    first: Option<Transferred>,
    end: Option<HandlerEnd>,
    /// The owner's counts after the handler was dropped.
    after_drop: ClientBodyObservation,
    rest: Option<Transferred>,
}

/// A body handle the handler moved into an application task is not the
/// handler's any more. The inbound peer's disconnect drops the handler and
/// leaves the stream live: the task reads the rest of the body to its end.
fn assert_transferred_handle_outlives_its_handler() {
    const CONTEXT: &str = "a body handle moved into an application task";

    let peer = WatchedPeer::bind(vec![
        Pace::Send(paced_head("200 OK", &[CHUNKED])),
        Pace::Send(paced_chunk(FIRST_CHUNK)),
        Pace::Gate,
        Pace::Send(paced_chunk(SECOND_CHUNK)),
        Pace::Send(paced_end(&[])),
    ]);
    let (reports, reported) = channel();
    let (ends, ended) = channel();
    let client = total_only_client();
    let router = transferring_router(CONTEXT, client, peer.url, reports, ends);

    let (seen, drained) = with_drained_server(None, router, |addr| {
        let inbound = start_request(addr, HELD_PATH);
        let first = reported.recv_timeout(BOUND).ok();
        drop(inbound);
        let end = ended.recv_timeout(BOUND).ok();
        let after_drop: ClientBodyObservation = peer.watched.streamed();
        peer.upstream.open_gate();
        let rest = reported.recv_timeout(BOUND).ok();
        TransferSeen {
            first,
            end,
            after_drop,
            rest,
        }
    });
    let recording = on_peers(&peer.peers, peer.upstream.finish(CONTEXT));

    assert!(
        matches!(&seen.first, Some(Ok(chunk)) if chunk == FIRST_CHUNK),
        "{CONTEXT}: the task's first demand returned {:?}",
        seen.first
    );
    assert!(
        matches!(seen.end, Some(HandlerEnd::Dropped)),
        "{CONTEXT}: the handler's run ended as {:?}",
        seen.end
    );
    assert!(
        !seen.after_drop.source_released,
        "{CONTEXT}: the handler's drop released a handle it had given away: {:?}",
        seen.after_drop
    );
    assert!(
        matches!(&seen.rest, Some(Ok(rest)) if rest == SECOND_CHUNK),
        "{CONTEXT}: the task read {:?} after the handler was dropped",
        seen.rest
    );
    assert!(
        drained,
        "{CONTEXT}: the scope drain never reached zero children"
    );
    assert_one_request(CONTEXT, &recording);
}

/// What the failed-delivery row observed before its runtime returned.
struct UndeliveredSeen {
    /// The owner's counts while it held the head it had not delivered.
    held: Result<ClientBodyObservation, String>,
    /// How the handler's run ended, or `None` if it never reported.
    end: Option<HandlerEnd>,
    /// Whether the held owner was let go at the delivery edge.
    let_go: Result<(), RuntimeError>,
    /// The owner's counts once its source was released and its task ended,
    /// read while the server still ran.
    released: Result<ClientBodyObservation, String>,
}

/// Wait until the owner holds its final head at the delivery edge, and read
/// its counts there.
async fn held_before_delivery(
    watched: &ScopedTransferOwner,
) -> Result<ClientBodyObservation, String> {
    let edge = TransferOwnerEdge::BeforeHeadDelivery;
    match within_watchdog(watched.wait_until_paused(edge)).await {
        Some(Ok(())) => Ok(watched.streamed()),
        Some(Err(error)) => Err(format!("the delivery edge was refused: {error:?}")),
        None => Err("the owner never held a head before its delivery".to_owned()),
    }
}

/// Arm the hold that stops the next owner at `edge`.
fn arm_delivery_edge(context: &str, watched: &ScopedTransferOwner, edge: TransferOwnerEdge) {
    if let Err(error) = watched.pause_once(edge) {
        panic!("{context}: the delivery edge was not armed: {error:?}");
    }
}

/// Require the owner to have been held at the delivery edge with its task
/// live and its source kept and unread.
fn assert_held_before_delivery(context: &str, held: Result<ClientBodyObservation, String>) {
    match held {
        Ok(held) => assert!(
            !held.source_released && !held.owner_ended && held.source_polls == 0,
            "{context}: the held owner had already moved: {held:?}"
        ),
        Err(unheld) => panic!("{context}: {unheld}"),
    }
}

/// Require the owner to have released a body it never read. `kept` names the
/// fault of an owner that did not release.
fn assert_unread_release(
    context: &str,
    released: Result<ClientBodyObservation, String>,
    kept: &str,
) {
    match released {
        Ok(released) => assert_eq!(
            (released.source_polls, released.retained_chunks),
            (0, 0),
            "{context}: an undelivered body was read: {released:?}"
        ),
        Err(unreleased) => panic!("{context}: {kept}: {unreleased}"),
    }
}

/// A head whose waiting send is gone when the owner delivers it leaves no
/// owner, no source, and no open transport.
///
/// The owner is held with the final head, before the delivery. The inbound
/// peer then disconnects, so the server drops the handler and the pending
/// send with it. The handler's report is dropped after its pending send, so
/// the way back is closed when the report arrives. Only then is the owner
/// let go, and its delivery fails.
///
/// Under a Camber runtime no handle aborts the task. The release must follow
/// from the failed delivery alone, so the row reads it while the server still
/// runs: no shutdown has been requested, and every bound of the client
/// outlasts the row.
fn assert_failed_head_delivery_leaves_no_orphan() {
    const CONTEXT: &str = "a head whose waiting send was dropped before its delivery";
    let edge = TransferOwnerEdge::BeforeHeadDelivery;

    let mut peer = WatchedPeer::bind(held_head_steps());
    arm_delivery_edge(CONTEXT, &peer.watched, edge);
    let resume = Arc::new(Notify::new());
    let (ends, ended) = channel();
    let router = streaming_router(
        unbounded_client(),
        peer.url,
        StreamHeld::Headers,
        &resume,
        ends,
    );

    let (seen, drained) = with_drained_server(None, router, |addr| {
        let inbound = start_request(addr, HELD_PATH);
        let held = on_peers(&peer.peers, held_before_delivery(&peer.watched));
        drop(inbound);
        let end = ended.recv_timeout(BOUND).ok();
        // An edge the owner never reached has nothing to release; the row
        // reports that as the unheld owner, and reads this result after it.
        let let_go = peer.watched.release(edge);
        let released = on_peers(
            &peer.peers,
            released_after_leaving(
                &mut peer.upstream,
                &peer.watched,
                "released its source and ended",
                |seen| seen.source_released && seen.owner_ended,
            ),
        );
        UndeliveredSeen {
            held,
            end,
            let_go,
            released,
        }
    });
    let recording = on_peers(&peer.peers, peer.upstream.finish(CONTEXT));

    assert_held_before_delivery(CONTEXT, seen.held);
    if let Err(error) = seen.let_go {
        panic!("{CONTEXT}: the held owner was not let go: {error:?}");
    }
    assert!(
        matches!(seen.end, Some(HandlerEnd::Dropped)),
        "{CONTEXT}: the handler's run ended as {:?}",
        seen.end
    );
    assert_unread_release(
        CONTEXT,
        seen.released,
        "the failed delivery left the stream live",
    );
    assert!(
        drained,
        "{CONTEXT}: the scope drain never reached zero children"
    );
    assert_one_connection_one_request(CONTEXT, &recording);
}

/// Streaming 5.T3
#[test]
fn streaming_response_keeps_its_admitting_runtime() {
    for trigger in [Trigger::Shutdown, Trigger::Disconnect] {
        for held in STREAM_HELD {
            assert_held_stream_ends(held, trigger);
        }
    }
    assert_closed_admission_sends_nothing();
    assert_first_poll_runtime_owns_the_stream();
    assert_scope_close_releases_a_retained_handle();
    assert_moved_handle_leaves_its_join_with_the_root();
    assert_transferred_handle_outlives_its_handler();
    assert_failed_head_delivery_leaves_no_orphan();
}

/// The executor one bare-Tokio streaming row runs on.
#[derive(Clone, Copy, Debug)]
enum Flavor {
    CurrentThread,
    MultiThread,
}

impl Flavor {
    fn runtime(self) -> tokio::runtime::Runtime {
        let mut builder = match self {
            Self::CurrentThread => tokio::runtime::Builder::new_current_thread(),
            Self::MultiThread => {
                let mut builder = tokio::runtime::Builder::new_multi_thread();
                builder.worker_threads(2);
                builder
            }
        };
        builder
            .enable_all()
            .build()
            .unwrap_or_else(|error| panic!("{self:?}: the executor could not be built: {error}"))
    }

    /// How many times the acknowledgement rows run.
    ///
    /// On several threads the row reads the owner's count while the owner
    /// runs beside it, so one pass can miss an owner that acknowledged first.
    fn passes(self) -> usize {
        match self {
            Self::CurrentThread => 1,
            Self::MultiThread => 8,
        }
    }
}

/// The operation total of the bare row that lets it expire. Real time crosses
/// it, and the head arrives well inside it.
const BARE_STREAM_TOTAL: Duration = Duration::from_secs(2);

/// One streamed response on bare Tokio, its peer, and its owner view.
struct BareStream {
    peer: PacedUpstream,
    watched: ScopedTransferOwner,
    response: ClientStreamResponse,
}

/// Send one GET at a peer that answers with `steps`, and require its head.
async fn bare_stream(context: &str, client: &Client, steps: Vec<Pace>) -> BareStream {
    let peer = PacedUpstream::bind_steps(steps).await;
    let watched = watch_streams(peer.addr());
    let head: StreamHead = must_settle(
        context,
        "the send",
        stream_get(client, &peer.url(UPSTREAM_PATH)),
    )
    .await;
    let response: ClientStreamResponse = answered(context, head);
    BareStream {
        peer,
        watched,
        response,
    }
}

/// An awaited clean end returns after the source is released. A status
/// conversion of a success returned the same live handle.
async fn assert_bare_end_acknowledges_release(flavor: Flavor) {
    let context = format!("{flavor:?}: a clean end");
    let context = context.as_str();
    let steps = vec![
        Pace::Send(paced_head("200 OK", &[CHUNKED])),
        Pace::Send(paced_chunk(FIRST_CHUNK)),
        Pace::Send(paced_end(&[])),
    ];
    let BareStream {
        peer,
        watched,
        response,
    } = bare_stream(context, &unbounded_client(), steps).await;

    let kept: StreamHead = response.error_for_status();
    let mut body: ClientBody = match kept {
        Ok(response) => response.into_body(),
        Err(error) => panic!("{context}: a success was converted to {error:?}"),
    };
    let (payload, end) = read_out(context, &mut body).await;
    let at_end: ClientBodyObservation = watched.streamed();
    let closed: Result<(), RuntimeError> = close(context, body).await;
    peer.finish(context).await;

    assert_eq!(payload, FIRST_CHUNK, "{context}: the delivered payload");
    assert!(
        matches!(end, Ok(None)),
        "{context}: the body ended as {end:?}"
    );
    assert!(
        at_end.source_released,
        "{context}: the end returned before the source was released: {at_end:?}"
    );
    assert!(
        matches!(closed, Ok(())),
        "{context}: a close after the end returned {closed:?}"
    );
}

/// An awaited failure returns after the source is released, and a close
/// behind it returns the same category.
async fn assert_bare_failure_acknowledges_release(flavor: Flavor) {
    let context = format!("{flavor:?}: a truncated body");
    let context = context.as_str();
    let steps = vec![
        Pace::Send(paced_head("200 OK", &[("Content-Length", "64")])),
        Pace::Send(FIRST_CHUNK.to_vec()),
        Pace::Close,
    ];
    let BareStream {
        peer,
        watched,
        response,
    } = bare_stream(context, &unbounded_client(), steps).await;

    let mut body: ClientBody = response.into_body();
    let (payload, end) = read_out(context, &mut body).await;
    let at_end: ClientBodyObservation = watched.streamed();
    let closed: Result<(), RuntimeError> = close(context, body).await;
    peer.finish(context).await;

    assert_eq!(payload, FIRST_CHUNK, "{context}: the delivered prefix");
    assert!(
        failed_as(&end, ClientError::Transport),
        "{context}: the body ended as {end:?}"
    );
    assert!(
        at_end.source_released,
        "{context}: the failure returned before the source was released: {at_end:?}"
    );
    assert!(
        failed_as(&closed, ClientError::Transport),
        "{context}: a close after the failure returned {closed:?}"
    );
}

/// Closing a live unread body succeeds, and returns after the source is
/// released.
async fn assert_bare_close_acknowledges_release(flavor: Flavor) {
    let context = format!("{flavor:?}: a close of an unread body");
    let context = context.as_str();
    let BareStream {
        mut peer,
        watched,
        response,
    } = bare_stream(context, &unbounded_client(), held_body_steps()).await;

    let body: ClientBody = response.into_body();
    let closed: Result<(), RuntimeError> = close(context, body).await;
    let at_close: ClientBodyObservation = watched.streamed();
    owner_must_reach(context, &watched, "ended its task", |seen| seen.owner_ended).await;
    peer.must_see_client_left(context).await;
    peer.finish(context).await;

    assert!(
        matches!(closed, Ok(())),
        "{context}: an intentional close returned {closed:?}"
    );
    assert!(
        at_close.source_released && at_close.source_polls == 0,
        "{context}: the close returned before release, or polled the body: {at_close:?}"
    );
}

/// Dropping an unread response releases its source and ends its owner while
/// the caller polls nothing.
async fn assert_bare_drop_releases(flavor: Flavor) {
    let context = format!("{flavor:?}: a dropped unread response");
    let context = context.as_str();
    let BareStream {
        mut peer,
        watched,
        response,
    } = bare_stream(context, &unbounded_client(), held_body_steps()).await;

    drop(response);
    let ended = owner_must_reach(context, &watched, "ended after the drop", |seen| {
        seen.source_released && seen.owner_ended
    })
    .await;
    peer.must_see_client_left(context).await;
    peer.finish(context).await;

    assert_eq!(
        (ended.source_polls, ended.retained_chunks),
        (0, 0),
        "{context}: a dropped response was polled or kept a chunk: {ended:?}"
    );
}

/// A dropped pending demand commits abandonment. The source is released,
/// every later demand reports cancellation, and the close succeeds.
async fn assert_bare_abandoned_demand_is_cancellation(flavor: Flavor) {
    let context = format!("{flavor:?}: an abandoned demand");
    let context = context.as_str();
    let BareStream {
        mut peer,
        watched,
        response,
    } = bare_stream(context, &unbounded_client(), held_head_steps()).await;

    let mut body: ClientBody = response.into_body();
    let pending = {
        let mut next = std::pin::pin!(body.next());
        let first: Poll<StreamNext> = poll_once(next.as_mut()).await;
        owner_must_reach(context, &watched, "accepted the demand", |seen| {
            seen.pending_demand
        })
        .await;
        first.is_pending()
    };
    owner_must_reach(
        context,
        &watched,
        "released after the abandonment",
        |seen| seen.source_released,
    )
    .await;
    let after: StreamNext = must_settle(context, "the next demand", body.next()).await;
    let closed: Result<(), RuntimeError> = close(context, body).await;
    peer.must_see_client_left(context).await;
    peer.finish(context).await;

    assert!(pending, "{context}: a withheld body answered its demand");
    assert!(
        matches!(after, Err(RuntimeError::Cancelled)),
        "{context}: the demand behind an abandoned one returned {after:?}"
    );
    assert!(
        matches!(closed, Ok(())),
        "{context}: a close after the caller's own cancellation returned {closed:?}"
    );
}

/// The operation total releases a retained unread response with no caller
/// poll, and stays the cause every later call reports.
async fn assert_bare_deadline_releases_a_retained_handle(flavor: Flavor) {
    let context = format!("{flavor:?}: a retained response past its total");
    let context = context.as_str();
    let client: Client = unbounded_client().total_timeout(BARE_STREAM_TOTAL);
    let expired = ClientError::Timeout(ClientTimeout::Operation);
    let BareStream {
        mut peer,
        watched,
        response,
    } = bare_stream(context, &client, held_body_steps()).await;

    let released = owner_must_reach(context, &watched, "released at its total", |seen| {
        seen.source_released
    })
    .await;
    let mut body: ClientBody = response.into_body();
    let next: StreamNext = demand(context, &mut body).await;
    let closed: Result<(), RuntimeError> = close(context, body).await;
    peer.must_see_client_left(context).await;
    peer.finish(context).await;

    assert_eq!(
        released.source_polls, 0,
        "{context}: an unread body was polled: {released:?}"
    );
    assert!(
        failed_as(&next, expired),
        "{context}: the demand after the total returned {next:?}"
    );
    assert!(
        failed_as(&closed, expired),
        "{context}: the close after the total returned {closed:?}"
    );
}

/// A status conversion of a live unread error response returns its status
/// error at once and only requests release. The owner releases its source
/// when it next runs.
///
/// On one thread the owner cannot run inside the synchronous conversion, so
/// the source is still held at the return. That is the proof that the
/// conversion is no acknowledgement.
async fn assert_bare_status_conversion_requests_release(flavor: Flavor) {
    let context = format!("{flavor:?}: a converted error status");
    let context = context.as_str();
    let steps = vec![
        Pace::Send(paced_head("500 Internal Server Error", &[CHUNKED])),
        Pace::Send(paced_chunk(FIRST_CHUNK)),
        Pace::UntilClosed,
    ];
    let BareStream {
        mut peer,
        watched,
        response,
    } = bare_stream(context, &unbounded_client(), steps).await;
    parked(context, &watched).await;

    let converted: Result<u16, RuntimeError> = response
        .error_for_status()
        .map(|kept: ClientStreamResponse| kept.status());
    let at_return: ClientBodyObservation = watched.streamed();
    let ended = owner_must_reach(context, &watched, "ended after the conversion", |seen| {
        seen.source_released && seen.owner_ended
    })
    .await;
    peer.must_see_client_left(context).await;
    peer.finish(context).await;

    assert!(
        failed_as(&converted, ClientError::Status(500)),
        "{context}: the conversion returned {converted:?}"
    );
    if matches!(flavor, Flavor::CurrentThread) {
        assert!(
            !at_return.source_released,
            "{context}: the conversion ran the owner to its release: {at_return:?}"
        );
    }
    assert_eq!(
        ended.source_polls, 0,
        "{context}: a converted response was polled: {ended:?}"
    );
}

/// A pending send dropped beside its arriving head leaves no owner and no
/// open transport.
///
/// The send is aborted and the peer's head is released with no turn between
/// them. The owner may read the abandonment first, or may fail to deliver a
/// head nobody waits for. Both end the same way.
async fn assert_bare_send_dropped_beside_its_head(flavor: Flavor) {
    let context = format!("{flavor:?}: a send dropped beside its head");
    let context = context.as_str();
    let mut steps = vec![Pace::Gate];
    steps.extend(held_body_steps());
    let mut peer = PacedUpstream::bind_steps(steps).await;
    let call = spawn_stream_get(unbounded_client(), peer.url(UPSTREAM_PATH));

    let parked = peer
        .reached("parked before its head", |seen| seen.gates == 1)
        .await;
    call.abort();
    peer.open_gate();
    let cancelled = was_cancelled(call).await;
    peer.must_see_client_left(context).await;
    let recording = peer.finish(context).await;

    assert!(parked.is_ok(), "{context}: {parked:?}");
    assert!(cancelled, "{context}: the dropped send was not cancelled");
    assert_one_connection_one_request(context, &recording);
}

/// A pending send dropped while its owner is held at the delivery edge ends
/// that owner, with the hold still in place.
///
/// The held owner polls nothing: not the way back to its send, not its
/// deadline, and not its body. No cooperative path can end it there. The row
/// never lets it go, so the task ends only because the dropped send aborted
/// it. A detached task stays held, and the row fails on its watchdog.
async fn assert_bare_dropped_send_aborts_a_held_owner(flavor: Flavor) {
    let context = format!("{flavor:?}: a send dropped while its owner held the head");
    let context = context.as_str();
    let edge = TransferOwnerEdge::BeforeHeadDelivery;
    let mut peer = PacedUpstream::bind_steps(held_body_steps()).await;
    let watched = watch_streams(peer.addr());
    arm_delivery_edge(context, &watched, edge);
    let call = spawn_stream_get(unbounded_client(), peer.url(UPSTREAM_PATH));

    let held = held_before_delivery(&watched).await;
    call.abort();
    let cancelled = was_cancelled(call).await;
    let ended = owner_reached(&watched, "ended while it was held", |seen| {
        seen.source_released && seen.owner_ended
    })
    .await;
    let left = peer.client_left().await;
    let recording = peer.finish(context).await;

    assert_held_before_delivery(context, held);
    assert!(cancelled, "{context}: the dropped send was not cancelled");
    assert_unread_release(context, ended, "the dropped send left its task");
    assert_client_left(context, left);
    assert_one_connection_one_request(context, &recording);
}

/// Every bare-Tokio row that runs on one executor of `flavor`.
async fn bare_stream_rows(flavor: Flavor) {
    for _ in 0..flavor.passes() {
        assert_bare_end_acknowledges_release(flavor).await;
        assert_bare_failure_acknowledges_release(flavor).await;
        assert_bare_close_acknowledges_release(flavor).await;
    }
    assert_bare_drop_releases(flavor).await;
    assert_bare_abandoned_demand_is_cancellation(flavor).await;
    assert_bare_status_conversion_requests_release(flavor).await;
    assert_bare_send_dropped_beside_its_head(flavor).await;
    assert_bare_dropped_send_aborts_a_held_owner(flavor).await;
    assert_bare_deadline_releases_a_retained_handle(flavor).await;
}

/// A handle whose owner task is lost with no cause committed reports a
/// transport failure, and never a clean end.
///
/// The executor that owns the task is dropped while the handle lives. The
/// peer runs on a runtime of its own, and the handle is read on a third.
fn assert_bare_task_loss_is_transport(flavor: Flavor) {
    let context = format!("{flavor:?}: an owner lost with its executor");
    let context = context.as_str();
    let mut peer = WatchedPeer::bind(held_head_steps());
    let client = unbounded_client();

    let owning = flavor.runtime();
    let head: StreamHead = owning.block_on(must_settle(
        context,
        "the send",
        stream_get(&client, &peer.url),
    ));
    let mut body: ClientBody = answered(context, head).into_body();
    drop(owning);
    let (next, closed) = flavor.runtime().block_on(async move {
        let next: StreamNext = demand(context, &mut body).await;
        let closed: Result<(), RuntimeError> = close(context, body).await;
        (next, closed)
    });
    let seen: ClientBodyObservation = peer.watched.streamed();
    let left = on_peers(&peer.peers, peer.upstream.client_left());
    let recording = on_peers(&peer.peers, peer.upstream.finish(context));

    assert!(
        failed_as(&next, ClientError::Transport),
        "{context}: the demand of a lost owner returned {next:?}"
    );
    assert!(
        failed_as(&closed, ClientError::Transport),
        "{context}: the close of a lost owner returned {closed:?}"
    );
    assert!(
        seen.source_released && seen.owner_ended,
        "{context}: a lost owner kept its source: {seen:?}"
    );
    assert_client_left(context, left);
    assert_one_request(context, &recording);
}

/// A streaming send polled with no Tokio runtime returns `NoRuntime`. It
/// does not panic, and it dials nothing.
fn assert_stream_without_an_executor_is_no_runtime() {
    assert_unexecuted_send_is_no_runtime(
        "a streaming send polled with no executor",
        |client, url| {
            stream_get(client, url)
                .now_or_never()
                .map(|head: StreamHead| head.map(|response| response.status()))
        },
    );
}

/// A mocked streaming send enters the operation, so one polled with no Tokio
/// runtime returns `NoRuntime` before the mock answers.
///
/// The same send on an executor is then answered by the mock, so the
/// registration matches the refused call too. The mock is called once in all.
fn assert_mocked_stream_without_an_executor_is_no_runtime() {
    const CONTEXT: &str = "a mocked streaming send polled with no executor";

    let url = "http://outbound-lifetime.mock.test/no-executor-stream";
    let canned = Response::text(200, MOCK_BODY).expect("a valid mock status");
    let registered = mock::http(url).returns(canned);
    let client = total_only_client();

    let polled: Option<Result<u16, RuntimeError>> = stream_get(&client, url)
        .now_or_never()
        .map(|head: StreamHead| head.map(|response| response.status()));
    let answered: Result<(u16, Vec<u8>, StreamNext), RuntimeError> =
        Flavor::CurrentThread.runtime().block_on(async {
            let response: ClientStreamResponse = stream_get(&client, url).await?;
            let status: u16 = response.status();
            let mut body: ClientBody = response.into_body();
            let (payload, end) = read_out(CONTEXT, &mut body).await;
            Ok::<_, RuntimeError>((status, payload, end))
        });

    assert!(
        matches!(polled, Some(Err(RuntimeError::NoRuntime))),
        "{CONTEXT}: returned {polled:?}"
    );
    let (status, payload, end) =
        answered.unwrap_or_else(|error| panic!("{CONTEXT}: the mock did not answer: {error:?}"));
    assert_eq!(status, 200, "{CONTEXT}: the mock's status");
    assert_eq!(
        payload,
        MOCK_BODY.as_bytes(),
        "{CONTEXT}: the mock's own answer must arrive"
    );
    assert!(
        matches!(end, Ok(None)),
        "{CONTEXT}: the mock's body ended as {end:?}"
    );
    registered.assert_called_once();
}

/// Streaming 5.T4
#[test]
fn streaming_response_drop_and_close_settle_on_bare_tokio() {
    assert_stream_without_an_executor_is_no_runtime();
    assert_mocked_stream_without_an_executor_is_no_runtime();
    for flavor in [Flavor::CurrentThread, Flavor::MultiThread] {
        flavor.runtime().block_on(bare_stream_rows(flavor));
        assert_bare_task_loss_is_transport(flavor);
    }
}
