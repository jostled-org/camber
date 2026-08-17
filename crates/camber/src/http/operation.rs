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
use super::route_budgets::RouteBudgets;
use super::server_lifecycle::ServerControl;
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
/// Ordered by the one declared precedence every pre-commit coordinator shares.
/// A terminal fixed in an earlier turn is immutable; for sources first observed
/// in the same turn, the earliest row here wins.
///
/// The three transfer rows are read by the streaming owners in
/// [`transfer`](super::transfer), which extend this one selector rather than
/// keeping a precedence table of their own. A later step that adds a terminal
/// adds a variant and an [`InboundTerminal::ORDER`] row; it does not redefine
/// the order already declared.
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

impl InboundTerminal {
    /// The declared precedence, highest first.
    ///
    /// Named exhaustively rather than derived from declaration order: the order
    /// is the contract, and a variant reordered for readability must not
    /// silently reorder the terminals a live service selects.
    const ORDER: [Self; 11] = [
        Self::ShutdownDeadline,
        Self::ForcedCancellation,
        Self::RouteBodyLimit,
        Self::TransferBytes,
        Self::BodyIdle,
        Self::TransferIdle,
        Self::TransferTotal,
        Self::RequestTotal,
        Self::Disconnect,
        Self::SourceFailure,
        Self::ResponseHead,
    ];
}

/// The inbound sources one scheduling turn observed as ready.
///
/// A closed input to one selector. Every coordinator collects readiness for a
/// whole turn before it decides, so two sources that became ready together are
/// resolved by the declared order rather than by whichever future the executor
/// happened to poll first.
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

    /// The one terminal this turn selects, or `None` while the request runs on.
    pub(super) fn select(self) -> Option<InboundTerminal> {
        InboundTerminal::ORDER
            .into_iter()
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
    shutdown_timeout: Duration,
    connection: Arc<ConnectionLiveness>,
    disconnect: DisconnectSignal,
}

impl OperationEnvelope {
    /// Mint the one envelope an admitted head carries.
    pub(super) fn admit(
        budgets: RouteBudgets,
        control: Option<tokio::sync::watch::Receiver<ServerControl>>,
        connection: &Arc<ConnectionLiveness>,
        disconnect: &DisconnectSignal,
        shutdown_timeout: Duration,
        script: Option<&LifecycleScript>,
    ) -> Self {
        let admitted_at = Instant::now();
        let request = budgets.request();
        let envelope = Self {
            id: OperationId::mint(),
            budgets,
            total: request.total().map(|total| admitted_at + total),
            control,
            shutdown_timeout,
            connection: Arc::clone(connection),
            disconnect: disconnect.clone(),
        };
        LifecycleScript::observe_operation_admitted(script, envelope.id, request.total());
        envelope
    }

    /// Publish this operation's identity from the owner that reached `stage`.
    pub(super) fn observe(&self, script: Option<&LifecycleScript>, stage: OperationStage) {
        LifecycleScript::observe_operation(script, self.id, stage);
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
    /// The producer's answer is weighed as one source among the carried ones, so
    /// a shutdown deadline, a forced cancellation, a peer whose lifetime ended,
    /// or an expired request total that became observable in the same scheduling
    /// turn is selected by the declared precedence before an upstream head this
    /// service has not committed.
    ///
    /// The quiet interval is deliberately absent: the payload owner accounts for
    /// data frames, and a second guard that never sees one would report a
    /// healthy upload as an idle body.
    pub(super) async fn pre_commit<T>(
        &self,
        producing: impl Future<Output = T>,
    ) -> Result<T, InboundTerminal> {
        let mut watch = InboundWatch::over(self.guard(None, self.total));
        let mut producing = std::pin::pin!(producing);
        let mut produced = None;
        std::future::poll_fn(|cx| {
            // Read first, weigh second — the one order every coordinator here
            // shares. A producer that answered in this turn is what makes the
            // turn a tie at all, and reading it after the carried sources would
            // decide the tie by poll order instead of by the declared one.
            produced = produced.take().or_else(|| answered(producing.as_mut(), cx));
            let ready = weighed(&mut watch, cx, produced.is_some());
            match (ready.select(), produced.take()) {
                (Some(InboundTerminal::ResponseHead), Some(value)) => {
                    std::task::Poll::Ready(Ok(value))
                }
                (Some(InboundTerminal::ResponseHead) | None, held) => {
                    produced = held;
                    std::task::Poll::Pending
                }
                // The head in hand is released rather than delivered: a local
                // cause selected in the same turn ends this request, and
                // committing the answer first would commit one to a request
                // this service has already ended.
                (Some(terminal), _) => std::task::Poll::Ready(Err(terminal)),
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
            shutdown_timeout: self.shutdown_timeout,
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
/// terminal itself is selected by [`InboundReady::select`], which every
/// coordinator shares.
pub(super) struct InboundGuard {
    idle: Option<Duration>,
    total: Option<Instant>,
    control: Option<tokio::sync::watch::Receiver<ServerControl>>,
    shutdown_timeout: Duration,
    /// The instant a graceful shutdown's deadline expires at, minted at the
    /// first turn that observed the transition and never re-minted.
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
    /// Collected as a whole turn before anything is selected, so a shutdown
    /// that expires in the same turn as a body-idle deadline is answered by the
    /// declared order rather than by poll order.
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

    /// Read the supervisor's control state, minting the shutdown deadline the
    /// first time a graceful transition is observed.
    ///
    /// The mint is once per owner and never restarted: a later turn that still
    /// sees `Graceful` reads the deadline the first turn fixed.
    fn control_state(&mut self) -> Option<ServerControl> {
        let control = *self.control.as_ref()?.borrow();
        match (control, self.shutdown_deadline) {
            (ServerControl::Graceful, None) => {
                self.shutdown_deadline = Some(Instant::now() + self.shutdown_timeout);
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

/// Every source one pre-commit turn can decide, the producer's included.
fn weighed(
    watch: &mut InboundWatch,
    cx: &mut std::task::Context<'_>,
    answered: bool,
) -> InboundReady {
    let carried = watch.observed(cx, None);
    match answered {
        true => carried.with_response_head(),
        false => carried,
    }
}

/// The inbound authority one poll-driven owner holds.
///
/// A streaming transfer is driven from inside a body poll or a `poll_fn`, not
/// from a `select!`, so it cannot await [`InboundGuard::quiet`]. This wraps the
/// same guard and answers the same question one poll at a time: which sources
/// are ready now, and wake me when one of them can change that.
///
/// What it arms a wake for is the deadlines and the peer's response lifetime.
/// The supervisor's control state is read synchronously instead, every turn,
/// because a graceful transition does not end an admitted operation — the
/// deadline it mints does, and minting that deadline is what arms the wake that
/// ends the work. A forced cancellation takes the connection's task with it, so
/// the transfer's own release runs through its drop.
pub(super) struct InboundWatch {
    guard: InboundGuard,
    /// The peer's response lifetime, awaited through one future built once. The
    /// signal resolves once, so nothing ever rebuilds it.
    lifetime: std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + Sync>>,
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
        Self {
            guard,
            lifetime: Box::pin(async move {
                signal.cancelled().await;
            }),
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
    /// Interest is registered before the sources are read, which is what makes
    /// a source that becomes ready between the two visible to this turn instead
    /// of parking the owner behind it.
    pub(super) fn observed(
        &mut self,
        cx: &mut std::task::Context<'_>,
        extra: Option<Instant>,
    ) -> InboundReady {
        let _ = self.lifetime.as_mut().poll(cx);
        self.arm(cx, extra);
        self.guard.observed()
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
        let _ = sleep.as_mut().poll(cx);
        self.sleep = Some(sleep);
        self.armed = Some(wanted);
    }
}

/// How one admitted request's inbound work ended when it did not complete.
///
/// The two arms are the precedence table's two dispositions, not a success and
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
}

/// The refusal one mapped terminal answers with.
///
/// `None` is a silent row: the precedence table gives it no mapper, so the
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
