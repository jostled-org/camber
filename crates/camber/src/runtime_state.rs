use crate::resource::registry::ResourceRegistry;
// The one forced-stop window every owner shares. The root scope, the external
// cancellation watcher, and the server supervisor all give an aborted owner the
// same grace rather than three that can drift apart.
use crate::lifecycle::{
    FORCED_JOIN_GRACE, LifecycleFailureKind, LifecycleParticipant, LifecyclePhase,
};
use crate::runtime_test_support::{ParticipantDisposition, RuntimeCheckpoint, RuntimeSchedule};
use crate::tls::CertStore;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

pub(crate) const DEFAULT_HEALTH_INTERVAL: Duration = Duration::from_secs(10);

pub(crate) type TlsConfig = Arc<rustls::ServerConfig>;

/// Take a lock whose previous holder panicked, instead of refusing it.
///
/// Poisoning says a holder unwound; it does not say the guarded value is
/// unusable. Everything locked here is admission bookkeeping, a panic slot, or
/// a checkpoint another thread is parked on, and the panic that poisoned the
/// lock is already being reported through its own path. Propagating the poison
/// would add a second panic raised from teardown, or leave a waiter with
/// nobody left to wake it — either way replacing the original fault with one
/// caused by reporting it. Generic over `LockResult` so `Mutex::lock`,
/// `Condvar::wait` and `Condvar::wait_timeout` all recover through this one
/// definition.
pub(crate) fn recover_poisoned<T>(result: std::sync::LockResult<T>) -> T {
    result.unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn default_worker_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get() * 4)
        .unwrap_or(16)
}

/// Runtime configuration. Stored in RuntimeInner, read by server components.
#[derive(Clone)]
pub(crate) struct RuntimeConfig {
    pub(crate) worker_threads: usize,
    /// The complete outer service envelope, stored once.
    ///
    /// The runtime's header, request, transfer, shutdown, profiling, and
    /// connection bounds are one validated value rather than a field each, so a
    /// server started inside this runtime narrows a single policy instead of
    /// reassembling it from parts that can disagree.
    pub(crate) server_policy: crate::http::ServerPolicy,
    pub(crate) tracing_enabled: bool,
    pub(crate) metrics_enabled: bool,
    #[cfg(feature = "profiling")]
    pub(crate) profiling_enabled: bool,
    pub(crate) health_interval: Duration,
    /// The deadline each registered resource's lifecycle callbacks run under.
    ///
    /// Held beside the health interval rather than inside it: the interval says
    /// how often a probe starts, and this says how long any one callback may
    /// take, which is the bound the runtime's aggregate shutdown then narrows.
    pub(crate) resource_budget: crate::ResourceBudget,
    pub(crate) tls_config: Option<TlsConfig>,
    pub(crate) cert_store: Option<CertStore>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            worker_threads: default_worker_threads(),
            server_policy: crate::http::ServerPolicy::default(),
            tracing_enabled: false,
            metrics_enabled: false,
            #[cfg(feature = "profiling")]
            profiling_enabled: false,
            health_interval: DEFAULT_HEALTH_INTERVAL,
            resource_budget: crate::ResourceBudget::default(),
            tls_config: None,
            cert_store: None,
        }
    }
}

/// Shared runtime state. Async tasks use Tokio task-local storage; synchronous
/// entry points use thread-local storage.
pub(crate) struct RuntimeInner {
    shutdown: LatchSignal,
    /// The one deadline every framework-owned shutdown participant shares.
    ///
    /// Held by the runtime rather than by each owner, because that is the
    /// difference the whole type makes: a server, a connection, a resource, and
    /// the executor all narrow one expiry instead of each starting its own copy
    /// of the same grace.
    shutdown_deadline: Arc<crate::lifecycle::AggregateShutdown>,
    scope: TaskScope,
    test_schedule: Option<Arc<RuntimeSchedule>>,
    cancel_task: Mutex<CancelWatcherState>,
    pub(crate) config: RuntimeConfig,
    pub(crate) metrics_handle: Option<metrics_exporter_prometheus::PrometheusHandle>,
    pub(crate) tokio_handle: Option<tokio::runtime::Handle>,
    pub(crate) resources: Option<ResourceRegistry>,
}

struct CancelWatcherState {
    current: Option<CancelWatcher>,
}

struct CancelWatcher {
    identity: Arc<tokio::sync::Notify>,
    handle: tokio::task::JoinHandle<()>,
}

/// Sticky one-shot signal: a latching flag plus waiter notification.
///
/// Backs both runtime shutdown and root-scope closing. A waiter that arrives
/// after the latch fires still observes it.
#[derive(Clone)]
pub(crate) struct LatchSignal {
    fired: Arc<AtomicBool>,
    notify: Arc<tokio::sync::Notify>,
}

/// Runtime shutdown is one latching signal; scope closing is the other.
pub(crate) type ShutdownSignal = LatchSignal;

impl LatchSignal {
    fn new() -> Self {
        Self {
            fired: Arc::new(AtomicBool::new(false)),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub(crate) fn is_fired(&self) -> bool {
        self.fired.load(Ordering::Acquire)
    }

    /// Latch the signal and wake every waiter. Firing twice is a no-op.
    pub(crate) fn fire(&self) {
        self.fired.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }

    /// Rebuild the latch from the two halves a caller already holds.
    ///
    /// The public `signals::spawn_signal_watcher` contract takes the flag and
    /// the notifier separately, so that wrapper would otherwise have to
    /// open-code `fire`. Reassembling them there keeps one definition of what
    /// firing a latch means, and it is the only boundary that ever holds the
    /// halves apart.
    pub(crate) fn from_parts(fired: Arc<AtomicBool>, notify: Arc<tokio::sync::Notify>) -> Self {
        Self { fired, notify }
    }

    pub(crate) async fn wait(&self) {
        self.wait_observed(|| std::future::ready(())).await;
    }

    /// Wait for the latch, running `observe` after each registration and
    /// before the sticky state is read. Registration precedes the read, so
    /// no fire can land in a check/register gap.
    pub(crate) async fn wait_observed<F, Fut>(&self, observe: F)
    where
        F: Fn() -> Fut,
        Fut: Future<Output = ()>,
    {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            observe().await;
            match self.is_fired() {
                true => return,
                false => notified.await,
            }
        }
    }
}

/// The two lifecycle signals a Camber-owned background loop must exit on.
///
/// Shutdown implies scope closing — `request_shutdown` fires both on one call,
/// whatever asked for it — but closing does not imply shutdown: the user
/// closure's return closes admission with the shutdown latch left unset. A
/// perpetual child observes the pair so that either trigger ends it, without
/// depending on which one the runtime saw first.
#[derive(Clone)]
pub(crate) struct LifecycleSignals {
    shutdown: LatchSignal,
    closing: LatchSignal,
}

impl LifecycleSignals {
    /// The signals of the runtime established for this task or thread, or a
    /// pair of inert latches when none is.
    ///
    /// The runtime is resolved once and both halves come from that one lookup,
    /// so the pair a child stops on always names a single runtime — the same
    /// rule `task::admit_signalled_loop` states for the scope that owns it.
    pub(crate) fn current() -> Self {
        match try_current_runtime() {
            Some(inner) => Self::from_runtime(&inner),
            // Nothing can request shutdown or close a scope without a runtime,
            // so a pair that never fires is the honest answer.
            None => Self {
                shutdown: LatchSignal::new(),
                closing: LatchSignal::new(),
            },
        }
    }

    /// The signals of one named runtime, for a caller that already resolved
    /// its context.
    pub(crate) fn from_runtime(inner: &RuntimeInner) -> Self {
        Self {
            shutdown: inner.shutdown_signal(),
            closing: inner.scope_closing(),
        }
    }

    /// True once either signal has fired.
    pub(crate) fn is_fired(&self) -> bool {
        self.shutdown.is_fired() || self.closing.is_fired()
    }

    /// Resolve as soon as either signal fires.
    pub(crate) async fn wait(&self) {
        tokio::select! {
            () = self.shutdown.wait() => {}
            () = self.closing.wait() => {}
        }
    }

    /// Sleep one `interval`, or break as soon as either signal fires.
    ///
    /// `select!` is unbiased, so a sleep completing in the same poll as a
    /// signal can win the race and let the loop body run once more after its
    /// owner asked it to stop. The re-check makes the decision independent of
    /// that ordering. Every perpetual Camber-owned loop that wakes on a fixed
    /// interval breaks through here, so the rule is written once.
    pub(crate) async fn tick(&self, interval: Duration) -> ControlFlow<()> {
        tokio::select! {
            () = tokio::time::sleep(interval) => {}
            () = self.wait() => return ControlFlow::Break(()),
        }
        match self.is_fired() {
            true => ControlFlow::Break(()),
            false => ControlFlow::Continue(()),
        }
    }

    /// Run `work` to completion, or break as soon as either signal fires.
    ///
    /// A scope child's loop body is the part that outlives its own interval: a
    /// health probe or an ACME order carries its own multi-second timeout, and
    /// awaiting it unguarded would spend the drain's escalation budget before
    /// the loop ever reached its next `tick`. Racing it means the child stops
    /// at an await point rather than at the forced-abort boundary.
    ///
    /// Deliberately no post-select re-check, unlike [`Self::tick`] and
    /// `schedule::next_wake`: a won `work` arm holds a completed `Fut::Output`,
    /// and re-checking would throw away a result the child already produced.
    /// A tick carries nothing, so there is nothing to lose by re-deciding.
    pub(crate) async fn guard<Fut>(&self, work: Fut) -> ControlFlow<(), Fut::Output>
    where
        Fut: Future,
    {
        tokio::select! {
            output = work => ControlFlow::Continue(output),
            () = self.wait() => ControlFlow::Break(()),
        }
    }
}

impl RuntimeInner {
    pub(crate) fn with_config_and_schedule(
        config: RuntimeConfig,
        test_schedule: Option<Arc<RuntimeSchedule>>,
    ) -> Self {
        let shutdown_deadline = crate::lifecycle::AggregateShutdown::new(
            config.server_policy.shutdown_timeout_value(),
            test_schedule.clone(),
        );
        Self {
            shutdown: LatchSignal::new(),
            shutdown_deadline,
            scope: TaskScope::new(),
            test_schedule,
            cancel_task: Mutex::new(CancelWatcherState { current: None }),
            config,
            metrics_handle: None,
            tokio_handle: None,
            resources: None,
        }
    }

    /// The one aggregate shutdown this runtime and its owned servers share.
    pub(crate) fn shutdown_deadline(&self) -> Arc<crate::lifecycle::AggregateShutdown> {
        Arc::clone(&self.shutdown_deadline)
    }

    /// Request runtime shutdown. Shutdown implies scope closing, so admission
    /// closes on the same call.
    ///
    /// Closing runs FIRST. Another thread that reads `is_shutting_down()` as
    /// true must not then be admitted, and firing the shutdown latch first
    /// leaves exactly that window open. Nothing loses a signal by the reorder:
    /// `LifecycleSignals::is_fired` ORs the two, so a child watching for
    /// shutdown stops on whichever of them it observes first.
    /// The transition is also where the one aggregate deadline is minted. A
    /// second request reads the first one's expiry back rather than extending
    /// it.
    pub(crate) fn request_shutdown(&self) {
        self.close_scope();
        self.shutdown_deadline.mint_at(tokio::time::Instant::now());
        self.shutdown.fire();
    }

    pub(crate) fn shutdown_signal(&self) -> ShutdownSignal {
        self.shutdown.clone()
    }

    pub(crate) fn is_shutdown_requested(&self) -> bool {
        self.shutdown.is_fired()
    }

    /// Close root-scope admission and fire `ScopeClosing`. Idempotent — the
    /// first of the two triggers (closure return, shutdown request) wins.
    pub(crate) fn close_scope(&self) {
        self.pause_test_schedule(RuntimeCheckpoint::ScopeCloseTransition);
        self.scope.close();
    }

    pub(crate) fn scope_closing(&self) -> LatchSignal {
        self.scope.closing()
    }

    /// This runtime's OWN executor, or the typed absence a caller propagates.
    ///
    /// Every site that launches a child on behalf of a runtime resolves it
    /// here. The ambient pool is never asked for: a free `tokio::spawn` or
    /// `spawn_blocking` panics when no Tokio runtime is entered and silently
    /// attaches the child to a foreign one when a different runtime is —
    /// either way the scope awaiting the child would not be the scope it runs
    /// under. A runtime with no executor has nowhere to run one, and
    /// `NoRuntime` is what says so.
    ///
    /// `watch_cancel` is the deliberate exception: it is reached from a `pub`
    /// no-op API with no result to carry a refusal, so it warns instead.
    pub(crate) fn executor(&self) -> Result<&tokio::runtime::Handle, crate::RuntimeError> {
        self.tokio_handle
            .as_ref()
            .ok_or(crate::RuntimeError::NoRuntime)
    }

    /// Admit one non-preemptible blocking child. The scope tallies it at
    /// admission — there is no handle it could ever abort — and the returned
    /// slot releases the claim when dropped.
    pub(crate) fn admit_blocking(self: &Arc<Self>) -> Result<ScopeSlot, crate::RuntimeError> {
        self.admit(ChildKind::Blocking)
    }

    /// Admit one Camber-owned async child, whose panic no user handle can
    /// deliver.
    ///
    /// The scope records the first such panic so `run` can report it; a
    /// user-owned child keeps delivering its panic through its own handle.
    pub(crate) fn admit_internal_async<F>(
        self: &Arc<Self>,
        body: F,
    ) -> Result<(), crate::RuntimeError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let sink = Arc::clone(self);
        self.admit_async(capture_child_panic(body, sink))
    }

    /// Admit one async child and spawn it behind a start gate, so the scope
    /// registers the joinable handle before the child's body runs. The gate
    /// closes the race between `tokio::spawn` returning a handle and the
    /// child completing.
    pub(crate) fn admit_async<F>(self: &Arc<Self>, body: F) -> Result<(), crate::RuntimeError>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        // Resolved before anything is counted, so a runtime with nowhere to run
        // the child refuses it instead of admitting one it cannot launch.
        let executor = self.executor()?;
        let slot = self.admit(ChildKind::Async)?;
        let id = slot.id;
        let gate = Arc::new(tokio::sync::Notify::new());
        // The task-local scope is built here rather than through the
        // `scope_runtime` async fn: that wrapper would construct the same
        // future again on its first poll, and child futures can be large.
        let handle = executor.spawn(
            TASK_RUNTIME.scope(Arc::clone(self), gated_child(Arc::clone(&gate), body, slot)),
        );
        // Aborted outside the guard: `abort` can drop a never-polled task
        // inline, and the dropped child's `ScopeSlot` retakes the same lock.
        //
        // The abort lands before the gate opens, so the body never runs: the
        // admission is refused, not merely stopped. Reporting success here
        // would hand the caller a handle for a child that will never produce a
        // result. The slot rides inside the aborted future, so dropping it
        // releases the claim this call took — and when the child was already
        // dropped unpolled, that release has happened and the seeded entry is
        // gone, which is the other way registration comes back refused.
        if let Some(swept) = self.scope.register_async(id, handle) {
            swept.abort();
            return Err(crate::RuntimeError::ScopeClosed);
        }
        self.pause_test_schedule(RuntimeCheckpoint::AdmissionRegistered);
        gate.notify_one();
        Ok(())
    }

    /// Admit one child to the root scope, or refuse it once admission closed.
    fn admit(self: &Arc<Self>, kind: ChildKind) -> Result<ScopeSlot, crate::RuntimeError> {
        match self.scope.admit(kind) {
            None => Err(crate::RuntimeError::ScopeClosed),
            Some(id) => {
                self.pause_test_schedule(RuntimeCheckpoint::AdmissionCounted);
                Ok(ScopeSlot {
                    runtime: Arc::clone(self),
                    id,
                    kind,
                })
            }
        }
    }

    /// How many children the root scope retains an entry for.
    pub(crate) fn scope_registry_len(&self) -> usize {
        self.scope.registry_len()
    }

    /// How many children the scope owner has awaited to Tokio-handle
    /// completion.
    pub(crate) fn scope_joined_count(&self) -> usize {
        self.scope.joined_count()
    }

    /// Take the first panic an internally-owned child recorded, leaving the
    /// slot empty so no later reader sees it twice.
    pub(crate) fn take_internal_panic(&self) -> Option<crate::RuntimeError> {
        self.scope.take_internal_panic()
    }

    /// Report the child count the drain finished on, so a leak probe has one
    /// definite observation instead of a sampled race.
    fn observe_drain_end(&self) {
        let outstanding = self.scope.count();
        self.pause_test_schedule(RuntimeCheckpoint::ScopeWaitObserved(outstanding));
    }

    /// Publish this runtime to its attached test schedule, so the seam's
    /// read-only scope probes need no ambient runtime context.
    pub(crate) fn publish_to_test_schedule(self: &Arc<Self>) {
        if let Some(schedule) = self.test_schedule.as_ref() {
            schedule.attach_runtime(self);
        }
    }

    pub(crate) fn pause_test_schedule(&self, checkpoint: RuntimeCheckpoint) {
        if let Some(schedule) = self.test_schedule.as_ref() {
            schedule.pause(checkpoint);
        }
    }

    /// Whether this runtime has been told to admit no worker for `resource`'s
    /// next lifecycle callback.
    ///
    /// The one scheduling decision a resource coordinator asks about. A worker
    /// the operating system refuses and a worker the seam refuses are the same
    /// absence, and production — not the seam — is what names the resource that
    /// lost one as having lost its worker.
    pub(crate) fn resource_worker_refused(&self, resource: &str) -> bool {
        self.test_schedule
            .as_ref()
            .is_some_and(|schedule| schedule.refuses_resource_worker(resource))
    }

    /// Watch an external cancellation future, requesting shutdown once it
    /// completes. A later registration displaces this one.
    ///
    /// The watcher runs on the runtime's own executor: a free `tokio::spawn`
    /// would panic with no ambient Tokio runtime and would attach the watcher
    /// to a foreign one when a different runtime is ambient — leaving this
    /// runtime with a shutdown request no task of its own could deliver. With
    /// no executor there is nothing to run the watcher on, so the registration
    /// is refused and said so, rather than panicking inside a `pub` no-op API.
    fn watch_cancel<F>(self: &Arc<Self>, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        match self.tokio_handle.as_ref() {
            None => tracing::warn!(
                "external cancellation watcher not registered: the runtime has no executor"
            ),
            Some(executor) => {
                let gate = Arc::new(tokio::sync::Notify::new());
                let task_inner = Arc::clone(self);
                let mut state = recover_poisoned(self.cancel_task.lock());
                let task_gate = Arc::clone(&gate);
                let handle = executor.spawn(async move {
                    task_gate.notified().await;
                    let outcome = crate::task::catch_panic_async(future).await;
                    task_inner.finish_cancel_watcher(&task_gate, outcome);
                });
                let displaced = state.current.replace(CancelWatcher {
                    identity: Arc::clone(&gate),
                    handle,
                });
                drop(state);
                abort_cancel_watcher(displaced);
                gate.notify_one();
            }
        }
    }

    fn finish_cancel_watcher(
        &self,
        identity: &Arc<tokio::sync::Notify>,
        outcome: Result<(), crate::RuntimeError>,
    ) {
        match outcome {
            Ok(()) => self.request_shutdown_if_current(identity),
            Err(error) => {
                drop(self.take_cancel_watcher(identity));
                self.scope.record_internal_panic(error);
            }
        }
    }

    /// Apply a completed watcher while its identity is still current.
    ///
    /// The state guard remains held through the shutdown request. A replacement
    /// therefore linearizes either before this check and makes the completion
    /// stale, or after shutdown is already applied; it cannot return between
    /// the check and the request.
    fn request_shutdown_if_current(&self, identity: &Arc<tokio::sync::Notify>) {
        let mut state = recover_poisoned(self.cancel_task.lock());
        match state.current.as_ref() {
            Some(current) if Arc::ptr_eq(&current.identity, identity) => {
                let completed = state.current.take();
                self.request_shutdown();
                drop(state);
                drop(completed);
            }
            Some(_) | None => {}
        }
    }

    fn take_cancel_watcher(&self, identity: &Arc<tokio::sync::Notify>) -> Option<CancelWatcher> {
        let mut state = recover_poisoned(self.cancel_task.lock());
        match state.current.as_ref() {
            Some(current) if Arc::ptr_eq(&current.identity, identity) => state.current.take(),
            Some(_) | None => None,
        }
    }

    fn take_current_cancel_watcher(&self) -> Option<CancelWatcher> {
        recover_poisoned(self.cancel_task.lock()).current.take()
    }
}

fn abort_cancel_watcher(watcher: Option<CancelWatcher>) {
    if let Some(watcher) = watcher {
        watcher.handle.abort();
    }
}

/// Root-scope admission state. `Closing` and `Closed` both refuse admission;
/// they differ in what a zero count lets the drain conclude.
// `Copy` is what lets `on_close` take `self` by value under the count guard.
// No equality derive: every use is a match pattern, and an `==` this type does
// not offer cannot drift out of step with those patterns.
#[derive(Clone, Copy)]
enum Admission {
    Open,
    Closing,
    Closed,
}

impl Admission {
    /// The single `Open -> Closing`/`Closed` transition, taken under the count
    /// guard so it cannot interleave with an admit. A scope holding no children
    /// settles straight to `Closed`; an already-closed scope keeps its state,
    /// which is what makes the first of the two close triggers the winner.
    fn on_close(self, count: usize) -> Self {
        match (self, count) {
            (Self::Open, 0) => Self::Closed,
            (Self::Open, _) => Self::Closing,
            (already_closed, _) => already_closed,
        }
    }

    /// The state a drained scope settles into once its last child exits.
    fn on_drained(self) -> Self {
        match self {
            Self::Closing | Self::Closed => Self::Closed,
            Self::Open => Self::Open,
        }
    }
}

/// Identity for one admitted child, minted under the scope guard.
type TaskId = u64;

/// How the executor can stop an admitted child. An async child has a joinable
/// Tokio handle the scope retains; a blocking closure cannot be preempted, so
/// the scope keeps a tally entry instead of a handle it could never act on.
#[derive(Clone, Copy)]
enum ChildKind {
    Async,
    Blocking,
}

/// Admission state, child count, and the joinable handle registry under one
/// guard, so a close can never interleave with an admit.
struct ScopeState {
    admission: Admission,
    count: usize,
    next_id: TaskId,
    /// One entry per LIVE async child, seeded at admission and filled once
    /// `spawn` hands back the joinable handle. The entry is the child's
    /// existence, not its handle: keyed to admission and removed by the same
    /// `ScopeSlot::drop` every other exit runs through, so a child dropped
    /// before it could register leaves nothing behind.
    async_children: HashMap<TaskId, Option<tokio::task::JoinHandle<()>>>,
    blocking_children: HashSet<TaskId>,
    joined: usize,
    /// Set once the escalation has swept the registry, so a handle registered
    /// after that sweep is stopped by its registrar rather than retained by an
    /// owner that will never look again.
    stopped: bool,
}

impl ScopeState {
    /// Mint the child's identity, raise the count, and seed its registry
    /// entry.
    ///
    /// Both kinds get their entry HERE, at the one moment the child provably
    /// exists. An async child's entry starts empty and is filled when `spawn`
    /// returns its handle; seeding it later — at registration — would let a
    /// child dropped unpolled in that window run its removal first and have
    /// the registrar insert an entry afterwards that nothing would ever remove.
    fn count_child(&mut self, kind: ChildKind) -> TaskId {
        let id = self.next_id;
        self.next_id = id.wrapping_add(1);
        self.count += 1;
        match kind {
            ChildKind::Async => {
                self.async_children.insert(id, None);
            }
            ChildKind::Blocking => {
                self.blocking_children.insert(id);
            }
        }
        id
    }

    /// Fill a seeded entry with the child's joinable handle.
    ///
    /// Hands the handle back when there is no entry to fill: the child was
    /// dropped before it could register, or the escalation already swept the
    /// registry. Either way no owner will look at that handle again, so its
    /// registrar stops the child instead.
    fn fill_async_handle(
        &mut self,
        id: TaskId,
        handle: tokio::task::JoinHandle<()>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        match (self.stopped, self.async_children.get_mut(&id)) {
            (false, Some(entry)) => {
                *entry = Some(handle);
                None
            }
            _ => Some(handle),
        }
    }

    /// Drop the child's registry entry: a completed child leaves no handle.
    fn remove_child(&mut self, id: TaskId, kind: ChildKind) {
        match kind {
            ChildKind::Async => {
                self.async_children.remove(&id);
            }
            ChildKind::Blocking => {
                self.blocking_children.remove(&id);
            }
        }
    }

    /// How many children the owner retains a way to stop: one joinable handle
    /// per spawned async child, one tally entry per blocking child.
    ///
    /// A seeded async entry with no handle yet is NOT counted. The entry
    /// records that the child exists so its removal has something to remove;
    /// the handle is what the owner can act on, and that is what this number
    /// has always meant to the probes that read it.
    fn entries(&self) -> usize {
        let registered = self
            .async_children
            .values()
            .filter(|handle| handle.is_some())
            .count();
        registered + self.blocking_children.len()
    }
}

/// The runtime's root task scope: the completion owner of every admitted
/// child.
struct TaskScope {
    state: Mutex<ScopeState>,
    idle: Condvar,
    closing: LatchSignal,
    /// The first panic an internally-owned child raised. `Option` because no
    /// internal panic is the normal case, and first-write-wins because the
    /// runtime reports one fault, not a list.
    internal_panic: Mutex<Option<crate::RuntimeError>>,
}

impl TaskScope {
    fn new() -> Self {
        Self {
            state: Mutex::new(ScopeState {
                admission: Admission::Open,
                count: 0,
                next_id: 0,
                async_children: HashMap::new(),
                blocking_children: HashSet::new(),
                joined: 0,
                stopped: false,
            }),
            idle: Condvar::new(),
            closing: LatchSignal::new(),
            internal_panic: Mutex::new(None),
        }
    }

    /// Record an internally-owned child's panic, keeping the first one.
    ///
    /// The first panic is LOGGED as well as stored. The slot is only one of two
    /// ways it can leave the runtime, and the other one is not guaranteed: a
    /// closure that unwinds carries its own payload out past the `Result` this
    /// slot would have travelled in. Storing it silently made that combination
    /// — a Camber-owned child that panicked inside a failing `#[camber::test]`
    /// — leave no trace anywhere. A later panic is `warn`: the fault the
    /// runtime reports has already been named.
    fn record_internal_panic(&self, error: crate::RuntimeError) {
        let mut slot = self.panic_slot();
        match slot.as_ref() {
            Some(_) => tracing::warn!(%error, "further internally-owned child panic"),
            None => {
                tracing::error!(%error, "internally-owned child panicked");
                *slot = Some(error);
            }
        }
    }

    fn take_internal_panic(&self) -> Option<crate::RuntimeError> {
        self.panic_slot().take()
    }

    fn closing(&self) -> LatchSignal {
        self.closing.clone()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ScopeState> {
        recover_poisoned(self.state.lock())
    }

    /// The panic slot's guard.
    ///
    /// A panic is exactly what poisons this mutex, so the recovery
    /// `recover_poisoned` performs is load-bearing here: dropping the report
    /// because a previous reporter unwound would lose the one fault the slot
    /// exists to carry.
    fn panic_slot(&self) -> std::sync::MutexGuard<'_, Option<crate::RuntimeError>> {
        recover_poisoned(self.internal_panic.lock())
    }

    /// Count one child when admission is open. Returns its minted id, or
    /// `None` once admission has closed.
    fn admit(&self, kind: ChildKind) -> Option<TaskId> {
        let mut state = self.lock();
        match state.admission {
            Admission::Open => Some(state.count_child(kind)),
            Admission::Closing | Admission::Closed => None,
        }
    }

    /// Retain the joinable handle an async child was spawned with, so the
    /// scope can abort and join it later.
    ///
    /// Fills the entry admission already seeded — it never creates one, so a
    /// child that no longer exists cannot be registered at all. Returns the
    /// handle back when the entry is gone, which means the admission is
    /// reported REFUSED: the caller aborts the child before its gate opens, so
    /// its body never runs. That covers both ways the entry disappears — the
    /// child was dropped unpolled before registering, and the escalation swept
    /// the registry between the count and the spawn. Either would otherwise
    /// leave a handle no owner retains: counted, reported outstanding, and
    /// never stopped. The abort happens outside the guard, because it can drop
    /// a never-polled task inline and this lock is not reentrant.
    #[must_use = "an unregistered handle must be aborted by the caller"]
    fn register_async(
        &self,
        id: TaskId,
        handle: tokio::task::JoinHandle<()>,
    ) -> Option<tokio::task::JoinHandle<()>> {
        self.lock().fill_async_handle(id, handle)
    }

    fn registry_len(&self) -> usize {
        self.lock().entries()
    }

    fn joined_count(&self) -> usize {
        self.lock().joined
    }

    fn count(&self) -> usize {
        self.lock().count
    }

    /// Take every retained async handle out of the registry, so the owner can
    /// stop children it alone still holds a way to join.
    ///
    /// A seeded entry with no handle yet is drained with the rest and yields
    /// nothing to stop: its child is counted but not spawned, and `stopped`
    /// is what makes its registrar abort it instead.
    fn take_async_children(&self) -> Box<[tokio::task::JoinHandle<()>]> {
        let mut state = self.lock();
        state.stopped = true;
        state
            .async_children
            .drain()
            .filter_map(|(_, handle)| handle)
            .collect()
    }

    /// Acknowledge that the owner awaited one child's Tokio handle to
    /// completion.
    fn record_join(&self) {
        self.lock().joined += 1;
    }

    /// Abort every retained async child and join it under one bounded grace.
    ///
    /// Abort drops a yielding child at its next `.await`, so its handle
    /// resolves inside the grace and the join acknowledges it. A child the
    /// executor cannot stop never resolves, which is why the grace bounds the
    /// join instead of the join bounding itself.
    ///
    /// Each child settles as it is joined, so the record says how many owners
    /// the forced stop actually got back rather than how many it aborted. The
    /// ones still outstanding when the grace expires are named, because a child
    /// the executor cannot stop is exactly the participant an operator has to
    /// be told about.
    async fn force_stop(&self, shutdown: &crate::lifecycle::AggregateShutdown) {
        let handles = self.take_async_children();
        let aborted = handles.len();
        for handle in handles.iter() {
            handle.abort();
        }
        let mut joins: futures_util::stream::FuturesUnordered<_> =
            handles.into_vec().into_iter().collect();
        let mut settled = 0;
        let drain = async {
            while let Some(joined) = futures_util::StreamExt::next(&mut joins).await {
                report_forced_join(joined);
                self.record_join();
                settled += 1;
                shutdown.settle(
                    &LifecycleParticipant::BackgroundTask,
                    ParticipantDisposition::CancelledAndJoined,
                );
            }
        };
        if tokio::time::timeout(FORCED_JOIN_GRACE, drain)
            .await
            .is_err()
        {
            tracing::warn!("root scope drain grace expired with children still unstoppable");
        }
        for _ in settled..aborted {
            shutdown.settle(
                &LifecycleParticipant::BackgroundTask,
                ParticipantDisposition::Named,
            );
        }
    }

    fn finish(&self, id: TaskId, kind: ChildKind) {
        let mut state = self.lock();
        state.remove_child(id, kind);
        match state.count {
            0 => {
                tracing::error!("runtime task scope completed an unadmitted child");
                return;
            }
            1 => {
                state.count = 0;
                state.admission = state.admission.on_drained();
            }
            current => state.count = current - 1,
        }
        // Every exit wakes the drain, so its `ScopeWaitObserved` checkpoint
        // reports each count it passes through, not only the last one.
        self.idle.notify_all();
    }

    /// Perform the single atomic `Open -> Closing` transition and fire
    /// `ScopeClosing`. A scope with no children settles straight to `Closed`.
    fn close(&self) {
        let mut state = self.lock();
        state.admission = state.admission.on_close(state.count);
        drop(state);
        self.closing.fire();
    }

    /// Wait for children to exit cooperatively, bounded by `timeout`.
    ///
    /// Returns how many children the boundary found outstanding — zero once
    /// the scope has drained.
    ///
    /// The bound is tracked as an elapsed budget rather than an absolute
    /// instant: `shutdown_timeout` is clamped only from below, and every
    /// teardown runs this wait, so an absolute `Instant + Duration` would
    /// overflow and panic inside teardown on a caller-supplied duration near
    /// `Duration::MAX`.
    fn wait_timeout(&self, timeout: Duration, schedule: Option<&RuntimeSchedule>) -> usize {
        let started = std::time::Instant::now();
        let mut budget = timeout;
        let mut state = self.lock();
        while state.count > 0 {
            let checkpoint = RuntimeCheckpoint::ScopeWaitObserved(state.count);
            if let Some(schedule) = schedule.filter(|schedule| schedule.is_armed(checkpoint)) {
                drop(state);
                // A scheduling checkpoint alters ordering, never the bound: a
                // test holding this pause — 5.T5 serves a whole request inside
                // it — must not spend the drain's escalation budget, or the
                // seam would manufacture the timeout it exists to observe.
                let held = std::time::Instant::now();
                schedule.pause(checkpoint);
                budget = budget.saturating_add(held.elapsed());
                state = self.lock();
                continue;
            }
            let remaining = budget.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return state.count;
            }
            let (next_state, result) = recover_poisoned(self.idle.wait_timeout(state, remaining));
            state = next_state;
            if result.timed_out() {
                return state.count;
            }
        }
        0
    }
}

/// Report a child that unwound between the drain boundary and its abort.
///
/// A cancellation is the abort's own expected outcome and says nothing. A
/// panic is different: this handle is the scope's, the drain is discarding it,
/// and every ordinary panic route — the user handle, the internal panic slot —
/// has already been passed by. Unreported, the fault would vanish with the
/// handle.
fn report_forced_join(joined: Result<(), tokio::task::JoinError>) {
    match joined {
        Err(error) if error.is_panic() => {
            tracing::error!(%error, "root scope child panicked during the forced stop");
        }
        Ok(()) | Err(_) => {}
    }
}

/// One admitted child's claim on the root scope. Dropping it — on return,
/// cancellation, abort, or panic — releases that claim and removes exactly
/// this child's registry entry.
#[must_use = "dropping the slot releases the child's claim on the root scope"]
pub(crate) struct ScopeSlot {
    runtime: Arc<RuntimeInner>,
    id: TaskId,
    kind: ChildKind,
}

impl Drop for ScopeSlot {
    fn drop(&mut self) {
        self.runtime.scope.finish(self.id, self.kind);
    }
}

/// Hold an admitted child at its start gate until the scope has registered
/// its joinable handle, then run the body.
async fn gated_child<F>(gate: Arc<tokio::sync::Notify>, body: F, slot: ScopeSlot)
where
    F: Future<Output = ()>,
{
    gate.notified().await;
    body.await;
    drop(slot);
}

/// Run an internally-owned child, routing its panic to the scope.
///
/// No user holds a handle for this child, so an uncaught panic would be lost;
/// recording it is what lets `run` report the fault instead of the timeout it
/// often causes.
async fn capture_child_panic<F>(body: F, runtime: Arc<RuntimeInner>)
where
    F: Future<Output = ()>,
{
    if let Err(error) = crate::task::catch_panic_async(body).await {
        runtime.scope.record_internal_panic(error);
    }
}

tokio::task_local! {
    static TASK_RUNTIME: Arc<RuntimeInner>;
}

thread_local! {
    static RUNTIME: std::cell::RefCell<Option<Arc<RuntimeInner>>> = const { std::cell::RefCell::new(None) };
    static CANCEL_FLAG: std::cell::RefCell<Option<Arc<AtomicBool>>> = const { std::cell::RefCell::new(None) };
    static CANCEL_CHANNEL: std::cell::RefCell<Option<crossbeam_channel::Receiver<()>>> = const { std::cell::RefCell::new(None) };
}

/// Restores the prior synchronous runtime context when its scope exits.
pub struct RuntimeContextGuard {
    previous: Option<Arc<RuntimeInner>>,
}

impl Drop for RuntimeContextGuard {
    fn drop(&mut self) {
        RUNTIME.with(|cell| {
            *cell.borrow_mut() = self.previous.take();
        });
    }
}

/// A runtime context established for a test seam, whose root scope closes when
/// the context is uninstalled.
///
/// The plain `RuntimeContextGuard` restores the thread-local and nothing more,
/// which is correct for the one other thing that installs a context: a blocking
/// worker re-installing its SPAWNER's runtime, where closing that spawner's
/// scope would be a fault. A seam that establishes a runtime of its own owns
/// that runtime's whole lifecycle, so the close belongs here and not on the
/// shared guard.
///
/// The scope is closed, not drained. Closing is what the ownership contract
/// promises a child — `ScopeClosing` fires and every Camber-owned loop stops —
/// and the paused-clock test this seam serves has no thread it could block on a
/// drain.
///
/// The external cancellation watcher is aborted too. It is not a scope child,
/// so the close alone would leave it running: still holding the runtime, still
/// able to request a shutdown of a runtime nothing is installed for.
pub struct TestRuntimeContext {
    inner: Arc<RuntimeInner>,
    /// `Option` so `Drop` can order the steps explicitly: the watcher is
    /// aborted and the scope closes, then the previous context is restored.
    context: Option<RuntimeContextGuard>,
}

impl TestRuntimeContext {
    /// Pair an established runtime with the context guard that installed it.
    pub(crate) fn new(inner: Arc<RuntimeInner>, context: RuntimeContextGuard) -> Self {
        Self {
            inner,
            context: Some(context),
        }
    }
}

impl Drop for TestRuntimeContext {
    fn drop(&mut self) {
        teardown_runtime(&self.inner);
        self.inner.close_scope();
        drop(self.context.take());
    }
}

/// Restores per-task cancellation state when a blocking worker is reused.
pub(crate) struct CancelContextGuard {
    previous_flag: Option<Arc<AtomicBool>>,
    previous_channel: Option<crossbeam_channel::Receiver<()>>,
}

impl Drop for CancelContextGuard {
    fn drop(&mut self) {
        CANCEL_FLAG.with(|cell| {
            *cell.borrow_mut() = self.previous_flag.take();
        });
        CANCEL_CHANNEL.with(|cell| {
            *cell.borrow_mut() = self.previous_channel.take();
        });
    }
}

/// Install cancellation state for a blocking task and restore it on drop.
pub(crate) fn install_cancel_context(
    flag: Arc<AtomicBool>,
    channel: crossbeam_channel::Receiver<()>,
) -> CancelContextGuard {
    let previous_flag = CANCEL_FLAG.with(|cell| cell.borrow_mut().replace(flag));
    let previous_channel = CANCEL_CHANNEL.with(|cell| cell.borrow_mut().replace(channel));
    CancelContextGuard {
        previous_flag,
        previous_channel,
    }
}

/// Get the current task's cancellation channel receiver (if any).
pub(crate) fn cancel_channel() -> Option<crossbeam_channel::Receiver<()>> {
    CANCEL_CHANNEL.with(|cell| cell.borrow().clone())
}

/// Check whether the current task has been cancelled.
pub(crate) fn check_cancel() -> Result<(), crate::RuntimeError> {
    CANCEL_FLAG.with(|cell| {
        let borrow = cell.borrow();
        match borrow.as_ref() {
            Some(flag) if flag.load(Ordering::Acquire) => Err(crate::RuntimeError::Cancelled),
            _ => Ok(()),
        }
    })
}

/// Register an external shutdown signal. When `future` completes, Camber
/// treats it as a shutdown request. Calling again replaces the previous signal.
///
/// With no runtime established there is nothing to shut down, so the
/// registration is a no-op and `future` is dropped unpolled.
pub fn on_cancel<F>(future: F)
where
    F: Future<Output = ()> + Send + 'static,
{
    if let Some(inner) = try_current_runtime() {
        inner.watch_cancel(future);
    }
}

/// The runtime context established for this task or thread, if any.
///
/// Absence is a value the caller dispositions — no path fills it by minting a
/// default runtime, which would hand back an orphan no owner ever awaits.
pub(crate) fn try_current_runtime() -> Option<Arc<RuntimeInner>> {
    match TASK_RUNTIME.try_with(Arc::clone) {
        Ok(inner) => Some(inner),
        Err(_) => RUNTIME.with(|cell| cell.borrow().as_ref().map(Arc::clone)),
    }
}

/// The established runtime context, or the typed absence every
/// `Result`-returning entry point propagates.
pub(crate) fn runtime_context() -> Result<Arc<RuntimeInner>, crate::RuntimeError> {
    try_current_runtime().ok_or(crate::RuntimeError::NoRuntime)
}

/// Signal the runtime to shut down. A no-op with no runtime established.
pub fn request_shutdown() {
    if let Some(inner) = try_current_runtime() {
        inner.request_shutdown();
    }
}

/// Return the underlying Tokio runtime handle.
///
/// Use this inside handlers to run async code via `handle.block_on(...)`.
/// Panics if called outside a Camber runtime.
pub fn tokio_handle() -> tokio::runtime::Handle {
    tokio::runtime::Handle::current()
}

/// Check whether shutdown has been requested. False with no runtime: nothing
/// could have requested one.
pub fn is_shutting_down() -> bool {
    match try_current_runtime() {
        Some(inner) => inner.is_shutdown_requested(),
        None => false,
    }
}

/// Whether a runtime context is established for this task or thread.
///
/// Probes the two stores directly rather than going through
/// `try_current_runtime`: a presence test has no use for the `Arc`, and
/// cloning one to immediately drop it moves the refcount on every caller —
/// including `reject_nested_runtime`, which runs on every entry point.
pub(crate) fn has_runtime() -> bool {
    TASK_RUNTIME.try_with(|_| ()).is_ok() || RUNTIME.with(|cell| cell.borrow().is_some())
}

/// Bridge an async future to synchronous context.
///
/// Calls `block_in_place` + `Handle::block_on` internally. Use inside
/// `runtime::run` closures or `camber::spawn` tasks to call async code.
///
/// Tokio's `block_in_place` is called here directly, not through the total
/// `task::block_in_place` wrapper. The wrapper exists to keep a current-thread
/// runtime from panicking, and it cannot do that for this site: it would run
/// the closure inline, and `Handle::block_on` would then panic on the very
/// thread it is already blocking. Routing through it would swap one panic
/// message for another, so the bare call stays — it at least names the flavor
/// mismatch it failed on.
pub fn block_on<F: std::future::Future>(f: F) -> F::Output {
    tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(f))
}

#[must_use = "the runtime context is installed only while the returned guard lives"]
pub(crate) fn install_runtime(inner: Arc<RuntimeInner>) -> RuntimeContextGuard {
    let previous = RUNTIME.with(|cell| cell.borrow_mut().replace(inner));
    RuntimeContextGuard { previous }
}

/// Install a captured runtime context on this thread for as long as the guard
/// lives.
///
/// The synchronous counterpart of [`carry_runtime`], for a caller that runs a
/// blocking body rather than awaiting a future. Absence propagates the same
/// way: a captured `None` installs nothing and leaves the thread's own context
/// exactly as it found it, so no path fills runtime absence by minting one.
#[cfg(feature = "ws")]
#[must_use = "the carried context is installed only while the returned guard lives"]
pub(crate) fn install_carried_runtime(
    context: Option<Arc<RuntimeInner>>,
) -> Option<RuntimeContextGuard> {
    context.map(install_runtime)
}

/// Scope runtime context to a future so it follows that future across workers.
pub(crate) async fn scope_runtime<F>(inner: Arc<RuntimeInner>, future: F) -> F::Output
where
    F: Future,
{
    TASK_RUNTIME.scope(inner, future).await
}

/// Run a detached task under the runtime context its spawner captured.
///
/// Tokio task-locals do not cross `tokio::spawn`, so a task that must keep
/// seeing the runtime it was launched under carries the context in by value.
/// A captured `None` runs the future with no context — absence propagates, it
/// is never filled.
pub(crate) async fn carry_runtime<F>(context: Option<Arc<RuntimeInner>>, future: F) -> F::Output
where
    F: Future,
{
    match context {
        Some(inner) => scope_runtime(inner, future).await,
        None => future.await,
    }
}

/// Abort a lingering external cancellation watcher for this runtime.
pub(crate) fn teardown_runtime(inner: &RuntimeInner) {
    abort_cancel_watcher(inner.take_current_cancel_watcher());
}

/// Revoke, abort, and join the external cancellation watcher before resources
/// begin shutting down. Clearing the identity first makes a watcher already
/// inside `poll` stale before its abort can be observed.
pub(crate) fn stop_cancel_watcher(inner: &RuntimeInner, executor: &tokio::runtime::Handle) {
    let watcher = inner.take_current_cancel_watcher();
    let handle = match watcher {
        Some(watcher) => watcher.handle,
        None => return,
    };
    handle.abort();
    let joined =
        executor.block_on(async move { tokio::time::timeout(FORCED_JOIN_GRACE, handle).await });
    match joined {
        Ok(Err(error)) if error.is_panic() => inner
            .scope
            .record_internal_panic(crate::task::panic_to_error(error.into_panic())),
        Ok(Ok(())) | Ok(Err(_)) => {}
        Err(_) => tracing::warn!(
            "external cancellation watcher did not stop within the forced join grace"
        ),
    }
}

/// Drain the root scope, recording the participants it could not prove
/// finished.
///
/// The aggregate deadline is the drain's ESCALATION BOUNDARY, not a bound on
/// total teardown: children get whatever the one shared expiry has left to exit
/// on `ScopeClosing`, then every retained async handle is aborted and joined
/// under the fixed forced-join grace so no child the executor can stop outlives
/// the drain. Resource shutdown runs after, under the remainder of that same
/// expiry.
///
/// The scope settles as one participant and its children as another: the root
/// scope reports whether its own bounded drain finished, and the background
/// children report whether the forced stop got their joins back.
pub(crate) fn drain_root_scope(
    inner: &RuntimeInner,
    executor: &tokio::runtime::Handle,
    log: &mut crate::lifecycle::LifecycleFailureLog,
) {
    let shutdown = inner.shutdown_deadline();
    let remaining = shutdown.bounded(
        &LifecycleParticipant::RootScope,
        inner.config.server_policy.shutdown_timeout_value(),
    );
    let outstanding = inner
        .scope
        .wait_timeout(remaining, inner.test_schedule.as_deref());
    match outstanding {
        0 => shutdown.settle(
            &LifecycleParticipant::RootScope,
            ParticipantDisposition::Completed,
        ),
        count => {
            executor.block_on(inner.scope.force_stop(&shutdown));
            log.record(
                LifecycleParticipant::RootScope,
                LifecyclePhase::GracefulDrain,
                LifecycleFailureKind::ScopeDrainTimeout { outstanding: count },
            );
            shutdown.settle(
                &LifecycleParticipant::RootScope,
                ParticipantDisposition::Named,
            );
        }
    }
    inner.observe_drain_end();
}
