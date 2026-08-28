//! The one immutable aggregate a runtime's startup or teardown failures freeze
//! into, and the log a coordinator records them through.

use super::{LifecycleFailure, LifecycleFailureKind, LifecycleParticipant, LifecyclePhase};
use crate::RuntimeError;
use std::sync::Arc;

/// Every direct runtime-owned failure recorded during one startup or one
/// teardown.
///
/// Never empty: a clean run mints no aggregate at all, so a caller holding one
/// always has at least one failure to read. Every entry is a failure the
/// runtime reports; none of them is elected the one to act on. A caller acts on
/// the whole collection, through [`iter`](Self::iter) or the rendering below.
///
/// Entries are frozen in a stable rendering order — root scope, background
/// children, resources, then the exporter — whatever order teardown happened to
/// record them in, and keep their recording sequence inside one owner class.
/// That order is reproducible output and nothing else: it is not causal
/// precedence, and the first entry is not more responsible than the last.
///
/// The value is shared, not copied: the entries sit behind one `Arc`, so
/// cloning the account is a refcount bump and a caller may retain the whole
/// thing past the runtime that produced it.
#[derive(Clone, Debug)]
pub struct LifecycleFailures {
    /// Every recorded failure in the stable rendering order.
    entries: Arc<[LifecycleFailure]>,
}

impl LifecycleFailures {
    /// Every recorded failure, in the stable rendering order.
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

/// Every recorded failure on one operator line, in the stable rendering order.
///
/// All of them, not a chosen one and a count: an account that rendered a single
/// entry would put the runtime back in the business of deciding which owner an
/// operator should read, which is exactly what having no primary means it does
/// not do. The count leads so a reader knows how many entries follow before
/// reading them.
impl std::fmt::Display for LifecycleFailures {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{} recorded]", self.entries.len())?;
        for failure in self.entries.iter() {
            write!(f, " {failure};")?;
        }
        Ok(())
    }
}

/// Where one startup or teardown collects the failures it observed.
///
/// Owned by the coordinator running that lifecycle: it records as each
/// participant is decided, in whatever order the waits finish, and freezes once
/// at the end. Rendering order is applied at the freeze, so no caller has to
/// record in that order to be reported in it.
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
    /// The only way an aggregate is built, so the rendering order is decided
    /// once here rather than at each coordinator that records into it. Nothing
    /// is elected: every recorded failure crosses into the aggregate, and the
    /// order only makes two identical runs render identically.
    #[must_use]
    pub fn into_error(self) -> Option<RuntimeError> {
        let mut recorded = self.recorded;
        if recorded.is_empty() {
            return None;
        }
        // Stable, so the admission sequence a runtime already owns decides the
        // order inside one owner class and this only decides the classes.
        recorded.sort_by_key(|failure| failure.participant().report_order());
        Some(RuntimeError::Lifecycle(LifecycleFailures {
            entries: recorded.into(),
        }))
    }
}
