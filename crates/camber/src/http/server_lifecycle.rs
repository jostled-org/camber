use std::future::{Future, IntoFuture, Ready};
use std::ops::ControlFlow;
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use futures_util::FutureExt;
use futures_util::stream::{FuturesUnordered, StreamExt};

use super::BufferConfig;
#[cfg(feature = "ws")]
use super::Response;
use super::mock::{LifecycleCheckpoint, LifecycleFault, LifecycleScript, SupervisorJoinProbe};
use super::router::ServerDispatch;
use crate::runtime_state::{
    DEFAULT_KEEPALIVE_TIMEOUT, DEFAULT_SHUTDOWN_TIMEOUT, RuntimeInner, ShutdownSignal,
    carry_runtime,
};
use crate::task::{AsyncJoinFuture, panic_to_error};
use crate::{RuntimeError, runtime};

const OWNED_TASK_PANIC: &str = "injected owned HTTP task panic";
const SUPERVISOR_PROBE_PANIC: &str = "supervisor join probe panic";
const TRANSIENT_ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
const UPGRADE_PENDING: u8 = 0;
const UPGRADE_ADMITTED: u8 = 1;
const UPGRADE_CANCELLED: u8 = 2;

#[derive(Clone, Copy, Eq, PartialEq)]
enum ShutdownMode {
    Running,
    Graceful,
    Abort,
}

enum TerminalOutcome {
    Success,
    Fatal(RuntimeError),
    Cancelled,
    Timeout,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum ServerControl {
    Running,
    Graceful,
    Abort,
}

impl ServerControl {
    pub(super) fn send_abort(sender: &tokio::sync::watch::Sender<Self>) {
        sender.send_if_modified(|control| match control {
            Self::Running | Self::Graceful => {
                *control = Self::Abort;
                true
            }
            Self::Abort => false,
        });
    }

    pub(super) fn send_graceful(sender: &tokio::sync::watch::Sender<Self>) {
        sender.send_if_modified(|control| match control {
            Self::Running => {
                *control = Self::Graceful;
                true
            }
            Self::Graceful | Self::Abort => false,
        });
    }
}

/// What a server was started under, classified once at construction.
///
/// The runtime is the only server-scoped value stored: every timeout, limit,
/// and observability handle the server needs is read back out of it on demand,
/// so there is one answer to each question rather than a stored copy that can
/// disagree with its source. `None` is the standalone capture, and stays
/// `None`. Only the router's buffer sizes and the TLS flag are independent of
/// the runtime, so they are the only other fields.
pub(super) struct ServerContextSnapshot {
    runtime: Option<Arc<RuntimeInner>>,
    buffers: BufferConfig,
    is_tls: bool,
}

impl ServerContextSnapshot {
    pub(super) fn capture(buffers: BufferConfig, is_tls: bool) -> Self {
        Self {
            runtime: runtime::try_current_runtime(),
            buffers,
            is_tls,
        }
    }

    /// Whether the captured context came from a Camber runtime.
    ///
    /// The snapshot already resolved that question once. Asking the thread-local
    /// again would put the same question twice and let the two answers disagree.
    pub(super) fn is_camber(&self) -> bool {
        self.runtime.is_some()
    }

    fn config(&self) -> Option<&crate::runtime_state::RuntimeConfig> {
        self.runtime.as_ref().map(|runtime| &runtime.config)
    }

    fn runtime_shutdown(&self) -> Option<ShutdownSignal> {
        self.runtime
            .as_ref()
            .map(|runtime| runtime.shutdown_signal())
    }

    fn shutdown_timeout(&self) -> Duration {
        self.config()
            .map_or(DEFAULT_SHUTDOWN_TIMEOUT, |config| config.shutdown_timeout)
    }

    fn keepalive_timeout(&self) -> Duration {
        self.config()
            .map_or(DEFAULT_KEEPALIVE_TIMEOUT, |config| config.keepalive_timeout)
    }

    fn connection_limit(&self) -> Option<usize> {
        self.config().and_then(|config| config.connection_limit)
    }

    fn connection_context(&self) -> super::handle::ConnCtx {
        match &self.runtime {
            Some(runtime) => {
                super::handle::ConnCtx::from_runtime(runtime, self.buffers, self.is_tls)
            }
            None => self.standalone_context(),
        }
    }

    /// The context a server started outside a Camber runtime serves with: the
    /// router's own buffer limits and the transport's TLS state, and nothing
    /// that could only have come from a runtime.
    fn standalone_context(&self) -> super::handle::ConnCtx {
        super::handle::ConnCtx {
            tracing_enabled: false,
            metrics_handle: None,
            #[cfg(feature = "profiling")]
            profiling_enabled: false,
            max_request_body: self.buffers.max_request_body,
            sse_buffer_size: self.buffers.sse_buffer_size,
            #[cfg(feature = "ws")]
            ws_buffer_size: self.buffers.ws_buffer_size,
            health_state: None,
            is_tls: self.is_tls,
        }
    }
}

pub(super) struct ConnectionPermit {
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl ConnectionPermit {
    pub(super) fn new(permit: Option<tokio::sync::OwnedSemaphorePermit>) -> Arc<Self> {
        Arc::new(Self { permit })
    }
}

/// Return the connection's slot in the limit when the last owner of the shared
/// handle exits.
///
/// The permit is never read anywhere else — holding it IS the whole type — so
/// this release is also the only read the field has, and without it the field
/// is dead code.
impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        drop(self.permit.take());
    }
}

pub(super) struct ConnectionLifecycle {
    permit: Arc<ConnectionPermit>,
    control: Option<tokio::sync::watch::Receiver<ServerControl>>,
    registration: Option<tokio::sync::mpsc::Sender<UpgradeTicket>>,
    #[cfg(feature = "ws")]
    transport_registration: Option<tokio::sync::mpsc::Sender<TransportRegistration>>,
    #[cfg(feature = "ws")]
    transport_state: Option<tokio::sync::watch::Receiver<UpgradeTransportState>>,
    script: Option<Arc<LifecycleScript>>,
}

impl Clone for ConnectionLifecycle {
    fn clone(&self) -> Self {
        Self {
            permit: Arc::clone(&self.permit),
            control: self.control.clone(),
            registration: self.registration.clone(),
            #[cfg(feature = "ws")]
            transport_registration: self.transport_registration.clone(),
            #[cfg(feature = "ws")]
            transport_state: self.transport_state.clone(),
            script: self.script.clone(),
        }
    }
}

impl ConnectionLifecycle {
    pub(super) fn synchronous(permit: Option<tokio::sync::OwnedSemaphorePermit>) -> Self {
        Self {
            permit: ConnectionPermit::new(permit),
            control: None,
            registration: None,
            #[cfg(feature = "ws")]
            transport_registration: None,
            #[cfg(feature = "ws")]
            transport_state: None,
            script: None,
        }
    }

    fn owned(
        permit: Arc<ConnectionPermit>,
        control: tokio::sync::watch::Receiver<ServerControl>,
        registration: tokio::sync::mpsc::Sender<UpgradeTicket>,
        script: Option<Arc<LifecycleScript>>,
    ) -> Self {
        Self {
            permit,
            control: Some(control),
            registration: Some(registration),
            #[cfg(feature = "ws")]
            transport_registration: None,
            #[cfg(feature = "ws")]
            transport_state: None,
            script,
        }
    }

    #[cfg(feature = "ws")]
    pub(super) fn permit(&self) -> Arc<ConnectionPermit> {
        Arc::clone(&self.permit)
    }

    pub(super) fn script(&self) -> Option<Arc<LifecycleScript>> {
        self.script.clone()
    }

    #[cfg(feature = "ws")]
    pub(super) fn upgrade_registrar(&self) -> Option<UpgradeRegistrar> {
        let sender = self.registration.as_ref()?;
        let control = self.control.as_ref()?;
        let transport_registration = self.transport_registration.as_ref()?;
        let transport_state = self.transport_state.as_ref()?;
        Some(UpgradeRegistrar::new(
            sender.clone(),
            control.clone(),
            transport_registration.clone(),
            transport_state.clone(),
            self.script.clone(),
            Arc::downgrade(&self.permit),
        ))
    }

    #[cfg(feature = "ws")]
    pub(super) fn bind_upgrade_transport(&mut self) -> UpgradeTransportOwner {
        let (registration_sender, registration_receiver) = tokio::sync::mpsc::channel(1);
        let (state_sender, state_receiver) =
            tokio::sync::watch::channel(UpgradeTransportState::Pending);
        self.transport_registration = Some(registration_sender);
        self.transport_state = Some(state_receiver);
        UpgradeTransportOwner {
            registration_receiver,
            state_sender,
        }
    }
}

#[cfg(feature = "ws")]
#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum UpgradeTransportState {
    Pending,
    Committed,
    Cancelled,
}

#[cfg(feature = "ws")]
pub(super) struct UpgradeDispatchGate {
    state: tokio::sync::watch::Receiver<UpgradeTransportState>,
}

#[cfg(feature = "ws")]
impl UpgradeDispatchGate {
    pub(super) async fn committed(mut self) -> bool {
        loop {
            match *self.state.borrow_and_update() {
                UpgradeTransportState::Committed => return true,
                UpgradeTransportState::Cancelled => return false,
                UpgradeTransportState::Pending => {}
            }
            if self.state.changed().await.is_err() {
                return false;
            }
        }
    }
}

enum RegistrationDecision {
    Admitted,
    Rejected,
}

/// Cancel a pending upgrade: publish the cancelled state, then abort.
///
/// The order is load-bearing, and every caller depends on it. The state is
/// stored first so the supervisor's join reads the abort as a cancellation it
/// asked for rather than a fault, and `Release` is what makes that store visible
/// to the joining side. Stated once here so the seven cancellation sites cannot
/// each get it wrong.
fn cancel_upgrade(state: &AtomicU8, abort: &tokio::task::AbortHandle) {
    state.store(UPGRADE_CANCELLED, Ordering::Release);
    abort.abort();
}

pub(super) struct UpgradeTicket {
    handle: Option<tokio::task::JoinHandle<()>>,
    abort: tokio::task::AbortHandle,
    state: Arc<AtomicU8>,
    connection: Weak<ConnectionPermit>,
    acknowledgement: Option<tokio::sync::oneshot::Sender<RegistrationDecision>>,
    decision_request: Option<tokio::sync::oneshot::Sender<()>>,
    decision_ready: Option<tokio::sync::oneshot::Receiver<()>>,
}

impl UpgradeTicket {
    fn into_parts(mut self) -> UpgradeTicketParts {
        UpgradeTicketParts {
            handle: self.handle.take(),
            abort: self.abort.clone(),
            state: Arc::clone(&self.state),
            connection: self.connection.clone(),
            acknowledgement: self.acknowledgement.take(),
            decision_request: self.decision_request.take(),
            decision_ready: self.decision_ready.take(),
        }
    }

    #[cfg(feature = "ws")]
    async fn abort_and_join(mut self) {
        cancel_upgrade(&self.state, &self.abort);
        let handle = self.handle.take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}

impl Drop for UpgradeTicket {
    fn drop(&mut self) {
        if self.handle.is_some() {
            cancel_upgrade(&self.state, &self.abort);
        }
    }
}

struct UpgradeTicketParts {
    handle: Option<tokio::task::JoinHandle<()>>,
    abort: tokio::task::AbortHandle,
    state: Arc<AtomicU8>,
    connection: Weak<ConnectionPermit>,
    acknowledgement: Option<tokio::sync::oneshot::Sender<RegistrationDecision>>,
    decision_request: Option<tokio::sync::oneshot::Sender<()>>,
    decision_ready: Option<tokio::sync::oneshot::Receiver<()>>,
}

#[cfg(feature = "ws")]
pub(super) enum UpgradeRegistration {
    Admitted,
    Rejected,
    Unavailable,
}

#[cfg(feature = "ws")]
pub(super) struct UpgradeRegistrar {
    sender: tokio::sync::mpsc::Sender<UpgradeTicket>,
    control: tokio::sync::watch::Receiver<ServerControl>,
    transport_registration: tokio::sync::mpsc::Sender<TransportRegistration>,
    transport_state: tokio::sync::watch::Receiver<UpgradeTransportState>,
    script: Option<Arc<LifecycleScript>>,
    connection: Weak<ConnectionPermit>,
    abort: Option<tokio::task::AbortHandle>,
    state: Option<Arc<AtomicU8>>,
}

#[cfg(feature = "ws")]
impl UpgradeRegistrar {
    fn new(
        sender: tokio::sync::mpsc::Sender<UpgradeTicket>,
        control: tokio::sync::watch::Receiver<ServerControl>,
        transport_registration: tokio::sync::mpsc::Sender<TransportRegistration>,
        transport_state: tokio::sync::watch::Receiver<UpgradeTransportState>,
        script: Option<Arc<LifecycleScript>>,
        connection: Weak<ConnectionPermit>,
    ) -> Self {
        Self {
            sender,
            control,
            transport_registration,
            transport_state,
            script,
            connection,
            abort: None,
            state: None,
        }
    }

    pub(super) async fn submit(self, handle: tokio::task::JoinHandle<()>) -> UpgradeRegistration {
        let sender = self.transport_registration.clone();
        let (decision_sender, decision_receiver) = tokio::sync::oneshot::channel();
        let registration = TransportRegistration {
            registrar: Some(self),
            handle: Some(handle),
            decision: Some(decision_sender),
        };
        match sender.send(registration).await {
            Ok(()) => receive_upgrade_decision(decision_receiver).await,
            Err(error) => {
                error.0.abort_and_join().await;
                UpgradeRegistration::Unavailable
            }
        }
    }

    pub(super) fn dispatch_gate(&self) -> UpgradeDispatchGate {
        UpgradeDispatchGate {
            state: self.transport_state.clone(),
        }
    }

    async fn submit_to_supervisor(
        mut self,
        handle: tokio::task::JoinHandle<()>,
    ) -> SupervisedUpgradeRegistration {
        let abort = match self.abort.as_ref() {
            Some(abort) => abort.clone(),
            None => return SupervisedUpgradeRegistration::Unavailable,
        };
        let state = match self.state.as_ref() {
            Some(state) => Arc::clone(state),
            None => return SupervisedUpgradeRegistration::Unavailable,
        };
        let (acknowledgement, acknowledged) = tokio::sync::oneshot::channel();
        let (decision_request, decision_requested) = tokio::sync::oneshot::channel();
        let (decision_ready, decision_waiting) = tokio::sync::oneshot::channel();
        let ticket = UpgradeTicket {
            handle: Some(handle),
            abort,
            state,
            connection: self.connection.clone(),
            acknowledgement: Some(acknowledgement),
            decision_request: Some(decision_request),
            decision_ready: Some(decision_waiting),
        };
        match self.sender.send(ticket).await {
            Ok(()) => {}
            Err(error) => {
                error.0.abort_and_join().await;
                self.disarm();
                return SupervisedUpgradeRegistration::Unavailable;
            }
        }
        LifecycleScript::pause_at(
            self.script.as_deref(),
            LifecycleCheckpoint::AfterUpgradeTicketSubmitted,
        )
        .await;
        let mut acknowledged = acknowledged;
        let (early_decision, request_failed) = tokio::select! {
            biased;
            decision = &mut acknowledged => (Some(decision), false),
            request = decision_requested => match request {
                Ok(()) => (None, false),
                Err(_) => (None, true),
            },
        };
        let decision = match (early_decision, request_failed) {
            (Some(decision), _) => decision,
            (None, false) => {
                let _ = decision_ready.send(());
                acknowledged.await
            }
            (None, true) => {
                self.abort_expected();
                self.disarm();
                return SupervisedUpgradeRegistration::Unavailable;
            }
        };
        let registration = match decision {
            Ok(RegistrationDecision::Admitted) => {
                SupervisedUpgradeRegistration::Admitted(UpgradeCommitment {
                    abort: self.abort.clone(),
                    state: self.state.clone(),
                    armed: true,
                })
            }
            Ok(RegistrationDecision::Rejected) => SupervisedUpgradeRegistration::Rejected,
            Err(_) => {
                self.abort_expected();
                SupervisedUpgradeRegistration::Unavailable
            }
        };
        self.disarm();
        registration
    }

    fn prepare(&mut self, handle: &tokio::task::JoinHandle<()>) -> UpgradeCancellation {
        let abort = handle.abort_handle();
        let state = Arc::new(AtomicU8::new(UPGRADE_PENDING));
        self.abort = Some(abort.clone());
        self.state = Some(Arc::clone(&state));
        UpgradeCancellation { abort, state }
    }

    pub(super) fn control(&self) -> tokio::sync::watch::Receiver<ServerControl> {
        self.control.clone()
    }

    fn disarm(&mut self) {
        self.abort = None;
        self.state = None;
    }

    /// `prepare` arms both halves together and `disarm` clears both together,
    /// so either both are present or there is nothing armed to cancel.
    fn abort_expected(&self) {
        if let (Some(state), Some(abort)) = (self.state.as_ref(), self.abort.as_ref()) {
            cancel_upgrade(state, abort);
        }
    }
}

#[cfg(feature = "ws")]
async fn receive_upgrade_decision(
    receiver: tokio::sync::oneshot::Receiver<UpgradeRegistration>,
) -> UpgradeRegistration {
    match receiver.await {
        Ok(decision) => decision,
        Err(_) => UpgradeRegistration::Unavailable,
    }
}

#[cfg(feature = "ws")]
enum SupervisedUpgradeRegistration {
    Admitted(UpgradeCommitment),
    Rejected,
    Unavailable,
}

#[cfg(feature = "ws")]
pub(super) struct UpgradeCancellation {
    abort: tokio::task::AbortHandle,
    state: Arc<AtomicU8>,
}

#[cfg(feature = "ws")]
impl UpgradeCancellation {
    pub(super) fn cancel(&self) {
        cancel_upgrade(&self.state, &self.abort);
    }
}

#[cfg(feature = "ws")]
pub(super) struct UpgradeCommitment {
    abort: Option<tokio::task::AbortHandle>,
    state: Option<Arc<AtomicU8>>,
    armed: bool,
}

#[cfg(feature = "ws")]
impl UpgradeCommitment {
    pub(super) fn commit(mut self) {
        self.armed = false;
    }

    /// The two halves are taken from a registrar that armed them together, so
    /// either both are present or there is nothing armed to cancel.
    pub(super) fn cancel(&mut self) {
        if !self.armed {
            return;
        }
        if let (Some(state), Some(abort)) = (self.state.as_ref(), self.abort.as_ref()) {
            cancel_upgrade(state, abort);
        }
        self.armed = false;
    }
}

#[cfg(feature = "ws")]
impl Drop for UpgradeCommitment {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(feature = "ws")]
pub(super) struct TransportRegistration {
    registrar: Option<UpgradeRegistrar>,
    handle: Option<tokio::task::JoinHandle<()>>,
    decision: Option<tokio::sync::oneshot::Sender<UpgradeRegistration>>,
}

#[cfg(feature = "ws")]
impl TransportRegistration {
    pub(super) fn prepare(&mut self) -> Option<UpgradeCancellation> {
        match (self.registrar.as_mut(), self.handle.as_ref()) {
            (Some(registrar), Some(handle)) => Some(registrar.prepare(handle)),
            _ => None,
        }
    }

    pub(super) async fn register(mut self) -> TransportRegistrationOutcome {
        let registration = match (self.registrar.take(), self.handle.take()) {
            (Some(registrar), Some(handle)) => registrar.submit_to_supervisor(handle).await,
            _ => SupervisedUpgradeRegistration::Unavailable,
        };
        TransportRegistrationOutcome {
            registration,
            decision: self.decision.take(),
        }
    }

    async fn abort_and_join(mut self) {
        if let Some(registrar) = self.registrar.as_ref() {
            registrar.abort_expected();
        }
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
        }
    }
}

#[cfg(feature = "ws")]
impl Drop for TransportRegistration {
    fn drop(&mut self) {
        if let Some(registrar) = self.registrar.as_ref() {
            registrar.abort_expected();
        }
    }
}

#[cfg(feature = "ws")]
pub(super) struct TransportRegistrationOutcome {
    registration: SupervisedUpgradeRegistration,
    decision: Option<tokio::sync::oneshot::Sender<UpgradeRegistration>>,
}

#[cfg(feature = "ws")]
impl TransportRegistrationOutcome {
    pub(super) fn admitted(&self) -> bool {
        matches!(
            self.registration,
            SupervisedUpgradeRegistration::Admitted(_)
        )
    }

    pub(super) fn cancel(mut self) {
        if let SupervisedUpgradeRegistration::Admitted(commitment) = self.registration {
            drop(commitment);
        }
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(UpgradeRegistration::Unavailable);
        }
    }

    pub(super) fn complete(mut self) -> Option<UpgradeCommitment> {
        match self.registration {
            SupervisedUpgradeRegistration::Admitted(commitment) => {
                let sent = self
                    .decision
                    .take()
                    .is_some_and(|decision| decision.send(UpgradeRegistration::Admitted).is_ok());
                sent.then_some(commitment)
            }
            SupervisedUpgradeRegistration::Rejected => {
                self.send(UpgradeRegistration::Rejected);
                None
            }
            SupervisedUpgradeRegistration::Unavailable => {
                self.send(UpgradeRegistration::Unavailable);
                None
            }
        }
    }

    fn send(&mut self, registration: UpgradeRegistration) {
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(registration);
        }
    }
}

#[cfg(feature = "ws")]
pub(super) struct UpgradeTransportOwner {
    registration_receiver: tokio::sync::mpsc::Receiver<TransportRegistration>,
    state_sender: tokio::sync::watch::Sender<UpgradeTransportState>,
}

#[cfg(feature = "ws")]
impl UpgradeTransportOwner {
    pub(super) async fn next_registration(&mut self) -> Option<TransportRegistration> {
        self.registration_receiver.recv().await
    }

    pub(super) fn commit(&self) {
        self.state_sender
            .send_replace(UpgradeTransportState::Committed);
    }

    pub(super) fn cancel(&self) {
        self.state_sender
            .send_replace(UpgradeTransportState::Cancelled);
    }

    pub(super) async fn abort_pending(&mut self) {
        self.registration_receiver.close();
        while let Some(registration) = self.registration_receiver.recv().await {
            registration.abort_and_join().await;
        }
    }
}

#[cfg(feature = "ws")]
impl Drop for UpgradeRegistrar {
    fn drop(&mut self) {
        self.abort_expected();
    }
}

struct OwnedTask {
    handle: tokio::task::JoinHandle<()>,
    expected_cancellation: Option<Arc<AtomicU8>>,
}

impl Future for OwnedTask {
    type Output = OwnedTaskCompletion;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let result = Pin::new(&mut self.handle).poll(context);
        result.map(|result| OwnedTaskCompletion {
            result,
            expected_cancellation: self
                .expected_cancellation
                .as_ref()
                .is_some_and(|state| state.load(Ordering::Acquire) == UPGRADE_CANCELLED),
        })
    }
}

/// Abort the task this owner is letting go of.
///
/// This is the whole field-drop path: dropping `OwnedHttpTasks` drops its
/// `FuturesUnordered`, which drops every `OwnedTask` through here, so the set
/// needs no `Drop` of its own. `abort_all` covers the explicit path, which is
/// separate only because it also records that the supervisor asked.
impl Drop for OwnedTask {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

struct OwnedTaskCompletion {
    result: Result<(), tokio::task::JoinError>,
    expected_cancellation: bool,
}

pub(super) struct OwnedHttpTasks {
    tasks: FuturesUnordered<OwnedTask>,
    supervisor_aborted: bool,
}

impl OwnedHttpTasks {
    fn new() -> Self {
        Self {
            tasks: FuturesUnordered::new(),
            supervisor_aborted: false,
        }
    }

    fn insert(&mut self, handle: tokio::task::JoinHandle<()>) {
        self.tasks.push(OwnedTask {
            handle,
            expected_cancellation: None,
        });
    }

    fn insert_registered(&mut self, handle: tokio::task::JoinHandle<()>, state: Arc<AtomicU8>) {
        self.tasks.push(OwnedTask {
            handle,
            expected_cancellation: Some(state),
        });
    }

    fn is_empty(&self) -> bool {
        self.tasks.is_empty()
    }

    fn abort_all(&mut self) {
        self.supervisor_aborted = true;
        self.tasks.iter().for_each(|task| task.handle.abort());
    }

    async fn next(&mut self) -> Option<OwnedTaskCompletion> {
        self.tasks.next().await
    }

    async fn abort_and_drain(&mut self) {
        self.abort_all();
        while self.next().await.is_some() {}
    }
}

struct PendingAccepted {
    stream: tokio::net::TcpStream,
    remote_addr: std::net::SocketAddr,
}

enum SupervisorEvent {
    ScriptWake,
    Deadline,
    Control(ServerControl),
    Runtime(tokio::time::Instant),
    Accept(Result<(tokio::net::TcpStream, std::net::SocketAddr), std::io::Error>),
    Permit(Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError>),
    Registration(Option<UpgradeTicket>),
    Task(Option<OwnedTaskCompletion>),
}

pub(super) struct ServerSupervisor {
    listener: Option<tokio::net::TcpListener>,
    dispatch: Arc<ServerDispatch>,
    context: Arc<super::handle::ConnCtx>,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    keepalive_timeout: Duration,
    shutdown_timeout: Duration,
    runtime_shutdown: Option<ShutdownSignal>,
    runtime: Option<Arc<RuntimeInner>>,
    connection_limit: Option<Arc<tokio::sync::Semaphore>>,
    pending: Option<PendingAccepted>,
    control_sender: tokio::sync::watch::Sender<ServerControl>,
    control_receiver: tokio::sync::watch::Receiver<ServerControl>,
    registration_sender: Option<tokio::sync::mpsc::Sender<UpgradeTicket>>,
    registration_receiver: tokio::sync::mpsc::Receiver<UpgradeTicket>,
    registration_closed: bool,
    tasks: OwnedHttpTasks,
    mode: ShutdownMode,
    terminal: TerminalOutcome,
    deadline: Option<tokio::time::Instant>,
    rejected_connections: Vec<Weak<ConnectionPermit>>,
    abort_started: bool,
    script: Option<Arc<LifecycleScript>>,
}

impl ServerSupervisor {
    pub(super) fn new(
        listener: tokio::net::TcpListener,
        dispatch: ServerDispatch,
        tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
        snapshot: ServerContextSnapshot,
    ) -> (Self, tokio::sync::watch::Sender<ServerControl>) {
        let script = listener
            .local_addr()
            .ok()
            .and_then(super::mock::lifecycle_script);
        let (control_sender, control_receiver) =
            tokio::sync::watch::channel(ServerControl::Running);
        let owner_control = control_sender.clone();
        let (registration_sender, registration_receiver) = tokio::sync::mpsc::channel(32);
        let connection_limit = super::server::make_conn_limit(snapshot.connection_limit());
        let context = Arc::new(snapshot.connection_context());
        let keepalive_timeout = snapshot.keepalive_timeout();
        let shutdown_timeout = snapshot.shutdown_timeout();
        let runtime_shutdown = snapshot.runtime_shutdown();
        (
            Self {
                listener: Some(listener),
                dispatch: Arc::new(dispatch),
                context,
                tls_acceptor,
                keepalive_timeout,
                shutdown_timeout,
                runtime_shutdown,
                runtime: snapshot.runtime,
                connection_limit,
                pending: None,
                control_sender,
                control_receiver,
                registration_sender: Some(registration_sender),
                registration_receiver,
                registration_closed: false,
                tasks: OwnedHttpTasks::new(),
                mode: ShutdownMode::Running,
                terminal: TerminalOutcome::Success,
                deadline: None,
                rejected_connections: Vec::new(),
                abort_started: false,
                script,
            },
            owner_control,
        )
    }

    pub(super) async fn run(mut self) -> Result<(), RuntimeError> {
        let result = AssertUnwindSafe(self.run_core()).catch_unwind().await;
        match result {
            Ok(result) => result,
            Err(payload) => {
                self.begin_abort(None).await;
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
            self.drain_owned().await;
            return ControlFlow::Break(self.finish().await);
        }
        if self.graceful_drain_complete() {
            return self.settle_graceful_drain().await;
        }
        LifecycleScript::pause_at(
            self.script.as_deref(),
            LifecycleCheckpoint::BeforeSupervisorSelect,
        )
        .await;
        self.raise_supervisor_fault();
        let event = self.select_event().await;
        self.apply_event(event).await;
        ControlFlow::Continue(())
    }

    /// Whether a graceful drain has nothing of its own left to wait for.
    ///
    /// A predicate and nothing else: the registration channel can still hold a
    /// ticket submitted before admission closed, and disposing of that ticket
    /// is the caller's step, not a side effect of asking the question.
    fn graceful_drain_complete(&self) -> bool {
        self.mode == ShutdownMode::Graceful && self.tasks.is_empty()
    }

    /// Finish the graceful drain, unless a late upgrade ticket is still
    /// buffered. Rejecting one hands its task back to be joined, so the drain
    /// is reconsidered on the next pass rather than declared over here.
    async fn settle_graceful_drain(&mut self) -> ControlFlow<Result<(), RuntimeError>> {
        match self.registration_receiver.try_recv() {
            Ok(ticket) => {
                self.reject_ticket(ticket);
                ControlFlow::Continue(())
            }
            Err(
                tokio::sync::mpsc::error::TryRecvError::Empty
                | tokio::sync::mpsc::error::TryRecvError::Disconnected,
            ) => ControlFlow::Break(self.finish().await),
        }
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
            event = select_owned_work_event(&mut self.registration_receiver, self.registration_closed, &mut self.tasks, self.script.as_deref()) => event,
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
            event = select_owned_work_event(&mut self.registration_receiver, self.registration_closed, &mut self.tasks, self.script.as_deref()) => event,
        }
    }

    async fn apply_event(&mut self, event: SupervisorEvent) {
        match event {
            SupervisorEvent::Deadline => self.handle_deadline().await,
            SupervisorEvent::Control(ServerControl::Abort) => {
                self.begin_abort(Some(TerminalOutcome::Cancelled)).await;
            }
            SupervisorEvent::Control(ServerControl::Graceful) => self.enter_graceful(None).await,
            SupervisorEvent::Runtime(selected_at) => {
                self.runtime_shutdown = None;
                self.enter_graceful_at(None, selected_at).await;
            }
            SupervisorEvent::ScriptWake
            | SupervisorEvent::Control(ServerControl::Running)
            | SupervisorEvent::Task(None) => {}
            SupervisorEvent::Accept(result) => self.handle_accept(result).await,
            SupervisorEvent::Permit(result) => self.handle_permit(result).await,
            SupervisorEvent::Registration(Some(ticket)) => self.handle_ticket(ticket).await,
            SupervisorEvent::Registration(None) => self.registration_closed = true,
            SupervisorEvent::Task(Some(completion)) => {
                self.handle_task_completion(completion).await;
            }
        }
    }

    /// Answer the shutdown deadline: escalate a graceful drain into an abort,
    /// or — already aborting — stop waiting on rejected connections and force
    /// every owned task down.
    ///
    /// The forced arm is why an abort carries a deadline at all. A connection's
    /// answer to abort is a protocol-level graceful shutdown, so one serving a
    /// long-lived response holds its permit for as long as that response lives,
    /// and waiting for every rejection to release would never end.
    async fn handle_deadline(&mut self) {
        match self.mode {
            ShutdownMode::Abort => self.force_abort(),
            ShutdownMode::Running | ShutdownMode::Graceful => {
                self.begin_abort(Some(TerminalOutcome::Timeout)).await;
            }
        }
    }

    async fn handle_accept(
        &mut self,
        result: Result<(tokio::net::TcpStream, std::net::SocketAddr), std::io::Error>,
    ) {
        match result {
            Ok((stream, remote_addr)) => self.handle_accepted(stream, remote_addr).await,
            Err(error) if crate::error::is_transient_accept_error(&error) => {
                tracing::warn!(error = %error, "accept: fd limit reached, backing off");
                // EMFILE/ENFILE retry throttling is product behavior, not an
                // ordering barrier; immediate retries would spin the runtime.
                tokio::time::sleep(TRANSIENT_ACCEPT_BACKOFF).await;
            }
            Err(error) => self.enter_graceful(Some(RuntimeError::Io(error))).await,
        }
    }

    /// Take a freshly accepted socket to whichever step answers the connection
    /// limit: parking it to wait for a permit, or serving it straight away.
    ///
    /// A limited socket is only admitted, not serviced — admission is asked
    /// again on the far side of the permit wait, which is a real suspension the
    /// runtime shutdown signal can fire across — so the registration sender that
    /// admission produced is taken then rather than carried across it.
    async fn handle_accepted(
        &mut self,
        stream: tokio::net::TcpStream,
        remote_addr: std::net::SocketAddr,
    ) {
        LifecycleScript::pause_at(self.script.as_deref(), LifecycleCheckpoint::AfterAccept).await;
        match self.connection_limit {
            Some(_) => self.park_for_permit(stream, remote_addr).await,
            None => self.admit_and_spawn(stream, remote_addr, None).await,
        }
    }

    /// Hold an admitted socket until the connection limit has a permit for it.
    async fn park_for_permit(
        &mut self,
        stream: tokio::net::TcpStream,
        remote_addr: std::net::SocketAddr,
    ) {
        if let Some((stream, _)) = self.admit_or_close(stream).await {
            self.pending = Some(PendingAccepted {
                stream,
                remote_addr,
            });
        }
    }

    /// Hand back an accepted socket, together with the sender that registers
    /// its upgrades, only while admission is open — and close the socket
    /// otherwise.
    ///
    /// One question, asked once, answers both. `close_admission` takes the
    /// registration sender in the same step that publishes the transition
    /// `admission_is_open` reads, so the sender is present for exactly as long
    /// as admission is open, and a connection spawned from this pair never has
    /// to ask either half again. Every refusal of an accepted socket goes
    /// through here too, so a socket this server will not serve is always shut
    /// down rather than dropped raw.
    async fn admit_or_close(
        &self,
        stream: tokio::net::TcpStream,
    ) -> Option<(
        tokio::net::TcpStream,
        tokio::sync::mpsc::Sender<UpgradeTicket>,
    )> {
        let admitted = match self.admission_is_open() {
            true => self.registration_sender.clone(),
            false => None,
        };
        match admitted {
            Some(registration) => Some((stream, registration)),
            None => {
                close_socket(stream).await;
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
        stream: tokio::net::TcpStream,
        remote_addr: std::net::SocketAddr,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) {
        LifecycleScript::pause_at(self.script.as_deref(), LifecycleCheckpoint::AfterPermit).await;
        if let Some((stream, registration)) = self.admit_or_close(stream).await {
            self.spawn_connection(stream, remote_addr, permit, registration)
                .await;
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
        stream: tokio::net::TcpStream,
        remote_addr: std::net::SocketAddr,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
        registration: tokio::sync::mpsc::Sender<UpgradeTicket>,
    ) {
        LifecycleScript::pause_at(
            self.script.as_deref(),
            LifecycleCheckpoint::KeepaliveTimeoutConfigured(self.keepalive_timeout),
        )
        .await;
        // One subscription is this connection's whole shutdown authority: the
        // lifecycle carries it for the upgrade path and the serve path takes it
        // by value, so no reader has to ask a second time or find it missing.
        let control = self.control_sender.subscribe();
        let lifecycle = ConnectionLifecycle::owned(
            ConnectionPermit::new(permit),
            control.clone(),
            registration,
            self.script.clone(),
        );
        let state = super::conn::ConnectionState::new(
            Arc::clone(&self.dispatch),
            Arc::clone(&self.context),
            lifecycle,
            self.keepalive_timeout,
            Some(remote_addr.ip()),
        );
        let future =
            super::conn::serve_owned_connection(stream, self.tls_acceptor.clone(), state, control);
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
        self.tasks.insert(handle);
        if matches!(fault, Some(LifecycleFault::CancelNextOwnedTask)) {
            abort.abort();
        }
    }

    async fn handle_ticket(&mut self, ticket: UpgradeTicket) {
        let mut parts = ticket.into_parts();
        let handle = match parts.handle.take() {
            Some(handle) => handle,
            None => return,
        };
        self.tasks
            .insert_registered(handle, Arc::clone(&parts.state));
        LifecycleScript::pause_at(
            self.script.as_deref(),
            LifecycleCheckpoint::BeforeUpgradeAcknowledge,
        )
        .await;
        let decision_requested = parts
            .decision_request
            .take()
            .is_some_and(|request| request.send(()).is_ok());
        let decision_ready = match parts.decision_ready.take() {
            Some(decision_ready) => decision_ready.await.is_ok(),
            None => false,
        };
        self.raise_supervisor_fault();
        let admitted = decision_requested
            && decision_ready
            && self.admission_is_open()
            && parts
                .state
                .compare_exchange(
                    UPGRADE_PENDING,
                    UPGRADE_ADMITTED,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                )
                .is_ok();
        match admitted {
            true => Self::acknowledge_upgrade(&mut parts),
            false => self.reject_upgrade(&mut parts),
        }
    }

    fn acknowledge_upgrade(parts: &mut UpgradeTicketParts) {
        let sent = parts
            .acknowledgement
            .take()
            .is_some_and(|sender| sender.send(RegistrationDecision::Admitted).is_ok());
        if !sent {
            cancel_upgrade(&parts.state, &parts.abort);
        }
    }

    /// Refuse a ticket the supervisor already holds the task for.
    ///
    /// The rejection is tracked against the abort wait when this supervisor is
    /// already aborting — by control or by an expired deadline — because a
    /// refused bridge still holds its connection's permit until the connection
    /// itself lets go.
    fn reject_upgrade(&mut self, parts: &mut UpgradeTicketParts) {
        if self.rejection_requires_abort() {
            self.rejected_connections.push(parts.connection.clone());
        }
        Self::cancel_and_answer(parts);
    }

    /// Refuse a ticket taken straight off the registration channel, adopting
    /// the task it carries so the drain still joins it.
    ///
    /// This one tracks the rejection against `mode` rather than the published
    /// control: a ticket is only pulled off the channel by a drain that has
    /// already decided how it is ending.
    fn reject_ticket(&mut self, ticket: UpgradeTicket) {
        let mut parts = ticket.into_parts();
        if self.mode == ShutdownMode::Abort {
            self.rejected_connections.push(parts.connection.clone());
        }
        if let Some(handle) = parts.handle.take() {
            self.tasks
                .insert_registered(handle, Arc::clone(&parts.state));
        }
        Self::cancel_and_answer(&mut parts);
    }

    /// Cancel a ticket's bridge and tell its registrar so.
    ///
    /// One definition for both refusals. The answer is what lets the registrar
    /// produce its own `503` instead of waiting on an acknowledgement that never
    /// comes. A closed answer channel means the registrar is already gone, and
    /// the cancellation above is the whole disposition either way.
    fn cancel_and_answer(parts: &mut UpgradeTicketParts) {
        cancel_upgrade(&parts.state, &parts.abort);
        if let Some(sender) = parts.acknowledgement.take() {
            let _ = sender.send(RegistrationDecision::Rejected);
        }
    }

    /// Read one task completion as the failure it carries, or as nothing to
    /// report.
    ///
    /// Reading and disposal are separate because only the reading is shared.
    /// The running supervisor turns a failure into the candidate that explains
    /// the shutdown it starts; the post-panic drain already has its result and
    /// can only log. One classifier keeps the two from disagreeing about which
    /// cancellations are expected.
    fn classify_completion(&self, completion: OwnedTaskCompletion) -> Option<RuntimeError> {
        match completion.result {
            Ok(()) => None,
            Err(error)
                if error.is_cancelled()
                    && (self.tasks.supervisor_aborted || completion.expected_cancellation) =>
            {
                None
            }
            // Every other join failure — panic or unexpected cancellation —
            // reads through the one translation, so the cancellation message
            // has a single definition.
            Err(error) => Some(join_panic_to_error(error)),
        }
    }

    /// Dispose of one task completion the running supervisor observed. A join
    /// failure starts a graceful shutdown and is recorded as its candidate; a
    /// clean completion must not start one by arriving.
    async fn handle_task_completion(&mut self, completion: OwnedTaskCompletion) {
        match self.classify_completion(completion) {
            Some(error) => self.enter_graceful(Some(error)).await,
            None => {}
        }
    }

    async fn enter_graceful(&mut self, candidate: Option<RuntimeError>) {
        self.enter_graceful_at(candidate, tokio::time::Instant::now())
            .await;
    }

    async fn enter_graceful_at(
        &mut self,
        candidate: Option<RuntimeError>,
        selected_at: tokio::time::Instant,
    ) {
        if let Some(candidate) = candidate {
            self.record_candidate(candidate);
        }
        if self.mode != ShutdownMode::Running {
            return;
        }
        self.mode = ShutdownMode::Graceful;
        self.deadline = Some(selected_at + self.shutdown_timeout);
        self.close_admission(ServerControl::send_graceful).await;
    }

    /// Stop admitting and publish the control transition this shutdown makes.
    ///
    /// The five steps travel together: a listener or registration sender left
    /// behind would keep admitting work the transition just refused, the edge
    /// the supervisor publishes to its connections has to be consumed here or
    /// its own selector answers a request it made itself, and a socket already
    /// accepted into `pending` is one this server will now never serve, so it
    /// is shut down here rather than left to be dropped raw. Both transitions
    /// differ only in which request they publish.
    async fn close_admission(&mut self, publish: fn(&tokio::sync::watch::Sender<ServerControl>)) {
        self.listener.take();
        self.registration_sender.take();
        publish(&self.control_sender);
        self.control_receiver.borrow_and_update();
        self.close_pending().await;
    }

    /// Enter abort: stop admitting, tell every connection to shut down, and
    /// close out whatever was already accepted or buffered.
    ///
    /// The deadline is rearmed rather than cleared, because abort asks a
    /// connection for a protocol-level shutdown it can outlast — without a
    /// deadline the forced abort would wait forever on a permit that a
    /// long-lived response never releases.
    async fn begin_abort(&mut self, outcome: Option<TerminalOutcome>) {
        if let Some(outcome) = outcome {
            self.record_escalation(outcome);
        }
        self.mode = ShutdownMode::Abort;
        self.deadline = Some(tokio::time::Instant::now() + self.shutdown_timeout);
        self.close_admission(ServerControl::send_abort).await;
        self.reject_all_pending().await;
    }

    /// Record the failure a graceful shutdown carries provisionally. First
    /// writer wins: the failure that started the drain is the one that explains
    /// it, and a second failure observed while draining is a consequence of the
    /// first, not a better account of it.
    fn record_candidate(&mut self, candidate: RuntimeError) {
        match self.terminal {
            TerminalOutcome::Success => self.terminal = TerminalOutcome::Fatal(candidate),
            TerminalOutcome::Fatal(_) | TerminalOutcome::Cancelled | TerminalOutcome::Timeout => {}
        }
    }

    /// Record the outcome an escalation to abort reports, replacing whatever a
    /// graceful drain had recorded provisionally.
    ///
    /// The two rules are deliberately opposite. A candidate is provisional
    /// because the drain it belongs to might still finish; an escalation is how
    /// the server actually ended, so an explicit cancel or an expired deadline
    /// is what the caller is told, not the failure that was being drained. A
    /// timeout already recorded is the one exception: the drain is over by
    /// then, so nothing after it can restate why.
    fn record_escalation(&mut self, outcome: TerminalOutcome) {
        match self.terminal {
            TerminalOutcome::Timeout => {}
            TerminalOutcome::Success | TerminalOutcome::Fatal(_) | TerminalOutcome::Cancelled => {
                self.terminal = outcome;
            }
        }
    }

    async fn close_pending(&mut self) {
        if let Some(accepted) = self.pending.take() {
            close_socket(accepted.stream).await;
        }
    }

    fn start_abort_if_ready(&mut self) {
        if self.mode == ShutdownMode::Abort && !self.abort_started && self.rejections_complete() {
            self.force_abort();
        }
    }

    /// Whether every connection this supervisor rejected has released its
    /// permit.
    ///
    /// Pruning as it goes: a released connection can never come back, so
    /// keeping its `Weak` only makes the next pass rescan it.
    fn rejections_complete(&mut self) -> bool {
        self.rejected_connections
            .retain(|connection| connection.strong_count() > 0);
        self.rejected_connections.is_empty()
    }

    /// Abort every owned task, once, and disarm the deadline that was waiting
    /// to do it.
    fn force_abort(&mut self) {
        self.deadline = None;
        self.abort_started = true;
        self.tasks.abort_all();
    }

    fn abort_drain_complete(&self) -> bool {
        self.mode == ShutdownMode::Abort && self.abort_started && self.tasks.is_empty()
    }

    fn rejection_requires_abort(&self) -> bool {
        self.current_control() == ServerControl::Abort
            || self
                .deadline
                .is_some_and(|deadline| deadline <= tokio::time::Instant::now())
    }

    /// Close the registration channel and reject every ticket still on it.
    ///
    /// The one sweep for every path that stops taking tickets: a ticket left
    /// buffered would hold a task nobody joins, whichever way the supervisor is
    /// ending. The sender side is already gone by the time abort reaches here —
    /// `close_admission` takes it — so a closed receiver yields what is buffered
    /// and then `None` without ever suspending, and the flag records that the
    /// selector has nothing left to poll for.
    async fn reject_all_pending(&mut self) {
        self.registration_receiver.close();
        while let Some(ticket) = self.registration_receiver.recv().await {
            self.reject_ticket(ticket);
        }
        self.registration_closed = true;
    }

    async fn drain_owned(&mut self) {
        self.reject_all_pending().await;
        self.tasks.abort_and_drain().await;
    }

    async fn drain_owned_after_panic(&mut self) {
        self.reject_all_pending().await;
        // Preserve protocol-level shutdown after a supervisor fault, but never
        // let non-cooperative work outlive the configured shutdown deadline.
        // The deadline is one timer for the whole drain, selected on by `&mut`:
        // rebuilding it per completion would charge every task that finishes in
        // time a timer registration for a deadline that has not moved.
        let expiry = tokio::time::sleep_until(tokio::time::Instant::now() + self.shutdown_timeout);
        tokio::pin!(expiry);
        while !self.tasks.is_empty() {
            let completion = tokio::select! {
                biased;
                completion = self.tasks.next() => completion,
                () = &mut expiry => None,
            };
            match completion {
                Some(completion) => self.report_post_panic_completion(completion),
                None => break,
            }
        }
        if !self.tasks.is_empty() {
            self.tasks.abort_and_drain().await;
        }
    }

    /// Report a task that failed while the supervisor was already unwinding.
    ///
    /// Logged rather than recorded, because `run` returns the supervisor's own
    /// panic on this path and never reads the terminal outcome: a failure
    /// written there would be dropped with `self`. The message names the drain
    /// so the connection's failure is not read as a second supervisor fault.
    fn report_post_panic_completion(&self, completion: OwnedTaskCompletion) {
        match self.classify_completion(completion) {
            Some(error) => tracing::error!(
                error = %error,
                "owned HTTP task failed during the post-panic drain"
            ),
            None => {}
        }
    }

    fn current_control(&self) -> ServerControl {
        *self.control_sender.borrow()
    }

    fn admission_is_open(&self) -> bool {
        match self.current_control() {
            ServerControl::Running => !self
                .runtime_shutdown
                .as_ref()
                .is_some_and(|shutdown| shutdown.is_fired()),
            ServerControl::Graceful | ServerControl::Abort => false,
        }
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

    fn take_result(&mut self) -> Result<(), RuntimeError> {
        let terminal = std::mem::replace(&mut self.terminal, TerminalOutcome::Success);
        match terminal {
            TerminalOutcome::Success => Ok(()),
            TerminalOutcome::Fatal(error) => Err(error),
            TerminalOutcome::Cancelled => Err(RuntimeError::Cancelled),
            TerminalOutcome::Timeout => Err(RuntimeError::Timeout),
        }
    }

    async fn finish(&mut self) -> Result<(), RuntimeError> {
        let result = self.take_result();
        LifecycleScript::pause_at(
            self.script.as_deref(),
            LifecycleCheckpoint::AfterSupervisorResultSend,
        )
        .await;
        result
    }
}

async fn run_owned_connection<F>(
    future: F,
    fault: Option<LifecycleFault>,
    script: Option<Arc<LifecycleScript>>,
) where
    F: Future<Output = ()>,
{
    match fault {
        Some(LifecycleFault::PanicNextOwnedTask) => {
            std::panic::resume_unwind(Box::new(OWNED_TASK_PANIC));
        }
        Some(LifecycleFault::PanicNextOwnedTaskOpaque) => {
            std::panic::resume_unwind(Box::new(7usize));
        }
        Some(LifecycleFault::CancelNextOwnedTask)
        | Some(LifecycleFault::Accept(_) | LifecycleFault::PanicSupervisorCore)
        | None => future.await,
    }
    LifecycleScript::pause_at(
        script.as_deref(),
        LifecycleCheckpoint::AfterOwnedConnectionFutureCompleted,
    )
    .await;
}

async fn wait_for_script_wake(script: Option<&LifecycleScript>) {
    match script {
        Some(script) => script.wait_for_supervisor_wake().await,
        None => std::future::pending().await,
    }
}

/// Close the transport of a connection this server will not serve.
///
/// A refused socket is shut down rather than dropped, so the peer sees the
/// refusal as a close it can read instead of an unexplained silence. A failed
/// shutdown has nothing left to report to: the socket is going away either way.
async fn close_socket(mut stream: tokio::net::TcpStream) {
    use tokio::io::AsyncWriteExt;
    let _ = stream.shutdown().await;
}

/// Report a refused connection permit and close the socket it was for.
///
/// The semaphore closes only when the server itself is going away, so the
/// accepted socket has no owner left to serve it — and the error naming that is
/// the only account of why this connection was dropped.
async fn refuse_permit(error: &tokio::sync::AcquireError, accepted: Option<PendingAccepted>) {
    tracing::warn!(%error, "connection permit unavailable; closing accepted socket");
    if let Some(accepted) = accepted {
        close_socket(accepted.stream).await;
    }
}

/// Wait for the next event every selector observes first: the shutdown
/// deadline, an owner control request, or runtime shutdown.
///
/// This is the `biased` shutdown priority, stated once. Both selectors race it
/// ahead of their own admission arm, so neither can drift into answering a new
/// connection before a shutdown that is already pending.
async fn select_lifecycle_event(
    deadline: Option<tokio::time::Instant>,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    runtime_shutdown: Option<&ShutdownSignal>,
    script: Option<&LifecycleScript>,
) -> SupervisorEvent {
    tokio::select! {
        biased;
        () = wait_deadline(deadline), if deadline.is_some() => SupervisorEvent::Deadline,
        requested = wait_control(control) => SupervisorEvent::Control(requested),
        () = wait_runtime(runtime_shutdown, script), if runtime_shutdown.is_some() => {
            SupervisorEvent::Runtime(tokio::time::Instant::now())
        }
    }
}

/// Wait for the next event from work this supervisor already owns: an upgrade
/// registration, a finished task, or a test script wake.
///
/// Both selectors race this last, so owned work is answered only once nothing
/// more urgent is ready.
async fn select_owned_work_event(
    registration: &mut tokio::sync::mpsc::Receiver<UpgradeTicket>,
    registration_closed: bool,
    tasks: &mut OwnedHttpTasks,
    script: Option<&LifecycleScript>,
) -> SupervisorEvent {
    tokio::select! {
        biased;
        ticket = registration.recv(), if !registration_closed => {
            SupervisorEvent::Registration(ticket)
        }
        completion = tasks.next(), if !tasks.is_empty() => SupervisorEvent::Task(completion),
        // Deliberately unguarded: it is what keeps this select from being fully
        // disabled, which `select!` answers with a panic. The other two arms
        // need their guards — a closed receiver and an empty task set are both
        // ready at once and would spin — and `wait_for_script_wake` already
        // answers a missing script with `pending()`, so a guard here would only
        // repeat what the future already does.
        () = wait_for_script_wake(script) => SupervisorEvent::ScriptWake,
    }
}

/// Pause at the checkpoint one selected event names, if the test script names
/// one for it. A script wake is the script's own event and has none.
async fn pause_selected(script: Option<&LifecycleScript>, event: &SupervisorEvent) {
    if let Some(checkpoint) = selected_checkpoint(event) {
        LifecycleScript::pause_at(script, checkpoint).await;
    }
}

fn selected_checkpoint(event: &SupervisorEvent) -> Option<LifecycleCheckpoint> {
    match event {
        SupervisorEvent::Deadline => Some(LifecycleCheckpoint::SupervisorSelectedDeadline),
        SupervisorEvent::Control(_) => Some(LifecycleCheckpoint::SupervisorSelectedControl),
        SupervisorEvent::Runtime(_) => Some(LifecycleCheckpoint::SupervisorSelectedRuntime),
        SupervisorEvent::Accept(_) => Some(LifecycleCheckpoint::SupervisorSelectedAccept),
        SupervisorEvent::Permit(_) => Some(LifecycleCheckpoint::SupervisorSelectedPermit),
        SupervisorEvent::Registration(_) => {
            Some(LifecycleCheckpoint::SupervisorSelectedRegistration)
        }
        SupervisorEvent::Task(_) => Some(LifecycleCheckpoint::SupervisorSelectedTask),
        SupervisorEvent::ScriptWake => None,
    }
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Wait for the next control value this server publishes.
///
/// A dropped sender is the one case that has to be silent: no authority is
/// left to answer, and answering with the last value seen — always `Running`
/// for a server that never left it — would tell a caller the server is live
/// when nothing can say so any more. Every reader of this channel goes through
/// here, so that rule is written once.
async fn wait_control(receiver: &mut tokio::sync::watch::Receiver<ServerControl>) -> ServerControl {
    loop {
        match receiver.changed().await {
            Ok(()) => return *receiver.borrow_and_update(),
            Err(_) => std::future::pending().await,
        }
    }
}

/// Wait until this server leaves `Running`, and answer with what it left for.
///
/// The current value is read before any `changed()`: a connection subscribing
/// after the transition has no edge left to observe, and waiting for one would
/// serve it as though the server were still accepting work.
pub(super) async fn wait_shutdown_control(
    receiver: &mut tokio::sync::watch::Receiver<ServerControl>,
) -> ServerControl {
    let mut control = *receiver.borrow_and_update();
    while control == ServerControl::Running {
        control = wait_control(receiver).await;
    }
    control
}

async fn wait_runtime(shutdown: Option<&ShutdownSignal>, script: Option<&LifecycleScript>) {
    match shutdown {
        None => std::future::pending().await,
        Some(shutdown) => {
            shutdown
                .wait_observed(|| {
                    LifecycleScript::pause_at(script, LifecycleCheckpoint::BeforeRuntimeWait)
                })
                .await;
        }
    }
}

async fn accept_next(
    listener: Option<&tokio::net::TcpListener>,
    script: Option<&LifecycleScript>,
) -> Result<(tokio::net::TcpStream, std::net::SocketAddr), std::io::Error> {
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

pub(super) enum SupervisorJoin {
    Camber(AsyncJoinFuture<Result<(), RuntimeError>>),
    Tokio(tokio::task::JoinHandle<Result<(), RuntimeError>>),
    Ready(Ready<Result<(), RuntimeError>>),
}

pub(super) fn poll_supervisor_join(
    join: &mut SupervisorJoin,
    context: &mut Context<'_>,
) -> Poll<Result<(), RuntimeError>> {
    match join {
        // A Camber join failure is already a `RuntimeError`, so flattening it
        // is the inner result or that error unchanged.
        SupervisorJoin::Camber(future) => Pin::new(future)
            .poll(context)
            .map(|result| result.unwrap_or_else(Err)),
        SupervisorJoin::Tokio(handle) => Pin::new(handle).poll(context).map(flatten_tokio_join),
        SupervisorJoin::Ready(future) => Pin::new(future).poll(context),
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

pub(super) fn supervisor_join_probe(
    probe: SupervisorJoinProbe,
) -> super::server::ServerHandleFuture {
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
    super::server::ServerHandleFuture::from_join(join)
}

async fn string_panic_probe() -> Result<(), RuntimeError> {
    std::panic::resume_unwind(Box::new(SUPERVISOR_PROBE_PANIC));
}

async fn opaque_panic_probe() -> Result<(), RuntimeError> {
    std::panic::resume_unwind(Box::new(13usize));
}

#[cfg(feature = "ws")]
pub(super) fn unavailable_response() -> hyper::Response<super::body::HyperResponseBody> {
    close_response(500)
}

#[cfg(feature = "ws")]
pub(super) fn rejected_response() -> hyper::Response<super::body::HyperResponseBody> {
    close_response(503)
}

#[cfg(feature = "ws")]
fn close_response(status: u16) -> hyper::Response<super::body::HyperResponseBody> {
    let response = Response::empty_raw(status).with_header("Connection", "close");
    super::handle::to_hyper_full(response)
}
