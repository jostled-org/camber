//! One owner for everything a direct-WebSocket direction case binds, starts,
//! connects, arms, or spawns.
//!
//! Direction cases hold a live listener, an owned server, a raw client socket,
//! application threads parked in a blocking endpoint operation, and armed
//! production checkpoints at once. A case that asserts its way out through an
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
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, lifecycle};
use camber::http::{
    Request, Router, ServerHandle, ServerHandleFuture, WsCloseCause, WsConn, WsMessage, WsReceive,
    WsReceiver, WsSender,
};
use futures_util::FutureExt;

use super::http::{assert_server_joined, reserve_observed, wait_until_paused_bounded};
use super::ws_async::lifecycle_event;

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
    /// exists, the callback's own wait cannot end; dropping this handoff is
    /// what lets every parked callback return.
    #[expect(dead_code, reason = "held for its Drop, which unparks the callback")]
    parked: std::sync::mpsc::Sender<()>,
}

impl DirectionHandoff {
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

/// A router whose direct callback hands its connection out and parks.
///
/// Parking rather than returning, because every row here owns the halves
/// through the callback's own lifetime: a callback that returned would drop
/// whichever half the row had not taken, and end the connection under it.
pub fn direction_router(path: &str, buffer: usize) -> (Router, DirectionHandoff) {
    let (connections_tx, connections) = tokio::sync::mpsc::channel(4);
    let (parked, parked_rx) = std::sync::mpsc::channel();
    let parked_rx = Mutex::new(parked_rx);
    let mut router = Router::new();
    router.ws(path, move |_request: &Request, connection: WsConn| {
        connections_tx
            .blocking_send(connection)
            .map_err(|_| RuntimeError::ChannelClosed)?;
        park_until_released(&parked_rx);
        Ok(())
    });
    (
        router.ws_buffer_size(buffer),
        DirectionHandoff {
            connections,
            parked,
        },
    )
}

/// Hold a callback in its own frame until the case lets it go.
///
/// The release is the sender dropping, never a value: a case that had to
/// remember to send one would leave the callback parked on every failure path,
/// and a parked callback holds the connection its row is finished with. The
/// lock is recovered rather than refused for the same reason — a poisoned gate
/// would park the callback forever.
pub fn park_until_released(gate: &Mutex<Receiver<()>>) {
    let _ = gate
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .recv();
}

/// The route every direction row registers.
pub const DIRECTION_PATH: &str = "/ws";

/// The second route a returning row serves: a callback that never comes back.
pub const PARKED_PATH: &str = "/parked";

/// The one send the parked callback makes, once its row lets it go.
const PARKED_SEND: &str = "after-the-owner-completed";

/// What the parked callback reports, in the order it reports it.
///
/// One channel rather than two: these are one callback's own two steps, and a
/// second channel would let a row take them out of the order the callback took
/// them in.
#[derive(Debug)]
pub enum ParkedStep {
    /// The callback is in the blocking pool, holding its connection.
    Entered,
    /// What the connection it held across its owner's completion answered when
    /// it was finally used.
    Sent(Result<(), RuntimeError>),
}

/// Hand one step to the row, or report the row as gone.
fn report(
    steps: &tokio::sync::mpsc::Sender<ParkedStep>,
    step: ParkedStep,
) -> Result<(), RuntimeError> {
    steps
        .blocking_send(step)
        .map_err(|_| RuntimeError::ChannelClosed)
}

/// A router whose direct callback splits its connection, hands both halves out,
/// and returns — beside a second one that keeps its connection and parks.
///
/// The returning callback is the opposite of [`direction_router`] in the one way
/// that matters: it exits while the application still owns both halves, which is
/// the only way to ask whether a callback return ends a connection. The parked
/// route is what the same row asks the other half of that question with — an
/// owned server completes without claiming a callback still sitting in the
/// blocking pool has exited — and it has to be served by the same server, since
/// that server's completion is the claim.
///
/// The parked callback also makes one send after its row releases it. A
/// connection retained across its owner's completion is the second half of what
/// that completion did not claim: the callback is still in its frame, and the
/// connection it is holding is over. Only the callback can ask that, because
/// only the callback holds it.
pub fn returning_direction_router(path: &str, buffer: usize) -> (Router, ReturningHandoff) {
    let (halves_tx, halves) = tokio::sync::mpsc::channel(4);
    let (returned_tx, returned) = tokio::sync::mpsc::channel(4);
    let (steps_tx, parked_steps) = tokio::sync::mpsc::channel(4);
    let parked_frame = CallbackFrame::new();
    let (parked, parked_rx) = std::sync::mpsc::channel();
    let parked_rx = Mutex::new(parked_rx);
    let entered_frame = parked_frame.clone();
    let mut router = Router::new();
    router.ws(path, move |_request: &Request, connection: WsConn| {
        halves_tx
            .blocking_send(connection.split())
            .map_err(|_| RuntimeError::ChannelClosed)?;
        returned_tx
            .blocking_send(())
            .map_err(|_| RuntimeError::ChannelClosed)?;
        Ok(())
    });
    router.ws(
        PARKED_PATH,
        move |_request: &Request, connection: WsConn| {
            let _exit = entered_frame.enter();
            report(&steps_tx, ParkedStep::Entered)?;
            park_until_released(&parked_rx);
            report(&steps_tx, ParkedStep::Sent(connection.send(PARKED_SEND)))?;
            drop(connection);
            Ok(())
        },
    );
    (
        router.ws_buffer_size(buffer),
        ReturningHandoff {
            halves,
            returned,
            parked_steps,
            parked_frame,
            parked: Some(parked),
        },
    )
}

/// One blocking callback's frame, seen from outside it.
///
/// The case holds one of these to ask whether that frame is still there. The
/// callback holds the [`CallbackExit`] it hands out, which answers by dropping:
/// a flag set on drop rather than before the callback's last statement, because
/// the claim is about the frame itself, and only leaving it — by returning or
/// by unwinding — can say otherwise.
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
    parked_steps: tokio::sync::mpsc::Receiver<ParkedStep>,
    parked_frame: CallbackFrame,
    /// The other end of the channel the parked callback waits on.
    ///
    /// Never sent on: it is held for its `Drop`, which is what lets that
    /// callback leave once the case is done asking whether it has. In an
    /// `Option` because a row that wants the callback's last act needs to
    /// perform that drop itself, at the moment it names.
    parked: Option<std::sync::mpsc::Sender<()>>,
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

    /// Wait for the parked callback to report that it is in the blocking pool.
    pub async fn wait_parked(&mut self) {
        match self.parked_step().await {
            ParkedStep::Entered => {}
            other => panic!("the parked direct callback reported {other:?} before it entered"),
        }
    }

    /// Let the parked callback leave its frame.
    ///
    /// Explicit, so the release is an event of the row's own rather than
    /// whenever this handoff happens to be dropped: what the callback does next
    /// is only worth reading after the row has finished with its server.
    pub fn release_parked(&mut self) {
        drop(self.parked.take());
    }

    /// What the released callback's retained connection answered.
    pub async fn parked_send(&mut self) -> Result<(), RuntimeError> {
        match self.parked_step().await {
            ParkedStep::Sent(outcome) => outcome,
            other => panic!("the parked direct callback reported {other:?} instead of its send"),
        }
    }

    /// The next step that callback reported, under the suite's own bound.
    async fn parked_step(&mut self) -> ParkedStep {
        lifecycle_event(
            "the parked direct callback to report a step",
            self.parked_steps.recv(),
        )
        .await
        .expect("the parked direct callback reported nothing")
    }

    /// Whether the parked callback's frame has been left.
    pub fn parked_exited(&self) -> bool {
        self.parked_frame.left()
    }
}

/// Serve one capacity-`buffer` route whose callback returns, connect a peer, and
/// run `case` against the halves it left behind.
pub async fn returning_direction_row<C, Fut>(buffer: usize, case: C)
where
    C: FnOnce(Arc<DirectionTestFixture>, TcpStream, ReturningHandoff) -> Fut,
    Fut: Future<Output = ()>,
{
    let (router, handoff) = returning_direction_router(DIRECTION_PATH, buffer);
    DirectionTestFixture::run(
        |listener| camber::http::serve_background(listener, router),
        |fixture| async move {
            let peer = fixture.connect(DIRECTION_PATH);
            case(fixture, peer, handoff).await;
        },
    )
    .await;
}

/// The same row as [`direction_row`], over a peer whose close is a TCP reset.
///
/// A separate row rather than a flag, because the peer type differs: only a
/// Tokio socket can be given the zero linger that turns its close into the
/// transport failure a `PeerDisconnected` row needs.
pub async fn abortive_direction_row<C, Fut>(buffer: usize, case: C)
where
    C: FnOnce(Arc<DirectionTestFixture>, tokio::net::TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    let (router, mut handoff) = direction_router(DIRECTION_PATH, buffer);
    DirectionTestFixture::run(
        |listener| camber::http::serve_background(listener, router),
        |fixture| async move {
            let peer = fixture.connect_abortive(DIRECTION_PATH).await;
            let connection = handoff.connection().await;
            case(fixture, peer, connection).await;
            drop(handoff);
        },
    )
    .await;
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
/// teardown can drift from the others.
pub async fn direction_row<C, Fut>(buffer: usize, case: C)
where
    C: FnOnce(Arc<DirectionTestFixture>, TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    let (router, handoff) = direction_router(DIRECTION_PATH, buffer);
    run_row(
        |listener| camber::http::serve_background(listener, router),
        handoff,
        &[],
        case,
    )
    .await;
}

/// The same row, with `armed` checkpoints already in place when the peer
/// connects.
///
/// The bridge passes some of its checkpoints on the way to handing the callback
/// its connection, and a case that already holds one is too late to arm those.
pub async fn staged_direction_row<C, Fut>(
    buffer: usize,
    armed: &'static [LifecycleCheckpoint],
    case: C,
) where
    C: FnOnce(Arc<DirectionTestFixture>, TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    let (router, handoff) = direction_router(DIRECTION_PATH, buffer);
    run_row(
        |listener| camber::http::serve_background(listener, router),
        handoff,
        armed,
        case,
    )
    .await;
}

/// The same row, with the capacity configured on a `HostRouter` instead.
///
/// A separate public construction path, not a second spelling of the same one:
/// the child router below configures its own capacity and that value must not
/// reach either queue.
pub async fn host_direction_row<C, Fut>(buffer: usize, case: C)
where
    C: FnOnce(Arc<DirectionTestFixture>, TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    let (router, handoff) = direction_router(DIRECTION_PATH, IGNORED_CHILD_BUFFER);
    let mut hosts = camber::http::HostRouter::new();
    hosts.set_default(router);
    let hosts = hosts.ws_buffer_size(buffer);
    run_row(
        |listener| camber::http::serve_background_hosts(listener, hosts),
        handoff,
        &[],
        case,
    )
    .await;
}

/// Bind, serve, connect, take the callback's connection, run the case.
///
/// The handoff stays owned here for the whole case, so the production callback
/// stays parked in its own frame rather than returning under the row.
async fn run_row<S, C, Fut>(
    serve: S,
    mut handoff: DirectionHandoff,
    armed: &'static [LifecycleCheckpoint],
    case: C,
) where
    S: FnOnce(tokio::net::TcpListener) -> ServerHandle,
    C: FnOnce(Arc<DirectionTestFixture>, TcpStream, WsConn) -> Fut,
    Fut: Future<Output = ()>,
{
    DirectionTestFixture::run(serve, |fixture| async move {
        armed.iter().for_each(|checkpoint| fixture.arm(*checkpoint));
        let peer = fixture.connect(DIRECTION_PATH);
        let connection = handoff.connection().await;
        case(fixture, peer, connection).await;
        drop(handoff);
    })
    .await;
}

/// What one application thread the case started reports back.
///
/// The thread itself belongs to the fixture, which joins it; this is only the
/// bounded way to read its answer.
pub struct WorkerResult<T> {
    label: Box<str>,
    outcome: Receiver<T>,
}

impl<T> WorkerResult<T> {
    /// The worker's answer, or a failure naming the worker that never gave one.
    pub fn take(&self) -> T {
        match self.outcome.recv_timeout(DIRECTION_DEADLINE) {
            Ok(outcome) => outcome,
            Err(error) => panic!("{}: {error}", self.label),
        }
    }

    /// Whether the worker has answered yet, without waiting for it to.
    pub fn settled(&self) -> bool {
        !matches!(
            self.outcome.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Empty)
        )
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
    fn take(mut connection: WsConn) -> Self {
        let text = connection.recv();
        let binary = connection
            .recv_binary()
            .map(|data| Box::<[u8]>::from(data.as_ref()));
        let either = connection.recv_message();
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
    controller: Mutex<Option<Arc<LifecycleController>>>,
    server: Mutex<Option<ServerHandle>>,
    armed: Mutex<Vec<LifecycleCheckpoint>>,
    workers: Mutex<Vec<(Box<str>, std::thread::JoinHandle<()>)>>,
}

impl DirectionTestFixture {
    /// Reserve an observed listener, serve it, and run `case` against the
    /// result.
    ///
    /// The reservation is the suite's own: [`reserve_observed`] binds the
    /// ephemeral port and registers its observer before anything serves on it,
    /// which is the ordering every listener-scoped observation here depends on.
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
        let (listener, addr, controller) = reserve_observed().into_owned_parts();
        let fixture = Arc::new(Self {
            addr,
            controller: Mutex::new(Some(controller)),
            server: Mutex::new(Some(serve(listener))),
            armed: Mutex::new(Vec::new()),
            workers: Mutex::new(Vec::new()),
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
    /// closes. [`Self::observed`] and [`Self::checkpoint_polls`] are what a row
    /// reads through instead.
    pub fn controller(&self) -> Arc<LifecycleController> {
        self.controller
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(Arc::clone)
            .expect("the direction fixture already gave back its lifecycle observer")
    }

    /// Open one raw client WebSocket against this fixture's server.
    ///
    /// The socket is handed to the case outright. A socket needs no protocol to
    /// be released — closing its file descriptor is what a dropped `TcpStream`
    /// already does — so registering it here would buy nothing and cost the
    /// case the `&mut` its frame reads need.
    pub fn connect(&self, path: &str) -> TcpStream {
        direction_peer(self.addr, path)
    }

    /// Open one client WebSocket whose close will be a TCP reset.
    ///
    /// Tokio-side, because the zero linger that produces the reset is only
    /// reachable through a Tokio socket. A row takes this peer exactly when the
    /// transport failure, rather than an orderly close, is what it is about.
    pub async fn connect_abortive(&self, path: &str) -> tokio::net::TcpStream {
        use tokio::io::AsyncWriteExt;
        let mut peer = lifecycle_event(
            "the abortive direction peer to connect",
            super::http::abortive_tcp_socket().connect(self.addr),
        )
        .await
        .expect("connect abortive direction peer");
        // Bounded like the connect either side of it. A handshake is small
        // enough that a stalled write is unlikely, and an unlikely stall in an
        // unbounded write is a hung binary rather than a failed row.
        lifecycle_event(
            "the abortive direction handshake to be written",
            peer.write_all(super::ws::ws_upgrade_request(path).as_bytes()),
        )
        .await
        .expect("write abortive direction handshake");
        let head = super::ws_async::read_async_http_head(&mut peer, "the abortive handshake").await;
        assert!(
            head.starts_with("HTTP/1.1 101 "),
            "the abortive direction handshake was refused: {head}"
        );
        peer
    }

    /// Arm one production checkpoint, recording it for release at teardown.
    pub fn arm(&self, checkpoint: LifecycleCheckpoint) {
        self.controller()
            .pause_once(checkpoint)
            .expect("arm direction checkpoint");
        self.armed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(checkpoint);
    }

    /// Wait until production reaches an armed checkpoint.
    pub async fn wait_paused(&self, checkpoint: LifecycleCheckpoint) {
        wait_until_paused_bounded(
            &self.controller(),
            checkpoint,
            "production reaching its direction checkpoint",
        )
        .await;
    }

    /// Release one armed checkpoint and stop owning it.
    pub fn release(&self, checkpoint: LifecycleCheckpoint) {
        self.controller()
            .release(checkpoint)
            .expect("release direction checkpoint");
        self.forget(checkpoint);
    }

    /// Record one armed checkpoint's release without waking what waits there.
    ///
    /// The held future stays parked and observes the release on whatever poll
    /// something else provokes. It is the only way to put a production event
    /// that is already in hand into the same turn as one the case triggers
    /// afterwards: an ordinary release decides that turn by itself.
    pub fn stage_release(&self, checkpoint: LifecycleCheckpoint) {
        self.controller()
            .stage_release(checkpoint)
            .expect("stage direction checkpoint release");
        self.forget(checkpoint);
    }

    /// Take one message through each untimed facade receiver, on a worker.
    ///
    /// Run off the case's own task because all three loop on the receive half's
    /// unbounded blocking call: a facade that stopped answering would park the
    /// case — and the whole harness behind it — where the worker's own bounded
    /// result fails it instead. The connection comes back with the answers, so
    /// the rest of the row still owns it.
    pub fn spawn_facade_receives(&self, connection: WsConn) -> WorkerResult<FacadeReceives> {
        self.spawn_worker("facade-receives", move || FacadeReceives::take(connection))
    }

    fn forget(&self, checkpoint: LifecycleCheckpoint) {
        self.armed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .retain(|armed| *armed != checkpoint);
    }

    /// What this fixture's listener has published about its direct bridges.
    pub fn observed(&self) -> camber::http::mock::WebSocketDirectionObservation {
        self.controller().websocket_directions()
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

    /// Ask this fixture's still-held server to do one thing.
    ///
    /// A server the case has already finished with is a case asking for
    /// something that cannot happen, not a no-op: the row would go on to assert
    /// against a stop it never requested.
    fn with_server(&self, ask: impl FnOnce(&ServerHandle), what: &str) {
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

    /// Start one application thread and register it for a bounded join.
    ///
    /// A plain thread rather than an admitted task: these run public blocking
    /// endpoint operations, and the claim under test is what those operations do
    /// off every runtime as well as on one.
    pub fn spawn_worker<T, F>(&self, label: &str, work: F) -> WorkerResult<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (reported, outcome) = std::sync::mpsc::channel();
        let thread = std::thread::spawn(move || {
            let _ = reported.send(work());
        });
        self.workers
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push((label.into(), thread));
        WorkerResult {
            label: format!("worker `{label}` never reported").into(),
            outcome,
        }
    }

    /// Start one application thread that says when it is about to make its
    /// endpoint call, and wait for it to say so.
    ///
    /// [`Self::spawn_worker`] returns as soon as the thread is spawned, so a row
    /// that probes [`WorkerResult::settled`] straight after it is asking whether
    /// a thread that may not have been scheduled yet has finished — which is
    /// true of a thread that never started, and stays true if the operation
    /// under test stops blocking altogether. The report is the barrier that
    /// makes the probe a claim about the operation instead: it is sent from the
    /// worker's own frame with nothing between it and the call.
    ///
    /// Not a sleep, and not a substitute for one: it establishes that the worker
    /// reached its call, which is exactly what an unsettled result is supposed to
    /// mean.
    pub fn spawn_entered_worker<T, F>(&self, label: &str, work: F) -> WorkerResult<T>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let (entered, entry) = std::sync::mpsc::channel();
        let started = self.spawn_worker(label, move || {
            let _ = entered.send(());
            work()
        });
        entry.recv_timeout(DIRECTION_DEADLINE).unwrap_or_else(|_| {
            panic!("worker `{label}` never reached the endpoint operation it was started for")
        });
        started
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

    /// Let go of every checkpoint the case armed and did not spend.
    ///
    /// A checkpoint that was armed but never reached is not paused, and one the
    /// case already released is spent; both refuse the release and both are
    /// fine. What must not happen is a reached-and-held checkpoint surviving
    /// teardown, and that is the case this covers.
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
            Some(controller) => armed.into_iter().for_each(|checkpoint| {
                let _ = controller.release(checkpoint);
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

    /// Join every application thread the case started, under one bound each.
    ///
    /// The join itself is blocking and unbounded, so it runs on a blocking
    /// thread and the bound is applied to that. A worker still parked in an
    /// endpoint operation after the server has joined is the leak these cases
    /// exist to catch, so it fails the case rather than being waited on.
    ///
    /// What the join answered is read rather than only whether it answered. A
    /// worker whose closure panicked joins promptly and carries its panic back
    /// in that answer, so a bound that only checks the timer reports a case's
    /// own failed assertion as a healthy owner — and any row spawning a worker
    /// it never takes loses its assertion entirely. The payload is resumed here
    /// so the row fails on what the worker actually claimed.
    async fn join_workers(&self) {
        let workers = std::mem::take(
            &mut *self
                .workers
                .lock()
                .unwrap_or_else(|error| error.into_inner()),
        );
        for (label, thread) in workers {
            let joined = tokio::task::spawn_blocking(move || thread.join());
            match tokio::time::timeout(DIRECTION_DEADLINE, joined).await {
                Ok(Ok(Ok(()))) => {}
                Ok(Ok(Err(unwound))) => std::panic::resume_unwind(unwound),
                Ok(Err(join_error)) => panic!("the join of worker `{label}` failed: {join_error}"),
                Err(_) => panic!(
                    "worker `{label}` was still running {DIRECTION_DEADLINE:?} after the server joined"
                ),
            }
        }
    }

    /// Prove the listener's address is bindable and observable again.
    ///
    /// The permit, the transport, and the listener are all released by the same
    /// completion, and the bind is the one of the three an out-of-process
    /// observer can check without asking Camber.
    ///
    /// The registration is the half only Camber can answer. The registry is
    /// keyed by address and refuses a second controller for one, so a fixture
    /// that published this port as free while still holding its entry would fail
    /// an unrelated case that drew the port next. Taking a controller here is
    /// what makes [`Self::release_observer`] falsifiable: the failure lands on
    /// the fixture that leaked the entry rather than on whoever inherited it.
    async fn prove_address_reuse(addr: SocketAddr) {
        assert!(
            tokio::net::TcpListener::bind(addr).await.is_ok(),
            "the direction listener's address {addr} was still held after completion"
        );
        drop(lifecycle(addr).unwrap_or_else(|error| {
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
    /// binary hangs on a bridge nothing will ever release. Nothing here waits:
    /// this runs on whatever thread the unwind is on, and a bounded wait needs
    /// an executor.
    fn drop(&mut self) {
        self.release_every_gate();
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
/// all four callers.
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

/// The checkpoint every row that needs a held writer arms.
pub const BEFORE_WRITE: LifecycleCheckpoint = LifecycleCheckpoint::WebSocketBeforeOutboundWrite;

/// The WebSocket close opcode, as it appears on the wire.
pub const CLOSE: u8 = 0x08;

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
        .send("held-by-the-writer")
        .expect("admit the frame the writer holds");
    fixture.wait_paused(BEFORE_WRITE).await;
    sender
        .send("fills-the-only-slot")
        .expect("fill the one outbound slot");
}

/// A closed connection, as the compatibility facade reports one.
///
/// The facade maps every typed closure back to a broken pipe, so a row reading
/// one through `WsConn` asks for it by that name rather than by the cause the
/// two owners underneath would have given it.
pub fn assert_broken_pipe(outcome: Result<(), RuntimeError>, what: &str) {
    match outcome {
        Err(RuntimeError::Io(error)) if error.kind() == std::io::ErrorKind::BrokenPipe => {}
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

/// One blocking receive, so a row that only wants the answer does not have to
/// restate which half owns the `&mut`.
pub fn receive_once(mut receiver: WsReceiver) -> Result<WsReceive, RuntimeError> {
    receiver.recv()
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

/// Require that one receive answered with the connection being over, for
/// exactly `expected`.
pub fn assert_closed_with(received: WsReceive, expected: WsCloseCause, what: &str) {
    match received {
        WsReceive::Closed(cause) => assert_eq!(cause, expected, "{what}"),
        WsReceive::Message(message) => panic!("{what} took {message:?} instead of closing"),
    }
}
