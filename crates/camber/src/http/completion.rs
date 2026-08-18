//! The one account an admitted HTTP operation owes at its true terminal.
//!
//! An answer leaving dispatch is not a finished request. A buffered body still
//! has to reach the wire, a streaming body still has to end, a proxied body
//! still has to be forwarded, and an upgrade still has to be committed. Every
//! one of those can be cut short by a peer that leaves, a bound that is
//! crossed, or a server that shuts down, and the request the operator reads
//! about is the one that actually happened.
//!
//! So the answer stages its account here rather than recording it. The staged
//! value travels with the response into the body that owns its terminal, and
//! exactly one record is written there — under the status the peer was given,
//! the whole head-to-terminal duration, and the highest-precedence typed
//! boundary any owner fixed.

use super::boundary::{ByteBoundary, CrossedBound, DeadlineBoundary};
use super::disconnect::DisconnectCause;
use super::handle::ConnCtx;
use super::mock::LifecycleScript;
use super::operation::InboundTerminal;
use super::rejection::RejectionScope;
use super::server_lifecycle::ConnectionLifecycle;
use super::transfer::TransferDirection;
use std::sync::{Arc, OnceLock};

/// Which observability channels this process publishes a completion to.
///
/// Read once per request off the connection context rather than at the
/// terminal: the body that records is polled by Hyper long after the dispatch
/// that answered, and the context it answered under is not borrowed there.
#[derive(Clone, Copy)]
pub(super) struct Telemetry {
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

    pub(super) const fn events(self) -> bool {
        self.events
    }

    pub(super) const fn metrics(self) -> bool {
        self.metrics
    }
}

/// The closed terminal class one admitted HTTP operation ends on.
///
/// It extends the inbound precedence [`InboundTerminal`] declares rather than
/// restating it: the eleven inbound rows keep their order, and the three
/// response-lifetime rows an operation can end on without any owner fixing a
/// terminal are spliced in beneath them. A metric label reads this, so the
/// vocabulary is closed and every name is static text.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum CompletionTerminal {
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
    /// The server began shutting down before this response was produced.
    ServerShutdown,
    /// The peer's response lifetime ended before the answer was produced.
    Disconnect,
    /// This request ended early while its connection stayed live.
    StreamReset,
    /// A payload source this request read failed.
    SourceFailure,
    /// The response was produced to the peer in full.
    Completed,
}

impl CompletionTerminal {
    /// The declared precedence, highest first.
    ///
    /// Named exhaustively rather than derived from declaration order, for the
    /// reason [`InboundTerminal`]'s own table is: the order is the contract, and
    /// a variant moved for readability must not silently reorder what a live
    /// service reports.
    const ORDER: [Self; 13] = [
        Self::ShutdownDeadline,
        Self::ForcedCancellation,
        Self::RouteBodyLimit,
        Self::TransferBytes,
        Self::BodyIdle,
        Self::TransferIdle,
        Self::TransferTotal,
        Self::RequestTotal,
        Self::ServerShutdown,
        Self::Disconnect,
        Self::StreamReset,
        Self::SourceFailure,
        Self::Completed,
    ];

    /// Every name a terminal label may carry.
    ///
    /// Read off the declared order rather than transcribed beside it, so the
    /// list an operator's vocabulary is published from and the list a live
    /// selection walks are one list.
    pub(super) fn vocabulary() -> Box<[&'static str]> {
        Self::ORDER.map(Self::label).into_iter().collect()
    }

    /// Where this terminal sits in the declared order.
    fn precedence(self) -> usize {
        Self::ORDER
            .iter()
            .position(|declared| *declared == self)
            .unwrap_or(Self::ORDER.len())
    }

    /// The bounded name this terminal is reported and counted under.
    pub(super) const fn label(self) -> &'static str {
        match self {
            Self::ShutdownDeadline => "shutdown_deadline",
            Self::ForcedCancellation => "forced_cancellation",
            Self::RouteBodyLimit => "route_body_limit",
            Self::TransferBytes => "transfer_bytes",
            Self::BodyIdle => "body_idle",
            Self::TransferIdle => "transfer_idle",
            Self::TransferTotal => "transfer_total",
            Self::RequestTotal => "request_total",
            Self::ServerShutdown => "server_shutdown",
            Self::Disconnect => "disconnect",
            Self::StreamReset => "stream_reset",
            Self::SourceFailure => "source_failure",
            Self::Completed => "completed",
        }
    }

    /// The inbound terminal one owner fixed, as the class it completes under.
    ///
    /// `ResponseHead` is an inbound source reaching its normal end, which is
    /// not a terminal an operation completes under: the response it permitted
    /// decides that, so it maps to the produced answer.
    const fn of_inbound(terminal: InboundTerminal) -> Self {
        match terminal {
            InboundTerminal::ShutdownDeadline => Self::ShutdownDeadline,
            InboundTerminal::ForcedCancellation => Self::ForcedCancellation,
            InboundTerminal::RouteBodyLimit => Self::RouteBodyLimit,
            InboundTerminal::TransferBytes => Self::TransferBytes,
            InboundTerminal::BodyIdle => Self::BodyIdle,
            InboundTerminal::TransferIdle => Self::TransferIdle,
            InboundTerminal::TransferTotal => Self::TransferTotal,
            InboundTerminal::RequestTotal => Self::RequestTotal,
            InboundTerminal::Disconnect => Self::Disconnect,
            InboundTerminal::SourceFailure => Self::SourceFailure,
            InboundTerminal::ResponseHead => Self::Completed,
        }
    }

    /// The response lifetime this request's guard settled on, as its class.
    const fn of_cause(cause: DisconnectCause) -> Self {
        match cause {
            DisconnectCause::Completed => Self::Completed,
            DisconnectCause::ServerShutdown => Self::ServerShutdown,
            DisconnectCause::PeerDisconnect => Self::Disconnect,
            DisconnectCause::StreamReset => Self::StreamReset,
        }
    }

    /// The configured bound this terminal crossed, measured in `direction`.
    ///
    /// The direction is the reporting owner's, and only the byte row reads it:
    /// an upload maximum is never reported as the download's. Every other row
    /// names one bound whichever direction observed it, or names none at all.
    const fn crossed(self, direction: TransferDirection) -> CrossedBound {
        match self {
            Self::ShutdownDeadline | Self::ForcedCancellation => {
                CrossedBound::Deadline(DeadlineBoundary::AggregateShutdown)
            }
            Self::RouteBodyLimit => CrossedBound::Bytes(ByteBoundary::RequestBody),
            Self::TransferBytes => CrossedBound::Bytes(direction.bytes()),
            Self::BodyIdle => CrossedBound::Deadline(DeadlineBoundary::RequestBodyIdle),
            Self::TransferIdle => CrossedBound::Deadline(DeadlineBoundary::TransferIdle),
            Self::TransferTotal => CrossedBound::Deadline(DeadlineBoundary::TransferTotal),
            Self::RequestTotal => CrossedBound::Deadline(DeadlineBoundary::RequestTotal),
            Self::ServerShutdown
            | Self::Disconnect
            | Self::StreamReset
            | Self::SourceFailure
            | Self::Completed => CrossedBound::None,
        }
    }

    /// The class an operation that crossed `bound` ended on.
    ///
    /// The inverse of [`Self::crossed`] over the bounds an admitted operation
    /// can end on, and every other bound in the vocabulary belongs to a source
    /// this operation was reading rather than to the operation itself: an
    /// upstream, an outbound client, a file, a rendered profile, or a resource
    /// callback. All of those end the work the same way, so they share the one
    /// row that says so, and none of them is silently read as a request bound.
    const fn of_bound(bound: CrossedBound) -> Self {
        match bound {
            CrossedBound::None => Self::Completed,
            CrossedBound::Deadline(DeadlineBoundary::AggregateShutdown) => Self::ShutdownDeadline,
            CrossedBound::Deadline(DeadlineBoundary::RequestBodyIdle) => Self::BodyIdle,
            CrossedBound::Deadline(DeadlineBoundary::RequestTotal) => Self::RequestTotal,
            CrossedBound::Deadline(DeadlineBoundary::TransferIdle) => Self::TransferIdle,
            CrossedBound::Deadline(DeadlineBoundary::TransferTotal) => Self::TransferTotal,
            CrossedBound::Bytes(ByteBoundary::RequestBody) => Self::RouteBodyLimit,
            CrossedBound::Bytes(ByteBoundary::TransferUpload | ByteBoundary::TransferDownload) => {
                Self::TransferBytes
            }
            CrossedBound::Deadline(
                DeadlineBoundary::Header
                | DeadlineBoundary::ProxyConnect
                | DeadlineBoundary::ProxyRequest
                | DeadlineBoundary::ProxyUpstreamIdle
                | DeadlineBoundary::ClientConnect
                | DeadlineBoundary::ClientRequest
                | DeadlineBoundary::ClientResponseIdle
                | DeadlineBoundary::ResourceStartupHealth
                | DeadlineBoundary::ResourcePeriodicHealth
                | DeadlineBoundary::ResourceShutdown,
            )
            | CrossedBound::Bytes(
                ByteBoundary::ClientResponse
                | ByteBoundary::ProxyBufferedResponse
                | ByteBoundary::StaticFile
                | ByteBoundary::ProfilingResponse,
            ) => Self::SourceFailure,
        }
    }
}

/// One terminal an owner fixed, and the bound it crossed to fix it.
///
/// Carried together because they are one observation: a terminal read beside
/// another owner's boundary would name a bound this request never crossed.
#[derive(Clone, Copy)]
struct Crossed {
    terminal: CompletionTerminal,
    boundary: CrossedBound,
}

impl Crossed {
    /// One terminal a pre-commit owner selected.
    ///
    /// The upload is the direction, because it is the only transfer running
    /// before a response head commits: a download's own lifetime starts at the
    /// head this terminal prevented.
    const fn pre_commit(terminal: InboundTerminal) -> Self {
        Self::of(terminal, TransferDirection::Upload)
    }

    /// One terminal the committed response body fixed.
    ///
    /// The download is the direction for the mirror-image reason: the only
    /// transfer left once a head is on the wire is the body producing under it.
    const fn post_commit(terminal: InboundTerminal) -> Self {
        Self::of(terminal, TransferDirection::Download)
    }

    const fn of(terminal: InboundTerminal, direction: TransferDirection) -> Self {
        let terminal = CompletionTerminal::of_inbound(terminal);
        Self {
            terminal,
            boundary: terminal.crossed(direction),
        }
    }

    /// The bound one mapped refusal names, as the terminal it ended on.
    ///
    /// A refusal that crossed no bound is not an observation at all: it says
    /// only that this request was answered, which the response lifetime already
    /// decides more precisely.
    fn refused(bound: CrossedBound) -> Option<Self> {
        match bound {
            CrossedBound::None => None,
            named => Some(Self {
                terminal: CompletionTerminal::of_bound(named),
                boundary: named,
            }),
        }
    }

    /// The response lifetime a request's own guard settled on.
    const fn settled(cause: DisconnectCause) -> Self {
        Self {
            terminal: CompletionTerminal::of_cause(cause),
            boundary: CrossedBound::None,
        }
    }

    /// The higher-precedence of two observations.
    ///
    /// Ties keep the one already held, so a second owner reporting the class a
    /// first already fixed cannot restate it as its own boundary.
    fn strongest(self, other: Self) -> Self {
        match other.terminal.precedence() < self.terminal.precedence() {
            true => other,
            false => self,
        }
    }
}

/// One answered request's account, before its terminal is known.
///
/// It carries the identity every exit already names the request by, the status
/// the peer was actually given, and whatever typed terminal the owner that
/// answered had already fixed. What it does not carry is a terminal it invented:
/// the body or handoff that owns this response decides that, and hands it in
/// when it records.
pub(super) struct Completion {
    scope: RejectionScope,
    telemetry: Telemetry,
    status: u16,
    start: std::time::Instant,
    /// The terminal a pre-commit owner had already fixed, if one had.
    crossed: Option<Crossed>,
    script: Option<Arc<LifecycleScript>>,
}

impl Completion {
    /// Write this operation's one record, under the terminal its owner settled.
    ///
    /// Consumes itself, so "exactly one record per admitted operation" is the
    /// type's shape rather than a rule each caller has to keep.
    pub(super) fn record(self, settled: DisconnectCause, produced: Option<InboundTerminal>) {
        let Self {
            scope,
            telemetry,
            status,
            start,
            crossed,
            script,
        } = self;
        let selected = [crossed, produced.map(Crossed::post_commit)]
            .into_iter()
            .flatten()
            .fold(Crossed::settled(settled), Crossed::strongest);
        LifecycleScript::observe_completion_recorded(script.as_deref());
        super::record::record_request(
            telemetry,
            &super::record::Completed {
                scope: &scope,
                status,
                terminal: selected.terminal,
                boundary: selected.boundary,
            },
            start.elapsed(),
        );
    }
}

/// The clock one admitted request is measured on, and the account it owes.
///
/// One owner per served request, created before Hyper's head reaches dispatch
/// and read after the answer to it has been built. Every exit stages its account
/// here instead of recording one, so the request an operator reads about is
/// described once, by the owner that watched it end.
pub(super) struct RequestAccount {
    start: std::time::Instant,
    telemetry: Telemetry,
    script: Option<Arc<LifecycleScript>>,
    /// Written once, by the exit that answered this request.
    ///
    /// Set-once rather than replaceable: a second answer for one request is a
    /// second account, and the peer received only one of them.
    staged: OnceLock<Completion>,
}

impl RequestAccount {
    /// Start the clock one served request is measured on.
    pub(super) fn begin(ctx: &ConnCtx, lifecycle: &ConnectionLifecycle) -> Self {
        Self {
            start: std::time::Instant::now(),
            telemetry: Telemetry::of(ctx),
            script: lifecycle.script(),
            staged: OnceLock::new(),
        }
    }

    /// Stage the account for an answer no owner fixed a terminal for.
    pub(super) fn commit(&self, scope: &RejectionScope, status: u16) {
        self.stage(scope, status, None);
    }

    /// Stage the account for an answer a refusal produced.
    ///
    /// The bound the refusal names is the observation, not the refusal itself:
    /// a `404` or a failed authentication crossed nothing, and the response
    /// lifetime is what says how that request ended.
    pub(super) fn commit_refused(&self, scope: &RejectionScope, status: u16, bound: CrossedBound) {
        self.stage(scope, status, Crossed::refused(bound));
    }

    /// Stage the account for an answer whose pre-commit terminal is already
    /// known.
    pub(super) fn commit_terminal(
        &self,
        scope: &RejectionScope,
        status: u16,
        terminal: InboundTerminal,
    ) {
        self.stage(scope, status, Some(Crossed::pre_commit(terminal)));
    }

    /// Stage one answer's account, and count every account this request offered.
    ///
    /// A second offer is dropped rather than replacing the first: the peer
    /// received the first answer, so the first account is the true one. The
    /// offer is still counted, because "one answer, one account" is a claim a
    /// case has to be able to read rather than infer from a silence.
    fn stage(&self, scope: &RejectionScope, status: u16, crossed: Option<Crossed>) {
        LifecycleScript::observe_completion_staged(self.script.as_deref());
        let _ = self.staged.set(Completion {
            scope: scope.clone(),
            telemetry: self.telemetry,
            status,
            start: self.start,
            crossed,
            script: self.script.clone(),
        });
    }

    /// Hand the staged account to the owner that will watch this request end.
    ///
    /// `None` is a request no exit answered — the connection failed under it, or
    /// Hyper refused its head before any Camber owner ran — and it stays absent
    /// rather than becoming a record for a response no peer received.
    pub(super) fn into_completion(self) -> Option<Completion> {
        self.staged.into_inner()
    }
}
