//! Who inside a runtime a lifecycle failure belongs to.

use std::sync::Arc;

/// The owner a lifecycle failure is recorded against.
///
/// Closed and exhaustively matchable. Every participant a runtime waits for
/// during startup or teardown appears here exactly once, so an operator reading
/// an aggregate learns which owner failed rather than that "shutdown failed".
///
/// Only a resource carries an identity, and it carries its registered name
/// shared rather than copied: the coordinator that ran the callback, the
/// aggregate that retains it, and the operator event that renders it all read
/// one string.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LifecycleParticipant {
    /// The runtime's root task scope.
    RootScope,
    /// One server's accept and supervision owner.
    Server,
    /// One accepted connection.
    Connection,
    /// One protocol upgrade past its response head.
    Upgrade,
    /// One scope-admitted background child.
    BackgroundTask,
    /// One registered resource, under its registered name.
    Resource(Arc<str>),
    /// The metrics or trace exporter.
    Exporter,
    /// The Tokio executor underneath the runtime.
    Executor,
}

impl LifecycleParticipant {
    /// Where this owner sits in the deterministic order an aggregate lists.
    ///
    /// Root scope, then servers, then each server's connections and upgrades,
    /// then background children, then resources, then the exporter, then the
    /// executor. Recording order decides the sequence inside one rank, so the
    /// admission sequence a runtime already owns is the order a reader sees.
    pub(crate) const fn owner_rank(&self) -> u8 {
        match self {
            Self::RootScope => 0,
            Self::Server => 1,
            Self::Connection => 2,
            Self::Upgrade => 3,
            Self::BackgroundTask => 4,
            Self::Resource(_) => 5,
            Self::Exporter => 6,
            Self::Executor => 7,
        }
    }

    /// The bounded name this owner is reported under.
    pub(crate) const fn label(&self) -> &'static str {
        match self {
            Self::RootScope => "root-scope",
            Self::Server => "server",
            Self::Connection => "connection",
            Self::Upgrade => "upgrade",
            Self::BackgroundTask => "background-task",
            Self::Resource(_) => "resource",
            Self::Exporter => "exporter",
            Self::Executor => "executor",
        }
    }
}

impl std::fmt::Display for LifecycleParticipant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Resource(name) => write!(f, "resource {name}"),
            Self::RootScope
            | Self::Server
            | Self::Connection
            | Self::Upgrade
            | Self::BackgroundTask
            | Self::Exporter
            | Self::Executor => f.write_str(self.label()),
        }
    }
}
