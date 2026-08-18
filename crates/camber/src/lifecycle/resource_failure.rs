//! What one registered resource's lifecycle callback did instead of succeeding.

use super::ResourcePhase;
use crate::RuntimeError;
use std::sync::Arc;

/// How one resource lifecycle callback failed.
///
/// Closed and exhaustively matchable. The five outcomes are distinct answers an
/// operator acts on differently: a resource that reported itself unwell, one
/// whose callback unwound, one that ran past its deadline, one whose worker
/// never delivered, and one that could not be entered at all because an earlier
/// abandoned callback still holds it.
#[derive(Clone, Debug)]
pub enum ResourceFailureKind {
    /// The callback returned an error of its own.
    Returned(Arc<RuntimeError>),
    /// The callback unwound, carrying the panic payload's rendered text.
    Panicked(Arc<str>),
    /// The callback ran past the deadline its phase budget allowed.
    ///
    /// Camber stopped waiting. It did not stop the callback: the worker keeps
    /// the resource until the callback returns on its own.
    DeadlineExceeded,
    /// The worker carrying the callback never delivered a result at all.
    LostWorker,
    /// The callback never started, because an earlier abandoned callback for
    /// this same resource still holds it.
    BlockedByActiveCallback,
}

impl ResourceFailureKind {
    /// The bounded name this outcome is reported and published under.
    ///
    /// A closed vocabulary of static text: the health endpoint's failure value
    /// and an operator's failure line both read it, so it can never become a
    /// value derived from a resource or a cause.
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::Returned(_) => "returned",
            Self::Panicked(_) => "panicked",
            Self::DeadlineExceeded => "deadline",
            Self::LostWorker => "lost-worker",
            Self::BlockedByActiveCallback => "blocked",
        }
    }
}

impl std::fmt::Display for ResourceFailureKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Returned(cause) => write!(f, "returned: {cause}"),
            Self::Panicked(payload) => write!(f, "panicked: {payload}"),
            Self::DeadlineExceeded | Self::LostWorker | Self::BlockedByActiveCallback => {
                f.write_str(self.label())
            }
        }
    }
}

/// One named resource's failure in one lifecycle phase.
///
/// The name is shared rather than copied because the coordinator that records
/// the failure, the aggregate that retains it, and the operator event that
/// renders it all read the same registered name.
#[derive(Clone, Debug)]
pub struct ResourceFailure {
    name: Arc<str>,
    phase: ResourcePhase,
    kind: ResourceFailureKind,
}

impl ResourceFailure {
    /// Record what one resource callback did.
    ///
    /// Not part of the published surface: a caller reads failures, and only the
    /// coordinator that ran the callback may state one.
    #[doc(hidden)]
    #[must_use]
    pub const fn new(name: Arc<str>, phase: ResourcePhase, kind: ResourceFailureKind) -> Self {
        Self { name, phase, kind }
    }

    /// The registered name of the resource whose callback failed.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The lifecycle phase the failed callback belonged to.
    #[must_use]
    pub const fn phase(&self) -> ResourcePhase {
        self.phase
    }

    /// How the callback failed.
    #[must_use]
    pub const fn kind(&self) -> &ResourceFailureKind {
        &self.kind
    }

    /// The typed error the callback itself returned, if it returned one.
    ///
    /// The other four outcomes carry their whole account in the kind, so they
    /// answer `None` rather than manufacturing a nested error that never
    /// existed.
    #[must_use]
    pub fn cause(&self) -> Option<&RuntimeError> {
        match &self.kind {
            ResourceFailureKind::Returned(cause) => Some(cause),
            ResourceFailureKind::Panicked(_)
            | ResourceFailureKind::DeadlineExceeded
            | ResourceFailureKind::LostWorker
            | ResourceFailureKind::BlockedByActiveCallback => None,
        }
    }
}

impl std::fmt::Display for ResourceFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "resource {} {}: {}", self.name, self.phase, self.kind)
    }
}
