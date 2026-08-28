//! Who inside a runtime a lifecycle failure belongs to.

use std::sync::Arc;

/// The owner a lifecycle failure is recorded against.
///
/// Closed and exhaustively matchable. Every owner the runtime itself waits for
/// and authoritatively settles appears here exactly once, so an operator reading
/// an aggregate learns which owner failed rather than that "shutdown failed".
///
/// The vocabulary is deliberately narrow. A server, one of its connections, and
/// an upgrade past its response head settle inside the flat server tree that
/// owns them, and never reappear as runtime aggregate participants. The Tokio
/// executor is not here either: Camber gets no acknowledgement back from it, so
/// there is no fact about it this vocabulary could honestly state.
///
/// Only a resource carries an identity, and it carries its registered name
/// shared rather than copied: the coordinator that ran the callback, the
/// aggregate that retains it, and the operator event that renders it all read
/// one string.
///
/// `Exporter` is settlement-only vocabulary. The trace provider's shutdown is
/// unbounded and hands back nothing, so teardown settles it `Completed` through
/// `ShutdownOwner::EXPORTER` and has no outcome it could record a failure from.
/// It is here so the settlement inventory can name the owner it visited, and it
/// carries a report order for the same reason every other owner does — not
/// because an aggregate entry can reach it.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LifecycleParticipant {
    /// The runtime's root task scope.
    RootScope,
    /// One scope-admitted background child.
    BackgroundTask,
    /// One registered resource, under its registered name.
    Resource(Arc<str>),
    /// The metrics or trace exporter.
    Exporter,
}

impl LifecycleParticipant {
    /// Where this owner sits in the order an aggregate renders entries in.
    ///
    /// Reproducible output and nothing more: root scope, then background
    /// children, then resources, then the exporter, with recording order
    /// deciding the sequence inside one class. It is not causal precedence, and
    /// no caller may read the first entry as the failure to act on — every
    /// entry is a direct failure the account reports.
    pub(crate) const fn report_order(&self) -> u8 {
        match self {
            Self::RootScope => 0,
            Self::BackgroundTask => 1,
            Self::Resource(_) => 2,
            Self::Exporter => 3,
        }
    }
}

/// The bounded name each owner is reported under.
///
/// One arm per participant, written where the name is rendered. A separate
/// name table would have owed the resource an entry it could never reach: the
/// arm below carries the registered identity, which is the whole reason this
/// impl exists.
impl std::fmt::Display for LifecycleParticipant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resource(name) => write!(f, "resource {name}"),
            Self::RootScope => f.write_str("root-scope"),
            Self::BackgroundTask => f.write_str("background-task"),
            Self::Exporter => f.write_str("exporter"),
        }
    }
}
