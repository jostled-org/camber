//! The three server harnesses the disconnect journeys serve from: the
//! runtime-owned one, the scripted one whose production checkpoints a case
//! drives, and the blocking synchronous one.
//!
//! The synchronous harness is not a duplicate of the owned one. It is the only
//! path where a connection carries no upgrade registrar and the cause table's
//! shutdown row reads the runtime's latching flag rather than a server control
//! watch.

use super::fixture::{BOUND, DRAIN_BOUND, WORKER_THREADS};
use super::peer::send;
use super::routes::READY_PATH;
use crate::common::BoundListener;
use camber::RuntimeError;
use camber::http::{Router, ServerHandle};
use camber::runtime;
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, Sender, channel};

/// Bind the fixture's listener on an ephemeral port.
///
/// Separate from serving because a lifecycle controller is keyed by address and
/// must be installed between the two, and the reservation carries the address
/// it was bound to so nothing between the two has to re-read it.
fn bind_owned() -> BoundListener {
    BoundListener::bind_tcp("127.0.0.1:0").expect("failed to bind the fixture listener")
}

/// Serve `router` on an already-bound reservation and wait for its readiness
/// route to answer.
///
/// The handle comes back raw because dropping it is the teardown every fixture
/// here measures: a guard that cancelled and joined on its own behalf would be
/// doing the thing under test.
fn serve_ready(listener: BoundListener, router: Router) -> ServerHandle {
    let addr = listener.local_addr();
    let handle = crate::common::serve_background_ready(listener, router, BOUND)
        .expect("the fixture server never became ready");
    assert_ready_route(addr);
    handle
}

/// Require the readiness route itself to answer, not just the transport.
///
/// The readiness wait is satisfied by any HTTP response, which a server whose
/// router was built without [`READY_PATH`] also produces. Every fixture drives
/// traffic through that route, so the route is what is proven here.
fn assert_ready_route(addr: SocketAddr) {
    let ready = send(
        addr,
        "GET",
        READY_PATH,
        "the fixture server's readiness route",
    );
    assert_eq!(ready.status, 200, "the readiness probe missed the router");
}

/// Start a runtime-owned server on an ephemeral port and wait for it to serve.
///
/// Call from inside a `runtime::run` closure: the listener, the server, and
/// the returned handle are all owned by that runtime, which is what makes
/// teardown the owned path's own shutdown rather than a detached task.
pub(super) fn start_owned(router: Router) -> (SocketAddr, ServerHandle) {
    let listener = bind_owned();
    let addr = listener.local_addr();
    (addr, serve_ready(listener, router))
}

/// Run `body` against a runtime-owned server and return its value.
///
/// The handle is dropped inside the closure so the supervisor terminates and
/// the scope drain — which awaits the supervisor driver — completes.
pub(super) fn with_owned_server<T, F>(router: Router, body: F) -> T
where
    F: FnOnce(SocketAddr) -> T,
{
    with_owned_handle(router, |addr, handle| {
        let outcome = body(addr);
        drop(handle);
        outcome
    })
}

/// Run `body` against a runtime-owned server, handing it the server handle.
///
/// The body owns teardown: dropping the handle is what ends the supervisor,
/// and the scope drain — which awaits the supervisor driver — cannot complete
/// until it does.
pub(super) fn with_owned_handle<T, F>(router: Router, body: F) -> T
where
    F: FnOnce(SocketAddr, ServerHandle) -> T,
{
    owned_builder()
        .run(move || {
            let (addr, handle) = start_owned(router);
            body(addr, handle)
        })
        .expect("the runtime did not return cleanly")
}

/// Run `body` against a runtime-owned server whose production checkpoints this
/// case drives, and hand it the server handle it must dispose of.
///
/// The controller is keyed by the listener's address and has to exist before
/// the supervisor is built, so it is installed between binding and serving.
/// Body owns teardown: the runtime cannot return until the handle is gone.
#[cfg(feature = "ws")]
pub(super) fn with_scripted_server<T, F>(router: Router, body: F) -> T
where
    F: FnOnce(SocketAddr, ServerHandle, &camber::http::mock::LifecycleController) -> T,
{
    owned_builder()
        .run(move || {
            let listener = bind_owned();
            let addr = listener.local_addr();
            let controller = camber::http::mock::lifecycle(addr)
                .expect("failed to install the fixture lifecycle controller");
            let handle = serve_ready(listener, router);
            body(addr, handle, &controller)
        })
        .expect("the runtime did not return cleanly")
}

/// Block until production pauses at `checkpoint`.
///
/// `wait_until_paused` has no deadline of its own, so the bound here is what
/// makes production that never reaches the checkpoint fail the case rather than
/// park the thread driving it.
#[cfg(feature = "ws")]
pub(super) fn await_lifecycle_pause(
    controller: &camber::http::mock::LifecycleController,
    checkpoint: camber::http::mock::LifecycleCheckpoint,
) {
    super::fixture::bounded(
        "production to reach the lifecycle checkpoint",
        controller.wait_until_paused(checkpoint),
    )
    .expect("the lifecycle checkpoint was not armed");
}

/// What stopping a [`SyncServer`] observed.
///
/// Every failure is a value rather than a panic, because `Drop` reaches this on
/// the unwind path: a panic there aborts the process and destroys the assertion
/// output for the whole test binary.
enum StopOutcome {
    /// An earlier call already stopped the server.
    AlreadyStopped,
    /// The serve thread did not return within [`BOUND`].
    Timeout,
    /// The serve thread returned, but nothing arrived on the result channel.
    ///
    /// Kept apart from [`StopOutcome::Timeout`] because the thread getting
    /// there and its result never being reported are different defects: the
    /// first is the hang this harness exists to catch, the second is this
    /// harness's own reporting.
    NoOutcome,
    /// The serve thread panicked.
    Panicked,
    /// The serve thread had already been joined by an earlier call.
    ///
    /// The guard above catches the ordinary double-stop, so reaching this means
    /// the handle went missing some other way. Kept apart from
    /// [`StopOutcome::Panicked`] because the shared join reports the two as
    /// distinct faults and folding them here would undo that.
    AlreadyJoined,
    /// The join reported a fault that is not one of its own.
    ///
    /// `BoundedReadError` is shared with the stream readers, so its read
    /// verdicts are reachable by type without being reachable by this call.
    /// Carried as text rather than panicked on, for the same reason every
    /// outcome here is a value: `Drop` reaches this while a case unwinds.
    JoinFault(Box<str>),
    /// What `serve_listener` returned.
    Served(Result<(), RuntimeError>),
}

/// The runtime closure a [`SyncServer`] serves from.
///
/// Extracted so the serve thread is one closure deep.
fn serve_ephemeral(
    router: Router,
    shutdown: tokio::sync::oneshot::Receiver<()>,
    published: Sender<SocketAddr>,
) -> Result<(), RuntimeError> {
    let listener = camber::net::listen("127.0.0.1:0").expect("failed to bind the listener");
    let addr = listener
        .local_addr()
        .expect("the listener has no address")
        .tcp()
        .expect("the listener is not a TCP listener");
    runtime::on_cancel(async move {
        let _ = shutdown.await;
    });
    let _ = published.send(addr);
    camber::http::serve_listener(listener, router)
}

/// A blocking `serve_listener` running on its own OS thread.
///
/// One owner for the thread, the runtime inside it, and the shutdown trigger.
/// Teardown is explicit and bounded, and runs from `Drop` too, so a panicking
/// case cannot leave the thread serving.
pub(crate) struct SyncServer {
    addr: SocketAddr,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    finished: Receiver<Result<(), RuntimeError>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl SyncServer {
    pub(crate) fn start(router: Router) -> Self {
        let (shutdown, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let (addr_tx, addr_rx) = channel::<SocketAddr>();
        let (finished_tx, finished) = channel::<Result<(), RuntimeError>>();

        let thread = std::thread::spawn(move || {
            let outcome = runtime::builder()
                .worker_threads(WORKER_THREADS)
                .shutdown_timeout(DRAIN_BOUND)
                .run(move || serve_ephemeral(router, shutdown_rx, addr_tx));
            let _ = finished_tx.send(outcome.and_then(|served| served));
        });

        let addr = addr_rx
            .recv_timeout(BOUND)
            .expect("the serve thread never published its address");
        crate::common::wait_for_http_response(addr, BOUND)
            .expect("the synchronous server never became ready");
        // The same readiness proof the owned harness requires. Both callers
        // serve a `probe_router`, so accepting any HTTP response here would
        // leave that router's readiness route unexercised.
        assert_ready_route(addr);
        Self {
            addr,
            shutdown: Some(shutdown),
            finished,
            thread: Some(thread),
        }
    }

    pub(crate) fn addr(&self) -> SocketAddr {
        self.addr
    }

    /// Stop the server, reporting every outcome instead of panicking on it.
    ///
    /// The join is bounded, and that bound is the whole point: a
    /// `serve_listener` that never returns is the regression this harness
    /// exists to catch, and `JoinHandle::join` would park the test binary on it
    /// rather than report it.
    fn try_stop(&mut self) -> StopOutcome {
        use crate::common::BoundedReadError;

        match self.thread.as_ref() {
            Some(_) => {}
            None => return StopOutcome::AlreadyStopped,
        }
        match self.shutdown.take() {
            Some(trigger) => {
                let _ = trigger.send(());
            }
            None => {}
        }
        let served = self.finished.recv_timeout(BOUND);
        let joined = crate::common::join_thread_bounded(&mut self.thread, BOUND);
        match (served, joined) {
            (_, Err(BoundedReadError::JoinTimeout { .. })) => StopOutcome::Timeout,
            (_, Err(BoundedReadError::ThreadPanicked)) => StopOutcome::Panicked,
            (_, Err(BoundedReadError::AlreadyJoined)) => StopOutcome::AlreadyJoined,
            (_, Err(error)) => StopOutcome::JoinFault(error.to_string().into_boxed_str()),
            (Ok(served), Ok(())) => StopOutcome::Served(served),
            (Err(_), Ok(())) => StopOutcome::NoOutcome,
        }
    }

    /// Stop the server and require that `serve_listener` returned success.
    pub(crate) fn assert_served(&mut self) {
        match self.try_stop() {
            StopOutcome::Served(Ok(())) => {}
            StopOutcome::Served(Err(error)) => {
                panic!("serve_listener returned an error: {error}")
            }
            StopOutcome::AlreadyStopped => panic!("the serve thread was already stopped"),
            StopOutcome::Timeout => panic!("the serve thread did not return within the bound"),
            StopOutcome::NoOutcome => {
                panic!("the serve thread returned but never reported what serve_listener produced")
            }
            StopOutcome::Panicked => panic!("the serve thread panicked"),
            StopOutcome::AlreadyJoined => panic!("the serve thread was already joined"),
            StopOutcome::JoinFault(reason) => {
                panic!("joining the serve thread reported {reason}")
            }
        }
    }
}

impl Drop for SyncServer {
    fn drop(&mut self) {
        // A final safety net, not the shutdown path — and it runs while a
        // failing case is unwinding, where a panic here would abort the process
        // and take that case's output with it.
        drop(self.try_stop());
    }
}

/// The settings every disconnect fixture's runtime carries.
///
/// One definition of the shared settings — worker threads and the bounded
/// drain — so a fixture that varies only the connection limit cannot drift
/// from the rest. Taking the builder rather than making one is what lets the
/// observed fixtures apply the same settings to the builder their shared
/// scaffold supplies.
pub(super) fn owned_settings(
    builder: runtime::RuntimeBuilder,
    connection_limit: Option<usize>,
) -> runtime::RuntimeBuilder {
    let builder = builder
        .worker_threads(WORKER_THREADS)
        .shutdown_timeout(DRAIN_BOUND);
    match connection_limit {
        Some(limit) => builder.connection_limit(limit),
        None => builder,
    }
}

/// The runtime the unobserved fixtures in this module own their server from.
fn owned_builder() -> runtime::RuntimeBuilder {
    owned_settings(runtime::builder(), None)
}
