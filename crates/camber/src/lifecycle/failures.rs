//! The one immutable aggregate a runtime's startup or teardown failures freeze
//! into, and the log a coordinator records them through.

use super::{LifecycleFailure, LifecycleFailureKind, LifecycleParticipant, LifecyclePhase};
use crate::RuntimeError;
use std::sync::Arc;

/// Every framework-owned participant that failed during one startup or one
/// teardown.
///
/// Never empty: a clean run mints no aggregate at all, so a caller holding one
/// always has at least the failure [`primary`](Self::primary) names. Entries
/// are frozen in deterministic owner order — root scope, servers, their
/// connections and upgrades, background children, resources, exporter, then
/// executor — whatever order teardown happened to record them in, and keep
/// their recording sequence inside one owner class.
///
/// The value is shared, not copied: `RuntimeError::Lifecycle` carries it behind
/// an `Arc` so a caller may retain the whole account past the runtime that
/// produced it.
#[derive(Clone, Debug)]
pub struct LifecycleFailures {
    /// Every recorded failure in owner order.
    entries: Arc<[LifecycleFailure]>,
    /// The one failure that outranks the rest.
    ///
    /// Held by value rather than as an index into `entries`. Both say the same
    /// thing about a slice this type freezes and never mutates, but only the
    /// value makes [`primary`](Self::primary) total: an index would leave the
    /// one accessor a caller reaches for first as the only place in this crate
    /// that could panic on an invariant no type states. The clone shares the
    /// entry's payloads rather than copying them.
    primary: LifecycleFailure,
}

impl LifecycleFailures {
    /// The failure a caller should act on.
    ///
    /// Chosen by precedence — the aggregate deadline, explicit cancellation, a
    /// panic, a scope that could not drain, a resource callback, then every
    /// remaining subsystem outcome — with owner order breaking ties inside one
    /// class. The rest stay available through [`iter`](Self::iter).
    #[must_use]
    pub const fn primary(&self) -> &LifecycleFailure {
        &self.primary
    }

    /// Every recorded failure, in owner order.
    pub fn iter(&self) -> impl ExactSizeIterator<Item = &LifecycleFailure> {
        self.entries.iter()
    }

    /// How many participants failed.
    ///
    /// There is no `is_empty`: an aggregate exists only because at least one
    /// participant failed, so the question has one answer and asking it would
    /// suggest an empty aggregate is reachable.
    #[expect(
        clippy::len_without_is_empty,
        reason = "the aggregate is never empty; an is_empty that always answers false is a trap"
    )]
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

impl std::fmt::Display for LifecycleFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} [{} recorded]", self.primary, self.entries.len())
    }
}

/// Where one startup or teardown collects the failures it observed.
///
/// Owned by the coordinator running that lifecycle: it records as each
/// participant is decided, in whatever order the waits finish, and freezes once
/// at the end. Ordering and precedence are applied at the freeze, so no caller
/// has to record in owner order to be reported in it.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct LifecycleFailureLog {
    recorded: Vec<LifecycleFailure>,
}

impl LifecycleFailureLog {
    /// Start an empty log.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one participant's failure.
    pub fn record(
        &mut self,
        participant: LifecycleParticipant,
        phase: LifecyclePhase,
        kind: LifecycleFailureKind,
    ) {
        self.recorded
            .push(LifecycleFailure::new(participant, phase, kind));
    }

    /// Freeze the log into the error a lifecycle returns, or `None` when
    /// nothing failed.
    ///
    /// The only way an aggregate is built, so ordering is decided once here
    /// rather than at each coordinator that records into it. Owner order alone
    /// picks the primary: the entry a caller acts on is the outermost owner
    /// that failed, whatever it failed with.
    #[must_use]
    pub fn into_error(self) -> Option<RuntimeError> {
        let mut recorded = self.recorded;
        // Stable, so the admission sequence a runtime already owns decides the
        // order inside one owner class and this only decides the classes.
        recorded.sort_by_key(|failure| failure.participant().owner_rank());
        let primary = recorded.first()?.clone();
        Some(RuntimeError::Lifecycle(Arc::new(LifecycleFailures {
            entries: recorded.into(),
            primary,
        })))
    }
}
