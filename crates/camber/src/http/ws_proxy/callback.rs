//! The retained direct-callback child, and the one join deadline it answers to.
//!
//! A `Router::ws` callback is application code on a blocking worker. Camber
//! starts it, so Camber keeps its handle: every bridge terminal closes the
//! endpoints a cooperative callback wakes on, and this file is what the upgrade
//! owner then waits with. The deadline is absolute from the instant those
//! endpoints closed. A later server transition may bring it forward and nothing
//! may push it back, so no amount of escalation buys a blocked callback more
//! time than the phase it was already under.
//!
//! Tokio cannot take a blocking thread away, so a callback still in application
//! code at its deadline is named rather than claimed to have returned. That
//! naming is the whole disposition: one WARN event, then the upgrade owner
//! settles and the connection gives its permit back.

use tokio::time::Instant;

use super::super::completion::{ShutdownObservation, optional_label};
use super::super::mock::{LifecycleScript, UpgradeOwnerEdge, WebSocketCallbackObservation};
use super::super::server_lifecycle::ServerControl;
use super::super::server_stop::{CommittedStopReading, ServerStopState, StopPhase};
use super::super::websocket::WsCloseCause;
use super::framing::awaited_abort;
use crate::lifecycle::FORCED_JOIN_GRACE;

/// How one retained callback ended.
#[derive(Clone, Copy, Debug)]
enum CallbackDisposition {
    /// The callback returned, unwound, or was already gone when the join ran.
    Completed,
    /// Application code was still blocked when the join deadline arrived.
    OutstandingAfterForcedGrace,
}

impl CallbackDisposition {
    /// This disposition's closed name, as the event and the observation spell it.
    const fn label(self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::OutstandingAfterForcedGrace => "outstanding-after-forced-grace",
        }
    }
}

/// What the server had committed when one bridge woke its callback.
///
/// The same closed vocabulary a completion record's shutdown dimension carries,
/// read from the same committed stop state: a callback outstanding under a
/// cancellation and an operation finalized under one observed the same server,
/// and two spellings of that would be two things an operator has to reconcile.
/// Absence is a peer, direction-owner, or transport terminal with no server
/// transition behind it.
type CallbackShutdown = Option<ShutdownObservation>;

/// The one join deadline an upgrade owner fixes for its retained callback.
///
/// Fixed at the endpoint close, from the phase committed there, and only ever
/// brought forward afterwards. Holding the entry transition alongside the
/// instant is what lets the disposition say which transition set the deadline
/// rather than which one happened to be committed when the wait ran out.
///
/// Built where the endpoints close rather than where the join waits: a teardown
/// step between the two spends this grace, and a phase read after it would name
/// a transition that arrived while the callback was already awake.
pub(super) struct CallbackDeadline {
    /// The cause this bridge committed, which every half of it reads.
    cause: WsCloseCause,
    /// The connection and upgrade this callback is a child of.
    ///
    /// Every record carries it, so the whole history of one callback is
    /// attributable to the one upgrade owner that started it rather than to
    /// whichever bridge happened to publish nearby.
    owner: super::super::server_lifecycle::UpgradeIdentity,
    /// The transition committed when the endpoints closed.
    entered: CallbackShutdown,
    /// The instant this bridge closed the endpoints a blocked callback wakes on.
    closed_at: Instant,
    /// The instant the join gives up at, as it stands now.
    at: Instant,
    /// Whether a later cancellation commit brought that instant forward.
    shortened_by_cancel: bool,
}

impl CallbackDeadline {
    /// Fix this callback's deadline from the phase its bridge closed under.
    ///
    /// A graceful stop is the one phase that borrows the aggregate: the drain
    /// already owns an expiry, and a callback woken inside it is owed the rest
    /// of that drain and no more, because the fixed grace after it belongs to
    /// the owner that has to report what this join decided. Every other phase is
    /// a terminal nothing is draining towards, so the grace runs from the close
    /// itself. A server with no supervisor over it is the same case as one still
    /// running, because neither has asked this bridge for anything.
    pub(super) fn fixed(
        cause: WsCloseCause,
        owner: super::super::server_lifecycle::UpgradeIdentity,
        closed_at: Instant,
        stop: Option<&ServerStopState>,
        script: Option<&LifecycleScript>,
    ) -> Self {
        let (entered, at) = match stop {
            Some(stop) => stop.read(|reading| entry(closed_at, reading)),
            None => (None, closed_at + FORCED_JOIN_GRACE),
        };
        let fixed = Self {
            cause,
            owner,
            entered,
            closed_at,
            at,
            shortened_by_cancel: false,
        };
        fixed.publish(script, None, None);
        fixed
    }

    /// Publish this deadline as it stands, with a disposition once it has one.
    fn publish(
        &self,
        script: Option<&LifecycleScript>,
        disposition: Option<CallbackDisposition>,
        shutdown: Option<CallbackShutdown>,
    ) {
        LifecycleScript::observe_ws_callback(
            script,
            WebSocketCallbackObservation {
                connection: self.owner.connection,
                upgrade: self.owner.upgrade,
                entered: optional_label(self.entered, ShutdownObservation::label),
                endpoints_closed_at: self.closed_at,
                deadline: self.at,
                disposition: disposition.map(CallbackDisposition::label),
                shutdown: shutdown
                    .map(|shutdown| optional_label(shutdown, ShutdownObservation::label)),
            },
        );
    }

    /// Bring this deadline forward if a caller cancelled inside the drain it was
    /// fixed from.
    ///
    /// Only the aggregate-bounded row can move, and only a commanded
    /// cancellation moves it. An owner that merely went away commits the same
    /// forced phase and gives up none of the aggregate's remaining time, so
    /// reading the commit alone would take a drain away from a callback nobody
    /// asked to end now.
    ///
    /// The grace runs from the moment this wait heard the escalation, not from
    /// the instant it committed. A bridge the runtime does not schedule inside
    /// one grace of the commit would otherwise resume onto a deadline already
    /// past and get no grace at all.
    ///
    /// A deadline already running on the fixed grace is the shortest this
    /// contract offers, so a cancellation arriving after it would restart a wait
    /// rather than end one, and a repeat of the phase that fixed it changes
    /// nothing at all.
    fn narrow(&mut self, stop: Option<&ServerStopState>, script: Option<&LifecycleScript>) {
        let commanded = match (self.entered, stop) {
            (Some(ShutdownObservation::Graceful), Some(stop)) => stop.read(cancel_commanded),
            _ => false,
        };
        match (commanded, Instant::now() + FORCED_JOIN_GRACE) {
            (true, shortened) if shortened < self.at => {
                self.at = shortened;
                self.shortened_by_cancel = true;
                self.publish(script, None, None);
            }
            _ => {}
        }
    }

    /// Which transition this deadline reports at the disposition.
    ///
    /// A local terminal took no transition into the wait, so it reports
    /// whatever the server has committed by the time the wait ends. Every other
    /// entry already names one and reports it.
    ///
    /// The drain row is the one that can end somewhere other than where it
    /// started, and the disposition is what says whether it did: a cancellation
    /// that brought the deadline forward reports that cancellation, and only a
    /// callback still outstanding at the end reports the aggregate expiry it was
    /// fixed from running out. A callback that entered a drain and returned
    /// cooperatively expired nothing, so it reports the drain it entered.
    fn shutdown(
        &self,
        stop: Option<&ServerStopState>,
        disposition: CallbackDisposition,
    ) -> CallbackShutdown {
        match (self.entered, self.shortened_by_cancel, disposition) {
            (None, _, _) => ShutdownObservation::committed_now(stop),
            (Some(ShutdownObservation::Graceful), true, _) => Some(ShutdownObservation::Cancelled),
            (
                Some(ShutdownObservation::Graceful),
                false,
                CallbackDisposition::OutstandingAfterForcedGrace,
            ) => Some(ShutdownObservation::DeadlineExpired),
            (Some(entered), _, _) => Some(entered),
        }
    }
}

/// The entry transition and initial deadline one committed reading selects.
///
/// The drain row borrows the aggregate expiry itself and adds nothing to it.
/// The fixed grace is granted once, by the forced stop, and it is what the
/// server then spends waiting for the connection above this callback to hand
/// the answer up: `ServerSupervisor::forced_deadline` arms that last resort one
/// grace after the expiry it just answered. A callback that took a grace of its
/// own on top of the drain would run out on the very instant its connection is
/// taken away, so the disposition it published could never be read by the owner
/// that had to publish it.
///
/// The floor is the same grace measured from the close instead. An aggregate
/// that ran out before this bridge closed the endpoints has no drain left to
/// lend, and lending it anyway would name a callback outstanding at a deadline
/// already in the past — before anything asked it to return.
fn entry(closed_at: Instant, reading: &CommittedStopReading) -> (CallbackShutdown, Instant) {
    let entered = ShutdownObservation::committed(reading);
    let floor = closed_at + FORCED_JOIN_GRACE;
    let at = match (reading.phase, reading.aggregate_expiry) {
        (StopPhase::Graceful, Some(expiry)) => expiry.max(floor),
        _ => floor,
    };
    (entered, at)
}

/// Join this bridge's retained callback, or name it outstanding, before the
/// upgrade owner settles.
///
/// The one place the direct bridge's callback child is disposed of. Everything
/// the disposition needs is already fixed by the time it runs: the cause this
/// bridge committed, the instant it closed the callback's endpoints, and the
/// phase its server had committed by then.
pub(super) async fn settle_callback(
    handle: tokio::task::JoinHandle<()>,
    deadline: &mut CallbackDeadline,
    stop: Option<&ServerStopState>,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    script: Option<&LifecycleScript>,
) {
    LifecycleScript::pause_at_upgrade(script, UpgradeOwnerEdge::BeforeCallbackJoin).await;
    let disposition = join_within(handle, deadline, stop, control, script).await;
    let shutdown = deadline.shutdown(stop, disposition);
    match disposition {
        CallbackDisposition::Completed => {}
        CallbackDisposition::OutstandingAfterForcedGrace => {
            report_outstanding(deadline.cause, shutdown);
        }
    }
    deadline.publish(script, Some(disposition), Some(shutdown));
}

/// Wait for the callback within the deadline it owns, hearing one escalation.
///
/// The join is polled first every turn, so a callback that returned in the same
/// turn its deadline arrived reads as the cooperative return it was. The
/// escalation arm is watched exactly once: the forced phase commits once, and
/// an arm that answered the same standing value on every turn would spin.
async fn join_within(
    mut handle: tokio::task::JoinHandle<()>,
    deadline: &mut CallbackDeadline,
    stop: Option<&ServerStopState>,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    script: Option<&LifecycleScript>,
) -> CallbackDisposition {
    let mut watching = true;
    loop {
        tokio::select! {
            biased;
            _ = &mut handle => return CallbackDisposition::Completed,
            () = tokio::time::sleep_until(deadline.at) => {
                return CallbackDisposition::OutstandingAfterForcedGrace;
            }
            () = escalation(control, watching) => {
                deadline.narrow(stop, script);
                watching = false;
            }
        }
    }
}

/// Wait for the one later transition that may bring a join deadline forward.
///
/// A caller that has already heard it waits forever instead, which is what
/// leaves the deadline and the join as the only two answers left.
async fn escalation(control: &mut tokio::sync::watch::Receiver<ServerControl>, watching: bool) {
    match watching {
        true => awaited_abort(control).await,
        false => std::future::pending().await,
    }
}

/// Whether a caller asked for termination now, having committed the forced
/// phase.
///
/// The origin is what decides how much of the aggregate a shortened wait may
/// keep, exactly as `ServerSupervisor::forced_deadline` reads it: a caller that
/// asked for now gave the remaining drain up, and an owner that merely went
/// away gave up nothing.
fn cancel_commanded(reading: &CommittedStopReading) -> bool {
    reading.commanded && reading.forced_at.is_some()
}

/// Say that application code is still running, once, before anything settles.
///
/// The fields are closed and carry no identity: which callback it was is the
/// application's to know, and a request path or an owner identifier would make
/// this a per-connection diagnostic rather than the one fact Camber can honestly
/// state — that it stopped waiting and did not stop the callback.
fn report_outstanding(cause: WsCloseCause, shutdown: CallbackShutdown) {
    tracing::warn!(
        name: "camber.websocket.callback.outstanding",
        disposition = CallbackDisposition::OutstandingAfterForcedGrace.label(),
        cause = %cause,
        shutdown = optional_label(shutdown, ShutdownObservation::label),
        "WebSocket callback outstanding after its forced join grace"
    );
}
