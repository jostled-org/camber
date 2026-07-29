//! The deadlines every disconnect journey shares, and the fixtures that
//! observe teardown through the root scope's drain.
//!
//! The deadlines live here because every module in this tree measures against
//! the same two: one bound that a missing observation must fail at, and one
//! quiet window that a forbidden observation must stay silent through.
//!
//! Both fixtures are the shared armed-window scaffold with a runtime-owned
//! server inside them. The scaffold owns the controller, the armed handshake
//! that keeps the observer off an unarmed checkpoint, and the thread scope that
//! joins that observer whatever the runtime returned; only the server and the
//! window each fixture defines are written here.

use super::servers::{owned_settings, start_owned};
use crate::common::{ArmedWatch, observe_armed_sequence};
use camber::http::Router;
use camber::runtime;
use camber::runtime_test_support::RuntimeCheckpoint;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::mpsc::{Receiver, channel};
use std::time::Duration;

/// Deadline every socket read, task report, and readiness probe is bounded by.
///
/// The scaffold's own bound, not a second one beside it: the armed windows
/// below are polled against this same value, so a deadline redefined here would
/// have to track it by hand.
pub(crate) use crate::common::BOUND;

/// How long a signal that must NOT resolve is watched before it counts as
/// unresolved. Short on purpose: the assertion is falsified by a resolution,
/// not by waiting longer.
pub(super) const QUIET: Duration = Duration::from_millis(500);

/// Bound on the scope drain, which each fixture reaches by dropping its server
/// handle before the runtime closure returns.
pub(super) const DRAIN_BOUND: Duration = Duration::from_secs(5);

/// Budget for a handshake the runtime closure sends only after a whole test
/// body has run.
///
/// [`BOUND`] sizes one observation. A fixture that arms its checkpoint behind
/// the body covers startup, the readiness probe, and every step the body takes
/// with this single wait, so it is measured against a budget that exceeds the
/// sum of the bounds those steps are themselves held to. Each of them fails on
/// its own bound first, which is what keeps a slow-but-correct run from being
/// reported as a handshake that never arrived.
const BODY_BOUND: Duration = Duration::from_secs(120);

/// Worker threads every runtime in this tree runs on.
///
/// Stated once because two places need it and neither can read the other's: the
/// shared scaffold applies its own count before handing the builder to
/// `owned_settings`, and the synchronous harness builds a runtime the scaffold
/// never sees. Four, matching the scaffold, so a blocked observer cannot starve
/// the child it is observing.
pub(super) const WORKER_THREADS: usize = 4;

/// The scope children left once the drain settles: the supervisor driver. The
/// signal watcher exits on `ScopeClosing` before this point.
pub(crate) const DRIVER_ONLY: RuntimeCheckpoint = RuntimeCheckpoint::ScopeWaitObserved(1);

/// The scope children a drain window sees while a handler's producer is still
/// running: the supervisor driver plus that producer. The signal watcher, where
/// it exists, has already exited on `ScopeClosing`.
///
/// Beside [`DRIVER_ONLY`] because the two are one enumeration of what the drain
/// may still be counting, and a third occupancy stated somewhere else would be
/// a vocabulary nobody can read whole.
pub(super) const DRIVER_AND_PRODUCER: RuntimeCheckpoint = RuntimeCheckpoint::ScopeWaitObserved(2);

/// The terminal drain observation: no scope children left at all.
const SCOPE_DRAINED: RuntimeCheckpoint = RuntimeCheckpoint::ScopeWaitObserved(0);

/// Drive `future` to completion from a test thread, bounded by [`BOUND`].
///
/// Every await a case here performs off the runtime is a socket read, a task
/// join, or a checkpoint rendezvous against production this tree proves can
/// regress to never resolving. Without the bound that regression parks the
/// whole test binary instead of failing the case it belongs to, so the bound is
/// written once here rather than at each await. `step` names what the expired
/// bound was waiting for.
pub(super) fn bounded<F>(step: &str, future: F) -> F::Output
where
    F: IntoFuture,
{
    match try_bounded(future) {
        Some(output) => output,
        None => panic!("the bound expired waiting for {step}"),
    }
}

/// [`bounded`] for the callers that must have an expired bound back as a value.
///
/// A caller running inside a served handler cannot unwind into production, and
/// a caller whose other failure carries its own description must not have the
/// two collapsed into one message. Both need the expiry returned rather than
/// panicked.
pub(super) fn try_bounded<F>(future: F) -> Option<F::Output>
where
    F: IntoFuture,
{
    runtime::block_on(async { tokio::time::timeout(BOUND, future).await.ok() })
}

/// Run `body` against a runtime-owned server, then report whether the scope
/// drain observed its child count reach zero before the runtime returned.
///
/// The observer waits twice, against two budgets. The checkpoint is armed
/// behind `body`, so the handshake it waits on first covers startup, the
/// readiness probe, and the whole body and is measured against [`BODY_BOUND`];
/// the window it then probes is one observation and keeps [`BOUND`]. The probe
/// releases the pause even when the wait expired — a pause left armed with
/// nobody to release it wedges the drain, and a wedged drain is a hung test
/// rather than a failing one.
pub(super) fn with_drained_server<T, F>(
    connection_limit: Option<usize>,
    router: Router,
    body: F,
) -> (T, bool)
where
    F: FnOnce(SocketAddr) -> T,
{
    let (value, reached_zero) = observe_armed_sequence(
        |builder| owned_settings(builder, connection_limit),
        move |gate| {
            let (addr, handle) = start_owned(router);
            let value = body(addr);
            // Armed before the handle drop that lets the supervisor driver
            // finish, so the terminal observation cannot slip past unarmed.
            gate.arm(SCOPE_DRAINED);
            drop(handle);
            value
        },
        |watch| {
            watch.wait_armed_within(BODY_BOUND);
            watch.probe(SCOPE_DRAINED, |_| ()).is_some()
        },
    );

    (
        value.expect("the runtime did not return cleanly"),
        reached_zero,
    )
}

/// What a probe of the shutdown window observed.
pub(crate) struct DrainWindow<T> {
    /// The probe's value, or `None` if the drain never reported the child
    /// count that defines the window.
    pub(crate) probed: Option<T>,
    /// Whether the drain went on to observe a zero child count.
    pub(crate) reached_zero: bool,
}

/// Probe the window where root-scope admission has closed but the owned server
/// is still serving.
///
/// `before` runs with the server live and admission open; its value is held
/// until the window opens, so a probe can keep a connection or a spawned
/// producer alive across it. `probe` then runs on an observer thread while the
/// drain is paused at `counted`, and receives that value so it can release what
/// `before` kept alive.
///
/// The server outlives the closure — the handle is returned, not dropped — so
/// only the shutdown this fixture sends ends it.
pub(crate) fn with_drain_window<S, T, B, P>(
    connection_limit: Option<usize>,
    router: Router,
    before: B,
    counted: RuntimeCheckpoint,
    probe: P,
) -> DrainWindow<T>
where
    B: FnOnce(SocketAddr) -> S,
    P: FnOnce(SocketAddr, S) -> T + Send,
    S: Send,
    T: Send,
{
    let (published, arrival) = channel::<(SocketAddr, S)>();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    let (outcome, observed) = observe_armed_sequence(
        |builder| owned_settings(builder, connection_limit),
        move |gate| {
            let (addr, handle) = start_owned(router);
            // Published before the checkpoint is armed, so the observer that
            // the handshake releases finds the value already waiting.
            let _ = published.send((addr, before(addr)));
            runtime::on_cancel(async move {
                let _ = shutdown_rx.await;
            });
            gate.arm(counted);
            handle
        },
        move |watch| observe_drain_window(watch, &arrival, counted, probe, shutdown_tx),
    );

    match outcome {
        Ok(handle) => drop(handle),
        Err(error) => panic!("the runtime did not return cleanly: {error}"),
    }
    observed
}

/// The observer half of [`with_drain_window`]: wait for the window, probe it,
/// then drive teardown to its terminal observation.
///
/// The handshake is measured against [`BODY_BOUND`] for the same reason
/// [`with_drained_server`]'s is: this fixture arms `counted` behind startup, the
/// readiness probe, and a `before` hook that is itself several bounds long, so
/// [`BOUND`] here would report a slow-but-correct run as a checkpoint that was
/// never armed. Each of those steps fails on its own bound first.
///
/// Reading the arrival against a bound is what attributes a closure that
/// published nothing to the publication rather than to a window that never
/// opened.
fn observe_drain_window<S, T, P>(
    watch: &ArmedWatch<'_>,
    arrival: &Receiver<(SocketAddr, S)>,
    counted: RuntimeCheckpoint,
    probe: P,
    shutdown: tokio::sync::oneshot::Sender<()>,
) -> DrainWindow<T>
where
    P: FnOnce(SocketAddr, S) -> T,
{
    // `shutdown` is held to the end of this function on every path. Dropping
    // it resolves the closure's `on_cancel` watcher, so an unwinding probe
    // still ends the server the main thread is blocked on.
    watch.wait_armed_within(BODY_BOUND);
    let (addr, carried) = arrival
        .recv_timeout(BOUND)
        .expect("the runtime closure never published its listener address");

    // Polled against the bound rather than waited on: a drain that never
    // reports this count must fail the test, not park the observer on it.
    let probed = watch.probe(counted, move |_| probe(addr, carried));

    // Arm the terminal observation before requesting shutdown: arming it while
    // the driver still holds the count open is what keeps it from racing.
    //
    // A checkpoint that could not be armed is this harness failing, not the
    // drain, so it is reported as itself rather than returned as a drain that
    // never reached zero. Unwinding here is the same path any other observer
    // failure takes: the panic is caught, the controller kept disarmed until
    // `run` returns, and the payload resumed at the join — and `shutdown`
    // dropping on the way out still ends the server.
    match watch.controller().pause_once(SCOPE_DRAINED) {
        Ok(()) => {}
        Err(error) => panic!("the terminal drain checkpoint could not be armed: {error}"),
    }
    let _ = shutdown.send(());
    let reached_zero = watch.probe(SCOPE_DRAINED, |_| ()).is_some();

    DrainWindow {
        probed,
        reached_zero,
    }
}
