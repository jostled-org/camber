//! Which stage of a runtime's life a lifecycle failure happened in.

use super::ResourcePhase;

/// The stage a lifecycle failure was recorded in.
///
/// Closed and exhaustively matchable. Startup, the cooperative drain, the
/// forced join after the aggregate deadline, and the final teardown are four
/// different answers about how much time a participant was given; a resource
/// callback names its own [`ResourcePhase`] instead, because its deadline came
/// from the resource budget rather than from the aggregate stage it ran in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum LifecyclePhase {
    /// Before the service admitted any traffic.
    Startup,
    /// Cooperative shutdown, before the aggregate deadline expired.
    GracefulDrain,
    /// The bounded abort-and-join after the aggregate deadline expired.
    ForcedJoin,
    /// One registered resource's own callback phase.
    Resource(ResourcePhase),
    /// The last teardown work, after every owner has been decided.
    Finalize,
}

impl LifecyclePhase {
    /// The bounded name this stage is reported under.
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::Startup => "startup",
            Self::GracefulDrain => "graceful-drain",
            Self::ForcedJoin => "forced-join",
            Self::Resource(phase) => phase.label(),
            Self::Finalize => "finalize",
        }
    }
}

impl std::fmt::Display for LifecyclePhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}
