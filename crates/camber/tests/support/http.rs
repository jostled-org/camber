use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use camber::RuntimeError;
use camber::http::mock::{LifecycleCheckpoint, LifecycleController, lifecycle};
use camber::http::{
    self, BodyAdmission, BodyAdmissionContext, HostRouter, Request, Response, Router, ServerHandle,
    ServerPolicy,
};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(2);
/// The gap between attempts in every bounded poll the suite runs.
///
/// Five milliseconds, the finer of the two intervals the harness used before
/// this was stated once. The interval is a retry granularity, not a budget:
/// every wait built on it clamps its sleep to what is left of the caller's
/// deadline, so a shorter interval only costs wakeups, while the ten-millisecond
/// one lost most of its resolution against the twenty-five-millisecond bounds
/// several fixtures assert with.
pub const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// The bound on one readiness attempt, which is how long a `connect` that hangs
/// can stall the poll before the next attempt is made.
const PROBE_ATTEMPT: Duration = Duration::from_millis(100);
const IO_TIMEOUT: Duration = Duration::from_secs(5);
/// The bound one request-and-answer exchange runs under.
///
/// The same number [`connect`] arms on the socket, named once for the suites
/// that hand a bound of their own to [`request`]. Two spellings of it are two
/// things that can drift, and the copy that drifts is the one that stops
/// bounding what it was written for.
pub const WIRE_TIMEOUT: Duration = IO_TIMEOUT;
const MAX_HEADER_BYTES: usize = 64 * 1024;
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = MAX_HEADER_BYTES + MAX_BODY_BYTES + MAX_HEADER_BYTES;
const SERVER_CLEANUP_TIMEOUT: Duration = Duration::from_secs(2);

/// How much of `deadline` is left, saturating at zero.
///
/// Lets a multi-leg wait share one deadline instead of giving each leg its own
/// bound, so its worst case is that deadline rather than a multiple of it.
///
/// Stated in this module rather than beside the runtime-scope waits that also
/// need it, because this is the one support module every harness root mounts:
/// the socket readers, the stream reader, the process guard, and the drain
/// helpers all reach it from here, and only some of those roots mount the
/// others.
pub fn remaining(deadline: Instant) -> Duration {
    deadline.saturating_duration_since(Instant::now())
}

/// A socket whose close is a TCP reset rather than an orderly shutdown.
///
/// `SO_LINGER` at zero is what makes a dropped socket send `RST` instead of
/// `FIN`, which is the only way an in-process case can hand a peer a genuine
/// transport failure rather than an ordinary end of stream. One definition for
/// every case that needs one: a second spelling is a second chance to build a
/// socket that closes cleanly and to assert a disconnect that never happened.
#[expect(
    deprecated,
    reason = "SO_LINGER zero is required to produce a deterministic TCP reset"
)]
pub fn abortive_tcp_socket() -> tokio::net::TcpSocket {
    let socket = tokio::net::TcpSocket::new_v4().expect("open abortive socket");
    socket
        .set_linger(Some(Duration::ZERO))
        .expect("set zero linger");
    assert_eq!(
        socket.linger().expect("read back linger"),
        Some(Duration::ZERO)
    );
    socket
}

/// Poll `attempt` every [`POLL_INTERVAL`] until it produces a value, giving up
/// at `bound`.
///
/// The one bounded wait the whole harness is written from: a rendezvous that
/// never arrives fails the test at `bound` instead of parking the waiter on it.
/// `attempt` is always tried at least once, so a leg handed what is left of a
/// shared deadline still gets its answer rather than an automatic refusal.
pub fn poll_value<T>(bound: Duration, mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + bound;
    loop {
        match (attempt(), Instant::now() < deadline) {
            (Some(value), _) => return Some(value),
            (None, false) => return None,
            // Clamped, so the last retry cannot overshoot the bound by a whole
            // interval and report a failure the caller's budget still allowed.
            (None, true) => std::thread::sleep(POLL_INTERVAL.min(remaining(deadline))),
        }
    }
}

/// Poll `ready` until it reports success, giving up at `bound`.
///
/// [`poll_value`] for a wait whose answer is the arrival itself rather than a
/// value it carries.
pub fn poll_until(bound: Duration, mut ready: impl FnMut() -> bool) -> bool {
    poll_value(bound, || ready().then_some(())).is_some()
}

#[derive(Debug, thiserror::Error)]
pub enum FixtureError {
    #[error("fixture I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("fixture runtime failed: {0}")]
    Runtime(#[from] RuntimeError),
    /// `cause` carries the last transport or parse failure the readiness poll
    /// saw. Without it a refused connection, a malformed response, and a server
    /// that never bound all report the same sentence.
    #[error("server did not return a valid HTTP readiness response before {timeout:?}: {cause}")]
    ReadinessTimeout { timeout: Duration, cause: Box<str> },
    #[error("server shutdown did not complete before {timeout:?}")]
    ShutdownTimeout { timeout: Duration },
    /// No ambient Tokio runtime, so the bounded join has nothing to drive the
    /// server task to completion on.
    #[error("no Tokio runtime was available to join the fixture server")]
    NoJoinRuntime,
    /// A runtime that cannot host a blocking wait. Reported rather than
    /// asserted on, because the guard's `Drop` reaches this too.
    #[error("a {flavor} Tokio runtime cannot host the fixture server's bounded join")]
    UnjoinableRuntime { flavor: Box<str> },
}

pub struct BoundListener {
    listener: std::net::TcpListener,
    local_addr: SocketAddr,
}

impl BoundListener {
    pub fn bind_tcp(addr: &str) -> Result<Self, io::Error> {
        let listener = std::net::TcpListener::bind(addr)?;
        listener.set_nonblocking(true)?;
        let local_addr = listener.local_addr()?;
        Ok(Self {
            listener,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Hand the reservation to Tokio without unbinding it.
    ///
    /// Exposed so a fixture that must own the raw [`ServerHandle`] serves from
    /// the same reservation the rest of the suite does, rather than binding a
    /// second listener of its own.
    pub(crate) fn into_tokio(self) -> Result<tokio::net::TcpListener, io::Error> {
        tokio::net::TcpListener::from_std(self.listener)
    }
}

#[derive(Default)]
struct ServerCleanupState {
    joined: std::sync::atomic::AtomicBool,
    error: Mutex<Option<Box<str>>>,
}

pub struct ServerCleanupProbe(Arc<ServerCleanupState>);

impl ServerCleanupProbe {
    pub fn joined(&self) -> bool {
        self.0.joined.load(std::sync::atomic::Ordering::Acquire)
    }

    /// The cleanup fault the guard recorded, if it recorded one.
    ///
    /// Cloned, not taken: the accessor borrows, so reading it twice — a test
    /// that logs the fault and then asserts on it, or two probes cloned from
    /// one server — must give the same answer both times. Draining it here
    /// would report "no cleanup fault" for a server that had one.
    pub fn cleanup_error(&self) -> Option<Box<str>> {
        self.0
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

pub struct ReadyServer {
    local_addr: SocketAddr,
    handle: Option<ServerHandle>,
    /// The probe's answer, for a server this guard waited on. An adopted server
    /// was never probed, so it carries none.
    readiness: Option<HttpResponse>,
    cleanup: Arc<ServerCleanupState>,
}

impl ReadyServer {
    pub fn start(
        listener: BoundListener,
        router: Router,
        timeout: Duration,
    ) -> Result<Self, FixtureError> {
        Self::started_by(listener, timeout, move |listener| {
            http::serve_background(listener, router).expect("owned server requires a Tokio runtime")
        })
    }

    /// Serve a reservation with whichever server function the caller names, and
    /// guard it once it answers.
    ///
    /// [`ReadyServer::start`] is this for the plain-router case. A fixture that
    /// serves a host router, or that must register a listener-scoped observer
    /// before anything serves on its port, needs the same readiness wait and
    /// the same cancel-on-failure diagnosis, and a second copy of either is a
    /// second place they can drift.
    pub fn started_by(
        listener: BoundListener,
        timeout: Duration,
        serve: impl FnOnce(tokio::net::TcpListener) -> ServerHandle,
    ) -> Result<Self, FixtureError> {
        let local_addr = listener.local_addr();
        let handle = serve(listener.into_tokio()?);
        let readiness = match wait_for_http_response(local_addr, timeout) {
            Ok(response) => response,
            Err(error) => {
                return Err(FixtureError::ReadinessTimeout {
                    timeout,
                    cause: cancel_unready(handle, &error),
                });
            }
        };
        // Built field by field rather than from `adopt`: this type owns a
        // `Drop`, so the functional-update syntax that would reuse that
        // constructor cannot move its fields out.
        Ok(Self {
            local_addr,
            handle: Some(handle),
            readiness: Some(readiness),
            cleanup: Arc::new(ServerCleanupState::default()),
        })
    }

    /// Take ownership of a server that is already serving, without probing it.
    ///
    /// The guard's whole contract is the teardown: the handle lives in an
    /// `Option`, `Drop` cancels and joins it under a bound, and an assertion
    /// that fails anywhere in the case still releases the task and the listener
    /// under it. Three fixtures wrote that contract out, two of them locally,
    /// because they could not use this one — a fixture that must start serving
    /// at an exact moment, or serve over TLS, has nothing to probe when the
    /// guard is built.
    ///
    /// Readiness is what it gives up, and only that. A caller that adopts owns
    /// the question of when the server is answering; a caller that wants it
    /// answered for it uses [`ReadyServer::start`].
    pub fn adopt(local_addr: SocketAddr, handle: ServerHandle) -> Self {
        Self {
            local_addr,
            handle: Some(handle),
            readiness: None,
            cleanup: Arc::new(ServerCleanupState::default()),
        }
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// The answer the readiness probe read.
    ///
    /// Only a probed server has one. An adopted server was never probed, so
    /// asking it this is a caller reading a fixture it did not build — reported
    /// as the mistake it is rather than as an absent response.
    pub fn readiness_response(&self) -> &HttpResponse {
        self.readiness
            .as_ref()
            .expect("an adopted server was never probed, so it read no readiness response")
    }

    pub fn cleanup_probe(&self) -> ServerCleanupProbe {
        ServerCleanupProbe(Arc::clone(&self.cleanup))
    }

    pub fn shutdown_bounded(mut self, timeout: Duration) -> Result<(), FixtureError> {
        self.shutdown_and_join(timeout)
    }

    /// Give up the guard and keep the server handle.
    ///
    /// For a fixture whose subject IS the handle's disposal: dropping the
    /// handle is the teardown it measures, so the guard that would cancel and
    /// join on its own behalf has to step aside. Taking the handle disarms the
    /// `Drop` arm, which then has nothing left to join.
    pub fn into_handle(mut self) -> ServerHandle {
        self.handle
            .take()
            .expect("a ready server always owns its handle until it is given up")
    }

    fn shutdown_and_join(&mut self, timeout: Duration) -> Result<(), FixtureError> {
        let handle = match self.handle.take() {
            Some(handle) => handle,
            None => return Ok(()),
        };
        handle.shutdown();
        self.join(handle, timeout)
    }

    fn cancel_and_join(&mut self, timeout: Duration) -> Result<(), FixtureError> {
        let handle = match self.handle.take() {
            Some(handle) => handle,
            None => return Ok(()),
        };
        handle.cancel();
        self.join(handle, timeout)
    }

    fn join(&self, handle: ServerHandle, timeout: Duration) -> Result<(), FixtureError> {
        let joined = join_bounded(handle, timeout);
        if joined.is_ok() {
            self.cleanup
                .joined
                .store(true, std::sync::atomic::Ordering::Release);
        }
        joined
    }

    fn record_cleanup_error(&self, error: &FixtureError) {
        let mut cleanup_error = self
            .cleanup
            .error
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *cleanup_error = Some(error.to_string().into_boxed_str());
    }

    /// Record one cleanup fault, and fail the case on the faults that are one.
    ///
    /// Recording alone was the weaker half of this guard's contract: two
    /// fixtures wrote their own because a server that failed its join, or never
    /// joined at all, wrote a sentence into a probe most cases never read and
    /// then passed. A server told to stop and unable to stop is a fault, and it
    /// fails here.
    ///
    /// Two of the reports are not that fault. [`FixtureError::NoJoinRuntime`]
    /// and [`FixtureError::UnjoinableRuntime`] say this thread has no runtime
    /// that can host a blocking wait — a current-thread fixture, or a `Drop`
    /// outside any runtime — so cancellation is the whole of what the guard can
    /// owe there, and it has already been sent. They stay recorded, which is
    /// what a case that wants to assert on them reads.
    ///
    /// Nothing is raised during an unwind, for the reason
    /// [`fail_unless_panicking`] states.
    fn report_cleanup_failure(&self, error: &FixtureError) {
        self.record_cleanup_error(error);
        match error {
            FixtureError::NoJoinRuntime | FixtureError::UnjoinableRuntime { .. } => {}
            error => fail_unless_panicking(&format!("the fixture server did not join: {error}")),
        }
    }
}

/// Fail the calling test with `fault`, unless this thread is already unwinding.
///
/// The rule every fixture teardown reads. A teardown fault is worth a failure —
/// a server that could not stop, a fixture thread that never returned — but a
/// panic raised during an unwind aborts the process and destroys the whole
/// binary's assertion output, and the case's own failure is the one worth
/// reading. Two `Drop` implementations stated this; two statements of it are two
/// places one of them can be forgotten.
pub fn fail_unless_panicking(fault: &str) {
    match std::thread::panicking() {
        true => {}
        false => panic!("{fault}"),
    }
}

impl Drop for ReadyServer {
    fn drop(&mut self) {
        match self.cancel_and_join(SERVER_CLEANUP_TIMEOUT) {
            Ok(()) => {}
            Err(error) => self.report_cleanup_failure(&error),
        }
    }
}

/// Wait for one server task to finish, bounded, without ever panicking.
///
/// A cancelled server ended because it was told to, so that is a completed join
/// and not a failure. Stated once, because a guard's teardown and a failed
/// start's cleanup read the same three outcomes.
///
/// Every current caller reaches this from an assertion-unwind path, one of them
/// through [`ReadyServer`]'s `Drop`. A panic during unwind aborts the process
/// and destroys the whole binary's assertion output, so the two conditions the
/// blocking wait needs are reported rather than asserted on: `Handle::current`
/// panics with no ambient runtime, and `block_in_place` panics on a
/// current-thread runtime. `Drop` records either through the cleanup probe, and
/// a test that reads the probe still gets its own failure.
///
/// Multi-thread is the only flavor that can host this wait: `block_in_place`
/// hands the worker's remaining tasks to a sibling thread first, and a
/// current-thread runtime has no sibling to hand them to.
fn join_bounded(handle: ServerHandle, timeout: Duration) -> Result<(), FixtureError> {
    let runtime = match tokio::runtime::Handle::try_current() {
        Ok(runtime) => runtime,
        Err(_) => return Err(FixtureError::NoJoinRuntime),
    };
    let join = tokio::time::timeout(timeout, handle.join());
    let result = match runtime.runtime_flavor() {
        tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| runtime.block_on(join))
        }
        flavor => {
            return Err(FixtureError::UnjoinableRuntime {
                flavor: format!("{flavor:?}").into_boxed_str(),
            });
        }
    };
    match result {
        Ok(Ok(())) | Ok(Err(RuntimeError::Cancelled)) => Ok(()),
        Ok(Err(error)) => Err(FixtureError::Runtime(error)),
        Err(_) => Err(FixtureError::ShutdownTimeout { timeout }),
    }
}

/// Cancel a server that never answered, and report both faults as one cause.
///
/// A failed [`ReadyServer::start`] hands back no guard, so it can hand out no
/// cleanup probe either: a cleanup fault recorded there is written where nothing
/// can ever read it. It joins the readiness diagnosis in the returned error
/// instead, so one error carries both — and neither displaces the other.
fn cancel_unready(handle: ServerHandle, readiness_error: &io::Error) -> Box<str> {
    handle.cancel();
    match join_bounded(handle, SERVER_CLEANUP_TIMEOUT) {
        Ok(()) => readiness_error.to_string().into_boxed_str(),
        Err(cleanup_error) => format!(
            "{readiness_error}; the unready server also failed to shut down: {cleanup_error}"
        )
        .into_boxed_str(),
    }
}

pub fn spawn_server_ready(router: Router, timeout: Duration) -> Result<ReadyServer, FixtureError> {
    let listener = BoundListener::bind_tcp("127.0.0.1:0")?;
    ReadyServer::start(listener, router, timeout)
}

/// A body-admission policy that refuses everything it is asked about, and counts
/// the asking.
///
/// Registered on routes that consume no body, so the count staying at zero is
/// the claim: a bodyless class reaches no application body policy at all. Four
/// roots make that claim — SSE, direct and proxied WebSocket upgrades, and gRPC
/// — and a copy of the policy in each is a copy that can drift.
pub fn refusing_body_admission(
    asked: &Arc<std::sync::atomic::AtomicUsize>,
) -> impl Fn(&BodyAdmissionContext<'_>) -> Result<BodyAdmission, RuntimeError> + Send + Sync + 'static
{
    let asked = Arc::clone(asked);
    move |_context: &BodyAdmissionContext<'_>| {
        asked.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(RuntimeError::InvalidArgument(
            "a bodyless route must not reach body admission".into(),
        ))
    }
}

/// How long a release, an outstanding count, or a paused checkpoint has to
/// settle.
///
/// A settling budget, not a sleep: every wait built on it returns as soon as the
/// observation arrives, and fails the case when it never does.
pub const SETTLE_BOUND: Duration = Duration::from_secs(5);

/// Bind `addr` again, asking until it answers or `bound` expires.
///
/// The one thing about a completed server an out-of-process observer can check
/// without asking Camber: the port it served on is free again. A single ask
/// answers a wider question than that. A peer whose transport is still being
/// torn down holds the address for a few milliseconds after the server let it
/// go, and the kernel refuses the bind for exactly that window — the port
/// carries nothing by the time it answers. A listener a server kept is never
/// bindable, so the bound keeps the claim and drops only the window.
///
/// The refusal that expires the bound is the one returned, because it is the
/// state the address was actually left in.
pub async fn rebind_within(
    addr: SocketAddr,
    bound: Duration,
) -> io::Result<tokio::net::TcpListener> {
    /// What a refused bind waits before asking again. Long enough that a
    /// closing transport is gone by the next ask, short enough that the usual
    /// answer costs one of them.
    const RETRY: Duration = Duration::from_millis(10);
    let deadline = tokio::time::Instant::now() + bound;
    loop {
        let refusal = match tokio::net::TcpListener::bind(addr).await {
            Ok(listener) => return Ok(listener),
            Err(refusal) => refusal,
        };
        if tokio::time::Instant::now() >= deadline {
            return Err(refusal);
        }
        tokio::time::sleep(RETRY).await;
    }
}

/// A body-admission permit whose release is the only thing it records.
///
/// The permit a policy hands over is dropped by the owner the runtime gave it
/// to, so its `Drop` is the only place a release is observable at all. Five
/// roots wrote a probe of their own for that, and they had already drifted into
/// two different arithmetics for one observation.
///
/// `Send + Sync + 'static`, because that is what `BodyAdmission::with_permit`
/// takes: an owner that crosses runtime workers.
pub struct PermitProbe {
    released: Arc<AtomicUsize>,
    /// The pool this permit was drawn from, for a case whose claim is what is
    /// still outstanding rather than what has been released.
    pool: Option<Arc<AtomicUsize>>,
}

impl Drop for PermitProbe {
    fn drop(&mut self) {
        match self.pool.as_ref() {
            Some(pool) => {
                pool.fetch_sub(1, Ordering::SeqCst);
            }
            None => {}
        }
        self.released.fetch_add(1, Ordering::SeqCst);
    }
}

/// A permit that counts its own release and nothing else.
pub fn permit_probe(released: &Arc<AtomicUsize>) -> PermitProbe {
    PermitProbe {
        released: Arc::clone(released),
        pool: None,
    }
}

/// Draw one permit from `pool`, counted for the route that admitted it.
///
/// The pool is raised here and lowered when the permit is released. A host
/// table's children draw from one pool, and a per-route release count alone
/// cannot say that every owner handed back what it took.
pub fn pooled_permit_probe(released: &Arc<AtomicUsize>, pool: &Arc<AtomicUsize>) -> PermitProbe {
    pool.fetch_add(1, Ordering::SeqCst);
    PermitProbe {
        released: Arc::clone(released),
        pool: Some(Arc::clone(pool)),
    }
}

/// Assert a release count settles at exactly `expected`.
///
/// Polled rather than read once: an admitted request's permit owner is released
/// when the request future finishes, which can happen after the peer already has
/// its answer. A count that is already right returns on the first look, and one
/// that never arrives fails here with what it reached instead.
pub fn assert_released(released: &Arc<AtomicUsize>, expected: usize, label: &str) {
    let settled = poll_until(SETTLE_BOUND, || released.load(Ordering::SeqCst) == expected);
    assert!(
        settled,
        "{label}: expected {expected} releases, saw {}",
        released.load(Ordering::SeqCst)
    );
}

/// Assert a listener's own permit-owner release count settles at exactly
/// `expected`.
///
/// [`assert_released`] for the counter the production owner writes rather than
/// the one the application permit does, and polled for the same reason: the
/// owner is released when the request future finishes, which can happen after
/// the peer already has its answer.
pub fn assert_owners_released(controller: &LifecycleController, expected: usize, label: &str) {
    let settled = poll_until(SETTLE_BOUND, || {
        controller.body_permit_owners_dropped() == expected
    });
    assert!(
        settled,
        "{label}: expected {expected} permit owners released, saw {}",
        controller.body_permit_owners_dropped()
    );
}

/// How long an observed fixture has to answer its first probe.
const OBSERVED_READINESS: Duration = Duration::from_secs(5);

/// A reserved port whose lifecycle observer is registered before anything
/// serves on it.
///
/// The one-step spawn helpers bind and serve together, so a router configured
/// with a body-admission policy that reads its own listener's counters cannot
/// be built between those two steps. This splits them: the observer exists
/// first, the router is built against it, and the same reservation is served.
///
/// The owned server path is the one served, because that is where the lifecycle
/// seam is wired; the synchronous path reads no script.
pub struct ObservedPort {
    listener: BoundListener,
    controller: Arc<LifecycleController>,
}

/// Reserve an ephemeral port and register its lifecycle observer.
pub fn reserve_observed() -> ObservedPort {
    let listener = BoundListener::bind_tcp("127.0.0.1:0").expect("reserve an observed port");
    let controller =
        lifecycle(listener.local_addr()).expect("register the reservation's lifecycle observer");
    ObservedPort {
        listener,
        controller: Arc::new(controller),
    }
}

impl ObservedPort {
    pub fn controller(&self) -> Arc<LifecycleController> {
        Arc::clone(&self.controller)
    }

    pub fn serve(self, router: Router) -> ObservedServer {
        self.serve_with(move |listener| {
            http::serve_background(listener, router).expect("owned server requires a Tokio runtime")
        })
    }

    /// Serve this reservation under the policy the case names.
    ///
    /// [`ObservedPort::serve`] takes the server's own defaults, which no
    /// deadline row can use: the bound it drives is the one it configured, and
    /// a fixture that could not name it would have to reach for a whole runtime
    /// to say so.
    pub fn serve_with_policy(self, router: Router, policy: ServerPolicy) -> ObservedServer {
        self.serve_with(move |listener| {
            http::server(router)
                .policy(policy)
                .serve_background(listener)
                .expect("owned server requires a Tokio runtime")
        })
    }

    pub fn serve_hosts(self, hosts: HostRouter) -> ObservedServer {
        self.serve_with(move |listener| {
            http::serve_background_hosts(listener, hosts)
                .expect("owned server requires a Tokio runtime")
        })
    }

    /// Give the reservation up as an owned Tokio listener, its address, and its
    /// observer.
    ///
    /// [`ObservedPort::serve_with`] hands the listener to [`ReadyServer`], whose
    /// guard probes readiness and then cancels and joins on its own behalf. A
    /// fixture whose case reads the server's own completion outcome, or that
    /// must stop it at an exact moment, has to hold the raw [`ServerHandle`]
    /// instead — so it takes the reservation here and serves it itself, rather
    /// than binding a second listener and registering a second observer.
    ///
    /// The observer travels with the listener because that is the pairing this
    /// type exists to keep: registration happens before anything serves on the
    /// port, and the caller that takes it is the one that decides when to give
    /// it back.
    pub fn into_owned_parts(
        self,
    ) -> (
        tokio::net::TcpListener,
        SocketAddr,
        Arc<LifecycleController>,
    ) {
        let local_addr = self.listener.local_addr();
        let listener = self
            .listener
            .into_tokio()
            .expect("hand the observed reservation to its own owner");
        (listener, local_addr, self.controller)
    }

    fn serve_with(
        self,
        serve: impl FnOnce(tokio::net::TcpListener) -> ServerHandle,
    ) -> ObservedServer {
        let controller = self.controller;
        let server = ReadyServer::started_by(self.listener, OBSERVED_READINESS, serve)
            .expect("the observed fixture server answered");
        ObservedServer { server, controller }
    }
}

/// A served fixture and the observer registered for its listener.
///
/// Teardown is the guarded server's own: dropping this drops the guard, which
/// cancels and joins under a bound.
///
/// The observer outlives that by design. [`ObservedPort::controller`] hands out
/// an `Arc` so a body-admission closure captured into the served router can read
/// the same counters the case does, and the registration is released when the
/// last of those handles drops — which is the served router's, not this one's.
pub struct ObservedServer {
    server: ReadyServer,
    controller: Arc<LifecycleController>,
}

impl ObservedServer {
    pub fn addr(&self) -> SocketAddr {
        self.server.local_addr()
    }

    pub fn controller(&self) -> &LifecycleController {
        &self.controller
    }

    /// Stop this fixture and join it under `timeout`.
    ///
    /// Dropping does the same thing. A case whose claim IS the shutdown needs
    /// the failure reported rather than swallowed by a destructor, and one
    /// asserting on what a stopped server released needs the join to have
    /// happened before it reads its counters.
    pub fn shutdown_bounded(self, timeout: Duration) -> Result<(), FixtureError> {
        self.server.shutdown_bounded(timeout)
    }
}

/// Serve `router` on an already-bound reservation and hand back the raw handle
/// once the server answers.
///
/// The readiness wait is [`ReadyServer::start`]'s, so a fixture that owns its
/// own teardown does not carry a second copy of it. A server that never answers
/// is cancelled and joined before the error returns, exactly as the guarded
/// form does.
pub fn serve_background_ready(
    listener: BoundListener,
    router: Router,
    timeout: Duration,
) -> Result<ServerHandle, FixtureError> {
    ReadyServer::start(listener, router, timeout).map(ReadyServer::into_handle)
}

/// Hand an owned Tokio listener to whichever server serves it, and guard the
/// handle that comes back.
///
/// [`spawn_server_ready`] binds, serves plain HTTP, and probes, in one step.
/// A fixture that cannot take all three — one serving over TLS, or one that
/// must start serving at an exact moment after its peer is already waiting —
/// took none of them and wrote its own guard. `serve` is any function from a
/// listener to a [`ServerHandle`], so `serve_background`, `serve_background_tls`
/// and a closure over either all reach the same teardown.
///
/// The listener is already bound, because binding is what the caller varies:
/// a case that reserves its port before its runtime exists cannot have the
/// reservation made for it here.
pub fn serve_owned(
    listener: tokio::net::TcpListener,
    serve: impl FnOnce(tokio::net::TcpListener) -> ServerHandle,
) -> io::Result<ReadyServer> {
    let local_addr = listener.local_addr()?;
    Ok(ReadyServer::adopt(local_addr, serve(listener)))
}

/// Add a `/second` route that reports its first dispatch, and hand back the
/// receiver that observes it.
///
/// A connection-permit case reads the same one-shot twice: empty while a bridge
/// still holds the permit, closed once the owner has completed and dropped the
/// route with it. The take-once guard is what keeps a route dispatched more than
/// once from sending twice on a sender that only carries one value.
///
/// Stated here rather than beside either case, because the direct and proxied
/// halves of the claim live in different test binaries and a copy in each would
/// let the probe they share drift.
pub fn attach_dispatch_probe(router: &mut Router) -> tokio::sync::oneshot::Receiver<()> {
    let (dispatched_tx, dispatched_rx) = tokio::sync::oneshot::channel();
    let dispatched_tx = Arc::new(Mutex::new(Some(dispatched_tx)));
    router.get("/second", move |_request: &Request| {
        let dispatched_tx = Arc::clone(&dispatched_tx);
        async move {
            if let Some(sender) = dispatched_tx
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                let _ = sender.send(());
            }
            Response::text(200, "second")
        }
    });
    dispatched_rx
}

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Box<[(Box<str>, Box<str>)]>,
    pub body: Box<[u8]>,
    raw: Box<[u8]>,
}

impl HttpResponse {
    pub fn raw(&self) -> &[u8] {
        &self.raw
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_ref())
    }

    /// Every value this answer carries under one header name, in wire order.
    ///
    /// [`HttpResponse::header`] answers with the first value, which cannot say
    /// that a corrected header carries exactly one: enforcement that appended
    /// rather than replaced reads identically through it. Three roots wrote this
    /// lookup out to make that distinction, so the distinction lives here.
    ///
    /// Borrowed and sealed: every caller compares the values or reports them,
    /// and nothing appends to a lookup's result.
    pub fn header_values(&self, name: &str) -> Box<[&str]> {
        self.headers
            .iter()
            .filter(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_ref())
            .collect()
    }

    /// This answer's body, read as text however it was encoded.
    ///
    /// Lossy rather than checked: a body a case asserts on is a body a case can
    /// read, and a refusal that answered with bytes no encoding explains is a
    /// failure to report rather than a decode to propagate. It sits beside
    /// [`HttpResponse::header`] because it is the same kind of read — one field
    /// off one answer — and a free function elsewhere was one more name for it.
    ///
    /// Sealed: every caller compares the text or reports it, and nothing appends
    /// to a body that has already been sent.
    pub fn text(&self) -> Box<str> {
        String::from_utf8_lossy(&self.body).into()
    }

    /// One answer a transport framed for itself, read as every other one is.
    ///
    /// HTTP/2 hands its caller a status, a header map, and a drained body rather
    /// than the bytes of a message, so a root reading it used to carry a second
    /// answer type with its own hand-copied `header` and `text`. It reads this
    /// instead. The raw text is rebuilt from the parts rather than dropped: the
    /// leak assertions search the whole answer, head and body alike, and one
    /// that had no head to search would report a header leak as absent.
    pub fn from_parts(
        status: u16,
        headers: Box<[(Box<str>, Box<str>)]>,
        body: Box<[u8]>,
    ) -> HttpResponse {
        let mut raw = format!("HTTP/2 {status}\r\n");
        append_headers(
            &mut raw,
            headers
                .iter()
                .map(|(name, value)| (name.as_ref(), value.as_ref())),
        );
        raw.push_str("\r\n");
        let mut raw = raw.into_bytes();
        raw.extend_from_slice(&body);
        HttpResponse {
            status,
            headers,
            body,
            raw: raw.into_boxed_slice(),
        }
    }
}

/// The authority a request is addressed to when the case turns on no other one.
///
/// Public because the writers that take an authority as a parameter still need
/// one to name when the case is not about the authority, and a caller spelling
/// `"localhost"` again is a second copy of the default.
pub const DEFAULT_HOST: &str = "localhost";

/// The `Connection` value a request that wants no second exchange sends.
pub const CLOSE_AFTER_RESPONSE: &str = "close";

/// The `Connection` value a request that offers to send another one sends.
pub const KEEP_CONNECTION: &str = "keep-alive";

/// A request target with more path segments than the router will match.
///
/// The limit is the router's, so the fixture that provokes a URI-depth refusal
/// states it once here rather than once per suite that asserts on it. Sealed:
/// every caller sends the target as it stands and nothing appends to it.
pub fn overdeep_path() -> Box<str> {
    "/deep".repeat(33).into_boxed_str()
}

/// The target one table row asks for.
///
/// A row states its own target instead of carrying a flag another line has to
/// interpret. Stated beside [`overdeep_path`] because the deep case is that
/// function's, and two roots drove it from a bool of their own.
pub enum PathSpec {
    /// A target with more path segments than the router will match.
    Deep,
    /// The exact target this row asks for.
    Exact(&'static str),
}

impl PathSpec {
    /// The target this row sends, given the deep path its caller built once.
    ///
    /// The deep case borrows rather than rebuilding: [`overdeep_path`] allocates,
    /// and a table driving one row per iteration would allocate the same target
    /// again for every row that names it.
    pub fn resolve<'a>(&self, deep: &'a str) -> &'a str {
        match self {
            PathSpec::Deep => deep,
            PathSpec::Exact(path) => path,
        }
    }
}

/// Write `headers` onto a request or response head, one CRLF-terminated line
/// each.
///
/// Three heads are built in this suite — the bodyless request, the framed one,
/// and the WebSocket upgrade — and they differ in their start line and in
/// nothing else. Three copies of this loop were three places the separator, the
/// terminator, or the order could drift from what the other two send.
///
/// It takes any sequence of pairs rather than a slice of them. A caller holding
/// owned strings — [`HttpResponse::from_parts`], rebuilding a head out of an
/// HTTP/2 header map — collected a throwaway slice of borrows per response for
/// no reason but this parameter. A borrowing iterator writes the same head and
/// allocates nothing; the slice callers spell the same thing as
/// `.iter().copied()`.
pub fn append_headers<'a>(
    head: &mut String,
    headers: impl IntoIterator<Item = (&'a str, &'a str)>,
) {
    headers.into_iter().for_each(|(name, value)| {
        head.push_str(name);
        head.push_str(": ");
        head.push_str(value);
        head.push_str("\r\n");
    });
}

/// Send one request with an exact method, path, and `Host` value.
///
/// [`request`] writes `Host: localhost` for every call, so a case that turns on
/// the authority the peer sent — host routing, or an authority the server must
/// refuse — cannot be expressed through it. A second `Host` line is a different
/// request, not the same one with an override.
pub fn request_with_host(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
) -> io::Result<HttpResponse> {
    request_to_host(
        addr,
        method,
        path,
        host,
        &[("Connection", CLOSE_AFTER_RESPONSE)],
    )
}

/// Send one request under the suite's wire bound, failing the calling test
/// rather than reporting.
///
/// Five roots wrote this wrapper, panic text included. A send that does not
/// complete is never the claim under test in any of them: it is the fixture's
/// own transport breaking, and the row that asked for it has nothing left to
/// assert. The bound is [`WIRE_TIMEOUT`], because a caller that has given up its
/// error has no way to act on a budget of its own either.
pub fn send(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> HttpResponse {
    request(addr, method, path, headers, body, WIRE_TIMEOUT)
        .unwrap_or_else(|error| panic!("{method} {path} did not complete: {error}"))
}

/// [`send`], addressed to a named authority and carrying the headers the caller
/// names.
///
/// The authority is a parameter rather than one more header, for the reason
/// [`request_with_host`] gives: a request carrying two `Host` values is a
/// different request, not the same one with an override.
///
/// The one failure sentence a host-addressed send has. [`send_to_host`] is this
/// with the close preference filled in, so a peer that never answered is
/// reported the same way whichever form asked it.
pub fn send_to_host_with(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
) -> HttpResponse {
    request_to_host(addr, method, path, host, headers)
        .unwrap_or_else(|error| panic!("{method} {path} (Host: {host}) did not complete: {error}"))
}

/// [`send_to_host_with`], asking the server to close after it answers.
///
/// The preference every case that reads one answer and stops wants. A case whose
/// claim IS the connection's disposition states its own headers through
/// [`send_to_host_with`], because a request that asked for close would be
/// reading back its own preference.
pub fn send_to_host(addr: SocketAddr, method: &str, path: &str, host: &str) -> HttpResponse {
    send_to_host_with(
        addr,
        method,
        path,
        host,
        &[("Connection", CLOSE_AFTER_RESPONSE)],
    )
}

/// [`request_with_host`], with the connection preference left to the caller.
///
/// A case that reads the response's own `Connection` value cannot ask for close
/// itself: the server would echo the request's preference, and the assertion
/// would hold whether or not the framework had decided anything.
pub fn request_to_host(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
) -> io::Result<HttpResponse> {
    let mut stream = connect(addr)?;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {host}\r\n");
    append_headers(&mut head, headers.iter().copied());
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    read_http_response_bounded(&mut stream)
}

pub fn request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> io::Result<HttpResponse> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_write_timeout(Some(timeout))?;
    write_request(&mut stream, method, path, headers, body)?;
    with_read_deadline(&mut stream, timeout, |stream, deadline| {
        read_http_response(stream, Some(deadline))
    })
}

pub fn wait_for_http_response(addr: SocketAddr, timeout: Duration) -> io::Result<HttpResponse> {
    let deadline = Instant::now().checked_add(timeout).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "readiness deadline overflowed")
    })?;
    // Carried in the closure rather than returned by it, because the poll keeps
    // only the successful answer: without this, a readiness bound that expires
    // reports the expiry and loses the refusal, malformed response, or unbound
    // listener that actually caused it.
    let mut last_error = io::Error::from(io::ErrorKind::TimedOut);
    let probed = poll_value(timeout, || {
        let attempt = remaining(deadline).min(PROBE_ATTEMPT);
        // The budget is spent and the poll is about to end anyway. A zero connect
        // bound is refused outright, and that refusal would displace the
        // diagnosis this wait exists to carry out.
        if attempt.is_zero() {
            return None;
        }
        match probe_transport(addr, attempt) {
            Ok(response) => Some(response),
            Err(error) => {
                last_error = error;
                None
            }
        }
    });
    probed.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("HTTP readiness timed out; last error: {last_error}"),
        )
    })
}

fn probe_transport(addr: SocketAddr, timeout: Duration) -> io::Result<HttpResponse> {
    let mut stream = TcpStream::connect_timeout(&addr, timeout)?;
    stream.set_write_timeout(Some(timeout))?;
    stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: invalid\r\n\r\n")?;
    stream.flush()?;
    with_read_deadline(&mut stream, timeout, |stream, deadline| {
        read_http_response(stream, Some(deadline))
    })
}

/// One request's whole raw response text.
///
/// Sealed: the caller reads a status off it or searches it, and nothing appends
/// to it, so re-opening the text into a `String` would buy a spare capacity
/// field nothing uses.
pub fn raw_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Box<str> {
    raw_request_with_body(addr, method, path, headers, &[])
}

pub fn raw_request_with_body(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Box<str> {
    let mut stream = connect(addr).unwrap();
    write_request(&mut stream, method, path, headers, body).unwrap();
    let response = with_read_deadline(&mut stream, IO_TIMEOUT, |stream, deadline| {
        read_http_response(stream, Some(deadline))
    })
    .unwrap();
    String::from_utf8_lossy(response.raw())
        .into_owned()
        .into_boxed_str()
}

/// The status code off a raw response head.
///
/// A head with no readable status fails here with the text it was given. The
/// sentinel this used to return read as status 0, which failed the caller's
/// status assertion for the wrong reason and hid the malformed response that
/// caused it.
pub fn status_from_raw(raw: &str) -> u16 {
    raw.lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|status| status.parse().ok())
        .unwrap_or_else(|| panic!("the response head carried no readable status: {raw:?}"))
}

pub fn connect(addr: SocketAddr) -> io::Result<TcpStream> {
    let stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)?;
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    Ok(stream)
}

/// The request an admission probe sends once it has a connection.
const ADMISSION_PROBE_REQUEST: &[u8] =
    b"GET /retained HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";

/// What a read that never answered reports, whether the bound was the socket's
/// or the caller's own timer.
const ADMISSION_READ_TIMED_OUT: &str = "timed out waiting for closed admission";

/// What one stage of an admission probe established.
///
/// The triage itself, held apart from the transport that produced it, so the
/// async and blocking probes classify the same outcomes the same way instead of
/// each deciding for itself what counts as proof.
enum AdmissionStage {
    /// Nothing is accepting, or the peer is already gone. That is what closed
    /// admission means, and the probe is over.
    Closed,
    /// The stage passed without settling the question. Go on to the next one.
    Continue,
    /// The probe cannot answer the question, and why.
    Inconclusive(Box<str>),
}

/// End one admission probe.
///
/// A stage that settled the question returns; one that could not fails the test
/// with what it saw. `Continue` cannot reach here — every caller consumes it —
/// and reaching it would mean a probe that ran out of stages without a verdict.
fn settle(stage: AdmissionStage) {
    match stage {
        AdmissionStage::Closed => {}
        AdmissionStage::Continue => {
            panic!("the admission probe ran out of stages without reaching a verdict")
        }
        AdmissionStage::Inconclusive(reason) => panic!("{reason}"),
    }
}

/// Read a failed connect.
fn classify_connect_error(error: &io::Error, timeout: Duration) -> AdmissionStage {
    match error.kind() {
        // Refused, or answered by a listener already gone: nothing is accepting,
        // which is what closed admission means.
        io::ErrorKind::ConnectionRefused => AdmissionStage::Closed,
        _ if is_closed_connection_error(error) => AdmissionStage::Closed,
        // Neither completed nor refused within the bound. That is a listener
        // still bound with a saturated backlog — admission is open — so it
        // cannot be read as proof that it is closed.
        _ if is_deadline_expiry(error) => {
            AdmissionStage::Inconclusive(connect_never_settled(timeout))
        }
        // Any other connect failure is the fixture's own transport breaking —
        // an exhausted descriptor table, an unreachable route — and says
        // nothing about whether the server is still admitting.
        kind => AdmissionStage::Inconclusive(
            format!("the connect failed as {kind:?}, which is not proof that admission is closed: {error}")
                .into_boxed_str(),
        ),
    }
}

/// What a connect that neither completed nor was refused reports.
fn connect_never_settled(timeout: Duration) -> Box<str> {
    format!("the connect neither completed nor was refused within {timeout:?}").into_boxed_str()
}

/// Read the probe request's write.
fn classify_write(result: io::Result<()>) -> AdmissionStage {
    match result {
        Ok(()) => AdmissionStage::Continue,
        Err(error) if is_closed_connection_error(&error) => AdmissionStage::Closed,
        Err(error) => AdmissionStage::Inconclusive(
            format!("failed while probing closed admission: {error}").into_boxed_str(),
        ),
    }
}

/// Read the one response byte a still-admitting server would produce.
fn classify_read(result: io::Result<usize>) -> AdmissionStage {
    match result {
        Ok(0) => AdmissionStage::Closed,
        Err(error) if is_closed_connection_error(&error) => AdmissionStage::Closed,
        Err(error) if is_deadline_expiry(&error) => {
            AdmissionStage::Inconclusive(ADMISSION_READ_TIMED_OUT.into())
        }
        Ok(read) => AdmissionStage::Inconclusive(
            format!("closed admission produced {read} response byte(s)").into_boxed_str(),
        ),
        Err(error) => AdmissionStage::Inconclusive(
            format!("failed while waiting for closed admission: {error}").into_boxed_str(),
        ),
    }
}

pub async fn assert_admission_closed(addr: SocketAddr, timeout: Duration) {
    let mut stream = match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(addr)).await
    {
        Ok(Ok(stream)) => stream,
        Ok(Err(error)) => return settle(classify_connect_error(&error, timeout)),
        Err(_) => {
            return settle(AdmissionStage::Inconclusive(connect_never_settled(timeout)));
        }
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    match classify_write(stream.write_all(ADMISSION_PROBE_REQUEST).await) {
        AdmissionStage::Continue => {}
        stage => return settle(stage),
    }
    let mut byte = [0_u8; 1];
    let read = match tokio::time::timeout(timeout, stream.read(&mut byte)).await {
        Ok(read) => read,
        Err(_) => {
            return settle(AdmissionStage::Inconclusive(
                ADMISSION_READ_TIMED_OUT.into(),
            ));
        }
    };
    settle(classify_read(read));
}

/// [`assert_admission_closed`] for a caller with no runtime to await on.
///
/// The same triage, driven over a blocking socket: a sync harness that
/// hand-rolled its own would decide for itself what counts as proof that
/// admission is closed, and a connect failure that only means the fixture's own
/// transport broke would then pass as that proof.
pub fn assert_admission_closed_blocking(addr: SocketAddr, timeout: Duration) {
    let mut stream = match TcpStream::connect_timeout(&addr, timeout) {
        Ok(stream) => stream,
        Err(error) => return settle(classify_connect_error(&error, timeout)),
    };
    let armed = tolerate_dead_socket(stream.set_read_timeout(Some(timeout)))
        .and_then(|()| tolerate_dead_socket(stream.set_write_timeout(Some(timeout))));
    match classify_write(armed.and_then(|()| stream.write_all(ADMISSION_PROBE_REQUEST))) {
        AdmissionStage::Continue => {}
        stage => return settle(stage),
    }
    let mut byte = [0_u8; 1];
    settle(classify_read(stream.read(&mut byte)));
}

pub fn write_request(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    write_request_with_connection(stream, CLOSE_AFTER_RESPONSE, method, path, headers, body)
}

/// [`write_request`], with the connection preference the caller names.
///
/// A case that turns on the framework's own disposition cannot ask for close
/// itself: the server would be answering the peer's preference, and the
/// assertion would hold whether or not anything had decided anything.
pub fn write_request_with_connection(
    stream: &mut TcpStream,
    connection: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<()> {
    let head = body_request_head(
        DEFAULT_HOST,
        connection,
        method,
        path,
        headers,
        BodyFraming::Length(body.len()),
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()
}

/// The exact bytes one length-framed request puts on the wire.
///
/// For a case that paces its own writes. Every other writer here owns the socket
/// and decides when the bytes leave; a case whose claim is that the peer stopped
/// taking them needs the head and the payload as one buffer it can offer a piece
/// at a time. The head is [`body_request_head`]'s, so a paced request and a
/// written one cannot disagree about what was framed.
pub fn framed_request_bytes(
    connection: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Box<[u8]> {
    let head = body_request_head(
        DEFAULT_HOST,
        connection,
        method,
        path,
        headers,
        BodyFraming::Length(body.len()),
    );
    let mut wire = head.into_bytes();
    wire.extend_from_slice(body);
    wire.into_boxed_slice()
}

/// The exact bytes one chunked request head puts on the wire.
///
/// A producer that generates its body after the socket accepts the head needs
/// the fixed-body writers' header authority without fabricating a length.
pub fn framed_chunked_request_head(
    connection: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
) -> Box<[u8]> {
    body_request_head(
        DEFAULT_HOST,
        connection,
        method,
        path,
        headers,
        BodyFraming::Chunked,
    )
    .into_bytes()
    .into_boxed_slice()
}

/// How one request head frames the payload behind it.
///
/// The framing line is the only thing the suite's request heads differ in, so
/// it is the parameter rather than the reason for a second head builder.
#[derive(Clone, Copy)]
pub enum BodyFraming<'a> {
    /// A payload of exactly this many bytes.
    Length(usize),
    /// A declaration exactly as the caller spells it.
    ///
    /// A string rather than a number because what several cases turn on is a
    /// length no `usize` holds — `18446744073709551615` — and a refusal decided
    /// from a declaration must not need the payload to arrive before it can be
    /// given.
    Declared(&'a str),
    /// Chunked, which declares no length at all. The only way to reach an
    /// observed-byte bound: a declared oversize is refused before a frame is
    /// polled.
    Chunked,
}

impl BodyFraming<'_> {
    /// The one header line this framing states, CRLF-terminated.
    fn header_line(self) -> String {
        match self {
            BodyFraming::Length(body_len) => format!("Content-Length: {body_len}\r\n"),
            BodyFraming::Declared(declared) => format!("Content-Length: {declared}\r\n"),
            BodyFraming::Chunked => "Transfer-Encoding: chunked\r\n".to_owned(),
        }
    }
}

/// The head of one request, framing its payload as `framing` states.
///
/// Stated once because every writer in this module differs from the others in
/// its framing line and in nothing else, and each hand-written copy of a request
/// head is one more thing that can disagree with the rest about what was sent.
fn body_request_head(
    host: &str,
    connection: &str,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    framing: BodyFraming<'_>,
) -> String {
    let mut head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: {connection}\r\n{}",
        framing.header_line()
    );
    append_headers(&mut head, headers.iter().copied());
    head.push_str("\r\n");
    head
}

/// A chunk size that is not a size, sent where the framing promised one.
///
/// Hyper accepts the request head, so Camber's own body collection runs and
/// then fails — on neither the length limit nor the deadline, and with far
/// fewer bytes sent than any limit collects. This is the shape a mid-body
/// transport failure or a peer reset arrives in, written as bytes a test can
/// send on demand.
const BROKEN_CHUNK: &str = "zz\r\n";

/// Send a request whose chunked body framing breaks after the head.
///
/// The connection preference is the caller's, for the reason
/// [`write_request_with_connection`] gives: a case that turns on the
/// framework's own disposition cannot ask for close itself.
pub fn write_unreadable_body(
    stream: &mut TcpStream,
    connection: &str,
    method: &str,
    path: &str,
    content_type: &str,
) -> io::Result<()> {
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {DEFAULT_HOST}\r\nConnection: {connection}\r\n\
         Content-Type: {content_type}\r\nTransfer-Encoding: chunked\r\n\r\n{BROKEN_CHUNK}"
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

/// Open a connection, send a body that stops being readable, and read the answer
/// off it.
///
/// The socket comes back with the answer because the connection is half the
/// claim: a refusal that left the request body unread has decided a disposition,
/// and what that connection does with one more request is the only way to
/// observe it. Two roots wrote the connect, the write, and the bounded read out
/// as one sequence; the sequence is stated here and each root keeps its own
/// failure sentences.
pub fn send_unreadable_body(
    addr: SocketAddr,
    connection: &str,
    method: &str,
    path: &str,
    content_type: &str,
) -> io::Result<(HttpResponse, TcpStream)> {
    let mut stream = connect(addr)?;
    write_unreadable_body(&mut stream, connection, method, path, content_type)?;
    let refused = read_http_response_bounded(&mut stream)?;
    Ok((refused, stream))
}

/// Send one request that declares a body length and sends none of it.
///
/// The socket comes back because the connection is half the claim: framed bytes
/// that will never be read decide a disposition, and only the connection shows
/// it. [`BodyFraming::Declared`] states why the declaration travels as a string.
pub fn send_withheld_declaration(
    addr: SocketAddr,
    connection: &str,
    method: &str,
    path: &str,
    declared: &str,
) -> io::Result<(HttpResponse, TcpStream)> {
    let mut stream = connect(addr)?;
    let head = body_request_head(
        DEFAULT_HOST,
        connection,
        method,
        path,
        &[],
        BodyFraming::Declared(declared),
    );
    stream.write_all(head.as_bytes())?;
    stream.flush()?;
    let refused = read_http_response_bounded(&mut stream)?;
    Ok((refused, stream))
}

/// Open one chunked request to `host` on a connection the caller owns.
///
/// The authority is a parameter for the reason [`request_with_host`] gives: a
/// request carrying two `Host` values is a different request, not the same one
/// with an override, and a host-routed upload is addressed to a child.
/// [`BodyFraming::Chunked`] states why a case reaching the observed-byte bound
/// has to withhold the declaration.
pub fn write_chunked_head(
    stream: &mut TcpStream,
    connection: &str,
    method: &str,
    path: &str,
    host: &str,
) -> io::Result<()> {
    let head = body_request_head(host, connection, method, path, &[], BodyFraming::Chunked);
    stream.write_all(head.as_bytes())?;
    stream.flush()
}

/// Write and flush exactly one chunked data frame.
///
/// Flushed on its own, so a case naming three chunks is naming the three data
/// frames the server's collector will poll rather than whatever the transport
/// decided to coalesce.
///
/// Framed into one buffer and written once. Three writes let the size, the
/// payload, and the terminator reach the peer as three segments, and a case that
/// staged a scheduling order behind the last frame it sent would be racing the
/// wake the trailing two bytes provoke.
pub fn write_chunk(stream: &mut TcpStream, chunk: &[u8]) -> io::Result<()> {
    let mut framed = format!("{:x}\r\n", chunk.len()).into_bytes();
    framed.extend_from_slice(chunk);
    framed.extend_from_slice(b"\r\n");
    stream.write_all(&framed)?;
    stream.flush()
}

/// End a chunked body.
pub fn write_chunked_end(stream: &mut TcpStream) -> io::Result<()> {
    stream.write_all(b"0\r\n\r\n")?;
    stream.flush()
}

/// Write every named data frame, then end the body.
///
/// The terminator is written here so no caller of [`send_chunked`] can leave a
/// body unfinished by accident.
fn write_chunks(stream: &mut TcpStream, chunks: &[&[u8]]) -> io::Result<()> {
    chunks
        .iter()
        .try_for_each(|chunk| write_chunk(stream, chunk))?;
    write_chunked_end(stream)
}

/// Send one chunked request to `host` whose data frames are exactly `chunks`.
///
/// The socket comes back for the same reason [`send_unreadable_body`]'s does: a
/// refusal that stopped mid-body left framing unread, and what the connection
/// does next is the only observation of that.
///
/// The whole body is written before the answer is read. The frames are small by
/// construction, so they fit the transport's buffers, and a server that refuses
/// partway through is answering a request the peer has finished sending rather
/// than racing one it has not.
pub fn send_chunked(
    addr: SocketAddr,
    connection: &str,
    method: &str,
    path: &str,
    host: &str,
    chunks: &[&[u8]],
) -> io::Result<(HttpResponse, TcpStream)> {
    let mut stream = connect(addr)?;
    write_chunked_head(&mut stream, connection, method, path, host)?;
    tolerate_answered_peer(write_chunks(&mut stream, chunks))?;
    let answered = read_http_response_bounded(&mut stream)?;
    Ok((answered, stream))
}

/// The body length a stalled request declares and never finishes sending.
///
/// The declared length is the fixture, not its size: the peer sends one byte and
/// stops, so what the server waits on is the rest of a promise. Sixty-four is
/// what both roots that stall a body chose, and two constants under one name
/// were two numbers that could drift while both went on being called the stalled
/// length.
pub const STALLED_CONTENT_LENGTH: usize = 64;

/// The head of one request that promises [`STALLED_CONTENT_LENGTH`] body bytes
/// and sends one.
///
/// `connection` is an `Option` because one root states a preference and the
/// other deliberately states none: a case reading back the framework's own
/// disposition cannot send a preference for it to echo, and a case proving
/// reuse after the refusal has to offer keep-alive. Absence is a third request,
/// not a default either root can stand in for.
pub fn stalled_request_head(connection: Option<&str>, method: &str, path: &str) -> String {
    // Formatting into the head rather than through a second `format!`, because
    // a formatted write to a `String` cannot fail and the intermediate would be
    // one allocation per stalled request for nothing.
    use std::fmt::Write as _;
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: {DEFAULT_HOST}\r\n");
    append_headers(&mut head, connection.map(|value| ("Connection", value)));
    let _ = write!(head, "Content-Length: {STALLED_CONTENT_LENGTH}\r\n\r\nx");
    head
}

/// Send [`stalled_request_head`] onto a connection the caller owns.
///
/// The blocking half of the same fixture. A root driving the stall from an async
/// peer writes the head itself, because its socket is Tokio's and this one is
/// the standard library's; both send the same bytes because both build them
/// here.
pub fn write_stalled_body(
    stream: &mut TcpStream,
    connection: Option<&str>,
    method: &str,
    path: &str,
) -> io::Result<()> {
    stream.write_all(stalled_request_head(connection, method, path).as_bytes())?;
    stream.flush()
}

/// Send one request with a body to a named authority, on its own connection.
///
/// [`request`] addresses every call to `localhost`, and [`request_to_host`]
/// frames no body — so a case that turns on the authority the peer sent *and*
/// carries a body, which is what host-routed body collection is, can be
/// expressed through neither.
pub fn request_to_host_with_body(
    addr: SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<HttpResponse> {
    let mut stream = connect(addr)?;
    let head = body_request_head(
        host,
        CLOSE_AFTER_RESPONSE,
        method,
        path,
        headers,
        BodyFraming::Length(body.len()),
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    read_http_response_bounded(&mut stream)
}

/// What an already-answered connection does with one more request on it.
///
/// `Some` is a connection that framed and answered the second request; `None`
/// is one the server ended, whether the peer learns it on the write or at end
/// of stream. A forced-close disposition is observable only this way: the
/// header a response carries states an intent, and what the transport then did
/// with the connection is the behavior itself.
pub fn probe_connection_reuse(
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> io::Result<Option<HttpResponse>> {
    let written =
        write_request_with_connection(stream, CLOSE_AFTER_RESPONSE, method, path, headers, body);
    match written {
        Ok(()) => {}
        Err(error) if is_closed_connection_error(&error) => return Ok(None),
        Err(error) => return Err(error),
    }
    match read_http_response_bounded(stream) {
        Ok(response) => Ok(Some(response)),
        Err(error)
            if is_closed_connection_error(&error)
                || error.kind() == io::ErrorKind::UnexpectedEof =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

/// The target an already-answered connection is asked to carry one more request
/// to.
///
/// Its route does not matter. What the assertion below reads is whether the
/// connection framed and answered anything at all; a server that answers a
/// target it has no route for has answered.
const REUSE_PROBE_TARGET: &str = "/reuse-probe";

/// The body that probe carries: small enough that any configured limit admits
/// it, so a refusal cannot stand in for a connection that was already gone.
const REUSE_PROBE_BODY: &[u8] = b"probe";

/// Assert a refusal that left payload unread also ended the connection.
///
/// Seven sites asserted this as `matches!(reused, Ok(None) | Err(_))`, which
/// passes on the `Err` arm — and that arm is a deadline expiry as readily as a
/// genuine transport fault. A server that hung instead of closing reads as the
/// proof that it closed. The expiry fails here instead, and only an answered
/// second request or a settled `None` decides the claim.
pub fn assert_connection_closed(peer: &mut TcpStream, label: &str) {
    let answered = probe_connection_reuse(peer, "POST", REUSE_PROBE_TARGET, &[], REUSE_PROBE_BODY)
        .unwrap_or_else(|error| panic!("{label}: the reuse probe did not settle: {error}"));
    assert!(
        answered.is_none(),
        "{label}: payload left unread must not leave the connection able to frame \
         another request: {answered:?}"
    );
}

/// Read the response head: every byte through the blank line that ends it.
///
/// A streaming response never reaches end of body while its producer holds the
/// sender, so the whole-response readers cannot be used to prove its head was
/// produced — and one `read` returns whatever a single syscall produced, which
/// TCP is free to split anywhere inside that head.
pub fn read_head(stream: &mut TcpStream, timeout: Duration) -> io::Result<Box<[u8]>> {
    read_delimited(stream, b"\r\n\r\n", MAX_HEADER_BYTES, timeout)
}

/// Read through `delimiter`, returning every byte up to and including it.
///
/// `timeout` is a deadline over the whole frame, not a bound on one syscall:
/// the read is byte-at-a-time, so a per-read timeout would give every byte a
/// fresh full budget and a peer dribbling bytes under it would never expire.
/// The socket's prior read timeout is restored, so the bound set here cannot
/// leak into the rest of a test's use of the connection.
pub fn read_delimited(
    stream: &mut TcpStream,
    delimiter: &[u8],
    limit: usize,
    timeout: Duration,
) -> io::Result<Box<[u8]>> {
    with_read_deadline(stream, timeout, |stream, deadline| {
        let mut bytes = Vec::new();
        let end = read_through(
            stream,
            &mut bytes,
            0,
            delimiter,
            limit,
            "framed read",
            Some(deadline),
        )?;
        Ok(bytes[..end].into())
    })
}

/// Read a stream to closure and hand back what it wrote, as text.
///
/// [`read_until_closed`] for the cases whose subject is the whole transport
/// rather than one parsed message: a committed stream that ends short, a request
/// line Hyper never accepted, a proxied answer that outlives the framed reader's
/// own deadline. Three roots wrote the same read-then-decode pair, so what
/// counts as the end of such an answer is stated once.
pub fn drain_to_close(stream: &mut TcpStream, timeout: Duration) -> io::Result<String> {
    read_until_closed(stream, timeout).map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
}

/// Await `future` under `bound`, failing the calling test when it expires.
///
/// Three roots wrote this wrapper. It is a hang guard, not a timing assertion: a
/// rendezvous that never arrives fails its own test at the bound instead of
/// parking the whole binary on it, and `operation` names what was being waited
/// for so the failure says which leg stalled.
pub async fn bounded<F: std::future::Future>(
    future: F,
    bound: Duration,
    operation: &str,
) -> F::Output {
    tokio::time::timeout(bound, future)
        .await
        .unwrap_or_else(|_| panic!("{operation} timed out after {bound:?}"))
}

/// Wait until production pauses at `checkpoint`, under a bound.
///
/// `LifecycleController::wait_until_paused` has no deadline of its own, so
/// production that never reaches the checkpoint parks the observer on it and the
/// case hangs instead of failing. Two roots wrote a bounded form; `context`
/// names the step, so an expired bound says which pause never arrived.
///
/// The bound comes off the runtime's timer, so a case driving a paused clock
/// needs a real-clock bound of its own rather than this.
pub async fn wait_until_paused_bounded(
    controller: &LifecycleController,
    checkpoint: LifecycleCheckpoint,
    context: &str,
) {
    bounded(
        controller.wait_until_paused(checkpoint),
        SETTLE_BOUND,
        context,
    )
    .await
    .unwrap_or_else(|error| panic!("{context}: the lifecycle checkpoint was not armed: {error}"));
}

/// [`bounded`], for a caller whose runtime clock is paused.
///
/// A paused runtime advances to the nearest armed timer whenever it has nothing
/// left to run. A single `timeout` armed before the subject has a deadline of
/// its own is therefore the only timer in the process, the clock jumps straight
/// to it, and it elapses having proved nothing — so a plain [`bounded`] fails a
/// perfectly healthy wait. Every idle park before the subject arms its own timer
/// can consume one arming that way.
///
/// The bound is armed `armings` times instead. Each arming after the one the
/// clock jumped to is measured against a deadline that does exist, is the nearer
/// timer, and fires first, so an elapse that exhausts them all says the subject
/// never settled. An arming spent on virtual time costs no real time.
///
/// The future is pinned once and carried across armings rather than rebuilt: a
/// cancelled read keeps the bytes it already took, and a fresh future per arming
/// would drop them. `subject` names what never settled.
pub async fn bounded_under_pause<F: std::future::Future>(
    future: F,
    bound: Duration,
    armings: usize,
    subject: &str,
) -> F::Output {
    let mut future = std::pin::pin!(future);
    for _ in 0..armings {
        match tokio::time::timeout(bound, &mut future).await {
            Ok(output) => return output,
            // The clock jumped to this arming rather than to the subject's own
            // deadline. The next one is measured against a deadline that now
            // exists.
            Err(_) => {}
        }
    }
    panic!("{subject} did not settle across {armings} armings of {bound:?}")
}

/// Assert one server owner joined on an end its owner asked for.
///
/// A server told to stop returns `Ok(())` or `Cancelled` — it ended because it
/// was told to, which is a completed join and not a failure. Every other outcome
/// is a fault, and discarding the result with `let _` reports none of them: a
/// server that panicked, refused, or never joined at all reads exactly like one
/// that shut down cleanly.
pub fn assert_server_joined(result: Result<Result<(), RuntimeError>, tokio::time::error::Elapsed>) {
    match result {
        Ok(Ok(())) | Ok(Err(RuntimeError::Cancelled)) => {}
        Ok(Err(error)) => panic!("the server owner failed rather than stopping: {error}"),
        Err(expiry) => panic!("the server owner never joined: {expiry}"),
    }
}

/// Read a stream to closure, returning everything that arrived before it.
///
/// An abortive close is a close: a peer that ends with `Connection: close` and
/// an RST reaches the reader as one of the gone-peer kinds — which one depends
/// on the platform — and treating any of them as a failure would fail a correct
/// teardown. One statement of that rule, bounded by one deadline over the whole
/// read.
pub fn read_until_closed(stream: &mut TcpStream, timeout: Duration) -> io::Result<Box<[u8]>> {
    with_read_deadline(stream, timeout, |stream, deadline| {
        let mut bytes = Vec::new();
        match read_to_eof(stream, &mut bytes, MAX_RESPONSE_BYTES, Some(deadline)) {
            Err(error) if is_closed_connection_error(&error) => Ok(()),
            result => result,
        }?;
        Ok(bytes.into_boxed_slice())
    })
}

/// One direction's socket timeout, as the pair of accessors that reads it and
/// arms it.
///
/// `TcpStream` states the read and write bounds as two identical method pairs,
/// so every helper that has to save, arm, and restore one of them would
/// otherwise be written once per direction. Naming the pair as a value leaves
/// one such helper for the whole suite.
#[derive(Clone, Copy)]
pub struct SocketTimeout {
    current: fn(&TcpStream) -> io::Result<Option<Duration>>,
    arm: fn(&TcpStream, Option<Duration>) -> io::Result<()>,
}

/// The read direction's timeout accessors.
pub const READ_TIMEOUT: SocketTimeout = SocketTimeout {
    current: TcpStream::read_timeout,
    arm: TcpStream::set_read_timeout,
};

/// The write direction's timeout accessors.
pub const WRITE_TIMEOUT: SocketTimeout = SocketTimeout {
    current: TcpStream::write_timeout,
    arm: TcpStream::set_write_timeout,
};

/// Run `operation` with `armed` in force on one direction of `stream`,
/// restoring the socket's prior bound however it ended.
///
/// The restore is what keeps a bound set for one exchange from leaking into the
/// rest of a test's use of the connection, and it has to run on the failure path
/// too — so it is stated once here rather than once per reader and writer. A
/// restore that itself fails is reported, but never in place of the operation's
/// own error. A caller driving a multi-read frame arms the whole frame's budget
/// here and narrows it per read from [`apply_deadline`].
pub fn with_socket_timeout<T, E>(
    stream: &mut TcpStream,
    timeout: SocketTimeout,
    armed: Option<Duration>,
    operation: impl FnOnce(&mut TcpStream) -> Result<T, E>,
) -> Result<T, E>
where
    E: From<io::Error>,
{
    let previous = (timeout.current)(stream)?;
    tolerate_dead_socket((timeout.arm)(stream, armed))?;
    let result = operation(stream);
    let restore = tolerate_dead_socket((timeout.arm)(stream, previous));
    match (result, restore) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(E::from(error)),
    }
}

/// Run one framed read against a deadline computed from `timeout`.
///
/// The whole frame's budget is armed once so no read can outlast it even if a
/// reader forgets to narrow it, and [`apply_deadline`] then hands each read only
/// what is left.
pub(crate) fn with_read_deadline<T>(
    stream: &mut TcpStream,
    timeout: Duration,
    read: impl FnOnce(&mut TcpStream, Instant) -> io::Result<T>,
) -> io::Result<T> {
    let deadline = Instant::now() + timeout;
    with_socket_timeout(stream, READ_TIMEOUT, Some(timeout), |stream| {
        read(stream, deadline)
    })
}

/// Whether `error` is a peer that has already gone away.
///
/// The four kinds a closed connection reaches a writer or reader as, named
/// once: every probe that treats "the peer is gone" as its expected outcome
/// reads the same set.
pub fn is_closed_connection_error(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionAborted
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::NotConnected
    )
}

/// Accept a socket deadline that could not be armed or restored because the
/// socket is already dead, and report every other failure.
///
/// macOS refuses `setsockopt` with `EINVAL` once both directions of a socket
/// are shut down, so a peer that goes away mid-exchange turns the next arm or
/// restore into an error on that platform and on no other. Reporting it would
/// fail a probe for the very outcome it expects. Dropping the bound is safe
/// where the bound is: a dead socket answers immediately, so the I/O this
/// guards cannot hang, and its own closed-connection error stays the verdict
/// the caller reads.
pub fn tolerate_dead_socket(result: io::Result<()>) -> io::Result<()> {
    /// `EINVAL`, named rather than depending on `libc` for one integer.
    const INVALID_ARGUMENT: i32 = 22;
    match result {
        Err(error) if error.raw_os_error() == Some(INVALID_ARGUMENT) => Ok(()),
        result => result,
    }
}

/// Accept a write the peer's own answer ended, and report every other failure.
///
/// A refusal decided partway through a body closes the connection, so the frames
/// still in flight reach a socket the peer has already ended. That is the answer
/// arriving early rather than a fault, and the answer the caller reads next is
/// the verdict either way. The peer being gone reaches a writer as one of the
/// kinds [`is_closed_connection_error`] names — never as the `EINVAL` a socket
/// deadline fails with, which is why tolerating only that one would panic the
/// very rows this covers.
pub fn tolerate_answered_peer(result: io::Result<()>) -> io::Result<()> {
    match result {
        Err(error) if is_closed_connection_error(&error) => Ok(()),
        result => tolerate_dead_socket(result),
    }
}

/// Read one whole HTTP response, bounded by `deadline`.
///
/// `deadline` covers the head, the body, and every chunk between them, because
/// a per-read bound would give each byte a fresh full budget: a peer that stalls
/// mid-chunk under one would park the calling test with nothing to expire. A
/// caller arms it through [`with_read_deadline`], which restores the socket's
/// prior bound afterwards. `None` reads with whatever bound the socket already
/// carries, for a caller that owns its own.
///
/// Most callers want [`read_http_response_bounded`] instead: it arms the same
/// budget [`connect`] already put on the socket, so the frame deadline and the
/// socket bound stay one number rather than two that can drift apart.
pub fn read_http_response(
    stream: &mut TcpStream,
    deadline: Option<Instant>,
) -> io::Result<HttpResponse> {
    let mut raw = Vec::new();
    let header_end = read_through(
        stream,
        &mut raw,
        0,
        b"\r\n\r\n",
        MAX_HEADER_BYTES,
        "response headers",
        deadline,
    )?;
    let (status, headers) = parse_head(&raw[..header_end])?;
    let body: Box<[u8]> = match response_body_kind(status, &headers)? {
        BodyKind::None => Vec::new().into_boxed_slice(),
        BodyKind::Length(length) => {
            let body_end = header_end
                .checked_add(length)
                .ok_or_else(|| invalid_data("response length overflowed"))?;
            read_to_length(
                stream,
                &mut raw,
                body_end,
                MAX_RESPONSE_BYTES,
                "response body",
                deadline,
            )?;
            raw[header_end..body_end].into()
        }
        BodyKind::Chunked => read_chunked_body(stream, &mut raw, header_end, deadline)?,
        BodyKind::Eof => {
            read_to_eof(stream, &mut raw, MAX_RESPONSE_BYTES, deadline)?;
            raw[header_end..].into()
        }
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
        raw: raw.into_boxed_slice(),
    })
}

/// Read one whole response under the same budget [`connect`] arms on the socket.
///
/// The frame deadline and the socket's own read timeout are the same number, so
/// stating it once is what keeps them from drifting. Every caller that connects
/// through this module's helpers wants this form; the explicit-deadline
/// [`read_http_response`] is for a caller holding a budget of its own, such as a
/// fixture measuring one step of a longer journey.
pub fn read_http_response_bounded(stream: &mut TcpStream) -> io::Result<HttpResponse> {
    read_http_response(stream, Some(Instant::now() + IO_TIMEOUT))
}

enum BodyKind {
    None,
    Length(usize),
    Chunked,
    Eof,
}

fn response_body_kind(status: u16, headers: &[(Box<str>, Box<str>)]) -> io::Result<BodyKind> {
    if (100..200).contains(&status) || matches!(status, 204 | 304) {
        return Ok(BodyKind::None);
    }
    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && value
                .split(',')
                .any(|encoding| encoding.trim().eq_ignore_ascii_case("chunked"))
    });
    if chunked {
        return Ok(BodyKind::Chunked);
    }
    let lengths = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .map(|(_, value)| value.parse::<usize>())
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| invalid_data(format!("invalid content length: {error}")))?;
    match lengths.as_slice() {
        [] => Ok(BodyKind::Eof),
        [length, rest @ ..] if rest.iter().all(|candidate| candidate == length) => {
            validated_body_length(*length)
        }
        _ => Err(invalid_data(
            "response contained conflicting content lengths",
        )),
    }
}

fn validated_body_length(length: usize) -> io::Result<BodyKind> {
    match length <= MAX_BODY_BYTES {
        true => Ok(BodyKind::Length(length)),
        false => Err(invalid_data("response body exceeded size limit")),
    }
}

fn parse_head(bytes: &[u8]) -> io::Result<(u16, Box<[(Box<str>, Box<str>)]>)> {
    let head = std::str::from_utf8(bytes)
        .map_err(|error| invalid_data(format!("response head was not UTF-8: {error}")))?;
    let mut lines = head.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| invalid_data("response did not contain a valid status"))?;
    let headers = lines
        .take_while(|line| !line.is_empty())
        .map(|line| {
            let (name, value) = line
                .split_once(':')
                .ok_or_else(|| invalid_data("response contained a malformed header"))?;
            Ok((name.into(), value.trim().into()))
        })
        .collect::<io::Result<Vec<_>>>()?
        .into_boxed_slice();
    Ok((status, headers))
}

fn read_chunked_body(
    stream: &mut TcpStream,
    raw: &mut Vec<u8>,
    mut cursor: usize,
    deadline: Option<Instant>,
) -> io::Result<Box<[u8]>> {
    let mut body = Vec::new();
    loop {
        let line_end = read_through(
            stream,
            raw,
            cursor,
            b"\r\n",
            MAX_RESPONSE_BYTES,
            "chunk size line",
            deadline,
        )?;
        let size_end = line_end
            .checked_sub(2)
            .ok_or_else(|| invalid_data("chunk size framing underflowed"))?;
        let size_line = std::str::from_utf8(&raw[cursor..size_end])
            .map_err(|error| invalid_data(format!("chunk size was not UTF-8: {error}")))?;
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or_default(), 16)
            .map_err(|error| invalid_data(format!("invalid chunk size: {error}")))?;
        cursor = line_end;
        if size == 0 {
            read_chunk_trailers(stream, raw, cursor, deadline)?;
            return Ok(body.into_boxed_slice());
        }
        let body_end = body
            .len()
            .checked_add(size)
            .ok_or_else(|| invalid_data("chunk body length overflowed"))?;
        if body_end > MAX_BODY_BYTES {
            return Err(invalid_data("response body exceeded size limit"));
        }
        let payload_end = cursor
            .checked_add(size)
            .ok_or_else(|| invalid_data("chunk payload length overflowed"))?;
        let framed_end = payload_end
            .checked_add(2)
            .ok_or_else(|| invalid_data("chunk framing length overflowed"))?;
        read_to_length(
            stream,
            raw,
            framed_end,
            MAX_RESPONSE_BYTES,
            "chunk payload",
            deadline,
        )?;
        body.extend_from_slice(&raw[cursor..payload_end]);
        if &raw[payload_end..framed_end] != b"\r\n" {
            return Err(invalid_data("chunk did not end with CRLF"));
        }
        cursor = framed_end;
    }
}

fn read_chunk_trailers(
    stream: &mut TcpStream,
    raw: &mut Vec<u8>,
    mut cursor: usize,
    deadline: Option<Instant>,
) -> io::Result<()> {
    loop {
        let trailer_end = read_through(
            stream,
            raw,
            cursor,
            b"\r\n",
            MAX_RESPONSE_BYTES,
            "chunk trailer",
            deadline,
        )?;
        let empty_line_end = cursor
            .checked_add(2)
            .ok_or_else(|| invalid_data("chunk trailer length overflowed"))?;
        cursor = trailer_end;
        if trailer_end == empty_line_end {
            return Ok(());
        }
    }
}

/// Read from `start` until `delimiter` appears, naming what is being read in
/// every failure.
///
/// `subject` is what the caller was framing — response headers, a chunk size
/// line — so a size or overflow failure reports the read that actually hit it
/// rather than the one this helper was first written for. `start` is where the
/// frame begins in a buffer the caller is filling across several frames: a
/// chunked body reads one line after another out of the same `Vec`, and a
/// delimiter left behind by the previous frame is not this one's.
fn read_through(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    start: usize,
    delimiter: &[u8],
    limit: usize,
    subject: &str,
    deadline: Option<Instant>,
) -> io::Result<usize> {
    // Where the last search ended. `read_one` appends one byte, so rescanning
    // the whole frame per byte would cost the square of it; resuming one
    // delimiter short of the end covers a delimiter that straddles the previous
    // read and nothing more.
    let overlap = delimiter.len().saturating_sub(1);
    let mut scanned = start;
    loop {
        let from = scanned.saturating_sub(overlap).max(start);
        if let Some(position) = bytes[from..]
            .windows(delimiter.len())
            .position(|part| part == delimiter)
        {
            return from
                .checked_add(position)
                .and_then(|end| end.checked_add(delimiter.len()))
                .ok_or_else(|| invalid_data(format!("{subject} length overflowed")));
        }
        scanned = bytes.len().max(start);
        if bytes.len() >= limit {
            return Err(invalid_data(format!(
                "{subject} exceeded the {limit}-byte size limit"
            )));
        }
        read_one(stream, bytes, deadline)?;
    }
}

/// Fill `bytes` until it reaches `expected`, bounded by `deadline`.
///
/// The counted read every framed protocol is built from: an HTTP body of known
/// length, one chunk and its CRLF, a WebSocket header, mask key, or payload.
/// `deadline` bounds the whole fill rather than one syscall, so a peer dribbling
/// bytes under a per-read timeout still expires.
pub(crate) fn read_to_length(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    expected: usize,
    limit: usize,
    subject: &str,
    deadline: Option<Instant>,
) -> io::Result<()> {
    if expected > limit {
        return Err(invalid_data(format!(
            "{subject} exceeded the {limit}-byte size limit"
        )));
    }
    while bytes.len() < expected {
        let remaining = expected - bytes.len();
        let mut chunk = [0_u8; 4096];
        let read_limit = remaining.min(chunk.len());
        apply_deadline(stream, deadline)?;
        let count = attribute_deadline(stream.read(&mut chunk[..read_limit]), deadline)?;
        if count == 0 {
            return Err(io::Error::from(io::ErrorKind::UnexpectedEof));
        }
        bytes.extend_from_slice(&chunk[..count]);
    }
    Ok(())
}

/// Read until the peer closes, bounded by `deadline` and capped at `limit`.
///
/// The drain-to-close every reader that takes "everything the peer sent" is
/// built from: a body with no length, a whole response after `Connection:
/// close`, a stream read to its end. The only `InvalidData` it produces is the
/// size cap, which is what lets a caller with its own error type tell an
/// overflow apart from a transport failure.
pub(crate) fn read_to_eof(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    limit: usize,
    deadline: Option<Instant>,
) -> io::Result<()> {
    let mut chunk = [0_u8; 4096];
    loop {
        apply_deadline(stream, deadline)?;
        match attribute_deadline(stream.read(&mut chunk), deadline)? {
            0 => return Ok(()),
            count
                if bytes
                    .len()
                    .checked_add(count)
                    .is_some_and(|length| length <= limit) =>
            {
                bytes.extend_from_slice(&chunk[..count]);
            }
            _ => return Err(invalid_data("response exceeded size limit")),
        }
    }
}

fn read_one(
    stream: &mut TcpStream,
    bytes: &mut Vec<u8>,
    deadline: Option<Instant>,
) -> io::Result<()> {
    apply_deadline(stream, deadline)?;
    let mut byte = [0_u8; 1];
    match attribute_deadline(stream.read(&mut byte), deadline)? {
        0 => Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
        _ => {
            bytes.push(byte[0]);
            Ok(())
        }
    }
}

/// Give the next read only what is left of `deadline`.
///
/// This is what makes a multi-read frame bounded as a whole: the socket's own
/// timeout applies per syscall, so it is recomputed before each one. A deadline
/// already spent fails here rather than arming a zero timeout, which the
/// platform reads as no timeout at all.
fn apply_deadline(stream: &mut TcpStream, deadline: Option<Instant>) -> io::Result<()> {
    let left = match deadline {
        None => return Ok(()),
        Some(deadline) => remaining(deadline),
    };
    match left.is_zero() {
        true => Err(deadline_expired()),
        false => tolerate_dead_socket(stream.set_read_timeout(Some(left))),
    }
}

/// Whether `error` is a read that ran out of time rather than a broken
/// transport.
///
/// A read the socket's own bound cut short surfaces as `WouldBlock` on Unix and
/// `TimedOut` on Windows, and a deadline this module reports as spent uses the
/// second of those. One statement of the pair, so every probe that has to tell
/// its own bound apart from a peer that broke reads the same set.
pub fn is_deadline_expiry(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    )
}

/// Report a read the socket's own bound cut short as the frame's deadline
/// expiring.
///
/// [`apply_deadline`] arms exactly what is left of the frame's deadline, so a
/// read that expires at the socket level expired against that deadline — and the
/// platform names that `WouldBlock` on Unix and `TimedOut` on Windows. Both are
/// reported as the deadline's own failure, so one cause reaches the caller as
/// one verdict on every platform. A read taken without a deadline keeps whatever
/// the platform said.
fn attribute_deadline(result: io::Result<usize>, deadline: Option<Instant>) -> io::Result<usize> {
    match result {
        Err(error) if deadline.is_some() && is_deadline_expiry(&error) => Err(deadline_expired()),
        result => result,
    }
}

/// The one verdict a framed read that ran out of time reports.
fn deadline_expired() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "framed read exceeded its deadline")
}

fn invalid_data(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
