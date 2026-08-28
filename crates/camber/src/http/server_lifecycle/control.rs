//! The stop vocabulary one owned server speaks, and the waits its readers make.
//!
//! Everything here is about a transition rather than about work: what a public
//! command publishes, what the supervisor and its connections wait on to hear
//! it, and the deadline a shutdown ends at. Nothing here owns a task, a socket,
//! or a permit.

use std::sync::Arc;
use std::time::Duration;

use super::super::mock::LifecycleScript;
use super::super::server_stop::{ServerStopState, StopEvent};
use crate::lifecycle::AggregateShutdown;
use crate::runtime_state::ShutdownSignal;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::http) enum ServerControl {
    Running,
    Graceful,
    Abort,
}

impl ServerControl {
    pub(super) fn send_abort(sender: &tokio::sync::watch::Sender<Self>) {
        sender.send_if_modified(|control| match control {
            Self::Running | Self::Graceful => {
                *control = Self::Abort;
                true
            }
            Self::Abort => false,
        });
    }

    pub(super) fn send_graceful(sender: &tokio::sync::watch::Sender<Self>) {
        sender.send_if_modified(|control| match control {
            Self::Running => {
                *control = Self::Graceful;
                true
            }
            Self::Graceful | Self::Abort => false,
        });
    }
}

/// The authority one public server handle holds over the server it owns.
///
/// Both halves travel together because a stop is one act, in one order: the
/// command commits into the causal stop state, and only then is the transition
/// published. Nothing downstream can decide the outcome afterwards, so a caller
/// whose command has returned already knows how this server ends, and the watch
/// send is notification rather than authority.
pub(in crate::http) struct StopAuthority {
    control: tokio::sync::watch::Sender<ServerControl>,
    stop: Arc<ServerStopState>,
}

impl StopAuthority {
    pub(super) fn new(
        control: tokio::sync::watch::Sender<ServerControl>,
        stop: Arc<ServerStopState>,
    ) -> Self {
        Self { control, stop }
    }

    /// Ask for a graceful stop.
    ///
    /// The commit closes admission and mints the one aggregate deadline, so no
    /// participant woken by the notification can observe a graceful shutdown
    /// that has no deadline yet.
    pub(in crate::http) fn request_graceful(&self) {
        self.stop
            .apply(StopEvent::Graceful(tokio::time::Instant::now()));
        ServerControl::send_graceful(&self.control);
    }

    /// Ask for forced cancellation, which mints nothing at all.
    pub(in crate::http) fn request_cancel(&self) {
        self.stop.apply(StopEvent::Cancel);
        ServerControl::send_abort(&self.control);
    }

    /// Publish the abort an abandoned handle owes its server.
    ///
    /// The same forced phase a cancellation commits, and deliberately not the
    /// same origin: the server reads an owner that went away rather than a
    /// caller that asked, and only a caller that asked gives up the aggregate's
    /// remaining time.
    pub(in crate::http) fn abandon(&self) {
        self.stop.apply(StopEvent::Abandon);
        ServerControl::send_abort(&self.control);
    }
}

/// When a graceful shutdown ends for the requests one connection admitted.
///
/// Two bounds, narrowest wins. The shared one is the aggregate every
/// participant reads, so a request observing a transition late gets the time
/// that is left rather than a fresh copy of the grace. The local one is the
/// server's own configured timeout, which may be narrower than the runtime's
/// and stays this connection's bound when it is.
#[derive(Clone)]
pub(in crate::http) struct ConnectionShutdownDeadline {
    shared: Option<Arc<AggregateShutdown>>,
    local: Duration,
}

impl ConnectionShutdownDeadline {
    pub(super) fn new(shared: Option<Arc<AggregateShutdown>>, local: Duration) -> Self {
        Self { shared, local }
    }

    /// The one aggregate this connection's bounds are narrowed against.
    ///
    /// `None` is a connection with no supervisor over it, which narrows nothing
    /// and settles nowhere.
    #[cfg(feature = "ws")]
    pub(in crate::http) fn aggregate(&self) -> Option<Arc<AggregateShutdown>> {
        self.shared.clone()
    }

    /// The instant a graceful shutdown observed in this turn ends at.
    ///
    /// The narrowing goes through [`AggregateShutdown::bounded`], which is the
    /// one definition of that arithmetic, so this connection answers the same
    /// rule every other participant does — forced-join grace included. A private
    /// copy here was the only bound that could cut an owner to nothing after the
    /// aggregate was spent.
    pub(in crate::http) fn deadline(&self) -> tokio::time::Instant {
        let bound = match self.shared.as_ref() {
            Some(shared) => {
                shared.bounded(&crate::lifecycle::ShutdownOwner::CONNECTION, self.local)
            }
            None => self.local,
        };
        tokio::time::Instant::now() + bound
    }
}

pub(super) async fn wait_deadline(deadline: Option<tokio::time::Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Wait for the next control value this server publishes.
///
/// A dropped sender is the one case that has to be silent: no authority is
/// left to answer, and answering with the last value seen — always `Running`
/// for a server that never left it — would tell a caller the server is live
/// when nothing can say so any more. Every reader of this channel goes through
/// here, so that rule is written once.
pub(super) async fn wait_control(
    receiver: &mut tokio::sync::watch::Receiver<ServerControl>,
) -> ServerControl {
    match receiver.changed().await {
        Ok(()) => *receiver.borrow_and_update(),
        Err(_) => std::future::pending().await,
    }
}

/// Wait until this server leaves `Running`, and answer with what it left for.
///
/// The current value is read before any `changed()`: a connection subscribing
/// after the transition has no edge left to observe, and waiting for one would
/// serve it as though the server were still accepting work.
pub(in crate::http) async fn wait_shutdown_control(
    receiver: &mut tokio::sync::watch::Receiver<ServerControl>,
) -> ServerControl {
    let mut control = *receiver.borrow_and_update();
    while control == ServerControl::Running {
        control = wait_control(receiver).await;
    }
    control
}

pub(super) async fn wait_runtime(
    shutdown: Option<&ShutdownSignal>,
    script: Option<&LifecycleScript>,
) {
    match shutdown {
        None => std::future::pending().await,
        Some(shutdown) => {
            shutdown
                .wait_observed(|| {
                    LifecycleScript::pause_at_stop(
                        script,
                        super::super::mock::ServerStopEdge::BeforeRuntimeWait,
                    )
                })
                .await;
        }
    }
}

pub(super) async fn wait_for_script_wake(script: Option<&LifecycleScript>) {
    match script {
        Some(script) => script.wait_for_supervisor_wake().await,
        None => std::future::pending().await,
    }
}
