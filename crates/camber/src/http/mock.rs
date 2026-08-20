use super::Method;
use super::Response;
use super::boundary::CrossedBound;
use super::completion::CompletionTerminal;
use super::method::RequestMethod;
pub use super::operation::{InboundTerminal, OperationStage};
use super::rejection::RejectionProtocol;
use super::response::HeaderPair;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::RuntimeError;

/// Every name a completion label may carry, as production spells them.
///
/// A test seam, not API, on the same footing as [`LifecycleCheckpoint`]. It is
/// published from the closed vocabularies themselves rather than transcribed
/// beside them: a case that checked a scraped label against a hand-written list
/// would go on passing after production gained a name that list has never
/// heard of.
///
/// Read-only. Nothing here selects a terminal, crosses a bound, or records
/// anything.
#[doc(hidden)]
pub struct CompletionVocabulary {
    /// Every method name a completion can be recorded under.
    pub methods: Box<[&'static str]>,
    /// Every dispatch class a completion can be recorded under.
    pub protocols: Box<[&'static str]>,
    /// Every terminal class one completed operation can be recorded under.
    pub terminals: Box<[&'static str]>,
    /// Every configured bound a completion can name, including stated absence.
    pub boundaries: Box<[&'static str]>,
}

/// The closed vocabulary production records completions under.
#[doc(hidden)]
pub fn completion_vocabulary() -> CompletionVocabulary {
    CompletionVocabulary {
        methods: RequestMethod::vocabulary(),
        protocols: RejectionProtocol::vocabulary(),
        terminals: CompletionTerminal::vocabulary(),
        boundaries: CrossedBound::vocabulary(),
    }
}

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
    AfterOwnedConnectionFutureCompleted,
    AfterSupervisorResultSend,
    AfterUpgradeTicketSubmitted,
    UpgradePeerClosed,
    ConnectionPermitWaitPending,
    BeforeRuntimeWait,
    BeforeUpgradeAcknowledge,
    HeaderTimeoutConfigured(std::time::Duration),
    /// The budgets one admitted head resolved to, after every containing layer
    /// narrowed them. Read from the routing owner that resolves them, at the
    /// instant it does.
    RouteBudgetsResolved {
        request: super::RequestBudget,
        upload: super::TransferBudget,
        download: super::TransferBudget,
    },
    /// One transfer owner is about to read its source.
    ///
    /// Held here, a case makes every source it wants weighed ready before the
    /// turn that reads them begins, including the frame the source is holding.
    BeforeTransferSourcePoll,
    /// One transfer owner has read its source and is about to weigh a whole
    /// scheduling turn's ready sources against the declared precedence.
    BeforeTransferTerminalSelection,
    /// One admitted request's inbound coordinator is about to weigh a whole
    /// scheduling turn's ready sources against the declared precedence.
    ///
    /// Held here, a case stages every source it wants weighed together and the
    /// selection that follows sees all of them in the one turn.
    BeforeInboundTerminalSelection,
    /// The terminal one admitted request's inbound coordinator selected.
    InboundTerminalSelected(InboundTerminal),
    RequestBodyLimitConfigured(usize),
    RequestBodyLimitObserved,
    StreamingUpstreamHeadReady,
    StreamingUploadQuiesced,
    BeforeStreamingResponseCommit,
    /// Tonic has produced a response head Camber has not committed.
    ///
    /// Held here, a case makes a local upload or operation cause ready in the
    /// same scheduling turn as the head, which is the tie the declared
    /// precedence decides. The streaming proxy's counterpart is
    /// [`Self::StreamingUpstreamHeadReady`].
    #[cfg(feature = "grpc")]
    GrpcHeadReady,
    /// Camber committed tonic's response head and now owns only the two bodies
    /// around it.
    ///
    /// The post-handoff boundary: past it no Camber cause reaches a rejection
    /// mapper, tonic owns status and trailers, and each direction ends its own
    /// body.
    #[cfg(feature = "grpc")]
    GrpcHandoffCommitted,
    SseBufferConfigured(usize),
    WebSocketOutgoingBufferConfigured(usize),
    WebSocketIncomingBufferConfigured(usize),
    WebSocketBeforeOutboundWrite,
    WebSocketOutboundFrameBuilt,
    WebSocketInboundFrameArrived,
    WebSocketInboundFrameQueued,
    WebSocketBeforeTerminalSelection,
    WebSocketBeforeTerminalPoll,
    WebSocketTerminalSelected,
    WebSocketBeforePeerCloseAwait,
    MultipartCommandAccepted,
    MultipartIngressAdvanced,
    MultipartReplyPublished,
    MultipartHandlerCompleted,
    MultipartDriverTerminated,
    BeforeMultipartResponseSelection,
    /// One static-file worker has begun, before it has touched the filesystem.
    StaticFileWorkerEntered,
    /// One static-file worker has taken the filesystem's word for how large its
    /// file is, and has not opened it yet.
    StaticFileMetadataObserved,
    /// One profiling worker has begun, before it has sampled anything.
    #[cfg(feature = "profiling")]
    ProfilingWorkerEntered,
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
    released: Arc<ReleaseGate>,
}

impl CheckpointState {
    /// Record that production reached this checkpoint.
    ///
    /// Nothing is woken here. Reaching a checkpoint and being held at it are two
    /// moments, not one: the future this returns the gate to has not looked for
    /// its release yet, and [`ReleaseGate::poll_release`] owns the wake that says
    /// it has. See the gate's `looked` for what a case that was woken any earlier
    /// could do to the production poll it is standing in the middle of.
    fn pause(&mut self) -> Arc<ReleaseGate> {
        self.phase = CheckpointPhase::Paused;
        Arc::clone(&self.released)
    }
}

/// The release one paused checkpoint waits on.
///
/// The recorded release is re-read on every poll rather than only when a wake
/// arrives. Recording and waking are separate, because a case whose claim is
/// what one turn of a `select!` decides needs both of that turn's results ready
/// in the same poll: waking the future held here decides the turn before the
/// second result exists, so such a case records the release quietly and lets the
/// other result provoke the poll that observes both. A parked thread is the one
/// exception, for the reason [`Self::record`] states.
#[derive(Default)]
struct ReleaseGate {
    released: AtomicBool,
    /// How many times whatever waits here has looked for its release.
    ///
    /// One poll is one turn the held future took. A case that stages a release
    /// without waking anything reads this to tell a turn that has already been
    /// spent from one still to come.
    polls: AtomicUsize,
    waiting: Mutex<Option<std::task::Waker>>,
    /// Whether a blocking worker, rather than a future, waits here.
    ///
    /// Published before that worker's first look, so a release recorded at any
    /// point after it arrived finds it set. [`Self::record`] reads it to decide
    /// whether the release owes an unpark.
    parked_thread: AtomicBool,
    /// Woken when the held future takes its first look here.
    ///
    /// This, rather than the phase flip, is what an observer waits for. A case
    /// woken at the flip is standing inside the production poll that reached the
    /// checkpoint, and the two calls it makes next — arm the checkpoint it wants
    /// held, release the one it is holding — both land before that poll has
    /// looked here. The look then finds the release already recorded, the held
    /// future runs on within the same poll, and the checkpoint the case armed is
    /// never reached, because the poll that would have reached it is the one
    /// already running. Waiting for the first look puts the case's release on a
    /// poll that has not started.
    looked: tokio::sync::Notify,
}

impl ReleaseGate {
    /// Record the release, and unpark a thread waiting here.
    ///
    /// No task is woken, for the reason this type's own account gives. A parked
    /// thread is not that case: it decides no `select!` turn, and no poll is
    /// coming to it that something else could provoke, so a release recorded
    /// without the unpark would hold that worker until the controller closed.
    fn record(&self) {
        self.released.store(true, Ordering::Release);
        match self.parked_thread.load(Ordering::Acquire) {
            true => self.wake(),
            false => {}
        }
    }

    /// Whether the release has been recorded.
    ///
    /// A plain read, for a caller deciding whether it has anything to wait for.
    /// Waiting itself goes through [`Self::poll_release`], which answers the
    /// same question under the lock that makes the answer race-free.
    fn is_released(&self) -> bool {
        self.released.load(Ordering::Acquire)
    }

    /// How many turns whatever waits here has taken.
    fn polls(&self) -> usize {
        self.polls.load(Ordering::Acquire)
    }

    /// Whether the held future has looked here at least once.
    fn has_looked(&self) -> bool {
        self.polls() > 0
    }

    /// Wake whatever waits here.
    fn wake(&self) {
        let waiting = self
            .waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        match waiting {
            Some(waker) => waker.wake(),
            None => {}
        }
    }

    /// Hold until this gate's release has been recorded.
    async fn held(&self) {
        std::future::poll_fn(|cx| self.poll_release(cx)).await;
    }

    /// Hold this thread until this gate's release has been recorded.
    ///
    /// The blocking twin of [`Self::held`], for the one production owner that
    /// reaches a checkpoint from a blocking worker rather than from a poll.
    /// Deliberately the same gate and the same look: an observer waiting to
    /// hear the checkpoint was reached cannot tell a parked worker from a
    /// parked future, so [`LifecycleController::wait_until_paused`] needs no
    /// second spelling.
    ///
    /// The waker unparks this thread, so a release wakes the worker exactly the
    /// way it wakes a held future — nothing here spins. A staged release unparks
    /// it too, which [`Self::record`] accounts for.
    fn held_blocking(&self) {
        self.parked_thread.store(true, Ordering::Release);
        let waker = std::task::Waker::from(Arc::new(ThreadWaker(std::thread::current())));
        let cx = std::task::Context::from_waker(&waker);
        while self.poll_release(&cx).is_pending() {
            std::thread::park();
        }
    }

    /// Whether the release is recorded, registering for a wake when it is not.
    ///
    /// The registration happens under the same lock [`Self::wake`] takes, so a
    /// release recorded between the check and the registration still finds the
    /// waker it has to wake.
    ///
    /// The first look publishes itself whatever the answer was, because what an
    /// observer waits for is that the look happened and not what it found. It is
    /// published after the lock is given back, so the observer it frees — which
    /// may release this gate immediately — never contends with the call that
    /// freed it.
    fn poll_release(&self, cx: &std::task::Context<'_>) -> std::task::Poll<()> {
        let first = self.polls.fetch_add(1, Ordering::Release) == 0;
        let mut waiting = self
            .waiting
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let ready = match self.released.load(Ordering::Acquire) {
            true => std::task::Poll::Ready(()),
            false => {
                *waiting = Some(cx.waker().clone());
                std::task::Poll::Pending
            }
        };
        drop(waiting);
        if first {
            self.looked.notify_waiters();
        }
        ready
    }
}

/// The waker one parked worker is woken through.
///
/// A [`ReleaseGate`] records its release the same way whoever waits there is a
/// future or a thread; this is what turns that one recorded release into an
/// unpark instead of a task wake.
struct ThreadWaker(std::thread::Thread);

impl std::task::Wake for ThreadWaker {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.0.unpark();
    }
}

/// A test-only barrier checked before one production poll.
///
/// The barrier keeps looking until its checkpoint is armed. This lets a test
/// stage one nested future first, then stop the coordinator before another wake
/// lets it poll any terminal source. It can delay a poll but cannot choose its
/// result or change source readiness.
#[cfg(feature = "ws")]
pub(in crate::http) struct LifecyclePollGate<'a> {
    script: Option<&'a LifecycleScript>,
    checkpoint: LifecycleCheckpoint,
    release: Option<Arc<ReleaseGate>>,
    passed: bool,
}

#[cfg(feature = "ws")]
impl<'a> LifecyclePollGate<'a> {
    pub(in crate::http) const fn new(
        script: Option<&'a LifecycleScript>,
        checkpoint: LifecycleCheckpoint,
    ) -> Self {
        Self {
            script,
            checkpoint,
            release: None,
            passed: false,
        }
    }

    /// Hold the first poll that observes the armed checkpoint.
    pub(in crate::http) fn poll(&mut self, cx: &std::task::Context<'_>) -> std::task::Poll<()> {
        if self.passed {
            return std::task::Poll::Ready(());
        }
        if self.release.is_none() {
            self.release = self.script.and_then(|script| script.reach(self.checkpoint));
        }
        let ready = match self.release.as_ref() {
            Some(release) => release.poll_release(cx),
            None => std::task::Poll::Ready(()),
        };
        if ready.is_ready() && self.release.is_some() {
            self.passed = true;
        }
        ready
    }
}

struct ScriptState {
    closed: bool,
    checkpoints: Vec<CheckpointState>,
    fault: Option<LifecycleFault>,
}

/// What one listener's requests reported about their body handling.
///
/// Monotonic counters only. Nothing here chooses a limit, invokes a policy,
/// synthesizes a rejection, or takes ownership of anything: each value is
/// written by the production decision it names and read by the controller.
#[derive(Default)]
struct BodyObservations {
    frames_polled: AtomicUsize,
    peak_retained_bytes: AtomicUsize,
    permit_owners_dropped: AtomicUsize,
}

/// What the checked collector reported while reading this peer's answers.
///
/// The outbound counterpart of [`BodyObservations`], and the same two rules:
/// monotonic counters only, each written by the production collector's own
/// decision to poll a chunk or keep one. Nothing here chooses a maximum,
/// retains a byte, drops a frame, or selects a terminal.
#[derive(Default)]
struct CollectionObservations {
    /// Chunks the collector was handed, counted before it accounted for them,
    /// so a chunk refused for crossing the maximum is still counted as read.
    chunks_polled: AtomicUsize,
    /// The most any one collection from this peer retained at once.
    peak_retained_bytes: AtomicUsize,
    /// What the first chunk this scope ever retained left behind.
    ///
    /// The exact boundary of a source whose chunk sizes nobody declares: a case
    /// that wants a maximum landing exactly on one chunk reads it here rather
    /// than guessing a number the producer never promised. Zero means no chunk
    /// has been retained yet — a retained chunk always leaves bytes behind, and
    /// a chunk refused before anything was kept ends its collection.
    first_retained_bytes: AtomicUsize,
}

/// What one family of offloaded workers reported about its entries and its
/// maximum.
///
/// The three things every owner that answers on a blocking thread says the same
/// way, said once. Each owner's own record holds this as a field rather than
/// flattening it, so a family keeps whatever else only it can report while the
/// shared trio has one definition.
#[derive(Default)]
struct WorkerObservations {
    /// The ceiling the most recent answer from this family actually collects
    /// under, as its own collector compares it.
    ///
    /// The last value rather than a count, because that is the question a row
    /// asks: this answer, under this registration, froze this maximum. Zero is
    /// nothing answered yet — a frozen maximum is never zero — and
    /// [`usize::MAX`] is the explicit opt-out, which is what makes a defaulted
    /// spelling and an unbounded one two different observations rather than one.
    frozen_ceiling: AtomicUsize,
    /// Blocking workers that began.
    workers_entered: AtomicUsize,
    /// Blocking workers that handed back an answer or a refusal.
    ///
    /// The difference between this and [`Self::workers_entered`] is the whole
    /// abandonment claim: a caller that stopped waiting leaves a worker counted
    /// as entered and not yet returned, still holding what it owns.
    workers_returned: AtomicUsize,
}

/// What the static-file workers under one root reported about where they ran.
///
/// Counters and the maximum a read froze, each written by the production step
/// it names. Nothing here starts a worker, resolves a path, measures a file,
/// reads a byte, chooses a maximum, or decides which thread anything runs on.
#[derive(Default)]
struct StaticFileObservations {
    /// What the blocking workers under this root reported about themselves.
    worker: WorkerObservations,
    /// Path confinements that ran somewhere other than the awaiting thread.
    canonicalized_off_caller: AtomicUsize,
    /// Preflight measurements that ran somewhere other than the awaiting thread.
    metadata_off_caller: AtomicUsize,
    /// Checked reads that ran somewhere other than the awaiting thread.
    reads_off_caller: AtomicUsize,
    /// Filesystem steps of any kind that ran on the awaiting thread itself.
    steps_on_caller: AtomicUsize,
}

/// What the profiling workers in this process reported about where they ran and
/// what they retained.
///
/// Counters and the maximum one render froze, each written by the production owner
/// it names. Nothing here starts a worker, samples a stack, renders a byte,
/// chooses a maximum, or decides which thread anything runs on.
///
/// Held apart from [`StaticFileObservations`] rather than folded into it: the two
/// owners answer different questions — a file read reports three filesystem steps
/// under one served root, and a profile is one process-wide answer whose sampling
/// and rendering happen inside a single worker entry. One struct covering both
/// would carry a field that is meaningless for whichever owner wrote it. What the
/// two do share is [`WorkerObservations`], which both hold as a field.
#[cfg(feature = "profiling")]
#[derive(Default)]
struct ProfilingObservations {
    /// What this process's profiling workers reported about themselves.
    worker: WorkerObservations,
    /// Workers that began on the very thread awaiting them.
    ///
    /// Sampling and rendering both run inside one worker entry, so the thread
    /// that entry reports is the thread both of them ran on.
    entries_on_caller: AtomicUsize,
}

/// What one profiling worker published, at the moment it published it.
#[cfg(feature = "profiling")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::http) enum ProfilingEvent {
    /// The blocking worker began, on the awaiting thread or off it.
    Entered { off_caller: bool },
    /// This render froze the maximum it retains under.
    ///
    /// Carried as the collector's own comparison value, so [`usize::MAX`] is the
    /// explicit opt-out and every other value is a real maximum.
    CeilingFrozen(usize),
    /// The blocking worker handed back its answer or its refusal.
    Returned,
}

/// What this process's profiling workers have done so far.
///
/// Read-only, and every number in it is written by the production owner it names.
#[cfg(feature = "profiling")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProfilingObservation {
    pub frozen_ceiling: usize,
    pub workers_entered: usize,
    pub workers_returned: usize,
    pub entries_on_caller: usize,
}

/// The one filesystem step a static-file worker is reporting.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::http) enum StaticFileStep {
    /// Resolving the request to a real path inside the served root.
    Canonicalize,
    /// Taking the filesystem's word for how large that file is.
    Metadata,
    /// Reading that file under the collection's ceiling.
    Read,
}

/// What one static-file worker published, at the moment it published it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::http) enum StaticFileEvent {
    /// The blocking worker began, before it touched the filesystem.
    WorkerEntered,
    /// The blocking worker handed back its answer or its refusal.
    WorkerReturned,
    /// This read's collection froze the maximum it measures against.
    ///
    /// Carried as the collector's own comparison value, so [`usize::MAX`] is
    /// the explicit opt-out and every other value is a real ceiling.
    CeilingFrozen(usize),
    /// One filesystem step ran, on the awaiting thread or off it.
    Step {
        step: StaticFileStep,
        off_caller: bool,
    },
}

/// What one offloaded worker publishes, whichever owner it belongs to.
///
/// The vocabularies stay apart — a static-file worker reports filesystem steps a
/// profile has none of, and a profiling entry reports a placement a static-file
/// entry does not — so this carries only the line that puts one event onto the
/// script. It is what lets [`BlockingWorkerObserver`] state the entry-and-return
/// order once for owners that report different things at both ends.
pub(in crate::http) trait BlockingWorkerEvent: Sized {
    /// Publish one of these onto `script`, or nothing when none watches.
    fn publish(script: Option<&LifecycleScript>, event: Self);
}

#[cfg(feature = "profiling")]
impl BlockingWorkerEvent for ProfilingEvent {
    fn publish(script: Option<&LifecycleScript>, event: Self) {
        LifecycleScript::observe_profiling(script, event);
    }
}

impl BlockingWorkerEvent for StaticFileEvent {
    fn publish(script: Option<&LifecycleScript>, event: Self) {
        LifecycleScript::observe_static_file(script, event);
    }
}

/// The observer one offloaded worker reports through, and the thread it left.
///
/// Every owner that answers on a blocking thread holds one of these: the script
/// it publishes to, resolved once it is running where it may block, and the
/// identity of the thread awaiting it, so it can say it ran somewhere else. Held
/// as a field rather than restated as a pair of them, so an owner that offloads
/// its answer takes the whole protocol with the value — a resolve, an entry, a
/// hold, a return — instead of reassembling it.
pub(in crate::http) struct BlockingWorkerObserver {
    script: Option<Arc<LifecycleScript>>,
    caller: std::thread::ThreadId,
}

impl BlockingWorkerObserver {
    /// Take the awaiting thread's identity, before this worker owns anything.
    ///
    /// Built on the awaiting side and moved whole to the blocking thread, which
    /// is what makes the identity worth taking: the worker compares it against
    /// wherever it wakes up.
    pub(in crate::http) fn awaiting() -> Self {
        Self {
            script: None,
            caller: std::thread::current().id(),
        }
    }

    /// Take the script this worker reports to, once it is running where it may
    /// block.
    ///
    /// Resolved here rather than beside the request because this is where it is
    /// read: the awaiting worker never publishes anything through it, and a
    /// value it does not need is a value it should not lend.
    pub(in crate::http) fn resolve(&mut self, found: Option<Arc<LifecycleScript>>) {
        self.script = found;
    }

    /// The script this worker reports to, when one watches.
    fn script(&self) -> Option<&LifecycleScript> {
        self.script.as_deref()
    }

    /// A handle on that script for an owner that keeps reporting after this
    /// borrow ends.
    ///
    /// The checked collector each worker builds is the one that needs it: it
    /// publishes from wherever the bytes reach it, not from here.
    pub(in crate::http) fn shared(&self) -> Option<Arc<LifecycleScript>> {
        self.script.clone()
    }

    /// Whether this worker is running anywhere but the thread awaiting it.
    pub(in crate::http) fn ran_off_caller(&self) -> bool {
        std::thread::current().id() != self.caller
    }

    /// Publish one thing this worker did.
    pub(in crate::http) fn publish<E: BlockingWorkerEvent>(&self, event: E) {
        E::publish(self.script(), event);
    }

    /// Hold this worker at `checkpoint` for as long as a case holds it there.
    pub(in crate::http) fn hold_at(&self, checkpoint: LifecycleCheckpoint) {
        LifecycleScript::pause_blocking(self.script(), checkpoint);
    }

    /// Run `work` as this worker's whole answer, announced at both ends.
    ///
    /// The order is the protocol, and it is stated here once. The entry is
    /// published before the hold, so a case that parks this worker at
    /// `checkpoint` has already seen it begin. The return is published after the
    /// work, so the gap between the two counts is exactly what a caller that
    /// stopped waiting left behind. The two events are the owner's own, because
    /// what a static-file entry reports and what a profiling entry reports are
    /// not the same claim.
    pub(in crate::http) fn spanning<E: BlockingWorkerEvent, T>(
        &self,
        entered: E,
        returned: E,
        checkpoint: LifecycleCheckpoint,
        work: impl FnOnce() -> T,
    ) -> T {
        self.publish(entered);
        self.hold_at(checkpoint);
        let answered = work();
        self.publish(returned);
        answered
    }
}

/// What the static-file workers under one root have done so far.
///
/// Read-only, and every number in it is written by the production step it
/// names. Nothing here selects a ceiling, resolves a path, retains a byte, or
/// chooses the thread a step runs on.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct StaticFileObservation {
    /// The maximum the most recent read under this root collects under, as its
    /// collector compares it: `usize::MAX` is the explicit opt-out, and zero
    /// means no read has frozen one yet.
    pub frozen_ceiling: usize,
    pub workers_entered: usize,
    pub workers_returned: usize,
    pub canonicalized_off_caller: usize,
    pub metadata_off_caller: usize,
    pub reads_off_caller: usize,
    pub steps_on_caller: usize,
}

/// What one listener's admitted operations published about their envelopes.
///
/// Written by the production owner that mints an envelope and by each owner
/// that reads one, and by nothing else. Nothing here mints an identity,
/// selects a policy, or decides which owner runs.
#[derive(Default)]
struct OperationObservations {
    /// Envelopes this listener's admitted heads minted.
    admitted: AtomicUsize,
    /// The identity most recently read, from any owner.
    identity: AtomicU64,
    /// How many distinct identities every owner between them has read.
    ///
    /// The count, not the set: one identity read by four owners is what the
    /// carry claim needs, and a second envelope reaching any owner shows up
    /// here as two whichever owner saw it.
    distinct_identities: AtomicUsize,
    /// The request-total deadline the mint computed, as its offset from
    /// admission, in nanoseconds. `u64::MAX` is an unbounded total.
    total_from_admission: AtomicU64,
    /// How many distinct totals this listener's envelopes carried.
    distinct_totals: AtomicUsize,
    dispatch: AtomicUsize,
    middleware: AtomicUsize,
    body: AtomicUsize,
    response_head: AtomicUsize,
    /// Accounts this listener's exits staged, whether or not they were kept.
    ///
    /// Every offer, not every keeper: "one answer, one account" is a claim a
    /// case has to be able to read, and a second exit answering one request
    /// shows up here rather than as a silence the set-once slot swallowed.
    completions_staged: AtomicUsize,
    /// Accounts this listener's terminal owners actually recorded.
    completions_recorded: AtomicUsize,
}

/// What one streaming direction's owner published about itself.
///
/// Written by the production owner it names — the policy it froze, the frames it
/// polled, the bytes it admitted, the frame it released at a crossing, the one
/// terminal it fixed, and its own release — and read by the observing case.
/// Nothing here polls a source, admits a byte, selects a terminal, or releases
/// anything.
#[derive(Default)]
struct DirectionObservations {
    /// The byte maximum this direction froze. Zero is unbounded, which no
    /// validated maximum can be.
    max_bytes: AtomicUsize,
    /// The frozen quiet interval and lifetime, in nanoseconds. `u64::MAX` is
    /// unbounded.
    idle_nanos: AtomicU64,
    total_nanos: AtomicU64,
    /// Frames this owner polled out of its source, payload-carrying or not.
    frames_polled: AtomicUsize,
    /// The running total this direction has admitted.
    admitted_bytes: AtomicUsize,
    /// Frames released rather than delivered: the one that crossed the maximum,
    /// and the one in hand when a terminal was fixed.
    crossings_released: AtomicUsize,
    /// Set-once, exactly as the owner's own terminal is.
    terminal: OnceLock<InboundTerminal>,
    /// How many terminals reached this record, including any the set-once above
    /// kept out.
    terminals: AtomicUsize,
    /// How many owners of this direction reached their drop.
    releases: AtomicUsize,
}

/// One armed checkpoint an owner inside a poll is held at.
///
/// The gate itself, wrapped so that reaching a checkpoint and holding at it stay
/// the two moments they already are for every awaiting owner: the hold is taken
/// synchronously, and the poll that took it decides when to look for the
/// release.
pub(in crate::http) struct CheckpointHold(Arc<ReleaseGate>);

impl CheckpointHold {
    /// Whether this hold has been released, registering interest if it has not.
    pub(in crate::http) fn poll_released(
        &self,
        cx: &std::task::Context<'_>,
    ) -> std::task::Poll<()> {
        self.0.poll_release(cx)
    }
}

impl DirectionObservations {
    /// Apply one event this direction's owner published.
    fn apply(&self, event: TransferEvent) {
        let counter = match event {
            TransferEvent::PolicyFrozen(budget) => {
                self.max_bytes
                    .store(budget.max_bytes().unwrap_or(0), Ordering::Release);
                self.idle_nanos
                    .store(nanos_of(budget.idle()), Ordering::Release);
                self.total_nanos
                    .store(nanos_of(budget.total()), Ordering::Release);
                return;
            }
            // A running total, not a tally: the claim is how much this direction
            // admitted, and adding each report to the last would double it.
            TransferEvent::Admitted(counted) => {
                self.admitted_bytes.fetch_max(counted, Ordering::Release);
                return;
            }
            TransferEvent::Terminal(terminal) => {
                let _kept_the_first = self.terminal.set(terminal);
                &self.terminals
            }
            TransferEvent::FramePolled => &self.frames_polled,
            TransferEvent::CrossingReleased => &self.crossings_released,
            TransferEvent::Released => &self.releases,
        };
        counter.fetch_add(1, Ordering::Release);
    }

    /// This record as the snapshot an observing case reads.
    fn snapshot(&self) -> TransferDirectionObservation {
        TransferDirectionObservation {
            max_bytes: match self.max_bytes.load(Ordering::Acquire) {
                0 => None,
                max => Some(max),
            },
            idle: duration_of(self.idle_nanos.load(Ordering::Acquire)),
            total: duration_of(self.total_nanos.load(Ordering::Acquire)),
            frames_polled: self.frames_polled.load(Ordering::Acquire),
            admitted_bytes: self.admitted_bytes.load(Ordering::Acquire),
            crossings_released: self.crossings_released.load(Ordering::Acquire),
            terminal: self.terminal.get().copied(),
            terminals: self.terminals.load(Ordering::Acquire),
            releases: self.releases.load(Ordering::Acquire),
        }
    }
}

/// The nanosecond spelling one frozen deadline is recorded as.
fn nanos_of(configured: Option<std::time::Duration>) -> u64 {
    configured.map_or(UNBOUNDED_TOTAL_NANOS, |value| {
        u64::try_from(value.as_nanos()).unwrap_or(UNBOUNDED_TOTAL_NANOS)
    })
}

/// The deadline one recorded nanosecond value names.
///
/// Zero is "nothing recorded yet" rather than a configured deadline: every
/// finite deadline is validated above zero before a policy can hold one.
fn duration_of(nanos: u64) -> Option<std::time::Duration> {
    (nanos != UNBOUNDED_TOTAL_NANOS && nanos > 0).then(|| std::time::Duration::from_nanos(nanos))
}

/// The two independent direction records one listener's transfers publish to.
#[derive(Default)]
struct TransferObservations {
    upload: DirectionObservations,
    download: DirectionObservations,
}

/// What one thing a transfer owner did.
#[derive(Clone, Copy, Debug)]
pub(in crate::http) enum TransferEvent {
    /// The one budget this direction resolved to, before its first poll.
    PolicyFrozen(super::TransferBudget),
    /// One frame was polled out of the source.
    FramePolled,
    /// One payload frame was admitted, carrying the running total.
    Admitted(usize),
    /// One frame was released rather than delivered.
    CrossingReleased,
    /// The one terminal this direction fixed.
    Terminal(InboundTerminal),
    /// This owner reached its drop.
    Released,
}

/// What one listener's streaming transfers published, per direction.
///
/// Read-only. Every field is written by the production owner it names, and the
/// two directions never share a counter: that separation is the claim.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransferDirectionObservation {
    /// The frozen payload maximum, or `None` for an unbounded one.
    pub max_bytes: Option<usize>,
    /// The frozen quiet interval, or `None` for an unbounded one.
    pub idle: Option<std::time::Duration>,
    /// The frozen lifetime, or `None` for an unbounded one.
    pub total: Option<std::time::Duration>,
    /// Frames this direction's owner polled out of its source.
    pub frames_polled: usize,
    /// Payload bytes this direction admitted.
    pub admitted_bytes: usize,
    /// Frames released rather than delivered.
    pub crossings_released: usize,
    /// The one terminal this direction fixed, if it fixed one.
    pub terminal: Option<InboundTerminal>,
    /// How many terminals reached this record, including a second the set-once
    /// kept out.
    pub terminals: usize,
    /// How many owners of this direction reached their drop.
    pub releases: usize,
}

/// What one listener's two transfer directions published so far.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TransferObservation {
    /// What the streaming uploads under this listener published.
    pub upload: TransferDirectionObservation,
    /// What the streaming downloads under this listener published.
    pub download: TransferDirectionObservation,
}

/// Record one value, and count it when it differs from the last one recorded.
///
/// A last-value cell beside a change count. Four owners reading one identity
/// leave the count at one, and a second envelope reaching any owner leaves it
/// at two whichever owner saw it. The claim it supports is per admitted
/// request, so a case that wants it drives one request at a time against its
/// own listener.
fn count_distinct(last: &AtomicU64, distinct: &AtomicUsize, value: u64) {
    match last.swap(value, Ordering::AcqRel) {
        previous if previous == value => {}
        _ => {
            distinct.fetch_add(1, Ordering::Release);
        }
    }
}

/// The nanosecond spelling of an unbounded request total.
///
/// A real total is validated below the thirty-year policy ceiling, which is
/// under 10^18 nanoseconds, so the sentinel names no configurable value.
const UNBOUNDED_TOTAL_NANOS: u64 = u64::MAX;

/// What one listener's admitted operations have published so far.
///
/// Read-only. Every field is written by the production owner it names: the
/// mint that creates an envelope, and each pre-head owner that reads one.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OperationObservation {
    /// How many envelopes this listener's admitted heads minted.
    pub admitted: usize,
    /// The identity most recently read by any owner, or `None` if none has.
    pub identity: Option<u64>,
    /// How many distinct identities every owner between them read.
    pub distinct_identities: usize,
    /// The request-total deadline, as the offset from admission it was computed
    /// at. `None` is an unbounded total.
    pub total_from_admission: Option<std::time::Duration>,
    /// How many distinct request totals this listener's envelopes carried.
    pub distinct_totals: usize,
    /// How many times the classified-route owner read an identity.
    pub dispatch: usize,
    /// How many times the middleware owner read an identity.
    pub middleware: usize,
    /// How many times the payload owner read an identity.
    pub body: usize,
    /// How many times the response-head handoff read an identity.
    pub response_head: usize,
    /// How many accounts this listener's answering exits staged.
    pub completions_staged: usize,
    /// How many accounts this listener's terminal owners recorded.
    pub completions_recorded: usize,
}

/// What one listener's direct WebSocket bridges reported about their two
/// application queues.
///
/// Written by the production decisions they name — the bridge's own queue
/// construction, and the one terminal cause it commits — and read by the
/// observing case. Nothing here selects a capacity, admits a frame, closes a
/// queue, polls transport, releases a permit, or chooses a cause.
#[cfg(feature = "ws")]
#[derive(Default)]
struct WebSocketDirectionObservations {
    outbound_capacity: AtomicUsize,
    inbound_capacity: AtomicUsize,
    /// Set-once, exactly as the bridge's own terminal state is: a controller
    /// that could overwrite the recorded cause would let a case read a cause no
    /// endpoint ever observed.
    terminal: OnceLock<super::WsCloseCause>,
    /// How many causes reached this record, including any the set-once above
    /// kept out.
    ///
    /// The `OnceLock` alone cannot say that. A second commit carrying a
    /// different cause is discarded by it and reads exactly like a bridge that
    /// only ever committed one, which leaves "a committed cause survives a later
    /// escalation" unfalsifiable. Counting every commit is what lets a case
    /// claim the bridge fixed its cause once rather than merely first.
    commits: AtomicUsize,
    /// Peer messages the inbound pump has put in the receive queue.
    ///
    /// Counted where admission returns, so a case reads it to tell a message
    /// the receive owner can still take from one the pump never got in.
    inbound_admitted: AtomicUsize,
    inbound_settled: AtomicBool,
    outbound_settled: AtomicBool,
    /// Admitted outbound frames the bridge's terminal disposition cancelled.
    ///
    /// Counted where the bridge drops them, so a drain row reports zero and a
    /// cancel row reports what a successful `send` never put on the wire.
    outbound_cancelled: AtomicUsize,
    permit_released: AtomicBool,
}

/// What one listener's direct WebSocket bridges have published so far.
///
/// Read-only. The capacities are the ones the production bridge handed its two
/// bounded channels, not a copy of configuration; the cause is the one it
/// committed, or `None` while every bridge on this listener is still live.
#[cfg(feature = "ws")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebSocketDirectionObservation {
    pub outbound_capacity: usize,
    pub inbound_capacity: usize,
    pub terminal: Option<super::WsCloseCause>,
    /// How many causes the bridges on this listener committed.
    ///
    /// One per bridge. A second commit on one bridge is discarded by the
    /// set-once above, so this is the only place it is visible.
    pub terminal_commits: usize,
    pub inbound_admitted: usize,
    pub inbound_settled: bool,
    pub outbound_settled: bool,
    pub outbound_cancelled: usize,
    pub permit_released: bool,
}

pub(crate) struct LifecycleScript {
    state: Mutex<ScriptState>,
    supervisor_wake: tokio::sync::Notify,
    body: BodyObservations,
    /// What the checked collector published while reading this peer's answers.
    collection: CollectionObservations,
    /// What the static-file workers under this root published about where they
    /// ran and what they still hold.
    static_files: StaticFileObservations,
    /// What this process's profiling workers published about where they ran and
    /// what they retained.
    #[cfg(feature = "profiling")]
    profiling: ProfilingObservations,
    /// What this listener's admitted operations published about their one
    /// envelope each.
    operation: OperationObservations,
    /// What this listener's streaming transfers published, per direction.
    transfers: TransferObservations,
    /// What this listener's direct WebSocket bridges published.
    #[cfg(feature = "ws")]
    websocket: WebSocketDirectionObservations,
    /// What this listener's streaming multipart sessions have published.
    ///
    /// The production counters themselves, held here rather than beside each
    /// session, so a session reports through the same registration its
    /// checkpoints run through: one observer per listener, or none at all.
    multipart: super::multipart::SessionMetrics,
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
            body: BodyObservations::default(),
            collection: CollectionObservations::default(),
            static_files: StaticFileObservations::default(),
            #[cfg(feature = "profiling")]
            profiling: ProfilingObservations::default(),
            operation: OperationObservations::default(),
            transfers: TransferObservations::default(),
            #[cfg(feature = "ws")]
            websocket: WebSocketDirectionObservations::default(),
            multipart: super::multipart::SessionMetrics::default(),
        }
    }

    /// Write one observation onto the record `project` names, and do nothing
    /// when no controller watches.
    ///
    /// Every value this script publishes reaches its field through here, so
    /// "inert with no controller registered" is decided in one place rather than
    /// restated by each family of counters and each counter in it.
    fn observe<T>(
        script: Option<&Self>,
        project: impl FnOnce(&Self) -> &T,
        apply: impl FnOnce(&T),
    ) {
        match script {
            Some(script) => apply(project(script)),
            None => {}
        }
    }

    /// Record one envelope an admitted head minted, and the total it computed.
    ///
    /// Inert with no controller registered, exactly like [`Self::pause_at`].
    pub(in crate::http) fn observe_operation_admitted(
        script: Option<&Self>,
        id: super::operation::OperationId,
        total: Option<std::time::Duration>,
    ) {
        let nanos = nanos_of(total);
        Self::observe(
            script,
            |script| &script.operation,
            |operation| {
                operation.admitted.fetch_add(1, Ordering::Release);
                count_distinct(
                    &operation.total_from_admission,
                    &operation.distinct_totals,
                    nanos,
                );
                count_distinct(
                    &operation.identity,
                    &operation.distinct_identities,
                    id.value(),
                );
            },
        );
    }

    /// Record that the owner at `stage` read one operation's identity.
    ///
    /// Inert with no controller registered.
    pub(in crate::http) fn observe_operation(
        script: Option<&Self>,
        id: super::operation::OperationId,
        stage: OperationStage,
    ) {
        Self::observe(
            script,
            |script| &script.operation,
            |operation| {
                let reads = match stage {
                    OperationStage::Dispatch => &operation.dispatch,
                    OperationStage::Middleware => &operation.middleware,
                    OperationStage::Body => &operation.body,
                    OperationStage::ResponseHead => &operation.response_head,
                };
                reads.fetch_add(1, Ordering::Release);
                count_distinct(
                    &operation.identity,
                    &operation.distinct_identities,
                    id.value(),
                );
            },
        );
    }

    /// Record that one answering exit staged this request's account.
    ///
    /// Inert with no controller registered.
    pub(in crate::http) fn observe_completion_staged(script: Option<&Self>) {
        Self::observe(
            script,
            |script| &script.operation,
            |operation| {
                operation.completions_staged.fetch_add(1, Ordering::Release);
            },
        );
    }

    /// Record that one terminal owner wrote this request's account.
    ///
    /// Inert with no controller registered.
    pub(in crate::http) fn observe_completion_recorded(script: Option<&Self>) {
        Self::observe(
            script,
            |script| &script.operation,
            |operation| {
                operation
                    .completions_recorded
                    .fetch_add(1, Ordering::Release);
            },
        );
    }

    /// Record one thing a transfer owner did, under its own direction.
    ///
    /// Inert with no controller registered, exactly like [`Self::pause_at`]. The
    /// direction picks the record, so nothing an upload publishes can be read as
    /// a download's.
    pub(in crate::http) fn observe_transfer(
        script: Option<&Self>,
        direction: super::transfer::TransferDirection,
        event: TransferEvent,
    ) {
        Self::observe(
            script,
            |script| &script.transfers,
            |transfers| {
                let record = match direction {
                    super::transfer::TransferDirection::Upload => &transfers.upload,
                    super::transfer::TransferDirection::Download => &transfers.download,
                };
                record.apply(event);
            },
        );
    }

    /// Reach one checkpoint from inside a poll, and hand back the gate to hold at.
    ///
    /// `None` is a checkpoint nothing armed, a closed controller, or no
    /// controller at all: an owner that gets it runs straight through. The
    /// async [`Self::pause_at`] is what an owner that can await uses; this is for
    /// the streaming owners that are driven from a body poll and have no await to
    /// take.
    pub(in crate::http) fn hold_at(
        script: Option<&Self>,
        checkpoint: LifecycleCheckpoint,
    ) -> Option<CheckpointHold> {
        script
            .and_then(|script| script.reach(checkpoint))
            .map(CheckpointHold)
    }

    /// Write one direct-WebSocket observation.
    #[cfg(feature = "ws")]
    fn observe_ws(script: Option<&Self>, apply: impl FnOnce(&WebSocketDirectionObservations)) {
        Self::observe(script, |script| &script.websocket, apply);
    }

    /// Record the capacities one direct bridge handed its two bounded queues.
    ///
    /// Inert with no controller registered, exactly like [`Self::pause_at`].
    #[cfg(feature = "ws")]
    pub(in crate::http) fn observe_ws_capacities(
        script: Option<&Self>,
        outbound: usize,
        inbound: usize,
    ) {
        Self::observe_ws(script, |websocket| {
            websocket
                .outbound_capacity
                .store(outbound, Ordering::Release);
            websocket.inbound_capacity.store(inbound, Ordering::Release);
        });
    }

    /// Record the one terminal cause a direct bridge committed.
    ///
    /// The commit is counted before it is recorded, so a second one that the
    /// set-once refuses is still visible to a case. The refusal itself is
    /// discarded rather than reported: the first cause stays authoritative
    /// either way, and the count is what a row reads to tell one commit from
    /// two.
    #[cfg(feature = "ws")]
    pub(in crate::http) fn observe_ws_terminal(script: Option<&Self>, cause: super::WsCloseCause) {
        Self::observe_ws(script, |websocket| {
            websocket.commits.fetch_add(1, Ordering::Release);
            let _kept_the_first = websocket.terminal.set(cause);
        });
    }

    /// Record one peer message the inbound pump put in the receive queue.
    #[cfg(feature = "ws")]
    pub(in crate::http) fn count_ws_inbound_admitted(script: Option<&Self>) {
        Self::observe_ws(script, |websocket| {
            websocket.inbound_admitted.fetch_add(1, Ordering::Release);
        });
    }

    /// Record that one direction pump reached its own settlement.
    #[cfg(feature = "ws")]
    pub(in crate::http) fn observe_ws_pump_settled(
        script: Option<&Self>,
        direction: super::ws_proxy::WsDirection,
    ) {
        Self::observe_ws(script, |websocket| {
            let settled = match direction {
                super::ws_proxy::WsDirection::Inbound => &websocket.inbound_settled,
                super::ws_proxy::WsDirection::Outbound => &websocket.outbound_settled,
            };
            settled.store(true, Ordering::Release);
        });
    }

    /// Record the admitted outbound frames one terminal disposition cancelled.
    #[cfg(feature = "ws")]
    pub(in crate::http) fn count_ws_outbound_cancelled(script: Option<&Self>, cancelled: usize) {
        Self::observe_ws(script, |websocket| {
            websocket
                .outbound_cancelled
                .fetch_add(cancelled, Ordering::Release);
        });
    }

    /// Record that one direct bridge let go of its connection permit.
    #[cfg(feature = "ws")]
    pub(in crate::http) fn observe_ws_permit_released(script: Option<&Self>) {
        Self::observe_ws(script, |websocket| {
            websocket.permit_released.store(true, Ordering::Release);
        });
    }

    /// The counters this listener's multipart sessions publish through.
    pub(in crate::http) fn multipart(&self) -> &super::multipart::SessionMetrics {
        &self.multipart
    }

    /// Write one body observation.
    ///
    /// A fourth counter is the one line that names its field: the absence arm
    /// belongs to [`Self::observe`].
    fn observe_body(script: Option<&Self>, apply: impl FnOnce(&BodyObservations)) {
        Self::observe(script, |script| &script.body, apply);
    }

    /// Record one request-body frame the production collector polled out.
    ///
    /// Inert with no controller registered, exactly like [`Self::pause_at`].
    pub(crate) fn count_body_frame(script: Option<&Self>) {
        Self::observe_body(script, |body| {
            body.frames_polled.fetch_add(1, Ordering::Release);
        });
    }

    /// Record what one request holds after appending a decoded data frame.
    ///
    /// Kept as the high-water mark rather than a running sum: what a case
    /// claims about a bounded read is the most one request ever held at once,
    /// and a sum would report the whole listener's traffic instead.
    pub(crate) fn observe_body_retained(script: Option<&Self>, retained: usize) {
        Self::observe_body(script, |body| {
            body.peak_retained_bytes
                .fetch_max(retained, Ordering::Release);
        });
    }

    /// Write one buffered-collection observation.
    fn observe_collection(script: Option<&Self>, apply: impl FnOnce(&CollectionObservations)) {
        Self::observe(script, |script| &script.collection, apply);
    }

    /// Record one chunk the checked collector was handed.
    ///
    /// Counted before the collector accounts for it, so a chunk refused for
    /// crossing the maximum is counted as read and one refused before any read
    /// began is not. Inert with no controller registered.
    pub(in crate::http) fn count_collected_chunk(script: Option<&Self>) {
        Self::observe_collection(script, |collection| {
            collection.chunks_polled.fetch_add(1, Ordering::Release);
        });
    }

    /// Record what one collection holds once a chunk has been accounted for.
    ///
    /// Written after every accounting decision, kept or refused, so a crossing
    /// chunk that was retained anyway is reported rather than skipped with the
    /// refusal. The high-water mark, for the reason [`Self::observe_body_retained`]
    /// keeps one: the claim is the most a single collection ever held at once,
    /// and a sum would report every answer this peer gave instead.
    pub(in crate::http) fn observe_collected_retained(script: Option<&Self>, retained: usize) {
        Self::observe_collection(script, |collection| {
            collection
                .peak_retained_bytes
                .fetch_max(retained, Ordering::Release);
            // Recorded once, at the first total this scope ever held: a
            // collection that has kept nothing yet is still at zero, and the
            // chunk that changes that is the one whose size nobody declared.
            let _first = collection.first_retained_bytes.compare_exchange(
                0,
                retained,
                Ordering::AcqRel,
                Ordering::Acquire,
            );
        });
    }

    /// Record one thing a profiling worker did, where it did it.
    ///
    /// Inert with no controller registered, exactly like [`Self::pause_at`].
    #[cfg(feature = "profiling")]
    pub(in crate::http) fn observe_profiling(script: Option<&Self>, event: ProfilingEvent) {
        Self::observe(
            script,
            |script| &script.profiling,
            |profiling| {
                let counter = match event {
                    // A value, not a tally: the question is which maximum this
                    // render froze, and adding them would answer neither.
                    ProfilingEvent::CeilingFrozen(ceiling) => {
                        profiling
                            .worker
                            .frozen_ceiling
                            .store(ceiling, Ordering::Release);
                        return;
                    }
                    ProfilingEvent::Entered { off_caller: true } => {
                        &profiling.worker.workers_entered
                    }
                    // Counted under both, because an entry on the awaiting thread
                    // is still an entry: the ownership claim counts workers and
                    // the placement claim counts where they began.
                    ProfilingEvent::Entered { off_caller: false } => {
                        profiling.entries_on_caller.fetch_add(1, Ordering::Release);
                        &profiling.worker.workers_entered
                    }
                    ProfilingEvent::Returned => &profiling.worker.workers_returned,
                };
                counter.fetch_add(1, Ordering::Release);
            },
        );
    }

    /// Record one thing a static-file worker did, where it did it.
    ///
    /// Inert with no controller registered, exactly like [`Self::pause_at`].
    pub(in crate::http) fn observe_static_file(script: Option<&Self>, event: StaticFileEvent) {
        Self::observe(
            script,
            |script| &script.static_files,
            |files| {
                let counter = match event {
                    // A value, not a tally: the question is which maximum this
                    // read froze, and adding them would answer neither.
                    StaticFileEvent::CeilingFrozen(ceiling) => {
                        files
                            .worker
                            .frozen_ceiling
                            .store(ceiling, Ordering::Release);
                        return;
                    }
                    StaticFileEvent::WorkerEntered => &files.worker.workers_entered,
                    StaticFileEvent::WorkerReturned => &files.worker.workers_returned,
                    StaticFileEvent::Step {
                        off_caller: false, ..
                    } => &files.steps_on_caller,
                    StaticFileEvent::Step {
                        step: StaticFileStep::Canonicalize,
                        ..
                    } => &files.canonicalized_off_caller,
                    StaticFileEvent::Step {
                        step: StaticFileStep::Metadata,
                        ..
                    } => &files.metadata_off_caller,
                    StaticFileEvent::Step {
                        step: StaticFileStep::Read,
                        ..
                    } => &files.reads_off_caller,
                };
                counter.fetch_add(1, Ordering::Release);
            },
        );
    }

    /// Record one admitted permit owner reaching its drop.
    pub(crate) fn count_permit_owner_dropped(script: Option<&Self>) {
        Self::observe_body(script, |body| {
            body.permit_owners_dropped.fetch_add(1, Ordering::Release);
        });
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
                    released: Arc::new(ReleaseGate::default()),
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

    /// Wait until production is held at `checkpoint`.
    ///
    /// Held, rather than merely reached: the wait ends once the paused future has
    /// looked for its release, which is the first moment a caller can arm another
    /// checkpoint and release this one without both landing inside the production
    /// poll it is standing in. `ReleaseGate::looked` states what that costs a
    /// caller woken any earlier.
    ///
    /// Registration precedes the second read of the state, and both precede the
    /// wait. `notify_waiters` stores no permit: a look landing after an
    /// unregistered waiter read `Armed` is a wake that never happened, and the
    /// observer holds for its caller's whole bound on a checkpoint production is
    /// already held at.
    async fn wait_until_paused(&self, checkpoint: LifecycleCheckpoint) -> Result<(), RuntimeError> {
        loop {
            let gate = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                let entry = state
                    .checkpoints
                    .iter()
                    .find(|entry| entry.checkpoint == checkpoint)
                    .ok_or_else(|| Self::invalid("lifecycle checkpoint is not armed"))?;
                match (state.closed, entry.phase) {
                    (true, _) => return Err(Self::invalid("lifecycle controller is closed")),
                    (false, CheckpointPhase::Paused) if entry.released.has_looked() => {
                        return Ok(());
                    }
                    (false, CheckpointPhase::Released) => {
                        return Err(Self::invalid("lifecycle checkpoint was already released"));
                    }
                    (false, _) => Arc::clone(&entry.released),
                }
            };
            let notified = gate.looked.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let already_held = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                state.closed
                    || state.checkpoints.iter().any(|entry| {
                        entry.checkpoint == checkpoint
                            && entry.phase == CheckpointPhase::Paused
                            && entry.released.has_looked()
                    })
            };
            match already_held {
                true => continue,
                false => notified.await,
            }
        }
    }

    fn release_checkpoint(&self, checkpoint: LifecycleCheckpoint) -> Result<(), RuntimeError> {
        self.record_release(checkpoint)?.wake();
        Ok(())
    }

    /// Record one paused checkpoint's release, waking nothing.
    fn record_release(
        &self,
        checkpoint: LifecycleCheckpoint,
    ) -> Result<Arc<ReleaseGate>, RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let entry = state
            .checkpoints
            .iter_mut()
            .find(|entry| entry.checkpoint == checkpoint)
            .ok_or_else(|| Self::invalid("lifecycle checkpoint is not armed"))?;
        match entry.phase {
            CheckpointPhase::Paused => {
                entry.phase = CheckpointPhase::Released;
                let released = Arc::clone(&entry.released);
                released.record();
                Ok(released)
            }
            CheckpointPhase::Armed | CheckpointPhase::Released => {
                Err(Self::invalid("lifecycle checkpoint is not paused"))
            }
        }
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

    /// Pause at `checkpoint` when a script is watching, and do nothing when
    /// none is.
    ///
    /// Every checkpoint outside the supervisor reaches its script through an
    /// `Option`, so the absence arm is the common one. Stated here, beside the
    /// `pause` it guards, so a caller names its checkpoint and nothing else.
    pub(crate) async fn pause_at(script: Option<&Self>, checkpoint: LifecycleCheckpoint) {
        match script {
            Some(script) => script.pause(checkpoint).await,
            None => {}
        }
    }

    pub(crate) async fn pause(&self, checkpoint: LifecycleCheckpoint) {
        match self.reach(checkpoint) {
            Some(released) => released.held().await,
            None => {}
        }
    }

    /// Hold one blocking worker at `checkpoint` until a controller releases it.
    ///
    /// [`Self::pause_at`] for an owner that is not inside a poll. Inert with no
    /// controller registered, and inert for a checkpoint nothing armed: the
    /// worker runs straight through both without parking.
    pub(in crate::http) fn pause_blocking(script: Option<&Self>, checkpoint: LifecycleCheckpoint) {
        match script.and_then(|script| script.reach(checkpoint)) {
            Some(released) => released.held_blocking(),
            None => {}
        }
    }

    /// Mark this checkpoint reached, and hand back the gate it now waits on.
    ///
    /// `None` is a checkpoint nothing armed, or a closed controller: production
    /// runs straight through both.
    fn reach(&self, checkpoint: LifecycleCheckpoint) -> Option<Arc<ReleaseGate>> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match state.closed {
            true => None,
            false => state
                .checkpoints
                .iter_mut()
                .find(|entry| {
                    entry.checkpoint == checkpoint && entry.phase == CheckpointPhase::Armed
                })
                .map(CheckpointState::pause),
        }
    }

    pub(crate) async fn wait_for_supervisor_wake(&self) {
        self.supervisor_wake.notified().await;
    }

    /// How many turns whatever waits at `checkpoint` has taken.
    ///
    /// A checkpoint nothing armed is refused, the way every other lookup here
    /// refuses one. Payload-carrying variants match by value, so a case naming
    /// a limit it never armed would read a count of zero and pass every claim
    /// it made about turns without a turn ever being taken.
    fn checkpoint_polls(&self, checkpoint: LifecycleCheckpoint) -> Result<usize, RuntimeError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state
            .checkpoints
            .iter()
            .find(|entry| entry.checkpoint == checkpoint)
            .map(|entry| entry.released.polls())
            .ok_or_else(|| Self::invalid("lifecycle checkpoint is not armed"))
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
        let held = {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            state.closed = true;
            state
                .checkpoints
                .iter()
                .map(|entry| Arc::clone(&entry.released))
                .collect::<Vec<_>>()
        };
        held.into_iter().for_each(let_go);
    }
}

/// Let go of one checkpoint outright.
///
/// A closing controller owes both halves to every checkpoint it still holds:
/// whoever waits to hear production is held at it, and whatever is held at its
/// release. The observer is woken whether or not the look it waits for ever
/// happened, because on a closed controller it never will. Production parked at
/// a checkpoint resumes rather than waiting on a controller that no longer
/// exists.
fn let_go(released: Arc<ReleaseGate>) {
    released.looked.notify_waiters();
    released.record();
    released.wake();
}

/// What one registered controller watches.
///
/// A served listener is named by the address its peers reach. Static-file work
/// has no peer to name it, so it is named by the root it serves from, and two
/// roots are two independent observers. Profiling has neither: the profiler is one
/// process-wide registration, so its scope is the process and there is exactly one
/// of it. One registry holds all three because a controller is the same thing
/// either way: one script, its armed checkpoints, and its read-only counters.
#[derive(Clone, Debug, Eq, PartialEq)]
enum ObservedScope {
    Listener(std::net::SocketAddr),
    StaticRoot(Box<std::path::Path>),
    #[cfg(feature = "profiling")]
    Profiler,
}

struct LifecycleRegistration {
    scope: ObservedScope,
    script: Weak<LifecycleScript>,
}

fn lifecycle_registry() -> &'static Mutex<Vec<LifecycleRegistration>> {
    static REGISTRY: OnceLock<Mutex<Vec<LifecycleRegistration>>> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(Vec::new()))
}

/// How many controllers the registry holds, readable without the lock.
///
/// [`scoped_script`] is on the path of every buffered proxy forward, every
/// outbound client answer, and every static-file read, and it is asking a
/// question whose answer outside a test is always "nothing is watching". Taking
/// a process-wide mutex to hear that serializes every Tokio worker in the
/// process against one lock the production build never needed.
///
/// Written under the registry's own lock, on the two lines that change what the
/// registry holds. Read [`Ordering::Relaxed`] on the fast path, because the only
/// race it admits is a lookup that runs concurrently with the registration it
/// would have found, and no case has one: a controller is created before the
/// server, the root, or the profiler it observes, so the registration is ordered
/// ahead of the production path by whatever started that work. A stronger
/// ordering would not close a genuine race here either — it would still have to
/// read zero — and once a reader sees a nonzero count the registry's lock is
/// what publishes the entries behind it.
static REGISTERED_SCOPES: AtomicUsize = AtomicUsize::new(0);

#[doc(hidden)]
pub struct LifecycleController {
    scope: ObservedScope,
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

    /// Record one paused checkpoint's release without waking what waits there.
    ///
    /// The held future stays parked and observes the release on whatever poll
    /// something else provokes. That is the only way to stage two results into
    /// one turn of a `select!`: [`Self::release`] wakes the future it releases,
    /// so the turn is decided before the second result exists, and a precedence
    /// rule between two ready results is never exercised.
    ///
    /// A checkpoint held by a blocking worker is always unparked, staged or not.
    /// Withholding the wake is about a task wake deciding a `select!` turn early,
    /// which no blocking worker has; a parked thread has no later poll to observe
    /// the release on, so withholding it there would hang the case rather than
    /// stage anything.
    pub fn stage_release(&self, checkpoint: LifecycleCheckpoint) -> Result<(), RuntimeError> {
        self.script.record_release(checkpoint).map(drop)
    }

    /// How many turns whatever is held at `checkpoint` has taken.
    ///
    /// A held future looks for its release once per poll, so this counts the
    /// polls that future has been given since the checkpoint was armed. It is
    /// what tells a staged turn that something else already spent from one that
    /// is still to come: [`Self::stage_release`] leaves a release nothing has
    /// looked at yet, and only this says whether anything has since.
    ///
    /// A checkpoint nothing armed is an error rather than a count of zero. The
    /// count only means something against a checkpoint this controller holds,
    /// and a misnamed one read as zero satisfies every claim a case can make
    /// about turns.
    pub fn checkpoint_polls(&self, checkpoint: LifecycleCheckpoint) -> Result<usize, RuntimeError> {
        self.script.checkpoint_polls(checkpoint)
    }

    pub fn inject_once(&self, fault: LifecycleFault) -> Result<(), RuntimeError> {
        self.script.inject(fault)
    }

    /// How many request-body frames this listener's collectors have polled out.
    pub fn body_frames_polled(&self) -> usize {
        self.script.body.frames_polled.load(Ordering::Acquire)
    }

    /// The most bytes any one request on this listener retained at once.
    pub fn body_peak_retained_bytes(&self) -> usize {
        self.script.body.peak_retained_bytes.load(Ordering::Acquire)
    }

    /// How many chunks the checked collector read from this peer's answers.
    ///
    /// Zero after a refused declaration is the whole claim there: the maximum
    /// was crossed before anything was read, so nothing was allocated for it.
    pub fn collected_chunks_polled(&self) -> usize {
        self.script.collection.chunks_polled.load(Ordering::Acquire)
    }

    /// The most any one collection from this peer retained at once.
    pub fn collected_peak_retained_bytes(&self) -> usize {
        self.script
            .collection
            .peak_retained_bytes
            .load(Ordering::Acquire)
    }

    /// What the first chunk retained under this scope left behind.
    ///
    /// The exact boundary a source with undeclared chunk sizes offers: a case
    /// that freezes a maximum here lands it on a real chunk edge instead of a
    /// number the producer never promised. Zero is "nothing retained yet".
    pub fn collected_first_retained_bytes(&self) -> usize {
        self.script
            .collection
            .first_retained_bytes
            .load(Ordering::Acquire)
    }

    /// How many admitted permit owners this listener has released.
    pub fn body_permit_owners_dropped(&self) -> usize {
        self.script
            .body
            .permit_owners_dropped
            .load(Ordering::Acquire)
    }

    /// What this listener's admitted operations have published so far.
    ///
    /// Read-only. Every value is written by the production owner it names: the
    /// mint an admitted head runs, and each pre-head owner that reads the one
    /// envelope it produced. Nothing here mints an identity, chooses a policy,
    /// or decides which owner runs.
    pub fn operations_observed(&self) -> OperationObservation {
        let operation = &self.script.operation;
        let identity = operation.identity.load(Ordering::Acquire);
        let total = operation.total_from_admission.load(Ordering::Acquire);
        OperationObservation {
            admitted: operation.admitted.load(Ordering::Acquire),
            identity: (identity > 0).then_some(identity),
            distinct_identities: operation.distinct_identities.load(Ordering::Acquire),
            total_from_admission: duration_of(total),
            distinct_totals: operation.distinct_totals.load(Ordering::Acquire),
            dispatch: operation.dispatch.load(Ordering::Acquire),
            middleware: operation.middleware.load(Ordering::Acquire),
            body: operation.body.load(Ordering::Acquire),
            response_head: operation.response_head.load(Ordering::Acquire),
            completions_staged: operation.completions_staged.load(Ordering::Acquire),
            completions_recorded: operation.completions_recorded.load(Ordering::Acquire),
        }
    }

    /// What this listener's streaming transfers have published so far.
    ///
    /// Read-only, and every number in it is written by the production owner it
    /// names: the policy each direction froze, the frames it polled, the bytes
    /// it admitted, the frame it released instead of delivering, the one terminal
    /// it fixed, and its own release. Nothing here polls a source, admits a
    /// byte, selects a terminal, maps a refusal, or releases a producer.
    pub fn transfers_observed(&self) -> TransferObservation {
        TransferObservation {
            upload: self.script.transfers.upload.snapshot(),
            download: self.script.transfers.download.snapshot(),
        }
    }

    /// What this listener's direct WebSocket bridges have published so far.
    ///
    /// Read-only, and every value in it is written by the production decision
    /// it names — the bridge's own bounded-channel construction, and the single
    /// cause it commits before it lets go of either queue end. Nothing here
    /// sets a cause, enqueues a frame, closes a queue, polls transport,
    /// releases a permit, or synthesizes a result.
    #[cfg(feature = "ws")]
    pub fn websocket_directions(&self) -> WebSocketDirectionObservation {
        let websocket = &self.script.websocket;
        WebSocketDirectionObservation {
            outbound_capacity: websocket.outbound_capacity.load(Ordering::Acquire),
            inbound_capacity: websocket.inbound_capacity.load(Ordering::Acquire),
            terminal: websocket.terminal.get().copied(),
            terminal_commits: websocket.commits.load(Ordering::Acquire),
            inbound_admitted: websocket.inbound_admitted.load(Ordering::Acquire),
            inbound_settled: websocket.inbound_settled.load(Ordering::Acquire),
            outbound_settled: websocket.outbound_settled.load(Ordering::Acquire),
            outbound_cancelled: websocket.outbound_cancelled.load(Ordering::Acquire),
            permit_released: websocket.permit_released.load(Ordering::Acquire),
        }
    }

    /// What this listener's streaming multipart sessions have done so far.
    ///
    /// Read-only, and every number in it is written by the production decision
    /// it names: nothing here sets a terminal state, polls a body, maps a
    /// result, releases ownership, or commits a response. A served listener
    /// owns none of the allocations behind its bodies, so it witnesses no freed
    /// backing and claims none.
    pub fn multipart_observed(&self) -> MultipartObservation {
        MultipartObservation::of(self.script.multipart(), None, None)
    }

    /// What this process's profiling workers have done so far.
    ///
    /// Read-only, and every number in it is written by the production owner it
    /// names: nothing here starts a worker, samples a stack, renders a byte,
    /// chooses a maximum, or decides the thread any of it runs on.
    #[cfg(feature = "profiling")]
    pub fn profiling_observed(&self) -> ProfilingObservation {
        let profiling = &self.script.profiling;
        ProfilingObservation {
            frozen_ceiling: profiling.worker.frozen_ceiling.load(Ordering::Acquire),
            workers_entered: profiling.worker.workers_entered.load(Ordering::Acquire),
            workers_returned: profiling.worker.workers_returned.load(Ordering::Acquire),
            entries_on_caller: profiling.entries_on_caller.load(Ordering::Acquire),
        }
    }

    /// What the static-file workers under this root have done so far.
    ///
    /// Read-only, and every number in it is written by the production step it
    /// names: nothing here starts a worker, resolves a path, measures a file,
    /// reads a byte, or chooses the thread any of that runs on.
    pub fn static_files_observed(&self) -> StaticFileObservation {
        let files = &self.script.static_files;
        StaticFileObservation {
            frozen_ceiling: files.worker.frozen_ceiling.load(Ordering::Acquire),
            workers_entered: files.worker.workers_entered.load(Ordering::Acquire),
            workers_returned: files.worker.workers_returned.load(Ordering::Acquire),
            canonicalized_off_caller: files.canonicalized_off_caller.load(Ordering::Acquire),
            metadata_off_caller: files.metadata_off_caller.load(Ordering::Acquire),
            reads_off_caller: files.reads_off_caller.load(Ordering::Acquire),
            steps_on_caller: files.steps_on_caller.load(Ordering::Acquire),
        }
    }
}

impl Drop for LifecycleController {
    fn drop(&mut self) {
        self.script.close();
        let mut registry = lifecycle_registry()
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        registry.retain(|entry| {
            entry.scope != self.scope
                || entry
                    .script
                    .upgrade()
                    .is_some_and(|script| !Arc::ptr_eq(&script, &self.script))
        });
        // Counted down after the entry is gone and while the lock is still held,
        // so no lookup can read a count of zero over a registry this controller
        // is still listed in.
        REGISTERED_SCOPES.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Register one observer for `scope`, or refuse a second one.
///
/// The whole registration, stated once: a scope already watched cannot be
/// watched again, because two controllers over one script would each arm and
/// release checkpoints the other is holding.
fn register(scope: ObservedScope) -> Result<LifecycleController, RuntimeError> {
    let mut registry = lifecycle_registry()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    registry.retain(|entry| entry.script.strong_count() > 0);
    match registry.iter().any(|entry| entry.scope == scope) {
        true => Err(RuntimeError::InvalidArgument(
            "lifecycle controller already exists for scope".into(),
        )),
        false => {
            let script = Arc::new(LifecycleScript::new());
            registry.push(LifecycleRegistration {
                scope: scope.clone(),
                script: Arc::downgrade(&script),
            });
            // Counted up under the same lock the entry was pushed under, so the
            // count and the registry never disagree for a reader that takes it.
            REGISTERED_SCOPES.fetch_add(1, Ordering::Relaxed);
            Ok(LifecycleController { scope, script })
        }
    }
}

/// The script watching the one scope `watching` recognizes, if any.
///
/// The scope is recognized rather than rebuilt, so a production lookup that
/// finds nothing — which is every lookup outside a test — copies no address and
/// no path to ask the question. An empty registry is answered from
/// [`REGISTERED_SCOPES`] alone, so that lookup also takes no lock.
fn scoped_script(watching: impl Fn(&ObservedScope) -> bool) -> Option<Arc<LifecycleScript>> {
    match REGISTERED_SCOPES.load(Ordering::Relaxed) {
        0 => None,
        _ => registered_script(watching),
    }
}

/// The script watching the one scope `watching` recognizes, searched under the
/// registry's lock.
///
/// Reached only once a controller is known to exist, and it prunes the entries
/// whose controllers are gone on the way past.
fn registered_script(watching: impl Fn(&ObservedScope) -> bool) -> Option<Arc<LifecycleScript>> {
    let mut registry = lifecycle_registry()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    registry.retain(|entry| entry.script.strong_count() > 0);
    registry
        .iter()
        .find(|entry| watching(&entry.scope))
        .and_then(|entry| entry.script.upgrade())
}

#[doc(hidden)]
pub fn lifecycle(addr: std::net::SocketAddr) -> Result<LifecycleController, RuntimeError> {
    register(ObservedScope::Listener(addr))
}

/// Watch the static-file work one served root answers from.
///
/// The root is the one a caller names, before any canonicalization, because
/// that is the only spelling both a direct call and a registered route share.
#[doc(hidden)]
pub fn static_file_lifecycle(root: &std::path::Path) -> Result<LifecycleController, RuntimeError> {
    register(ObservedScope::StaticRoot(root.into()))
}

pub(crate) fn lifecycle_script(addr: std::net::SocketAddr) -> Option<Arc<LifecycleScript>> {
    scoped_script(|scope| matches!(scope, ObservedScope::Listener(listener) if *listener == addr))
}

pub(in crate::http) fn static_file_script(root: &std::path::Path) -> Option<Arc<LifecycleScript>> {
    scoped_script(
        |scope| matches!(scope, ObservedScope::StaticRoot(watched) if watched.as_ref() == root),
    )
}

/// Watch the profiling work this process answers.
///
/// One scope, because the profiler itself is one: `pprof` registers a single
/// process-wide sampler, so two profiling observers would be two views of the
/// same worker rather than two independent ones.
#[cfg(feature = "profiling")]
#[doc(hidden)]
pub fn profiling_lifecycle() -> Result<LifecycleController, RuntimeError> {
    register(ObservedScope::Profiler)
}

#[cfg(feature = "profiling")]
pub(in crate::http) fn profiling_script() -> Option<Arc<LifecycleScript>> {
    scoped_script(|scope| matches!(scope, ObservedScope::Profiler))
}

/// One checkpoint's reach and its first look, held apart.
///
/// A served checkpoint does both inside one poll — [`LifecycleScript::pause`]
/// takes the gate back from `reach` and polls it in the same call — so no case
/// outside this crate can tell which of the two moments
/// [`LifecycleController::wait_until_paused`] ends on. This holds the same
/// `CheckpointState` and `ReleaseGate` a listener's script owns, reaches the
/// checkpoint the way production reaches it, and leaves the look to its caller.
///
/// Its script is registered to no address, so [`lifecycle_script`] never
/// resolves it and no served connection, upgrade, or bridge can observe it. It
/// orders one gate it armed itself and reports what that gate has recorded. It
/// selects no policy, handshake result, terminal cause, cleanup outcome, or
/// permit release, and it reaches no checkpoint on any listener's behalf.
#[doc(hidden)]
pub struct CheckpointWaitProbe {
    script: LifecycleScript,
    checkpoint: LifecycleCheckpoint,
    /// The gate the reached checkpoint waits on, once it has been reached.
    gate: Option<Arc<ReleaseGate>>,
}

impl CheckpointWaitProbe {
    /// Reach the probed checkpoint, and stop before the first look.
    ///
    /// This is exactly what a production `pause_at` does first. What it does
    /// next — look for the release — is [`Self::look`], so the two moments are
    /// separate calls here and one call everywhere else.
    pub fn reach(&mut self) -> Result<(), RuntimeError> {
        match self.script.reach(self.checkpoint) {
            Some(gate) => {
                self.gate = Some(gate);
                Ok(())
            }
            None => Err(LifecycleScript::invalid(
                "lifecycle checkpoint is not armed",
            )),
        }
    }

    /// Take one turn of the future held at the checkpoint, and answer what that
    /// turn found: the release recorded, or not yet.
    ///
    /// The turn is the production poll's, not a copy of it: it goes through the
    /// same [`ReleaseGate::poll_release`] the held future calls, so it counts
    /// itself and publishes a first look the same way.
    pub fn look(&self) -> Result<bool, RuntimeError> {
        let gate = self
            .gate
            .as_ref()
            .ok_or_else(|| LifecycleScript::invalid("lifecycle checkpoint has not been reached"))?;
        let context = std::task::Context::from_waker(std::task::Waker::noop());
        Ok(gate.poll_release(&context).is_ready())
    }

    /// The wait a case takes on a checkpoint it armed.
    pub async fn wait_until_paused(&self) -> Result<(), RuntimeError> {
        self.script.wait_until_paused(self.checkpoint).await
    }

    /// How many turns the future held at the checkpoint has taken.
    pub fn polls(&self) -> Result<usize, RuntimeError> {
        self.script.checkpoint_polls(self.checkpoint)
    }

    /// Record the release without waking what waits at it, as
    /// [`LifecycleController::stage_release`] does.
    pub fn stage_release(&self) -> Result<(), RuntimeError> {
        self.script.record_release(self.checkpoint).map(drop)
    }
}

/// Arm one checkpoint on a script no listener resolves.
#[doc(hidden)]
pub fn checkpoint_wait_probe(
    checkpoint: LifecycleCheckpoint,
) -> Result<CheckpointWaitProbe, RuntimeError> {
    let script = LifecycleScript::new();
    script.arm(checkpoint)?;
    Ok(CheckpointWaitProbe {
        script,
        checkpoint,
        gate: None,
    })
}

#[doc(hidden)]
pub fn supervisor_join_probe(probe: SupervisorJoinProbe) -> super::server::ServerHandleFuture {
    super::server_lifecycle::supervisor_join_probe(probe)
}

/// Mint one request identifier through the exact production generator.
///
/// Semver-unsupported, and it takes no argument and offers no alternate
/// algorithm: a measurement of what generation costs has to measure the
/// generator a served request uses, and building a whole `Request` to reach it
/// would count the request's allocations instead.
#[doc(hidden)]
pub fn generated_request_id() -> super::RequestId {
    super::RequestId::generate()
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
    /// Shared with the [`MockHttp`] handle registration hands back, so the two
    /// owners of one immutable URL cost a refcount bump rather than a second
    /// allocation and a full copy.
    url: Arc<str>,
    status: u16,
    body: bytes::Bytes,
    /// Owned outright, not shared: nothing reads these headers but the
    /// interception below, which copies each pair out. An `Arc<[_]>` here paid
    /// for an atomic refcount block no one shares, and cost a second allocation
    /// and a full copy at registration — `Vec::into` cannot reuse the buffer
    /// for `Arc<[T]>` the way `into_boxed_slice` does.
    headers: Box<[HeaderPair]>,
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
    url: Arc<str>,
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
        let url = Arc::clone(&self.url);
        let entry = MockEntry {
            method,
            url: self.url,
            status: resp.status(),
            body: bytes::Bytes::copy_from_slice(resp.body_bytes()),
            headers: resp.headers().to_vec().into_boxed_slice(),
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
    url: Arc<str>,
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
    /// Deregister this mock, and only this mock.
    ///
    /// Matched by the identity of the shared counter, not by (url, method).
    /// `install` pushes unconditionally, so two live mocks can name the same
    /// URL and method; removing by name took both out, and the registry is
    /// process-global, so two tests in one binary that mocked the same URL
    /// deregistered each other. The survivor then counted nothing while the
    /// real network call went out — a flake, not a failure.
    fn drop(&mut self) {
        with_registry(|entries| {
            entries.retain(|entry| !Arc::ptr_eq(&entry.call_count, &self.call_count));
            if entries.is_empty() {
                MOCK_ACTIVE.store(false, Ordering::Release);
            }
        });
    }
}

/// One controlled source frame, and the witness that fires when its backing
/// allocation is released.
///
/// The backing is what a chunk copied out of it must not keep alive, so the
/// witness is the whole point: a chunk that borrowed instead of copying would
/// hold this owner and the witness would stay silent.
struct WitnessedFrame {
    bytes: Box<[u8]>,
    witness: Arc<AtomicUsize>,
}

impl AsRef<[u8]> for WitnessedFrame {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

impl Drop for WitnessedFrame {
    fn drop(&mut self) {
        self.witness.fetch_add(1, Ordering::AcqRel);
    }
}

/// The admitted permit's release witness.
struct WitnessedPermit(Arc<AtomicUsize>);

impl Drop for WitnessedPermit {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::AcqRel);
    }
}

/// The transport failure a controlled body ends with.
#[derive(Debug)]
struct ControlledBodyError(Box<str>);

impl std::fmt::Display for ControlledBodyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A concrete request body whose frames, stall, and ending a case chose in
/// advance.
///
/// It supplies bytes and scheduling and nothing else: it cannot choose a parser
/// state, a budget, a terminal summary, or a rejection.
struct ControlledBody {
    frames: std::collections::VecDeque<bytes::Bytes>,
    failure: Option<Box<str>>,
    /// How many frames this body hands out before it parks.
    stall_after: Option<usize>,
    handed: usize,
    /// Where this body stops handing out frames until a case lets it go.
    ///
    /// This is what makes "accepted, ingress advanced, no reply yet" a state a
    /// case can stand in. A body that is always ready runs from acceptance to
    /// publication inside one poll, so that phase would never be observable.
    /// The same gate a paused checkpoint waits on, because the two wait for the
    /// same thing and a second copy registered its waker after reading the
    /// release instead of under the lock that guards it — a release landing in
    /// that window woke nothing and parked the body for good.
    gate: Arc<ReleaseGate>,
}

impl ControlledBody {
    /// Whether this body has handed out everything it may before its gate opens.
    fn stalled(&self) -> bool {
        self.stall_after
            .is_some_and(|limit| self.handed >= limit && !self.gate.is_released())
    }

    /// The next thing this body hands out: a frame, a failure, or the end.
    fn next_frame(
        &mut self,
    ) -> Option<Result<hyper::body::Frame<bytes::Bytes>, ControlledBodyError>> {
        match self.frames.pop_front() {
            Some(frame) => {
                self.handed += 1;
                Some(Ok(hyper::body::Frame::data(frame)))
            }
            None => self
                .failure
                .take()
                .map(|message| Err(ControlledBodyError(message))),
        }
    }
}

impl hyper::body::Body for ControlledBody {
    type Data = bytes::Bytes;
    type Error = ControlledBodyError;

    fn poll_frame(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<hyper::body::Frame<Self::Data>, Self::Error>>> {
        let body = self.get_mut();
        if body.stalled() && body.gate.poll_release(cx).is_pending() {
            return std::task::Poll::Pending;
        }
        std::task::Poll::Ready(body.next_frame())
    }
}

/// How one observed multipart session ended.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MultipartTerminalKind {
    /// Terminal framing was consumed through end of body.
    Clean,
    /// An incomplete field, a canceled operation, or revocation ended it.
    Abandoned,
    /// A total or per-field byte crossing ended it.
    ByteLimit,
    /// An incoming transport read failure ended it.
    Unreadable,
    /// A grammar, structural, or framing failure ended it.
    Structural,
    /// The selected upload owner fixed one inbound or transfer terminal.
    Ended(InboundTerminal),
}

/// A read-only snapshot of what one multipart session has done so far.
///
/// Every number is written by the production decision it names. Nothing here
/// sets a terminal state, polls a body, maps a result, releases ownership, or
/// commits a response.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MultipartObservation {
    body_frames_polled: usize,
    commands_accepted: usize,
    replies_published: usize,
    parser_retained_bytes: usize,
    parser_peak_bytes: usize,
    reply_retained_bytes: usize,
    reply_peak_bytes: usize,
    active_metadata_peak_bytes: usize,
    source_frames_released: usize,
    source_frame_backings_freed: Option<usize>,
    permit_owners_dropped: usize,
    permit_backings_freed: Option<usize>,
    revocations: usize,
    drivers_terminated: usize,
}

impl MultipartObservation {
    /// Snapshot what one set of session counters currently holds.
    ///
    /// Every count the driver publishes is read from the counters, so both
    /// producers mean the same thing by it. The two freed-allocation counts are
    /// parameters because only a fixture that owns the allocation can witness
    /// it: a controlled session watches its own frame and permit backings go,
    /// and a served listener has no such witness and supplies none. One
    /// constructor, so a field added here cannot reach one producer and not the
    /// other.
    fn of(
        metrics: &super::multipart::SessionMetrics,
        source_frame_backings_freed: Option<usize>,
        permit_backings_freed: Option<usize>,
    ) -> Self {
        Self {
            body_frames_polled: metrics.body_frames_polled(),
            commands_accepted: metrics.commands_accepted(),
            replies_published: metrics.replies_published(),
            parser_retained_bytes: metrics.parser_retained_bytes(),
            parser_peak_bytes: metrics.parser_peak_bytes(),
            reply_retained_bytes: metrics.reply_retained_bytes(),
            reply_peak_bytes: metrics.reply_peak_bytes(),
            active_metadata_peak_bytes: metrics.active_metadata_peak_bytes(),
            source_frames_released: metrics.source_frames_released(),
            source_frame_backings_freed,
            permit_owners_dropped: metrics.permit_owners_dropped(),
            permit_backings_freed,
            revocations: metrics.revocations(),
            drivers_terminated: metrics.drivers_terminated(),
        }
    }

    /// How many payload frames the driver polled.
    pub fn body_frames_polled(&self) -> usize {
        self.body_frames_polled
    }

    /// How many commands the driver accepted.
    pub fn commands_accepted(&self) -> usize {
        self.commands_accepted
    }

    /// How many replies the driver published.
    pub fn replies_published(&self) -> usize {
        self.replies_published
    }

    /// What the parser budget and the outstanding reply hold now.
    pub fn parser_retained_bytes(&self) -> usize {
        self.parser_retained_bytes
    }

    /// The most the parser budget ever held.
    pub fn parser_peak_bytes(&self) -> usize {
        self.parser_peak_bytes
    }

    /// What the one outstanding reply payload owns now.
    pub fn reply_retained_bytes(&self) -> usize {
        self.reply_retained_bytes
    }

    /// The largest reply payload this session published.
    pub fn reply_peak_bytes(&self) -> usize {
        self.reply_peak_bytes
    }

    /// The largest active field metadata payload this session retained.
    pub fn active_metadata_peak_bytes(&self) -> usize {
        self.active_metadata_peak_bytes
    }

    /// How many spent source frames the driver released its handle on.
    ///
    /// The weaker of the two frame claims, and the one both fixtures can make:
    /// the driver dropped its `Bytes`, whether or not an application chunk still
    /// keeps the backing alive. For the copy-not-borrow claim read
    /// [`Self::source_frame_backings_freed`].
    pub fn source_frames_released(&self) -> usize {
        self.source_frames_released
    }

    /// How many source-frame backing allocations were proven freed.
    ///
    /// `None` where nothing witnesses the allocation. A served listener hands
    /// its bodies to hyper and holds no witness for them, so only a controlled
    /// session answers this — and its answer is the strong claim: a chunk that
    /// borrowed its frame instead of copying would keep the backing alive and
    /// this count would stay behind.
    pub fn source_frame_backings_freed(&self) -> Option<usize> {
        self.source_frame_backings_freed
    }

    /// How many admitted permit owners this session released.
    ///
    /// This session's own drivers, never another request class sharing the
    /// listener: the count is written by the driver that holds the permit.
    pub fn permit_owners_dropped(&self) -> usize {
        self.permit_owners_dropped
    }

    /// How many admitted permit backing allocations were proven freed.
    ///
    /// `None` where nothing witnesses the allocation, exactly as with
    /// [`Self::source_frame_backings_freed`].
    pub fn permit_backings_freed(&self) -> Option<usize> {
        self.permit_backings_freed
    }

    /// How many sessions had their command admission revoked by a coordinator.
    pub fn revocations(&self) -> usize {
        self.revocations
    }

    /// How many drivers returned their terminal summary.
    pub fn drivers_terminated(&self) -> usize {
        self.drivers_terminated
    }
}

/// What one finished multipart session ended as, and what it did.
#[doc(hidden)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MultipartOutcome {
    terminal: MultipartTerminalKind,
    diagnostic: Option<Box<str>>,
    observation: MultipartObservation,
}

impl MultipartOutcome {
    /// What one finished session ended as, and what it did.
    ///
    /// It consumes the terminal summary, so the diagnostic moves out of it
    /// rather than being copied out of a value dropped on the next line, and the
    /// terminal set is named here once instead of in one function per field.
    fn of(
        terminal: super::multipart::MultipartTerminal,
        observation: MultipartObservation,
    ) -> Self {
        use super::multipart::{MultipartFailure, MultipartTerminal};
        let (terminal, diagnostic) = match terminal {
            MultipartTerminal::Clean => (MultipartTerminalKind::Clean, None),
            MultipartTerminal::Abandoned => (MultipartTerminalKind::Abandoned, None),
            MultipartTerminal::ParserFailure(MultipartFailure::ByteLimit, diagnostic) => {
                (MultipartTerminalKind::ByteLimit, Some(diagnostic))
            }
            MultipartTerminal::ParserFailure(MultipartFailure::Unreadable, diagnostic) => {
                (MultipartTerminalKind::Unreadable, Some(diagnostic))
            }
            MultipartTerminal::ParserFailure(MultipartFailure::Structural, diagnostic) => {
                (MultipartTerminalKind::Structural, Some(diagnostic))
            }
            MultipartTerminal::Ended(failure) => (
                MultipartTerminalKind::Ended(failure.terminal()),
                Some(failure.error().to_string().into()),
            ),
        };
        Self {
            terminal,
            diagnostic,
            observation,
        }
    }

    /// The terminal summary the driver returned.
    pub fn terminal(&self) -> MultipartTerminalKind {
        self.terminal
    }

    /// The operator diagnostic a failed session recorded, if it failed.
    ///
    /// This is the private text the driver kept, not the fixed safe text a peer
    /// is answered with.
    pub fn diagnostic(&self) -> Option<&str> {
        self.diagnostic.as_deref()
    }

    /// The observations taken after the driver returned.
    pub fn observed(&self) -> MultipartObservation {
        self.observation
    }
}

/// Whether the admitted session owner still cannot be duplicated.
///
/// The driver is `pub(in crate::http)`, so no test outside the crate can name
/// it. Two implementations, one blanket and one constrained to `Clone`, resolve
/// the marker only while the driver has no `Clone` implementation: giving the
/// one owner of the body, the budget, the parser, and the permit a second owner
/// makes this call ambiguous and the crate stops compiling.
#[doc(hidden)]
pub fn multipart_session_owner_is_not_cloneable() -> bool {
    trait AmbiguousIfClone<Witness> {
        fn owns_one_session() -> bool {
            true
        }
    }
    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    impl<T: Clone> AmbiguousIfClone<u8> for T {}

    type ProductionDriver = super::multipart::MultipartSessionDriver<hyper::body::Incoming>;
    <ProductionDriver as AmbiguousIfClone<_>>::owns_one_session()
}

/// Build one controlled multipart session over the production driver.
///
/// The frames, the admitted maximum, and whether a permit exists are the only
/// inputs. The parser, the budget, the command protocol, the terminal summary,
/// and every refusal are the production code's.
#[doc(hidden)]
pub fn multipart_session(
    boundary: &str,
    limits: super::MultipartLimits,
) -> MultipartSessionBuilder {
    MultipartSessionBuilder {
        boundary: boundary.into(),
        limits,
        frames: Vec::new(),
        failure: None,
        body_limit: usize::MAX,
        permit: false,
        stall_after: None,
        source_drops: Arc::new(AtomicUsize::new(0)),
    }
}

#[doc(hidden)]
pub struct MultipartSessionBuilder {
    boundary: Box<str>,
    limits: super::MultipartLimits,
    frames: Vec<bytes::Bytes>,
    failure: Option<Box<str>>,
    body_limit: usize,
    permit: bool,
    stall_after: Option<usize>,
    source_drops: Arc<AtomicUsize>,
}

impl MultipartSessionBuilder {
    /// Park the body after `frames`, until the session is told to release it.
    pub fn stall_after(mut self, frames: usize) -> Self {
        self.stall_after = Some(frames);
        self
    }

    /// Append one controlled source frame, carrying its own drop witness.
    pub fn frame(mut self, bytes: &[u8]) -> Self {
        self.frames.push(bytes::Bytes::from_owner(WitnessedFrame {
            bytes: bytes.into(),
            witness: Arc::clone(&self.source_drops),
        }));
        self
    }

    /// Append a whole body split into frames of at most `size` bytes.
    ///
    /// A `size` of zero is a mistake in the case, not a request for one-byte
    /// frames: coercing it would silently turn a frame-count claim into a claim
    /// about a different body.
    pub fn frames_of(mut self, body: &[u8], size: usize) -> Self {
        assert!(size > 0, "frames_of requires a size of at least one byte");
        for chunk in body.chunks(size) {
            self = self.frame(chunk);
        }
        self
    }

    /// End the controlled body with a transport failure.
    pub fn transport_failure(mut self, message: &str) -> Self {
        self.failure = Some(message.into());
        self
    }

    /// The effective admitted maximum this session reads under.
    pub fn body_limit(mut self, bytes: usize) -> Self {
        self.body_limit = bytes;
        self
    }

    /// Retain an admission permit whose release this session observes.
    pub fn with_permit(mut self) -> Self {
        self.permit = true;
        self
    }

    /// Start the production driver and hand back the access handle.
    ///
    /// The observer is this session's own, registered to no listener: it carries
    /// the production counters the driver publishes through, and every
    /// checkpoint runs straight through because nothing armed one.
    pub fn start(self) -> MultipartSession {
        let permit_drops = Arc::new(AtomicUsize::new(0));
        let admitted = admitted_controlled_body(
            self.body_limit,
            self.permit
                .then(|| WitnessedPermit(Arc::clone(&permit_drops))),
        );
        let observer = Arc::new(LifecycleScript::new());
        let gate = Arc::new(ReleaseGate::default());
        let body = ControlledBody {
            frames: self.frames.into(),
            failure: self.failure,
            stall_after: self.stall_after,
            handed: 0,
            gate: Arc::clone(&gate),
        };
        let (stream, revocation, driver) = super::multipart::open(
            body,
            admitted,
            &self.boundary,
            self.limits,
            Some(Arc::clone(&observer)),
            // No admitted operation stands behind a controlled session, so it
            // carries no deadline, no cancellation, and no peer lifetime: the
            // claims these cases make are the parser's, the budget's, and the
            // command protocol's.
            super::transfer::TransferOwner::detached(
                super::transfer::TransferDirection::Upload,
                super::TransferBudget::unbounded(),
            ),
        );
        MultipartSession {
            stream: Some(stream),
            revocation,
            driver: tokio::spawn(driver.run()),
            observer,
            gate,
            source_drops: self.source_drops,
            permit_drops,
        }
    }
}

/// Build the admitted body one controlled session reads under.
fn admitted_controlled_body(
    limit: usize,
    permit: Option<WitnessedPermit>,
) -> super::body_admission::AdmittedBody {
    let admission = match permit {
        Some(probe) => super::BodyAdmission::with_permit(limit, probe),
        None => super::BodyAdmission::new(limit),
    };
    super::body_admission::AdmittedBody {
        limit,
        permit: admission.into_permit(None),
    }
}

/// One running controlled multipart session.
///
/// It owns the access handle, the coordinator's revocation, and the driver task.
/// Dropping it releases the handle and stops the driver, so a case that fails or
/// panics still leaves nothing running.
#[doc(hidden)]
pub struct MultipartSession {
    stream: Option<super::MultipartStream>,
    revocation: super::multipart::MultipartRevocation,
    driver: tokio::task::JoinHandle<super::multipart::MultipartTerminal>,
    observer: Arc<LifecycleScript>,
    gate: Arc<ReleaseGate>,
    source_drops: Arc<AtomicUsize>,
    permit_drops: Arc<AtomicUsize>,
}

impl MultipartSession {
    /// The handler-facing access handle, while this session still holds it.
    pub fn stream(&mut self) -> Option<&mut super::MultipartStream> {
        self.stream.as_mut()
    }

    /// Take the access handle, so a case can move it, hold it, or drop it.
    pub fn take_stream(&mut self) -> Option<super::MultipartStream> {
        self.stream.take()
    }

    /// Close command admission the way handler completion does.
    pub fn revoke(&self) {
        self.revocation.revoke();
    }

    /// Let a parked controlled body hand out the rest of its frames.
    pub fn release_body(&self) {
        self.gate.record();
        self.gate.wake();
    }

    /// What this session has done so far.
    ///
    /// The freed-backing counts are this fixture's own witnesses: they fire when
    /// the backing allocation is released, which is the claim a chunk that
    /// borrowed instead of copying would fail.
    pub fn observed(&self) -> MultipartObservation {
        MultipartObservation::of(
            self.observer.multipart(),
            Some(self.source_drops.load(Ordering::Acquire)),
            Some(self.permit_drops.load(Ordering::Acquire)),
        )
    }

    /// Wait until the driver has published at least `count` replies.
    pub async fn wait_for_replies(&self, count: usize) {
        self.observer.multipart().wait_for_replies(count).await;
    }

    /// Release the handle, join the driver, and report what it ended as.
    ///
    /// A case that moved the handle elsewhere must drop it first: the driver
    /// stops when no handle can issue another command.
    pub async fn finish(mut self) -> Result<MultipartOutcome, RuntimeError> {
        self.stream = None;
        self.release_body();
        let terminal = (&mut self.driver).await.map_err(Self::unjoined)?;
        Ok(MultipartOutcome::of(terminal, self.observed()))
    }

    /// Name one driver task that produced no terminal summary.
    ///
    /// A canceled task is not a panicked one: dropping a session aborts its
    /// driver, and reporting that as a panic sends a case looking for a payload
    /// that never existed.
    fn unjoined(error: tokio::task::JoinError) -> RuntimeError {
        match error.is_cancelled() {
            true => RuntimeError::Cancelled,
            false => RuntimeError::TaskPanicked(error.to_string().into()),
        }
    }
}

impl Drop for MultipartSession {
    fn drop(&mut self) {
        self.stream = None;
        self.release_body();
        self.driver.abort();
    }
}
