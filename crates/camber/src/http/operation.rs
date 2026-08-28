//! The one owner an admitted request's time, identity, and cancellation share.
//!
//! Hyper admits a head, the existing host and route classifier selects the
//! authority that head runs under, and exactly one [`OperationEnvelope`] is
//! minted from the two. Every pre-head owner — dispatch, the middleware gate,
//! the body reader, and the response-head handoff — reads that one value rather
//! than rebuilding a private deadline, identity, or cancellation state of its
//! own.
//!
//! A head Hyper refused, and a head that resolved no route authority, mint
//! nothing: there is no policy to run them under, so there is no operation to
//! name.

use super::disconnect::{ConnectionLiveness, DisconnectSignal};
use super::mock::LifecycleScript;
use super::rejection::Rejected;
use super::request_budget::RequestBudget;
use super::response_commitment::{OperationCommitment, ResponseOrigin};
use super::route_budgets::RouteBudgets;
use super::server_lifecycle::{ConnectionShutdownDeadline, ServerControl};
use super::transfer::{TransferDirection, TransferOwner};
use super::transfer_budget::TransferBudget;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::time::Instant;

/// Which production owner read an operation's identity.
///
/// A test seam, not API. Hidden from the documentation and outside the semver
/// promise this crate makes, on the same footing as `runtime_test_support`.
/// Reach it from a test root and nowhere else.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum OperationStage {
    /// The classified route this admitted head dispatches through.
    Dispatch,
    /// The middleware chain that surrounds the selected route.
    Middleware,
    /// The owner that reads the request payload.
    Body,
    /// The handoff that commits this request's response head.
    ResponseHead,
}

/// The closed set of inbound terminals one admitted request can end on.
///
/// A set and not a ranking. Which of two facts ends a request is decided by
/// which one commits into that operation's
/// [`OperationCommitment`](super::response_commitment::OperationCommitment)
/// first, so declaration order here carries no meaning and nothing reads it.
///
/// The three transfer rows are read by the streaming owners in
/// [`transfer`](super::transfer), which commit into their own transfer owner's
/// set-once terminal on the same rule.
///
/// A test seam, not API, on the same footing as [`OperationStage`].
#[doc(hidden)]
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum InboundTerminal {
    /// The deadline every graceful shutdown participant shares expired.
    ShutdownDeadline,
    /// The server was explicitly cancelled.
    ForcedCancellation,
    /// The route-aware body admission's byte maximum was crossed.
    RouteBodyLimit,
    /// One streaming direction's own byte maximum was crossed.
    TransferBytes,
    /// The quiet interval allowed between request body data frames expired.
    BodyIdle,
    /// The quiet interval allowed between one transfer's data frames expired.
    TransferIdle,
    /// One streaming direction's lifetime expired.
    TransferTotal,
    /// The lifetime from admitted head to committed response head expired.
    RequestTotal,
    /// The peer's response lifetime ended before an answer was possible.
    Disconnect,
    /// The inbound source failed.
    SourceFailure,
    /// The request payload ended, so the response head may be produced, or one
    /// transfer's source reached its normal end.
    ResponseHead,
}

/// The inbound sources one turn observed as ready.
///
/// A closed input to one reading. Every coordinator reads its own source or
/// producer first and the carried sources second, and
/// [`InboundReady::first_ready`] answers in that same order — so the fact this
/// owner read for itself is the fact it commits, and a carried source decides
/// only a turn its owner's own read decided nothing in.
///
/// There is no rank over the whole set. [`InboundReady::OWN_READ`] is one read
/// with one causal edge inside it; [`InboundReady::CARRIED`] is a set with none.
/// Two carried sources that first became observable in the same turn may commit
/// in either order, and both results are correct.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(super) struct InboundReady {
    shutdown_deadline: bool,
    forced_cancellation: bool,
    route_body_limit: bool,
    transfer_bytes: bool,
    body_idle: bool,
    transfer_idle: bool,
    transfer_total: bool,
    request_total: bool,
    disconnect: bool,
    source_failure: bool,
    response_head: bool,
}

impl InboundReady {
    /// Whether this turn observed one named source as ready.
    ///
    /// Wildcard-free: a terminal a later step adds fails to compile here until
    /// it names the source it is read from.
    const fn holds(self, terminal: InboundTerminal) -> bool {
        match terminal {
            InboundTerminal::ShutdownDeadline => self.shutdown_deadline,
            InboundTerminal::ForcedCancellation => self.forced_cancellation,
            InboundTerminal::RouteBodyLimit => self.route_body_limit,
            InboundTerminal::TransferBytes => self.transfer_bytes,
            InboundTerminal::BodyIdle => self.body_idle,
            InboundTerminal::TransferIdle => self.transfer_idle,
            InboundTerminal::TransferTotal => self.transfer_total,
            InboundTerminal::RequestTotal => self.request_total,
            InboundTerminal::Disconnect => self.disconnect,
            InboundTerminal::SourceFailure => self.source_failure,
            InboundTerminal::ResponseHead => self.response_head,
        }
    }

    /// Record that one transfer direction's byte maximum was crossed.
    pub(super) const fn with_transfer_bytes(self) -> Self {
        Self {
            transfer_bytes: true,
            ..self
        }
    }

    /// Record that one transfer's quiet interval expired.
    pub(super) const fn with_transfer_idle(self) -> Self {
        Self {
            transfer_idle: true,
            ..self
        }
    }

    /// Record that one transfer's lifetime expired.
    pub(super) const fn with_transfer_total(self) -> Self {
        Self {
            transfer_total: true,
            ..self
        }
    }

    /// Record that the wire ended this request's payload.
    pub(super) const fn with_response_head(self) -> Self {
        Self {
            response_head: true,
            ..self
        }
    }

    /// Record that the inbound source failed.
    pub(super) const fn with_source_failure(self) -> Self {
        Self {
            source_failure: true,
            ..self
        }
    }

    /// Record that route-aware admission refused the frame this turn read.
    pub(super) const fn with_route_body_limit(self) -> Self {
        Self {
            route_body_limit: true,
            ..self
        }
    }

    /// Record one terminal a local source outside this turn already fixed.
    ///
    /// The gRPC pre-head coordinator is the owner this exists for: tonic holds
    /// the upload body, so the terminal that upload fixes is reported to the
    /// coordinator rather than observed by it. Folding the report into the turn
    /// is what lets it be weighed against a tonic head that became ready
    /// alongside it, under the read order every coordinator here shares rather
    /// than by which of the two this handoff happened to poll first.
    ///
    /// Wildcard-free for the same reason [`Self::holds`] is: a terminal a later
    /// step adds fails to compile here until it names its source.
    const fn with_local(self, terminal: InboundTerminal) -> Self {
        match terminal {
            InboundTerminal::ShutdownDeadline => Self {
                shutdown_deadline: true,
                ..self
            },
            InboundTerminal::ForcedCancellation => Self {
                forced_cancellation: true,
                ..self
            },
            InboundTerminal::RouteBodyLimit => self.with_route_body_limit(),
            InboundTerminal::TransferBytes => self.with_transfer_bytes(),
            InboundTerminal::BodyIdle => Self {
                body_idle: true,
                ..self
            },
            InboundTerminal::TransferIdle => self.with_transfer_idle(),
            InboundTerminal::TransferTotal => self.with_transfer_total(),
            InboundTerminal::RequestTotal => Self {
                request_total: true,
                ..self
            },
            InboundTerminal::Disconnect => Self {
                disconnect: true,
                ..self
            },
            InboundTerminal::SourceFailure => self.with_source_failure(),
            InboundTerminal::ResponseHead => self.with_response_head(),
        }
    }

    /// This owner's own read, in the order the edges inside it establish.
    ///
    /// Not a rank. The frame this turn took off its source, the maximum that
    /// frame crossed, the transport that failed under it, and the payload's end
    /// are all one read, and the causal edge inside it is the peer's own end: a
    /// source that failed in the same turn a peer's response lifetime ended did
    /// not fail beside that fact, it failed because of it. So the peer's end is
    /// the earlier of the two, and the read failure it caused is not a competing
    /// answer.
    const OWN_READ: [InboundTerminal; 5] = [
        InboundTerminal::RouteBodyLimit,
        InboundTerminal::TransferBytes,
        InboundTerminal::Disconnect,
        InboundTerminal::SourceFailure,
        InboundTerminal::ResponseHead,
    ];

    /// The sources this owner carries but did not read for itself.
    ///
    /// Unordered, and named as a set rather than a table: no public action,
    /// owner commit, or protocol acknowledgement separates two of these that
    /// first became observable together, so either is a correct answer and this
    /// array only decides which one a rerun of the same turn produces. Nothing
    /// may state one of them as outranking another — a case whose two facts are
    /// both here owes its closed result set and identical cleanup for every
    /// member.
    const CARRIED: [InboundTerminal; 6] = [
        InboundTerminal::ShutdownDeadline,
        InboundTerminal::ForcedCancellation,
        InboundTerminal::BodyIdle,
        InboundTerminal::TransferIdle,
        InboundTerminal::TransferTotal,
        InboundTerminal::RequestTotal,
    ];

    /// The fact this turn commits, or `None` while the request runs on.
    ///
    /// The owner's own read is asked first because that read is what the turn
    /// started with, and so what this owner commits. A carried source decides
    /// only a turn its owner's own read decided nothing in.
    pub(super) fn first_ready(self) -> Option<InboundTerminal> {
        Self::OWN_READ
            .into_iter()
            .chain(Self::CARRIED)
            .find(|terminal| self.holds(*terminal))
    }
}

/// The identity one admitted operation is known by at every owner it reaches.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(super) struct OperationId(u64);

impl OperationId {
    /// The next identity in this process.
    ///
    /// Relaxed because the only claim made of it is distinctness: nothing
    /// orders two operations by their identities, and the counter publishes no
    /// other memory.
    fn mint() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }

    pub(super) const fn value(self) -> u64 {
        self.0
    }
}

/// One admitted request's carried time, identity, and cancellation authority.
///
/// Deliberately not `Clone`. One admitted head owns one envelope; the narrow
/// handles below are what move to the single body, upgrade, or response owner
/// that needs them, so no second owner can hold a second copy of a deadline
/// that is meant to be one.
pub(super) struct OperationEnvelope {
    id: OperationId,
    /// The effective request and transfer policies, already narrowed
    /// outer-to-inner by the classifier that selected this route's authority.
    budgets: RouteBudgets,
    /// The absolute instant this request's total deadline expires at, computed
    /// once from the admitted head.
    total: Option<Instant>,
    /// The server's shutdown and forced-cancellation authority.
    ///
    /// `None` is a connection with no supervisor to hear from, which is the
    /// detached synchronous callback case. It never observes either terminal.
    control: Option<tokio::sync::watch::Receiver<ServerControl>>,
    /// When a graceful shutdown ends for this request.
    ///
    /// Read rather than derived: an admitted request that observes a graceful
    /// transition answers to the instant the transition minted, narrowed by its
    /// own server's configured grace, not to a fresh copy of that grace started
    /// when this connection happened to notice.
    shutdown: ConnectionShutdownDeadline,
    connection: Arc<ConnectionLiveness>,
    disconnect: DisconnectSignal,
    /// The one response commitment every producer of this operation shares.
    ///
    /// Shared rather than owned, because the producers are concurrent: an
    /// upload measuring its route's maximum, a coordinator holding an upstream
    /// head, and this operation's own carried deadlines are three owners of one
    /// answer, and the cell is what makes only the first of them the answer.
    commitment: Arc<OperationCommitment>,
}

impl OperationEnvelope {
    /// Mint the one envelope an admitted head carries.
    pub(super) fn admit(
        budgets: RouteBudgets,
        control: Option<tokio::sync::watch::Receiver<ServerControl>>,
        connection: &Arc<ConnectionLiveness>,
        disconnect: &DisconnectSignal,
        shutdown: ConnectionShutdownDeadline,
        script: Option<&Arc<LifecycleScript>>,
        account: &Arc<super::completion::CompletionAccount>,
    ) -> Self {
        let admitted_at = Instant::now();
        let request = budgets.request();
        let id = OperationId::mint();
        let envelope = Self {
            id,
            commitment: OperationCommitment::minted(
                id,
                script.map(Arc::clone),
                Arc::clone(account),
            ),
            budgets,
            total: request.total().map(|total| admitted_at + total),
            control,
            shutdown,
            connection: Arc::clone(connection),
            disconnect: disconnect.clone(),
        };
        LifecycleScript::observe_operation_admitted(
            script.map(Arc::as_ref),
            envelope.id,
            request.total(),
        );
        envelope
    }

    /// Publish this operation's identity from the owner that reached `stage`.
    pub(super) fn observe(&self, script: Option<&LifecycleScript>, stage: OperationStage) {
        LifecycleScript::observe_operation(script, self.id, stage);
    }

    /// The one response commitment every producer of this operation shares.
    pub(super) const fn commitment(&self) -> &Arc<OperationCommitment> {
        &self.commitment
    }

    /// Record one produced response head against this operation's commitment.
    ///
    /// A producer arriving after the answer was decided is counted and keeps
    /// its own answer: the peer it would tell is either gone or already served,
    /// and the cell is what keeps a later cause from mapping a second status.
    pub(super) fn commit_response(&self, origin: ResponseOrigin) {
        self.commitment.record_head(origin);
    }

    /// The effective request policy this operation runs under.
    pub(super) const fn budget(&self) -> RequestBudget {
        self.budgets.request()
    }

    /// The narrow inbound handle one payload owner takes.
    ///
    /// Built per owner rather than stored, so the idle interval each owner
    /// measures starts when that owner started reading rather than when the
    /// head was admitted.
    pub(super) fn inbound(&self) -> InboundGuard {
        self.guard(self.budgets.request().body_idle(), self.total)
    }

    /// The narrow upload owner one streaming request consumer takes.
    ///
    /// The consumer's own registration policy is narrowed under the route's,
    /// which the classifier already narrowed under the host's, the server's, and
    /// the runtime's. The carried request deadlines travel with it: an upload
    /// runs before the response head commits, so its turn weighs the request's
    /// idle and total against the transfer's own two.
    pub(super) fn upload(
        &self,
        registration: TransferBudget,
        observer: Option<Arc<LifecycleScript>>,
    ) -> TransferOwner {
        TransferOwner::new(
            TransferDirection::Upload,
            registration.narrowed_by(self.budgets.upload()),
            InboundWatch::over(self.inbound()),
            observer,
        )
    }

    /// The narrow upload owner one gRPC request body takes.
    ///
    /// The request total is deliberately absent, unlike [`Self::upload`]. The
    /// gRPC pre-head coordinator weighs that deadline itself against tonic's
    /// head, and tonic keeps this body after the head commits — where
    /// request-total enforcement has already ended. What travels with it is the
    /// request's quiet interval, which stays this payload's own bound for as
    /// long as tonic reads it.
    #[cfg(feature = "grpc")]
    pub(super) fn handoff_upload(
        &self,
        registration: TransferBudget,
        observer: Option<Arc<LifecycleScript>>,
    ) -> TransferOwner {
        TransferOwner::new(
            TransferDirection::Upload,
            registration.narrowed_by(self.budgets.upload()),
            InboundWatch::over(self.guard(self.budgets.request().body_idle(), None)),
            observer,
        )
    }

    /// The narrow download owner one streaming response body takes.
    ///
    /// The request deadlines are deliberately absent. A download begins at a
    /// committed response head, and request-total enforcement ends there, so a
    /// body that carried them would answer to a deadline that is no longer
    /// reachable and charge response time to the request that produced it.
    pub(super) fn download(
        &self,
        registration: TransferBudget,
        observer: Option<Arc<LifecycleScript>>,
    ) -> TransferOwner {
        TransferOwner::new(
            TransferDirection::Download,
            registration.narrowed_by(self.budgets.download()),
            InboundWatch::over(self.guard(None, None)),
            observer,
        )
    }

    /// Run one pre-commit producer under this operation's carried authority.
    ///
    /// For an owner whose answer is a whole future rather than a stream of
    /// frames — the streaming proxy's upstream head is the one this exists for.
    /// The producer's answer is this coordinator's own read, so a turn it
    /// answered in is a turn it settles: an upstream head this service accepted
    /// at its own commit barrier is the answer even when a shutdown deadline, a
    /// forced cancellation, a peer whose lifetime ended, or an expired request
    /// total became observable in the same scheduling turn. Those carried
    /// sources decide only a turn the producer has not answered in.
    ///
    /// The quiet interval is deliberately absent: the payload owner accounts for
    /// data frames, and a second guard that never sees one would report a
    /// healthy upload as an idle body.
    ///
    /// The producer takes the commitment itself, at its own commit barrier —
    /// the upstream head this proxy accepted, or the head tonic handed over —
    /// because that barrier is where the answer stops being revocable. What this
    /// coordinator owns is the other side: a carried cause may take the cell
    /// while it is still free, and once anything holds it, no carried cause maps
    /// and the producer's answer is the one this request gives.
    pub(super) async fn pre_commit<T>(
        &self,
        producing: impl Future<Output = T>,
        observer: Option<&LifecycleScript>,
    ) -> Result<T, PreCommitCause> {
        self.pre_commit_beside(producing, std::future::pending(), observer)
            .await
    }

    /// Run one pre-commit producer beside a local source of the caller's own.
    ///
    /// [`Self::pre_commit`] with one more source in the turn. `local` is a
    /// terminal some other owner already fixed and reported — the gRPC upload
    /// body tonic holds is the owner this exists for, because a body inside
    /// tonic cannot be observed from here and its terminal must still be
    /// weighed against tonic's head in the one turn both became ready in.
    ///
    /// A `local` that never resolves is exactly [`Self::pre_commit`], so the
    /// two share one selector rather than one deciding a tie the other cannot.
    pub(super) async fn pre_commit_beside<T>(
        &self,
        producing: impl Future<Output = T>,
        local: impl Future<Output = PreCommitCause>,
        observer: Option<&LifecycleScript>,
    ) -> Result<T, PreCommitCause> {
        let selected = self.select_pre_commit(producing, local).await;
        let committed = match &selected {
            Ok(_) => None,
            Err(cause) => Some(cause.terminal),
        };
        LifecycleScript::pause_at_settled_commit(observer, committed).await;
        selected
    }

    /// Run one pre-commit producer until this operation's answer is settled.
    ///
    /// One turn reads the producer, then the local report, then the carried
    /// sources. A producer that answered owns the answer, because whatever it
    /// answered with was decided at its own commit barrier. Otherwise the first
    /// fact this turn read attempts the shared cell: taking it ends the request
    /// on that cause, and finding it taken means the producer already owns the
    /// answer and this turn only waits for it.
    async fn select_pre_commit<T>(
        &self,
        producing: impl Future<Output = T>,
        local: impl Future<Output = PreCommitCause>,
    ) -> Result<T, PreCommitCause> {
        let mut watch = InboundWatch::over(self.guard(None, self.total));
        let mut producing = std::pin::pin!(producing);
        let mut local = std::pin::pin!(local);
        let mut reported: Option<PreCommitCause> = None;
        // Whether the shared cell was found taken by some other owner. Once it
        // is, this turn stops reading carried sources at all: they can no longer
        // change the answer, and re-reading an expired deadline every turn would
        // spin this coordinator against its own wake.
        let mut settled = false;
        std::future::poll_fn(|cx| {
            // Read first, commit second — the one order every coordinator here
            // shares. The producer's answer is this owner's own read, so a turn
            // it answered in is a turn it settles in. It is deliberately absent
            // from the carried set below: a producer that answered has already
            // settled this turn, and folding it in would make the answer one more
            // source to be weighed against rather than the fact it is.
            if let Some(value) = answered(producing.as_mut(), cx) {
                return std::task::Poll::Ready(Ok(value));
            }
            if settled {
                return std::task::Poll::Pending;
            }
            reported = reported.take().or_else(|| answered(local.as_mut(), cx));
            let carried = watch.observed(cx, None);
            let ready = reported
                .as_ref()
                .map_or(carried, |cause| carried.with_local(cause.terminal));
            let Some(terminal) = ready.first_ready() else {
                return std::task::Poll::Pending;
            };
            let cause = selected_cause(terminal, reported.take());
            match self.commitment.commit_cause(cause.terminal) {
                Ok(()) => std::task::Poll::Ready(Err(cause)),
                // Already committed, so this cause maps nothing. The owner that
                // took the cell answers this request, and the only thing left
                // for this turn is the wait for it.
                Err(_committed) => {
                    settled = true;
                    std::task::Poll::Pending
                }
            }
        })
        .await
    }

    /// One inbound handle over the two request deadlines its owner enforces.
    fn guard(&self, idle: Option<Duration>, total: Option<Instant>) -> InboundGuard {
        InboundGuard {
            idle,
            total,
            control: self.control.clone(),
            shutdown: self.shutdown.clone(),
            shutdown_deadline: None,
            connection: Arc::clone(&self.connection),
            disconnect: self.disconnect.clone(),
            quiet_since: Instant::now(),
        }
    }

    /// The remaining request-total time, or `None` when the total is unbounded.
    ///
    /// Zero is a deadline already crossed, which is what the handler and
    /// response-head owners answer as [`InboundTerminal::RequestTotal`].
    pub(super) fn remaining_total(&self) -> Option<Duration> {
        self.total
            .map(|total| total.saturating_duration_since(Instant::now()))
    }
}

/// The inbound authority one payload owner holds.
///
/// It answers two questions and nothing else: which sources are ready in this
/// turn, and how long the turn may wait before one of them becomes ready. The
/// terminal itself is selected by [`InboundReady::first_ready`], which every
/// coordinator shares.
pub(super) struct InboundGuard {
    idle: Option<Duration>,
    total: Option<Instant>,
    control: Option<tokio::sync::watch::Receiver<ServerControl>>,
    /// The shared aggregate this owner's shutdown answer comes from.
    shutdown: ConnectionShutdownDeadline,
    /// The instant the graceful shutdown this owner observed expires at, read
    /// once from the shared aggregate and never re-read.
    shutdown_deadline: Option<Instant>,
    connection: Arc<ConnectionLiveness>,
    disconnect: DisconnectSignal,
    /// When the quiet interval this owner measures last restarted.
    quiet_since: Instant,
}

impl InboundGuard {
    /// Account one admitted data frame against the quiet interval.
    ///
    /// Only a frame that delivered payload restarts it. Trailers and empty
    /// frames carry no bytes, so a peer cannot hold a body open by sending
    /// them.
    pub(super) fn frame_delivered(&mut self, delivered: usize) {
        match delivered {
            0 => {}
            _ => self.quiet_since = Instant::now(),
        }
    }

    /// Every source this turn can decide without touching the wire.
    ///
    /// Read as a whole turn so the answer is reproducible, not so it is ranked.
    /// A shutdown that expires in the same turn as a body-idle deadline has no
    /// edge to either side of it: whichever of them reaches this operation's
    /// response commitment first owns the answer, and both are correct results
    /// of the same race. Reading them together only keeps a rerun of the same
    /// turn from answering differently.
    pub(super) fn observed(&mut self) -> InboundReady {
        let now = Instant::now();
        let control = self.control_state();
        InboundReady {
            shutdown_deadline: self
                .shutdown_deadline
                .is_some_and(|deadline| now >= deadline),
            forced_cancellation: matches!(control, Some(ServerControl::Abort)),
            route_body_limit: false,
            transfer_bytes: false,
            body_idle: self
                .idle
                .is_some_and(|idle| now.saturating_duration_since(self.quiet_since) >= idle),
            transfer_idle: false,
            transfer_total: false,
            request_total: self.total.is_some_and(|total| now >= total),
            disconnect: self.connection.is_terminating() || self.disconnect.observed().is_some(),
            source_failure: false,
            response_head: false,
        }
    }

    /// Read the supervisor's control state, taking the shutdown deadline the
    /// first time a graceful transition is observed.
    ///
    /// The deadline is READ from the aggregate every participant shares, not
    /// derived here: the transition that published `Graceful` already fixed the
    /// one instant, so a connection that observes it late gets the time left
    /// rather than a fresh copy of the grace. A later turn that still sees
    /// `Graceful` reads back what the first turn took.
    fn control_state(&mut self) -> Option<ServerControl> {
        let control = *self.control.as_ref()?.borrow();
        match (control, self.shutdown_deadline) {
            (ServerControl::Graceful, None) => {
                self.shutdown_deadline = Some(self.shutdown.deadline());
            }
            (ServerControl::Graceful | ServerControl::Abort | ServerControl::Running, _) => {}
        }
        Some(control)
    }

    /// Wait until one source this guard owns can change the next turn's answer.
    ///
    /// It resolves rather than returning a terminal: the caller re-collects the
    /// whole turn afterwards, so a source that became ready alongside the wire
    /// is still weighed against it.
    pub(super) async fn quiet(&mut self) {
        let deadline = self.next_deadline();
        // Cloned out first: the control receiver below is borrowed mutably, and
        // the signal is a refcount that every clone resolves through alike.
        let disconnect = self.disconnect.clone();
        match (self.control.as_mut(), deadline) {
            (Some(control), Some(deadline)) => {
                tokio::select! {
                    biased;
                    _ = control.changed() => {}
                    _ = disconnect.cancelled() => {}
                    () = tokio::time::sleep_until(deadline) => {}
                }
            }
            (Some(control), None) => {
                tokio::select! {
                    biased;
                    _ = control.changed() => {}
                    _ = disconnect.cancelled() => {}
                }
            }
            (None, Some(deadline)) => {
                tokio::select! {
                    biased;
                    _ = disconnect.cancelled() => {}
                    () = tokio::time::sleep_until(deadline) => {}
                }
            }
            (None, None) => {
                disconnect.cancelled().await;
            }
        }
    }

    /// The earliest instant any carried deadline can change the answer.
    fn next_deadline(&self) -> Option<Instant> {
        [
            self.idle.map(|idle| self.quiet_since + idle),
            self.total,
            self.shutdown_deadline,
        ]
        .into_iter()
        .flatten()
        .min()
    }
}

/// The cause one pre-commit turn ended an admitted request on.
///
/// It carries the refusal the owner that measured the bound already minted, so
/// no later stage restates a maximum or a deadline it did not enforce. A cause
/// this operation observed itself has none: the shared failure table names it
/// from the request policy the operation carries.
pub(super) struct PreCommitCause {
    terminal: InboundTerminal,
    wire: Option<Box<Rejected>>,
}

impl PreCommitCause {
    /// The cause a local owner reported, with the refusal it minted.
    ///
    /// Only the gRPC handoff reports one: every other pre-commit source is
    /// observed by this operation itself, through [`Self::observed`].
    #[cfg(feature = "grpc")]
    pub(super) fn reported(terminal: InboundTerminal, wire: Option<Rejected>) -> Self {
        Self {
            terminal,
            wire: wire.map(Box::new),
        }
    }

    /// The cause this operation's own carried sources produced.
    const fn observed(terminal: InboundTerminal) -> Self {
        Self {
            terminal,
            wire: None,
        }
    }

    pub(super) const fn terminal(&self) -> InboundTerminal {
        self.terminal
    }

    /// The refusal the measuring owner minted, given up to the failure table.
    pub(super) fn wire(self) -> Option<Rejected> {
        self.wire.map(|wire| *wire)
    }
}

/// The cause one selected terminal is answered with.
///
/// A local owner's report is used only for the terminal it actually named: a
/// different terminal this turn selected is this operation's own cause, and the
/// refusal the upload minted for a bound nothing selected has nothing to say
/// about it.
fn selected_cause(terminal: InboundTerminal, reported: Option<PreCommitCause>) -> PreCommitCause {
    match reported {
        Some(cause) if cause.terminal == terminal => cause,
        Some(_) | None => PreCommitCause::observed(terminal),
    }
}

/// The answer one producer has given, if it has given one this turn.
fn answered<T>(
    producing: std::pin::Pin<&mut impl Future<Output = T>>,
    cx: &mut std::task::Context<'_>,
) -> Option<T> {
    match producing.poll(cx) {
        std::task::Poll::Ready(value) => Some(value),
        std::task::Poll::Pending => None,
    }
}

/// The inbound authority one poll-driven owner holds.
///
/// A streaming transfer is driven from inside a body poll or a `poll_fn`, not
/// from a `select!`, so it cannot await [`InboundGuard::quiet`]. This wraps the
/// same guard and answers the same question one poll at a time: which sources
/// are ready now, and wake me when one of them can change that.
///
/// What it arms a wake for is the deadlines, the peer's response lifetime, and
/// the supervisor's next control transition. The transition is a wake and not
/// only a synchronous read because an owner whose other wakes are a deadline
/// far away, or a producer that is a single future, would otherwise stay parked
/// through the stop that ends it: a graceful transition mints the aggregate
/// deadline as it is read, and nothing would bring the owner back to read it.
pub(super) struct InboundWatch {
    guard: InboundGuard,
    /// The peer's response lifetime, awaited through one future built once. The
    /// signal resolves once, so nothing ever rebuilds it.
    lifetime: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>,
    /// The supervisor's next control transition, awaited through one future.
    ///
    /// Rebuilt after each transition, because a graceful stop escalates to a
    /// cancellation and both are transitions this owner must observe. `None` is
    /// an owner with no supervisor to answer to.
    transition: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>>,
    /// The deadline wake in flight, and the instant it was armed for.
    ///
    /// Kept across polls rather than rebuilt per frame: a wake armed for an
    /// interval that has since moved later only fires early, and the turn it
    /// wakes re-reads every source and arms the correct one. A wake armed for an
    /// instant later than the earliest deadline would miss it, so that is the
    /// one case this rebuilds for.
    sleep: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    armed: Option<Instant>,
}

impl InboundWatch {
    /// Wrap one inbound handle for an owner that reads it from a poll.
    pub(super) fn over(guard: InboundGuard) -> Self {
        let signal = guard.disconnect.clone();
        let transition = transition_wake(guard.control.as_ref());
        Self {
            guard,
            lifetime: Box::pin(async move {
                signal.cancelled().await;
            }),
            transition,
            sleep: None,
            armed: None,
        }
    }

    /// Every source this turn can decide, with the wake for the next turn armed.
    ///
    /// `extra` is the caller's own earliest deadline — a transfer's quiet
    /// interval or lifetime — so one wake covers both families rather than each
    /// owner arming a timer of its own.
    ///
    /// The sources are read before the deadline wake is armed, because reading
    /// them is what mints the aggregate shutdown deadline: armed first, the wake
    /// would cover every deadline except the one that turn just created, and the
    /// owner would park on a stop it had already observed.
    pub(super) fn observed(
        &mut self,
        cx: &mut std::task::Context<'_>,
        extra: Option<Instant>,
    ) -> InboundReady {
        let _ = self.lifetime.as_mut().poll(cx);
        let _ = self
            .transition
            .as_mut()
            .map(|transition| transition.as_mut().poll(cx));
        let ready = self.guard.observed();
        self.arm(cx, extra);
        ready
    }

    /// Arm the wake for the earliest deadline this turn can be changed by.
    fn arm(&mut self, cx: &mut std::task::Context<'_>, extra: Option<Instant>) {
        let wanted = [self.guard.next_deadline(), extra]
            .into_iter()
            .flatten()
            .min();
        let Some(wanted) = wanted else {
            return;
        };
        // A wake already armed no later than the earliest deadline still covers
        // it, so long as it has not already fired: a fired wake registers no
        // waker, and returning on one would park this owner with nothing left to
        // wake it.
        let covering = self.armed.is_some_and(|armed| armed <= wanted);
        let held = covering
            && self
                .sleep
                .as_mut()
                .is_some_and(|sleep| sleep.as_mut().poll(cx).is_pending());
        if held {
            return;
        }
        let mut sleep = Box::pin(tokio::time::sleep_until(wanted));
        match sleep.as_mut().poll(cx) {
            // Already passed, which an elapsed sleep registers no waker for. The
            // turn that reads it has to be asked for, or this owner parks on a
            // deadline it has just learned it is already past.
            std::task::Poll::Ready(()) => cx.waker().wake_by_ref(),
            std::task::Poll::Pending => {}
        }
        self.sleep = Some(sleep);
        self.armed = Some(wanted);
    }
}

/// One wake for every state the supervisor publishes, if there is a supervisor.
///
/// It never completes, which is what lets one future cover every transition: an
/// owner that replaced a resolved wake would have to rebuild it inside its own
/// poll, and a rebuilt wake over a closed channel resolves on every poll. The
/// clone marks the value in hand as seen, so the first wake is the next
/// transition and not the one this owner has already read.
fn transition_wake(
    control: Option<&tokio::sync::watch::Receiver<ServerControl>>,
) -> Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>> {
    let mut control = control.cloned()?;
    Some(Box::pin(async move {
        loop {
            match control.changed().await {
                // One turn per published transition. The owner re-reads every
                // source in that turn, and this wait resumes for the next one,
                // so a graceful stop and the cancellation behind it are both
                // observed through the one future.
                Ok(()) => tokio::task::yield_now().await,
                // A closed sender is the supervisor gone. It publishes nothing
                // more, so this wake goes quiet rather than waking an owner
                // whose remaining sources still have to decide the request.
                Err(_closed) => std::future::pending().await,
            }
        }
    }))
}

/// How one admitted request's inbound work ended when it did not complete.
///
/// The two arms are the declared mapping's two dispositions, not a success and
/// a failure: a mapped cause still owes the peer the one response its route's
/// mapper produces, while a silent cause has no response to give at all.
pub(super) enum InboundFailure {
    /// The cause is answered through the selected rejection mapper, once.
    Mapped(Rejected),
    /// The cause ends the transport and releases ownership without a mapped
    /// response.
    Silent(InboundTerminal),
}

impl InboundFailure {
    /// The failure one selected terminal produces.
    ///
    /// `wire` is the refusal this turn's wire read already minted — the
    /// route-aware limit that dropped a crossing frame, or the transport's own
    /// account of a source failure. Both rows carry it, and neither restates a
    /// bound the producer already named.
    pub(super) fn of(
        terminal: InboundTerminal,
        budget: RequestBudget,
        wire: Option<Rejected>,
    ) -> Self {
        match mapped_refusal(terminal, budget, wire) {
            Some(rejected) => Self::Mapped(rejected),
            None => Self::Silent(terminal),
        }
    }

    /// The failure one cause produces when it did not take the commitment.
    ///
    /// A late owner maps nothing, whatever the shared table would have given
    /// it: the response this operation gives is already another producer's, and
    /// a second mapped answer would be a second status for one request.
    pub(super) const fn silent(terminal: InboundTerminal) -> Self {
        Self::Silent(terminal)
    }
}

/// The refusal one mapped terminal answers with.
///
/// `None` is a silent row: the declared mapping gives it no mapper, so the
/// transport ends and ownership releases without a mapped response. A row whose
/// producer is missing its own refusal falls here too, which closes the
/// transport rather than inventing a category for a bound nothing named.
fn mapped_refusal(
    terminal: InboundTerminal,
    budget: RequestBudget,
    wire: Option<Rejected>,
) -> Option<Rejected> {
    match terminal {
        // The three transfer rows join the two that already travel this way:
        // the owner that measured the bound minted the refusal that names it,
        // and no later stage restates a maximum or a deadline it did not
        // enforce.
        InboundTerminal::RouteBodyLimit
        | InboundTerminal::TransferBytes
        | InboundTerminal::TransferIdle
        | InboundTerminal::TransferTotal
        | InboundTerminal::SourceFailure => wire,
        InboundTerminal::BodyIdle => budget.body_idle().map(Rejected::body_timeout),
        InboundTerminal::RequestTotal => budget.total().map(Rejected::request_timeout),
        InboundTerminal::ShutdownDeadline
        | InboundTerminal::ForcedCancellation
        | InboundTerminal::Disconnect
        | InboundTerminal::ResponseHead => None,
    }
}
