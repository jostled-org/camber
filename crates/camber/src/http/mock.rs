use super::Method;
use super::Response;
use super::boundary::CrossedBound;
use super::completion::{ABSENT_LABEL, ConnectionEnd, DeliveryOutcome, ShutdownObservation};
use super::method::RequestMethod;
pub use super::operation::{InboundTerminal, OperationStage};
use super::rejection::{RejectionKind, RejectionProtocol};
use super::response::HeaderPair;
pub use super::response_commitment::{ResponseCommit, ResponseOrigin};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use crate::RuntimeError;

/// Every name a completion label may carry, as production spells them.
///
/// A test seam, not API, on the same footing as [`BlockingWorkerEdge`]. It is
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
    /// Every producer a completion can name, including stated absence.
    pub origins: Box<[&'static str]>,
    /// Every typed rejection a completion can name, including stated absence.
    pub rejections: Box<[&'static str]>,
    /// Every delivery outcome a completed operation can be recorded under.
    pub deliveries: Box<[&'static str]>,
    /// Every connection end a completion can name, including stated absence.
    pub connection_ends: Box<[&'static str]>,
    /// Every configured bound a completion can name, including stated absence.
    pub boundaries: Box<[&'static str]>,
    /// Every shutdown observation a completion can name, including absence.
    pub shutdowns: Box<[&'static str]>,
}

/// The closed vocabulary production records completions under.
#[doc(hidden)]
pub fn completion_vocabulary() -> CompletionVocabulary {
    CompletionVocabulary {
        methods: RequestMethod::vocabulary(),
        protocols: RejectionProtocol::vocabulary(),
        origins: ResponseOrigin::vocabulary(),
        rejections: rejection_vocabulary(),
        deliveries: DeliveryOutcome::vocabulary(),
        connection_ends: ConnectionEnd::vocabulary(),
        boundaries: CrossedBound::vocabulary(),
        shutdowns: ShutdownObservation::vocabulary(),
    }
}

/// Every typed rejection a completion label may carry, including absence.
///
/// Built from the public taxonomy rather than beside it, so a category added
/// there reaches the vocabulary a scraped label is checked against. The absence
/// is the common case: only a framework origin names a typed rejection at all.
fn rejection_vocabulary() -> Box<[&'static str]> {
    std::iter::once(ABSENT_LABEL)
        .chain(RejectionKind::ALL.map(RejectionKind::label))
        .collect()
}

/// Every edge one offloaded blocking worker can be held at.
///
/// Owner-local and nothing more. A controller holding one of these stops that
/// worker where the edge names; it cannot choose a maximum, resolve a path,
/// measure a file, sample a stack, retain a byte, or decide which thread
/// anything runs on.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockingWorkerEdge {
    /// One static-file worker has begun, before it has touched the filesystem.
    StaticFileWorkerEntered,
    /// One static-file worker has taken the filesystem's word for how large its
    /// file is, and has not opened it yet.
    StaticFileMetadataObserved,
    /// One profiling worker has begun, before it has sampled anything.
    #[cfg(feature = "profiling")]
    ProfilingWorkerEntered,
}

/// Every fault one connection owner's admission can be given.
///
/// Owner-local: the accept this listener's connection owners come in through is
/// the only thing it can fail. It cannot panic a task, cancel one, or unwind the
/// supervisor, so a case built on it proves what a failed admission does rather
/// than what a failed owner does.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionFault {
    /// The next accept returns this error kind instead of a socket.
    Accept(std::io::ErrorKind),
}

/// Every fault one server task can be given.
///
/// Owner-local on the other side of the same script: these are the ways an owned
/// task or the supervisor's own core can end badly. None of them fails an
/// accept, and none of them arms, releases, or observes an edge, so a fault view
/// over this vocabulary decides no schedule.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerTaskFault {
    /// The next owned task panics with a named payload.
    PanicNextOwnedTask,
    /// The next owned task panics with a payload nothing can name.
    PanicNextOwnedTaskOpaque,
    /// The next owned task is aborted the instant it is registered.
    CancelNextOwnedTask,
    /// The supervisor's own core unwinds.
    PanicSupervisorCore,
}

/// One armed fault, whichever owner's vocabulary named it.
///
/// One script holds at most one fault, so the two vocabularies share a single
/// slot rather than one each: a second fault armed while one is unconsumed is
/// refused whether or not it belongs to the same owner, and no owner's fault can
/// be taken by an owner it does not belong to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ArmedFault {
    Connection(ConnectionFault),
    ServerTask(ServerTaskFault),
}

impl ArmedFault {
    /// Whether the supervisor has to be woken to observe this fault.
    ///
    /// A failed accept and an unwound core are read by the supervisor's own
    /// pass, which may already be parked on a listener that will never speak
    /// again. Every other fault is read by the task it faults, which is polled
    /// by the work that reaches it.
    fn wakes_supervisor(self) -> bool {
        matches!(
            self,
            Self::Connection(ConnectionFault::Accept(_))
                | Self::ServerTask(ServerTaskFault::PanicSupervisorCore)
        )
    }
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

/// Every edge a server-stop owner can be held at.
///
/// Owner-local and nothing more. A controller holding one of these stops the
/// owner where the edge names; it cannot submit an event, choose a phase, mint a
/// deadline, or fix a result. That is what makes a case built on it a proof
/// about commit order rather than a staged poll order.
///
/// The two commit edges are the linearization point itself. The rest are the
/// supervisor's own passes over the stop it is carrying out — taking its next
/// event, acting on the one it took, waiting on the runtime signal, and handing
/// the flat result back — which is the same owner, held at a different moment.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerStopEdge {
    /// The owner is about to take the stop state's lock.
    BeforeCommit,
    /// The owner has committed and has not yet acted on the transition.
    AfterCommit,
    /// The supervisor is about to take its next event.
    BeforeSupervisorSelect,
    /// The supervisor took the one aggregate deadline expiring.
    SupervisorSelectedDeadline,
    /// The supervisor took a published control transition.
    SupervisorSelectedControl,
    /// The supervisor took the runtime's shutdown signal.
    SupervisorSelectedRuntime,
    /// The supervisor took an accepted socket.
    SupervisorSelectedAccept,
    /// The supervisor took a connection permit.
    SupervisorSelectedPermit,
    /// The supervisor took one owned connection's completion.
    SupervisorSelectedTask,
    /// The supervisor is about to wait on the runtime's shutdown signal.
    BeforeRuntimeWait,
    /// The supervisor has published the flat result and has not returned yet.
    AfterSupervisorResultSend,
}

/// Every edge one connection owner can be held at.
///
/// Owner-local: a controller holding one of these stops that connection where
/// the edge names. It cannot admit a socket, take a permit, choose a header
/// bound, size a buffer, transfer a child, or settle anything. The three
/// configuration edges are the connection's own resolved bounds, read at the
/// instant the owner resolves them.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionOwnerEdge {
    /// The server took a socket off the listener and has not answered the
    /// connection limit for it yet.
    AfterAccept,
    /// The socket has answered the connection limit and has no owner yet.
    AfterPermit,
    /// The permit this socket needs was not free, so the wait suspended.
    PermitWaitPending,
    /// The header bound this connection owner will serve under.
    HeaderTimeoutConfigured(std::time::Duration),
    /// The connection owner's future returned and it has not settled yet.
    AfterConnectionFutureCompleted,
    /// The queue depth one server-sent-events response was built over.
    SseBufferConfigured(usize),
    /// The queue depth one bridge's outbound direction was built over.
    WebSocketOutgoingBufferConfigured(usize),
    /// The queue depth one bridge's inbound direction was built over.
    WebSocketIncomingBufferConfigured(usize),
}

/// Every edge one upgrade child can be held at.
///
/// Owner-local, and deliberately only the four moments the handoff has: the
/// child was offered to its connection, the connection is about to answer, the
/// connection has taken the child and has not answered yet, and the peer went
/// away. A controller holding one of these cannot admit, refuse, commit,
/// cancel, or join the upgrade.
#[doc(hidden)]
#[cfg(feature = "ws")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UpgradeOwnerEdge {
    /// The handler offered its bridge to the connection owner and is waiting to
    /// hear whether the connection took it.
    AfterHandoffSubmitted,
    /// The connection owner is about to answer the offer.
    BeforeTransferAcknowledge,
    /// The connection owner has taken the child and recorded the transfer, and
    /// the handler is still waiting for the answer that releases its `101`.
    ///
    /// The only edge from which the record can be read while the peer provably
    /// cannot have seen the acknowledgement: it sits after the transfer is
    /// published and before the answer that lets the handler produce a
    /// response. A hold at [`Self::BeforeTransferAcknowledge`] is upstream of
    /// the transfer, so it can prove nothing about it.
    AfterTransferRecorded,
    /// The bridge has closed its callback's endpoints and fixed the one join
    /// deadline, and has not started waiting on the callback yet.
    ///
    /// The only edge from which a later server transition can be released into
    /// a deadline that is already fixed. A hold before the endpoint close would
    /// change which row of the table the deadline came from; a hold after the
    /// wait began would arrive too late to be a later transition at all.
    BeforeCallbackJoin,
    /// The upgraded transport's peer closed its half.
    PeerClosed,
}

/// Every edge one direct WebSocket direction owner can be held at.
///
/// Owner-local, and every edge belongs to exactly one direction: the outbound
/// owner's two write moments and the inbound owner's two read moments. A
/// controller holding one of these stops that direction where the edge names.
/// It cannot admit a message, build or decode a frame, close a queue, or fix
/// this connection's cause — the terminal fact belongs to
/// [`WebSocketTerminalEdge`], which this vocabulary cannot name.
#[doc(hidden)]
#[cfg(feature = "ws")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebSocketDirectionEdge {
    /// The outbound owner is about to write whatever it is holding.
    BeforeOutboundWrite,
    /// The outbound owner built one admitted message's transport frame, and the
    /// sink has not taken it yet.
    ///
    /// The one moment the converted frame lives on the write future's own
    /// stack rather than in the owner's held slot, which is what makes it the
    /// edge a payload-release row reads.
    OutboundFrameBuilt,
    /// The inbound owner took one item off the transport and has not read what
    /// it is.
    ///
    /// Ahead of what the item turns out to be, because a message, a close, and
    /// a failed transport are three answers to the same read and an edge that
    /// held only some of them could not stage the others.
    InboundFrameArrived,
    /// The inbound owner placed one peer message in the receive queue.
    InboundFrameQueued,
}

/// Every edge one direct WebSocket's terminal owner can be held at.
///
/// The two commit edges are this bridge's linearization point against its
/// server's causal stop state: a controller can stop the coordinator before or
/// after it fixes this connection's one cause. It cannot offer a cause, rank
/// one, or choose the disposition the cause decides, so a case built on it
/// proves commit order rather than staging one. The third edge is the graceful
/// settlement's own pass over the close it still owes the peer.
#[doc(hidden)]
#[cfg(feature = "ws")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WebSocketTerminalEdge {
    /// One owner has offered a cause and the coordinator has not committed it.
    ///
    /// The only edge from which a server stop can be released into a bridge
    /// that is already holding another answer, which is what separates a commit
    /// that reads the shared stop state from one that trusts its own poll.
    BeforeCommit,
    /// This connection's one cause is committed, and neither direction has
    /// settled yet.
    AfterCommit,
    /// The graceful settlement is about to wait for the peer's close.
    BeforePeerCloseAwait,
}

/// Every edge one admitted operation's response commitment can be held at.
///
/// Owner-local: a controller holding one of these stops the producer that
/// reached it. It cannot offer a cause, name an origin, map a rejection, or take
/// the commitment on any owner's behalf, so a case built on it proves which
/// producer reached the cell first rather than deciding that for production.
///
/// The two commit edges are the linearization point. The rest are the bounds and
/// heads the producers around that point resolve, read at the instant their own
/// owner resolves them.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResponseCommitmentEdge {
    /// One producer is about to attempt this operation's response commitment.
    BeforeResponseCommit,
    /// One producer's attempt has settled and the answer has not left yet.
    ///
    /// The only edge from which a later cause can be released into a commitment
    /// that is already taken, which is what separates a commit order from a
    /// scheduling order. It is reached by every settled attempt, whether the
    /// producer took the cell or found it held.
    AfterResponseCommit,
    /// One named cause took this operation's commitment.
    ///
    /// [`Self::AfterResponseCommit`] for a case that has more than one cause in
    /// flight and must wait for its own. A cause that reached a cell another
    /// producer already held never arrives here — being late is exactly what it
    /// has to prove — so a wait on this edge is a wait on a commit that won.
    CauseCommitted(InboundTerminal),
    /// The budgets one admitted head resolved to, after every containing layer
    /// narrowed them. Read from the routing owner that resolves them, at the
    /// instant it does.
    RouteBudgetsResolved {
        request: super::RequestBudget,
        upload: super::TransferBudget,
        download: super::TransferBudget,
    },
    /// The byte maximum route-aware admission resolved for one request body.
    RequestBodyLimitConfigured(usize),
    /// One request body's crossing frame was observed and has not been reported.
    RequestBodyLimitObserved,
    /// A streaming upstream has produced a response head Camber has not
    /// committed.
    UpstreamHeadReady,
    /// The forwarding upload confirmed it stopped, after the head was committed.
    UploadQuiesced,
    /// The committed upstream head is quiesced and the downstream response has
    /// not been built.
    BeforeDownstreamCommit,
    /// Tonic has produced a response head Camber has not committed.
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
}

/// Every edge one transfer owner can be held at.
///
/// Owner-local, and only this direction's own two moments: before it reads its
/// source, and after that read, before it commits the one terminal it will end
/// on. A controller holding one of these cannot supply a frame, charge a byte,
/// mint a deadline, or choose the terminal.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransferOwnerEdge {
    /// One transfer owner is about to read its source.
    ///
    /// Held here, a case makes every source it wants observable ready before
    /// the turn that reads them begins, including the frame the source holds.
    BeforeSourcePoll,
    /// One transfer owner has read its source and has not committed the
    /// terminal that read decided.
    BeforeTerminalCommit,
}

/// Every edge one streaming-multipart session owner can be held at.
///
/// Owner-local, and exactly the five moments the session has: a command taken,
/// the ingress advanced, a reply published, the handler returned, and the driver
/// stopped — plus the response the route owes when those are done. A controller
/// holding one of these submits no command, parses no part, and selects no
/// response.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MultipartOwnerEdge {
    /// The session took one handler command off its queue.
    CommandAccepted,
    /// The session advanced its ingress past one parsed part.
    IngressAdvanced,
    /// The session published one reply to the handler that asked for it.
    ReplyPublished,
    /// The multipart handler returned.
    HandlerCompleted,
    /// The multipart driver stopped.
    DriverTerminated,
    /// The route is about to choose the response this session produced.
    BeforeResponseSelection,
}

/// One place production can be held, whichever vocabulary named it.
///
/// The gate, the arm/release protocol, and the poll accounting are one
/// mechanism, so they are keyed by one type. Every arm is one owner's own
/// vocabulary, so a controller minted for that owner reaches its family and no
/// other: there is no arm left that spans owners, and none a case could name to
/// reach across them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PauseKey {
    ServerStop(ServerStopEdge),
    ConnectionOwner(ConnectionOwnerEdge),
    #[cfg(feature = "ws")]
    UpgradeOwner(UpgradeOwnerEdge),
    #[cfg(feature = "ws")]
    WebSocketDirection(WebSocketDirectionEdge),
    #[cfg(feature = "ws")]
    WebSocketTerminal(WebSocketTerminalEdge),
    ResponseCommitment(ResponseCommitmentEdge),
    TransferOwner(TransferOwnerEdge),
    Multipart(MultipartOwnerEdge),
    BlockingWorker(BlockingWorkerEdge),
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum CheckpointPhase {
    Armed,
    Paused,
    Released,
}

struct CheckpointState {
    key: PauseKey,
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

    /// Record the release and wake whatever waits here.
    ///
    /// Both halves an owner is owed when the release is not staged, spelled
    /// once. A caller that recorded without waking would leave a held future
    /// parked on a release nothing will make it look for.
    ///
    /// [`LifecycleScript::release_checkpoint`] is not a caller. Its record half
    /// happens under the checkpoint state lock, inside
    /// [`LifecycleScript::record_release`], and the wake is deliberately left
    /// outside it; this pairs the two for the callers that hold no such lock.
    fn release(&self) {
        self.record();
        self.wake();
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
    /// parked future, so [`LifecycleScript::wait_until_paused`] needs no
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

struct ScriptState {
    closed: bool,
    checkpoints: Vec<CheckpointState>,
    fault: Option<ArmedFault>,
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

/// What one listener's request-body owners published so far.
///
/// [`BodyObservations`] as a reader sees it. Read-only, and every field is
/// written by the production owner it names — the collector that polled a
/// frame, the admission that retained bytes, and the permit owner that reached
/// its drop. Nothing here polls a body, retains a byte, chooses a maximum, or
/// releases a permit.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct RequestBodyObservation {
    /// Payload frames this listener's collectors polled out.
    pub frames_polled: usize,
    /// The most bytes any one request on this listener retained at once.
    pub peak_retained_bytes: usize,
    /// How many admitted permit owners reached their drop.
    pub permit_owners_dropped: usize,
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

/// [`CollectionObservations`] as a reader sees it.
///
/// Read-only, and every field is written by the production collector's own
/// decision to poll a chunk or keep one. Nothing here chooses a maximum, retains
/// a byte, drops a frame, or selects a terminal.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct CollectionObservation {
    /// Chunks the collector was handed, counted before it accounted for them.
    pub chunks_polled: usize,
    /// The most any one collection under this scope retained at once.
    pub peak_retained_bytes: usize,
    /// What the first chunk this scope ever retained left behind. Zero means no
    /// chunk has been retained yet.
    pub first_retained_bytes: usize,
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

    /// Hold this worker at `edge` for as long as a case holds it there.
    pub(in crate::http) fn hold_at(&self, edge: BlockingWorkerEdge) {
        LifecycleScript::pause_blocking(self.script(), edge);
    }

    /// Run `work` as this worker's whole answer, announced at both ends.
    ///
    /// The order is the protocol, and it is stated here once. The entry is
    /// published before the hold, so a case that parks this worker at `edge`
    /// has already seen it begin. The return is published after the
    /// work, so the gap between the two counts is exactly what a caller that
    /// stopped waiting left behind. The two events are the owner's own, because
    /// what a static-file entry reports and what a profiling entry reports are
    /// not the same claim.
    pub(in crate::http) fn spanning<E: BlockingWorkerEvent, T>(
        &self,
        entered: E,
        returned: E,
        edge: BlockingWorkerEdge,
        work: impl FnOnce() -> T,
    ) -> T {
        self.publish(entered);
        self.hold_at(edge);
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
    /// How many times the identity above changed.
    ///
    /// A change count, not a set: one identity read by four owners is what the
    /// carry claim needs, and a second envelope reaching any owner shows up here
    /// as two whichever owner saw it. Two identities read in turn count every
    /// switch, because only the last one is held.
    distinct_identities: AtomicUsize,
    /// The request-total deadline the mint computed, as its offset from
    /// admission, in nanoseconds. `u64::MAX` is an unbounded total.
    total_from_admission: AtomicU64,
    /// How many times the total above changed, on the same terms.
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
    /// How many policies this direction froze, beside the three values above.
    ///
    /// The values alone cannot say whether they were ever written: an unbounded
    /// maximum and a direction that froze nothing both read as the sentinel. The
    /// count is what separates them, on the same terms as `terminals` beside the
    /// set-once `terminal`.
    policies_frozen: AtomicUsize,
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
                &self.policies_frozen
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
            policies_frozen: self.policies_frozen.load(Ordering::Acquire),
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

/// What one listener's response commitments have published to.
///
/// Written only by [`super::response_commitment::OperationCommitment`], on
/// every attempt: the cell it settled on, how many producers reached it, and
/// how many of those found it already taken.
#[derive(Default)]
struct CommitmentObservations {
    committed: Mutex<Option<ResponseCommit>>,
    attempts: AtomicUsize,
    commits: AtomicUsize,
    late: AtomicUsize,
    /// The identity of the last operation to take a commitment, and how many
    /// times that identity changed.
    operation: AtomicU64,
    distinct: AtomicUsize,
}

/// What one listener's response commitments settled on.
///
/// Read-only. Every field is written by the one production cell it names, and
/// the separation of `commits` from `attempts` is the set-once claim itself: a
/// second producer reaching a taken cell is counted, not silently dropped.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResponseCommitmentObservation {
    /// The fact the most recent commitment settled on.
    pub committed: Option<ResponseCommit>,
    /// How many producers reached a commitment at all.
    pub attempts: usize,
    /// How many of those took the cell.
    pub commits: usize,
    /// How many of those found it already taken.
    pub late: usize,
    /// The identity of the last operation to take a commitment.
    pub operation: u64,
    /// How many times the operation recorded above changed.
    ///
    /// A change count, not a set count. The cell holds the last identity only,
    /// so two operations taking commitments in turn count every switch between
    /// them. Read this against one request at a time, which is what the claim is
    /// about.
    pub distinct_operations: usize,
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
    /// How many policies this direction froze.
    ///
    /// What says the three values above were written at all. A direction that
    /// never froze a policy reports the same `None` an unbounded one does, so a
    /// row claiming an unbounded budget reads this beside it.
    pub policies_frozen: usize,
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
/// at two whichever owner saw it.
///
/// It counts changes, not distinct values. A run that reads A, then B, then A
/// again counts three: the cell holds the last value only, so nothing here
/// remembers that A was already seen. The claim it supports is per admitted
/// request, and the two readings agree there, so a case that wants it drives one
/// request at a time against its own listener.
fn count_changes(last: &AtomicU64, changes: &AtomicUsize, value: u64) {
    match last.swap(value, Ordering::AcqRel) {
        previous if previous == value => {}
        _ => {
            changes.fetch_add(1, Ordering::Release);
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
    /// How many times the identity recorded here changed.
    ///
    /// A change count, not a set count. Four owners reading one identity leave
    /// it at one, and a second envelope leaves it at two. A run that interleaves
    /// two identities counts every switch between them, so read this against one
    /// request at a time — which is what the claim is about.
    pub distinct_identities: usize,
    /// The request-total deadline, as the offset from admission it was computed
    /// at. `None` is an unbounded total.
    pub total_from_admission: Option<std::time::Duration>,
    /// How many times the request total recorded here changed, on the same
    /// terms as `distinct_identities`.
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
    /// How every retained callback on this listener was disposed of.
    ///
    /// A list rather than one slot, because a listener serves many bridges and
    /// a case reading only the last would report the one that happened to
    /// settle last as the only one there was.
    callbacks: Mutex<Vec<WebSocketCallbackObservation>>,
}

/// One decision a direct bridge published about the callback it retained.
///
/// Three moments reach this record and no others: the endpoint close that fixes
/// the join deadline, a later transition that brings that deadline forward, and
/// the disposition the join ended at. Every value is written by the production
/// decision it names — nothing here waits, joins, moves a deadline, or chooses
/// a disposition — and appending rather than overwriting is what lets a case
/// read the whole history and say the deadline never moved later.
#[cfg(feature = "ws")]
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WebSocketCallbackObservation {
    /// The connection that owns the upgrade this callback belongs to.
    ///
    /// The same opaque per-run identity
    /// [`ConnectionOwnershipEvent::ConnectionUpgradeTransferred`] names, written
    /// by the bridge rather than by the connection that recorded the transfer.
    /// Two independent writers of one identity is what makes a callback's parent
    /// checkable instead of assumed.
    pub connection: u64,
    /// The upgrade child that started this callback.
    pub upgrade: u64,
    /// The transition committed when the bridge closed the callback's
    /// endpoints: `none`, `graceful`, `cancelled`, or `deadline-expired`.
    pub entered: &'static str,
    /// The instant the bridge closed the endpoints a blocked callback wakes on.
    pub endpoints_closed_at: tokio::time::Instant,
    /// The join deadline as it stood when this record was published.
    pub deadline: tokio::time::Instant,
    /// How the join ended, or `None` at every record published before it did.
    ///
    /// `completed` or `outstanding-after-forced-grace`.
    pub disposition: Option<&'static str>,
    /// The transition the disposition reported, or `None` before it.
    ///
    /// `none`, `graceful`, `cancelled`, or `deadline-expired`. Distinct from
    /// [`Self::entered`] for exactly one entry — a local terminal reports
    /// whatever the server committed while the join was waiting.
    pub shutdown: Option<&'static str>,
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

/// One admission, transfer, or settlement in the framework-owned parent-child
/// tree, as the passive observer records it.
///
/// The identities are opaque and per-run: they say which parent contains which
/// child, and nothing about addresses, paths, or peers. The two legacy
/// server-scope upgrade events are named separately from the connection-scope
/// ones on purpose — the whole point of the observation is to make the
/// difference between a sibling registry and a connection-local child visible.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConnectionOwnershipEvent {
    ServerConnectionRegistered {
        connection: u64,
    },
    ServerConnectionSettled {
        connection: u64,
    },
    /// An upgrade registered beside its connection rather than beneath it.
    ServerUpgradeRegistered {
        upgrade: u64,
    },
    /// An upgrade that settled against the server registry it registered with.
    ServerUpgradeSettled {
        upgrade: u64,
    },
    ConnectionRequestAdmitted {
        connection: u64,
        request: u64,
    },
    ConnectionRequestSettled {
        connection: u64,
        request: u64,
    },
    ConnectionUpgradeTransferred {
        connection: u64,
        upgrade: u64,
    },
    ConnectionUpgradeSettled {
        connection: u64,
        upgrade: u64,
    },
}

/// What one listener's owner tree has registered, transferred, and settled.
///
/// Read-only and passive. Every event is written by the production mutation it
/// names. The observer cannot admit, cancel, release, transfer, or settle any
/// owner, and no production decision reads it.
#[doc(hidden)]
pub struct ConnectionOwnershipObservation {
    pub events: Box<[ConnectionOwnershipEvent]>,
}

impl ConnectionOwnershipObservation {
    /// Whether the tree ever registered `event`.
    pub fn contains(&self, event: ConnectionOwnershipEvent) -> bool {
        self.events.contains(&event)
    }
}

/// The passive owner-tree record one listener publishes into.
#[derive(Default)]
struct ConnectionOwnershipObservations {
    events: Mutex<Vec<ConnectionOwnershipEvent>>,
    /// The next opaque identity this run hands an owner.
    next_identity: AtomicU64,
}

pub(crate) struct LifecycleScript {
    state: Mutex<ScriptState>,
    supervisor_wake: tokio::sync::Notify,
    /// The last commit this listener's causal stop state published.
    ///
    /// One reading rather than a field per number: the state commits under one
    /// lock, so a copy taken there is internally consistent, and an observer
    /// that read six separately updated counters could not say the same.
    server_stop: Mutex<super::server_stop::CommittedStopReading>,
    /// What this listener's owner tree registered, transferred, and settled.
    ownership: ConnectionOwnershipObservations,
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
    /// What this listener's admitted operations committed as their one
    /// response, and how many producers reached that commitment.
    commitment: CommitmentObservations,
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
            server_stop: Mutex::new(super::server_stop::CommittedStopReading::default()),
            ownership: ConnectionOwnershipObservations::default(),
            body: BodyObservations::default(),
            collection: CollectionObservations::default(),
            static_files: StaticFileObservations::default(),
            #[cfg(feature = "profiling")]
            profiling: ProfilingObservations::default(),
            operation: OperationObservations::default(),
            commitment: CommitmentObservations::default(),
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
                count_changes(
                    &operation.total_from_admission,
                    &operation.distinct_totals,
                    nanos,
                );
                count_changes(
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
                count_changes(
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
    pub(in crate::http) fn hold_at_transfer(
        script: Option<&Self>,
        edge: TransferOwnerEdge,
    ) -> Option<CheckpointHold> {
        script
            .and_then(|script| script.reach(PauseKey::TransferOwner(edge)))
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

    /// Record one decision a direct bridge published about its retained
    /// callback.
    ///
    /// Appended in the order production reached them, and the last of them is
    /// the disposition — which is published before the connection permit is
    /// released, so a case reading a released permit with no disposition beside
    /// it has read the ordering violation rather than a race.
    #[cfg(feature = "ws")]
    pub(in crate::http) fn observe_ws_callback(
        script: Option<&Self>,
        decided: WebSocketCallbackObservation,
    ) {
        Self::observe_ws(script, |websocket| {
            websocket
                .callbacks
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(decided);
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

    /// Publish one commit of the causal stop state.
    ///
    /// Called from inside the stop state's own lock, so the reading an observer
    /// takes is the one that commit left rather than a blend of two.
    pub(in crate::http) fn observe_server_stop(
        script: Option<&Self>,
        reading: super::server_stop::CommittedStopReading,
    ) {
        Self::observe(
            script,
            |script| &script.server_stop,
            |published| {
                *published.lock().unwrap_or_else(|error| error.into_inner()) = reading;
            },
        );
    }

    fn server_stop_observed(&self) -> ServerStopObservation {
        let reading = *self
            .server_stop
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        reading_observation(&reading)
    }

    /// Hand one owner the opaque identity it is recorded under.
    ///
    /// Minted even with no controller registered, because an owner carries its
    /// identity for its whole life and a value that changed when an observer
    /// appeared would name two owners in one run. Identities start at one, so
    /// zero remains "no owner".
    pub(in crate::http) fn mint_owner_identity(script: Option<&Self>) -> u64 {
        match script {
            Some(script) => {
                script
                    .ownership
                    .next_identity
                    .fetch_add(1, Ordering::AcqRel)
                    + 1
            }
            None => 0,
        }
    }

    /// Record one admission, transfer, or settlement in the owner tree.
    ///
    /// Inert with no controller registered, exactly like [`Self::pause_at`].
    /// Passive throughout: nothing production does reads this record back.
    pub(in crate::http) fn observe_ownership(
        script: Option<&Self>,
        event: ConnectionOwnershipEvent,
    ) {
        Self::observe(
            script,
            |script| &script.ownership,
            |ownership| {
                ownership
                    .events
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .push(event);
            },
        );
    }

    fn ownership_observed(&self) -> ConnectionOwnershipObservation {
        let events = self
            .ownership
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        ConnectionOwnershipObservation {
            events: events.into_boxed_slice(),
        }
    }

    /// What this listener's direct WebSocket bridges have published so far.
    ///
    /// Record one producer's attempt at an operation's response commitment.
    ///
    /// Written by the commitment itself, on both outcomes: the attempt that
    /// took the cell and the attempt that found it taken are the two facts a
    /// set-once claim is made of. Inert with no controller registered.
    pub(in crate::http) fn observe_commitment(
        script: Option<&Self>,
        id: super::operation::OperationId,
        attempt: ResponseCommit,
        settled: &Result<(), ResponseCommit>,
    ) {
        Self::observe(
            script,
            |script| &script.commitment,
            |commitment| {
                commitment.attempts.fetch_add(1, Ordering::Release);
                match settled {
                    Ok(()) => {
                        commitment.commits.fetch_add(1, Ordering::Release);
                        count_changes(&commitment.operation, &commitment.distinct, id.value());
                        let mut held = commitment
                            .committed
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner());
                        *held = Some(attempt);
                    }
                    Err(_committed) => {
                        commitment.late.fetch_add(1, Ordering::Release);
                    }
                }
            },
        );
    }

    /// What this listener's response commitments have settled on so far.
    fn commitment_observed(&self) -> ResponseCommitmentObservation {
        let commitment = &self.commitment;
        ResponseCommitmentObservation {
            committed: *commitment
                .committed
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
            attempts: commitment.attempts.load(Ordering::Acquire),
            commits: commitment.commits.load(Ordering::Acquire),
            late: commitment.late.load(Ordering::Acquire),
            operation: commitment.operation.load(Ordering::Acquire),
            distinct_operations: commitment.distinct.load(Ordering::Acquire),
        }
    }

    /// What this listener's request-body owners have published so far.
    ///
    /// Stated once here rather than at each controller that reads it, so the
    /// broad observer and the narrow request-body owner cannot answer
    /// differently about one payload.
    fn body_observed(&self) -> RequestBodyObservation {
        let body = &self.body;
        RequestBodyObservation {
            frames_polled: body.frames_polled.load(Ordering::Acquire),
            peak_retained_bytes: body.peak_retained_bytes.load(Ordering::Acquire),
            permit_owners_dropped: body.permit_owners_dropped.load(Ordering::Acquire),
        }
    }

    /// What this scope's checked collections have published so far.
    ///
    /// Stated once here rather than at each controller that reads it, on the
    /// same terms as the payload above. One reader, so no two callers can answer
    /// differently about one collection. One snapshot, so the three values
    /// describe a single instant rather than one instant per getter.
    fn collection_observed(&self) -> CollectionObservation {
        let collection = &self.collection;
        CollectionObservation {
            chunks_polled: collection.chunks_polled.load(Ordering::Acquire),
            peak_retained_bytes: collection.peak_retained_bytes.load(Ordering::Acquire),
            first_retained_bytes: collection.first_retained_bytes.load(Ordering::Acquire),
        }
    }

    /// What this listener's admitted operations have published so far.
    ///
    /// Stated once here rather than at each controller that reads it, so the
    /// broad observer and the narrow response-commitment owner cannot answer
    /// differently about one operation.
    fn operation_observed(&self) -> OperationObservation {
        let operation = &self.operation;
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
    /// Stated once here rather than at each controller that reads it, so the
    /// broad observer and the narrow transfer owner cannot answer differently
    /// about one direction.
    fn transfer_observed(&self) -> TransferObservation {
        TransferObservation {
            upload: self.transfers.upload.snapshot(),
            download: self.transfers.download.snapshot(),
        }
    }

    /// What this process's profiling workers have done so far.
    ///
    /// Read-only, and every number in it is written by the production owner it
    /// names: nothing here starts a worker, samples a stack, renders a byte,
    /// chooses a maximum, or decides the thread any of it runs on.
    #[cfg(feature = "profiling")]
    fn profiling_observed(&self) -> ProfilingObservation {
        let profiling = &self.profiling;
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
    fn static_files_observed(&self) -> StaticFileObservation {
        let files = &self.static_files;
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

    /// What this listener's streaming multipart sessions have published so far.
    ///
    /// Stated once here rather than at each controller that reads it, so the
    /// broad observer and the narrow session owner cannot answer differently
    /// about one session. A served listener owns none of the allocations behind
    /// its bodies, so it witnesses no freed backing and claims none.
    fn multipart_observed(&self) -> MultipartObservation {
        MultipartObservation::of(self.multipart(), None, None)
    }

    /// Stated once here rather than at each controller that reads it, so the
    /// broad observer and the narrow terminal owner cannot answer differently
    /// about one bridge.
    #[cfg(feature = "ws")]
    fn websocket_observed(&self) -> WebSocketDirectionObservation {
        let websocket = &self.websocket;
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

    fn invalid(message: &'static str) -> RuntimeError {
        RuntimeError::InvalidArgument(message.into())
    }

    fn arm(&self, key: PauseKey) -> Result<(), RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let result = match (
            state.closed,
            state
                .checkpoints
                .iter()
                .any(|entry| entry.key == key && entry.phase != CheckpointPhase::Released),
        ) {
            (true, _) => Err(Self::invalid("lifecycle controller is closed")),
            (false, true) => Err(Self::invalid("lifecycle checkpoint is already armed")),
            (false, false) => {
                state.checkpoints.retain(|entry| entry.key != key);
                state.checkpoints.push(CheckpointState {
                    key,
                    phase: CheckpointPhase::Armed,
                    released: Arc::new(ReleaseGate::default()),
                });
                Ok(())
            }
        };
        drop(state);
        if result.is_ok() && key == PauseKey::ServerStop(ServerStopEdge::BeforeSupervisorSelect) {
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
    async fn wait_until_paused(&self, key: PauseKey) -> Result<(), RuntimeError> {
        loop {
            let gate = {
                let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
                let entry = state
                    .checkpoints
                    .iter()
                    .find(|entry| entry.key == key)
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
                        entry.key == key
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

    fn release_checkpoint(&self, key: PauseKey) -> Result<(), RuntimeError> {
        self.record_release(key)?.wake();
        Ok(())
    }

    /// Record one paused checkpoint's release, waking nothing.
    fn record_release(&self, key: PauseKey) -> Result<Arc<ReleaseGate>, RuntimeError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        let entry = state
            .checkpoints
            .iter_mut()
            .find(|entry| entry.key == key)
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

    fn arm_fault(&self, fault: ArmedFault) -> Result<(), RuntimeError> {
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
        if result.is_ok() && fault.wakes_supervisor() {
            self.supervisor_wake.notify_one();
        }
        result
    }

    /// Hold one server-stop owner at `edge`, and do nothing when no controller
    /// watches.
    ///
    /// [`Self::pause_at`] for the narrow owner-local vocabulary. Separate entry
    /// point rather than a shared one taking either key, because the caller is
    /// the production owner and naming a broad checkpoint from the stop state
    /// must not be possible.
    pub(in crate::http) async fn pause_at_stop(script: Option<&Self>, edge: ServerStopEdge) {
        Self::pause_at_key(script, PauseKey::ServerStop(edge)).await;
    }

    /// Hold one connection owner at `edge`, and do nothing when no controller
    /// watches.
    pub(crate) async fn pause_at_connection(script: Option<&Self>, edge: ConnectionOwnerEdge) {
        Self::pause_at_key(script, PauseKey::ConnectionOwner(edge)).await;
    }

    /// Hold one upgrade child at `edge`, and do nothing when no controller
    /// watches.
    #[cfg(feature = "ws")]
    pub(in crate::http) async fn pause_at_upgrade(script: Option<&Self>, edge: UpgradeOwnerEdge) {
        Self::pause_at_key(script, PauseKey::UpgradeOwner(edge)).await;
    }

    /// Hold one WebSocket direction owner at `edge`, and do nothing when no
    /// controller watches.
    #[cfg(feature = "ws")]
    pub(in crate::http) async fn pause_at_direction(
        script: Option<&Self>,
        edge: WebSocketDirectionEdge,
    ) {
        Self::pause_at_key(script, PauseKey::WebSocketDirection(edge)).await;
    }

    /// Hold one WebSocket terminal owner at `edge`, and do nothing when no
    /// controller watches.
    #[cfg(feature = "ws")]
    pub(in crate::http) async fn pause_at_ws_terminal(
        script: Option<&Self>,
        edge: WebSocketTerminalEdge,
    ) {
        Self::pause_at_key(script, PauseKey::WebSocketTerminal(edge)).await;
    }

    /// Hold one response producer at `edge`, and do nothing when no controller
    /// watches.
    pub(in crate::http) async fn pause_at_response_commit(
        script: Option<&Self>,
        edge: ResponseCommitmentEdge,
    ) {
        Self::pause_at_key(script, PauseKey::ResponseCommitment(edge)).await;
    }

    /// Hold one producer at both edges its settled commitment attempt reaches.
    ///
    /// Two edges and not one, because a case with more than one cause in flight
    /// cannot wait on a shared edge without racing the other cause onto it.
    /// `committed` is the cause that took the cell, and the named edge is
    /// reached only for one — so a wait on it is a wait on a commit that won,
    /// and a producer that arrived late passes straight through it.
    ///
    /// Stated once here rather than at each coordinator, so the body reader and
    /// the pre-commit coordinator cannot publish a commit differently.
    pub(in crate::http) async fn pause_at_settled_commit(
        script: Option<&Self>,
        committed: Option<InboundTerminal>,
    ) {
        Self::pause_at_response_commit(script, ResponseCommitmentEdge::AfterResponseCommit).await;
        match committed {
            Some(terminal) => {
                Self::pause_at_response_commit(
                    script,
                    ResponseCommitmentEdge::CauseCommitted(terminal),
                )
                .await;
            }
            None => {}
        }
    }

    /// Hold one multipart session owner at `edge`, and do nothing when no
    /// controller watches.
    pub(in crate::http) async fn pause_at_multipart(
        script: Option<&Self>,
        edge: MultipartOwnerEdge,
    ) {
        Self::pause_at_key(script, PauseKey::Multipart(edge)).await;
    }

    async fn pause_at_key(script: Option<&Self>, key: PauseKey) {
        match script {
            Some(script) => script.pause(key).await,
            None => {}
        }
    }

    async fn pause(&self, key: PauseKey) {
        match self.reach(key) {
            Some(released) => released.held().await,
            None => {}
        }
    }

    /// Hold one blocking worker at `edge` until a controller releases it.
    ///
    /// [`Self::pause_at`] for an owner that is not inside a poll. Inert with no
    /// controller registered, and inert for an edge nothing armed: the worker
    /// runs straight through both without parking.
    pub(in crate::http) fn pause_blocking(script: Option<&Self>, edge: BlockingWorkerEdge) {
        match script.and_then(|script| script.reach(PauseKey::BlockingWorker(edge))) {
            Some(released) => released.held_blocking(),
            None => {}
        }
    }

    /// Mark this checkpoint reached, and hand back the gate it now waits on.
    ///
    /// `None` is a checkpoint nothing armed, or a closed controller: production
    /// runs straight through both.
    fn reach(&self, key: PauseKey) -> Option<Arc<ReleaseGate>> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match state.closed {
            true => None,
            false => state
                .checkpoints
                .iter_mut()
                .find(|entry| entry.key == key && entry.phase == CheckpointPhase::Armed)
                .map(CheckpointState::pause),
        }
    }

    pub(crate) async fn wait_for_supervisor_wake(&self) {
        self.supervisor_wake.notified().await;
    }

    /// How many turns whatever waits at `key`'s edge has taken.
    ///
    /// An edge nothing armed is refused, the way every other lookup here
    /// refuses one. Payload-carrying variants match by value, so a case naming
    /// a limit it never armed would read a count of zero and pass every claim
    /// it made about turns without a turn ever being taken.
    fn edge_polls(&self, key: PauseKey) -> Result<usize, RuntimeError> {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state
            .checkpoints
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.released.polls())
            .ok_or_else(|| Self::invalid("lifecycle checkpoint is not armed"))
    }

    pub(crate) fn take_accept_fault(&self) -> Option<std::io::ErrorKind> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match state.fault {
            Some(ArmedFault::Connection(ConnectionFault::Accept(kind))) => {
                state.fault = None;
                Some(kind)
            }
            _ => None,
        }
    }

    pub(crate) fn take_owned_task_fault(&self) -> Option<ServerTaskFault> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match state.fault {
            Some(ArmedFault::ServerTask(
                fault @ (ServerTaskFault::PanicNextOwnedTask
                | ServerTaskFault::PanicNextOwnedTaskOpaque
                | ServerTaskFault::CancelNextOwnedTask),
            )) => {
                state.fault = None;
                Some(fault)
            }
            _ => None,
        }
    }

    pub(crate) fn take_supervisor_fault(&self) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        match state.fault {
            Some(ArmedFault::ServerTask(ServerTaskFault::PanicSupervisorCore)) => {
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
    released.release();
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

/// One watched scope, held for exactly as long as something reads it.
///
/// The registration is an owner in its own right rather than a field of the
/// hub: it is what the registry lists, what production resolves against, and
/// what releases the scope when it is dropped. Everything else — the hub's
/// broad vocabulary, and every scoped view — is built from one of these and
/// keeps it alive by holding it.
///
/// What it mints is one owner-local controller per family, each over the same
/// script. Minting one hands out that family's own vocabulary and nothing
/// else, which is what lets a scoped view name the families its case reads and
/// reach no others.
#[doc(hidden)]
pub struct ScopeRegistration {
    scope: ObservedScope,
    script: Arc<LifecycleScript>,
}

impl ScopeRegistration {
    /// The narrow controller for this listener's server-stop owner.
    ///
    /// Owner-local: it can hold that owner at either side of its commit into the
    /// causal stop state and read what that state has committed. It cannot arm a
    /// checkpoint outside the stop vocabulary, submit a stop event, choose a
    /// phase, or fix a result — the two vocabularies are separate types over one
    /// gate, so naming a broad checkpoint through it does not compile.
    pub fn server_stop(&self) -> ServerStopController {
        ServerStopController {
            script: Arc::clone(&self.script),
        }
    }

    /// The narrow controller for this listener's connection owners.
    ///
    /// Owner-local, on the same footing as [`Self::server_stop`]: it holds a
    /// connection where one of its own edges names and reads nothing else. It
    /// cannot admit a socket, take or release a permit, transfer a child, or
    /// settle an owner, and naming a broad checkpoint through it does not
    /// compile.
    pub fn connection_owner(&self) -> ConnectionOwnerController {
        ConnectionOwnerController {
            script: Arc::clone(&self.script),
        }
    }

    /// The narrow controller for this listener's upgrade children.
    ///
    /// Owner-local: it holds a handoff at one of the three moments it has, and
    /// carries no way to admit, refuse, commit, cancel, or join the upgrade.
    #[cfg(feature = "ws")]
    pub fn upgrade_owner(&self) -> UpgradeOwnerController {
        UpgradeOwnerController {
            script: Arc::clone(&self.script),
        }
    }

    /// The narrow controller for this listener's WebSocket direction owners.
    ///
    /// Owner-local: it holds one direction at one of its own read or write
    /// moments, and carries no way to admit a message, frame one, or say why
    /// the connection ended.
    #[cfg(feature = "ws")]
    pub fn websocket_direction(&self) -> WebSocketDirectionController {
        WebSocketDirectionController {
            script: Arc::clone(&self.script),
        }
    }

    /// The narrow controller for this listener's WebSocket terminal owners.
    ///
    /// Owner-local: it holds a bridge on either side of the commit that fixes
    /// its one cause and reads what that bridge committed. It cannot offer a
    /// cause or choose the disposition one decides.
    #[cfg(feature = "ws")]
    pub fn websocket_terminal(&self) -> WebSocketTerminalController {
        WebSocketTerminalController {
            script: Arc::clone(&self.script),
        }
    }

    /// The narrow controller for this listener's response producers.
    ///
    /// Owner-local: it holds one producer on either side of its attempt at an
    /// operation's response commitment, and reads the bounds and heads the
    /// producers around that attempt resolved. It maps no rejection, names no
    /// origin, and takes the commitment for nobody.
    pub fn response_commitment(&self) -> ResponseCommitmentController {
        ResponseCommitmentController {
            script: Arc::clone(&self.script),
        }
    }

    /// The narrow controller for this listener's transfer owners.
    ///
    /// Owner-local: it holds one direction before its source read or before the
    /// terminal that read decided is committed, and reads what the direction
    /// published. It supplies no frame and chooses no terminal.
    pub fn transfer_owner(&self) -> TransferOwnerController {
        TransferOwnerController {
            script: Arc::clone(&self.script),
        }
    }

    /// The narrow observer for this listener's request-body owners.
    ///
    /// Owner-local: it reports what the collectors polled, what admission
    /// retained, and how many permit owners let go. It holds nothing, because
    /// the request body has no edge of its own — the producers around it are
    /// held at [`Self::response_commitment`]'s — and it admits no frame,
    /// chooses no maximum, and releases no permit.
    pub fn request_body_owner(&self) -> RequestBodyOwnerController {
        RequestBodyOwnerController {
            script: Arc::clone(&self.script),
        }
    }

    /// The narrow controller for this listener's streaming-multipart sessions.
    ///
    /// Owner-local: it holds one session at one of the moments its own protocol
    /// has, and submits no command, parses no part, and selects no response.
    pub fn multipart_owner(&self) -> MultipartOwnerController {
        MultipartOwnerController {
            script: Arc::clone(&self.script),
        }
    }

    /// The narrow controller for this scope's offloaded blocking workers.
    ///
    /// Owner-local: it holds one worker at one of its own two moments and reads
    /// what that family of workers reported. It resolves no path, measures no
    /// file, samples no stack, chooses no maximum, and decides no thread.
    pub fn blocking_worker(&self) -> BlockingWorkerController {
        BlockingWorkerController {
            script: Arc::clone(&self.script),
        }
    }

    /// The narrow controller for this listener's server tasks.
    ///
    /// A fault vocabulary and nothing else: it decides how the next owned task
    /// or the supervisor's own core ends, and it arms no checkpoint, releases
    /// nothing, observes nothing, and settles nothing. Failing an accept belongs
    /// to the connection owner that comes in through it, so naming one here does
    /// not compile.
    pub fn server_task(&self) -> ServerTaskController {
        ServerTaskController {
            script: Arc::clone(&self.script),
        }
    }
}

impl Drop for ScopeRegistration {
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
fn register(scope: ObservedScope) -> Result<ScopeRegistration, RuntimeError> {
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
            Ok(ScopeRegistration { scope, script })
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

/// One registered listener scope, reachable only through the view it was taken
/// for.
///
/// It holds the registration itself rather than a hub built over one, so the
/// scope is watched for exactly this value's life and released with it and there
/// is no broad vocabulary retained behind the view. What it exposes is `View`
/// and nothing else, so a case that names one owner family cannot reach that
/// scope's server stop, connection permits, or response commitment on the way
/// past. A case that names more than one family takes the same single
/// registration with a named record of those families' controllers as its
/// `View`, so breadth is stated by what a case asked for rather than by handing
/// it the hub.
#[doc(hidden)]
pub struct ScopedOwner<View> {
    owner: View,
    /// The registration this observer's scope is watched under.
    ///
    /// Held rather than dropped after the view is taken: dropping it closes the
    /// script and unregisters the scope, and every later lookup by production
    /// would then find nothing to publish to.
    registered: ScopeRegistration,
}

impl<View> std::ops::Deref for ScopedOwner<View> {
    type Target = View;

    fn deref(&self) -> &Self::Target {
        &self.owner
    }
}

impl<View> std::fmt::Debug for ScopedOwner<View> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ScopedOwner")
            .field("scope", &self.registered.scope)
            .finish()
    }
}

/// Register `scope` and keep only what `view` takes from it.
///
/// The registration is the same one scope every observer of this endpoint,
/// root, or profiler shares — a scope answers to one script, so a second
/// registration for it is refused — and it is kept as the registration it is,
/// with no hub built over it for the view to reach back through.
fn scoped_owner<View>(
    scope: ObservedScope,
    view: impl FnOnce(&ScopeRegistration) -> View,
) -> Result<ScopedOwner<View>, RuntimeError> {
    register(scope).map(|registered| ScopedOwner {
        owner: view(&registered),
        registered,
    })
}

/// Register the listener at `addr` and keep only what `view` takes from it.
fn scoped_listener<View>(
    addr: std::net::SocketAddr,
    view: impl FnOnce(&ScopeRegistration) -> View,
) -> Result<ScopedOwner<View>, RuntimeError> {
    scoped_owner(ObservedScope::Listener(addr), view)
}

/// One listener scope watched only through its transfer owners.
#[doc(hidden)]
pub type ScopedTransferOwner = ScopedOwner<TransferOwnerController>;

/// Watch only the transfer owners of the endpoint at `addr`.
///
/// A case whose whole claim is what a direction polled, charged, and ended asks
/// for this rather than for the hub, so it cannot reach a server stop, a
/// connection permit, or a commitment on the way past.
#[doc(hidden)]
pub fn transfer_owner(addr: std::net::SocketAddr) -> Result<ScopedTransferOwner, RuntimeError> {
    scoped_listener(addr, ScopeRegistration::transfer_owner)
}

/// One listener scope watched only through its response commitments.
#[doc(hidden)]
pub type ScopedResponseCommitment = ScopedOwner<ResponseCommitmentController>;

/// Watch only the response commitments of the endpoint at `addr`.
///
/// A case whose whole claim is which producer took an operation's head asks for
/// this rather than for the hub, so it cannot reach that scope's server stop,
/// connection permits, or transfer owners on the way past.
#[doc(hidden)]
pub fn response_commitment(
    addr: std::net::SocketAddr,
) -> Result<ScopedResponseCommitment, RuntimeError> {
    scoped_listener(addr, ScopeRegistration::response_commitment)
}

/// One listener scope watched through the commitment an operation settled and
/// the connection that carried that answer to its peer.
///
/// Two families rather than one, because a post-head row needs both: the cell
/// says which producer took the head, and only the connection can say the answer
/// under it is over. A peer that leaves with payload still to be written is
/// observed by the connection that failed to write it, so a case reading the
/// cell in front of that settlement would be reading a window in which nothing
/// had happened yet.
#[doc(hidden)]
pub struct CommittedAnswer {
    /// The producers whose attempt at the operation's head the row reads.
    pub commitment: ResponseCommitmentController,
    /// The connections that carried those answers to their peers.
    pub connections: ConnectionOwnerController,
}

/// One listener scope watched through its commitments and connection owners.
#[doc(hidden)]
pub type ScopedCommittedAnswer = ScopedOwner<CommittedAnswer>;

/// Watch the commitments and connection owners of the endpoint at `addr`.
#[doc(hidden)]
pub fn committed_answer(addr: std::net::SocketAddr) -> Result<ScopedCommittedAnswer, RuntimeError> {
    scoped_listener(addr, |registered| CommittedAnswer {
        commitment: registered.response_commitment(),
        connections: registered.connection_owner(),
    })
}

/// One listener scope watched through the commitment an operation settled and
/// the stop that can end that operation before any producer commits.
///
/// Two families rather than one, because a forced cancellation reaches both at
/// once: the command publishes one transition, and the operation reading it and
/// the supervisor aborting on it are woken by that single publish. A case that
/// asks what such an operation committed has to hold the supervisor where it
/// selected the transition, or it is reading whichever of the two tasks the
/// runtime happened to poll first.
#[doc(hidden)]
pub struct StoppedCommitment {
    /// The producers whose attempt at the operation's head the row reads.
    pub commitment: ResponseCommitmentController,
    /// The supervisor's own passes over the stop that can end it first.
    pub stop: ServerStopController,
}

/// One listener scope watched through its commitments and its stop.
#[doc(hidden)]
pub type ScopedStoppedCommitment = ScopedOwner<StoppedCommitment>;

/// Watch the commitments and the stop authority of the endpoint at `addr`.
#[doc(hidden)]
pub fn stopped_commitment(
    addr: std::net::SocketAddr,
) -> Result<ScopedStoppedCommitment, RuntimeError> {
    scoped_listener(addr, |registered| StoppedCommitment {
        commitment: registered.response_commitment(),
        stop: registered.server_stop(),
    })
}

/// One listener scope watched only through its streaming-multipart sessions.
#[doc(hidden)]
pub type ScopedMultipartOwner = ScopedOwner<MultipartOwnerController>;

/// Watch only the streaming-multipart sessions of the endpoint at `addr`.
///
/// A case whose whole claim is what one session read, published, and released
/// asks for this rather than for the hub, so it cannot reach that scope's server
/// stop, connection permits, or response commitments on the way past.
#[doc(hidden)]
pub fn multipart_owner(addr: std::net::SocketAddr) -> Result<ScopedMultipartOwner, RuntimeError> {
    scoped_listener(addr, ScopeRegistration::multipart_owner)
}

/// One listener scope watched only through its request-body owners.
#[doc(hidden)]
pub type ScopedRequestBodyOwner = ScopedOwner<RequestBodyOwnerController>;

/// Watch only the request-body owners of the endpoint at `addr`.
///
/// A case whose whole claim is what an admitted payload polled, retained, and
/// released asks for this rather than for the hub, so it cannot reach that
/// scope's server stop, connection permits, or response commitments on the way
/// past.
#[doc(hidden)]
pub fn request_body_owner(
    addr: std::net::SocketAddr,
) -> Result<ScopedRequestBodyOwner, RuntimeError> {
    scoped_listener(addr, ScopeRegistration::request_body_owner)
}

/// One listener scope watched only through its server-stop owner.
#[doc(hidden)]
pub type ScopedServerStop = ScopedOwner<ServerStopController>;

/// Watch only the server-stop owner of the endpoint at `addr`.
///
/// A case whose whole claim is the order one supervisor took its own events in
/// asks for this rather than for the hub, so it cannot hold a connection, fail
/// an accept, or fault a task on the way past.
#[doc(hidden)]
pub fn server_stop(addr: std::net::SocketAddr) -> Result<ScopedServerStop, RuntimeError> {
    scoped_listener(addr, ScopeRegistration::server_stop)
}

/// One listener scope watched only through its connection owners.
#[doc(hidden)]
pub type ScopedConnectionOwner = ScopedOwner<ConnectionOwnerController>;

/// Watch only the connection owners of the endpoint at `addr`.
///
/// A case whose whole claim is where one connection was held, what its tree
/// recorded, or what a failed admission does asks for this rather than for the
/// hub, so it cannot select a supervisor branch or fault a task on the way past.
#[doc(hidden)]
pub fn connection_owner(addr: std::net::SocketAddr) -> Result<ScopedConnectionOwner, RuntimeError> {
    scoped_listener(addr, ScopeRegistration::connection_owner)
}

/// One listener scope watched only through its upgrade children.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub type ScopedUpgradeOwner = ScopedOwner<UpgradeOwnerController>;

/// Watch only the upgrade children of the endpoint at `addr`.
///
/// A case whose whole claim is where one handoff was held, or what the callback
/// that handoff retained settled as, asks for this rather than for the hub, so
/// it cannot select a supervisor branch, hold a connection, or fault a task on
/// the way past.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub fn upgrade_owner(addr: std::net::SocketAddr) -> Result<ScopedUpgradeOwner, RuntimeError> {
    scoped_listener(addr, ScopeRegistration::upgrade_owner)
}

/// One listener scope watched only through its server tasks.
#[doc(hidden)]
pub type ScopedServerTask = ScopedOwner<ServerTaskController>;

/// Watch only the server tasks of the endpoint at `addr`.
///
/// A case whose whole claim is what a faulted owned task does to the result its
/// server returns asks for this rather than for the hub: it decides how that
/// task ends and holds nothing, so nothing about the row is staged.
#[doc(hidden)]
pub fn server_task(addr: std::net::SocketAddr) -> Result<ScopedServerTask, RuntimeError> {
    scoped_listener(addr, ScopeRegistration::server_task)
}

/// The two owners one script can arm a fault on.
///
/// Named fields rather than a record of edges, because what this view is for is
/// the refusal itself: one script holds one fault, so arming a connection's and
/// then a task's is the case that proves the slot is shared across owners rather
/// than one slot per vocabulary.
#[doc(hidden)]
pub struct ArmedFaults {
    /// The connection owners whose admission this scope can fail.
    pub connections: ConnectionOwnerController,
    /// The server tasks this scope can end badly.
    pub tasks: ServerTaskController,
}

/// One listener scope watched through both of its fault vocabularies.
#[doc(hidden)]
pub type ScopedArmedFaults = ScopedOwner<ArmedFaults>;

/// Watch both fault vocabularies of the endpoint at `addr`.
#[doc(hidden)]
pub fn armed_faults(addr: std::net::SocketAddr) -> Result<ScopedArmedFaults, RuntimeError> {
    scoped_listener(addr, |registered| ArmedFaults {
        connections: registered.connection_owner(),
        tasks: registered.server_task(),
    })
}

/// The supervisor's own passes, and the connections it selects between.
///
/// Two families because a selection row needs both: the stop owner says which
/// branch the supervisor took, and only the connection owner can put the work
/// that branch competes against in hand — a socket held after accept, a permit
/// wait that suspended, or a completed connection waiting to be reaped.
#[doc(hidden)]
pub struct SupervisorSelection {
    /// The supervisor's own passes over the stop it is carrying out.
    pub stop: ServerStopController,
    /// The connection owners whose work those passes select between.
    pub connections: ConnectionOwnerController,
}

/// One listener scope watched through its stop owner and its connections.
#[doc(hidden)]
pub type ScopedSupervisorSelection = ScopedOwner<SupervisorSelection>;

/// Watch the stop owner and connection owners of the endpoint at `addr`.
#[doc(hidden)]
pub fn supervisor_selection(
    addr: std::net::SocketAddr,
) -> Result<ScopedSupervisorSelection, RuntimeError> {
    scoped_listener(addr, |registered| SupervisorSelection {
        stop: registered.server_stop(),
        connections: registered.connection_owner(),
    })
}

/// A supervisor selection, and the task faults its candidates come from.
///
/// [`SupervisorSelection`] plus the one thing a candidate row also needs: a
/// panicked or cancelled owned task is what gives the supervisor something to
/// carry, and the fault that produces it belongs to the server task rather than
/// to the connection that ran it.
#[doc(hidden)]
pub struct FaultedSelection {
    /// The supervisor's own passes over the stop it is carrying out.
    pub stop: ServerStopController,
    /// The connection owners whose work those passes select between.
    pub connections: ConnectionOwnerController,
    /// The server tasks whose failure the supervisor carries.
    pub tasks: ServerTaskController,
}

/// One listener scope watched through its selection owners and task faults.
#[doc(hidden)]
pub type ScopedFaultedSelection = ScopedOwner<FaultedSelection>;

/// Watch the stop owner, connection owners, and task faults of `addr`.
#[doc(hidden)]
pub fn faulted_selection(
    addr: std::net::SocketAddr,
) -> Result<ScopedFaultedSelection, RuntimeError> {
    scoped_listener(addr, |registered| FaultedSelection {
        stop: registered.server_stop(),
        connections: registered.connection_owner(),
        tasks: registered.server_task(),
    })
}

/// The supervisor's own passes, and the upgrade children it defers.
///
/// Two families because a submitted-registration row is exactly the pair: the
/// child is offered by an upgrade owner and held there, and what the supervisor
/// does with the ticket that offer left behind is the stop owner's pass.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub struct RegistrationSelection {
    /// The supervisor's own passes over the stop it is carrying out.
    pub stop: ServerStopController,
    /// The upgrade children whose registrations those passes defer or refuse.
    pub upgrades: UpgradeOwnerController,
}

/// One listener scope watched through its stop owner and upgrade children.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub type ScopedRegistrationSelection = ScopedOwner<RegistrationSelection>;

/// Watch the stop owner and upgrade children of the endpoint at `addr`.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub fn registration_selection(
    addr: std::net::SocketAddr,
) -> Result<ScopedRegistrationSelection, RuntimeError> {
    scoped_listener(addr, |registered| RegistrationSelection {
        stop: registered.server_stop(),
        upgrades: registered.upgrade_owner(),
    })
}

/// A registration selection, and the connections the same supervisor holds.
///
/// [`RegistrationSelection`] plus the connection owners, because a registration
/// row that competes with a connection needs both sides in hand. A permit row
/// needs the connection holding the limit to complete before the permit the
/// deferred registration is racing can become ready at all; a transfer row needs
/// a finished connection waiting to be reaped, so that the child answering
/// through its own owner is visibly not the one the supervisor is about to take.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub struct SupervisedRegistration {
    /// The supervisor's own passes over the stop it is carrying out.
    pub stop: ServerStopController,
    /// The connection owners those registrations wait behind or run beside.
    pub connections: ConnectionOwnerController,
    /// The upgrade children whose registrations the supervisor defers.
    pub upgrades: UpgradeOwnerController,
}

/// One listener scope watched through its stop, connection, and upgrade owners.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub type ScopedSupervisedRegistration = ScopedOwner<SupervisedRegistration>;

/// Watch the stop, connection, and upgrade owners of the endpoint at `addr`.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub fn supervised_registration(
    addr: std::net::SocketAddr,
) -> Result<ScopedSupervisedRegistration, RuntimeError> {
    scoped_listener(addr, |registered| SupervisedRegistration {
        stop: registered.server_stop(),
        connections: registered.connection_owner(),
        upgrades: registered.upgrade_owner(),
    })
}

/// A registration selection, and the task fault that unwinds the supervisor.
///
/// [`RegistrationSelection`] plus the server tasks, because an unwind row is
/// exactly that pair: the upgrade owner holds one child at its handoff and
/// another at its acknowledgement, and only the task vocabulary can panic the
/// supervisor core those held children are then answered by. The stop owner is
/// what brings the parked supervisor round to the fault and reports the phase it
/// committed on the way out.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub struct FaultedRegistration {
    /// The supervisor's own passes over the stop it is carrying out.
    pub stop: ServerStopController,
    /// The upgrade children held across that stop.
    pub upgrades: UpgradeOwnerController,
    /// The server tasks whose failure ends the supervisor.
    pub tasks: ServerTaskController,
}

/// One listener scope watched through its stop, upgrade, and task owners.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub type ScopedFaultedRegistration = ScopedOwner<FaultedRegistration>;

/// Watch the stop owner, upgrade children, and task faults of `addr`.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub fn faulted_registration(
    addr: std::net::SocketAddr,
) -> Result<ScopedFaultedRegistration, RuntimeError> {
    scoped_listener(addr, |registered| FaultedRegistration {
        stop: registered.server_stop(),
        upgrades: registered.upgrade_owner(),
        tasks: registered.server_task(),
    })
}

/// The connections one server registered, and the upgrade children they
/// transferred.
///
/// The owner tree itself and nothing above it: a row here reads which
/// connection a server took, which upgrade that connection handed on, and where
/// either of them was held. The stop that could revoke both is a different
/// owner's fact, so a case built on this cannot end the server it is reading.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub struct OwnerTree {
    /// The connections the server registered, and what their tree recorded.
    pub connections: ConnectionOwnerController,
    /// The upgrade children those connections transferred.
    pub upgrades: UpgradeOwnerController,
}

/// One listener scope watched through its connection owners and their upgrade
/// children.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub type ScopedOwnerTree = ScopedOwner<OwnerTree>;

/// Watch the connection owners and upgrade children of the endpoint at `addr`.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub fn owner_tree(addr: std::net::SocketAddr) -> Result<ScopedOwnerTree, RuntimeError> {
    scoped_listener(addr, |registered| OwnerTree {
        connections: registered.connection_owner(),
        upgrades: registered.upgrade_owner(),
    })
}

/// An owner tree, the stop that revokes it, and the terminal each bridge
/// committed.
///
/// The four families one retained-callback row reads, and no others. The
/// callback is the upgrade child's own, so its deadline and disposition come
/// from [`UpgradeOwnerController::callbacks`]; the transition that deadline was
/// fixed against is the stop owner's; the connection owner says which upgrade
/// its connection transferred, which is what makes the callback's parent
/// checkable from two writers; and the terminal owner says whether the bridge
/// under it had already released its permit.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub struct RetainedCallback {
    /// The supervisor's own passes, and the deadlines its commits minted.
    pub stop: ServerStopController,
    /// The connections whose transfers name each callback's parent.
    pub connections: ConnectionOwnerController,
    /// The upgrade children that retained the callbacks.
    pub upgrades: UpgradeOwnerController,
    /// The terminal owners whose bridges settled around those callbacks.
    pub terminals: WebSocketTerminalController,
}

/// One listener scope watched through its stop, connection, upgrade, and
/// terminal owners.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub type ScopedRetainedCallback = ScopedOwner<RetainedCallback>;

/// Watch the stop, connection, upgrade, and terminal owners of `addr`.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub fn retained_callback(
    addr: std::net::SocketAddr,
) -> Result<ScopedRetainedCallback, RuntimeError> {
    scoped_listener(addr, |registered| RetainedCallback {
        stop: registered.server_stop(),
        connections: registered.connection_owner(),
        upgrades: registered.upgrade_owner(),
        terminals: registered.websocket_terminal(),
    })
}

/// One upgrade child, everything it retains, and the connection that
/// transferred it.
///
/// The four families a direction row holds and reads, and no others. The
/// direction owners are where a read or a write is held; the terminal owner is
/// where the one cause is committed and what the bridge published about it; the
/// upgrade child is what retained the callback those two settle around; and the
/// connection owner is what says that child was its own and settled under it. A
/// case built on this cannot select a supervisor branch or take a permit.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub struct RetainedBridge {
    /// The connections that transferred each bridge's upgrade child.
    pub connections: ConnectionOwnerController,
    /// The upgrade children that retained the callbacks.
    pub upgrades: UpgradeOwnerController,
    /// The direction owners holding each transport half.
    pub directions: WebSocketDirectionController,
    /// The terminal owners that commit the one cause each bridge ends on.
    pub terminals: WebSocketTerminalController,
}

/// One listener scope watched through its connection, upgrade, direction, and
/// terminal owners.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub type ScopedRetainedBridge = ScopedOwner<RetainedBridge>;

/// Watch the connection, upgrade, direction, and terminal owners of `addr`.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub fn retained_bridge(addr: std::net::SocketAddr) -> Result<ScopedRetainedBridge, RuntimeError> {
    scoped_listener(addr, |registered| RetainedBridge {
        connections: registered.connection_owner(),
        upgrades: registered.upgrade_owner(),
        directions: registered.websocket_direction(),
        terminals: registered.websocket_terminal(),
    })
}

/// One listener scope registered for production to publish to, and read by
/// nobody.
///
/// A fixture that has to bind before it serves — because its router is built
/// against its own address — needs the reservation, not an owner. Registering
/// with an empty view keeps that scope's script alive for exactly the
/// reservation's life while naming no family at all, so the fixture cannot
/// reach a stop, a permit, or a commitment it never claimed to read.
#[doc(hidden)]
pub type ScopedUnwatched = ScopedOwner<()>;

/// Register the endpoint at `addr` and watch none of its owners.
#[doc(hidden)]
pub fn unwatched(addr: std::net::SocketAddr) -> Result<ScopedUnwatched, RuntimeError> {
    scoped_listener(addr, |_| ())
}

/// One transfer direction, and the stop that can end it before its source does.
///
/// Two families because a shutdown row reads both: the aggregate deadline the
/// stop mints is what ends the stream, and only the transfer owner can say which
/// terminal that deadline fixed. The stop half is also what holds the forced
/// abort behind the deadline, so the row reads the transfer's own decision
/// rather than whatever the escalation took away first.
#[doc(hidden)]
pub struct StoppedTransfer {
    /// The directions whose terminals and released sources the row reads.
    pub transfers: TransferOwnerController,
    /// The supervisor's own passes over the stop those directions end under.
    pub stop: ServerStopController,
}

/// One listener scope watched through its transfer owners and its stop.
#[doc(hidden)]
pub type ScopedStoppedTransfer = ScopedOwner<StoppedTransfer>;

/// Watch the transfer owners and stop authority of the endpoint at `addr`.
#[doc(hidden)]
pub fn stopped_transfer(addr: std::net::SocketAddr) -> Result<ScopedStoppedTransfer, RuntimeError> {
    scoped_listener(addr, |registered| StoppedTransfer {
        transfers: registered.transfer_owner(),
        stop: registered.server_stop(),
    })
}

/// A stopped transfer, and the commitment the operation carrying it settled.
///
/// [`StoppedTransfer`] plus the response commitment, because a staged proxy row
/// needs all three: the commitment cell says which producer took the head and
/// holds the upstream at it, the transfer owner says what the upload polled and
/// released behind that hold, and the stop is what ends the operation under
/// both.
#[doc(hidden)]
pub struct StagedTransfer {
    /// The producers whose attempt at the operation's head the row holds.
    pub commitment: ResponseCommitmentController,
    /// The directions polling payload behind that held head.
    pub transfers: TransferOwnerController,
    /// The supervisor's own passes over the stop the operation ends under.
    pub stop: ServerStopController,
}

/// One listener scope watched through its commitment, transfer, and stop owners.
#[doc(hidden)]
pub type ScopedStagedTransfer = ScopedOwner<StagedTransfer>;

/// Watch the commitment, transfer, and stop owners of the endpoint at `addr`.
#[doc(hidden)]
pub fn staged_transfer(addr: std::net::SocketAddr) -> Result<ScopedStagedTransfer, RuntimeError> {
    scoped_listener(addr, |registered| StagedTransfer {
        commitment: registered.response_commitment(),
        transfers: registered.transfer_owner(),
        stop: registered.server_stop(),
    })
}

/// The commitment an operation settled, and the payload admission behind it.
///
/// Two families because an admission row reads both ends of one operation: the
/// cell says which producer answered, and the request-body owner says how much
/// of the declared payload was polled, retained, and released before that answer
/// was taken.
#[doc(hidden)]
pub struct AdmittedCommitment {
    /// The producers whose attempt at the operation's head the row reads.
    pub commitment: ResponseCommitmentController,
    /// The admitted payloads those producers answered over.
    pub bodies: RequestBodyOwnerController,
}

/// One listener scope watched through its commitments and request bodies.
#[doc(hidden)]
pub type ScopedAdmittedCommitment = ScopedOwner<AdmittedCommitment>;

/// Watch the commitments and request-body owners of the endpoint at `addr`.
#[doc(hidden)]
pub fn admitted_commitment(
    addr: std::net::SocketAddr,
) -> Result<ScopedAdmittedCommitment, RuntimeError> {
    scoped_listener(addr, |registered| AdmittedCommitment {
        commitment: registered.response_commitment(),
        bodies: registered.request_body_owner(),
    })
}

/// One streaming-multipart session, and the payload admission in front of it.
///
/// Two families because a pre-body row is exactly the boundary between them:
/// the request-body owner says what admission polled and released before the
/// session existed, and the session owner says what its own driver had read by
/// the time the route answered.
#[doc(hidden)]
pub struct MultipartBody {
    /// The streaming-multipart sessions this listener drove.
    pub sessions: MultipartOwnerController,
    /// The admitted payloads those sessions were handed.
    pub bodies: RequestBodyOwnerController,
}

/// One listener scope watched through its multipart sessions and request
/// bodies.
#[doc(hidden)]
pub type ScopedMultipartBody = ScopedOwner<MultipartBody>;

/// Watch the multipart sessions and request-body owners of `addr`.
#[doc(hidden)]
pub fn multipart_body(addr: std::net::SocketAddr) -> Result<ScopedMultipartBody, RuntimeError> {
    scoped_listener(addr, |registered| MultipartBody {
        sessions: registered.multipart_owner(),
        bodies: registered.request_body_owner(),
    })
}

/// The commitment an operation settled, and the directions that carried it.
///
/// Two families because a post-head row starts where a pre-head row ends: the
/// cell says which producer took the head, and once that head is on the wire
/// only the transfer owner can say how the payload under it ended.
#[doc(hidden)]
pub struct CommittedTransfer {
    /// The producers whose attempt at the operation's head the row reads.
    pub commitment: ResponseCommitmentController,
    /// The directions carrying payload under those committed heads.
    pub transfers: TransferOwnerController,
}

/// One listener scope watched through its commitment and transfer owners.
#[doc(hidden)]
pub type ScopedCommittedTransfer = ScopedOwner<CommittedTransfer>;

/// Watch the commitment and transfer owners of the endpoint at `addr`.
#[doc(hidden)]
pub fn committed_transfer(
    addr: std::net::SocketAddr,
) -> Result<ScopedCommittedTransfer, RuntimeError> {
    scoped_listener(addr, |registered| CommittedTransfer {
        commitment: registered.response_commitment(),
        transfers: registered.transfer_owner(),
    })
}

/// Every owner one admitted operation crosses, from its permit to its payload.
///
/// Four families because a service-operation matrix row is one operation read
/// end to end: the connection that admitted it, the envelope its head minted,
/// the payload admission behind that head, and the direction that carried the
/// answer out. Each fact belongs to a different owner, and a row proving they
/// describe one operation has to read all four of them.
#[doc(hidden)]
pub struct AdmittedOperation {
    /// The connections whose permits admitted these operations.
    pub connections: ConnectionOwnerController,
    /// The producers whose attempt at each operation's head the row reads.
    pub commitment: ResponseCommitmentController,
    /// The admitted payloads behind those heads.
    pub bodies: RequestBodyOwnerController,
    /// The directions that carried those answers out.
    pub transfers: TransferOwnerController,
}

/// One listener scope watched through every owner an operation crosses.
#[doc(hidden)]
pub type ScopedAdmittedOperation = ScopedOwner<AdmittedOperation>;

/// Watch the connection, commitment, body, and transfer owners of `addr`.
#[doc(hidden)]
pub fn admitted_operation(
    addr: std::net::SocketAddr,
) -> Result<ScopedAdmittedOperation, RuntimeError> {
    scoped_listener(addr, |registered| AdmittedOperation {
        connections: registered.connection_owner(),
        commitment: registered.response_commitment(),
        bodies: registered.request_body_owner(),
        transfers: registered.transfer_owner(),
    })
}

/// One upgrade child held at its handoff, and the commitment that refused it.
///
/// Two families because a refused-handoff row is exactly the pair: the upgrade
/// owner holds the ticket where the registrar took it, which is what proves
/// negotiation had already succeeded, and only the commitment cell can say the
/// framework — not a bridge — took the one answer before a `101` could exist.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub struct HandoffCommitment {
    /// The upgrade children this row holds at their handoff.
    pub upgrades: UpgradeOwnerController,
    /// The producers whose attempt at the operation's head the row reads.
    pub commitment: ResponseCommitmentController,
}

/// One listener scope watched through its upgrade children and commitments.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub type ScopedHandoffCommitment = ScopedOwner<HandoffCommitment>;

/// Watch the upgrade children and commitments of the endpoint at `addr`.
#[doc(hidden)]
#[cfg(feature = "ws")]
pub fn handoff_commitment(
    addr: std::net::SocketAddr,
) -> Result<ScopedHandoffCommitment, RuntimeError> {
    scoped_listener(addr, |registered| HandoffCommitment {
        upgrades: registered.upgrade_owner(),
        commitment: registered.response_commitment(),
    })
}

/// A multipart session and its admission, the transfer beside them, and the
/// stop that can revoke all three.
///
/// [`MultipartBody`] plus two more, because a revocation row asks two further
/// questions. The session's own bound or the server's aggregate deadline ends
/// it, and holding the supervisor where it selected the escalation is what keeps
/// the forced abort from taking the session away before it has released what it
/// owned. The transfer owner is read to prove the negative: route-aware
/// admission owns the byte authority here, so the direction beside the session
/// must have counted nothing of its own.
#[doc(hidden)]
pub struct StoppedMultipart {
    /// The streaming-multipart sessions this listener drove.
    pub sessions: MultipartOwnerController,
    /// The admitted payloads those sessions were handed.
    pub bodies: RequestBodyOwnerController,
    /// The directions beside those sessions, which own no byte authority here.
    pub transfers: TransferOwnerController,
    /// The supervisor's own passes over the stop that can revoke them.
    pub stop: ServerStopController,
}

/// One listener scope watched through its session, body, transfer, and stop
/// owners.
#[doc(hidden)]
pub type ScopedStoppedMultipart = ScopedOwner<StoppedMultipart>;

/// Watch the multipart session, request-body, transfer, and stop owners of
/// `addr`.
#[doc(hidden)]
pub fn stopped_multipart(
    addr: std::net::SocketAddr,
) -> Result<ScopedStoppedMultipart, RuntimeError> {
    scoped_listener(addr, |registered| StoppedMultipart {
        sessions: registered.multipart_owner(),
        bodies: registered.request_body_owner(),
        transfers: registered.transfer_owner(),
        stop: registered.server_stop(),
    })
}

/// An admitted commitment, and the stop that can end it before a producer
/// commits.
///
/// [`AdmittedCommitment`] plus the stop owner, because a staged precedence row
/// needs all three: the cell says which cause took the head, the request-body
/// owner says what the payload had polled when it did, and the stop is what
/// publishes the transition both are weighed against. The stop half also holds
/// the supervisor where it selected that transition, so the row reads the
/// operation's own decision rather than the abort behind it.
#[doc(hidden)]
pub struct StagedCommitment {
    /// The producers whose attempt at the operation's head the row holds.
    pub commitment: ResponseCommitmentController,
    /// The admitted payload polling behind that held head.
    pub bodies: RequestBodyOwnerController,
    /// The supervisor's own passes over the stop the operation is weighed
    /// against.
    pub stop: ServerStopController,
}

/// One listener scope watched through its commitment, body, and stop owners.
#[doc(hidden)]
pub type ScopedStagedCommitment = ScopedOwner<StagedCommitment>;

/// Watch the commitment, request-body, and stop owners of the endpoint at
/// `addr`.
#[doc(hidden)]
pub fn staged_commitment(
    addr: std::net::SocketAddr,
) -> Result<ScopedStagedCommitment, RuntimeError> {
    scoped_listener(addr, |registered| StagedCommitment {
        commitment: registered.response_commitment(),
        bodies: registered.request_body_owner(),
        stop: registered.server_stop(),
    })
}

/// One offloaded worker family, and the collector charging what it retains.
///
/// Two families because a blocking worker's row asks two questions: where the
/// work ran and what maximum it froze, which the worker owner publishes, and how
/// much it actually retained on the way, which the checked collector charging
/// those bytes publishes. Named fields rather than a tuple, so a row states
/// which owner it is reading rather than which position that owner sits in.
#[doc(hidden)]
pub struct WorkerRetention {
    /// Where this family of workers ran, and what its last answer froze.
    pub worker: BlockingWorkerController,
    /// What the collector charging those workers' bytes read and retained.
    pub collected: TransferOwnerController,
}

/// One scope watched only through its offloaded workers and their retention.
#[doc(hidden)]
pub type ScopedBlockingWorker = ScopedOwner<WorkerRetention>;

/// The two owners a blocking-worker row reads, over one registration.
fn worker_retention(registered: &ScopeRegistration) -> WorkerRetention {
    WorkerRetention {
        worker: registered.blocking_worker(),
        collected: registered.transfer_owner(),
    }
}

/// Watch only the static-file workers one served root answers from.
///
/// The root is the one a caller names, before any canonicalization, because
/// that is the only spelling both a direct call and a registered route share.
///
/// A case whose whole claim is where a read ran, what it froze, and what it
/// retained asks for this rather than for the hub, so it cannot reach a server
/// stop, a connection permit, or a response commitment on the way past.
#[doc(hidden)]
pub fn static_file_worker(root: &std::path::Path) -> Result<ScopedBlockingWorker, RuntimeError> {
    scoped_owner(ObservedScope::StaticRoot(root.into()), worker_retention)
}

pub(crate) fn lifecycle_script(addr: std::net::SocketAddr) -> Option<Arc<LifecycleScript>> {
    scoped_script(|scope| matches!(scope, ObservedScope::Listener(listener) if *listener == addr))
}

pub(in crate::http) fn static_file_script(root: &std::path::Path) -> Option<Arc<LifecycleScript>> {
    scoped_script(
        |scope| matches!(scope, ObservedScope::StaticRoot(watched) if watched.as_ref() == root),
    )
}

/// Watch only the profiling workers this process answers with.
///
/// One scope, because the profiler itself is one: `pprof` registers a single
/// process-wide sampler, so two profiling observers would be two views of the
/// same worker rather than two independent ones.
#[cfg(feature = "profiling")]
#[doc(hidden)]
pub fn profiling_worker() -> Result<ScopedBlockingWorker, RuntimeError> {
    scoped_owner(ObservedScope::Profiler, worker_retention)
}

#[cfg(feature = "profiling")]
pub(in crate::http) fn profiling_script() -> Option<Arc<LifecycleScript>> {
    scoped_script(|scope| matches!(scope, ObservedScope::Profiler))
}

/// The narrow controller over one scope's offloaded blocking workers.
///
/// Two powers and no others: hold one worker of that family at an edge its own
/// protocol has, and read what that family reported about where it ran and what
/// it froze. It resolves no path, measures no file, samples no stack, chooses no
/// maximum, retains no byte, and decides no thread, so a case built on it proves
/// what the worker did rather than staging it.
#[doc(hidden)]
#[derive(Clone)]
pub struct BlockingWorkerController {
    script: Arc<LifecycleScript>,
}

impl BlockingWorkerController {
    /// What the static-file workers under this root have done so far.
    ///
    /// Read-only, and every number in it is written by the production step it
    /// names.
    pub fn static_files_observed(&self) -> StaticFileObservation {
        self.script.static_files_observed()
    }

    /// What this process's profiling workers have done so far.
    ///
    /// Read-only, and every number in it is written by the production owner it
    /// names.
    #[cfg(feature = "profiling")]
    pub fn profiling_observed(&self) -> ProfilingObservation {
        self.script.profiling_observed()
    }
}

/// The narrow controller over one listener's server-stop owner.
///
/// Two powers and no others: hold that owner at either side of its commit into
/// the causal stop state, and read what the state has committed. It carries no
/// way to name a broad checkpoint, submit a stop event, mint a deadline, or fix
/// a result, so a case built on it proves commit order rather than staging one.
#[doc(hidden)]
#[derive(Clone)]
pub struct ServerStopController {
    script: Arc<LifecycleScript>,
}

/// Give one narrow controller the arm/wait/release protocol over its own
/// vocabulary.
///
/// The three calls are the whole protocol every owner-local controller needs,
/// and they differ only in which [`PauseKey`] arm the edge belongs to. Written
/// once here so a new owner gets the protocol by naming its key rather than by
/// copying three bodies that could drift apart.
macro_rules! owner_local_controller {
    ($controller:ident, $edge:ty, $key:path) => {
        impl $controller {
            pub fn pause_once(&self, edge: $edge) -> Result<(), RuntimeError> {
                self.script.arm($key(edge))
            }

            pub async fn wait_until_paused(&self, edge: $edge) -> Result<(), RuntimeError> {
                self.script.wait_until_paused($key(edge)).await
            }

            pub fn release(&self, edge: $edge) -> Result<(), RuntimeError> {
                self.script.release_checkpoint($key(edge))
            }
        }
    };
}

/// Give one narrow controller the poll count over its own edge vocabulary.
///
/// A second macro rather than a fourth call in the one above. The protocol
/// every owner needs and the turn count only some owners can use are two
/// grants. An owner whose held future no other event can provoke a poll for has
/// nothing to read here, so it never takes this one.
macro_rules! owner_local_polls {
    ($controller:ident, $edge:ty, $key:path) => {
        impl $controller {
            /// How many turns the owner held at `edge` has taken.
            ///
            /// A held future looks for its release once per poll, so this counts
            /// the polls that owner has been given since the edge was armed. It
            /// is what says whether a release staged without a wake has since
            /// been observed. An edge nothing armed is refused rather than
            /// counted as zero.
            ///
            /// # Errors
            ///
            /// Refuses an edge this script has not armed.
            pub fn polls(&self, edge: $edge) -> Result<usize, RuntimeError> {
                self.script.edge_polls($key(edge))
            }
        }
    };
}

/// Give one narrow controller the staged release over its own edge vocabulary.
///
/// Separate from the poll count for the same reason that count is separate from
/// the protocol. Staging a release is owner-local authority. Only a controller
/// whose own owner can produce the wake that consumes the release may hold it.
macro_rules! owner_local_staged_release {
    ($controller:ident, $edge:ty, $key:path) => {
        impl $controller {
            /// Record one held owner's release without waking it.
            ///
            /// The owner stays parked and observes the release on whatever poll
            /// something else provokes. That is the only way to put a fact this
            /// owner is already holding into the same turn as one the case
            /// publishes afterwards: an ordinary release wakes the owner, which
            /// spends the turn before the second fact exists. A staged release
            /// is a release — the edge is spent, exactly as `release` leaves it.
            ///
            /// # Errors
            ///
            /// Refuses an edge this script never armed, one production has not
            /// reached yet, and one already released.
            pub fn release_without_waking(&self, edge: $edge) -> Result<(), RuntimeError> {
                self.script.record_release($key(edge)).map(drop)
            }
        }
    };
}

owner_local_controller!(ServerStopController, ServerStopEdge, PauseKey::ServerStop);
owner_local_controller!(
    BlockingWorkerController,
    BlockingWorkerEdge,
    PauseKey::BlockingWorker
);

impl ServerStopController {
    /// What this server's stop state has committed so far.
    pub fn observed(&self) -> ServerStopObservation {
        self.script.server_stop_observed()
    }
}

/// The narrow controller over one listener's connection owners.
///
/// It holds a connection where one of its own edges names, and reads what that
/// listener's owner tree recorded. It admits no socket, takes and releases no
/// permit, transfers no child, and settles no owner, so a case built on it
/// proves the tree production wrote rather than one the case arranged.
#[doc(hidden)]
#[derive(Clone)]
pub struct ConnectionOwnerController {
    script: Arc<LifecycleScript>,
}

owner_local_controller!(
    ConnectionOwnerController,
    ConnectionOwnerEdge,
    PauseKey::ConnectionOwner
);
owner_local_polls!(
    ConnectionOwnerController,
    ConnectionOwnerEdge,
    PauseKey::ConnectionOwner
);

impl ConnectionOwnerController {
    /// What this listener's owner tree has registered, transferred, and settled.
    ///
    /// Read-only and passive: every event is written by the production registry
    /// mutation it names, and no production decision reads it back.
    pub fn observed(&self) -> ConnectionOwnershipObservation {
        self.script.ownership_observed()
    }

    /// Fail this listener's next admission once.
    ///
    /// The accept is the connection owner's own way in, so failing it belongs
    /// here rather than to whatever the supervisor does with the failure. One
    /// script arms one fault: a second, from either vocabulary, is refused
    /// while the first is unconsumed.
    pub fn inject_once(&self, fault: ConnectionFault) -> Result<(), RuntimeError> {
        self.script.arm_fault(ArmedFault::Connection(fault))
    }
}

/// The narrow controller over one listener's server tasks.
///
/// One power: decide how the next owned task, or the supervisor's own core,
/// ends. It arms no checkpoint, releases nothing, reads nothing back, and
/// cannot fail an admission, so a case built on it proves what a server does
/// with a failure rather than when it sees one.
#[doc(hidden)]
#[derive(Clone)]
pub struct ServerTaskController {
    script: Arc<LifecycleScript>,
}

impl ServerTaskController {
    /// End this listener's next server task badly, once.
    ///
    /// One script arms one fault, so a second — this vocabulary's or the
    /// connection owner's — is refused while the first is unconsumed.
    pub fn inject_once(&self, fault: ServerTaskFault) -> Result<(), RuntimeError> {
        self.script.arm_fault(ArmedFault::ServerTask(fault))
    }
}

/// The narrow controller over one listener's upgrade children.
///
/// Three moments and nothing else. It cannot admit, refuse, commit, cancel, or
/// join an upgrade, so a case built on it proves the connection's own transfer
/// order rather than staging one.
#[doc(hidden)]
#[cfg(feature = "ws")]
#[derive(Clone)]
pub struct UpgradeOwnerController {
    script: Arc<LifecycleScript>,
}

#[cfg(feature = "ws")]
owner_local_controller!(
    UpgradeOwnerController,
    UpgradeOwnerEdge,
    PauseKey::UpgradeOwner
);

#[cfg(feature = "ws")]
impl UpgradeOwnerController {
    /// What this listener's upgrade children decided about the callbacks they
    /// retained, in the order those decisions were published.
    ///
    /// Owner-local and read-only: the retained callback is the upgrade's own
    /// child, so the deadline it fixed, every transition that brought that
    /// deadline forward, and the disposition its join ended at are all this
    /// owner's facts. Nothing here joins a callback, moves a deadline, or names
    /// a disposition.
    pub fn callbacks(&self) -> Box<[WebSocketCallbackObservation]> {
        self.script
            .websocket
            .callbacks
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .into_boxed_slice()
    }
}

/// The narrow controller over one listener's WebSocket direction owners.
///
/// It holds one direction where one of its own edges names. It admits no
/// message, builds and decodes no frame, closes no queue, and offers no cause,
/// so a case built on it proves what the direction that owns the transport half
/// did rather than what the case arranged for it.
#[doc(hidden)]
#[cfg(feature = "ws")]
#[derive(Clone)]
pub struct WebSocketDirectionController {
    script: Arc<LifecycleScript>,
}

#[cfg(feature = "ws")]
owner_local_controller!(
    WebSocketDirectionController,
    WebSocketDirectionEdge,
    PauseKey::WebSocketDirection
);
#[cfg(feature = "ws")]
owner_local_polls!(
    WebSocketDirectionController,
    WebSocketDirectionEdge,
    PauseKey::WebSocketDirection
);
#[cfg(feature = "ws")]
owner_local_staged_release!(
    WebSocketDirectionController,
    WebSocketDirectionEdge,
    PauseKey::WebSocketDirection
);

/// The narrow controller over one listener's WebSocket terminal owners.
///
/// Two commit edges and the graceful close it owes, and nothing else. It cannot
/// offer a cause, rank one, choose a disposition, or settle a direction, so a
/// case built on it proves the order the bridge committed in rather than
/// staging one.
#[doc(hidden)]
#[cfg(feature = "ws")]
#[derive(Clone)]
pub struct WebSocketTerminalController {
    script: Arc<LifecycleScript>,
}

#[cfg(feature = "ws")]
owner_local_controller!(
    WebSocketTerminalController,
    WebSocketTerminalEdge,
    PauseKey::WebSocketTerminal
);

#[cfg(feature = "ws")]
impl WebSocketTerminalController {
    /// What this listener's direct bridges have committed and settled so far.
    pub fn observed(&self) -> WebSocketDirectionObservation {
        self.script.websocket_observed()
    }
}

/// The narrow controller over one listener's response producers.
///
/// It holds one producer where one of its own edges names, and reads what that
/// operation's commitment settled on. It maps no rejection, names no origin,
/// produces no head, and takes the cell on nobody's behalf, so a case built on
/// it proves which producer reached the commitment first rather than choosing
/// the winner for production.
#[doc(hidden)]
#[derive(Clone)]
pub struct ResponseCommitmentController {
    script: Arc<LifecycleScript>,
}

owner_local_controller!(
    ResponseCommitmentController,
    ResponseCommitmentEdge,
    PauseKey::ResponseCommitment
);
owner_local_polls!(
    ResponseCommitmentController,
    ResponseCommitmentEdge,
    PauseKey::ResponseCommitment
);
owner_local_staged_release!(
    ResponseCommitmentController,
    ResponseCommitmentEdge,
    PauseKey::ResponseCommitment
);

impl ResponseCommitmentController {
    /// What this listener's response commitments have settled on.
    pub fn observed(&self) -> ResponseCommitmentObservation {
        self.script.commitment_observed()
    }

    /// What the admitted operations behind those commitments published.
    ///
    /// Read from this owner because the commitment is the operation's own cell:
    /// the envelope every pre-head owner read, and the account the answering
    /// exit staged, are facts about the same operation the cell belongs to. The
    /// account is staged where the answer is handed over rather than where it
    /// reaches the peer, so a case that needs the transport under a committed
    /// head to be over waits on the connection owner instead.
    pub fn operations_observed(&self) -> OperationObservation {
        self.script.operation_observed()
    }
}

/// The narrow controller over one listener's transfer owners.
///
/// Two moments per direction and nothing else: before the source read, and
/// after it, before the terminal that read decided is committed. It supplies no
/// frame, charges no byte, and chooses no terminal.
#[doc(hidden)]
#[derive(Clone)]
pub struct TransferOwnerController {
    script: Arc<LifecycleScript>,
}

owner_local_controller!(
    TransferOwnerController,
    TransferOwnerEdge,
    PauseKey::TransferOwner
);
owner_local_polls!(
    TransferOwnerController,
    TransferOwnerEdge,
    PauseKey::TransferOwner
);
owner_local_staged_release!(
    TransferOwnerController,
    TransferOwnerEdge,
    PauseKey::TransferOwner
);

impl TransferOwnerController {
    /// What this listener's transfer owners have polled, charged, and ended.
    pub fn observed(&self) -> TransferObservation {
        self.script.transfer_observed()
    }

    /// What the checked collector read from this peer's answers.
    ///
    /// A collection is one direction's own read under one direction's own
    /// ceiling, which is why it is read from here: the owner that charged the
    /// bytes is the owner that publishes them. One snapshot rather than three
    /// reads, so the peak and the first boundary answer for one instant.
    pub fn collected(&self) -> CollectionObservation {
        self.script.collection_observed()
    }

    /// How many chunks the checked collector read from this peer's answers.
    ///
    /// Zero after a refused declaration is the whole claim there: the maximum
    /// was crossed before anything was read, so nothing was allocated for it.
    pub fn collected_chunks_polled(&self) -> usize {
        self.collected().chunks_polled
    }

    /// The most any one collection from this peer retained at once.
    pub fn collected_peak_retained_bytes(&self) -> usize {
        self.collected().peak_retained_bytes
    }

    /// What the first chunk retained under this scope left behind.
    ///
    /// The exact boundary a source with undeclared chunk sizes offers: a case
    /// that freezes a maximum here lands it on a real chunk edge instead of a
    /// number the producer never promised. Zero is "nothing retained yet".
    pub fn collected_first_retained_bytes(&self) -> usize {
        self.collected().first_retained_bytes
    }
}

/// The narrow observer over one listener's request-body owners.
///
/// Observation only, because a request body has no edge of its own: the frame
/// it is read in is the producer's, and that producer is held at the response
/// commitment. What this reports is what the collector polled, what admission
/// retained, and how many permit owners let go — and it polls no frame, retains
/// no byte, chooses no maximum, and releases no permit.
#[doc(hidden)]
#[derive(Clone)]
pub struct RequestBodyOwnerController {
    script: Arc<LifecycleScript>,
}

impl RequestBodyOwnerController {
    /// What this listener's request-body owners have polled, retained, and
    /// released.
    pub fn observed(&self) -> RequestBodyObservation {
        self.script.body_observed()
    }
}

/// The narrow controller over one listener's streaming-multipart sessions.
///
/// It holds one session owner where one of its own edges names. It submits no
/// command, parses no part, publishes no reply, and selects no response.
#[doc(hidden)]
#[derive(Clone)]
pub struct MultipartOwnerController {
    script: Arc<LifecycleScript>,
}

owner_local_controller!(
    MultipartOwnerController,
    MultipartOwnerEdge,
    PauseKey::Multipart
);
owner_local_polls!(
    MultipartOwnerController,
    MultipartOwnerEdge,
    PauseKey::Multipart
);

impl MultipartOwnerController {
    /// What this listener's streaming multipart sessions have done so far.
    ///
    /// Read-only, and every number in it is written by the production decision
    /// it names: nothing here accepts a command, polls a body, publishes a
    /// reply, terminates a driver, or selects a response.
    pub fn observed(&self) -> MultipartObservation {
        self.script.multipart_observed()
    }
}

/// One owned server's causal stop state, driven directly.
///
/// The production state machine, constructed with no listener, no supervisor,
/// and no children, so a case can apply every event from every phase and read
/// exactly what committed. It serves nothing, admits nothing, and cancels
/// nothing: the only authority it has is the authority every production owner
/// already has, which is to submit a fact and be told what the commit changed.
#[doc(hidden)]
pub struct ServerStopProbe {
    state: Arc<super::server_stop::ServerStopState>,
}

impl ServerStopProbe {
    /// Submit one event and answer the phase the commit left, and whether it
    /// moved.
    pub fn apply(&self, event: ServerStopEvent) -> ServerStopTransition {
        let transition = self.state.apply(match event {
            ServerStopEvent::Graceful => {
                super::server_stop::StopEvent::Graceful(tokio::time::Instant::now())
            }
            ServerStopEvent::Cancel => super::server_stop::StopEvent::Cancel,
            ServerStopEvent::Abandon => super::server_stop::StopEvent::Abandon,
            ServerStopEvent::Fatal => super::server_stop::StopEvent::Fatal(RuntimeError::Http(
                "probed server-fatal fact".into(),
            )),
            ServerStopEvent::DeadlineExpiry => super::server_stop::StopEvent::DeadlineExpiry,
            ServerStopEvent::Settled => super::server_stop::StopEvent::Settled,
        });
        ServerStopTransition {
            phase: phase_label(transition.phase),
            changed: transition.changed,
        }
    }

    /// What this stop state has committed so far.
    pub fn observed(&self) -> ServerStopObservation {
        observation_of(&self.state)
    }

    /// Take the immutable flat result, exactly as the supervisor does.
    ///
    /// `None` while admission is still open, and `None` a second time: the
    /// result leaves once, to the one owner that reports it.
    pub fn take_result(&self) -> Option<Result<(), RuntimeError>> {
        self.state.settle()
    }
}

/// Drive one causal stop state over `grace`, with nothing attached to it.
#[doc(hidden)]
pub fn server_stop_probe(grace: std::time::Duration) -> ServerStopProbe {
    ServerStopProbe {
        state: super::server_stop::ServerStopState::new(
            crate::lifecycle::AggregateShutdown::new(grace, None),
            None,
        ),
    }
}

/// The events an owner may submit to a causal stop state, as a case names them.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ServerStopEvent {
    /// Public `shutdown`, runtime shutdown, or a signal watcher.
    Graceful,
    /// Public `cancel`.
    Cancel,
    /// An armed owner went away without asking.
    Abandon,
    /// A server-fatal fact one owner reported.
    Fatal,
    /// The one aggregate deadline expired.
    DeadlineExpiry,
    /// The listener and every owned child settled.
    Settled,
}

/// What committing one stop event changed.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerStopTransition {
    pub phase: &'static str,
    pub changed: bool,
}

/// What one server's causal stop state has committed.
///
/// Read-only, and every field is written by the production commit it names.
/// The committed error itself is absent by design: it leaves through the flat
/// result exactly once, so an observation reports how the server ended rather
/// than holding a second copy of why.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ServerStopObservation {
    /// `running`, `graceful`, `cancelled`, `deadline-expired`, or `finished`.
    pub phase: &'static str,
    /// `pending`, `completed`, `failed`, `cancelled`, or `deadline-expired`.
    pub outcome: &'static str,
    /// The one aggregate expiry this server's graceful commit minted.
    ///
    /// `None` until a graceful phase commits, and the same instant from then on.
    /// A case reads it twice to prove an escalation kept the deadline it was
    /// already under rather than minting itself a fresh grace.
    pub aggregate_deadline: Option<tokio::time::Instant>,
    /// The instant this server's forced phase committed.
    ///
    /// `None` until a cancellation or an abandoned owner commits one, and the
    /// same instant from then on. A case reads it to say a bound derived from
    /// the escalation was derived from the commit rather than from whenever the
    /// owner that answered it happened to be scheduled.
    pub forced_commit: Option<tokio::time::Instant>,
    /// Whether a caller asked for forced termination, rather than an owner
    /// having gone away.
    pub cancel_commanded: bool,
    /// How many events moved the committed phase.
    pub commits: u64,
    /// How many events were applied, including compatible repeats and no-ops.
    pub applied: u64,
    /// Whether a first server-fatal fact was recorded.
    pub fatal_recorded: bool,
    /// Fatal facts that arrived after the first, kept as diagnostics only.
    pub later_fatal_facts: u64,
}

fn phase_label(phase: super::server_stop::StopPhase) -> &'static str {
    match phase {
        super::server_stop::StopPhase::Running => "running",
        super::server_stop::StopPhase::Graceful => "graceful",
        super::server_stop::StopPhase::Cancelled => "cancelled",
        super::server_stop::StopPhase::TimedOut => "deadline-expired",
        super::server_stop::StopPhase::Finished => "finished",
    }
}

fn outcome_label(outcome: super::server_stop::StopOutcome) -> &'static str {
    match outcome {
        super::server_stop::StopOutcome::Pending => "pending",
        super::server_stop::StopOutcome::Completed => "completed",
        super::server_stop::StopOutcome::Failed => "failed",
        super::server_stop::StopOutcome::Cancelled => "cancelled",
        super::server_stop::StopOutcome::TimedOut => "deadline-expired",
    }
}

/// Read one stop state directly, for the probe that owns its own.
fn observation_of(state: &super::server_stop::ServerStopState) -> ServerStopObservation {
    state.read(reading_observation)
}

fn reading_observation(
    reading: &super::server_stop::CommittedStopReading,
) -> ServerStopObservation {
    ServerStopObservation {
        phase: phase_label(reading.phase),
        outcome: outcome_label(reading.outcome),
        aggregate_deadline: reading.aggregate_expiry,
        forced_commit: reading.forced_at,
        cancel_commanded: reading.commanded,
        commits: reading.commits,
        applied: reading.applied,
        fatal_recorded: reading.fatal_recorded,
        later_fatal_facts: reading.later_fatal_facts,
    }
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
        self.gate.release();
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
