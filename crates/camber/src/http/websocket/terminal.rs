use std::sync::OnceLock;

/// Why a direct WebSocket ended.
///
/// One connection settles on exactly one of these, and every half reads that
/// same value. The cause is what an application acts on, so it names the event
/// rather than the layer that noticed it: a peer that sent a close frame and a
/// peer whose transport failed are different answers, and a server that was
/// asked to stop gracefully is a different answer than one that was cancelled.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum WsCloseCause {
    /// The peer sent a close frame.
    PeerClosed,
    /// The transport ended, failed, or carried a frame that could not be
    /// parsed.
    PeerDisconnected,
    /// The server was asked to stop gracefully.
    ServerShutdown,
    /// The server was cancelled, or its bridge was taken away.
    ServerCancelled,
    /// The connection's only receive owner was dropped.
    ReceiverDropped,
    /// The connection's last send handle was dropped.
    SendersDropped,
}

impl std::fmt::Display for WsCloseCause {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl WsCloseCause {
    /// Whether this cause drops the messages already queued for the receive
    /// owner.
    ///
    /// Two causes do. A cancelled server is not keeping any promise about what
    /// it had buffered, and a connection whose receive owner has gone has
    /// nobody left to deliver to. Every other cause is an orderly end, and an
    /// orderly end still owes the application the messages that arrived before
    /// it.
    ///
    /// Every arm is named. A wildcard here would hand a cause added later the
    /// answer that keeps the queue, with no compiler error to say a decision was
    /// made for it.
    pub(super) fn discards_queued_messages(self) -> bool {
        match self {
            Self::ServerCancelled | Self::ReceiverDropped => true,
            Self::PeerClosed
            | Self::PeerDisconnected
            | Self::ServerShutdown
            | Self::SendersDropped => false,
        }
    }

    /// This cause's stable wire-free name.
    ///
    /// Stated once here rather than in the error's format string, so the text
    /// an application logs and the text `RuntimeError` renders cannot drift.
    fn as_str(self) -> &'static str {
        match self {
            Self::PeerClosed => "peer closed",
            Self::PeerDisconnected => "peer disconnected",
            Self::ServerShutdown => "server shutdown",
            Self::ServerCancelled => "server cancelled",
            Self::ReceiverDropped => "receiver dropped",
            Self::SendersDropped => "senders dropped",
        }
    }
}

/// The one terminal answer a direct WebSocket's halves share.
///
/// Set once and never rewritten: a sender clone, the receive owner, and the
/// bridge coordinator all read it, and a cause that could change would let two
/// halves of one connection disagree about why it ended. `Arc` rather than a
/// copy per half for the same reason — there is one answer, so there is one
/// place it lives.
#[derive(Default)]
pub(crate) struct TerminalState {
    cause: OnceLock<WsCloseCause>,
}

impl TerminalState {
    /// Fix `cause` as this connection's answer, and report the answer that is
    /// now fixed.
    ///
    /// The committed cause is returned rather than the offered one, so a caller
    /// that lost a race to an earlier commit proceeds on the authoritative
    /// value instead of its own.
    pub(crate) fn commit(&self, cause: WsCloseCause) -> WsCloseCause {
        match self.cause.set(cause) {
            Ok(()) => cause,
            Err(_) => self.committed(),
        }
    }

    /// The committed cause, or `None` while the connection is live.
    pub(super) fn cause(&self) -> Option<WsCloseCause> {
        self.cause.get().copied()
    }

    /// The cause a half must report for an operation that found its queue
    /// closed.
    ///
    /// A queue is closed only after its bridge has committed, so the committed
    /// value is the ordinary answer — including on cancellation, which an
    /// aborting server publishes to its bridges and lets them settle. The
    /// fallback covers the one path left that skips the commit: a bridge that
    /// has not settled by the server's shutdown deadline is taken away where it
    /// stands and drops both queue ends without running its exit, and a bridge
    /// taken away is what [`WsCloseCause::ServerCancelled`] means.
    pub(super) fn committed(&self) -> WsCloseCause {
        self.cause().unwrap_or(WsCloseCause::ServerCancelled)
    }
}
