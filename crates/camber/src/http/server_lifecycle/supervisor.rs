//! The one task that admits sockets into a server's owner tree, and settles it.
//!
//! It owns connections and nothing beneath them. A request, an upgrade, or a
//! direction is reached through the connection that contains it, so the only
//! registry here is [`OwnedConnections`] and the only settlement it publishes is
//! a connection's.

use std::future::{Future, IntoFuture};
use std::ops::ControlFlow;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::FutureExt;

use super::super::BufferConfig;
use super::super::mock::{
    ConnectionOwnerEdge, ConnectionOwnershipEvent, LifecycleScript, ServerStopEdge,
    ServerTaskFault, SupervisorJoinProbe,
};
use super::super::router::ServerDispatch;
use super::super::server_stop::{ServerStopState, StopEvent, StopOutcome, StopPhase};
use super::connections::{
    ConnectionAdmission, ConnectionCompletion, ConnectionLifecycle, ConnectionPermit,
    OwnedConnections, UpgradeRetention, connection_disposition,
};
use super::control::{
    ServerControl, StopAuthority, wait_control, wait_deadline, wait_for_script_wake, wait_runtime,
};
use crate::lifecycle::{AggregateShutdown, FORCED_JOIN_GRACE, ShutdownOwner};
use crate::runtime_state::{RuntimeInner, ShutdownSignal, carry_runtime};
use crate::runtime_test_support::ParticipantDisposition;
use crate::task::{AsyncJoinFuture, panic_to_error};
use crate::{RuntimeError, runtime};

const OWNED_TASK_PANIC: &str = "injected owned HTTP task panic";
const SUPERVISOR_PROBE_PANIC: &str = "supervisor join probe panic";
const TRANSIENT_ACCEPT_BACKOFF: Duration = Duration::from_millis(100);

/// What a server was started under, classified once at construction.
///
/// The runtime is the only server-scoped value stored: every timeout, limit,
/// and observability handle the server needs is read back out of it on demand,
/// so there is one answer to each question rather than a stored copy that can
/// disagree with its source. `None` is the standalone capture, and stays
/// `None`. Only the router's buffer sizes and the TLS flag are independent of
/// the runtime, so they are the only other fields.
pub(in crate::http) struct ServerContextSnapshot {
    runtime: Option<Arc<RuntimeInner>>,
    buffers: BufferConfig,
    is_tls: bool,
    policy: super::super::ServerPolicy,
}

impl ServerContextSnapshot {
    /// Capture the serving context, and resolve the policy this server serves
    /// under, at the caller's exact instant.
    ///
    /// Both halves are answered here, together, because they are one decision:
    /// the runtime that is current when a terminal `serve*` call runs is the
    /// runtime whose policy contains this server's. Resolving either later — at
    /// first poll of a returned future, or from a fresh lookup inside a spawned
    /// task — would let ambient context that changed in between choose a
    /// different authority than the caller had.
    pub(in crate::http) fn capture(
        buffers: BufferConfig,
        policy: super::super::ServerPolicy,
    ) -> Self {
        let runtime = runtime::try_current_runtime();
        let policy = match runtime.as_ref() {
            Some(runtime) => policy.narrowed_by(runtime.config.server_policy),
            None => policy,
        };
        Self {
            runtime,
            buffers,
            is_tls: false,
            policy,
        }
    }

    /// The TLS configuration the captured runtime supplies, if any.
    ///
    /// Read from the runtime this snapshot already captured, so the runtime
    /// that supplies a server's policy is the runtime that supplies its
    /// certificate.
    pub(in crate::http) fn runtime_tls(&self) -> Option<Arc<rustls::ServerConfig>> {
        self.runtime
            .as_ref()
            .and_then(|runtime| runtime.config.tls_config.clone())
    }

    /// Record whether this server's transport is TLS.
    ///
    /// Separate from capture because the answer depends on the runtime the
    /// capture resolved: a builder without its own certificate inherits the
    /// runtime's.
    pub(in crate::http) fn with_tls(self, is_tls: bool) -> Self {
        Self { is_tls, ..self }
    }

    fn runtime_shutdown(&self) -> Option<ShutdownSignal> {
        self.runtime
            .as_ref()
            .map(|runtime| runtime.shutdown_signal())
    }

    /// The aggregate shutdown this server stops against.
    ///
    /// The runtime's when there is one, so a server inside a runtime narrows the
    /// same expiry every other participant reads instead of starting its own
    /// copy of the grace. A standalone server owns the only shutdown there is,
    /// so it mints one over its own configured grace.
    fn shutdown_deadline(&self) -> Arc<AggregateShutdown> {
        match self.runtime.as_ref() {
            Some(runtime) => runtime.shutdown_deadline(),
            None => AggregateShutdown::new(self.shutdown_timeout(), None),
        }
    }

    fn shutdown_timeout(&self) -> Duration {
        self.policy.shutdown_timeout_value()
    }

    fn header_timeout(&self) -> Duration {
        self.policy.header_timeout_value()
    }

    fn connection_limit(&self) -> Option<usize> {
        self.policy.connection_limit_value()
    }

    fn connection_context(&self) -> super::super::handle::ConnCtx {
        match &self.runtime {
            Some(runtime) => super::super::handle::ConnCtx::from_runtime(
                runtime,
                self.buffers,
                self.is_tls,
                self.policy,
            ),
            None => self.standalone_context(),
        }
    }

    /// The context a server started outside a Camber runtime serves with: the
    /// router's own buffer limits and the transport's TLS state, and nothing
    /// that could only have come from a runtime.
    fn standalone_context(&self) -> super::super::handle::ConnCtx {
        super::super::handle::ConnCtx {
            tracing_enabled: false,
            metrics_handle: None,
            #[cfg(feature = "profiling")]
            profiling_enabled: false,
            sse_buffer_size: self.buffers.sse_buffer_size,
            #[cfg(feature = "ws")]
            ws_buffer_size: self.buffers.ws_buffer_size,
            resources: None,
            is_tls: self.is_tls,
            policy: self.policy,
        }
    }
}

struct PendingAccepted {
    stream: crate::net::AcceptedStream,
    remote_addr: Option<std::net::SocketAddr>,
}

enum SupervisorEvent {
    ScriptWake,
    Deadline,
    Control(ServerControl),
    Runtime(tokio::time::Instant),
    Accept(Result<(crate::net::AcceptedStream, Option<std::net::SocketAddr>), std::io::Error>),
    Permit(Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError>),
    Connection(Option<ConnectionCompletion>),
}

pub(in crate::http) struct ServerSupervisor {
    listener: Option<crate::net::Listener>,
    /// What removing the listener's socket path reported, when it failed.
    ///
    /// Held until the terminal result is taken, because that is the only place
    /// one error can be chosen: the supervisor is the last owner of the
    /// listener, so a failure here has no caller left to reach except the one
    /// waiting on how this server ended.
    cleanup_failure: Option<RuntimeError>,
    dispatch: Arc<ServerDispatch>,
    context: Arc<super::super::handle::ConnCtx>,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    header_timeout: Duration,
    shutdown_timeout: Duration,
    /// The one aggregate expiry this server and every other participant share.
    ///
    /// Held beside `shutdown_timeout` rather than replacing it: the configured
    /// grace is this server's own bound, and the aggregate is the outer one it
    /// may narrow but never outlive.
    shutdown: Arc<AggregateShutdown>,
    /// The one causal stop state this server, its handle, and its connection
    /// owners share.
    ///
    /// It holds the control phase, the first server-fatal fact, and the
    /// immutable flat result. The supervisor no longer decides any of the
    /// three; it applies what has already committed and settles the drain.
    stop: Arc<ServerStopState>,
    /// The committed phase this supervisor has already acted on.
    ///
    /// The phase says what the server has decided; this says how far the
    /// supervisor's own admission, deadline, and task disposition have caught
    /// up with it. Without it, every wake would close admission again.
    applied: StopPhase,
    runtime_shutdown: Option<ShutdownSignal>,
    runtime: Option<Arc<RuntimeInner>>,
    connection_limit: Option<Arc<tokio::sync::Semaphore>>,
    pending: Option<PendingAccepted>,
    control_sender: tokio::sync::watch::Sender<ServerControl>,
    control_receiver: tokio::sync::watch::Receiver<ServerControl>,
    connections: OwnedConnections,
    deadline: Option<tokio::time::Instant>,
    abort_started: bool,
    script: Option<Arc<LifecycleScript>>,
}

impl ServerSupervisor {
    pub(in crate::http) fn new(
        listener: crate::net::Listener,
        dispatch: ServerDispatch,
        tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
        snapshot: ServerContextSnapshot,
    ) -> (Self, StopAuthority) {
        let script = listener
            .tcp_addr()
            .and_then(super::super::mock::lifecycle_script);
        let (control_sender, control_receiver) =
            tokio::sync::watch::channel(ServerControl::Running);
        let owner_control = control_sender.clone();
        let connection_limit = super::super::server::make_conn_limit(snapshot.connection_limit());
        let context = Arc::new(snapshot.connection_context());
        let header_timeout = snapshot.header_timeout();
        let shutdown_timeout = snapshot.shutdown_timeout();
        let shutdown = snapshot.shutdown_deadline();
        let runtime_shutdown = snapshot.runtime_shutdown();
        let stop = ServerStopState::new(Arc::clone(&shutdown), script.clone());
        let authority = StopAuthority::new(owner_control, Arc::clone(&stop));
        (
            Self {
                listener: Some(listener),
                cleanup_failure: None,
                dispatch: Arc::new(dispatch),
                context,
                tls_acceptor,
                header_timeout,
                shutdown_timeout,
                shutdown,
                stop,
                applied: StopPhase::Running,
                runtime_shutdown,
                runtime: snapshot.runtime,
                connection_limit,
                pending: None,
                control_sender,
                control_receiver,
                connections: OwnedConnections::new(),
                deadline: None,
                abort_started: false,
                script,
            },
            authority,
        )
    }

    /// Whether this server was started inside a Camber runtime.
    pub(in crate::http) fn is_camber(&self) -> bool {
        self.runtime.is_some()
    }

    fn is_cancelled(&self) -> bool {
        self.stop.cancel_commanded()
    }

    /// Whether this supervisor has applied one of the two forced phases.
    ///
    /// Deliberately the APPLIED phase and not the committed one. A command
    /// commits before the supervisor is woken, so a committed forced phase this
    /// supervisor has not acted on yet is a server that has not closed
    /// admission, rejected its pending sockets, or told its tasks anything. A
    /// drain predicate that read the commit would declare that work finished
    /// before it had been asked to end.
    fn applied_forced(&self) -> bool {
        matches!(self.applied, StopPhase::Cancelled | StopPhase::TimedOut)
    }

    pub(in crate::http) async fn run(mut self) -> Result<(), RuntimeError> {
        let result = AssertUnwindSafe(self.run_core()).catch_unwind().await;
        match result {
            Ok(result) => result,
            Err(payload) => {
                // The supervisor itself is the owner that went away, so the
                // forced phase is committed the same way an abandoned handle
                // commits it. The panic is what this call returns; the state's
                // own result is never read on this path.
                self.stop.apply(StopEvent::Abandon);
                self.applied = StopPhase::Cancelled;
                self.begin_abort().await;
                self.drain_owned_after_panic().await;
                Err(panic_to_error(payload))
            }
        }
    }

    async fn run_core(&mut self) -> Result<(), RuntimeError> {
        loop {
            match self.step().await {
                ControlFlow::Break(result) => return result,
                ControlFlow::Continue(()) => {}
            }
        }
    }

    /// One supervisor pass: settle a drain that has finished, or take the next
    /// event and apply it.
    async fn step(&mut self) -> ControlFlow<Result<(), RuntimeError>> {
        self.start_abort_if_ready();
        if self.abort_drain_complete() {
            return ControlFlow::Break(self.finish().await);
        }
        if self.graceful_drain_complete() {
            return ControlFlow::Break(self.finish().await);
        }
        LifecycleScript::pause_at_stop(
            self.script.as_deref(),
            ServerStopEdge::BeforeSupervisorSelect,
        )
        .await;
        self.raise_supervisor_fault();
        let event = self.select_event().await;
        self.apply_event(event).await;
        ControlFlow::Continue(())
    }

    /// Whether a graceful drain has nothing left to wait for.
    ///
    /// One question, because there is one registry: every request, upgrade, and
    /// direction this server admitted is reached through a connection, so an
    /// empty connection set is the whole drain.
    fn graceful_drain_complete(&self) -> bool {
        self.applied == StopPhase::Graceful && self.connections.is_empty()
    }

    /// Take the next event, and pause at the checkpoint it selected.
    ///
    /// The pause happens here rather than inside a `select!` arm so that it
    /// always runs against a committed choice: an arm that awaits before the
    /// outer race has resolved could have its already-selected event dropped
    /// when a later arm becomes ready.
    async fn select_event(&mut self) -> SupervisorEvent {
        let event = match self.pending.is_some() {
            true => self.select_pending_event().await,
            false => self.select_listener_event().await,
        };
        pause_selected(self.script.as_deref(), &event).await;
        event
    }

    /// Race the listener against the events every selector observes.
    ///
    /// The listener is the whole admission guard: it is taken by the same
    /// `close_admission` step that leaves `Running`, so asking the mode as well
    /// would store one fact twice. The guard cannot be dropped, though —
    /// `accept_next` answers an injected accept fault before it looks at the
    /// listener, so an unguarded arm would keep synthesising accept errors
    /// after admission closed.
    async fn select_listener_event(&mut self) -> SupervisorEvent {
        let listener = self.listener.as_ref();
        tokio::select! {
            biased;
            event = select_lifecycle_event(self.deadline, &mut self.control_receiver, self.runtime_shutdown.as_ref(), self.script.as_deref()) => event,
            accepted = accept_next(listener, self.script.as_deref()), if listener.is_some() => {
                SupervisorEvent::Accept(accepted)
            }
            event = select_owned_connection_event(&mut self.connections, self.script.as_deref()) => event,
        }
    }

    /// Race the connection limit against the events every selector observes.
    ///
    /// The permit arm needs no guard of its own: `acquire_connection_permit`
    /// answers an absent limit with `pending()` before it does anything else.
    async fn select_pending_event(&mut self) -> SupervisorEvent {
        tokio::select! {
            biased;
            event = select_lifecycle_event(self.deadline, &mut self.control_receiver, self.runtime_shutdown.as_ref(), self.script.as_deref()) => event,
            permit = crate::net::accept::acquire_connection_permit(self.connection_limit.as_ref(), self.script.as_deref()) => {
                SupervisorEvent::Permit(permit)
            }
            event = select_owned_connection_event(&mut self.connections, self.script.as_deref()) => event,
        }
    }

    async fn apply_event(&mut self, event: SupervisorEvent) {
        match event {
            SupervisorEvent::Deadline => self.handle_deadline().await,
            // A published control transition carries no decision of its own:
            // the command committed its phase before sending, so the wake only
            // says there is a committed phase to catch up with.
            SupervisorEvent::Control(
                ServerControl::Abort | ServerControl::Graceful | ServerControl::Running,
            ) => self.apply_committed_phase().await,
            SupervisorEvent::Runtime(selected_at) => {
                self.runtime_shutdown = None;
                self.commit_graceful_at(selected_at).await;
            }
            SupervisorEvent::ScriptWake | SupervisorEvent::Connection(None) => {}
            SupervisorEvent::Accept(result) => self.handle_accept(result).await,
            SupervisorEvent::Permit(result) => self.handle_permit(result).await,
            SupervisorEvent::Connection(Some(completion)) => {
                self.handle_connection_completion(completion).await;
            }
        }
    }

    /// Answer the shutdown deadline: commit the timeout a graceful drain
    /// expired into, or — already forced — take every remaining connection down
    /// where it stands.
    ///
    /// The forced arm is why an abort carries a deadline at all. A connection's
    /// answer to abort is a protocol-level graceful shutdown, so one serving a
    /// long-lived response or holding an upgrade child keeps its permit for as
    /// long as that work lives, and waiting for it would never end.
    async fn handle_deadline(&mut self) {
        match self.applied_forced() {
            true => self.force_abort(),
            false => {
                self.stop.commit(StopEvent::DeadlineExpiry).await;
                self.apply_committed_phase().await;
            }
        }
    }

    /// Bring this supervisor's admission, deadline, and task disposition into
    /// line with the phase the stop state has committed.
    ///
    /// The one place a committed phase turns into supervisor action, so the
    /// handle, the runtime signal, the aggregate deadline, and a fatal fact all
    /// reach the same effects through one path and cannot disagree about which
    /// of them close admission. Repeats are free: a phase already applied does
    /// nothing.
    async fn apply_committed_phase(&mut self) {
        let committed = self.stop.phase();
        match (self.applied, committed) {
            (StopPhase::Running, StopPhase::Graceful) => {
                self.applied = StopPhase::Graceful;
                self.enter_graceful().await;
            }
            // `Finished` is not a phase to catch up with: only this supervisor
            // settles, and it has already applied whatever it settled from.
            (StopPhase::Running | StopPhase::Graceful, forced @ StopPhase::Cancelled)
            | (StopPhase::Running | StopPhase::Graceful, forced @ StopPhase::TimedOut) => {
                self.applied = forced;
                self.begin_abort().await;
            }
            _ => {}
        }
    }

    async fn handle_accept(
        &mut self,
        result: Result<(crate::net::AcceptedStream, Option<std::net::SocketAddr>), std::io::Error>,
    ) {
        match result {
            Ok((stream, remote_addr)) => self.handle_accepted(stream, remote_addr).await,
            Err(error) if crate::error::is_transient_accept_error(&error) => {
                tracing::warn!(error = %error, "accept: fd limit reached, backing off");
                // EMFILE/ENFILE retry throttling is product behavior, not an
                // ordering barrier; immediate retries would spin the runtime.
                tokio::time::sleep(TRANSIENT_ACCEPT_BACKOFF).await;
            }
            Err(error) => self.report_fatal(RuntimeError::Io(error)).await,
        }
    }

    /// Take a freshly accepted socket to whichever step answers the connection
    /// limit: parking it to wait for a permit, or serving it straight away.
    ///
    /// A limited socket is only admitted, not serviced — admission is asked
    /// again on the far side of the permit wait, which is a real suspension the
    /// runtime shutdown signal can fire across — so the admission question is
    /// asked there rather than carried across it.
    async fn handle_accepted(
        &mut self,
        stream: crate::net::AcceptedStream,
        remote_addr: Option<std::net::SocketAddr>,
    ) {
        LifecycleScript::pause_at_connection(
            self.script.as_deref(),
            ConnectionOwnerEdge::AfterAccept,
        )
        .await;
        match self.connection_limit {
            Some(_) => self.park_for_permit(stream, remote_addr).await,
            None => self.admit_and_spawn(stream, remote_addr, None).await,
        }
    }

    /// Hold an admitted socket until the connection limit has a permit for it.
    async fn park_for_permit(
        &mut self,
        stream: crate::net::AcceptedStream,
        remote_addr: Option<std::net::SocketAddr>,
    ) {
        if let Some(stream) = self.admit_or_close(stream).await {
            self.pending = Some(PendingAccepted {
                stream,
                remote_addr,
            });
        }
    }

    /// Hand back an accepted socket only while admission is open, and close the
    /// socket otherwise.
    ///
    /// Every refusal of an accepted socket goes through here, so a socket this
    /// server will not serve is always shut down rather than dropped raw.
    async fn admit_or_close(
        &self,
        stream: crate::net::AcceptedStream,
    ) -> Option<crate::net::AcceptedStream> {
        match self.admission_is_open() {
            true => Some(stream),
            false => {
                stream.close().await;
                None
            }
        }
    }

    /// Serve a socket that has answered the connection limit, if admission is
    /// still open for it.
    ///
    /// Both ways of answering that limit end here — an unlimited server, and
    /// one that waited for a permit — so the recheck a limit wait requires is
    /// also the only admission check either path makes, and the `AfterPermit`
    /// checkpoint marks the same moment for both.
    async fn admit_and_spawn(
        &mut self,
        stream: crate::net::AcceptedStream,
        remote_addr: Option<std::net::SocketAddr>,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) {
        LifecycleScript::pause_at_connection(
            self.script.as_deref(),
            ConnectionOwnerEdge::AfterPermit,
        )
        .await;
        if let Some(stream) = self.admit_or_close(stream).await {
            self.spawn_connection(stream, remote_addr, permit).await;
        }
    }

    async fn handle_permit(
        &mut self,
        result: Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError>,
    ) {
        let accepted = self.pending.take();
        match (result, accepted) {
            (Ok(permit), Some(accepted)) => {
                self.admit_and_spawn(accepted.stream, accepted.remote_addr, Some(permit))
                    .await;
            }
            (Ok(_), None) => {}
            (Err(error), accepted) => refuse_permit(&error, accepted).await,
        }
    }

    async fn spawn_connection(
        &mut self,
        stream: crate::net::AcceptedStream,
        remote_addr: Option<std::net::SocketAddr>,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) {
        LifecycleScript::pause_at_connection(
            self.script.as_deref(),
            ConnectionOwnerEdge::HeaderTimeoutConfigured(self.header_timeout),
        )
        .await;
        // One subscription is this connection's whole shutdown authority: the
        // lifecycle carries it for the upgrade path and the serve path takes it
        // by value, so no reader has to ask a second time or find it missing.
        let control = self.control_sender.subscribe();
        let identity = LifecycleScript::mint_owner_identity(self.script.as_deref());
        let lifecycle = ConnectionLifecycle::owned(ConnectionAdmission {
            identity,
            permit: ConnectionPermit::new(permit),
            control: control.clone(),
            shutdown: super::control::ConnectionShutdownDeadline::new(
                Some(Arc::clone(&self.shutdown)),
                self.shutdown_timeout,
            ),
            stop: Arc::clone(&self.stop),
            script: self.script.clone(),
        });
        let upgrade = UpgradeRetention::new();
        let state = super::super::conn::ConnectionState::new(
            Arc::clone(&self.dispatch),
            Arc::clone(&self.context),
            lifecycle,
            self.header_timeout,
            remote_addr.map(|addr| addr.ip()),
        );
        let future = super::super::conn::serve_owned_connection(
            stream,
            self.tls_acceptor.clone(),
            state,
            control,
            #[cfg(feature = "ws")]
            Arc::clone(&upgrade),
        );
        let fault = self
            .script
            .as_ref()
            .and_then(|script| script.take_owned_task_fault());
        // A detached connection task inherits no runtime context, so the one
        // this server was started under is carried in. It comes from the
        // snapshot rather than from a fresh lookup here, so every connection
        // carries the same runtime the rest of the server context was read
        // from. Handlers served by an owned server keep that runtime.
        let handle = tokio::spawn(carry_runtime(
            self.runtime.clone(),
            run_owned_connection(future, fault, self.script.clone()),
        ));
        let abort = handle.abort_handle();
        self.connections.insert(handle, identity, upgrade);
        LifecycleScript::observe_ownership(
            self.script.as_deref(),
            ConnectionOwnershipEvent::ServerConnectionRegistered {
                connection: identity,
            },
        );
        if matches!(fault, Some(ServerTaskFault::CancelNextOwnedTask)) {
            abort.abort();
        }
    }

    /// Read one connection completion as the failure it carries, or as nothing
    /// to report.
    ///
    /// Reading and disposal are separate because only the reading is shared.
    /// The running supervisor turns a failure into the candidate that explains
    /// the shutdown it starts; the post-panic drain already has its result and
    /// can only log. One classifier keeps the two from disagreeing about which
    /// cancellations were asked for.
    fn classify_completion(&self, completion: ConnectionCompletion) -> Option<RuntimeError> {
        match completion.result {
            Ok(()) => None,
            Err(error) if error.is_cancelled() && self.connections.supervisor_aborted() => None,
            // Every other join failure — panic or unexpected cancellation —
            // reads through the one translation, so the cancellation message
            // has a single definition.
            Err(error) => Some(join_panic_to_error(error)),
        }
    }

    /// Dispose of one connection completion the running supervisor observed. A
    /// join failure starts a graceful shutdown and is recorded as its candidate;
    /// a clean completion must not start one by arriving.
    async fn handle_connection_completion(&mut self, completion: ConnectionCompletion) {
        let (identity, cancelled) = (completion.identity, was_cancelled(&completion));
        let failure = self.classify_completion(completion);
        self.settle_connection(
            identity,
            connection_disposition(failure.as_ref(), cancelled),
        );
        if let Some(error) = failure {
            self.report_fatal(error).await;
        }
    }

    /// Publish how one owned connection was disposed of.
    ///
    /// Every connection settles here, whichever way its join came back, so the
    /// inventory says what happened to each of them rather than only to the ones
    /// that failed. There is one name to publish, because the registry holds one
    /// kind of owner: an upgrade settles under the connection that transferred
    /// it, not beside it.
    fn settle_connection(&self, identity: u64, disposition: ParticipantDisposition) {
        self.shutdown
            .settle(&ShutdownOwner::CONNECTION, disposition);
        LifecycleScript::observe_ownership(
            self.script.as_deref(),
            ConnectionOwnershipEvent::ServerConnectionSettled {
                connection: identity,
            },
        );
    }

    /// Commit a server-fatal fact and act on whatever phase that left.
    ///
    /// The fact starts the drain it explains when the server is still running,
    /// and is kept as the provisional flat result. A forced phase committed
    /// after it still decides how the server ends.
    async fn report_fatal(&mut self, error: RuntimeError) {
        self.stop.commit(StopEvent::Fatal(error)).await;
        self.apply_committed_phase().await;
    }

    /// Commit a graceful stop observed at `selected_at`, and act on it.
    async fn commit_graceful_at(&mut self, selected_at: tokio::time::Instant) {
        self.stop.commit(StopEvent::Graceful(selected_at)).await;
        self.apply_committed_phase().await;
    }

    /// Close admission for the graceful phase the stop state already committed.
    async fn enter_graceful(&mut self) {
        self.deadline = Some(self.graceful_deadline());
        self.close_admission(ServerControl::send_graceful).await;
    }

    /// The instant this server's graceful drain ends at.
    ///
    /// The one aggregate expiry is anchored at the instant the phase COMMITTED,
    /// not at whenever this supervisor got round to observing it, so no owner
    /// held between the two restarts the shared clock. A runtime that already
    /// transitioned hands back the instant it fixed, so a server entering its
    /// drain late gets the time that is left rather than a fresh copy of the
    /// grace.
    ///
    /// The server's own configured timeout may only narrow that, and it is
    /// measured from now for the same reason
    /// [`ConnectionShutdownDeadline::deadline`] measures it from now: it is this
    /// server's drain length, which starts when the server begins draining, and
    /// it is a narrowing rather than a second aggregate. Anchoring it at the
    /// commit instead would silently shorten every drain by however long the
    /// transition took to observe.
    fn graceful_deadline(&self) -> tokio::time::Instant {
        let committed_at = self
            .stop
            .graceful_at()
            .unwrap_or_else(tokio::time::Instant::now);
        let shared = self
            .shutdown
            .read_or_mint(&ShutdownOwner::SERVER, committed_at);
        shared.min(tokio::time::Instant::now() + self.shutdown_timeout)
    }

    /// Stop admitting and publish the control transition this shutdown makes.
    ///
    /// The four steps travel together: a listener left behind would keep
    /// admitting work the transition just refused — and giving it up here is
    /// what every later admission question reads — the edge the supervisor
    /// publishes to its connections has to be consumed here or its own selector
    /// answers a request it made itself, and a socket already accepted into
    /// `pending` is one this server will now never serve, so it is shut down
    /// here rather than left to be dropped raw. Both transitions differ only in
    /// which request they publish.
    async fn close_admission(&mut self, publish: fn(&tokio::sync::watch::Sender<ServerControl>)) {
        self.release_listener();
        publish(&self.control_sender);
        self.control_receiver.borrow_and_update();
        self.close_pending().await;
    }

    /// Give the listener up, and keep what removing its socket path reported.
    ///
    /// Admission closes exactly once, so this runs once: the second transition
    /// finds the listener already taken and has nothing to remove. The removal
    /// happens HERE, on the owner that is giving the listener away, because
    /// `Drop` is the only step after it and a destructor can log a failed
    /// removal but cannot return it. A Unix socket path replaced under the
    /// service is the case that matters — the listener refuses to delete what is
    /// no longer its own socket, and the caller waiting on this server is told.
    fn release_listener(&mut self) {
        let Some(listener) = self.listener.take() else {
            return;
        };
        if let Err(error) = listener.cleanup() {
            self.cleanup_failure = Some(error);
        }
    }

    /// Enter abort: stop admitting, tell every connection to shut down, and
    /// close out whatever was already accepted.
    ///
    /// The deadline is rearmed rather than cleared, because abort asks a
    /// connection for a protocol-level shutdown it can outlast — without a
    /// deadline the forced abort would wait forever on a permit that a
    /// long-lived response never releases.
    ///
    /// What it is rearmed to is NOT a second grace period. Abort is forced
    /// termination, so the window is whatever the one aggregate expiry still
    /// has, and never less than the fixed forced-join grace every other forced
    /// stop gives an owner it just told to end.
    async fn begin_abort(&mut self) {
        self.deadline = Some(self.forced_deadline());
        self.close_admission(ServerControl::send_abort).await;
    }

    /// The instant the forced abort stops waiting on the owners it told to end.
    ///
    /// An explicit cancellation gets no grace at all beyond the fixed
    /// forced-join window: the caller asked for now, and reading the aggregate
    /// would hand back time it deliberately gave up.
    fn forced_deadline(&self) -> tokio::time::Instant {
        let now = tokio::time::Instant::now();
        let remaining = match self.is_cancelled() {
            true => Duration::ZERO,
            false => self
                .shutdown
                .remaining(&ShutdownOwner::SERVER)
                .unwrap_or(self.shutdown_timeout),
        };
        now + remaining.max(FORCED_JOIN_GRACE)
    }

    async fn close_pending(&mut self) {
        if let Some(accepted) = self.pending.take() {
            accepted.stream.close().await;
        }
    }

    fn start_abort_if_ready(&mut self) {
        if self.applied_forced() && !self.abort_started {
            self.begin_forced_abort();
        }
    }

    /// End the connections that cannot end themselves, and leave the deadline
    /// armed for the ones that can.
    ///
    /// The deadline is what makes this safe to do in two stages. A connection
    /// holding an upgrade child is given the abort it was already told about and
    /// the time to settle what it owns under it; every other connection is ended
    /// here, because its answer to an abort is a drain with no bound of its own.
    /// A retained child that does not settle is still forced, by the same
    /// deadline this abort has carried since it began.
    ///
    /// The time such a connection is given is that deadline and no more of it
    /// than it needs. A bridge settles against the abort it is reading, so the
    /// window closes as soon as the last one answers; only a bridge parked
    /// somewhere no control transition reaches waits the deadline out.
    fn begin_forced_abort(&mut self) {
        self.abort_started = true;
        self.connections.abort_without_retained_upgrade();
    }

    /// Abort every connection, once, and disarm the deadline that was waiting
    /// to do it.
    ///
    /// The deadline's own answer, and the end of the settlement window
    /// [`Self::begin_forced_abort`] opened: a connection still holding a child
    /// by now is taken away where it stands.
    fn force_abort(&mut self) {
        self.deadline = None;
        self.abort_started = true;
        self.connections.abort_all();
    }

    /// Whether a forced stop has nothing left to join.
    ///
    /// `Finished` is deliberately excluded: settlement has already committed
    /// the immutable result, so a second pass through here would ask for one
    /// that no longer exists.
    fn abort_drain_complete(&self) -> bool {
        self.applied_forced() && self.abort_started && self.connections.is_empty()
    }

    async fn drain_owned_after_panic(&mut self) {
        // Preserve protocol-level shutdown after a supervisor fault, but never
        // let non-cooperative work outlive the configured shutdown deadline.
        // The deadline is one timer for the whole drain, selected on by `&mut`:
        // rebuilding it per completion would charge every task that finishes in
        // time a timer registration for a deadline that has not moved.
        let expiry = tokio::time::sleep_until(tokio::time::Instant::now() + self.shutdown_timeout);
        tokio::pin!(expiry);
        while !self.connections.is_empty() {
            let completion = tokio::select! {
                biased;
                completion = self.connections.next() => completion,
                () = &mut expiry => None,
            };
            match completion {
                Some(completion) => self.report_post_panic_completion(completion),
                None => break,
            }
        }
        self.abort_and_report_remaining().await;
    }

    /// End every connection the post-panic drain's own deadline outlasted, and
    /// report each join as it comes back.
    ///
    /// The forced sweep reports through the same path the bounded wait above it
    /// does. A connection joined here failed exactly as loudly as one joined a
    /// moment earlier, so discarding these completions would silence a failure
    /// purely for having been slow.
    async fn abort_and_report_remaining(&mut self) {
        self.connections.abort_all();
        while let Some(completion) = self.connections.next().await {
            self.report_post_panic_completion(completion);
        }
    }

    /// Report a task that failed while the supervisor was already unwinding.
    ///
    /// Logged rather than recorded, because `run` returns the supervisor's own
    /// panic on this path and never reads the terminal outcome: a failure
    /// written there would be dropped with `self`. The message names the drain
    /// so the connection's failure is not read as a second supervisor fault.
    fn report_post_panic_completion(&self, completion: ConnectionCompletion) {
        match self.classify_completion(completion) {
            Some(error) => tracing::error!(
                error = %error,
                "owned HTTP connection failed during the post-panic drain"
            ),
            None => {}
        }
    }

    /// Whether this server is still taking sockets off its listener.
    ///
    /// The listener IS the admission state: it is given up by the same step
    /// that publishes the transition, so a flag beside it would store the one
    /// fact twice and could disagree with it.
    fn admission_is_open(&self) -> bool {
        self.listener.is_some()
            && !self
                .runtime_shutdown
                .as_ref()
                .is_some_and(|shutdown| shutdown.is_fired())
    }

    fn raise_supervisor_fault(&self) {
        let should_panic = self
            .script
            .as_ref()
            .is_some_and(|script| script.take_supervisor_fault());
        if should_panic {
            std::panic::resume_unwind(Box::new("injected server supervisor panic"));
        }
    }

    /// Commit settlement and take the immutable flat result.
    ///
    /// Nothing is chosen here. The phase committed before this call decides the
    /// result, and a command that arrives afterwards finds the server finished.
    /// Both drain predicates that lead here exclude `Running` and `Finished`,
    /// so the state always has a result to hand over.
    fn take_result(&mut self) -> Result<(), RuntimeError> {
        let served = self.stop.settle().unwrap_or(Ok(()));
        report_serve_outcome(served, self.cleanup_failure.take())
    }

    async fn finish(&mut self) -> Result<(), RuntimeError> {
        let result = self.take_result();
        self.settle_server();
        LifecycleScript::pause_at_stop(
            self.script.as_deref(),
            ServerStopEdge::AfterSupervisorResultSend,
        )
        .await;
        result
    }

    /// Publish how this server ended, and record it where the runtime has to
    /// account for it.
    ///
    /// Nothing is RECORDED here. A server's own account leaves through its flat
    /// result, which is the value its owner already reads and the one place a
    /// per-server failure belongs; the runtime's aggregate names the owners no
    /// caller holds a handle for. Cancelling a server is a control action a
    /// caller asked for, not a lifecycle failure the runtime has to report.
    ///
    /// What is published is the disposition, so the settlement inventory says
    /// what happened to this server rather than leaving it to be inferred from
    /// a `Result` the aggregate never sees.
    ///
    /// The outcome is read from the state that committed it, not recovered by
    /// matching the error back out of the `Result` that same settlement encoded
    /// it into. Both forced endings are ends this server was given rather than
    /// reached: a cancellation the caller asked for, and an aggregate deadline
    /// that expired under it. Reporting either as `Completed` would claim the
    /// drain finished on its own.
    fn settle_server(&self) {
        let disposition = match self.stop.outcome() {
            StopOutcome::Cancelled | StopOutcome::TimedOut => {
                ParticipantDisposition::CancelledAndJoined
            }
            StopOutcome::Pending | StopOutcome::Completed | StopOutcome::Failed => {
                ParticipantDisposition::Completed
            }
        };
        self.shutdown.settle(&ShutdownOwner::SERVER, disposition);
    }
}

/// Report both halves of a served listener's outcome.
///
/// Only one error can leave, and how the serving ended is the one that says
/// why; a cleanup failure behind it is logged rather than destroyed. A serve
/// that ended cleanly has nothing of its own to report, so the cleanup failure
/// is the whole answer — which is how a socket path replaced under the service
/// reaches the caller instead of stopping at a warning.
fn report_serve_outcome(
    served: Result<(), RuntimeError>,
    cleanup: Option<RuntimeError>,
) -> Result<(), RuntimeError> {
    match (served, cleanup) {
        (Ok(()), None) => Ok(()),
        (Ok(()), Some(error)) | (Err(error), None) => Err(error),
        (Err(error), Some(cleanup_error)) => {
            tracing::warn!(%cleanup_error, "listener cleanup failed after a serve error");
            Err(error)
        }
    }
}

/// Whether one connection's join came back as the cancellation it was given.
///
/// Read from the join rather than from the supervisor's own flag, so a
/// connection that finished on its own in the race before an abort landed is
/// still reported as having completed. The flag says what the supervisor asked
/// for; only the join says what happened.
fn was_cancelled(completion: &ConnectionCompletion) -> bool {
    completion
        .result
        .as_ref()
        .err()
        .is_some_and(tokio::task::JoinError::is_cancelled)
}

async fn run_owned_connection<F>(
    future: F,
    fault: Option<ServerTaskFault>,
    script: Option<Arc<LifecycleScript>>,
) where
    F: Future<Output = ()>,
{
    match fault {
        Some(ServerTaskFault::PanicNextOwnedTask) => {
            std::panic::resume_unwind(Box::new(OWNED_TASK_PANIC));
        }
        Some(ServerTaskFault::PanicNextOwnedTaskOpaque) => {
            std::panic::resume_unwind(Box::new(7usize));
        }
        Some(ServerTaskFault::CancelNextOwnedTask | ServerTaskFault::PanicSupervisorCore)
        | None => future.await,
    }
    LifecycleScript::pause_at_connection(
        script.as_deref(),
        ConnectionOwnerEdge::AfterConnectionFutureCompleted,
    )
    .await;
}

/// Report a refused connection permit and close the socket it was for.
///
/// The semaphore closes only when the server itself is going away, so the
/// accepted socket has no owner left to serve it — and the error naming that is
/// the only account of why this connection was dropped.
async fn refuse_permit(error: &tokio::sync::AcquireError, accepted: Option<PendingAccepted>) {
    tracing::warn!(%error, "connection permit unavailable; closing accepted socket");
    if let Some(accepted) = accepted {
        accepted.stream.close().await;
    }
}

/// Wait for the next event every selector observes first: the shutdown
/// deadline, an owner control request, or runtime shutdown.
///
/// This is the `biased` shutdown priority, stated once. Both selectors race it
/// ahead of their own admission arm, so neither can drift into answering a new
/// connection before a shutdown that is already pending.
///
/// No arm carries a guard: `wait_deadline` and `wait_runtime` each answer their
/// own absent input with `pending()`, and the control arm is never disabled, so
/// this select cannot be fully disabled either. A guard here would repeat what
/// the future already does.
async fn select_lifecycle_event(
    deadline: Option<tokio::time::Instant>,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    runtime_shutdown: Option<&ShutdownSignal>,
    script: Option<&LifecycleScript>,
) -> SupervisorEvent {
    tokio::select! {
        biased;
        () = wait_deadline(deadline) => SupervisorEvent::Deadline,
        requested = wait_control(control) => SupervisorEvent::Control(requested),
        () = wait_runtime(runtime_shutdown, script) => {
            SupervisorEvent::Runtime(tokio::time::Instant::now())
        }
    }
}

/// Wait for the next event from work this supervisor already owns: a finished
/// connection, or a test script wake.
///
/// Both selectors race this last, so owned work is answered only once nothing
/// more urgent is ready.
async fn select_owned_connection_event(
    connections: &mut OwnedConnections,
    script: Option<&LifecycleScript>,
) -> SupervisorEvent {
    tokio::select! {
        biased;
        completion = connections.next(), if !connections.is_empty() => {
            SupervisorEvent::Connection(completion)
        }
        // Deliberately unguarded: it is what keeps this select from being fully
        // disabled, which `select!` answers with a panic. The other arm needs its
        // guard — an empty connection set is ready at once and would spin — and
        // `wait_for_script_wake` already answers a missing script with
        // `pending()`, so a guard here would only repeat what the future does.
        () = wait_for_script_wake(script) => SupervisorEvent::ScriptWake,
    }
}

/// Hold the supervisor at the stop edge one selected event names, if the test
/// controller named one for it. A script wake is the script's own event and has
/// none.
async fn pause_selected(script: Option<&LifecycleScript>, event: &SupervisorEvent) {
    if let Some(edge) = selected_stop_edge(event) {
        LifecycleScript::pause_at_stop(script, edge).await;
    }
}

fn selected_stop_edge(event: &SupervisorEvent) -> Option<ServerStopEdge> {
    match event {
        SupervisorEvent::Deadline => Some(ServerStopEdge::SupervisorSelectedDeadline),
        SupervisorEvent::Control(_) => Some(ServerStopEdge::SupervisorSelectedControl),
        SupervisorEvent::Runtime(_) => Some(ServerStopEdge::SupervisorSelectedRuntime),
        SupervisorEvent::Accept(_) => Some(ServerStopEdge::SupervisorSelectedAccept),
        SupervisorEvent::Permit(_) => Some(ServerStopEdge::SupervisorSelectedPermit),
        SupervisorEvent::Connection(_) => Some(ServerStopEdge::SupervisorSelectedTask),
        SupervisorEvent::ScriptWake => None,
    }
}

async fn accept_next(
    listener: Option<&crate::net::Listener>,
    script: Option<&LifecycleScript>,
) -> Result<(crate::net::AcceptedStream, Option<std::net::SocketAddr>), std::io::Error> {
    if let Some(kind) = script.and_then(|script| script.take_accept_fault()) {
        return Err(std::io::Error::from(kind));
    }
    match listener {
        Some(listener) => listener.accept().await,
        None => std::future::pending().await,
    }
}

fn join_panic_to_error(error: tokio::task::JoinError) -> RuntimeError {
    match error.is_panic() {
        true => panic_to_error(error.into_panic()),
        false => RuntimeError::TaskPanicked("owned HTTP task cancelled unexpectedly".into()),
    }
}

pub(in crate::http) enum SupervisorJoin {
    /// A supervisor driven inline by the owner's own future rather than a
    /// spawned task. Dropping the owner drops the supervisor with it.
    Owned(Pin<Box<dyn Future<Output = Result<(), RuntimeError>> + Send>>),
    Camber(AsyncJoinFuture<Result<(), RuntimeError>>),
    Tokio(tokio::task::JoinHandle<Result<(), RuntimeError>>),
}

pub(in crate::http) fn poll_supervisor_join(
    join: &mut SupervisorJoin,
    context: &mut Context<'_>,
) -> Poll<Result<(), RuntimeError>> {
    match join {
        // A Camber join failure is already a `RuntimeError`, so flattening it
        // is the inner result or that error unchanged.
        SupervisorJoin::Owned(future) => future.as_mut().poll(context),
        SupervisorJoin::Camber(future) => Pin::new(future)
            .poll(context)
            .map(|result| result.unwrap_or_else(Err)),
        SupervisorJoin::Tokio(handle) => Pin::new(handle).poll(context).map(flatten_tokio_join),
    }
}

fn flatten_tokio_join(
    result: Result<Result<(), RuntimeError>, tokio::task::JoinError>,
) -> Result<(), RuntimeError> {
    match result {
        Ok(server_result) => server_result,
        Err(error) if error.is_cancelled() => Err(RuntimeError::TaskPanicked(
            "server supervisor cancelled unexpectedly".into(),
        )),
        Err(error) => Err(join_panic_to_error(error)),
    }
}

pub(in crate::http) fn supervisor_join_probe(
    probe: SupervisorJoinProbe,
) -> super::super::server::ServerHandleFuture {
    let join = match probe {
        SupervisorJoinProbe::CamberCancelled => {
            let handle =
                crate::task::spawn_async(std::future::pending::<Result<(), RuntimeError>>());
            handle.cancel();
            SupervisorJoin::Camber(handle.into_future())
        }
        SupervisorJoinProbe::CamberStringPanic => {
            SupervisorJoin::Camber(crate::task::spawn_async(string_panic_probe()).into_future())
        }
        SupervisorJoinProbe::CamberOpaquePanic => {
            SupervisorJoin::Camber(crate::task::spawn_async(opaque_panic_probe()).into_future())
        }
        SupervisorJoinProbe::CamberChannelClosed => {
            SupervisorJoin::Camber(AsyncJoinFuture::closed())
        }
        SupervisorJoinProbe::TokioSuccess => SupervisorJoin::Tokio(tokio::spawn(async { Ok(()) })),
        SupervisorJoinProbe::TokioCancelled => {
            let handle = tokio::spawn(std::future::pending::<Result<(), RuntimeError>>());
            handle.abort();
            SupervisorJoin::Tokio(handle)
        }
        SupervisorJoinProbe::TokioStringPanic => {
            SupervisorJoin::Tokio(tokio::spawn(string_panic_probe()))
        }
        SupervisorJoinProbe::TokioOpaquePanic => {
            SupervisorJoin::Tokio(tokio::spawn(opaque_panic_probe()))
        }
    };
    super::super::server::ServerHandleFuture::from_join(join)
}

async fn string_panic_probe() -> Result<(), RuntimeError> {
    std::panic::resume_unwind(Box::new(SUPERVISOR_PROBE_PANIC));
}

async fn opaque_panic_probe() -> Result<(), RuntimeError> {
    std::panic::resume_unwind(Box::new(13usize));
}
