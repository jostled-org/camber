//! Who reads the one shared shutdown clock, and settles under it.
//!
//! A wider vocabulary than the one a runtime aggregate reports failures in, and
//! deliberately a separate type. The runtime accounts for the owners no caller
//! holds a handle for; a server accounts for its own connections and upgrades
//! inside its flat tree. Both stop against one clock, so both name themselves
//! here — but only the first arm can ever become an aggregate entry, and that
//! is a fact the compiler enforces rather than one a reviewer has to keep.

use super::LifecycleParticipant;
use std::sync::Arc;

/// One owner inside a server's flat tree.
///
/// Closed and exhaustively matchable. These owners settle inside the server
/// that owns them: a connection is accounted for by its server, and an upgrade
/// by the connection that transferred it. None of them is a runtime aggregate
/// participant, which is why the names live here instead of beside the runtime's
/// own.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ServerTreeOwner {
    /// One server's accept and supervision owner.
    Server,
    /// One accepted connection.
    Connection,
    /// One protocol upgrade past its response head.
    #[cfg(feature = "ws")]
    Upgrade,
}

/// The bounded name each server-tree owner is reported under.
impl std::fmt::Display for ServerTreeOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Server => f.write_str("server"),
            Self::Connection => f.write_str("connection"),
            #[cfg(feature = "ws")]
            Self::Upgrade => f.write_str("upgrade"),
        }
    }
}

/// Who read the one shared shutdown expiry, or settled under it.
///
/// Two arms, because the two owner trees are accounted for by different owners.
/// A [`Runtime`](Self::Runtime) owner is one the runtime aggregate can name in
/// the account a caller reads back; a [`ServerTree`](Self::ServerTree) owner
/// never reaches that account at all.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ShutdownOwner {
    /// One owner the runtime aggregate reports directly.
    Runtime(LifecycleParticipant),
    /// One owner inside a single server's flat tree.
    ServerTree(ServerTreeOwner),
}

impl ShutdownOwner {
    /// One server's accept and supervision owner.
    pub(crate) const SERVER: Self = Self::ServerTree(ServerTreeOwner::Server);
    /// One accepted connection.
    pub(crate) const CONNECTION: Self = Self::ServerTree(ServerTreeOwner::Connection);
    /// One protocol upgrade past its response head.
    #[cfg(feature = "ws")]
    pub(crate) const UPGRADE: Self = Self::ServerTree(ServerTreeOwner::Upgrade);
    /// The runtime's root task scope.
    pub(crate) const ROOT_SCOPE: Self = Self::Runtime(LifecycleParticipant::RootScope);
    /// One scope-admitted background child.
    pub(crate) const BACKGROUND_TASK: Self = Self::Runtime(LifecycleParticipant::BackgroundTask);
    /// The metrics or trace exporter.
    #[cfg(feature = "otel")]
    pub(crate) const EXPORTER: Self = Self::Runtime(LifecycleParticipant::Exporter);

    /// The shared-clock owner one registered resource reads and settles under.
    ///
    /// The name is shared rather than copied: the coordinator that ran the
    /// callback and the observation that renders it read one string.
    pub(crate) fn resource(name: &Arc<str>) -> Self {
        Self::Runtime(LifecycleParticipant::Resource(Arc::clone(name)))
    }
}

/// The bounded name each shared-clock owner is reported under.
///
/// Delegated to whichever vocabulary owns the name, so a participant renamed in
/// one place cannot be rendered under two spellings here.
impl std::fmt::Display for ShutdownOwner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Runtime(participant) => write!(f, "{participant}"),
            Self::ServerTree(owner) => write!(f, "{owner}"),
        }
    }
}
