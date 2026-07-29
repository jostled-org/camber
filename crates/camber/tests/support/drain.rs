use camber::runtime::{self, RuntimeBuilder};
use camber::runtime_test_support::{RuntimeCheckpoint, RuntimeController, runtime_schedule};
use camber::{Resource, RuntimeError};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::http::{POLL_INTERVAL, poll_until, remaining};

/// Worker threads every scheduled runtime-scope case runs on. Enough that a
/// blocked observer cannot starve the child it is observing.
const WORKER_THREADS: usize = 4;

/// Bounds every rendezvous a runtime-scope test waits on.
///
/// Deliberately generous: it is a hang guard, not a timing assertion. A wait
/// that reaches it reports failure instead of parking the test binary.
pub const BOUND: Duration = Duration::from_secs(10);

/// A drain escalation boundary short enough that a wedged child reaches it
/// well inside the test's own deadline.
pub const SHORT_DRAIN: Duration = Duration::from_millis(500);

/// Longer than any test runs, so only an explicit trigger or a lifecycle
/// signal can end a loop under test.
pub const PERPETUAL: Duration = Duration::from_secs(3600);

/// Drive one future to completion on a throwaway current-thread Tokio runtime.
///
/// A Tokio runtime establishes no Camber context, so a test that must poll a
/// Camber handle outside `runtime::run` can do it here without minting the
/// context under proof.
pub fn block_on_detached<F: Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(future)
}

/// The inert lifecycle hook: the phase a [`RecordingResource`] owner does not
/// observe.
pub fn ignore_hook() -> Result<(), RuntimeError> {
    Ok(())
}

/// A registered resource whose two lifecycle hooks are supplied by its owner,
/// so one type covers every health-and-shutdown ordering probe.
pub struct RecordingResource<H, S> {
    name: &'static str,
    on_health: H,
    on_shutdown: S,
}

impl<H, S> RecordingResource<H, S>
where
    H: Fn() -> Result<(), RuntimeError> + Send + Sync + 'static,
    S: Fn() -> Result<(), RuntimeError> + Send + Sync + 'static,
{
    /// Register `name` with the hooks whose invocation the caller records.
    /// Pass [`ignore_hook`] for the phase this case does not observe.
    pub fn new(name: &'static str, on_health: H, on_shutdown: S) -> Self {
        Self {
            name,
            on_health,
            on_shutdown,
        }
    }
}

impl<H, S> Resource for RecordingResource<H, S>
where
    H: Fn() -> Result<(), RuntimeError> + Send + Sync + 'static,
    S: Fn() -> Result<(), RuntimeError> + Send + Sync + 'static,
{
    fn name(&self) -> &str {
        self.name
    }

    fn health_check(&self) -> Result<(), RuntimeError> {
        (self.on_health)()
    }

    fn shutdown(&self) -> Result<(), RuntimeError> {
        (self.on_shutdown)()
    }
}

/// How many entries the root scope retains, or `usize::MAX` when the seam
/// cannot be read.
///
/// Read without unwrapping, because most readings are taken inside a paused
/// window: a probe that panicked there would never release the pause and the
/// runtime would never return. The sentinel fails the caller's assertion
/// instead of hanging its test, and stating that here keeps every probe from
/// re-deriving it.
pub fn registry_len(controller: &RuntimeController) -> usize {
    controller.scope_registry_len().unwrap_or(usize::MAX)
}

/// Wait, bounded, for the root scope to retain no more than `remaining`
/// entries.
///
/// The scope releases a child's slot only after it has finished with it — a
/// panicking child records its fault first — so the registry falling to a known
/// length is an ordering edge, not a wall-clock budget. A registry that cannot
/// be read counts as not-yet-there, so an unreadable seam expires the bound
/// instead of passing on a missing answer.
pub fn wait_registry_at_most(
    controller: &RuntimeController,
    remaining: usize,
    bound: Duration,
) -> bool {
    poll_until(bound, || registry_len(controller) <= remaining)
}

/// The result a forcibly aborted child's handle delivers.
///
/// Production's documented forced-abort contract: an aborted task drops its
/// result sender without ever sending, and every delivery site reports that one
/// condition with this message. Spelled once here so a test asserting on it
/// cannot drift from the runtime that produces it.
///
/// Private, because [`assert_forced_abort`] is how a test asks the question: a
/// harness handed the message could assert on it in its own words and drift
/// from the check this module states.
const FORCED_ABORT_MESSAGE: &str = "task channel closed";

/// Assert `outcome` is the documented forced-abort result.
///
/// A wedged child that the drain had to abort is only proved aborted by what
/// its handle then delivers, so the check and the message it fails with belong
/// together rather than once per subsystem.
pub fn assert_forced_abort<T: std::fmt::Debug>(outcome: &Result<T, RuntimeError>) {
    assert!(
        matches!(outcome, Err(RuntimeError::TaskPanicked(message)) if &**message == FORCED_ABORT_MESSAGE),
        "the forced abort did not deliver the documented closed-channel result: {outcome:?}"
    );
}

/// The slot a runtime closure hands one child's handle out through.
///
/// A run that ends in an escalated drain displaces the closure's own value, so
/// a handle the test must still join afterwards cannot leave through the
/// return. Cloning the slot gives the closure its end; the test takes the
/// handle back once the run is over, and the diagnosis for a closure that never
/// filled it is written here rather than at each call.
pub struct WedgedHandle<T>(Arc<Mutex<Option<T>>>);

impl<T> WedgedHandle<T> {
    /// An empty slot, ready to be cloned into a runtime closure.
    pub fn new() -> Self {
        Self(Arc::new(Mutex::new(None)))
    }

    /// Hand the child's handle out of the closure.
    pub fn record(&self, handle: T) {
        *self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
    }

    /// Take the handle the closure recorded.
    pub fn take(&self) -> T {
        self.take_expecting("the closure never handed its wedged handle out")
    }

    /// Take the recorded value, naming what its absence would mean here.
    ///
    /// The slot carries join handles, ordering markers, and teardown timestamps,
    /// and an empty one means something different for each: a child that never
    /// started, a resource whose shutdown never ran, a closure that never
    /// reached teardown. A shared sentence would report all three the same way,
    /// so the caller supplies the one that localizes its own case.
    pub fn take_expecting(&self, missing: &str) -> T {
        match self
            .0
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            Some(value) => value,
            None => panic!("{missing}"),
        }
    }
}

impl<T> Default for WedgedHandle<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> Clone for WedgedHandle<T> {
    /// Share the slot, not the handle: `T` is a join handle and is never
    /// duplicated.
    fn clone(&self) -> Self {
        Self(Arc::clone(&self.0))
    }
}

/// Await one Camber child handle, bounded.
///
/// A handle that never resolves parks the test on it rather than failing it, so
/// every join a test performs outside the runtime's own drain goes through this
/// bound. The failure names the handle type that never came back, so a test
/// joining several children still reports which one.
pub async fn join_bounded<H>(handle: H, bound: Duration) -> H::Output
where
    H: std::future::IntoFuture,
{
    match tokio::time::timeout(bound, handle.into_future()).await {
        Ok(output) => output,
        Err(_) => panic!(
            "{} did not resolve within {bound:?}",
            std::any::type_name::<H>()
        ),
    }
}

/// Release the drain from `checkpoint` once it pauses there, reporting whether
/// it ever paused within `bound`.
///
/// `release` succeeds exactly when production is paused at the checkpoint, so
/// polling it bounds this wait: a drain that never reports the observation
/// fails the test rather than parking the observer on it.
///
/// Private on purpose. A release taken outside [`ReleaseOnDrop`] is a release
/// no unwind can complete, which is the one mistake that guard exists to
/// prevent; publishing it would export that mistake as API.
fn release_when_paused(
    controller: &RuntimeController,
    checkpoint: RuntimeCheckpoint,
    bound: Duration,
) -> bool {
    poll_until(bound, || controller.release(checkpoint).is_ok())
}

/// Wait for production to pause at `checkpoint`, reporting whether it did
/// within `bound`.
///
/// The sibling of [`release_when_paused`] for an observer that must probe the
/// paused window before releasing it: `wait_until_paused` blocks without a
/// deadline, so polling the read-only predicate is what makes a drain that
/// never reports the observation fail the test rather than park it.
pub fn wait_paused_bounded(
    controller: &RuntimeController,
    checkpoint: RuntimeCheckpoint,
    bound: Duration,
) -> bool {
    poll_until(bound, || controller.is_paused(checkpoint))
}

/// Leaves the checkpoint holding nothing, however the observation ended.
///
/// A checkpoint left armed or paused blocks production from ever returning, so
/// a probe that panics — or a wait that expired before the pause ever arrived —
/// would wedge the thread instead of failing the test. `Drop` disarms rather
/// than releases: `release` reports an error precisely when nothing is paused,
/// which is the expired observer's own state, so it cannot clean up after a
/// window that never opened. `disarm` is infallible and idempotent and covers
/// both — it drops an `Armed` checkpoint so production falls through it, and
/// releases a `Paused` one so the child held there resumes.
struct ReleaseOnDrop<'a> {
    controller: &'a RuntimeController,
    checkpoint: RuntimeCheckpoint,
    released: bool,
}

impl ReleaseOnDrop<'_> {
    /// Release on the ordinary path, reporting whether production was still
    /// paused there.
    ///
    /// The `Drop` arm is disarmed only by a release that actually happened: a
    /// failed release leaves a checkpoint still holding something, which is
    /// exactly what `Drop` is for.
    fn release(mut self, bound: Duration) -> bool {
        let released = release_when_paused(self.controller, self.checkpoint, bound);
        self.released = released;
        released
    }
}

impl Drop for ReleaseOnDrop<'_> {
    fn drop(&mut self) {
        match self.released {
            true => {}
            false => self.controller.disarm(),
        }
    }
}

/// Observe one paused checkpoint window: wait for production to reach
/// `checkpoint`, read the scope through `probe`, then release it.
///
/// Both legs share one deadline and the guard clears the checkpoint however
/// this ends, so neither a checkpoint that never arrives, one that arrives
/// late, nor a panicking probe can park the observer or strand production at
/// the pause — and the worst case is `bound` rather than a multiple of it.
/// `None` reports a window that was never observed, which fails the caller's
/// assertion instead of hanging its test.
///
/// A release that fails after a successful probe is a different failure and is
/// reported as one: the observation itself stands, and only the controller that
/// refused to let production go is at fault.
pub fn probe_paused_window<P, T>(
    controller: &RuntimeController,
    checkpoint: RuntimeCheckpoint,
    bound: Duration,
    probe: P,
) -> Option<T>
where
    P: FnOnce() -> T,
{
    let deadline = Instant::now() + bound;
    let guard = ReleaseOnDrop {
        controller,
        checkpoint,
        released: false,
    };
    // A window that never opened leaves the guard's `Drop` to disarm the
    // checkpoint. That is the case `release` cannot serve — nothing is paused
    // yet — and leaving it armed would park the next production run that
    // reaches it, with no observer left to let it go.
    match wait_paused_bounded(controller, checkpoint, remaining(deadline)) {
        false => None,
        true => {
            let observed = probe();
            assert!(
                guard.release(remaining(deadline)),
                "the paused window was read but production could not be released from it"
            );
            Some(observed)
        }
    }
}

/// The runtime closure's half of the armed handshake.
///
/// The seam holds one checkpoint at a time, so a closure that needs several
/// windows arms them one at a time, in the order it wants them observed.
pub struct ArmedGate<'a> {
    controller: &'a RuntimeController,
    armed: Sender<()>,
}

impl ArmedGate<'_> {
    /// Arm `checkpoint`, then tell the observer it may probe that window.
    pub fn arm(&self, checkpoint: RuntimeCheckpoint) {
        self.controller
            .pause_once(checkpoint)
            .expect("the runtime schedule refused to arm the next checkpoint");
        self.armed
            .send(())
            .expect("the observer thread stopped listening for the armed handshake");
    }

    /// The controller the runtime under observation is attached to.
    pub fn controller(&self) -> &RuntimeController {
        self.controller
    }
}

/// The observer thread's half of the armed handshake.
pub struct ArmedWatch<'a> {
    controller: &'a RuntimeController,
    armed: Receiver<()>,
}

impl ArmedWatch<'_> {
    /// Block until the closure arms its next checkpoint.
    ///
    /// Bounded: an observer that probed an unarmed checkpoint would poll a
    /// window production has already passed, so a handshake that never arrives
    /// fails the test here instead of failing an assertion later for the wrong
    /// reason.
    pub fn wait_armed(&self) {
        self.wait_armed_within(BOUND);
    }

    /// Block until the closure arms its next checkpoint, against `bound`.
    ///
    /// [`BOUND`] sizes one observation. A handshake that only arrives after the
    /// closure has run a whole test body is a different budget, and a caller
    /// with that shape says so here rather than widening the bound every other
    /// observation is measured against.
    pub fn wait_armed_within(&self, bound: Duration) {
        self.armed
            .recv_timeout(bound)
            .expect("the runtime closure never armed its next checkpoint");
    }

    /// Probe one armed window. See [`probe_paused_window`].
    pub fn probe<P, R>(&self, checkpoint: RuntimeCheckpoint, probe: P) -> Option<R>
    where
        P: FnOnce(&RuntimeController) -> R,
    {
        probe_paused_window(self.controller, checkpoint, BOUND, || {
            probe(self.controller)
        })
    }

    /// The controller the runtime under observation is attached to.
    pub fn controller(&self) -> &RuntimeController {
        self.controller
    }
}

/// Lowers the flag an abandoned observer polls, however the runtime's own call
/// ended.
///
/// `run` catches a closure's unwind, tears down, then resumes the payload, so a
/// store written after the call is skipped on that path. An observer still
/// spinning on the flag would then never return and the thread scope joining it
/// would hang, which is the bounded failure turning into a hung binary that the
/// spin exists to prevent. Lowering from `Drop` covers both exits.
struct LowerOnDrop<'a>(&'a AtomicBool);

impl Drop for LowerOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Keep the controller holding nothing until `run` is through, so an observer
/// that gave up cannot leave production parked at a checkpoint.
///
/// The observer is the only party that clears a checkpoint. One that unwound or
/// expired still owes production every pause it might reach on the way out, and
/// a single `disarm` cannot pay that: the closure may arm again afterwards.
/// Spinning until the run returns covers whatever it arms.
fn abandon_until_run_returns(controller: &RuntimeController, in_progress: &AtomicBool) {
    while in_progress.load(Ordering::Acquire) {
        controller.disarm();
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// What an observer that has given up calls to keep production moving.
///
/// Handed to the observer rather than left to it: an observer can give up
/// without panicking — a window that never opened is a reading it simply does
/// not have — and that path owes production exactly what the panicking path
/// owes it.
pub struct Abandon<'a> {
    controller: &'a RuntimeController,
    in_progress: &'a AtomicBool,
    on_abandon: &'a (dyn Fn() + Sync),
}

impl Abandon<'_> {
    /// Pay production everything the abandoning observer still owes it.
    fn run(&self) {
        (self.on_abandon)();
        abandon_until_run_returns(self.controller, self.in_progress);
    }
}

/// Run one scheduled runtime alongside an observer thread, joining both however
/// either ended.
///
/// The scaffold every scheduled-observation case shares. The observer is the
/// only party that clears a checkpoint, so one that failed while a window was
/// held would leave the closure parked and `run` would never return — a bounded
/// assertion failure turned into a hung binary. Catching the unwind here lets
/// the failed observer keep disarming until `run` is through, so the panic
/// surfaces at the join with its own message instead of never surfacing at all.
///
/// `on_abandon` is what a case's observer owes production beyond the disarm: a
/// run whose closure parks a holder child names the release of that holder here,
/// and a run with no such debt passes a no-op.
fn observe_while_running<O, R, T>(
    controller: &RuntimeController,
    on_abandon: &(dyn Fn() + Sync),
    observe: O,
    run: impl FnOnce() -> Result<T, RuntimeError>,
) -> (Result<T, RuntimeError>, R)
where
    O: FnOnce(&Abandon<'_>) -> R + Send,
    R: Send,
{
    let run_in_progress = AtomicBool::new(true);
    let (result, observed) = std::thread::scope(|scope| {
        let abandon = Abandon {
            controller,
            in_progress: &run_in_progress,
            on_abandon,
        };
        let observer = scope.spawn(move || {
            let observed =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| observe(&abandon)));
            if observed.is_err() {
                abandon.run();
            }
            observed
        });

        let lowered = LowerOnDrop(&run_in_progress);
        let result = run();
        drop(lowered);

        (result, observer.join().unwrap())
    });

    match observed {
        Ok(observed) => (result, observed),
        Err(payload) => std::panic::resume_unwind(payload),
    }
}

/// Run one scheduled runtime alongside an observer thread that probes the
/// windows its closure arms.
///
/// Owns the whole scaffold a scope-scheduling case needs: the controller, the
/// armed handshake that keeps the observer off an unarmed checkpoint, the
/// thread scope that joins the observer whatever the runtime returned, and the
/// builder the schedule attaches to. `configure` adds only what the case
/// itself varies.
pub fn observe_armed_sequence<C, B, O, T, R>(
    configure: C,
    body: B,
    observe: O,
) -> (Result<T, RuntimeError>, R)
where
    C: FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    B: FnOnce(&ArmedGate<'_>) -> T,
    O: FnOnce(&ArmedWatch<'_>) -> R + Send,
    R: Send,
{
    let controller = runtime_schedule();
    let (armed_tx, armed_rx) = channel::<()>();
    let builder = configure(runtime::builder().worker_threads(WORKER_THREADS))
        .with_test_schedule(&controller);

    let watch = ArmedWatch {
        controller: &controller,
        armed: armed_rx,
    };
    let gate = ArmedGate {
        controller: &controller,
        armed: armed_tx,
    };
    // Nothing beyond the disarm is owed here: this observer holds no child of
    // its own, so a checkpoint left holding nothing is the whole debt.
    observe_while_running(
        &controller,
        &|| {},
        // `move`: the watch owns the handshake receiver, which is `Send` but not
        // `Sync`, so the observer thread must take it rather than borrow it.
        move |_abandon| observe(&watch),
        || builder.run(|| body(&gate)),
    )
}

/// Run one scheduled runtime whose closure arms exactly one checkpoint behind
/// its body, and probe that single window.
///
/// The common shape of [`observe_armed_sequence`]. `None` reports a window
/// that was never observed, which fails the caller's assertion instead of
/// hanging its test.
pub fn observe_armed_window<C, B, P, T, R>(
    configure: C,
    checkpoint: RuntimeCheckpoint,
    body: B,
    probe: P,
) -> (Result<T, RuntimeError>, Option<R>)
where
    C: FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    B: FnOnce(&RuntimeController) -> T,
    P: FnOnce(&RuntimeController) -> R + Send,
    R: Send,
{
    observe_armed_sequence(
        configure,
        |gate| {
            let value = body(gate.controller());
            gate.arm(checkpoint);
            value
        },
        |watch| {
            watch.wait_armed();
            watch.probe(checkpoint, probe)
        },
    )
}

/// Root-scope children the runtime admits for itself, which every exact
/// occupancy count must account for.
///
/// On Unix that is the OS signal watcher; elsewhere `signals` does not compile
/// and the runtime admits none.
#[cfg(unix)]
pub const RUNTIME_OWNED_CHILDREN: usize = 1;
/// Root-scope children the runtime admits for itself. See the Unix variant.
#[cfg(not(unix))]
pub const RUNTIME_OWNED_CHILDREN: usize = 0;

/// The drain escalation boundary every [`prove_scope_owned`] run is built on.
///
/// Proving the cooperative `ScopeClosing` exit means returning from the closure
/// WITHOUT requesting shutdown, so this is what a child that never observes
/// closing runs into: the runtime force-aborts the retained handles and returns
/// `ScopeDrainTimeout`. Short enough that the escalation lands well inside the
/// observer's own bound, and the failure it produces is what
/// [`ScopeOwnedProof::assert_owned`] reports under the subsystem's name.
const DRAIN_ESCALATION: Duration = Duration::from_secs(1);

/// What one [`prove_scope_owned`] run observed about the root scope.
pub struct ScopeOwnedProof {
    /// Registry entries while the subject was running.
    pub occupants: usize,
    /// Registry entries the drain saw once only the holder remained, or
    /// `None` when the drain never paused at that window.
    pub entries_at_drain: Option<usize>,
    /// What the run returned. `Err` means the drain did not finish
    /// cooperatively: a child was still retained at [`DRAIN_ESCALATION`] and
    /// had to be force-aborted.
    pub drained: Result<(), RuntimeError>,
}

impl ScopeOwnedProof {
    /// Assert `subsystem` was one of `expected_occupants` root-scope children
    /// while it ran, exited on `ScopeClosing` alone, and left no registry
    /// entry behind.
    pub fn assert_owned(&self, subsystem: &str, expected_occupants: usize) {
        match &self.drained {
            Ok(()) => {}
            Err(error) => {
                panic!("{subsystem} did not exit on ScopeClosing: the drain escalated ({error})")
            }
        }
        assert_eq!(
            self.occupants, expected_occupants,
            "{subsystem} was not a root-scope child while it ran"
        );
        match self.entries_at_drain {
            None => panic!("{subsystem}: the drain never paused at its holder-only window"),
            Some(entries) => assert_eq!(
                entries, 1,
                "{subsystem} was still registered when only the holder should remain"
            ),
        }
    }
}

/// Run one runtime whose closure admits Camber-owned loops and then returns
/// WITHOUT requesting shutdown, so only `ScopeClosing` can end them.
///
/// A holder child pins the drain at a count of one, which makes the registry
/// reading at that pause exact: everything else must already have exited and
/// been removed. `body` runs inside the closure with the controller the
/// runtime is attached to, and its value is returned alongside the proof so a
/// caller can assert on whatever its own subject reported.
///
/// The closure reports its readings through a channel rather than through the
/// run's own value, so an escalated drain still reaches the caller as a named
/// failure instead of an unwrapped `Result` panicking here.
pub fn prove_scope_owned<C, B, T>(bound: Duration, configure: C, body: B) -> (ScopeOwnedProof, T)
where
    C: FnOnce(RuntimeBuilder) -> RuntimeBuilder,
    B: FnOnce(Arc<RuntimeController>) -> T,
{
    let controller = Arc::new(runtime_schedule());
    let holder_only = RuntimeCheckpoint::ScopeWaitObserved(1);
    let observer_controller = Arc::clone(&controller);
    let closure_controller = Arc::clone(&controller);
    let body_controller = Arc::clone(&controller);

    let hold = Arc::new(tokio::sync::Notify::new());
    let observer_hold = Arc::clone(&hold);
    let abandon_hold = Arc::clone(&hold);
    let closure_hold = Arc::clone(&hold);

    let (armed_tx, armed_rx) = channel::<()>();
    let (reported_tx, reported_rx) = channel::<(usize, T)>();

    let builder = configure(
        runtime::builder()
            .worker_threads(WORKER_THREADS)
            .shutdown_timeout(DRAIN_ESCALATION),
    )
    .with_test_schedule(&controller);

    // The holder is freed by this observer alone, so an observer that gives up
    // — by panicking, or by never seeing the window — owes production that
    // release on top of the disarm. Without it the drain waits on a child
    // nothing frees and `run` never returns, which is the hang a bounded wait
    // exists to rule out. Freeing it escalates the drain instead, and the
    // missing reading fails `assert_owned`.
    let (drained, entries_at_drain) = observe_while_running(
        &controller,
        &move || abandon_hold.notify_one(),
        // `move`: the handshake receiver is `Send` but not `Sync`, so the
        // observer thread must take it rather than borrow it.
        move |abandon| {
            // One deadline over both legs, so a handshake that arrives late
            // cannot buy the probe a second full budget.
            let deadline = Instant::now() + bound;
            let entries = match armed_rx.recv_timeout(remaining(deadline)) {
                Err(_) => None,
                Ok(()) => probe_paused_window(
                    &observer_controller,
                    holder_only,
                    remaining(deadline),
                    || {
                        let entries = registry_len(&observer_controller);
                        // Frees the holder, so releasing this window drains the
                        // scope.
                        observer_hold.notify_one();
                        entries
                    },
                ),
            };
            match entries {
                Some(_) => {}
                None => abandon.run(),
            }
            entries
        },
        || {
            builder.run(move || {
                camber::spawn_async(async move { closure_hold.notified().await });

                let value = body(body_controller);

                let occupants = closure_controller
                    .scope_registry_len()
                    .expect("the runtime schedule could not read the root scope registry");
                reported_tx
                    .send((occupants, value))
                    .expect("the caller stopped listening for the closure's occupancy");
                closure_controller
                    .pause_once(holder_only)
                    .expect("the runtime schedule refused to arm the holder-only window");
                armed_tx
                    .send(())
                    .expect("the observer thread stopped listening for the armed handshake");
            })
        },
    );

    let (occupants, value) = match reported_rx.recv_timeout(bound) {
        Ok(reported) => reported,
        Err(_) => panic!("the runtime closure never reported its occupancy: {drained:?}"),
    };
    (
        ScopeOwnedProof {
            occupants,
            entries_at_drain,
            drained,
        },
        value,
    )
}
