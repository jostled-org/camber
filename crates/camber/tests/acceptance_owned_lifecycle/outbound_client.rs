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

use crate::disconnect::fixture::{BOUND, with_drained_server};
use crate::disconnect::peer::start_request;
use crate::disconnect::routes::probe_router;
use crate::disconnect::servers::with_owned_server;
use crate::retry_upstream::{
    Answer, Call, PeerEvent, RunnableDriver, ScriptedUpstream, advance_to, assert_released,
    describe, on_peers, peer_runtime, saw_each, settle_wakeups, settled, spawn_get,
    wait_for_body_collection, within_watchdog,
};

use camber::http;
use camber::http::Client;
use camber::http::ClientError;
use camber::http::ClientResponse;
use camber::http::ClientTimeout;
use camber::http::mock::{self, ScopedTransferOwner};
use camber::http::{Request, Response, Router};
use camber::{RuntimeError, runtime};
use futures_util::FutureExt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;
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
        http::client()
            .total_timeout(UNREACHED)
            .connect_timeout(UNREACHED)
            .request_timeout(UNREACHED)
            .response_idle_timeout(UNREACHED)
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
        let mut end = EndReport {
            ends: ends.clone(),
            returned: None,
        };
        async move {
            let result: Result<ClientResponse, RuntimeError> = client.get(&url).await;
            let returned = result.map(|response| response.status());
            let status = returned.as_ref().map_or(502, |_| 200);
            end.returned = Some(returned);
            drop(end);
            Response::empty(status)
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
    fn end(self, addr: SocketAddr, inbound: std::net::TcpStream) -> Observed {
        let kept_open = match self.trigger {
            Trigger::Shutdown => {
                runtime::request_shutdown();
                Some(inbound)
            }
            Trigger::Disconnect => {
                drop(inbound);
                None
            }
        };
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
    let collected = mock::transfer_owner(upstream.addr()).expect("one observer per upstream");
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

    let (observed, drained) = match trigger {
        Trigger::Shutdown => (with_owned_server(router, |addr| journey.run(addr)), None),
        Trigger::Disconnect => {
            let (observed, drained) = with_drained_server(None, router, |addr| journey.run(addr));
            (observed, Some(drained))
        }
    };
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
    http::client()
        .total_timeout(BARE_TOTAL)
        .connect_timeout(UNREACHED)
        .request_timeout(UNREACHED)
        .response_idle_timeout(UNREACHED)
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
    let joined = within_watchdog(call).await;
    let released = upstream.saw(PeerEvent::Released(0)).await;
    let starts = BareRow::finish(driver, upstream, &context).await;

    assert!(
        matches!(&joined, Some(Err(error)) if error.is_cancelled()),
        "{context}: the dropped call was not cancelled"
    );
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
    let listener = crate::common::UndialedListener::bind();
    let url = format!("http://{}{UPSTREAM_PATH}", listener.addr());
    let client: Client = http::client().total_timeout(UNREACHED);

    let polled: Option<Result<ClientResponse, RuntimeError>> = client.get(&url).now_or_never();

    assert!(
        matches!(polled, Some(Err(RuntimeError::NoRuntime))),
        "a call polled with no executor returned {}",
        describe(&polled)
    );
    listener.assert_undialed("a call polled with no executor");
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
    let client: Client = http::client().total_timeout(UNREACHED);

    let polled: Option<Result<ClientResponse, RuntimeError>> = client.get(url).now_or_never();
    let answered: Result<ClientResponse, RuntimeError> =
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a current-thread executor")
            .block_on(client.get(url));

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
