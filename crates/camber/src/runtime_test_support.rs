use crate::RuntimeError;
use crate::lifecycle::ShutdownOwner;
use crate::runtime_state::{RuntimeConfig, RuntimeInner, recover_poisoned};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock, Weak};
use std::time::Duration;

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

/// The one aggregate shutdown deadline production minted, as the transition
/// that minted it fixed it.
///
/// Read-only, and written by the coordinator that performed the mint: a test
/// cannot mint, extend, or replace a deadline through this type.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ShutdownDeadlineMint {
    at: tokio::time::Instant,
    expiry: tokio::time::Instant,
}

impl ShutdownDeadlineMint {
    /// The instant the first graceful transition was taken at.
    #[must_use]
    pub const fn at(&self) -> tokio::time::Instant {
        self.at
    }

    /// The absolute expiry that transition fixed.
    #[must_use]
    pub const fn expiry(&self) -> tokio::time::Instant {
        self.expiry
    }

    /// The grace between the two, as the configured value it was minted from.
    #[must_use]
    pub fn grace(&self) -> Duration {
        self.expiry.saturating_duration_since(self.at)
    }
}

/// One framework owner's reading of the shared aggregate shutdown deadline.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShutdownDeadlineReading {
    participant: Box<str>,
    expiry: tokio::time::Instant,
}

impl ShutdownDeadlineReading {
    /// The owner that read the deadline, under the bounded name the lifecycle
    /// vocabulary reports it as.
    #[must_use]
    pub fn participant(&self) -> &str {
        &self.participant
    }

    /// The absolute expiry that owner was given.
    #[must_use]
    pub const fn expiry(&self) -> tokio::time::Instant {
        self.expiry
    }
}

/// How one framework-owned shutdown participant was disposed of.
///
/// Closed and exhaustively matchable, and chosen entirely by production: the
/// three values are the three outcomes an aggregate teardown is allowed to
/// reach, so an owner missing from the record is a defect rather than a fourth
/// disposition.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParticipantDisposition {
    /// The owner finished its own work before the shared deadline.
    Completed,
    /// The owner was cancelled and its join was acknowledged.
    CancelledAndJoined,
    /// The owner could not be proven finished, and is named in the returned
    /// aggregate instead.
    Named,
}

impl ParticipantDisposition {
    /// The bounded name this disposition is recorded under.
    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::CancelledAndJoined => "cancelled-and-joined",
            Self::Named => "named",
        }
    }
}

/// One participant's recorded settlement.
#[doc(hidden)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParticipantSettlement {
    participant: Box<str>,
    disposition: ParticipantDisposition,
}

impl ParticipantSettlement {
    /// The owner that settled.
    #[must_use]
    pub fn participant(&self) -> &str {
        &self.participant
    }

    /// How it settled.
    #[must_use]
    pub const fn disposition(&self) -> ParticipantDisposition {
        self.disposition
    }
}

/// What one run published about the children its root scope admitted.
///
/// Facts about named children, and one fact about the scope itself. There is no
/// total here on purpose: a count answers "how many children exist", which is a
/// question about the whole runtime, and every case that used to ask it was
/// really asking whether one child it had just started had been admitted,
/// retained, joined, or let go.
#[derive(Default)]
struct ScopeSettlementObservations {
    /// Claims taken before an admission, oldest first.
    ///
    /// One claim binds to one admission, in the order both happened, so a case
    /// that claims and then starts a child names that child and no other.
    claims: std::collections::VecDeque<ScopeClaim>,
    /// Which admitted child each bound claim named.
    named: std::collections::HashMap<ScopeClaim, ScopeChildId>,
    /// Which admitted child production admitted under each subsystem name.
    ///
    /// The other half of naming, for the Camber-owned loops the runtime starts
    /// for itself. Those admissions happen during startup, before any case body
    /// runs, so no claim could have been taken ahead of them — but production
    /// already carries the name it admitted each one under, and that name binds
    /// to the child rather than to a position in the admission order.
    subsystems: std::collections::HashMap<Box<str>, ScopeChildId>,
    /// Children the scope owner retains a way to stop.
    retained: std::collections::HashSet<ScopeChildId>,
    /// Children whose Tokio handle the scope owner awaited to completion.
    joined: std::collections::HashSet<ScopeChildId>,
    /// Children that have left the scope.
    settled: std::collections::HashSet<ScopeChildId>,
    /// Whether the root scope itself has drained.
    drained: bool,
    /// The claim minted next.
    next_claim: ScopeClaim,
}

/// One case's claim on the child a runtime admits next.
type ScopeClaim = u64;

/// How one case identifies the root-scope child it is asking about.
///
/// Two ways in, because there are two kinds of child. A case that starts its
/// own child claims the next admission before starting it. A Camber-owned loop
/// the runtime started during its own setup was admitted before any case body
/// ran, so it is identified by the name production admitted it under.
enum ScopeSubject {
    /// The child bound to the claim this case took before starting it.
    Admission(ScopeClaim),
    /// The child production admitted under this subsystem name.
    Subsystem(Box<str>),
}

/// The identity production minted for one admitted child.
pub(crate) type ScopeChildId = u64;

/// Every aggregate-shutdown observation one run published.
#[derive(Default)]
struct ShutdownObservations {
    mint: Option<ShutdownDeadlineMint>,
    mints: usize,
    readings: Vec<ShutdownDeadlineReading>,
    settlements: Vec<ParticipantSettlement>,
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
    /// What the aggregate shutdown coordinator published this run.
    ///
    /// Observations only. Nothing here alters a deadline, a disposition, or an
    /// aggregate entry; production writes each row at the owner that decided
    /// it.
    shutdown: Mutex<ShutdownObservations>,
    /// What the root scope published about the children it admitted.
    ///
    /// Observations only, on the same terms as `shutdown`: production admits,
    /// retains, joins, and releases exactly as it would with nothing attached.
    scope: Mutex<ScopeSettlementObservations>,
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
            shutdown: Mutex::new(ShutdownObservations::default()),
            scope: Mutex::new(ScopeSettlementObservations::default()),
        }
    }

    fn shutdown_observations(&self) -> MutexGuard<'_, ShutdownObservations> {
        recover_poisoned(self.shutdown.lock())
    }

    fn scope_observations(&self) -> MutexGuard<'_, ScopeSettlementObservations> {
        recover_poisoned(self.scope.lock())
    }

    /// Claim the child this runtime admits next.
    fn claim_next_admission(&self) -> ScopeClaim {
        let mut observed = self.scope_observations();
        let claim = observed.next_claim;
        observed.next_claim = claim.wrapping_add(1);
        observed.claims.push_back(claim);
        claim
    }

    /// Bind the oldest waiting claim to the child production just admitted.
    ///
    /// A run with no claim waiting binds no positional claim: the unnamed
    /// children a runtime admits for itself are not the subject of any case,
    /// and holding them would grow that map for the life of a long run.
    ///
    /// A named admission takes no claim. The name production carried is the
    /// whole handle on such a child. A Camber-owned loop reaches admission from
    /// a case body as readily as from startup: the renewal and signal-watcher
    /// seams do exactly that. Binding it positionally as well would hand a case
    /// holding an outstanding claim a loop it never started, in place of the
    /// child it took the claim for.
    pub(crate) fn record_scope_admission(&self, child: ScopeChildId, subsystem: Option<&str>) {
        let mut observed = self.scope_observations();
        let positional_claim = match subsystem {
            Some(subsystem) => {
                observed.subsystems.insert(Box::from(subsystem), child);
                None
            }
            None => observed.claims.pop_front(),
        };
        if let Some(claim) = positional_claim {
            observed.named.insert(claim, child);
        }
    }

    /// Record that the scope owner now retains a way to stop this child.
    pub(crate) fn record_scope_retention(&self, child: ScopeChildId) {
        self.scope_observations().retained.insert(child);
    }

    /// Record that the scope owner awaited this child's handle to completion.
    pub(crate) fn record_scope_join(&self, child: ScopeChildId) {
        self.scope_observations().joined.insert(child);
    }

    /// Record that this child has left the scope.
    pub(crate) fn record_scope_settlement(&self, child: ScopeChildId) {
        let mut observed = self.scope_observations();
        observed.retained.remove(&child);
        observed.settled.insert(child);
    }

    /// Record that the root scope itself has drained.
    pub(crate) fn record_scope_drained(&self) {
        self.scope_observations().drained = true;
    }

    /// Whether one subject's child has reached the fact `reached` reads.
    ///
    /// A subject production has not admitted yet answers `false`: the child does
    /// not exist, so it has reached nothing. That is a different answer from the
    /// refusal an unattached or double-attached controller gives, which is why
    /// the two leave through different sides of the `Result`.
    fn child_reached(
        &self,
        subject: &ScopeSubject,
        reached: impl FnOnce(&ScopeSettlementObservations, ScopeChildId) -> bool,
    ) -> Result<bool, RuntimeError> {
        self.attached()?;
        let observed = self.scope_observations();
        let child = match subject {
            ScopeSubject::Admission(claim) => observed.named.get(claim).copied(),
            ScopeSubject::Subsystem(name) => observed.subsystems.get(name).copied(),
        };
        Ok(match child {
            Some(child) => reached(&observed, child),
            None => false,
        })
    }

    /// Record the one mint a graceful transition performed.
    pub(crate) fn record_deadline_mint(
        &self,
        at: tokio::time::Instant,
        expiry: tokio::time::Instant,
    ) {
        let mut observed = self.shutdown_observations();
        observed.mints += 1;
        observed
            .mint
            .get_or_insert(ShutdownDeadlineMint { at, expiry });
    }

    /// Record one owner's reading of the shared expiry.
    pub(crate) fn record_deadline_reading(
        &self,
        owner: &ShutdownOwner,
        expiry: tokio::time::Instant,
    ) {
        self.shutdown_observations()
            .readings
            .push(ShutdownDeadlineReading {
                participant: owner.to_string().into_boxed_str(),
                expiry,
            });
    }

    /// Record how one owner was disposed of.
    pub(crate) fn record_settlement(
        &self,
        owner: &ShutdownOwner,
        disposition: ParticipantDisposition,
    ) {
        self.shutdown_observations()
            .settlements
            .push(ParticipantSettlement {
                participant: owner.to_string().into_boxed_str(),
                disposition,
            });
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

    /// Whether exactly one runtime published itself to this controller.
    ///
    /// The two failures are different bugs in the test that hit them — a
    /// controller attached to two builders, and one never attached to a builder
    /// at all — so they are reported apart rather than as one `NoRuntime`,
    /// which names a production absence neither of these is.
    ///
    /// A torn-down runtime is NOT a failure here. What this guards is a set of
    /// recorded facts, and a fact outlives the owner that recorded it: every
    /// case that reads what a child settled as reads it after `run` returned.
    fn attached(&self) -> Result<(), RuntimeError> {
        match (self.conflicted.load(Ordering::Acquire), self.runtime.get()) {
            (true, _) => Err(Self::invalid(
                "scheduling controller was attached to more than one runtime",
            )),
            (false, None) => Err(Self::invalid(
                "scheduling controller has no runtime attached",
            )),
            (false, Some(_)) => Ok(()),
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

    /// The narrow controller for this runtime's root scope settlements.
    ///
    /// Owner-local: it names one child the scope admits and reads what that
    /// child reached. It admits nothing, stops nothing, joins nothing, and
    /// answers no question about how many children the runtime holds.
    pub fn scope_settlement(&self) -> ScopeSettlementController {
        ScopeSettlementController {
            schedule: Arc::clone(&self.schedule),
        }
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

    /// How many aggregate shutdown deadlines production minted. Read-only.
    ///
    /// One is the whole contract: a second mint is a nested owner restarting a
    /// deadline the first transition already fixed.
    pub fn shutdown_deadline_mints(&self) -> usize {
        self.schedule.shutdown_observations().mints
    }

    /// The one mint, as the transition that took it fixed it. Read-only.
    pub fn shutdown_deadline_mint(&self) -> Option<ShutdownDeadlineMint> {
        self.schedule.shutdown_observations().mint
    }

    /// Every owner's reading of the shared expiry, in the order they read it.
    /// Read-only.
    pub fn shutdown_deadline_readings(&self) -> Box<[ShutdownDeadlineReading]> {
        self.schedule
            .shutdown_observations()
            .readings
            .clone()
            .into_boxed_slice()
    }

    /// Every participant settlement production recorded, in settle order.
    /// Read-only.
    pub fn participant_settlements(&self) -> Box<[ParticipantSettlement]> {
        self.schedule
            .shutdown_observations()
            .settlements
            .clone()
            .into_boxed_slice()
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

/// The narrow controller over one runtime's root-scope settlements.
///
/// Three powers and no others: name the child the scope admits next, name a
/// Camber-owned subsystem the runtime admitted for itself, and read whether the
/// scope itself has drained. It cannot admit a child, stop one, join one, choose
/// a drain window, or count what the runtime holds — so a case built on it
/// states what happened to a child it named rather than what the whole runtime
/// happened to contain while it looked.
#[doc(hidden)]
pub struct ScopeSettlementController {
    schedule: Arc<RuntimeSchedule>,
}

impl ScopeSettlementController {
    /// Name the child this runtime's root scope admits next.
    ///
    /// Taken before the child is started, so the claim binds to that admission
    /// and not to whichever child the runtime happened to admit first. Claims
    /// bind in the order they were taken.
    pub fn name_next_admission(&self) -> AdmittedScope {
        self.name(ScopeSubject::Admission(
            self.schedule.claim_next_admission(),
        ))
    }

    /// Name the Camber-owned subsystem this runtime admitted under `subsystem`.
    ///
    /// The only form that reaches a Camber-owned loop: the resource health
    /// coordinator, the signal watcher, the renewal loops. The runtime admits
    /// some of them during its own setup, and a doc-hidden seam admits the rest
    /// from the case body. A startup admission is already past by the time that
    /// body runs, so no claim could precede it. A named admission takes no
    /// positional claim either way, so a case holding one keeps it for the child
    /// it started. A name production never admitted answers "not yet" to every
    /// fact, which fails the case that named it rather than passing on another
    /// child.
    ///
    /// One name holds one child, and the last admission under it wins. A run
    /// that admits two children under one string — a fixture admitting a second
    /// signal watcher beside the one every startup admits — leaves this bound to
    /// the later of the two. Name a subsystem the runtime admits once, or take a
    /// positional claim through
    /// [`name_next_admission`](Self::name_next_admission), which binds to the
    /// admission it precedes whatever it is called.
    pub fn name_subsystem(&self, subsystem: &str) -> AdmittedScope {
        self.name(ScopeSubject::Subsystem(Box::from(subsystem)))
    }

    /// Attach one subject to this runtime's observations.
    fn name(&self, subject: ScopeSubject) -> AdmittedScope {
        AdmittedScope {
            schedule: Arc::clone(&self.schedule),
            subject,
        }
    }

    /// Whether the root scope has drained: admission closed and no child left.
    ///
    /// The scope's own settled fact, published at the transition production
    /// takes. It says nothing about how many children ever existed.
    ///
    /// # Errors
    ///
    /// Refuses a controller attached to no runtime or to more than one.
    pub fn drained(&self) -> Result<bool, RuntimeError> {
        self.schedule.attached()?;
        Ok(self.schedule.scope_observations().drained)
    }
}

/// One named child of a runtime's root scope, and what it has reached.
///
/// Every answer is `false` until production admits the child this names, so a
/// case that reads a fact before its subject exists sees "not yet" rather than
/// another child's answer.
#[doc(hidden)]
pub struct AdmittedScope {
    schedule: Arc<RuntimeSchedule>,
    subject: ScopeSubject,
}

impl AdmittedScope {
    /// Whether the scope has admitted this child.
    ///
    /// # Errors
    ///
    /// Refuses a controller attached to no runtime or to more than one.
    pub fn admitted(&self) -> Result<bool, RuntimeError> {
        self.schedule.child_reached(&self.subject, |_, _| true)
    }

    /// Whether the scope owner retains a way to stop this child: its joinable
    /// handle for an async child, its tally entry for a blocking one.
    ///
    /// # Errors
    ///
    /// Refuses a controller attached to no runtime or to more than one.
    pub fn retained(&self) -> Result<bool, RuntimeError> {
        self.schedule
            .child_reached(&self.subject, |observed, child| {
                observed.retained.contains(&child)
            })
    }

    /// Whether the scope owner awaited this child's Tokio handle to
    /// completion.
    ///
    /// The join acknowledgment, which the child leaving the scope does not
    /// establish on its own.
    ///
    /// # Errors
    ///
    /// Refuses a controller attached to no runtime or to more than one.
    pub fn joined(&self) -> Result<bool, RuntimeError> {
        self.schedule
            .child_reached(&self.subject, |observed, child| {
                observed.joined.contains(&child)
            })
    }

    /// Whether this child has left the scope.
    ///
    /// # Errors
    ///
    /// Refuses a controller attached to no runtime or to more than one.
    pub fn settled(&self) -> Result<bool, RuntimeError> {
        self.schedule
            .child_reached(&self.subject, |observed, child| {
                observed.settled.contains(&child)
            })
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
