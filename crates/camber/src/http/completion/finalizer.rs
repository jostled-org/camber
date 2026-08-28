//! The one owner that reads an operation's facts and writes its record.
//!
//! An answer leaving dispatch is not a finished request. A buffered body still
//! has to reach the wire, a streaming body still has to end, a proxied body
//! still has to be forwarded, and an upgrade still has to be committed. Every
//! one of those can be cut short by a peer that leaves, a bound that is crossed,
//! or a server that shuts down, and the request the operator reads about is the
//! one that actually happened.
//!
//! So this is the only thing that records, it is uniquely owned by the response
//! guard whose drop is that lifetime's true end, and it consumes itself when it
//! runs. "Exactly one record per admitted operation" is therefore the type's
//! shape rather than a rule every exit has to keep.

use super::super::disconnect::DisconnectCause;
use super::super::mock::LifecycleScript;
use super::super::server_stop::ServerStopState;
use super::facts::{CompletionAccount, ShutdownObservation};
use std::sync::Arc;

/// The uniquely owned finalizer one admitted operation carries to its terminal.
pub(in crate::http) struct OperationFinalizer {
    account: Arc<CompletionAccount>,
    /// The causal stop state this operation's server commits its control into.
    ///
    /// `None` is a connection with no supervisor over it, which is read as "no
    /// server transition to observe" rather than as a running server: a detached
    /// connection has no phase to snapshot at all.
    stop: Option<Arc<ServerStopState>>,
}

impl OperationFinalizer {
    /// The one finalizer an admitted operation's response lifetime carries.
    pub(in crate::http) const fn owning(
        account: Arc<CompletionAccount>,
        stop: Option<Arc<ServerStopState>>,
    ) -> Self {
        Self { account, stop }
    }

    /// The account every other owner of this operation writes its facts into.
    pub(in crate::http) const fn account(&self) -> &Arc<CompletionAccount> {
        &self.account
    }

    /// Write this operation's delivery and shutdown facts, and record it once.
    ///
    /// Consumes itself, so a second record for one operation is unrepresentable
    /// rather than guarded against.
    pub(in crate::http) fn finalize(self, settled: DisconnectCause) {
        let Self { account, stop } = self;
        let snapshot =
            account.settled(settled, ShutdownObservation::committed_now(stop.as_deref()));
        LifecycleScript::observe_completion_recorded(account.script().map(Arc::as_ref));
        super::super::record::record_request(
            account.telemetry(),
            &account.identity(),
            &snapshot,
            account.elapsed(),
        );
    }
}
