use super::Method;
use super::Response;
pub use super::operation::{InboundTerminal, OperationStage};
use super::response::HeaderPair;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
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
/// other result provoke the poll that observes both.
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
    /// Record the release. Nothing is woken.
    fn record(&self) {
        self.released.store(true, Ordering::Release);
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
    /// What this listener's admitted operations published about their one
    /// envelope each.
    operation: OperationObservations,
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
            operation: OperationObservations::default(),
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
        let nanos = total.map_or(UNBOUNDED_TOTAL_NANOS, |total| {
            u64::try_from(total.as_nanos()).unwrap_or(UNBOUNDED_TOTAL_NANOS)
        });
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

    /// Record one paused checkpoint's release without waking what waits there.
    ///
    /// The held future stays parked and observes the release on whatever poll
    /// something else provokes. That is the only way to stage two results into
    /// one turn of a `select!`: [`Self::release`] wakes the future it releases,
    /// so the turn is decided before the second result exists, and a precedence
    /// rule between two ready results is never exercised.
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
            // Zero is "nothing recorded yet", not a configured total: every
            // finite total is validated above zero before a policy can hold it.
            total_from_admission: (total != UNBOUNDED_TOTAL_NANOS && total > 0)
                .then(|| std::time::Duration::from_nanos(total)),
            distinct_totals: operation.distinct_totals.load(Ordering::Acquire),
            dispatch: operation.dispatch.load(Ordering::Acquire),
            middleware: operation.middleware.load(Ordering::Acquire),
            body: operation.body.load(Ordering::Acquire),
            response_head: operation.response_head.load(Ordering::Acquire),
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
