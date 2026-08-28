//! The orthogonal facts one admitted HTTP operation ends with.
//!
//! What replaced a strongest terminal: a request does not end on one thing. It
//! ends with a producer that answered it, an answer that was or was not
//! delivered, a connection that may have gone away under it, a configured bound
//! it may have crossed, and a server that may have been stopping while it ran.
//! A single fold over those five said only the loudest of them, and an operator
//! reading it could not tell an application response interrupted by a departing
//! peer from a peer that left before any answer existed.
//!
//! So each dimension is its own set-once cell with one production writer. The
//! response commitment names the origin, the mapper names the typed rejection,
//! the enforcer names the bound, the connection guard names the connection end,
//! and the finalizer names delivery and the shutdown it snapshotted. Setting one
//! erases and outranks none of the others.

use super::super::boundary::{ByteBoundary, CrossedBound, DeadlineBoundary};
use super::super::disconnect::DisconnectCause;
use super::super::handle::ConnCtx;
use super::super::mock::LifecycleScript;
use super::super::operation::InboundTerminal;
use super::super::rejection::{RejectionKind, RejectionScope, RequestIdentity};
use super::super::response_commitment::ResponseOrigin;
use super::super::server_stop::{CommittedStopReading, StopOutcome, StopPhase};
use super::super::transfer::TransferDirection;
use std::sync::{Arc, Mutex, OnceLock};

/// The name every absent completion dimension is published under.
///
/// One spelling, because a counter whose label is sometimes absent splits one
/// time series into two — and two spellings of "absent" split it into three.
pub(in crate::http) const ABSENT: &str = "none";

/// Which observability channels this process publishes a completion to.
///
/// Read once per request off the connection context rather than at the
/// terminal: the finalizer runs long after the dispatch that answered, and the
/// context it answered under is not borrowed there.
#[derive(Clone, Copy)]
pub(in crate::http) struct Telemetry {
    events: bool,
    metrics: bool,
}

impl Telemetry {
    /// What the connection this request arrived on publishes.
    fn of(ctx: &ConnCtx) -> Self {
        Self {
            events: ctx.tracing_enabled,
            metrics: ctx.metrics_handle.is_some(),
        }
    }

    pub(in crate::http) const fn events(self) -> bool {
        self.events
    }

    pub(in crate::http) const fn metrics(self) -> bool {
        self.metrics
    }
}

/// What became of the answer this operation committed, if it committed one.
///
/// Orthogonal to the producer that answered and to whatever ended the
/// connection: an application response that reached the peer in full and one
/// cut short halfway are the same origin and different deliveries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::http) enum DeliveryOutcome {
    /// This operation ended before any response head was committed.
    NotCommitted,
    /// The committed response lifetime produced its whole body or handoff.
    Produced,
    /// The committed response lifetime ended before its body or handoff did.
    Interrupted,
}

impl DeliveryOutcome {
    /// Every delivery a completion can be recorded under.
    const ALL: [Self; 3] = [Self::NotCommitted, Self::Produced, Self::Interrupted];

    /// Every name a delivery label may carry.
    pub(in crate::http) fn vocabulary() -> Box<[&'static str]> {
        Self::ALL.map(Self::label).into_iter().collect()
    }

    /// The bounded name this delivery is reported and counted under.
    const fn label(self) -> &'static str {
        match self {
            Self::NotCommitted => "not-committed",
            Self::Produced => "produced",
            Self::Interrupted => "interrupted",
        }
    }

    /// How a committed answer's lifetime ended, or that none was committed.
    ///
    /// `committed` is whether any head was committed for this operation, by a
    /// producer or by Camber's own cause table. A lifetime that ended without
    /// one delivered nothing, whatever the connection under it then did.
    const fn of(committed: bool, settled: DisconnectCause) -> Self {
        match (committed, settled) {
            (false, _) => Self::NotCommitted,
            (true, DisconnectCause::Completed) => Self::Produced,
            (
                true,
                DisconnectCause::PeerDisconnect
                | DisconnectCause::StreamReset
                | DisconnectCause::ServerShutdown,
            ) => Self::Interrupted,
        }
    }
}

/// How this operation's connection ended under it, if it ended at all.
///
/// Absent for every operation whose connection outlived it, which includes
/// every completed response and every operation a server stop ended: a server
/// that stopped is a shutdown observation, not a connection that failed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::http) enum ConnectionEnd {
    /// The peer closed its side of the transport.
    PeerDisconnected,
    /// This request ended early while its connection stayed live.
    StreamReset,
    /// A concrete read or write on this transport failed.
    TransportFailed,
}

impl ConnectionEnd {
    /// Every connection end a completion can be recorded under.
    const ALL: [Self; 3] = [
        Self::PeerDisconnected,
        Self::StreamReset,
        Self::TransportFailed,
    ];

    /// Every name a connection-end label may carry, including stated absence.
    pub(in crate::http) fn vocabulary() -> Box<[&'static str]> {
        optional_vocabulary(Self::ALL.map(Self::label))
    }

    /// The bounded name this connection end is reported and counted under.
    const fn label(self) -> &'static str {
        match self {
            Self::PeerDisconnected => "peer-disconnected",
            Self::StreamReset => "stream-reset",
            Self::TransportFailed => "transport-failed",
        }
    }

    /// The connection end one settled response lifetime names, if any.
    ///
    /// `failed` is the transport's own report that a read or write returned an
    /// error, which is what tells a peer that closed cleanly from one whose
    /// connection broke. Both mark the connection terminating, so without it the
    /// two would be one row.
    pub(in crate::http) const fn of(settled: DisconnectCause, failed: bool) -> Option<Self> {
        match (settled, failed) {
            (DisconnectCause::Completed | DisconnectCause::ServerShutdown, _) => None,
            (DisconnectCause::PeerDisconnect, false) => Some(Self::PeerDisconnected),
            (DisconnectCause::PeerDisconnect, true) => Some(Self::TransportFailed),
            (DisconnectCause::StreamReset, _) => Some(Self::StreamReset),
        }
    }
}

/// The last server phase committed before an owner snapshotted it.
///
/// Absent is a server that was still running, which is a different fact from a
/// server that had asked for a graceful stop and a different one again from a
/// deadline that had already expired.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::http) enum ShutdownObservation {
    /// A graceful stop was committed and had not escalated.
    Graceful,
    /// Forced cancellation was committed.
    Cancelled,
    /// The one aggregate deadline expired.
    DeadlineExpired,
}

impl ShutdownObservation {
    /// Every shutdown observation a completion can be recorded under.
    const ALL: [Self; 3] = [Self::Graceful, Self::Cancelled, Self::DeadlineExpired];

    /// Every name a shutdown label may carry, including stated absence.
    pub(in crate::http) fn vocabulary() -> Box<[&'static str]> {
        optional_vocabulary(Self::ALL.map(Self::label))
    }

    /// The bounded name this observation is reported and counted under.
    pub(in crate::http) const fn label(self) -> &'static str {
        match self {
            Self::Graceful => "graceful",
            Self::Cancelled => "cancelled",
            Self::DeadlineExpired => "deadline-expired",
        }
    }

    /// What this server has committed right now, or nothing to observe.
    ///
    /// The one reader of a live stop state, so the finalizer that snapshots a
    /// request's shutdown dimension and the bridge that names a callback's
    /// cannot take that reading under two different locks or two different
    /// spellings. A request or a callback with no server over it has nothing
    /// committed and says so.
    pub(in crate::http) fn committed_now(
        stop: Option<&super::super::server_stop::ServerStopState>,
    ) -> Option<Self> {
        stop.and_then(|stop| stop.read(Self::committed))
    }

    /// The transition one committed stop reading names, if any.
    ///
    /// A finished server has collapsed its phase into a result, so the outcome
    /// is the only field left that still says what it did.
    pub(in crate::http) const fn committed(reading: &CommittedStopReading) -> Option<Self> {
        match reading.phase {
            StopPhase::Running => None,
            StopPhase::Graceful => Some(Self::Graceful),
            StopPhase::Cancelled => Some(Self::Cancelled),
            StopPhase::TimedOut => Some(Self::DeadlineExpired),
            StopPhase::Finished => Some(Self::finished(reading.outcome)),
        }
    }

    /// How a server that has already fixed its result ended.
    const fn finished(outcome: StopOutcome) -> Self {
        match outcome {
            StopOutcome::Cancelled => Self::Cancelled,
            StopOutcome::TimedOut => Self::DeadlineExpired,
            StopOutcome::Pending | StopOutcome::Completed | StopOutcome::Failed => Self::Graceful,
        }
    }
}

/// The name an optional dimension publishes, or the stated absence.
///
/// Written once and shared by every optional dimension, so "absent" cannot be
/// spelled one way by the origin and another by the shutdown.
pub(in crate::http) fn optional_label<T: Copy>(
    value: Option<T>,
    named: impl FnOnce(T) -> &'static str,
) -> &'static str {
    match value {
        Some(value) => named(value),
        None => ABSENT,
    }
}

/// Every name an optional dimension may publish, including stated absence.
///
/// The companion of [`optional_label`], and shared for the same reason: a
/// vocabulary that omits the absence name, or spells it its own way, publishes
/// a set of labels the live counter does not stay inside.
pub(in crate::http) fn optional_vocabulary<const N: usize>(
    named: [&'static str; N],
) -> Box<[&'static str]> {
    std::iter::once(ABSENT).chain(named).collect()
}

/// The configured bound one pre-commit or post-commit terminal crossed.
///
/// The direction is the reporting owner's, and only the byte row reads it: an
/// upload maximum is never reported as the download's. Every other row names one
/// bound whichever direction observed it, or names none at all.
///
/// The three rows that name nothing are the ones where the bound is not this
/// operation's: a departing peer crossed no configured maximum, a source that
/// failed is the source's own account, and a response head arriving is the
/// inbound side reaching its normal end.
const fn crossed_by(terminal: InboundTerminal, direction: TransferDirection) -> CrossedBound {
    match terminal {
        InboundTerminal::ShutdownDeadline | InboundTerminal::ForcedCancellation => {
            CrossedBound::Deadline(DeadlineBoundary::AggregateShutdown)
        }
        InboundTerminal::RouteBodyLimit => CrossedBound::Bytes(ByteBoundary::RequestBody),
        InboundTerminal::TransferBytes => CrossedBound::Bytes(direction.bytes()),
        InboundTerminal::BodyIdle => CrossedBound::Deadline(DeadlineBoundary::RequestBodyIdle),
        InboundTerminal::TransferIdle => CrossedBound::Deadline(DeadlineBoundary::TransferIdle),
        InboundTerminal::TransferTotal => CrossedBound::Deadline(DeadlineBoundary::TransferTotal),
        InboundTerminal::RequestTotal => CrossedBound::Deadline(DeadlineBoundary::RequestTotal),
        InboundTerminal::Disconnect
        | InboundTerminal::SourceFailure
        | InboundTerminal::ResponseHead => CrossedBound::None,
    }
}

/// One admitted operation's completion facts, each written once.
///
/// Every cell is `OnceLock` rather than a replaceable field, because a second
/// writer for one dimension is a second account of one request and the peer
/// received only one of them. A repeat of the same supporting fact is dropped;
/// nothing here ranks, folds, or rewrites.
#[derive(Default)]
struct CompletionFacts {
    /// The status the peer was actually given, once a head committed.
    status: OnceLock<u16>,
    /// The producer that took this operation's response commitment.
    origin: OnceLock<ResponseOrigin>,
    /// The typed framework rejection its mapper applied, if one did.
    rejection: OnceLock<RejectionKind>,
    /// The configured bound the enforcer that ended this operation crossed.
    boundary: OnceLock<CrossedBound>,
    /// How this operation's connection ended under it, if it did.
    connection_end: OnceLock<ConnectionEnd>,
    /// Whether a pre-commit cause, rather than a producer, took the commitment.
    ///
    /// The one thing an absent origin cannot say on its own. An operation that
    /// ended on a disconnect, a reset, a cancellation, a shutdown, or a crossed
    /// bound has no producer and never will; a committed head with no producer
    /// behind it is a head Hyper wrote for an admitted operation. Both leave the
    /// origin cell empty, and only this tells them apart.
    ended_before_a_head: OnceLock<()>,
}

/// One admitted operation's account: who it is, what it publishes, and what it
/// ended with.
///
/// Shared behind an `Arc` by every owner that can name one of its facts — the
/// response commitment, the answering exit, the enforcer, the response body, and
/// the connection guard. It records nothing itself: exactly one
/// [`OperationFinalizer`](super::finalizer::OperationFinalizer) reads it at the
/// end of the response lifetime and writes the operation's one record.
pub(in crate::http) struct CompletionAccount {
    start: std::time::Instant,
    telemetry: Telemetry,
    script: Option<Arc<LifecycleScript>>,
    /// What this request is called, as the last stage to name it spelled it.
    ///
    /// Deliberately not set-once: the identity is refined as dispatch resolves a
    /// route, a class, and a representation, and the record names the request the
    /// way the stage that answered it did. It is never a completion dimension.
    /// The request identifier and the raw path it holds are event-only; the
    /// counter and the histogram read the bounded method and protocol labels off
    /// it and nothing else.
    identity: Mutex<Arc<RequestIdentity>>,
    /// What this request is called by, held apart from the name it is called.
    ///
    /// The one part of the identity refinement never changes, so it is read
    /// without the lock the refined name needs. Every admitted head asks for it
    /// once, and a `Copy` scalar behind a mutex would charge that head an
    /// acquisition and a refcount pair to answer a question no stage can
    /// re-answer.
    request_id: super::super::rejection::RequestId,
    facts: CompletionFacts,
}

impl CompletionAccount {
    /// Start the clock one served request is measured on.
    ///
    /// The identity is the one the admitted head minted, before any route, class,
    /// or representation has been established. An operation whose peer leaves
    /// before an exit answers is still named by it.
    pub(in crate::http) fn begin(
        ctx: &ConnCtx,
        script: Option<Arc<LifecycleScript>>,
        identity: RequestIdentity,
    ) -> Arc<Self> {
        Arc::new(Self {
            start: std::time::Instant::now(),
            telemetry: Telemetry::of(ctx),
            script,
            request_id: identity.request_id(),
            identity: Mutex::new(Arc::new(identity)),
            facts: CompletionFacts::default(),
        })
    }

    /// The identifier every answer to this request is keyed by.
    pub(in crate::http) const fn request_id(&self) -> super::super::rejection::RequestId {
        self.request_id
    }

    /// Record the producer that took this operation's response commitment.
    ///
    /// Reached from `response_commitment` and from nowhere else, so the dimension
    /// has one owning module. The commitment calls it for the attempt that took
    /// the cell — an owner that arrived late answered nobody, so naming it there
    /// would report a producer the peer never heard from — and for the one answer
    /// that has a producer but no cell, a head refused before any operation was
    /// minted.
    pub(in crate::http) fn record_origin(&self, origin: ResponseOrigin) {
        let _ = self.facts.origin.set(origin);
    }

    /// Record that this operation ended before any producer committed a head.
    pub(in crate::http) fn record_uncommitted_head(&self) {
        let _ = self.facts.ended_before_a_head.set(());
    }

    /// Record the typed framework rejection its mapper applied.
    pub(in crate::http) fn record_rejection(&self, kind: RejectionKind) {
        let _ = self.facts.rejection.set(kind);
    }

    /// Record the configured bound the owner that ended this operation crossed.
    ///
    /// A crossing of nothing is not an observation and is not recorded: it says
    /// only that this operation was answered, which delivery already states.
    pub(in crate::http) fn record_boundary(&self, crossed: CrossedBound) {
        match crossed {
            CrossedBound::None => {}
            named => {
                let _ = self.facts.boundary.set(named);
            }
        }
    }

    /// Record the bound one download owner's terminal crossed.
    ///
    /// The download is the direction because this is a body already producing
    /// under a committed head: the only transfer left by then is that body.
    pub(in crate::http) fn record_download_boundary(&self, terminal: InboundTerminal) {
        self.record_boundary(crossed_by(terminal, TransferDirection::Download));
    }

    /// Record how this operation's connection ended under it.
    pub(in crate::http) fn record_connection_end(&self, ended: Option<ConnectionEnd>) {
        match ended {
            Some(ended) => {
                let _ = self.facts.connection_end.set(ended);
            }
            None => {}
        }
    }

    /// Stage the account for an answer no owner fixed a bound for.
    pub(in crate::http) fn commit(&self, scope: &RejectionScope, status: u16) {
        self.stage(scope, status, CrossedBound::None);
    }

    /// Stage the account for an answer a refusal produced.
    ///
    /// The bound the refusal names is the observation, not the refusal itself: a
    /// `404` or a failed authentication crossed nothing, and delivery and the
    /// connection end are what say how that request ended.
    pub(in crate::http) fn commit_refused(
        &self,
        scope: &RejectionScope,
        status: u16,
        bound: CrossedBound,
    ) {
        self.stage(scope, status, bound);
    }

    /// Stage the account for an answer whose pre-commit terminal is already
    /// known.
    ///
    /// The upload is the direction: it is the only transfer running before a
    /// response head commits, because a download's own lifetime starts at the
    /// head this terminal prevented.
    pub(in crate::http) fn commit_terminal(
        &self,
        scope: &RejectionScope,
        status: u16,
        terminal: InboundTerminal,
    ) {
        self.stage(
            scope,
            status,
            crossed_by(terminal, TransferDirection::Upload),
        );
    }

    /// Record one answer's status and identity, and count the offer.
    ///
    /// A second offer sets nothing: the peer received the first answer, so the
    /// first status is the true one, and the name that answer went out under is
    /// the one the offer that took the cell carried. The offer is still counted,
    /// because "one answer, one account" is a claim a case has to be able to
    /// read rather than infer from a silence.
    fn stage(&self, scope: &RejectionScope, status: u16, crossed: CrossedBound) {
        LifecycleScript::observe_completion_staged(self.script.as_deref());
        if self.facts.status.set(status).is_ok() {
            *self.held_identity() = scope.identity();
        }
        self.record_boundary(crossed);
    }

    /// The identity past a lock a panicking exit poisoned.
    ///
    /// An exit that panicked between taking the lock and writing left the name
    /// exactly as it found it, so the account is still sound. Refusing to read it
    /// would turn one failed request into every later owner panicking on it.
    fn held_identity(&self) -> std::sync::MutexGuard<'_, Arc<RequestIdentity>> {
        self.identity
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub(in crate::http) const fn telemetry(&self) -> Telemetry {
        self.telemetry
    }

    pub(in crate::http) fn script(&self) -> Option<&Arc<LifecycleScript>> {
        self.script.as_ref()
    }

    pub(in crate::http) fn elapsed(&self) -> std::time::Duration {
        self.start.elapsed()
    }

    /// The name this request is recorded under, as its last naming stage left it.
    pub(in crate::http) fn identity(&self) -> Arc<RequestIdentity> {
        Arc::clone(&self.held_identity())
    }

    /// The six dimensions this operation ended with, beside the delivery and
    /// shutdown its finalizer just wrote.
    ///
    /// Taken as one value because it is one record: dimensions read one at a
    /// time by the recorder could describe two different moments of one request.
    pub(in crate::http) fn settled(
        &self,
        settled: DisconnectCause,
        shutdown: Option<ShutdownObservation>,
    ) -> CompletionSnapshot {
        let status = self.facts.status.get().copied();
        CompletionSnapshot {
            status,
            origin: self.published_origin(status.is_some()),
            rejection: self.facts.rejection.get().copied(),
            delivery: DeliveryOutcome::of(status.is_some(), settled),
            connection_end: self.facts.connection_end.get().copied(),
            boundary: self.facts.boundary.get().copied().unwrap_or_default(),
            shutdown,
        }
    }

    /// Which producer this record names, over a committed head with no Camber
    /// producer behind it.
    ///
    /// [`ResponseOrigin::Protocol`] is never written into the commitment cell
    /// because nothing reaches that cell to write it: a head Hyper wrote for an
    /// admitted operation had no Camber owner to arrive. The finalizer is the one
    /// owner that runs after the response lifetime and can see a committed status
    /// with no committed origin, so it is the one that can state it.
    ///
    /// No served request reaches that state today. Every exit that stages a
    /// status names its producer first, and a head answered before an operation
    /// was minted names one too, so the arm below is the mapping and not an
    /// observation. It is kept rather than folded into absence because the two
    /// mean opposite things, and 10.T2 proves the drive publishes neither.
    ///
    /// An operation whose commitment a pre-commit cause took is the other
    /// producerless case, and it is not this one: Camber answered it from its own
    /// cause table, so the record carries that status with no origin at all.
    fn published_origin(&self, committed: bool) -> Option<ResponseOrigin> {
        match (
            self.facts.origin.get().copied(),
            self.facts.ended_before_a_head.get().is_some(),
            committed,
        ) {
            (Some(origin), _, _) => Some(origin),
            (None, true, _) | (None, false, false) => None,
            (None, false, true) => Some(ResponseOrigin::Protocol),
        }
    }
}

/// The seven orthogonal dimensions one completion record publishes.
///
/// Every field is independent of every other: setting one erases and outranks
/// none of the rest, which is the whole difference from the singular terminal
/// this replaced.
pub(in crate::http) struct CompletionSnapshot {
    /// The status the peer was given, or absent when no head committed.
    pub(in crate::http) status: Option<u16>,
    pub(in crate::http) origin: Option<ResponseOrigin>,
    pub(in crate::http) rejection: Option<RejectionKind>,
    pub(in crate::http) delivery: DeliveryOutcome,
    pub(in crate::http) connection_end: Option<ConnectionEnd>,
    pub(in crate::http) boundary: CrossedBound,
    pub(in crate::http) shutdown: Option<ShutdownObservation>,
}

impl CompletionSnapshot {
    pub(in crate::http) fn origin_label(&self) -> &'static str {
        optional_label(self.origin, ResponseOrigin::label)
    }

    /// The typed rejection this record names, if it names one.
    ///
    /// Published only under a framework origin, because that is the only
    /// producer a typed rejection mapper answers for. A router terminal or a
    /// conversion refusal is categorised for the rejection counter under its own
    /// producer, and repeating that category here would say a framework mapper
    /// answered a request it never saw.
    pub(in crate::http) fn rejection_label(&self) -> &'static str {
        match self.origin {
            Some(ResponseOrigin::Framework) => optional_label(self.rejection, RejectionKind::label),
            _ => ABSENT,
        }
    }

    pub(in crate::http) const fn delivery_label(&self) -> &'static str {
        self.delivery.label()
    }

    pub(in crate::http) fn connection_end_label(&self) -> &'static str {
        optional_label(self.connection_end, ConnectionEnd::label)
    }

    pub(in crate::http) const fn boundary_label(&self) -> &'static str {
        self.boundary.label()
    }

    pub(in crate::http) fn shutdown_label(&self) -> &'static str {
        optional_label(self.shutdown, ShutdownObservation::label)
    }
}
