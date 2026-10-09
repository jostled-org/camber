//! One owner for everything a direct-WebSocket direction case binds, starts,
//! connects, arms, or spawns.
//!
//! Direction cases hold a live listener, an owned server, a raw client socket,
//! application tasks suspended in an endpoint operation, and armed production
//! checkpoints at once. A case that asserts its way out through an
//! unwind leaves every one of them behind: a parked checkpoint keeps the
//! production bridge from ever finishing, and a bridge that never finishes keeps
//! the server from joining and the port from being rebindable. So cleanup here
//! is not `Drop` order — it is an explicit bounded protocol the runner performs
//! before it resumes the unwind, and `Drop` is only the net under it.

#![cfg(feature = "ws")]

use std::future::Future;
use std::net::{SocketAddr, TcpStream};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::mock::{
    ScopedRetainedBridge, WebSocketDirectionEdge, WebSocketTerminalEdge, retained_bridge,
};
use camber::http::{
    Request, Router, ServerHandle, ServerHandleFuture, WsCloseCause, WsConn, WsMessage, WsReceive,
    WsReceiver, WsSender,
};
use futures_util::FutureExt;

use super::http::{
    BridgeHold, assert_server_joined, bounded_pause, reserve_registered, serve_router_with_policy,
};
use super::ws_async::{expect_async_close, lifecycle_event};
use super::ws_callbacks::{CallbackRelease, callback_gate, park_until_released};

/// The bound every wait, join, read, and shutdown in a direction case runs
/// under.
///
/// One value rather than one per operation: every wait here is on an in-process
/// localhost transport that either settles immediately or is never going to,
/// and a case that picked its own bound would be stating a scheduling claim it
/// does not own.
///
/// The suite's own async-observation bound rather than a fourth spelling of the
/// same five seconds: [`lifecycle_event`] already waits under it, so a separate
/// constant here would only be one more thing that could come to disagree about
/// how long a direction case is allowed to wait.
pub const DIRECTION_DEADLINE: Duration = super::ws_async::ASYNC_EVENT_TIMEOUT;

/// What a caught unwind carries, ready to be resumed.
type Unwound = Box<dyn std::any::Any + Send>;

/// What the direct callback hands the case, and what keeps it parked.
///
/// Held apart from the fixture because the router that carries it is built
/// before the listener exists.
pub struct DirectionHandoff {
    connections: tokio::sync::mpsc::Receiver<WsConn>,
    /// The other end of the channel every parked callback waits on.
    ///
    /// Never read and never sent on: it is held for its `Drop`. While it
    /// exists, the callback's own wait cannot end, and letting it go is what
    /// lets every parked callback return.
    ///
    /// In an `Option` because the fixture takes it. A parked callback is a
    /// child its connection cannot settle without, so the row that asks its
    /// server to stop has to be the one that lets the callback go — a gate
    /// released only when this handoff drops would hold the connection past
    /// the stop and turn every such row into a drain that ran out.
    parked: Option<CallbackRelease>,
}

impl DirectionHandoff {
    /// Take the gate every parked callback of this row waits on.
    pub fn take_gate(&mut self) -> Option<CallbackRelease> {
        self.parked.take()
    }

    /// The next connection the production callback was given.
    pub async fn connection(&mut self) -> WsConn {
        lifecycle_event(
            "the direct callback to hand out its connection",
            self.connections.recv(),
        )
        .await
        .expect("the direct callback never handed out a connection")
    }
}

/// A router whose direct callback hands its connection out and parks, at the
/// default queue capacity.
///
/// Parking rather than returning, because every row here owns the halves
/// through the callback's own lifetime: a callback that returned would drop
/// whichever half the row had not taken, and end the connection under it.
pub fn parking_router(path: &str) -> (Router, DirectionHandoff) {
    let (connections_tx, connections) = tokio::sync::mpsc::channel(4);
    let (parked, parked_rx) = callback_gate();
    let mut router = Router::new();
    router.ws(path, move |_request: &Request, connection: WsConn| {
        let connections_tx = connections_tx.clone();
        let parked_rx = parked_rx.clone();
        async move {
            connections_tx
                .send(connection)
                .await
                .map_err(|_| RuntimeError::ChannelClosed)?;
            park_until_released(&parked_rx).await;
            Ok(())
        }
    });
    (
        router,
        DirectionHandoff {
            connections,
            parked: Some(parked),
        },
    )
}

/// The same parking router, with both queues sized to `buffer`.
pub fn direction_router(path: &str, buffer: usize) -> (Router, DirectionHandoff) {
    let (router, handoff) = parking_router(path);
    (router.ws_buffer_size(buffer), handoff)
}

/// The route every direction row registers.
pub const DIRECTION_PATH: &str = "/ws";

/// The second route a returning row serves: a callback that never comes back.
pub const PARKED_PATH: &str = "/parked";

/// A router whose direct callback splits its connection, hands both halves out,
/// and returns — beside a second one that keeps its connection and parks.
///
/// The returning callback is the opposite of [`direction_router`] in the one way
/// that matters: it exits while the application still owns both halves, which is
/// the only way to ask whether a callback return ends a connection. The parked
/// route is what the same row asks the other half of that question with — a
/// callback future still suspended when its connection ends is settled by the
/// bridge that owns it, so the owned server's completion means that future is
/// gone — and it has to be served by the same server, since that server's
/// completion is the claim.
///
/// The parked callback reports that it is suspended and does nothing else: it
/// holds its connection until its bridge cancels it at the settlement deadline.
pub fn returning_direction_router(path: &str, buffer: usize) -> (Router, ReturningHandoff) {
    let (halves_tx, halves) = tokio::sync::mpsc::channel(4);
    let (returned_tx, returned) = tokio::sync::mpsc::channel(4);
    let (entered_tx, parked_entered) = tokio::sync::mpsc::channel(4);
    let parked_frame = CallbackFrame::new();
    let (parked, parked_rx) = callback_gate();
    let entered_frame = parked_frame.clone();
    let mut router = Router::new();
    router.ws(path, move |_request: &Request, connection: WsConn| {
        let halves_tx = halves_tx.clone();
        let returned_tx = returned_tx.clone();
        async move {
            halves_tx
                .send(connection.split())
                .await
                .map_err(|_| RuntimeError::ChannelClosed)?;
            returned_tx
                .send(())
                .await
                .map_err(|_| RuntimeError::ChannelClosed)?;
            Ok(())
        }
    });
    router.ws(
        PARKED_PATH,
        move |_request: &Request, connection: WsConn| {
            let exit = entered_frame.enter();
            let entered_tx = entered_tx.clone();
            let parked_rx = parked_rx.clone();
            async move {
                let _held = (exit, connection);
                entered_tx
                    .send(())
                    .await
                    .map_err(|_| RuntimeError::ChannelClosed)?;
                park_until_released(&parked_rx).await;
                Ok(())
            }
        },
    );
    (
        router.ws_buffer_size(buffer),
        ReturningHandoff {
            halves,
            returned,
            parked_entered,
            parked_frame,
            _parked: parked,
        },
    )
}

/// One callback future's captures, seen from outside it.
///
/// The case holds one of these to ask whether that future still exists. The
/// callback captures the [`CallbackExit`] it hands out, which answers by
/// dropping: a flag set on drop rather than before the callback's last
/// statement, because the claim is about the future itself, and only its
/// destruction — by returning, unwinding, or being cancelled — can say
/// otherwise.
#[derive(Clone)]
struct CallbackFrame(Arc<std::sync::atomic::AtomicBool>);

impl CallbackFrame {
    fn new() -> Self {
        Self(Arc::new(std::sync::atomic::AtomicBool::new(false)))
    }

    /// The guard one entry of the callback holds for its own lifetime.
    fn enter(&self) -> CallbackExit {
        CallbackExit(self.clone())
    }

    fn left(&self) -> bool {
        self.0.load(std::sync::atomic::Ordering::Acquire)
    }
}

/// The callback's own end of a [`CallbackFrame`].
struct CallbackExit(CallbackFrame);

impl Drop for CallbackExit {
    fn drop(&mut self) {
        self.0.0.store(true, std::sync::atomic::Ordering::Release);
    }
}

/// What a returning callback hands the case, and how it reports its own exit.
pub struct ReturningHandoff {
    halves: tokio::sync::mpsc::Receiver<(WsSender, WsReceiver)>,
    returned: tokio::sync::mpsc::Receiver<()>,
    parked_entered: tokio::sync::mpsc::Receiver<()>,
    parked_frame: CallbackFrame,
    /// The other end of the channel the parked callback waits on.
    ///
    /// Never sent on, and held for the handoff's whole life: the row's claim is
    /// that its owner's completion ends the parked callback, so this release
    /// must never be what does. Its `Drop` only lets the callback go on a path
    /// where the row failed first.
    _parked: CallbackRelease,
}

impl ReturningHandoff {
    /// The two halves the production callback split out of its connection.
    pub async fn halves(&mut self) -> (WsSender, WsReceiver) {
        lifecycle_event(
            "the direct callback to split out its halves",
            self.halves.recv(),
        )
        .await
        .expect("the direct callback never handed out its halves")
    }

    /// Wait for the callback to report that it returned.
    pub async fn wait_returned(&mut self) {
        lifecycle_event("the direct callback to return", self.returned.recv())
            .await
            .expect("the direct callback never returned");
    }

    /// Wait for the parked callback to report that it is suspended.
    pub async fn wait_parked(&mut self) {
        lifecycle_event(
            "the parked direct callback to report that it entered",
            self.parked_entered.recv(),
        )
        .await
        .expect("the parked direct callback never entered");
    }

    /// Whether the parked callback's future has been destroyed.
    pub fn parked_exited(&self) -> bool {
        self.parked_frame.left()
    }
}

/// The same row as [`async_direction_row`], over a peer whose close is a TCP reset.
///
/// Only a Tokio socket can be given the zero linger that turns its close into
/// the transport failure a `PeerDisconnected` row needs.
pub async fn abortive_direction_row<C, Fut>(buffer: usize, case: C)
where
    C: FnOnce(Arc<DirectionTestFixture>, tokio::net::TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    abortive_direction_row_with_shutdown(buffer, DIRECTION_DEADLINE, case).await;
}

pub async fn abortive_direction_row_with_shutdown<C, Fut>(
    buffer: usize,
    shutdown: Duration,
    case: C,
) where
    C: FnOnce(Arc<DirectionTestFixture>, tokio::net::TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    direction_row_with_shutdown(buffer, shutdown, DirectionPeer::Abortive, case).await;
}

/// The capacity a child router under a `HostRouter` row configures for itself.
///
/// Deliberately not the capacity that row claims: the top-level owner is what
/// serving captures, so a child value that still reached the queues would show
/// up as this number instead of the one the row set.
const IGNORED_CHILD_BUFFER: usize = 8;

/// Serve one capacity-`buffer` direct route, connect a peer, and hand the case
/// the fixture, that peer, and the connection the production callback was
/// given.
///
/// Every row here needs the same five steps in the same order, and a row that
/// wrote them out again is a row whose listener, server, callback handoff, and
/// teardown can drift from the others. Peer I/O is async, so the case never
/// blocks the server's executor.
pub async fn async_direction_row<C, Fut>(buffer: usize, case: C)
where
    C: FnOnce(Arc<DirectionTestFixture>, tokio::net::TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    async_direction_row_with_shutdown(buffer, DIRECTION_DEADLINE, case).await;
}

pub async fn async_direction_row_with_shutdown<C, Fut>(buffer: usize, shutdown: Duration, case: C)
where
    C: FnOnce(Arc<DirectionTestFixture>, tokio::net::TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    direction_row_with_shutdown(buffer, shutdown, DirectionPeer::Orderly, case).await;
}

/// Serve one bounded direct route under `shutdown` and run `case` over a
/// `peer` of the kind the row names.
async fn direction_row_with_shutdown<C, Fut>(
    buffer: usize,
    shutdown: Duration,
    peer: DirectionPeer,
    case: C,
) where
    C: FnOnce(Arc<DirectionTestFixture>, tokio::net::TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    let (router, handoff) = direction_router(DIRECTION_PATH, buffer);
    run_async_row(
        |listener| bounded_direction_server(listener, router, shutdown),
        handoff,
        peer,
        case,
    )
    .await;
}

/// How a row's peer ends its side of the transport.
#[derive(Clone, Copy)]
enum DirectionPeer {
    /// An orderly TCP close.
    Orderly,
    /// A TCP reset, for the rows about a transport failure.
    Abortive,
}

/// Bind, serve, connect, take the callback's connection, run the case.
///
/// The handoff stays owned here for the whole case, so the production callback
/// stays parked in its own frame rather than returning under the row. Its gate
/// goes to the fixture, which is what releases the callback when the row asks
/// its server to stop.
async fn run_async_row<S, C, Fut>(
    serve: S,
    mut handoff: DirectionHandoff,
    peer: DirectionPeer,
    case: C,
) where
    S: FnOnce(tokio::net::TcpListener) -> ServerHandle,
    C: FnOnce(Arc<DirectionTestFixture>, tokio::net::TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    DirectionTestFixture::run(serve, |fixture| async move {
        fixture.hold_callbacks(handoff.take_gate());
        let peer = match peer {
            DirectionPeer::Orderly => fixture.connect_async(DIRECTION_PATH).await,
            DirectionPeer::Abortive => fixture.connect_abortive(DIRECTION_PATH).await,
        };
        let connection = handoff.connection().await;
        case(fixture, peer, connection).await;
        drop(handoff);
    })
    .await;
}

fn bounded_direction_server(
    listener: tokio::net::TcpListener,
    router: Router,
    shutdown: Duration,
) -> ServerHandle {
    serve_router_with_policy(
        listener,
        router,
        camber::http::ServerPolicy::default()
            .connection_limit(1)
            .expect("one connection is a valid bound")
            .shutdown_timeout(shutdown)
            .expect("the direction deadline is finite"),
    )
}

pub async fn async_returning_direction_row<C, Fut>(buffer: usize, case: C)
where
    C: FnOnce(Arc<DirectionTestFixture>, tokio::net::TcpStream, ReturningHandoff) -> Fut,
    Fut: Future<Output = ()>,
{
    let (router, handoff) = returning_direction_router(DIRECTION_PATH, buffer);
    DirectionTestFixture::run(
        |listener| {
            camber::http::serve_background(listener, router)
                .expect("owned server requires a Tokio runtime")
        },
        |fixture| async move {
            let peer = fixture.connect_async(DIRECTION_PATH).await;
            case(fixture, peer, handoff).await;
        },
    )
    .await;
}

/// The same row, with the capacity configured on a `HostRouter` instead.
///
/// A separate public construction path, not a second spelling of the same one:
/// the child router below configures its own capacity and that value must not
/// reach either queue.
pub async fn async_host_direction_row<C, Fut>(buffer: usize, case: C)
where
    C: FnOnce(Arc<DirectionTestFixture>, tokio::net::TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    let (router, handoff) = direction_router(DIRECTION_PATH, IGNORED_CHILD_BUFFER);
    let mut hosts = camber::http::HostRouter::new();
    hosts.set_default(router);
    let hosts = hosts.ws_buffer_size(buffer);
    run_async_row(
        |listener| {
            camber::http::serve_background_hosts(listener, hosts)
                .expect("owned server requires a Tokio runtime")
        },
        handoff,
        DirectionPeer::Orderly,
        case,
    )
    .await;
}

/// What one application task the case started reports back.
///
/// The task itself belongs to the fixture, which joins it; this is only the
/// bounded way to read its answer.
pub struct WorkerResult<T> {
    label: Box<str>,
    outcome: tokio::sync::oneshot::Receiver<T>,
}

impl<T> WorkerResult<T> {
    /// The worker's answer, or a failure naming the worker that never gave one.
    pub async fn take(self) -> T {
        match tokio::time::timeout(DIRECTION_DEADLINE, self.outcome).await {
            Ok(Ok(outcome)) => outcome,
            Ok(Err(_)) => panic!("{}: the task ended without an answer", self.label),
            Err(_) => panic!("{}: no answer within {DIRECTION_DEADLINE:?}", self.label),
        }
    }
}

/// Poll one endpoint operation once and require that it is waiting.
///
/// A single poll in the caller's own task, so the claim is about the operation
/// and not about when another task was scheduled: a `Pending` here is the
/// operation itself declining to finish. The operation stays pinned with the
/// caller, who decides whether to drive it on or to drop it.
pub async fn assert_pending<F>(operation: Pin<&mut F>, what: &str)
where
    F: Future,
    F::Output: std::fmt::Debug,
{
    match futures_util::poll!(operation) {
        Poll::Pending => {}
        Poll::Ready(outcome) => panic!("{what} finished with {outcome:?} instead of waiting"),
    }
}

/// What the three untimed facade receives answered, and the connection that
/// answered them.
pub struct FacadeReceives {
    pub text: Option<Box<str>>,
    pub binary: Option<Box<[u8]>>,
    pub either: Option<WsMessage>,
    pub connection: WsConn,
}

impl FacadeReceives {
    /// Take one message through each untimed facade receiver, in order.
    ///
    /// Untimed, so the caller bounds the whole of it: a facade that stopped
    /// answering would otherwise park the row. The connection comes back with
    /// the answers, so the rest of the row still owns it.
    pub async fn take(mut connection: WsConn) -> Self {
        let text = connection.recv().await;
        let binary = connection
            .recv_binary()
            .await
            .map(|data| Box::<[u8]>::from(data.as_ref()));
        let either = connection.recv_message().await;
        Self {
            text,
            binary,
            either,
            connection,
        }
    }
}

/// Everything one direction case binds, starts, arms, or spawns.
pub struct DirectionTestFixture {
    addr: SocketAddr,
    /// This listener's observer, held only until teardown is finished with it.
    ///
    /// In an `Option` because the registration is keyed by address: the
    /// registry refuses a second controller for one address, so a fixture that
    /// held its own until the last handle dropped would publish the port as
    /// reusable while still occupying the entry a case drawing that port needs.
    /// [`DirectionTestFixture::finish`] takes it back first.
    controller: Mutex<Option<Arc<ScopedRetainedBridge>>>,
    server: Mutex<Option<ServerHandle>>,
    armed: Mutex<Vec<BridgeHold>>,
    workers: Mutex<Vec<(Box<str>, tokio::task::JoinHandle<()>)>>,
    /// The gate this row's parked callbacks wait on, once the row hands it over.
    ///
    /// `None` for a row whose callbacks park on nothing. Held rather than read:
    /// dropping it is the release.
    callbacks: Mutex<Option<CallbackRelease>>,
}

impl DirectionTestFixture {
    /// Reserve an observed listener, serve it, and run `case` against the
    /// result.
    ///
    /// The reservation is the suite's own: [`reserve_registered`] binds the
    /// ephemeral port and registers its observer before anything serves on it,
    /// which is the ordering every listener-scoped observation here depends on.
    /// The observer names the four families a retained bridge has and no
    /// others: the direction owners a row holds a transport half at, the
    /// terminal owner that commits the one cause both halves end on, the
    /// upgrade child that retained the callback they settle around, and the
    /// connection owner that says the child was its own. A row here cannot
    /// reach the supervisor pass or the permit that admitted the connection
    /// under it.
    /// The server is served from that reservation rather than through the
    /// guarded fixtures beside it, because a case here reads the server's own
    /// completion outcome and stops it at an exact moment — see
    /// [`Self::stop`] for what teardown owes in exchange.
    ///
    /// The case's unwind is caught rather than allowed to propagate, because
    /// every resource above outlives the assertion that failed. The fixture is
    /// shared rather than moved into the case for the same reason: a moved
    /// fixture is dropped inside the unwinding future, which leaves `Drop` — a
    /// synchronous net that cannot join anything — as the only cleanup on the
    /// one path where a parked worker is most likely. Holding a second handle
    /// here keeps the fixture alive past the unwind, so the same bounded
    /// [`Self::finish`] protocol runs on failure as on success, and the unwind
    /// is resumed after it.
    pub async fn run<S, C, Fut>(serve: S, case: C)
    where
        S: FnOnce(tokio::net::TcpListener) -> ServerHandle,
        C: FnOnce(Arc<DirectionTestFixture>) -> Fut,
        Fut: Future<Output = ()>,
    {
        let (listener, addr, controller) = reserve_registered(retained_bridge).into_owned_parts();
        let fixture = Arc::new(Self {
            addr,
            controller: Mutex::new(Some(controller)),
            server: Mutex::new(Some(serve(listener))),
            armed: Mutex::new(Vec::new()),
            workers: Mutex::new(Vec::new()),
            callbacks: Mutex::new(None),
        });
        let cased = AssertUnwindSafe(case(Arc::clone(&fixture)))
            .catch_unwind()
            .await;
        let finished = AssertUnwindSafe(fixture.finish()).catch_unwind().await;
        Self::report(cased, finished);
    }

    /// Report the case's own failure ahead of any cleanup failure, and lose
    /// neither.
    ///
    /// The case's assertion is what the row is about, so it is the one resumed.
    /// A cleanup failure behind it still names a leaked owner, an unreleased
    /// gate, or an address the server never gave back, so it is printed rather
    /// than dropped.
    fn report(cased: Result<(), Unwound>, finished: Result<(), Unwound>) {
        match (cased, finished) {
            (Err(cased), Ok(())) => std::panic::resume_unwind(cased),
            (Err(cased), Err(_)) => {
                eprintln!(
                    "the direction fixture also failed its bounded teardown after the case failed"
                );
                std::panic::resume_unwind(cased)
            }
            (Ok(()), Err(finished)) => std::panic::resume_unwind(finished),
            (Ok(()), Ok(())) => {}
        }
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// This fixture's listener observer, for the length of one read.
    ///
    /// A handle rather than a borrow: teardown gives the registration back, and
    /// nothing behind a lock can be borrowed past that. A case asking for one
    /// afterwards is reading a listener this fixture has already finished with,
    /// so it is reported rather than answered.
    ///
    /// The handle carries the registration, so a case that stores one holds the
    /// registry entry teardown gave back and re-opens the address-reuse race it
    /// closes. [`Self::observed`] is what a row reads through instead.
    pub fn controller(&self) -> Arc<ScopedRetainedBridge> {
        self.controller
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(Arc::clone)
            .expect("the direction fixture already gave back its lifecycle observer")
    }

    pub async fn connect_async(&self, path: &str) -> tokio::net::TcpStream {
        super::ws_callbacks::upgraded_ws_peer(self.addr, path, "the direction peer").await
    }

    /// Open one client WebSocket whose close will be a TCP reset.
    ///
    /// A row takes this peer exactly when the transport failure, rather than
    /// an orderly close, is what it is about.
    pub async fn connect_abortive(&self, path: &str) -> tokio::net::TcpStream {
        super::ws_callbacks::abortive_upgraded_ws_peer(
            self.addr,
            path,
            "the abortive direction peer",
        )
        .await
    }

    /// Arm one production owner's edge, recording it for release at teardown.
    ///
    /// The edge is named through [`BridgeHold`], which carries the name and no
    /// authority: each arm reaches production through exactly the narrow
    /// controller that owns it, so a row here can hold a direction or a
    /// terminal owner and can offer neither one a cause.
    pub fn arm(&self, hold: BridgeHold) {
        hold.arm_on(&self.controller())
            .expect("arm direction owner edge");
        self.armed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(hold);
    }

    /// Wait until production reaches an armed edge.
    ///
    /// The context names the edge. A row here waits at several of them in
    /// sequence, and one shared wording turns every expiry into the same
    /// sentence — which says a pause never arrived without saying which.
    pub async fn wait_paused(&self, hold: BridgeHold) {
        let owners = self.controller();
        bounded_pause(
            hold.paused_on(&owners),
            &format!("production reaching direction owner edge {hold:?}"),
        )
        .await;
    }

    /// Release one armed edge and stop owning it.
    pub fn release(&self, hold: BridgeHold) {
        hold.release_on(&self.controller())
            .expect("release direction owner edge");
        self.forget(hold);
    }

    /// Record one held direction's release without waking it.
    ///
    /// The direction stays parked and observes the release on whatever poll
    /// something else provokes. It is the only way to put a frame the direction
    /// is already holding into the same turn as an event the row publishes
    /// afterwards: an ordinary release wakes it, and the turn is spent before
    /// the second event exists.
    ///
    /// The direction has to still own what it is holding for that to be worth
    /// staging. A coordinator answering in the same turn drops the direction's
    /// running future, so a row stages the release at an edge where the frame
    /// lives on the pump — never at one where it lives on that future's own
    /// stack, which is a frame the row's own staging then loses.
    pub fn release_without_waking(&self, edge: WebSocketDirectionEdge) {
        self.controller()
            .directions
            .release_without_waking(edge)
            .expect("release the held direction without waking it");
        self.forget(BridgeHold::Direction(edge));
    }

    /// Queue one async peer message, and hold until the receive owner can see
    /// it.
    ///
    /// The queued edge is the barrier: a row that wants a receive to find a
    /// message ready must not guess when the inbound owner put it there. The
    /// edge is armed before the write, so the pump cannot pass it unheld, and
    /// released only once the message is in the queue.
    pub async fn queue_from_async_peer(
        &self,
        peer: &mut tokio::net::TcpStream,
        opcode: u8,
        payload: &[u8],
        context: &str,
    ) {
        self.arm(QUEUED);
        super::ws_async::write_async_ws_frame(peer, opcode, payload, context).await;
        self.wait_paused(QUEUED).await;
        self.release(QUEUED);
    }

    /// Take this row's parked-callback gate, so the fixture can release it.
    pub fn hold_callbacks(&self, gate: Option<CallbackRelease>) {
        *self
            .callbacks
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = gate;
    }

    /// Let every parked callback of this row return.
    ///
    /// Idempotent: the gate leaves once, and a second call finds nothing left
    /// to release. A row with no parked callback releases nothing.
    pub fn release_callbacks(&self) {
        drop(
            self.callbacks
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take(),
        );
    }

    fn forget(&self, hold: BridgeHold) {
        self.armed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|armed| *armed != hold);
    }

    /// What this fixture's listener has published about its direct bridges.
    pub fn observed(&self) -> camber::http::mock::WebSocketDirectionObservation {
        self.controller().terminals.observed()
    }

    /// What those bridges have published about the callbacks they started.
    ///
    /// Every record one bridge appended, in the order it appended them: the
    /// deadline it fixed when it closed its callback's endpoints, any later
    /// transition that brought that deadline forward, and the disposition the
    /// join ended at.
    pub fn callbacks(&self) -> Box<[camber::http::mock::WebSocketCallbackObservation]> {
        self.controller().upgrades.callbacks()
    }

    /// What this fixture's listener recorded about the connections under it.
    ///
    /// Read-only, and the only place a row can ask whose child a bridge's
    /// upgrade was: the transfer and the settlement are both written by the
    /// connection owner that performed them.
    pub fn ownership(&self) -> camber::http::mock::ConnectionOwnershipObservation {
        self.controller().connections.observed()
    }

    /// Ask this fixture's server to stop gracefully, without taking it.
    ///
    /// Taking it is what [`Self::finish`] does, and a case that needs its
    /// server to still be joinable afterwards cannot be the one that takes it.
    pub fn shutdown_server(&self) {
        self.with_server(ServerHandle::shutdown, "shut down");
    }

    /// Take this fixture's server's cancellation authority, without taking it.
    pub fn cancel_server(&self) {
        self.with_server(ServerHandle::cancel, "cancel");
    }

    /// Commit this server's cancellation, then hold the bridge on the far side
    /// of the cause it fixed.
    ///
    /// The public command is the whole barrier: `cancel` returns only once the
    /// forced phase is committed in the shared stop state, and the bridge's own
    /// commit is taken inside that state's lock — so every cause it could
    /// otherwise have offered afterwards, including the disconnect that
    /// cancellation's own transport close produces, reads the earlier fact.
    /// The terminal edge stays armed so the caller can inspect the committed
    /// cause before allowing settlement.
    pub async fn select_server_cancellation(&self) {
        self.arm(AFTER_COMMIT);
        self.cancel_server();
        self.wait_paused(AFTER_COMMIT).await;
        assert_eq!(
            self.observed().terminal,
            Some(WsCloseCause::ServerCancelled),
            "the bridge committed another cause after cancellation was accepted"
        );
    }

    /// Ask this fixture's still-held server to do one thing.
    ///
    /// A server the case has already finished with is a case asking for
    /// something that cannot happen, not a no-op: the row would go on to assert
    /// against a stop it never requested.
    fn with_server(&self, ask: impl FnOnce(&ServerHandle), what: &str) {
        // Before the command, never after it. A parked callback is a child of
        // the connection that started it, so a server told to stop while one is
        // still in application code drains for the whole grace and reports the
        // row's own fixture rather than its claim.
        self.release_callbacks();
        let held = self
            .server
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        match held.as_ref() {
            Some(server) => ask(server),
            None => panic!("the direction fixture no longer holds a server to {what}"),
        }
    }

    /// Take this fixture's server and wait for it to complete.
    ///
    /// The case owns the outcome from here; [`Self::finish`] finds no server
    /// left and skips its own stop. A row that asserts on completion order —
    /// pumps settled, permit released, transport gone — needs the completion
    /// itself, not the teardown's best effort at one.
    pub async fn join_server(&self) -> Result<(), RuntimeError> {
        let server = self
            .server
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
            .expect("the direction fixture no longer holds a server to join");
        lifecycle_event("the direction server to complete", server.join()).await
    }

    /// Start one application task and register it for a bounded join.
    ///
    /// An owned task rather than a detached one: the fixture joins it at
    /// teardown, after the server, and a task still running then is a leak
    /// the row fails on.
    pub fn spawn_worker<T, F>(&self, label: &str, work: F) -> WorkerResult<T>
    where
        T: Send + 'static,
        F: Future<Output = T> + Send + 'static,
    {
        let (reported, outcome) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = reported.send(work.await);
        });
        self.workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((label.into(), task));
        WorkerResult {
            label: format!("worker `{label}` never reported").into(),
            outcome,
        }
    }

    /// Release every held gate, shut the server down, join every owner, give
    /// the listener's observer back, and prove its address is free again.
    ///
    /// Ordered, not incidental: a still-parked checkpoint holds the production
    /// bridge, the bridge holds the server, and the server holds the address.
    /// Doing any of these out of order turns a clean teardown into the hang it
    /// exists to prevent. The observer goes back last but one, because the
    /// address is published as reusable by the step after it and a registration
    /// this fixture still held would refuse the next case that drew this port.
    async fn finish(&self) {
        self.release_every_gate();
        // Before the stop below, for the reason the stop commands release it:
        // a callback still in application code is a child the connection that
        // started it cannot settle without, so a teardown that stopped first
        // would drain to its deadline and report this fixture rather than the
        // row it was running.
        self.release_callbacks();
        let server = self
            .server
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        match server {
            Some(server) => Self::stop(server).await,
            None => {}
        }
        self.join_workers().await;
        self.release_observer();
        Self::prove_address_reuse(self.addr).await;
    }

    /// Give this listener's registration back.
    ///
    /// The registry is keyed by address and refuses a second controller for
    /// one, so the entry has to be gone before [`Self::prove_address_reuse`]
    /// advertises the port as free — otherwise a concurrent case in the same
    /// binary that draws the freed ephemeral port is refused an observer of its
    /// own and fails for this fixture's reason.
    fn release_observer(&self) {
        drop(
            self.controller
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take(),
        );
    }

    /// Let go of every owner edge the case armed and did not spend.
    ///
    /// An edge that was armed but never reached is not paused, and one the case
    /// already released is spent; both refuse the release and both are fine.
    /// What must not happen is a reached-and-held edge surviving teardown, and
    /// that is the case this covers.
    fn release_every_gate(&self) {
        let armed =
            std::mem::take(&mut *self.armed.lock().unwrap_or_else(|error| error.into_inner()));
        let controller = self
            .controller
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(Arc::clone);
        // A fixture that already gave its observer back has nothing to release:
        // closing the controller lets go of every checkpoint it still held.
        match controller {
            Some(controller) => armed.into_iter().for_each(|hold| {
                let _ = hold.release_on(&controller);
            }),
            None => {}
        }
    }

    /// Ask the server to stop gracefully, then take its cancellation authority
    /// if it does not.
    ///
    /// The graceful join's own outcome is asserted rather than discarded. A
    /// server whose stop expired, whose supervisor failed, or that only ended
    /// because teardown escalated is exactly the leak these rows exist to catch,
    /// and a match that reads only "the timer did not fire" reports none of the
    /// three.
    ///
    /// The future is pinned in place rather than boxed: it is carried across two
    /// armings and never moved, which is what [`std::pin::pin`] is for.
    async fn stop(server: ServerHandle) {
        server.shutdown();
        let mut joined = std::pin::pin!(server.join());
        match tokio::time::timeout(DIRECTION_DEADLINE, joined.as_mut()).await {
            Ok(completed) => assert_server_joined(Ok(completed)),
            Err(_) => Self::cancel_and_join(joined.as_mut()).await,
        }
    }

    /// Cancel a server that would not stop gracefully, and fail the case for
    /// having had to.
    ///
    /// The escalation is reported rather than quietly performed: a graceful stop
    /// that never completed is a fault of the row, and a teardown that upgraded
    /// to cancellation on its behalf would leave the row green. The cancelled
    /// join is asserted first, because a server that will not stop even for a
    /// cancellation is the worse of the two faults and names itself.
    async fn cancel_and_join(mut joined: Pin<&mut ServerHandleFuture>) {
        joined.cancel();
        assert_server_joined(tokio::time::timeout(DIRECTION_DEADLINE, joined.as_mut()).await);
        panic!(
            "the direction server did not stop gracefully within {DIRECTION_DEADLINE:?}, so teardown cancelled it"
        );
    }

    /// Join every application task the case started, under one bound each.
    ///
    /// A worker still suspended in an endpoint operation after the server has
    /// joined is the leak these cases exist to catch, so it is cancelled,
    /// joined, and then fails the case rather than being waited on.
    ///
    /// What the join answered is read rather than only whether it answered. A
    /// worker that panicked joins promptly and carries its panic back in that
    /// answer, so a bound that only checks the timer reports a case's own
    /// failed assertion as a healthy owner — and any row spawning a worker it
    /// never takes loses its assertion entirely. The payload is resumed here so
    /// the row fails on what the worker actually claimed.
    async fn join_workers(&self) {
        let workers = std::mem::take(
            &mut *self
                .workers
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for (label, mut task) in workers {
            match tokio::time::timeout(DIRECTION_DEADLINE, &mut task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
                Ok(Err(error)) => panic!("the join of worker `{label}` failed: {error}"),
                Err(_) => {
                    task.abort();
                    let cancelled = tokio::time::timeout(DIRECTION_DEADLINE, task).await;
                    panic!(
                        "worker `{label}` was still running {DIRECTION_DEADLINE:?} after the server joined; its cancelled join answered {cancelled:?}"
                    )
                }
            }
        }
    }

    /// Prove the listener's address is bindable and observable again.
    ///
    /// The permit, the transport, and the listener are all released by the same
    /// completion, and the bind is the one of the three an out-of-process
    /// observer can check without asking Camber.
    ///
    /// The bind is the shared bounded one, for the reason stated there: a row's
    /// own peers are still being torn down when its server completes, and a
    /// single ask reports that window as a listener still held.
    ///
    /// The registration is the half only Camber can answer. The registry is
    /// keyed by address and refuses a second controller for one, so a fixture
    /// that published this port as free while still holding its entry would fail
    /// an unrelated case that drew the port next. Taking a controller here is
    /// what makes [`Self::release_observer`] falsifiable: the failure lands on
    /// the fixture that leaked the entry rather than on whoever inherited it.
    async fn prove_address_reuse(addr: SocketAddr) {
        match super::http::rebind_within(addr, DIRECTION_DEADLINE).await {
            Ok(listener) => drop(listener),
            Err(error) => panic!(
                "the direction listener's address {addr} was still held \
                 {DIRECTION_DEADLINE:?} after completion: {error}"
            ),
        }
        drop(camber::http::mock::unwatched(addr).unwrap_or_else(|error| {
            panic!("the direction listener's observer for {addr} outlived its fixture: {error}")
        }));
    }
}

impl Drop for DirectionTestFixture {
    /// The net under [`DirectionTestFixture::finish`], never a substitute for
    /// it.
    ///
    /// A case that unwound before its runner could finish still has to let
    /// production out of every gate it armed and stop the server, or the whole
    /// binary hangs on a bridge nothing will ever release. Worker tasks are
    /// cancelled rather than joined. Nothing here waits:
    /// this runs on whatever thread the unwind is on, and a bounded wait needs
    /// an executor.
    fn drop(&mut self) {
        self.release_every_gate();
        self.release_callbacks();
        self.workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .for_each(|(_, task)| task.abort());
        match self
            .server
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            Some(server) => server.cancel(),
            None => {}
        }
    }
}

/// Open one raw client WebSocket against an already-serving address.
///
/// Free rather than a fixture method, because the runtime-authority rows serve
/// from three different owners and only one of them is a
/// [`DirectionTestFixture`]. The handshake and the `101` check are the same for
/// every one of them.
///
/// No bound is armed on the socket here. Every frame reader and writer in the
/// suite arms its own for the length of one operation and restores what it
/// found, so a deadline set once at connect governs no read a direction case
/// makes — it only reads as one the fixture supplies and does not.
pub fn direction_peer(addr: SocketAddr, path: &str) -> TcpStream {
    let mut peer = super::ws::start_upgrade(addr, path);
    let head = super::ws::read_until_double_crlf(&mut peer);
    assert!(
        head.starts_with("HTTP/1.1 101 "),
        "the direction handshake was refused: {head}"
    );
    peer
}

/// The edge that holds the outbound direction with the frame it is about to
/// write in its own hand.
///
/// Named as the bare edge as well as the hold below, because a row that stages
/// this direction's release names the edge alone: the staged release is asked
/// of that owner's own controller, which takes no [`BridgeHold`].
pub const BEFORE_WRITE_EDGE: WebSocketDirectionEdge = WebSocketDirectionEdge::BeforeOutboundWrite;
/// The checkpoint every row that needs a held writer arms.
pub const BEFORE_WRITE: BridgeHold = BridgeHold::Direction(BEFORE_WRITE_EDGE);
/// The checkpoint every row that needs a held converted frame arms, after
/// production built it and before the sink takes it.
pub const FRAME_BUILT: BridgeHold =
    BridgeHold::Direction(WebSocketDirectionEdge::OutboundFrameBuilt);
/// The edge that holds a bridge once its one cause is committed.
pub const AFTER_COMMIT: BridgeHold = BridgeHold::Terminal(WebSocketTerminalEdge::AfterCommit);
/// The edge that holds a bridge with a cause offered and not yet committed.
pub const BEFORE_COMMIT: BridgeHold = BridgeHold::Terminal(WebSocketTerminalEdge::BeforeCommit);
/// The edge that holds the inbound direction once a peer message is queued.
pub const QUEUED: BridgeHold = BridgeHold::Direction(WebSocketDirectionEdge::InboundFrameQueued);

/// The frame [`fill_outbound_behind_the_writer`] leaves the writer holding.
pub const HELD_TEXT: &str = "held-by-the-writer";
/// The frame [`fill_outbound_behind_the_writer`] leaves in the one slot.
pub const FILLING_TEXT: &str = "fills-the-only-slot";

/// Fill a capacity-one outbound queue behind a held writer.
///
/// Two sends, not one: the pump takes the first frame off the queue and parks
/// holding it, which leaves the single slot empty again. The second is what
/// actually fills it, and every row that wants a blocked or refused send needs
/// both.
///
/// The writer is left held at [`BEFORE_WRITE`], because a row that filled the
/// queue is a row about what happens while it is full: the release belongs to
/// the row, and teardown lets go of it if the row does not.
pub async fn fill_outbound_behind_the_writer(fixture: &DirectionTestFixture, sender: &WsSender) {
    fixture.arm(BEFORE_WRITE);
    sender
        .send(HELD_TEXT)
        .await
        .expect("admit the frame the writer holds");
    fixture.wait_paused(BEFORE_WRITE).await;
    sender
        .try_send(FILLING_TEXT)
        .expect("fill the one outbound slot");
}

/// Drop the receive owner, require the close it owes the peer, and require
/// every sender's waiting send to read `ReceiverDropped`.
pub async fn assert_receiver_drop_closes(
    peer: &mut tokio::net::TcpStream,
    receiver: WsReceiver,
    senders: &[&WsSender],
) {
    drop(receiver);
    expect_async_close(peer, "the close a dropped receive owner owes").await;
    for sender in senders {
        assert_eq!(
            closed_cause(
                sender.send("after").await,
                "a send after the receiver dropped"
            ),
            WsCloseCause::ReceiverDropped,
            "a retained sender did not read the receiver's drop"
        );
    }
}

/// A closed connection, as the compatibility facade reports one.
///
/// The facade maps every typed closure back to a broken pipe, so a row reading
/// one through `WsConn` asks for it by that name rather than by the cause the
/// two owners underneath would have given it.
pub fn assert_broken_pipe(outcome: Result<(), RuntimeError>, what: &str) {
    match outcome {
        Err(error) if super::ws::is_peer_departure(&error) => {}
        other => panic!("{what} did not report a broken pipe: {other:?}"),
    }
}

/// The cause a closed public operation reported, or a failure naming what it
/// reported instead.
pub fn closed_cause<T: std::fmt::Debug>(
    outcome: Result<T, RuntimeError>,
    what: &str,
) -> WsCloseCause {
    match outcome {
        Err(RuntimeError::WebSocketClosed(cause)) => cause,
        other => panic!("{what} answered {other:?} rather than a typed WebSocket closure"),
    }
}

/// One typed receive under [`DIRECTION_DEADLINE`], so no row waits on a
/// message or a terminal cause that is never coming.
///
/// `WsReceiver::recv` has no deadline, and a row calls this on its own task: a
/// bridge that stopped delivering would park the row, and the harness behind
/// it, instead of failing it.
pub async fn bounded_receive(receiver: &mut WsReceiver, what: &str) -> WsReceive {
    receiver
        .recv_timeout(DIRECTION_DEADLINE)
        .await
        .unwrap_or_else(|error| panic!("{what} was refused: {error}"))
}

/// One receive, so a row that only wants the answer does not have to restate
/// which half owns the `&mut`.
pub async fn receive_once(mut receiver: WsReceiver) -> Result<WsReceive, RuntimeError> {
    receiver.recv().await
}

/// The message one receive answered with, or a failure naming the answer it
/// gave instead.
///
/// Stated once because every row asks the same two questions of a receive — did
/// it answer with a message, and was it the right one — and a row that
/// re-derived that from the enum would report "the pattern did not match"
/// instead of what actually arrived.
pub fn received_message(received: WsReceive, what: &str) -> WsMessage {
    match received {
        WsReceive::Message(message) => message,
        WsReceive::Closed(cause) => panic!("{what} closed with `{cause}` instead of a message"),
    }
}

/// Require that one receive answered with exactly `expected` as text.
pub fn assert_received_text(received: WsReceive, expected: &str, what: &str) {
    match received_message(received, what) {
        WsMessage::Text(text) => assert_eq!(&*text, expected, "{what}"),
        WsMessage::Binary(data) => panic!("{what} took binary {data:?} instead of {expected:?}"),
    }
}

/// Require that one receive answered with exactly `expected` as binary.
pub fn assert_received_binary(received: WsReceive, expected: &[u8], what: &str) {
    match received_message(received, what) {
        WsMessage::Binary(data) => assert_eq!(data.as_ref(), expected, "{what}"),
        WsMessage::Text(text) => panic!("{what} took text {text:?} instead of {expected:?}"),
    }
}

/// The cause a closed receive settled on, or a failure naming what it took
/// instead.
pub fn closed_receive_cause(received: WsReceive, what: &str) -> WsCloseCause {
    match received {
        WsReceive::Closed(cause) => cause,
        WsReceive::Message(message) => panic!("{what} took {message:?} instead of closing"),
    }
}

/// Require that one receive answered with the connection being over, for
/// exactly `expected`.
pub fn assert_closed_with(received: WsReceive, expected: WsCloseCause, what: &str) {
    assert_eq!(closed_receive_cause(received, what), expected, "{what}");
}
