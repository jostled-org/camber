//! What one lifecycle participant did instead of finishing, and who did it.

use super::{LifecycleParticipant, LifecyclePhase, ResourceFailure};
use crate::RuntimeError;
use crate::http::DeadlineBoundary;
use std::sync::Arc;

/// How one lifecycle participant failed.
///
/// Closed and exhaustively matchable. Each variant is an outcome an operator
/// acts on differently: a participant that ran past the one aggregate deadline,
/// one that was cancelled outright, one that unwound, a scope that could not
/// drain, a resource callback that reported through its own vocabulary, a join
/// Camber never got back, and a typed error a subsystem returned.
///
/// The payloads are shared rather than copied because the coordinator that
/// records a failure, the aggregate that retains it, and the operator event
/// that renders it all read the same value.
#[derive(Clone, Debug)]
pub enum LifecycleFailureKind {
    /// The participant was still running when its deadline expired, named as
    /// the same closed boundary the policy that configured it is written in.
    DeadlineExceeded(DeadlineBoundary),
    /// The participant was cancelled rather than given a grace period.
    Cancelled,
    /// The participant unwound, carrying the panic payload's rendered text.
    TaskPanicked(Arc<str>),
    /// The root scope's bounded drain expired, carrying how many children the
    /// boundary found outstanding.
    ScopeDrainTimeout {
        /// Children that had not exited cooperatively when the drain expired.
        outstanding: usize,
    },
    /// A registered resource's callback failed in its own vocabulary.
    Resource(ResourceFailure),
    /// A worker Camber owned never delivered a join result, named by what was
    /// being joined.
    JoinLost(Arc<str>),
    /// A subsystem returned a typed error of its own.
    Operation(Arc<RuntimeError>),
}

impl LifecycleFailureKind {
    /// The typed error nested inside this outcome, for the two kinds that
    /// carry one.
    ///
    /// The others carry their whole account directly, so they answer `None`
    /// rather than manufacturing an error that never existed.
    fn cause(&self) -> Option<&RuntimeError> {
        match self {
            Self::Resource(resource) => resource.cause(),
            Self::Operation(cause) => Some(cause),
            Self::DeadlineExceeded(_)
            | Self::Cancelled
            | Self::TaskPanicked(_)
            | Self::ScopeDrainTimeout { .. }
            | Self::JoinLost(_) => None,
        }
    }
}

impl std::fmt::Display for LifecycleFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::DeadlineExceeded(boundary) => write!(f, "deadline exceeded: {boundary}"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::TaskPanicked(payload) => write!(f, "panicked: {payload}"),
            Self::ScopeDrainTimeout { outstanding } => {
                write!(
                    f,
                    "scope drain timed out; children outstanding: {outstanding}"
                )
            }
            Self::Resource(resource) => write!(f, "{resource}"),
            Self::JoinLost(what) => write!(f, "join lost: {what}"),
            Self::Operation(cause) => write!(f, "{cause}"),
        }
    }
}

/// One participant's failure in one lifecycle phase.
///
/// Immutable once recorded. The three facts are read back through accessors
/// rather than fields so the aggregate that freezes a failure and the operator
/// event that renders it cannot disagree about which of them is authoritative.
#[derive(Clone, Debug)]
pub struct LifecycleFailure {
    participant: LifecycleParticipant,
    phase: LifecyclePhase,
    kind: LifecycleFailureKind,
}

impl LifecycleFailure {
    /// Record what one participant did.
    ///
    /// Crate-internal: a caller reads failures, and only the coordinator that
    /// waited on the participant may state one.
    pub(crate) const fn new(
        participant: LifecycleParticipant,
        phase: LifecyclePhase,
        kind: LifecycleFailureKind,
    ) -> Self {
        Self {
            participant,
            phase,
            kind,
        }
    }

    /// The owner this failure belongs to.
    #[must_use]
    pub const fn participant(&self) -> &LifecycleParticipant {
        &self.participant
    }

    /// The lifecycle stage the failure happened in.
    #[must_use]
    pub const fn phase(&self) -> LifecyclePhase {
        self.phase
    }

    /// How the participant failed.
    #[must_use]
    pub const fn kind(&self) -> &LifecycleFailureKind {
        &self.kind
    }

    /// The typed error nested inside this failure, if it carries one.
    #[must_use]
    pub fn cause(&self) -> Option<&RuntimeError> {
        self.kind.cause()
    }
}

impl std::fmt::Display for LifecycleFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.kind {
            // A resource failure already names its own resource and phase in
            // its own vocabulary. Prefixing it with this failure's participant
            // and phase would state both twice on one operator line.
            LifecycleFailureKind::Resource(resource) => write!(f, "{resource}"),
            LifecycleFailureKind::DeadlineExceeded(_)
            | LifecycleFailureKind::Cancelled
            | LifecycleFailureKind::TaskPanicked(_)
            | LifecycleFailureKind::ScopeDrainTimeout { .. }
            | LifecycleFailureKind::JoinLost(_)
            | LifecycleFailureKind::Operation(_) => {
                write!(f, "{} {}: {}", self.participant, self.phase, self.kind)
            }
        }
    }
}
