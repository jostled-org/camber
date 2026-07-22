use std::future::{Future, IntoFuture, Ready};
use std::panic::AssertUnwindSafe;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
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
use crate::resource::HealthState;
use crate::runtime_state::{DEFAULT_KEEPALIVE_TIMEOUT, DEFAULT_SHUTDOWN_TIMEOUT};
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

pub(super) struct ServerContextSnapshot {
    runtime_shutdown: Option<RuntimeShutdown>,
    shutdown_timeout: Duration,
    keepalive_timeout: Duration,
    connection_limit: Option<usize>,
    tracing_enabled: bool,
    metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    #[cfg(feature = "profiling")]
    profiling_enabled: bool,
    health_state: Option<HealthState>,
    buffers: BufferConfig,
    is_tls: bool,
}

#[derive(Clone)]
struct RuntimeShutdown {
    requested: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

impl ServerContextSnapshot {
    pub(super) fn capture(buffers: BufferConfig, is_tls: bool) -> Self {
        match runtime::has_runtime() {
            true => Self::from_camber(buffers, is_tls),
            false => Self::standalone(buffers, is_tls),
        }
    }

    pub(super) fn standalone(buffers: BufferConfig, is_tls: bool) -> Self {
        Self {
            runtime_shutdown: None,
            shutdown_timeout: DEFAULT_SHUTDOWN_TIMEOUT,
            keepalive_timeout: DEFAULT_KEEPALIVE_TIMEOUT,
            connection_limit: None,
            tracing_enabled: false,
            metrics_handle: None,
            #[cfg(feature = "profiling")]
            profiling_enabled: false,
            health_state: None,
            buffers,
            is_tls,
        }
    }

    fn from_camber(buffers: BufferConfig, is_tls: bool) -> Self {
        let current = runtime::current_runtime();
        Self {
            runtime_shutdown: Some(RuntimeShutdown {
                requested: Arc::clone(&current.shutdown),
                notify: Arc::clone(&current.shutdown_notify),
            }),
            shutdown_timeout: current.config.shutdown_timeout,
            keepalive_timeout: current.config.keepalive_timeout,
            connection_limit: current.config.connection_limit,
            tracing_enabled: current.config.tracing_enabled,
            metrics_handle: current.metrics_handle.clone(),
            #[cfg(feature = "profiling")]
            profiling_enabled: current.config.profiling_enabled,
            health_state: current.health_state.clone(),
            buffers,
            is_tls,
        }
    }

    fn connection_context(&self) -> super::handle::ConnCtx {
        super::handle::ConnCtx {
            tracing_enabled: self.tracing_enabled,
            metrics_handle: self.metrics_handle.clone(),
            #[cfg(feature = "profiling")]
            profiling_enabled: self.profiling_enabled,
            max_request_body: self.buffers.max_request_body,
            sse_buffer_size: self.buffers.sse_buffer_size,
            #[cfg(feature = "ws")]
            ws_buffer_size: self.buffers.ws_buffer_size,
            health_state: self.health_state.clone(),
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

    pub(super) fn control(&self) -> Option<tokio::sync::watch::Receiver<ServerControl>> {
        self.control.clone()
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
        self.state.store(UPGRADE_CANCELLED, Ordering::Release);
        self.abort.abort();
        let handle = self.handle.take();
        if let Some(handle) = handle {
            let _ = handle.await;
        }
    }
}

impl Drop for UpgradeTicket {
    fn drop(&mut self) {
        if self.handle.is_some() {
            self.state.store(UPGRADE_CANCELLED, Ordering::Release);
            self.abort.abort();
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
        pause(
            &self.script,
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

    fn abort_expected(&self) {
        if let Some(state) = self.state.as_ref() {
            state.store(UPGRADE_CANCELLED, Ordering::Release);
        }
        if let Some(abort) = self.abort.as_ref() {
            abort.abort();
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
        self.state.store(UPGRADE_CANCELLED, Ordering::Release);
        self.abort.abort();
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

    pub(super) fn cancel(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(state) = self.state.as_ref() {
            state.store(UPGRADE_CANCELLED, Ordering::Release);
        }
        if let Some(abort) = self.abort.as_ref() {
            abort.abort();
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

impl Drop for OwnedHttpTasks {
    fn drop(&mut self) {
        self.tasks.iter().for_each(|task| task.handle.abort());
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
    runtime_shutdown: Option<RuntimeShutdown>,
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
        let connection_limit = snapshot
            .connection_limit
            .map(|limit| Arc::new(tokio::sync::Semaphore::new(limit)));
        let context = Arc::new(snapshot.connection_context());
        (
            Self {
                listener: Some(listener),
                dispatch: Arc::new(dispatch),
                context,
                tls_acceptor,
                keepalive_timeout: snapshot.keepalive_timeout,
                shutdown_timeout: snapshot.shutdown_timeout,
                runtime_shutdown: snapshot.runtime_shutdown,
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
                self.announce_abort();
                self.close_pending().await;
                self.drain_owned_after_panic().await;
                Err(panic_to_error(payload))
            }
        }
    }

    async fn run_core(&mut self) -> Result<(), RuntimeError> {
        loop {
            self.start_abort_if_ready();
            if self.abort_drain_complete() {
                self.drain_owned().await;
                return self.finish().await;
            }
            if self.graceful_drain_complete() {
                return self.finish().await;
            }
            pause(&self.script, LifecycleCheckpoint::BeforeSupervisorSelect).await;
            self.raise_supervisor_fault();
            let event = self.select_event().await;
            let should_finish = self.apply_event(event).await;
            if should_finish {
                self.drain_owned().await;
                return self.finish().await;
            }
        }
    }

    fn graceful_drain_complete(&mut self) -> bool {
        if self.mode != ShutdownMode::Graceful || !self.tasks.is_empty() {
            return false;
        }
        match self.registration_receiver.try_recv() {
            Ok(ticket) => {
                self.reject_ticket(ticket);
                false
            }
            Err(
                tokio::sync::mpsc::error::TryRecvError::Empty
                | tokio::sync::mpsc::error::TryRecvError::Disconnected,
            ) => true,
        }
    }

    async fn select_event(&mut self) -> SupervisorEvent {
        match self.pending.is_some() {
            true => self.select_pending_event().await,
            false => self.select_listener_event().await,
        }
    }

    async fn select_listener_event(&mut self) -> SupervisorEvent {
        let listener = self.listener.as_ref();
        tokio::select! {
            biased;
            () = wait_deadline(self.deadline), if self.deadline.is_some() => {
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedDeadline).await;
                SupervisorEvent::Deadline
            }
            control = wait_control(&mut self.control_receiver) => {
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedControl).await;
                SupervisorEvent::Control(control)
            }
            () = wait_runtime(self.runtime_shutdown.as_ref(), self.script.as_ref()), if self.runtime_shutdown.is_some() => {
                let selected_at = tokio::time::Instant::now();
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedRuntime).await;
                SupervisorEvent::Runtime(selected_at)
            }
            accepted = accept_next(listener, self.script.as_ref()), if listener.is_some() && self.mode == ShutdownMode::Running => {
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedAccept).await;
                SupervisorEvent::Accept(accepted)
            }
            ticket = self.registration_receiver.recv(), if !self.registration_closed => {
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedRegistration).await;
                SupervisorEvent::Registration(ticket)
            }
            completion = self.tasks.next(), if !self.tasks.is_empty() => {
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedTask).await;
                SupervisorEvent::Task(completion)
            }
            () = wait_for_script_wake(self.script.as_ref()), if self.script.is_some() => {
                SupervisorEvent::ScriptWake
            }
        }
    }

    async fn select_pending_event(&mut self) -> SupervisorEvent {
        let semaphore = self.connection_limit.as_ref();
        tokio::select! {
            biased;
            () = wait_deadline(self.deadline), if self.deadline.is_some() => {
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedDeadline).await;
                SupervisorEvent::Deadline
            }
            control = wait_control(&mut self.control_receiver) => {
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedControl).await;
                SupervisorEvent::Control(control)
            }
            () = wait_runtime(self.runtime_shutdown.as_ref(), self.script.as_ref()), if self.runtime_shutdown.is_some() => {
                let selected_at = tokio::time::Instant::now();
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedRuntime).await;
                SupervisorEvent::Runtime(selected_at)
            }
            permit = crate::net::accept::acquire_connection_permit(semaphore, self.script.as_ref()), if semaphore.is_some() => {
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedPermit).await;
                SupervisorEvent::Permit(permit)
            }
            ticket = self.registration_receiver.recv(), if !self.registration_closed => {
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedRegistration).await;
                SupervisorEvent::Registration(ticket)
            }
            completion = self.tasks.next(), if !self.tasks.is_empty() => {
                pause(&self.script, LifecycleCheckpoint::SupervisorSelectedTask).await;
                SupervisorEvent::Task(completion)
            }
            () = wait_for_script_wake(self.script.as_ref()), if self.script.is_some() => {
                SupervisorEvent::ScriptWake
            }
        }
    }

    async fn apply_event(&mut self, event: SupervisorEvent) -> bool {
        match event {
            SupervisorEvent::Deadline => {
                self.begin_abort(Some(TerminalOutcome::Timeout));
                self.close_pending().await;
                self.reject_buffered_tickets();
                false
            }
            SupervisorEvent::Control(ServerControl::Abort) => {
                self.begin_abort(Some(TerminalOutcome::Cancelled));
                self.close_pending().await;
                self.reject_buffered_tickets();
                false
            }
            SupervisorEvent::Control(ServerControl::Graceful) => {
                self.enter_graceful(None);
                self.close_pending().await;
                false
            }
            SupervisorEvent::Runtime(selected_at) => {
                self.runtime_shutdown = None;
                self.enter_graceful_at(None, selected_at);
                self.close_pending().await;
                false
            }
            SupervisorEvent::ScriptWake
            | SupervisorEvent::Control(ServerControl::Running)
            | SupervisorEvent::Task(None) => false,
            SupervisorEvent::Accept(result) => {
                self.handle_accept(result).await;
                false
            }
            SupervisorEvent::Permit(result) => {
                self.handle_permit(result).await;
                false
            }
            SupervisorEvent::Registration(Some(ticket)) => {
                self.handle_ticket(ticket).await;
                false
            }
            SupervisorEvent::Registration(None) => {
                self.registration_closed = true;
                false
            }
            SupervisorEvent::Task(Some(completion)) => {
                self.handle_task_completion(completion);
                false
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
                tracing::warn!("accept: fd limit reached, backing off");
                // EMFILE/ENFILE retry throttling is product behavior, not an
                // ordering barrier; immediate retries would spin the runtime.
                tokio::time::sleep(TRANSIENT_ACCEPT_BACKOFF).await;
            }
            Err(error) => self.enter_graceful(Some(RuntimeError::Io(error))),
        }
    }

    async fn handle_accepted(
        &mut self,
        stream: tokio::net::TcpStream,
        remote_addr: std::net::SocketAddr,
    ) {
        pause(&self.script, LifecycleCheckpoint::AfterAccept).await;
        if !self.admission_is_open() {
            close_socket(stream).await;
            return;
        }
        match self.connection_limit {
            Some(_) => {
                self.pending = Some(PendingAccepted {
                    stream,
                    remote_addr,
                });
            }
            None => self.handle_unlimited_connection(stream, remote_addr).await,
        }
    }

    async fn handle_unlimited_connection(
        &mut self,
        stream: tokio::net::TcpStream,
        remote_addr: std::net::SocketAddr,
    ) {
        pause(&self.script, LifecycleCheckpoint::AfterPermit).await;
        if !self.admission_is_open() {
            close_socket(stream).await;
            return;
        }
        self.spawn_connection(stream, remote_addr, None).await;
    }

    async fn handle_permit(
        &mut self,
        result: Result<tokio::sync::OwnedSemaphorePermit, tokio::sync::AcquireError>,
    ) {
        let accepted = self.pending.take();
        let permit = match result {
            Ok(permit) => permit,
            Err(_) => return,
        };
        pause(&self.script, LifecycleCheckpoint::AfterPermit).await;
        let accepted = match (self.admission_is_open(), accepted) {
            (true, Some(accepted)) => accepted,
            (false, Some(accepted)) => {
                close_socket(accepted.stream).await;
                return;
            }
            (_, None) => return,
        };
        self.spawn_connection(accepted.stream, accepted.remote_addr, Some(permit))
            .await;
    }

    async fn spawn_connection(
        &mut self,
        stream: tokio::net::TcpStream,
        remote_addr: std::net::SocketAddr,
        permit: Option<tokio::sync::OwnedSemaphorePermit>,
    ) {
        pause(
            &self.script,
            LifecycleCheckpoint::KeepaliveTimeoutConfigured(self.keepalive_timeout),
        )
        .await;
        let registration = match self.registration_sender.as_ref() {
            Some(sender) => sender.clone(),
            None => return,
        };
        let lifecycle = ConnectionLifecycle::owned(
            ConnectionPermit::new(permit),
            self.control_sender.subscribe(),
            registration,
            self.script.clone(),
        );
        let future = super::conn::serve_owned_connection(
            stream,
            self.tls_acceptor.clone(),
            Arc::clone(&self.dispatch),
            Arc::clone(&self.context),
            lifecycle,
            self.keepalive_timeout,
            remote_addr.ip(),
        );
        let fault = self
            .script
            .as_ref()
            .and_then(|script| script.take_owned_task_fault());
        let handle = tokio::spawn(run_owned_connection(future, fault));
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
        pause(&self.script, LifecycleCheckpoint::BeforeUpgradeAcknowledge).await;
        let decision_requested = parts
            .decision_request
            .take()
            .is_some_and(|request| request.send(()).is_ok());
        let decision_ready = match parts.decision_ready.take() {
            Some(decision_ready) => decision_ready.await.is_ok(),
            None => false,
        };
        self.raise_supervisor_fault();
        let runtime_requested = self
            .runtime_shutdown
            .as_ref()
            .is_some_and(|shutdown| shutdown.requested.load(Ordering::Acquire));
        let admission_open = self.current_control() == ServerControl::Running && !runtime_requested;
        let admitted = decision_requested
            && decision_ready
            && admission_open
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
            false => self.reject_upgrade(&mut parts).await,
        }
    }

    fn acknowledge_upgrade(parts: &mut UpgradeTicketParts) {
        let sent = parts
            .acknowledgement
            .take()
            .is_some_and(|sender| sender.send(RegistrationDecision::Admitted).is_ok());
        if !sent {
            parts.state.store(UPGRADE_CANCELLED, Ordering::Release);
            parts.abort.abort();
        }
    }

    async fn reject_upgrade(&mut self, parts: &mut UpgradeTicketParts) {
        parts.state.store(UPGRADE_CANCELLED, Ordering::Release);
        parts.abort.abort();
        if self.rejection_requires_abort() {
            self.rejected_connections.push(parts.connection.clone());
        }
        if let Some(sender) = parts.acknowledgement.take() {
            let _ = sender.send(RegistrationDecision::Rejected);
        }
    }

    fn reject_ticket(&mut self, ticket: UpgradeTicket) {
        let mut parts = ticket.into_parts();
        parts.state.store(UPGRADE_CANCELLED, Ordering::Release);
        parts.abort.abort();
        if self.mode == ShutdownMode::Abort {
            self.rejected_connections.push(parts.connection.clone());
        }
        if let Some(handle) = parts.handle.take() {
            self.tasks
                .insert_registered(handle, Arc::clone(&parts.state));
        }
        if let Some(sender) = parts.acknowledgement.take() {
            let _ = sender.send(RegistrationDecision::Rejected);
        }
    }

    fn handle_task_completion(&mut self, completion: OwnedTaskCompletion) {
        match completion.result {
            Ok(()) => {}
            Err(error)
                if error.is_cancelled()
                    && (self.tasks.supervisor_aborted || completion.expected_cancellation) => {}
            Err(error) if error.is_cancelled() => self.enter_graceful(Some(
                RuntimeError::TaskPanicked("owned HTTP task cancelled unexpectedly".into()),
            )),
            Err(error) => self.enter_graceful(Some(join_panic_to_error(error))),
        }
    }

    fn enter_graceful(&mut self, candidate: Option<RuntimeError>) {
        self.enter_graceful_at(candidate, tokio::time::Instant::now());
    }

    fn enter_graceful_at(
        &mut self,
        candidate: Option<RuntimeError>,
        selected_at: tokio::time::Instant,
    ) {
        if let (Some(candidate), TerminalOutcome::Success) = (candidate, &self.terminal) {
            self.terminal = TerminalOutcome::Fatal(candidate);
        }
        if self.mode != ShutdownMode::Running {
            return;
        }
        self.mode = ShutdownMode::Graceful;
        self.deadline = Some(selected_at + self.shutdown_timeout);
        self.listener.take();
        self.registration_sender.take();
        ServerControl::send_graceful(&self.control_sender);
        self.control_receiver.borrow_and_update();
    }

    fn begin_abort(&mut self, outcome: Option<TerminalOutcome>) {
        if let (false, Some(outcome)) = (matches!(self.terminal, TerminalOutcome::Timeout), outcome)
        {
            self.terminal = outcome;
        }
        self.mode = ShutdownMode::Abort;
        self.deadline = None;
        self.listener.take();
        self.registration_sender.take();
        ServerControl::send_abort(&self.control_sender);
        self.control_receiver.borrow_and_update();
    }

    fn announce_abort(&mut self) {
        self.mode = ShutdownMode::Abort;
        self.deadline = None;
        self.listener.take();
        self.registration_sender.take();
        ServerControl::send_abort(&self.control_sender);
        self.control_receiver.borrow_and_update();
    }

    async fn close_pending(&mut self) {
        if let Some(accepted) = self.pending.take() {
            close_socket(accepted.stream).await;
        }
    }

    fn reject_buffered_tickets(&mut self) {
        self.registration_sender.take();
        self.registration_receiver.close();
        while let Ok(ticket) = self.registration_receiver.try_recv() {
            self.reject_ticket(ticket);
        }
        self.registration_closed = true;
    }

    fn start_abort_if_ready(&mut self) {
        let rejections_complete = self
            .rejected_connections
            .iter()
            .all(|connection| connection.strong_count() == 0);
        if self.mode == ShutdownMode::Abort && !self.abort_started && rejections_complete {
            self.tasks.abort_all();
            self.abort_started = true;
        }
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

    async fn drain_owned(&mut self) {
        self.registration_receiver.close();
        while let Some(ticket) = self.registration_receiver.recv().await {
            self.reject_ticket(ticket);
        }
        self.tasks.abort_and_drain().await;
    }

    async fn drain_owned_after_panic(&mut self) {
        self.registration_receiver.close();
        while let Some(ticket) = self.registration_receiver.recv().await {
            self.reject_ticket(ticket);
        }
        // Preserve protocol-level shutdown after a supervisor fault, but never
        // let non-cooperative work outlive the configured shutdown deadline.
        let deadline = tokio::time::Instant::now() + self.shutdown_timeout;
        while !self.tasks.is_empty() {
            let completion = tokio::select! {
                biased;
                completion = self.tasks.next() => completion,
                () = tokio::time::sleep_until(deadline) => None,
            };
            match completion {
                Some(completion) => self.handle_task_completion(completion),
                None => break,
            }
        }
        if !self.tasks.is_empty() {
            self.tasks.abort_and_drain().await;
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
                .is_some_and(|shutdown| shutdown.requested.load(Ordering::Acquire)),
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
        pause(&self.script, LifecycleCheckpoint::AfterSupervisorResultSend).await;
        result
    }
}

async fn run_owned_connection<F>(future: F, fault: Option<LifecycleFault>)
where
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
}

async fn pause(script: &Option<Arc<LifecycleScript>>, checkpoint: LifecycleCheckpoint) {
    if let Some(script) = script {
        script.pause(checkpoint).await;
    }
}

async fn wait_for_script_wake(script: Option<&Arc<LifecycleScript>>) {
    match script {
        Some(script) => script.wait_for_supervisor_wake().await,
        None => std::future::pending().await,
    }
}

async fn close_socket(stream: tokio::net::TcpStream) {
    let handle = tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let mut stream = stream;
        let _ = stream.shutdown().await;
    });
    let _ = handle.await;
}

async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

async fn wait_control(receiver: &mut tokio::sync::watch::Receiver<ServerControl>) -> ServerControl {
    loop {
        match receiver.changed().await {
            Ok(()) => return *receiver.borrow_and_update(),
            Err(_) => std::future::pending().await,
        }
    }
}

async fn wait_runtime(shutdown: Option<&RuntimeShutdown>, script: Option<&Arc<LifecycleScript>>) {
    let shutdown = match shutdown {
        Some(shutdown) => shutdown,
        None => return std::future::pending().await,
    };
    loop {
        let notified = shutdown.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if let Some(script) = script {
            script.pause(LifecycleCheckpoint::BeforeRuntimeWait).await;
        }
        if shutdown.requested.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

async fn accept_next(
    listener: Option<&tokio::net::TcpListener>,
    script: Option<&Arc<LifecycleScript>>,
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
        SupervisorJoin::Camber(future) => Pin::new(future).poll(context).map(flatten_camber_join),
        SupervisorJoin::Tokio(handle) => Pin::new(handle).poll(context).map(flatten_tokio_join),
        SupervisorJoin::Ready(future) => Pin::new(future).poll(context),
    }
}

fn flatten_camber_join(
    result: Result<Result<(), RuntimeError>, RuntimeError>,
) -> Result<(), RuntimeError> {
    match result {
        Ok(server_result) => server_result,
        Err(error) => Err(error),
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
