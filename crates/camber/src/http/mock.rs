use super::Method;
use super::Response;
use super::response::HeaderPair;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::RuntimeError;

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleCheckpoint {
    BeforeSupervisorSelect,
    SupervisorSelectedDeadline,
    SupervisorSelectedControl,
    SupervisorSelectedRuntime,
    SupervisorSelectedAccept,
    SupervisorSelectedPermit,
    SupervisorSelectedRegistration,
    SupervisorSelectedTask,
    AfterAccept,
    AfterPermit,
    AfterSupervisorResultSend,
    AfterUpgradeTicketSubmitted,
    UpgradePeerClosed,
    ConnectionPermitWaitPending,
    BeforeRuntimeWait,
    BeforeUpgradeAcknowledge,
    KeepaliveTimeoutConfigured(std::time::Duration),
    RequestBodyLimitConfigured(usize),
    SseBufferConfigured(usize),
    WebSocketOutgoingBufferConfigured(usize),
    WebSocketIncomingBufferConfigured(usize),
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LifecycleFault {
    Accept(std::io::ErrorKind),
    PanicNextOwnedTask,
    PanicNextOwnedTaskOpaque,
    CancelNextOwnedTask,
    PanicSupervisorCore,
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SupervisorJoinProbe {
    CamberCancelled,
    CamberStringPanic,
    CamberOpaquePanic,
    CamberChannelClosed,
    TokioSuccess,
    TokioCancelled,
    TokioStringPanic,
    TokioOpaquePanic,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CheckpointPhase {
    Armed,
    Paused,
    Released,
}

struct CheckpointState {
    checkpoint: LifecycleCheckpoint,
    phase: CheckpointPhase,
    reached: Arc<tokio::sync::Notify>,
    released: Arc<tokio::sync::Notify>,
}

struct ScriptState {
    closed: bool,
    checkpoints: Vec<CheckpointState>,
    fault: Option<LifecycleFault>,
}

pub(crate) struct LifecycleScript {
    state: Mutex<ScriptState>,
    supervisor_wake: tokio::sync::Notify,
}

impl LifecycleScript {
    fn new() -> Self {
        Self {
            state: Mutex::new(ScriptState {
                closed: false,
                checkpoints: Vec::new(),
                fault: None,
            }),
            supervisor_wake: tokio::sync::Notify::new(),
        }
    }

    fn invalid(message: &'static str) -> RuntimeError {
        RuntimeError::InvalidArgument(message.into())
    }

    fn arm(&self, checkpoint: LifecycleCheckpoint) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let result = match (
            state.closed,
            state.checkpoints.iter().any(|entry| {
                entry.checkpoint == checkpoint && entry.phase != CheckpointPhase::Released
            }),
        ) {
            (true, _) => Err(Self::invalid("lifecycle controller is closed")),
            (false, true) => Err(Self::invalid("lifecycle checkpoint is already armed")),
            (false, false) => {
                state
                    .checkpoints
                    .retain(|entry| entry.checkpoint != checkpoint);
                state.checkpoints.push(CheckpointState {
                    checkpoint,
                    phase: CheckpointPhase::Armed,
                    reached: Arc::new(tokio::sync::Notify::new()),
                    released: Arc::new(tokio::sync::Notify::new()),
                });
                Ok(())
            }
        };
        drop(state);
        if result.is_ok() && checkpoint == LifecycleCheckpoint::BeforeSupervisorSelect {
            self.supervisor_wake.notify_one();
        }
        result
    }

    async fn wait_until_paused(&self, checkpoint: LifecycleCheckpoint) -> Result<(), RuntimeError> {
        loop {
            let reached = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                let entry = state
                    .checkpoints
                    .iter()
                    .find(|entry| entry.checkpoint == checkpoint)
                    .ok_or_else(|| Self::invalid("lifecycle checkpoint is not armed"))?;
                match (state.closed, entry.phase) {
                    (true, _) => return Err(Self::invalid("lifecycle controller is closed")),
                    (false, CheckpointPhase::Paused) => return Ok(()),
                    (false, CheckpointPhase::Released) => {
                        return Err(Self::invalid("lifecycle checkpoint was already released"));
                    }
                    (false, CheckpointPhase::Armed) => Arc::clone(&entry.reached),
                }
            };
            let notified = reached.notified();
            tokio::pin!(notified);
            let already_paused = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.closed
                    || state.checkpoints.iter().any(|entry| {
                        entry.checkpoint == checkpoint && entry.phase == CheckpointPhase::Paused
                    })
            };
            match already_paused {
                true => continue,
                false => notified.await,
            }
        }
    }

    fn release_checkpoint(&self, checkpoint: LifecycleCheckpoint) -> Result<(), RuntimeError> {
        let released = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            let entry = state
                .checkpoints
                .iter_mut()
                .find(|entry| entry.checkpoint == checkpoint)
                .ok_or_else(|| Self::invalid("lifecycle checkpoint is not armed"))?;
            match entry.phase {
                CheckpointPhase::Paused => {
                    entry.phase = CheckpointPhase::Released;
                    Arc::clone(&entry.released)
                }
                CheckpointPhase::Armed | CheckpointPhase::Released => {
                    return Err(Self::invalid("lifecycle checkpoint is not paused"));
                }
            }
        };
        released.notify_waiters();
        Ok(())
    }

    fn inject(&self, fault: LifecycleFault) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let result = match (state.closed, state.fault.is_some()) {
            (true, _) => Err(Self::invalid("lifecycle controller is closed")),
            (false, true) => Err(Self::invalid("lifecycle fault is already armed")),
            (false, false) => {
                state.fault = Some(fault);
                Ok(())
            }
        };
        drop(state);
        if result.is_ok()
            && matches!(
                fault,
                LifecycleFault::Accept(_) | LifecycleFault::PanicSupervisorCore
            )
        {
            self.supervisor_wake.notify_one();
        }
        result
    }

    pub(crate) async fn pause(&self, checkpoint: LifecycleCheckpoint) {
        let released = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if state.closed {
                return;
            }
            let entry = match state.checkpoints.iter_mut().find(|entry| {
                entry.checkpoint == checkpoint && entry.phase == CheckpointPhase::Armed
            }) {
                Some(entry) => entry,
                None => return,
            };
            entry.phase = CheckpointPhase::Paused;
            entry.reached.notify_waiters();
            Arc::clone(&entry.released)
        };
        loop {
            let notified = released.notified();
            tokio::pin!(notified);
            let done = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                let generation = state
                    .checkpoints
                    .iter()
                    .find(|entry| Arc::ptr_eq(&entry.released, &released));
                state.closed
                    || match generation {
                        Some(entry) => entry.phase == CheckpointPhase::Released,
                        None => true,
                    }
            };
            match done {
                true => return,
                false => notified.await,
            }
        }
    }

    pub(crate) async fn wait_for_supervisor_wake(&self) {
        self.supervisor_wake.notified().await;
    }

    pub(crate) fn take_accept_fault(&self) -> Option<std::io::ErrorKind> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match state.fault {
            Some(LifecycleFault::Accept(kind)) => {
                state.fault = None;
                Some(kind)
            }
            _ => None,
        }
    }

    pub(crate) fn take_owned_task_fault(&self) -> Option<LifecycleFault> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match state.fault {
            Some(
                fault @ (LifecycleFault::PanicNextOwnedTask
                | LifecycleFault::PanicNextOwnedTaskOpaque
                | LifecycleFault::CancelNextOwnedTask),
            ) => {
                state.fault = None;
                Some(fault)
            }
            _ => None,
        }
    }

    pub(crate) fn take_supervisor_fault(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match state.fault {
            Some(LifecycleFault::PanicSupervisorCore) => {
                state.fault = None;
                true
            }
            _ => false,
        }
    }

    fn close(&self) {
        let notifications = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.closed = true;
            state
                .checkpoints
                .iter()
                .flat_map(|entry| [Arc::clone(&entry.reached), Arc::clone(&entry.released)])
                .collect::<Vec<_>>()
        };
        notifications
            .into_iter()
            .for_each(|notification| notification.notify_waiters());
    }
}

struct LifecycleRegistration {
    addr: std::net::SocketAddr,
    script: Weak<LifecycleScript>,
}

fn lifecycle_registry() -> &'static Mutex<Vec<LifecycleRegistration>> {
    static REGISTRY: OnceLock<Mutex<Vec<LifecycleRegistration>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

#[doc(hidden)]
pub struct LifecycleController {
    addr: std::net::SocketAddr,
    script: Arc<LifecycleScript>,
}

impl LifecycleController {
    pub fn pause_once(&self, checkpoint: LifecycleCheckpoint) -> Result<(), RuntimeError> {
        self.script.arm(checkpoint)
    }

    pub async fn wait_until_paused(
        &self,
        checkpoint: LifecycleCheckpoint,
    ) -> Result<(), RuntimeError> {
        self.script.wait_until_paused(checkpoint).await
    }

    pub fn release(&self, checkpoint: LifecycleCheckpoint) -> Result<(), RuntimeError> {
        self.script.release_checkpoint(checkpoint)
    }

    pub fn inject_once(&self, fault: LifecycleFault) -> Result<(), RuntimeError> {
        self.script.inject(fault)
    }
}

impl Drop for LifecycleController {
    fn drop(&mut self) {
        self.script.close();
        let mut registry = lifecycle_registry()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        registry.retain(|entry| {
            entry.addr != self.addr
                || entry
                    .script
                    .upgrade()
                    .is_some_and(|script| !Arc::ptr_eq(&script, &self.script))
        });
    }
}

#[doc(hidden)]
pub fn lifecycle(addr: std::net::SocketAddr) -> Result<LifecycleController, RuntimeError> {
    let mut registry = lifecycle_registry()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    registry.retain(|entry| entry.script.strong_count() > 0);
    match registry.iter().any(|entry| entry.addr == addr) {
        true => Err(RuntimeError::InvalidArgument(
            "lifecycle controller already exists for address".into(),
        )),
        false => {
            let script = Arc::new(LifecycleScript::new());
            registry.push(LifecycleRegistration {
                addr,
                script: Arc::downgrade(&script),
            });
            Ok(LifecycleController { addr, script })
        }
    }
}

pub(crate) fn lifecycle_script(addr: std::net::SocketAddr) -> Option<Arc<LifecycleScript>> {
    let mut registry = lifecycle_registry()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    registry.retain(|entry| entry.script.strong_count() > 0);
    registry
        .iter()
        .find(|entry| entry.addr == addr)
        .and_then(|entry| entry.script.upgrade())
}

#[doc(hidden)]
pub fn supervisor_join_probe(probe: SupervisorJoinProbe) -> super::server::ServerHandleFuture {
    super::server_lifecycle::supervisor_join_probe(probe)
}

/// Global registry of mock HTTP responses.
///
/// When a mock is registered, `http::get`/`http::post` check this registry
/// before making a real network call. Mocks are keyed by (method, URL).
/// Uses a Vec for linear scan — the registry is test-only with few entries.
static MOCK_ACTIVE: AtomicBool = AtomicBool::new(false);
static MOCK_REGISTRY: Mutex<Option<Vec<MockEntry>>> = Mutex::new(None);

struct MockEntry {
    method: Option<Method>,
    url: Box<str>,
    status: u16,
    body: bytes::Bytes,
    headers: Arc<[HeaderPair]>,
    call_count: Arc<AtomicUsize>,
}

fn with_registry<F, R>(f: F) -> R
where
    F: FnOnce(&mut Vec<MockEntry>) -> R,
{
    let mut guard = MOCK_REGISTRY.lock().unwrap_or_else(|e| e.into_inner());
    let entries = guard.get_or_insert_with(Vec::new);
    f(entries)
}

/// Check the mock registry for a matching (method, URL) pair.
/// Returns Some(Response) if a mock is registered, None otherwise.
///
/// Matching priority: exact method match first, then method-agnostic (None).
pub(crate) fn try_intercept(method: Method, url: &str) -> Option<Response> {
    if !MOCK_ACTIVE.load(Ordering::Acquire) {
        return None;
    }
    with_registry(|entries| {
        let entry = find_mock_entry(entries, method, url)?;
        entry.call_count.fetch_add(1, Ordering::Release);
        let headers: Vec<HeaderPair> = entry
            .headers
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect();
        Some(Response::new(entry.status, entry.body.clone(), headers))
    })
}

fn find_mock_entry<'a>(
    entries: &'a [MockEntry],
    method: Method,
    url: &str,
) -> Option<&'a MockEntry> {
    entries
        .iter()
        .find(|e| e.url.as_ref() == url && e.method == Some(method))
        .or_else(|| {
            entries
                .iter()
                .find(|e| e.url.as_ref() == url && e.method.is_none())
        })
}

/// Register a method-agnostic mock for an outbound HTTP URL.
///
/// Matches any HTTP method. Use `http_method` for method-specific mocks.
/// Returns a `MockHttpBuilder` to configure the canned response.
pub fn http(url: &str) -> MockHttpBuilder {
    MockHttpBuilder {
        method: None,
        url: url.into(),
        response: None,
    }
}

/// Register a method-specific mock for an outbound HTTP URL.
///
/// Only matches requests with the given HTTP method.
/// Returns a `MockHttpBuilder` to configure the canned response.
pub fn http_method(method: Method, url: &str) -> MockHttpBuilder {
    MockHttpBuilder {
        method: Some(method),
        url: url.into(),
        response: None,
    }
}

/// Builder for configuring a mock HTTP response.
pub struct MockHttpBuilder {
    method: Option<Method>,
    url: Box<str>,
    response: Option<Response>,
}

impl MockHttpBuilder {
    /// Set the canned response to return when the URL is requested.
    pub fn returns(mut self, response: Response) -> MockHttp {
        self.response = Some(response);
        self.install()
    }

    fn install(self) -> MockHttp {
        let resp = match self.response {
            Some(r) => r,
            None => Response::empty_raw(200),
        };
        let call_count = Arc::new(AtomicUsize::new(0));
        let method = self.method;
        let url = self.url.clone();
        let entry = MockEntry {
            method,
            url: self.url,
            status: resp.status(),
            body: bytes::Bytes::copy_from_slice(resp.body_bytes()),
            headers: resp.headers().to_vec().into(),
            call_count: Arc::clone(&call_count),
        };
        with_registry(|entries| {
            entries.push(entry);
            MOCK_ACTIVE.store(true, Ordering::Release);
        });
        MockHttp {
            method,
            url,
            call_count,
        }
    }
}

/// Handle to a registered mock. Use to assert call counts.
///
/// The mock is automatically deregistered when this handle is dropped.
pub struct MockHttp {
    method: Option<Method>,
    url: Box<str>,
    call_count: Arc<AtomicUsize>,
}

impl MockHttp {
    /// Panics if the mock was not called exactly once.
    pub fn assert_called_once(&self) {
        let count = self.call_count.load(Ordering::Acquire);
        assert!(
            count == 1,
            "expected mock for {} {} to be called once, was called {count} times",
            match self.method {
                Some(m) => m.as_str(),
                None => "*",
            },
            self.url
        );
    }
}

impl Drop for MockHttp {
    fn drop(&mut self) {
        let method = self.method;
        let url = &self.url;
        with_registry(|entries| {
            entries.retain(|e| !(e.url == *url && e.method == method));
            if entries.is_empty() {
                MOCK_ACTIVE.store(false, Ordering::Release);
            }
        });
    }
}
