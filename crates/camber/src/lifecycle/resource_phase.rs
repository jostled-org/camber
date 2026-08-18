//! The closed vocabulary of lifecycle callbacks a registered resource runs.

/// Which of a registered resource's callbacks a budget, deadline, or failure
/// belongs to.
///
/// Closed and exhaustively matchable: a new phase is a deliberate API change,
/// not a silent addition a caller's `match` quietly ignores. The three names
/// are the same three a [`ResourceBudget`](super::ResourceBudget) carries a
/// duration for, so a phase and its deadline can never disagree about how many
/// phases exist.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ResourcePhase {
    /// The initial readiness probe, run before the service admits traffic.
    StartupHealth,
    /// A later probe, run on the runtime's configured health interval.
    PeriodicHealth,
    /// The teardown callback, run after the root scope drains.
    Shutdown,
}

impl ResourcePhase {
    /// The bounded name this phase is reported under.
    ///
    /// A closed vocabulary of static text, for the reason
    /// [`DeadlineBoundary::label`] is one: an operator's failure line and an
    /// operator's health entry both read it, so it can never become a value
    /// derived from a resource.
    ///
    /// [`DeadlineBoundary::label`]: crate::http::DeadlineBoundary
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::StartupHealth => "startup-health",
            Self::PeriodicHealth => "periodic-health",
            Self::Shutdown => "shutdown",
        }
    }
}

impl std::fmt::Display for ResourcePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}
