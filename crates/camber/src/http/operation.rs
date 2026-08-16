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

use super::disconnect::DisconnectSignal;
use super::mock::LifecycleScript;
use super::rejection::Rejected;
use super::request_budget::RequestBudget;
use super::server_lifecycle::ServerControl;
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
/// Later steps extend this set with the transfer terminals their adapters own.
/// They add a variant and an [`InboundTerminal::ORDER`] row; they do not
/// redefine the order already declared.
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
    /// The quiet interval allowed between request body data frames expired.
    BodyIdle,
    /// The lifetime from admitted head to committed response head expired.
    RequestTotal,
    /// The peer's response lifetime ended before an answer was possible.
    Disconnect,
    /// The inbound source failed.
    SourceFailure,
    /// The request payload ended, so the response head may be produced.
    ResponseHead,
}

impl InboundTerminal {
    /// The declared precedence, highest first.
    ///
    /// Named exhaustively rather than derived from declaration order: the order
    /// is the contract, and a variant reordered for readability must not
    /// silently reorder the terminals a live service selects.
    const ORDER: [Self; 8] = [
        Self::ShutdownDeadline,
        Self::ForcedCancellation,
        Self::RouteBodyLimit,
        Self::BodyIdle,
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
    body_idle: bool,
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
            InboundTerminal::BodyIdle => self.body_idle,
            InboundTerminal::RequestTotal => self.request_total,
            InboundTerminal::Disconnect => self.disconnect,
            InboundTerminal::SourceFailure => self.source_failure,
            InboundTerminal::ResponseHead => self.response_head,
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
    /// The effective request policy, already narrowed outer-to-inner by the
    /// classifier that selected this route's authority.
    budget: RequestBudget,
    /// The absolute instant this request's total deadline expires at, computed
    /// once from the admitted head.
    total: Option<Instant>,
    /// The server's shutdown and forced-cancellation authority.
    ///
    /// `None` is a connection with no supervisor to hear from, which is the
    /// detached synchronous callback case. It never observes either terminal.
    control: Option<tokio::sync::watch::Receiver<ServerControl>>,
    shutdown_timeout: Duration,
    disconnect: DisconnectSignal,
}

impl OperationEnvelope {
    /// Mint the one envelope an admitted head carries.
    pub(super) fn admit(
        budget: RequestBudget,
        control: Option<tokio::sync::watch::Receiver<ServerControl>>,
        disconnect: &DisconnectSignal,
        shutdown_timeout: Duration,
        script: Option<&LifecycleScript>,
    ) -> Self {
        let admitted_at = Instant::now();
        let envelope = Self {
            id: OperationId::mint(),
            budget,
            total: budget.total().map(|total| admitted_at + total),
            control,
            shutdown_timeout,
            disconnect: disconnect.clone(),
        };
        LifecycleScript::observe_operation_admitted(script, envelope.id, budget.total());
        envelope
    }

    /// Publish this operation's identity from the owner that reached `stage`.
    pub(super) fn observe(&self, script: Option<&LifecycleScript>, stage: OperationStage) {
        LifecycleScript::observe_operation(script, self.id, stage);
    }

    /// The effective request policy this operation runs under.
    pub(super) const fn budget(&self) -> RequestBudget {
        self.budget
    }

    /// The narrow inbound handle one payload owner takes.
    ///
    /// Built per owner rather than stored, so the idle interval each owner
    /// measures starts when that owner started reading rather than when the
    /// head was admitted.
    pub(super) fn inbound(&self) -> InboundGuard {
        InboundGuard {
            idle: self.budget.body_idle(),
            total: self.total,
            control: self.control.clone(),
            shutdown_timeout: self.shutdown_timeout,
            shutdown_deadline: None,
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
            body_idle: self
                .idle
                .is_some_and(|idle| now.saturating_duration_since(self.quiet_since) >= idle),
            request_total: self.total.is_some_and(|total| now >= total),
            disconnect: self.disconnect.observed().is_some(),
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
        InboundTerminal::RouteBodyLimit | InboundTerminal::SourceFailure => wire,
        InboundTerminal::BodyIdle => budget.body_idle().map(Rejected::body_timeout),
        InboundTerminal::RequestTotal => budget.total().map(Rejected::request_timeout),
        InboundTerminal::ShutdownDeadline
        | InboundTerminal::ForcedCancellation
        | InboundTerminal::Disconnect
        | InboundTerminal::ResponseHead => None,
    }
}
