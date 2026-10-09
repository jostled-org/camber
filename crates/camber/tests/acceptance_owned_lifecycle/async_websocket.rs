//! Daemon-live proof that an async `Router::ws` callback and its transport make
//! progress together on every executor Camber supports, and that each callback
//! keeps the authority of the server that started it.
//!
//! Every journey serves a real route, performs real upgrades, and frames over
//! real sockets. The peers use async I/O throughout: a current-thread runtime
//! has one thread, and a peer blocked on a socket read there would stop the
//! server it is waiting on.

#![cfg(feature = "ws")]

use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use std::time::Duration;

use camber::RuntimeError;
use camber::http::{Request, Router, WsCloseCause, WsConn, WsMessage, WsReceive};
use camber::runtime;

use crate::common::{
    OwnedServer, TEXT, WsExecutor, assert_graceful_close_then_eof, assert_transport_eof,
    close_ws_peer, drain_until_closed, expect_async_close, expect_async_text, lifecycle_event,
    upgraded_ws_peer, write_async_ws_frame,
};
use crate::spawn_probe::{AWAITING_PEER, ChildParts, SpawnProbe, await_peer};

/// The route every journey here upgrades on.
const JOURNEY_PATH: &str = "/journey";

/// What a callback's spawned sender writes while the callback itself waits.
const OUTBOUND: &str = "sent-while-the-callback-waits";

/// The prefix a journey callback answers each peer message with.
const ECHO: &str = "echo:";

/// How long a whole runtime thread has to report back.
///
/// A hang guard, not a timing claim: every wait inside the thread is bounded on
/// its own, and this only stops a runtime that never returns from parking the
/// test binary.
const THREAD_BOUND: Duration = Duration::from_secs(30);

/// What one journey callback reports once it has returned.
type JourneyReport = Result<WsCloseCause, RuntimeError>;

/// A router whose callback answers one message and then waits for the end.
fn journey_router() -> (Router, tokio::sync::mpsc::Receiver<JourneyReport>) {
    let (reports_tx, reports) = tokio::sync::mpsc::channel(4);
    let mut router = Router::new();
    router.ws(
        JOURNEY_PATH,
        move |_request: &Request, connection: WsConn| {
            let reports = reports_tx.clone();
            async move {
                let report = journey_callback(connection).await;
                reports
                    .send(report)
                    .await
                    .map_err(|_| RuntimeError::ChannelClosed)
            }
        },
    );
    (router, reports)
}

/// One journey callback: an independent sender, one suspended receive and its
/// answer, then a second receive that waits for the connection to end.
///
/// The spawned sender is application work the callback owns, so the callback
/// joins it before it returns. What it reports is the cause its last receive
/// was closed with.
async fn journey_callback(connection: WsConn) -> JourneyReport {
    let (sender, mut receiver) = connection.split();
    let independent = sender.clone();
    let outbound = tokio::spawn(async move { independent.send(OUTBOUND).await });
    let message = match receiver.recv().await? {
        WsReceive::Message(WsMessage::Text(text)) => text,
        other => {
            return Err(RuntimeError::InvalidArgument(
                format!("the journey peer sent {other:?}").into(),
            ));
        }
    };
    sender.send(&format!("{ECHO}{message}")).await?;
    outbound
        .await
        .map_err(|error| RuntimeError::TaskPanicked(error.to_string().into()))??;
    match receiver.recv().await? {
        WsReceive::Closed(cause) => Ok(cause),
        WsReceive::Message(message) => Err(RuntimeError::InvalidArgument(
            format!("the journey peer sent {message:?} after its answer").into(),
        )),
    }
}

/// Close from the peer's side, take the bridge's echoed close, and require the
/// transport to end after it.
async fn close_and_take_echo(peer: &mut tokio::net::TcpStream, context: &str) {
    close_ws_peer(peer, context).await;
    expect_async_close(peer, context).await;
    assert_transport_eof(peer, context).await;
}

/// Send one message and require the callback's answer to it.
async fn exchange(peer: &mut tokio::net::TcpStream, message: &str, context: &str) {
    write_async_ws_frame(peer, TEXT, message.as_bytes(), context).await;
    expect_async_text(peer, &format!("{ECHO}{message}"), context).await;
}

/// The one journey both executors run.
///
/// The first callback is left suspended in its receive while a second
/// connection completes its whole exchange, so neither a callback that waits
/// nor the bridge polling it can hold up anything beside it. Each peer reads
/// its spawned sender's frame before it sends anything, and that frame can only
/// have been written while its callback was still waiting for the peer.
async fn progress_journey(executor: WsExecutor) {
    let context = format!("the {executor:?} journey");
    let (router, mut reports) = journey_router();
    let server = OwnedServer::bind(router, &context).await;
    let mut first = upgraded_ws_peer(server.addr(), JOURNEY_PATH, &context).await;
    expect_async_text(&mut first, OUTBOUND, &context).await;
    let mut second = upgraded_ws_peer(server.addr(), JOURNEY_PATH, &context).await;
    expect_async_text(&mut second, OUTBOUND, &context).await;
    exchange(&mut second, "second", &context).await;
    exchange(&mut first, "first", &context).await;

    server.handle().shutdown();
    assert_graceful_close_then_eof(&mut first, &context).await;
    assert_graceful_close_then_eof(&mut second, &context).await;
    for _ in 0..2 {
        let report = lifecycle_event(&context, reports.recv())
            .await
            .unwrap_or_else(|| panic!("{context}: a callback returned without reporting"));
        assert!(
            matches!(report, Ok(WsCloseCause::ServerShutdown)),
            "{context}: a callback ended with {report:?}"
        );
    }
    server.stop_cleanly(&context).await;
}

// async-first-websockets 2.T2 — Invariant 3: callback and transport progress on
// a current-thread runtime and on a one-worker runtime, through real upgrades,
// duplex exchange, a second connection, and a bounded graceful shutdown.
#[test]
fn async_callback_and_transport_progress_on_supported_executors() {
    for executor in WsExecutor::ALL {
        executor.run(|| progress_journey(executor));
    }
}

// async-first-websockets 2.T2 — Invariant 2: `Router::ws` accepts a `Send`
// future that is neither `Sync` nor `Unpin`.
//
// Compile-only. The future keeps owned request metadata and a `Cell` — `Send`
// but not `Sync` — across a Tokio sleep and ordinary endpoint awaits, and a
// `PhantomPinned` across them, which makes it not `Unpin`. Registering it is the
// whole claim: no socket is opened and nothing is polled.
#[test]
fn async_websocket_callback_accepts_send_non_sync_future() {
    /// A capture that may not move once pinned, and is not `Copy`, so holding
    /// it across an await is what holds the future in place.
    struct Unmovable(std::marker::PhantomPinned);

    let mut router = Router::new();
    router.ws(JOURNEY_PATH, |request: &Request, mut connection: WsConn| {
        let path: Box<str> = request.path().into();
        async move {
            let answered = std::cell::Cell::new(0_usize);
            let pinned = Unmovable(std::marker::PhantomPinned);
            tokio::time::sleep(Duration::ZERO).await;
            while let Some(message) = connection.recv().await {
                connection.send(&message).await?;
                answered.set(answered.get() + 1);
            }
            connection.send(&path).await?;
            answered.set(answered.get() + 1);
            drop(pinned);
            Ok(())
        }
    });
    drop(router);
}

/// One serving runtime's tag, and the child its callback was resumed into.
///
/// The tag is what the peer resumes the callback with, so the child's answer
/// is something only that callback read after its suspension.
const TAGS: [&str; 2] = ["alpha", "beta"];

/// One Camber runtime serving one authority route on a thread of its own.
///
/// The runtime owns its root children and the owned server inside it owns its
/// callbacks, and this struct reaches neither directly: it starts the runtime,
/// tells it when to stop serving, and reads what its runtime finally joined.
///
/// No `Drop`: a failed row lets go by dropping fields, and nothing here joins on
/// drop. Fields drop in declaration order, so `reports` goes first and its probe
/// releases a held child before `stop` lets the runtime stop serving; `thread`
/// only detaches.
struct AuthorityRuntime {
    addr: SocketAddr,
    reports: AuthorityReports,
    stop: tokio::sync::oneshot::Sender<()>,
    thread: std::thread::JoinHandle<()>,
}

/// What one authority runtime reports back to its row, and the child it holds.
struct AuthorityReports {
    tag: &'static str,
    child: SpawnProbe,
    served: Receiver<()>,
    joined: Receiver<Result<Box<str>, RuntimeError>>,
}

impl AuthorityRuntime {
    fn start(tag: &'static str) -> Self {
        let (parts, child) = ChildParts::new();
        let (addr_tx, addr_rx) = std::sync::mpsc::channel();
        let (stop, stop_rx) = tokio::sync::oneshot::channel();
        let (served_tx, served) = std::sync::mpsc::channel();
        let (joined_tx, joined) = std::sync::mpsc::channel();
        let serving = Serving {
            addr: addr_tx,
            stop: stop_rx,
            served: served_tx,
        };
        let thread = std::thread::spawn(move || {
            // The runtime returns only once its root children have; the join
            // below then reads the answer its callback's child left behind.
            let outcome = runtime::builder()
                .worker_threads(1)
                .shutdown_timeout(THREAD_BOUND)
                .run(move || runtime::block_on(serve_authority(parts, serving)))
                .and_then(camber::JoinHandle::join);
            let _ = joined_tx.send(outcome);
        });
        let addr = addr_rx
            .recv_timeout(THREAD_BOUND)
            .unwrap_or_else(|error| panic!("the {tag} runtime never served: {error}"));
        Self {
            addr,
            reports: AuthorityReports {
                tag,
                child,
                served,
                joined,
            },
            stop,
            thread,
        }
    }

    /// Require this runtime's callback child to be running.
    fn assert_child_entered(&self) {
        assert!(
            self.reports.child.entered(),
            "the {} callback's child never ran",
            self.reports.tag
        );
    }

    /// Stop serving, release the child, and require what the runtime joined.
    ///
    /// The server stops first, so the runtime is left waiting on nothing but
    /// its root child; that the runtime has not returned until the child is
    /// released says the child is this runtime's, and the answer it joins says
    /// which callback spawned it.
    fn stop_and_join(self) {
        let Self {
            reports,
            stop,
            thread,
            ..
        } = self;
        let tag = reports.tag;
        stop.send(())
            .unwrap_or_else(|()| panic!("the {tag} runtime stopped on its own"));
        let joined = match reports.served_then_joined() {
            Some(joined) => joined,
            None => reports.resume_thread_unwind(thread),
        };
        assert!(
            matches!(&joined, Ok(answer) if **answer == *tag),
            "the {tag} runtime joined {joined:?} from its callback's child"
        );
        match thread.join() {
            Err(unwound) => std::panic::resume_unwind(unwound),
            Ok(()) => {}
        }
    }
}

impl AuthorityReports {
    /// Wait for the server to be joined, then release the child and take what
    /// the runtime joined.
    ///
    /// `None` is a runtime thread that went away without answering either
    /// channel. It unwound, so the caller that owns the thread fails the row on
    /// its panic.
    fn served_then_joined(&self) -> Option<Result<Box<str>, RuntimeError>> {
        match self.served.recv_timeout(THREAD_BOUND) {
            Ok(()) => {}
            Err(RecvTimeoutError::Timeout) => {
                panic!(
                    "the {} server never joined within {THREAD_BOUND:?}",
                    self.tag
                )
            }
            Err(RecvTimeoutError::Disconnected) => return None,
        }
        match self.joined.try_recv() {
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => return None,
            Ok(early) => panic!(
                "the {} runtime returned {early:?} while its callback's child was still held",
                self.tag
            ),
        }
        self.child.release_and_finish();
        match self.joined.recv_timeout(THREAD_BOUND) {
            Ok(joined) => Some(joined),
            Err(RecvTimeoutError::Timeout) => {
                panic!(
                    "the {} runtime never returned within {THREAD_BOUND:?}",
                    self.tag
                )
            }
            Err(RecvTimeoutError::Disconnected) => None,
        }
    }

    /// Join a runtime thread that hung up without answering, and fail the row
    /// on what it unwound with.
    ///
    /// The child is released before the join: a runtime still counting a held
    /// child would not return, and the join would wait on it.
    fn resume_thread_unwind(&self, thread: std::thread::JoinHandle<()>) -> ! {
        self.child.release();
        match thread.join() {
            Err(unwound) => std::panic::resume_unwind(unwound),
            Ok(()) => panic!("the {} runtime thread ended without answering", self.tag),
        }
    }
}

/// What a serving runtime tells its row, and what the row tells it.
struct Serving {
    addr: Sender<SocketAddr>,
    stop: tokio::sync::oneshot::Receiver<()>,
    /// Said once the server is joined, while the child may still be held.
    served: Sender<()>,
}

/// Serve one authority route inside the calling Camber runtime until told to
/// stop, and hand back the child its callback spawned.
///
/// The server is shut down and joined here, through its own owner, before the
/// child goes back to the runtime that admitted it.
async fn serve_authority(parts: ChildParts, serving: Serving) -> camber::JoinHandle<Box<str>> {
    let (spawned_tx, mut spawned) = tokio::sync::mpsc::channel(1);
    let mut router = Router::new();
    router.ws(
        JOURNEY_PATH,
        move |_request: &Request, mut connection: WsConn| {
            let spawned = spawned_tx.clone();
            let parts = parts.clone();
            async move {
                let child = resumed_spawn(&mut connection, parts).await?;
                spawned
                    .send(child)
                    .await
                    .map_err(|_| RuntimeError::ChannelClosed)?;
                drain_until_closed(&mut connection).await;
                Ok(())
            }
        },
    );
    let server = OwnedServer::bind(router, "the authority runtime").await;
    serving
        .addr
        .send(server.addr())
        .expect("the row stopped waiting for the authority address");
    let child = lifecycle_event("the resumed callback's spawn", spawned.recv())
        .await
        .expect("the resumed callback never spawned its child");
    lifecycle_event("the row's stop request", serving.stop)
        .await
        .expect("the row went away without stopping its runtime");
    lifecycle_event("the authority server to join", server.stop())
        .await
        .expect("the authority server joined cleanly");
    serving
        .served
        .send(())
        .expect("the row stopped waiting for the server to join");
    child
}

/// Acknowledge, suspend until the peer answers, and spawn a child that returns
/// what the peer sent.
async fn resumed_spawn(
    connection: &mut WsConn,
    parts: ChildParts,
) -> Result<camber::JoinHandle<Box<str>>, RuntimeError> {
    let tag = await_peer(connection).await?;
    Ok(camber::spawn(parts.body(tag)))
}

// async-first-websockets 2.T7 — Invariant 9: two Camber runtimes serve at once,
// and each callback, resumed after a real suspension, spawns into the runtime
// whose server started it.
//
// Both callbacks are suspended at the same time before either is resumed. The
// second runtime is stopped and joined while the first one's child is still
// held, so a child admitted to the wrong runtime would hold that runtime's
// completion and fail its bounded join.
#[test]
fn concurrent_async_callbacks_keep_their_serving_authority() {
    let runtimes = TAGS.map(AuthorityRuntime::start);
    let peers = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build the peers' executor");
    let addrs = runtimes.each_ref().map(|served| served.addr);
    let mut connected = peers.block_on(async {
        let mut connected = Vec::new();
        for addr in addrs {
            let mut peer = upgraded_ws_peer(addr, JOURNEY_PATH, "an authority peer").await;
            expect_async_text(&mut peer, AWAITING_PEER, "an authority peer").await;
            connected.push(peer);
        }
        for (peer, tag) in connected.iter_mut().zip(TAGS) {
            write_async_ws_frame(peer, TEXT, tag.as_bytes(), "an authority peer").await;
        }
        connected
    });
    runtimes
        .iter()
        .for_each(AuthorityRuntime::assert_child_entered);
    peers.block_on(async {
        for peer in connected.iter_mut() {
            close_and_take_echo(peer, "an authority peer").await;
        }
    });
    let [alpha, beta] = runtimes;
    beta.stop_and_join();
    alpha.stop_and_join();
}
