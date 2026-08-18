use crate::RuntimeError;
use crate::runtime_state::{RuntimeConfig, RuntimeInner, recover_poisoned};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};

/// Runtime scheduling points exposed only for deterministic integration tests.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RuntimeCheckpoint {
    /// The shutdown notification is registered, before sticky state is read.
    ShutdownWaitRegistered,
    /// A child's admission raised the root-scope count, before its body runs.
    AdmissionCounted,
    /// A child's joinable handle is registered and its start gate is created,
    /// before the gate opens and its body runs.
    AdmissionRegistered,
    /// The root scope is about to perform its atomic `Open -> Closing` step.
    ScopeCloseTransition,
    /// The drain observed this child count before waiting for it to change.
    ScopeWaitObserved(usize),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CheckpointPhase {
    Armed,
    Paused,
    Released,
}

struct CheckpointState {
    closed: bool,
    checkpoint: Option<(RuntimeCheckpoint, CheckpointPhase)>,
}

pub(crate) struct RuntimeSchedule {
    state: Mutex<CheckpointState>,
    changed: Condvar,
    runtime: OnceLock<Weak<RuntimeInner>>,
    /// Set when a second builder tried to attach. Latched rather than logged
    /// alone, because the probes are what must refuse afterwards.
    conflicted: AtomicBool,
    /// Resources whose next lifecycle callback gets no worker admitted.
    ///
    /// A scheduling decision, not a result: the seam declines to start a
    /// worker, exactly as the operating system can, and production decides what
    /// a callback with no worker is called.
    refused_workers: Mutex<std::collections::HashSet<Box<str>>>,
}

impl RuntimeSchedule {
    fn new() -> Self {
        Self {
            state: Mutex::new(CheckpointState {
                closed: false,
                checkpoint: None,
            }),
            changed: Condvar::new(),
            runtime: OnceLock::new(),
            conflicted: AtomicBool::new(false),
            refused_workers: Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Admit no worker for this resource's lifecycle callbacks until the
    /// refusal is lifted.
    fn refuse_resource_worker(&self, resource: &str) {
        recover_poisoned(self.refused_workers.lock()).insert(Box::from(resource));
    }

    /// Admit workers for this resource again.
    fn admit_resource_worker(&self, resource: &str) {
        recover_poisoned(self.refused_workers.lock()).remove(resource);
    }

    /// Whether this resource's next callback gets no worker.
    pub(crate) fn refuses_resource_worker(&self, resource: &str) -> bool {
        recover_poisoned(self.refused_workers.lock()).contains(resource)
    }

    /// Publish the runtime this controller was attached to, so the read-only
    /// scope probes name one runtime instead of an ambient context a plain
    /// observer thread does not have.
    pub(crate) fn attach_runtime(&self, runtime: &Arc<RuntimeInner>) {
        match self.runtime.set(Arc::downgrade(runtime)) {
            Ok(()) => {}
            Err(_) => self.record_conflict(),
        }
    }

    /// Record that one controller was attached to more than one builder.
    ///
    /// The warning alone reached nobody: a bare `cargo test` installs no
    /// subscriber, so the misuse left no output and the second runtime's probes
    /// silently reported on the first one — a leak probe passing for the wrong
    /// reason. The flag is what makes every later probe refuse instead, so the
    /// test fails where it reads the wrong runtime rather than passing.
    fn record_conflict(&self) {
        self.conflicted.store(true, Ordering::Release);
        tracing::warn!("scheduling controller already has a runtime attached");
    }

    /// The attached runtime, while it is still alive.
    ///
    /// The three failures are different bugs in the test that hit them — a
    /// controller attached to two builders, one never attached to a builder,
    /// and one whose runtime has already been torn down — so they are reported
    /// apart rather than as one `NoRuntime`, which names a production absence
    /// none of these is.
    fn attached_runtime(&self) -> Result<Arc<RuntimeInner>, RuntimeError> {
        match (self.conflicted.load(Ordering::Acquire), self.runtime.get()) {
            (true, _) => Err(Self::invalid(
                "scheduling controller was attached to more than one runtime",
            )),
            (false, None) => Err(Self::invalid(
                "scheduling controller has no runtime attached",
            )),
            (false, Some(runtime)) => Weak::upgrade(runtime).ok_or_else(|| {
                Self::invalid("scheduling controller's attached runtime was already dropped")
            }),
        }
    }

    fn invalid(message: &'static str) -> RuntimeError {
        RuntimeError::InvalidArgument(message.into())
    }

    /// The checkpoint state.
    ///
    /// The recovery `recover_poisoned` performs matters here because the other
    /// side of the seam is production code parked at a checkpoint: refusing the
    /// lock would leave it parked with nobody able to take `close` through.
    fn state(&self) -> MutexGuard<'_, CheckpointState> {
        recover_poisoned(self.state.lock())
    }

    /// Sleep on `changed` until it is notified, handing the guard over and
    /// taking it back on the same terms as [`Self::state`].
    fn wait_changed<'a>(
        &self,
        state: MutexGuard<'a, CheckpointState>,
    ) -> MutexGuard<'a, CheckpointState> {
        recover_poisoned(self.changed.wait(state))
    }

    fn arm(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        let mut state = self.state();
        match (state.closed, state.checkpoint) {
            (true, _) => Err(Self::invalid("runtime scheduling controller is closed")),
            (false, Some((_, CheckpointPhase::Armed | CheckpointPhase::Paused))) => Err(
                Self::invalid("runtime scheduling checkpoint is already armed"),
            ),
            (false, None | Some((_, CheckpointPhase::Released))) => {
                state.checkpoint = Some((checkpoint, CheckpointPhase::Armed));
                Ok(())
            }
        }
    }

    /// Whether an open controller currently holds this exact checkpoint in
    /// this exact phase. A closed controller holds nothing.
    fn in_phase(&self, checkpoint: RuntimeCheckpoint, phase: CheckpointPhase) -> bool {
        let state = self.state();
        matches!(
            (state.closed, state.checkpoint),
            (false, Some((held, held_phase))) if held == checkpoint && held_phase == phase
        )
    }

    pub(crate) fn is_armed(&self, checkpoint: RuntimeCheckpoint) -> bool {
        self.in_phase(checkpoint, CheckpointPhase::Armed)
    }

    pub(crate) fn pause(&self, checkpoint: RuntimeCheckpoint) {
        let mut state = self.state();
        match (state.closed, state.checkpoint) {
            (false, Some((armed, CheckpointPhase::Armed))) if armed == checkpoint => {
                state.checkpoint = Some((checkpoint, CheckpointPhase::Paused));
                self.changed.notify_all();
            }
            _ => return,
        }

        while matches!(
            state.checkpoint,
            Some((paused, CheckpointPhase::Paused)) if paused == checkpoint
        ) && !state.closed
        {
            state = self.wait_changed(state);
        }
    }

    fn is_paused(&self, checkpoint: RuntimeCheckpoint) -> bool {
        self.in_phase(checkpoint, CheckpointPhase::Paused)
    }

    fn wait_until_paused(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        let mut state = self.state();
        loop {
            match (state.closed, state.checkpoint) {
                (true, _) => {
                    return Err(Self::invalid("runtime scheduling controller is closed"));
                }
                (false, Some((paused, CheckpointPhase::Paused))) if paused == checkpoint => {
                    return Ok(());
                }
                (false, Some((released, CheckpointPhase::Released))) if released == checkpoint => {
                    return Err(Self::invalid(
                        "runtime scheduling checkpoint was already released",
                    ));
                }
                (false, Some((armed, _))) if armed != checkpoint => {
                    return Err(Self::invalid(
                        "runtime scheduling checkpoint does not match the armed checkpoint",
                    ));
                }
                (false, None) => {
                    return Err(Self::invalid("runtime scheduling checkpoint is not armed"));
                }
                (false, Some(_)) => state = self.wait_changed(state),
            }
        }
    }

    fn release(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        let mut state = self.state();
        match (state.closed, state.checkpoint) {
            (true, _) => Err(Self::invalid("runtime scheduling controller is closed")),
            (false, Some((paused, CheckpointPhase::Paused))) if paused == checkpoint => {
                state.checkpoint = Some((checkpoint, CheckpointPhase::Released));
                self.changed.notify_all();
                Ok(())
            }
            (false, Some((armed, _))) if armed != checkpoint => Err(Self::invalid(
                "runtime scheduling checkpoint does not match the armed checkpoint",
            )),
            (false, _) => Err(Self::invalid("runtime scheduling checkpoint is not paused")),
        }
    }

    /// Give up on the held checkpoint, whatever phase it is in.
    ///
    /// An observer that waits for a pause and never sees one leaves the
    /// checkpoint `Armed`. Production reaching it afterwards then pauses with
    /// nobody left to release it, and the runtime never returns — the observer
    /// converted its own bounded failure into a hang. Disarming makes a
    /// checkpoint no test is waiting on a no-op: an `Armed` one is dropped so
    /// `pause` falls straight through, and a `Paused` one is released so the
    /// child already held there resumes. Closing would do both, but a closed
    /// controller also refuses every later call, which a test still using its
    /// other checkpoints cannot afford.
    fn disarm(&self) {
        let mut state = self.state();
        state.checkpoint = match state.checkpoint {
            Some((checkpoint, CheckpointPhase::Paused)) => {
                Some((checkpoint, CheckpointPhase::Released))
            }
            _ => None,
        };
        self.changed.notify_all();
    }

    fn close(&self) {
        let mut state = self.state();
        state.closed = true;
        self.changed.notify_all();
    }
}

/// Controller for one runtime instance's deterministic scheduling seam.
#[doc(hidden)]
pub struct RuntimeController {
    schedule: Arc<RuntimeSchedule>,
}

impl RuntimeController {
    /// Arm one value-matched checkpoint.
    pub fn pause_once(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        self.schedule.arm(checkpoint)
    }

    /// Block until production code reaches the armed checkpoint.
    pub fn wait_until_paused(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        self.schedule.wait_until_paused(checkpoint)
    }

    /// Whether production code is paused at `checkpoint` right now.
    ///
    /// Read-only, and the predicate a test polls when it must probe the
    /// checkpoint while production is still held there: the blocking wait has
    /// no deadline, so an observation that never comes would park the observer
    /// instead of failing its test.
    pub fn is_paused(&self, checkpoint: RuntimeCheckpoint) -> bool {
        self.schedule.is_paused(checkpoint)
    }

    /// Release production code from the paused checkpoint.
    pub fn release(&self, checkpoint: RuntimeCheckpoint) -> Result<(), RuntimeError> {
        self.schedule.release(checkpoint)
    }

    /// Abandon whatever checkpoint is held, so a bounded observation that
    /// expired cannot leave production paused with no observer.
    ///
    /// This is the failure path's counterpart to `release`: `release` reports
    /// an error when nothing is paused, which is exactly the state an expired
    /// observer is in, so it cannot be used to clean up after itself. Infallible
    /// and idempotent — holding nothing is the state it establishes.
    pub fn disarm(&self) {
        self.schedule.disarm();
    }

    /// How many children the attached runtime's root scope retains an entry
    /// for: one joinable handle per async child, one tally entry per
    /// non-preemptible blocking child. Read-only.
    pub fn scope_registry_len(&self) -> Result<usize, RuntimeError> {
        Ok(self.schedule.attached_runtime()?.scope_registry_len())
    }

    /// How many children the scope owner has awaited to Tokio-handle
    /// completion — the join acknowledgment, which registry removal and a
    /// zero count alone do not establish. Read-only.
    pub fn scope_joined_count(&self) -> Result<usize, RuntimeError> {
        Ok(self.schedule.attached_runtime()?.scope_joined_count())
    }

    /// Admit no worker for `resource`'s lifecycle callbacks.
    ///
    /// The seam declines to START a worker, which is the one refusal the
    /// operating system can also deliver. It manufactures no result: what a
    /// callback with no worker is called, and where that name is reported, stay
    /// entirely with the production coordinator.
    pub fn refuse_resource_worker(&self, resource: &str) {
        self.schedule.refuse_resource_worker(resource);
    }

    /// Admit workers for `resource` again.
    pub fn admit_resource_worker(&self, resource: &str) {
        self.schedule.admit_resource_worker(resource);
    }

    pub(crate) fn schedule(&self) -> Arc<RuntimeSchedule> {
        Arc::clone(&self.schedule)
    }
}

impl Drop for RuntimeController {
    fn drop(&mut self) {
        self.schedule.close();
    }
}

/// Create an unregistered controller for one runtime builder.
#[doc(hidden)]
pub fn runtime_schedule() -> RuntimeController {
    RuntimeController {
        schedule: Arc::new(RuntimeSchedule::new()),
    }
}

/// A runtime context installed on the current thread for the duration of a
/// test. Dropping it closes that runtime's root scope, then restores the
/// previous context.
#[doc(hidden)]
pub use crate::runtime_state::TestRuntimeContext;

/// Establish a runtime context on the current thread, for a test that drives a
/// server outside `runtime::run`.
///
/// A paused-clock Tokio test cannot host the blocking `run` entry, yet a server
/// it starts still needs a real runtime to observe shutdown from. Establishing
/// one explicitly is what the removed implicit context mint used to do by
/// accident.
///
/// The returned guard IS the context: discarding it uninstalls immediately,
/// which is the one failure this entry exists to prevent. Dropping it aborts
/// this runtime's external cancellation watcher, closes its root scope, and
/// then restores the previous context — so a child admitted through the seam
/// observes `ScopeClosing` the way `run` would have given it. Without that
/// close the seam would model the opposite of the ownership contract it exists
/// to support: every child it admitted would be silently orphaned.
///
/// The runtime itself is established through `runtime::establish_runtime`, the
/// same function the two executor-owning entry points enter, so this seam
/// cannot drift from what production means by a running Camber runtime. It owns
/// no executor of its own: the ambient Tokio handle is taken when one is
/// entered, and absence propagates as the `NoRuntime` a runtime with nowhere to
/// launch a child already reports.
#[doc(hidden)]
#[must_use = "the context is uninstalled as soon as the guard is dropped"]
pub fn install_runtime_context() -> TestRuntimeContext {
    install_configured_runtime_context(RuntimeConfig::default())
}

/// Install a test runtime context that bounds no admitted request's own time.
///
/// A fixture that holds one request open for the whole of its claim needs that
/// request's deadlines to decide nothing. Under paused time that is not a
/// remote possibility: the clock advances to the next timer whenever the
/// runtime idles, which is exactly while such a fixture waits on the socket I/O
/// its observation is built on.
///
/// It has to be set here rather than on the server, because the runtime is the
/// outer authority: an inner unbounded server policy inherits the outer bound
/// instead of erasing it, which is the precedence rule working as designed.
#[doc(hidden)]
pub fn install_runtime_context_without_request_deadlines() -> TestRuntimeContext {
    let mut config = RuntimeConfig::default();
    config.server_policy = config
        .server_policy
        .request_budget(crate::http::RequestBudget::unbounded());
    install_configured_runtime_context(config)
}

/// Establish one test runtime context from the configuration it is given.
fn install_configured_runtime_context(config: RuntimeConfig) -> TestRuntimeContext {
    let (inner, context) = crate::runtime::establish_runtime(
        tokio::runtime::Handle::try_current().ok(),
        config,
        None,
        None,
        None,
    );
    TestRuntimeContext::new(inner, context)
}

/// Await the root scope's `ScopeClosing` signal from a test-owned child.
///
/// An observation future, not a checkpoint: it alters no scheduling, count,
/// or shutdown state.
///
/// With no runtime established there is no scope to close, so the observer
/// parks rather than allocating an inert latch no owner could ever fire.
#[doc(hidden)]
pub async fn wait_scope_closing() {
    match crate::runtime::try_current_runtime() {
        Some(runtime) => runtime.scope_closing().wait().await,
        None => std::future::pending().await,
    }
}

/// Admit the OS signal watcher loop the runtime uses, with no signal ever
/// delivered.
///
/// The loop is crate-private, so a test cannot build it. This entry does not
/// rebuild it either: it calls the very function the runtime's own setup calls,
/// so the construction AND the admission wrapper under test are production's.
/// Alters no scheduling, count, or shutdown state beyond that admission.
///
/// # Errors
///
/// Propagates the admission outcome: `NoRuntime` with no runtime context,
/// `ScopeClosed` once admission has closed.
#[doc(hidden)]
pub fn admit_signal_watcher_for_test() -> Result<(), RuntimeError> {
    crate::runtime::admit_signal_watcher(&crate::runtime::runtime_context()?)
}

/// Admit the ACME renewal loop the runtime uses, driven by a scripted event
/// list instead of a live ACME directory stream.
///
/// The loop consumes the scripted events and then stays pending, so only a
/// lifecycle signal can end it. Admitted through the same named-subsystem
/// wrapper the runtime's own setup uses, so the seam proves production's
/// admission path and not one of its own.
///
/// # Errors
///
/// Propagates the admission outcome: `NoRuntime` with no runtime context,
/// `ScopeClosed` once admission has closed.
#[cfg(feature = "acme")]
#[doc(hidden)]
pub fn admit_acme_renewal_for_test(
    events: Box<[Result<Box<str>, Box<str>>]>,
) -> Result<(), RuntimeError> {
    use futures_util::StreamExt;

    let scripted = futures_util::stream::iter(events)
        .chain(futures_util::stream::pending::<Result<Box<str>, Box<str>>>());
    crate::task::admit_signalled_subsystem_on(
        &crate::runtime::runtime_context()?,
        "acme renewal",
        move |signals| crate::acme::acme_renewal_loop(scripted, signals),
    )
}

/// Admit the DNS-01 renewal loop the runtime uses, against a test-supplied
/// cert store and DNS provider.
///
/// The renewal check interval is measured in hours, so the provider is never
/// reached: only a lifecycle signal ends the loop. Admitted through the same
/// named-subsystem wrapper the runtime's own setup uses, so the seam proves
/// production's admission path and not one of its own.
///
/// # Errors
///
/// Propagates the admission outcome: `NoRuntime` with no runtime context,
/// `ScopeClosed` once admission has closed.
#[cfg(feature = "dns01")]
#[doc(hidden)]
pub fn admit_dns01_renewal_for_test<P>(
    store: crate::tls::CertStore,
    provider: P,
) -> Result<(), RuntimeError>
where
    P: crate::dns01::DnsProvider + 'static,
{
    let acme = crate::dns01::AcmeDns01::new("camber-dns01-renewal-test", ["localhost"]);
    crate::task::admit_signalled_subsystem_on(
        &crate::runtime::runtime_context()?,
        "dns01 renewal",
        move |signals| crate::dns01::dns01_renewal_loop(acme, provider, store, signals),
    )
}
