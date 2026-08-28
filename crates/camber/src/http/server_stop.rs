//! The one causal stop state an owned server commits its control facts into.
//!
//! Every owned server has exactly one of these. The public handle, the
//! supervisor, and each connection owner share it, and one short synchronous
//! lock is the linearization point for server control, the first server-fatal
//! fact, and the immutable terminal result. Concurrent events are ordered by
//! their successful lock acquisition, which is a real production commit edge
//! rather than executor poll priority.
//!
//! Channels and task wakeups carry notification only. A public command applies
//! its event here and returns; the watch send that follows tells the supervisor
//! a decision it can no longer change. That is what makes an accepted
//! cancellation authoritative before anything observes it.

use std::sync::{Arc, Mutex, MutexGuard};

use tokio::time::Instant;

use super::mock::{LifecycleScript, ServerStopEdge};
use crate::RuntimeError;
use crate::lifecycle::AggregateShutdown;

/// What one owned server has committed about its own stopping.
///
/// Monotonic: a server leaves `Running` once, enters at most one forced phase,
/// and finishes once. The phase alone decides the flat result, so there is no
/// second ranking to disagree with it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StopPhase {
    /// New work may still be admitted.
    Running,
    /// Admission has closed and owned work may drain.
    Graceful,
    /// Public cancellation, or an armed owner's `Drop`, forced termination.
    Cancelled,
    /// The one aggregate deadline expired during the drain.
    TimedOut,
    /// The flat server result is committed and immutable.
    Finished,
}

/// How a finished server ended, without moving the error that says why.
///
/// The committed [`RuntimeError`] leaves through [`ServerStopState::settle`]
/// exactly once, so nothing can read it twice. This closed vocabulary is what
/// the supervisor publishing this server's disposition, and any observer of the
/// same commit, read instead.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum StopOutcome {
    /// No settlement has committed a result yet.
    Pending,
    /// The drain finished with no fatal fact.
    Completed,
    /// The drain finished carrying the first committed fatal fact.
    Failed,
    /// An accepted cancellation committed before terminal commitment.
    Cancelled,
    /// The aggregate deadline committed before terminal commitment.
    TimedOut,
}

/// One fact an owner is authorized to submit to the stop state.
pub(super) enum StopEvent {
    /// Public `shutdown`, runtime shutdown, or a signal watcher, at the instant
    /// the request was made.
    Graceful(Instant),
    /// Public `cancel`: a caller asked for forced termination now.
    Cancel,
    /// An armed owner went away without asking for anything.
    ///
    /// The same forced phase as [`Self::Cancel`], and deliberately not the same
    /// origin: only a caller that asked gives up the aggregate's remaining time.
    Abandon,
    /// A server-fatal fact one owner reported.
    Fatal(RuntimeError),
    /// The one aggregate deadline expired.
    DeadlineExpiry,
    /// The listener and every owned child settled.
    Settled,
}

/// What committing one event changed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct StopTransition {
    /// The phase this event left committed.
    pub(super) phase: StopPhase,
    /// Whether this event moved the committed phase.
    ///
    /// A compatible repeat leaves this false, which is what makes repeating a
    /// command idempotent for every owner reading the transition.
    pub(super) changed: bool,
}

/// The fields one stop state commits under its lock.
struct CommittedStop {
    phase: StopPhase,
    /// The instant the graceful phase committed, which the drain deadline reads.
    ///
    /// Held rather than re-read, so the drain bound derives from the commit
    /// rather than from however long an owner took to observe it.
    graceful_at: Option<Instant>,
    /// The one aggregate expiry the graceful commit fixed.
    ///
    /// Written by the mint itself and never again, so a later escalation that
    /// minted a second deadline, or restarted the first, would move it. That is
    /// the only thing separating an escalation the aggregate still bounds from
    /// one that gave itself a fresh grace.
    aggregate_expiry: Option<Instant>,
    /// The instant the forced phase committed, read under this same lock.
    ///
    /// Taken where the phase moves rather than where a caller asked, so an
    /// owner deriving a bound from it derives it from the commit and not from
    /// how long its own observation took to arrive.
    forced_at: Option<Instant>,
    /// The first server-fatal fact, which becomes the flat result unless a
    /// forced phase commits before settlement.
    fatal: Option<RuntimeError>,
    /// Whether a server-fatal fact was ever recorded here.
    ///
    /// Sticky, and deliberately not derived from `fatal` or from the outcome.
    /// Settlement takes the fact and a forced phase discards it, so a reading
    /// that re-derived this would answer true before settlement and false after
    /// it — an observer watching a monotonic commit would see a recorded fact
    /// disappear.
    fatal_recorded: bool,
    /// The immutable flat result, written once by settlement and taken once by
    /// the supervisor that reports it.
    result: Option<Result<(), RuntimeError>>,
    outcome: StopOutcome,
    /// Whether a public cancellation command, rather than an abandoned owner,
    /// committed the forced phase.
    ///
    /// A caller that asked for termination now has given up the aggregate's
    /// remaining time; an owner that merely went away has not.
    commanded: bool,
    /// How many events moved the committed phase.
    commits: u64,
    /// How many events were applied, including compatible repeats and no-ops.
    applied: u64,
    /// Fatal facts that arrived after the first one, kept as a count because
    /// they are structured diagnostics rather than a better account of the end.
    later_fatal_facts: u64,
}

/// The causal stop state one owned server shares with its handle, supervisor,
/// and connection owners.
pub(super) struct ServerStopState {
    committed: Mutex<CommittedStop>,
    /// The one aggregate expiry a graceful commit mints from.
    shutdown: Arc<AggregateShutdown>,
    /// Where commits are published, and where the two narrow commit edges pause,
    /// for a test that registered an observer. `None` in every other run.
    script: Option<Arc<LifecycleScript>>,
}

impl ServerStopState {
    pub(super) fn new(
        shutdown: Arc<AggregateShutdown>,
        script: Option<Arc<LifecycleScript>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            committed: Mutex::new(CommittedStop {
                phase: StopPhase::Running,
                graceful_at: None,
                aggregate_expiry: None,
                forced_at: None,
                fatal: None,
                fatal_recorded: false,
                result: None,
                outcome: StopOutcome::Pending,
                commanded: false,
                commits: 0,
                applied: 0,
                later_fatal_facts: 0,
            }),
            shutdown,
            script,
        })
    }

    /// Commit one event, publish the commit, and answer what it changed.
    ///
    /// Synchronous by design: this is what a public command calls before it
    /// returns, so the decision exists before any caller or supervisor can read
    /// a notification about it.
    pub(super) fn apply(&self, event: StopEvent) -> StopTransition {
        let mut committed = self.lock();
        let transition = self.apply_locked(&mut committed, event);
        LifecycleScript::observe_server_stop(self.script.as_deref(), committed.reading());
        transition
    }

    /// Commit one event into a state a caller already holds the lock on.
    ///
    /// Split out so a caller that has more than committing to do under one
    /// acquisition — settlement, which commits and then takes the result it
    /// fixed — does all of it inside the single linearization point rather than
    /// releasing the lock between the two halves.
    fn apply_locked(&self, committed: &mut CommittedStop, event: StopEvent) -> StopTransition {
        committed.applied += 1;
        let before = committed.phase;
        match event {
            StopEvent::Graceful(at) => self.commit_graceful(committed, at),
            StopEvent::Cancel => Self::commit_forced(committed, true),
            StopEvent::Abandon => Self::commit_forced(committed, false),
            StopEvent::Fatal(error) => self.commit_fatal(committed, error),
            StopEvent::DeadlineExpiry => Self::commit_timeout(committed),
            StopEvent::Settled => Self::commit_settlement(committed),
        }
        let changed = committed.phase != before;
        if changed {
            committed.commits += 1;
        }
        StopTransition {
            phase: committed.phase,
            changed,
        }
    }

    /// Commit one supervisor-side event between the two narrow pause edges.
    ///
    /// The pauses are outside the lock because they are where a case holds one
    /// owner while another owner commits. A controller can stop this owner
    /// before or after its commit; it cannot submit a fact or choose a result.
    pub(super) async fn commit(&self, event: StopEvent) -> StopTransition {
        LifecycleScript::pause_at_stop(self.script.as_deref(), ServerStopEdge::BeforeCommit).await;
        let transition = self.apply(event);
        LifecycleScript::pause_at_stop(self.script.as_deref(), ServerStopEdge::AfterCommit).await;
        transition
    }

    /// The phase committed right now.
    pub(super) fn phase(&self) -> StopPhase {
        self.lock().phase
    }

    /// The instant the graceful phase committed, if it has.
    pub(super) fn graceful_at(&self) -> Option<Instant> {
        self.lock().graceful_at
    }

    /// Whether a caller asked for forced termination, rather than an owner
    /// having gone away.
    pub(super) fn cancel_commanded(&self) -> bool {
        self.lock().commanded
    }

    /// How this server ended, in the closed vocabulary settlement fixed.
    ///
    /// The committed answer rather than a re-derivation: an owner publishing
    /// what happened to this server reads the outcome the settlement wrote,
    /// so it cannot disagree with the flat result that was fixed beside it.
    pub(super) fn outcome(&self) -> StopOutcome {
        self.lock().outcome
    }

    /// Commit settlement and take the immutable flat result.
    ///
    /// `None` while admission is still open: a settlement observed from
    /// `Running` says nothing has been asked of this server yet, so there is no
    /// result to fix. Every caller reaches this from a committed stop phase, and
    /// the drain predicates that lead here exclude both `Running` and `Finished`.
    ///
    /// One acquisition covers the commit and the hand-off, because a second
    /// settler landing between them would take the result and leave this caller
    /// reporting a cancelled or failed server as a clean one.
    pub(super) fn settle(&self) -> Option<Result<(), RuntimeError>> {
        let mut committed = self.lock();
        self.apply_locked(&mut committed, StopEvent::Settled);
        let result = committed.result.take();
        LifecycleScript::observe_server_stop(self.script.as_deref(), committed.reading());
        result
    }

    /// Read the current commit under the same lock every event commits under.
    pub(super) fn read<T>(&self, reader: impl FnOnce(&CommittedStopReading) -> T) -> T {
        reader(&self.lock().reading())
    }

    fn lock(&self) -> MutexGuard<'_, CommittedStop> {
        self.committed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// Close admission and start the one aggregate clock, from `Running` alone.
    fn commit_graceful(&self, committed: &mut CommittedStop, at: Instant) {
        match committed.phase {
            StopPhase::Running => {
                committed.phase = StopPhase::Graceful;
                committed.graceful_at = Some(at);
                committed.aggregate_expiry = Some(self.shutdown.mint_at(at));
            }
            StopPhase::Graceful
            | StopPhase::Cancelled
            | StopPhase::TimedOut
            | StopPhase::Finished => {}
        }
    }

    /// Enter forced cancellation, the one permitted escalation.
    ///
    /// A public command that follows an abandoned owner still records that a
    /// caller asked: the origin decides how much of the aggregate the forced
    /// wait may keep, and a caller asking for now has given all of it up.
    fn commit_forced(committed: &mut CommittedStop, commanded: bool) {
        committed.commanded |= commanded;
        match committed.phase {
            StopPhase::Running | StopPhase::Graceful => {
                committed.phase = StopPhase::Cancelled;
                committed.forced_at = Some(Instant::now());
            }
            StopPhase::Cancelled | StopPhase::TimedOut | StopPhase::Finished => {}
        }
    }

    /// Record the first server-fatal fact, and start the drain it explains.
    ///
    /// Later facts are counted rather than stored. The failure that started the
    /// drain is the one that explains it, and a forced phase committed after it
    /// replaces the result without erasing that the fact was recorded.
    fn commit_fatal(&self, committed: &mut CommittedStop, error: RuntimeError) {
        committed.fatal_recorded = true;
        match (committed.phase, committed.fatal.is_some()) {
            (StopPhase::Running, false) => {
                committed.fatal = Some(error);
                self.commit_graceful(committed, Instant::now());
            }
            (StopPhase::Running, true) => {
                committed.later_fatal_facts += 1;
                self.commit_graceful(committed, Instant::now());
            }
            (StopPhase::Graceful, false) => committed.fatal = Some(error),
            (StopPhase::Graceful, true)
            | (StopPhase::Cancelled | StopPhase::TimedOut | StopPhase::Finished, _) => {
                committed.later_fatal_facts += 1;
            }
        }
    }

    /// Answer the one aggregate deadline. Only a drain can expire.
    fn commit_timeout(committed: &mut CommittedStop) {
        match committed.phase {
            StopPhase::Graceful => committed.phase = StopPhase::TimedOut,
            StopPhase::Running
            | StopPhase::Cancelled
            | StopPhase::TimedOut
            | StopPhase::Finished => {}
        }
    }

    /// Fix the flat result from the phase that was committed before settlement.
    ///
    /// Admission still open is the one settlement that fixes nothing: the server
    /// has been asked for nothing, so there is no result to make immutable. A
    /// server already finished keeps the result it committed.
    ///
    /// A forced phase replaces the result the first fatal fact would have given.
    /// The fact is displaced rather than erased: it is counted beside every
    /// later fact and named where it is displaced, so the accept-loop or
    /// supervisor failure that started the drain still reaches an operator.
    fn commit_settlement(committed: &mut CommittedStop) {
        let (result, outcome) = match committed.phase {
            StopPhase::Running | StopPhase::Finished => return,
            StopPhase::Cancelled => {
                Self::displace_fatal(committed, "cancelled");
                (Err(RuntimeError::Cancelled), StopOutcome::Cancelled)
            }
            StopPhase::TimedOut => {
                Self::displace_fatal(committed, "deadline-expired");
                (Err(RuntimeError::Timeout), StopOutcome::TimedOut)
            }
            StopPhase::Graceful => Self::drained_result(committed.fatal.take()),
        };
        committed.phase = StopPhase::Finished;
        committed.result = Some(result);
        committed.outcome = outcome;
    }

    /// What a drain that reached settlement with no forced phase over it ends as.
    fn drained_result(fatal: Option<RuntimeError>) -> (Result<(), RuntimeError>, StopOutcome) {
        match fatal {
            Some(error) => (Err(error), StopOutcome::Failed),
            None => (Ok(()), StopOutcome::Completed),
        }
    }

    /// Account for the fatal fact a forced phase took the result away from.
    ///
    /// The fact is real and stays counted; only its claim on the flat result is
    /// lost. Naming it here is the one place an operator learns what the forced
    /// stop replaced, because the value itself is dropped with this call.
    fn displace_fatal(committed: &mut CommittedStop, phase: &'static str) {
        let Some(error) = committed.fatal.take() else {
            return;
        };
        committed.later_fatal_facts += 1;
        tracing::warn!(%error, phase, "server-fatal fact displaced by a forced stop");
    }
}

/// The read-only account one owner takes of a stop state.
///
/// Every value is written by the production commit it names. Nothing here
/// submits an event, chooses a phase, mints a deadline, or fixes a result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct CommittedStopReading {
    pub(super) phase: StopPhase,
    pub(super) outcome: StopOutcome,
    /// The one aggregate expiry this server's graceful commit fixed.
    pub(super) aggregate_expiry: Option<Instant>,
    /// The instant this server's forced phase committed, if one has.
    pub(super) forced_at: Option<Instant>,
    pub(super) commanded: bool,
    pub(super) commits: u64,
    pub(super) applied: u64,
    pub(super) later_fatal_facts: u64,
    pub(super) fatal_recorded: bool,
}

/// A server nothing has asked anything of yet.
impl Default for CommittedStopReading {
    fn default() -> Self {
        Self {
            phase: StopPhase::Running,
            outcome: StopOutcome::Pending,
            aggregate_expiry: None,
            forced_at: None,
            commanded: false,
            commits: 0,
            applied: 0,
            later_fatal_facts: 0,
            fatal_recorded: false,
        }
    }
}

impl CommittedStop {
    /// This commit, as an observer reads it.
    fn reading(&self) -> CommittedStopReading {
        CommittedStopReading {
            phase: self.phase,
            outcome: self.outcome,
            aggregate_expiry: self.aggregate_expiry,
            forced_at: self.forced_at,
            commanded: self.commanded,
            commits: self.commits,
            applied: self.applied,
            later_fatal_facts: self.later_fatal_facts,
            fatal_recorded: self.fatal_recorded,
        }
    }
}
