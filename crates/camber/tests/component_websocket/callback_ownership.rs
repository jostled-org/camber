//! The retained direct callback: one join deadline, fixed where its bridge woke
//! it, and never pushed back.
//!
//! Every row drives the production direct bridge over a real loopback peer and
//! reads what the upgrade owner published. The barriers are production edges,
//! not sleeps: the bridge is held at the one edge that sits after the deadline
//! is fixed and before the join begins, which is the only place a later server
//! transition can be released into a deadline that already exists.
//!
//! Time is paused only where a row wants the deadline to arrive without waiting
//! it out. The deadline claims themselves are absolute instants, so they read
//! the same either way; the pause is what turns "the join gave up at exactly
//! this instant, and not one tick before it" into an assertion rather than a
//! wall-clock race.

#![cfg(feature = "ws")]

use std::net::SocketAddr;
use std::time::Duration;

use camber::__private::FORCED_JOIN_GRACE;
use camber::http::mock::{
    ScopedRetainedCallback, UpgradeOwnerEdge, WebSocketCallbackObservation, WebSocketTerminalEdge,
};
use camber::http::{Request, Router, ServerHandle, WsCloseCause, WsConn, WsReceiver, WsSender};

use crate::common::{
    ASYNC_EVENT_TIMEOUT, CallbackPark, DropWitnesses, FrozenClock, Observers, OwnerPoint, Owns,
    RetainedCallbackListener, TIMER_TICK, TraceCapture, arm_point, assert_callbacks_own,
    assert_cancelled_fields, block_on_detached, callback_gate, capture_events, close_ws_peer,
    lifecycle_event, only_event, park_until_released, published_callbacks, release_point,
    stall_callback_transport, transferred_child, upgraded_ws_peer,
};

/// The route every row offers its bridge on.
const SOCKET_ROUTE: &str = "/ws";

/// The drain bound every row's server is built with.
///
/// Long enough that a graceful row's aggregate expiry is unmistakably far from
/// the fixed forced-join grace, so a deadline built from the wrong one of the
/// two cannot pass by arithmetic coincidence.
const DRAIN_BOUND: Duration = Duration::from_secs(20);

/// The edge every row holds its bridge at: after the join deadline is fixed,
/// before the join begins.
const JOIN_EDGE: UpgradeOwnerEdge = UpgradeOwnerEdge::BeforeCallbackSettle;

/// The one WARN event a callback dropped at its deadline publishes.
const CANCELLED_EVENT: &str = "name=camber.websocket.callback.cancelled";

/// How much of a held join's grace a row spends before escalating.
///
/// Most of it, so the forced window the escalation opens ends well clear of the
/// deadline already running. It is the row's own arrangement and never a wait:
/// the clock is frozen, and this is spent in one step.
const ESCALATION_OFFSET: Duration = Duration::from_millis(60);

/// Require exactly `expected` callback futures to still hold their captures,
/// and every one already destroyed to have gone before any disposition was
/// named.
///
/// The factory enters one witness and moves it into the future it returns, so
/// a witness goes only when that future is destroyed — by returning or by being
/// dropped at its deadline. Production publishes a disposition only after the
/// future is gone, so a witness that saw one already published outlived the
/// settlement that named it.
fn assert_live(witnesses: &DropWitnesses, expected: usize, context: &str) {
    let dropped = witnesses.dropped();
    assert_eq!(
        witnesses.entered() - dropped.len(),
        expected,
        "{context}: the wrong number of callback futures still held their captures"
    );
    assert!(
        dropped.iter().all(|at| at.dispositions == 0),
        "{context}: a disposition was published while its callback still held its captures: {dropped:?}"
    );
}

/// A router whose one bridge parks its callback on `parked`.
///
/// The callback suspends on the gate and nothing else, so closing its
/// endpoints wakes nothing: only the release or the bridge's deadline ends it.
fn parked_callback_router(parked: &CallbackPark, witnesses: &DropWitnesses) -> Router {
    let parked = parked.clone();
    let witnesses = witnesses.clone();
    let mut router = Router::new();
    router.ws(SOCKET_ROUTE, move |_request: &Request, conn: WsConn| {
        let parked = parked.clone();
        let capture = witnesses.enter();
        async move {
            let _held = (capture, conn);
            park_until_released(&parked).await;
            Ok(())
        }
    });
    router
}

/// One row's server: a real listener, an observer over it, its handle, and the
/// drop witnesses of its callback futures.
struct ParkedServer {
    addr: SocketAddr,
    controller: ScopedRetainedCallback,
    handle: ServerHandle,
    witnesses: DropWitnesses,
}

/// Serve the router `route` builds over this server's own drop witnesses.
///
/// The listener, the observer, the witnesses, and the drain bound are the same
/// for every row; only what the callback does with its connection differs.
async fn parked_server_with(route: impl FnOnce(&DropWitnesses) -> Router) -> ParkedServer {
    let bound = RetainedCallbackListener::bind().await;
    let witnesses = DropWitnesses::new(Observers::of(&bound.controller));
    let (addr, controller, handle) = bound.serve(route(&witnesses), DRAIN_BOUND);
    ParkedServer {
        addr,
        controller,
        handle,
        witnesses,
    }
}

async fn parked_server(parked: &CallbackPark) -> ParkedServer {
    parked_server_with(|witnesses| parked_callback_router(parked, witnesses)).await
}

/// A server whose callback hands its split endpoints to the row, then parks.
async fn escaped_parked_server(
    parked: &CallbackPark,
) -> (
    ParkedServer,
    tokio::sync::mpsc::Receiver<(WsSender, WsReceiver)>,
) {
    let (sender, escaped) = tokio::sync::mpsc::channel(1);
    let server =
        parked_server_with(|witnesses| escaping_callback_router(parked, witnesses, sender)).await;
    (server, escaped)
}

/// A router whose one bridge hands its split endpoints out on `sender`, then
/// parks its callback on `parked`.
fn escaping_callback_router(
    parked: &CallbackPark,
    witnesses: &DropWitnesses,
    sender: tokio::sync::mpsc::Sender<(WsSender, WsReceiver)>,
) -> Router {
    let parked = parked.clone();
    let witnesses = witnesses.clone();
    let mut router = Router::new();
    router.ws(SOCKET_ROUTE, move |_request: &Request, conn: WsConn| {
        let sender = sender.clone();
        let parked = parked.clone();
        let capture = witnesses.enter();
        async move {
            let _held = capture;
            sender
                .send(conn.split())
                .await
                .map_err(|_| camber::RuntimeError::ChannelClosed)?;
            park_until_released(&parked).await;
            Ok(())
        }
    });
    router
}

async fn local_terminal_cancels_callback_while_transport_is_pending() {
    let context = "local terminal with a stalled peer";
    let (gate, parked) = callback_gate();
    let (server, mut escaped) = escaped_parked_server(&parked).await;
    let peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    let (sender, receiver) = lifecycle_event(context, escaped.recv())
        .await
        .expect("callback endpoints");
    let clock = FrozenClock::freeze();
    while_frozen(
        stall_callback_transport(&sender, receiver, &server.controller, context),
        context,
    )
    .await;
    let fixed = only_decision(&server.controller, context);
    assert_decision(
        &fixed,
        "none",
        fixed.endpoints_closed_at + FORCED_JOIN_GRACE,
        context,
    );
    assert_eq!(
        server.controller.terminals.observed().terminal,
        Some(WsCloseCause::ReceiverDropped)
    );
    assert_live(&server.witnesses, 1, context);
    assert_still_waiting(&server.controller, context).await;
    advance_onto(fixed.deadline, context).await;
    let decisions = await_decisions(&server.controller, 2, context).await;
    assert_disposition(&decisions, "cancelled", "none", context);
    assert_live(&server.witnesses, 0, context);
    assert!(
        !server.controller.terminals.observed().permit_released,
        "{context}: transport finished before callback destruction"
    );
    assert!(
        !server.controller.terminals.observed().inbound_settled,
        "{context}: pending close settled early"
    );
    assert_eq!(server.controller.stop.observed().phase, "running");
    drop((sender, peer, gate));
    stop_row(server, clock).await;
}

/// Wait until the bridge is held at `point`, under a real-clock bound.
///
/// Every point a row waits at sits on a retained callback's way to its
/// settlement — its terminal commit or the join itself — so a bridge that never
/// arrives is a bridge with no retained callback to settle, which is what the
/// failure says rather than reporting a missing pause.
async fn hold_at<P>(controller: &ScopedRetainedCallback, point: P, context: &str)
where
    P: OwnerPoint,
    ScopedRetainedCallback: Owns<P::Owner>,
{
    tokio::time::timeout(ASYNC_EVENT_TIMEOUT, point.paused_on(controller))
        .await
        .unwrap_or_else(|_| {
            panic!("{context}: the bridge never held a retained callback at {point:?}")
        })
        .unwrap_or_else(|error| panic!("{context}: waiting at {point:?} failed: {error}"));
}

/// Assert every record this listener published names the upgrade its connection
/// transferred.
///
/// The unique-parent half of the claim, read from two writers rather than
/// inferred from one: the connection records the transfer, the bridge records
/// the callback, and a callback beneath a different upgrade — or beneath none —
/// disagrees here. A row with one connection cannot see a callback claimed
/// twice, so it asserts the half it can: this callback's parent is that upgrade.
fn assert_owned_by_the_transferred_upgrade(
    controller: &ScopedRetainedCallback,
    decisions: &[WebSocketCallbackObservation],
    context: &str,
) {
    assert_callbacks_own(decisions, transferred_child(controller, context), context);
}

/// The one decision published so far, or a failure naming what was there.
fn only_decision(
    controller: &ScopedRetainedCallback,
    context: &str,
) -> WebSocketCallbackObservation {
    let decisions = published_callbacks(controller);
    assert_eq!(
        decisions.len(),
        1,
        "{context}: exactly one callback decision was expected: {decisions:?}"
    );
    decisions[0]
}

/// Wait until `count` decisions have been published, without letting the
/// runtime idle.
///
/// A yield loop rather than a sleep, and deliberately so under paused time: an
/// idle runtime auto-advances its clock, and a wait that advanced time would
/// fire the very deadline the row is about to measure.
async fn await_decisions(
    controller: &ScopedRetainedCallback,
    count: usize,
    context: &str,
) -> Box<[WebSocketCallbackObservation]> {
    for _ in 0..DECISION_YIELDS {
        let decisions = published_callbacks(controller);
        if decisions.len() >= count {
            return decisions;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "{context}: {count} callback decisions never arrived: {:?}",
        published_callbacks(controller)
    )
}

/// How many turns a decision is given to arrive before the row fails.
///
/// A bound in turns rather than in time, because time is what the surrounding
/// rows are measuring. Every decision here is published within a handful of
/// turns of the release that produces it, so this is a runaway guard.
const DECISION_YIELDS: usize = 100_000;

/// How many turns a join is watched for before it counts as still waiting.
///
/// Enough for the released bridge to be scheduled, reach the join, and park on
/// its deadline several times over. The clock cannot move during these turns —
/// a runnable task is always ready — so a decision arriving here would be one
/// production made before the instant it published.
const WAITING_YIELDS: usize = 256;

/// Await `future` without ever letting the runtime idle.
///
/// A frozen clock advances itself the moment nothing is runnable, and every row
/// that waits for a production edge under one has a forced window armed just
/// beyond it. Polling the future between yields keeps a task runnable, so the
/// wait is bounded in turns and the clock stays exactly where the row put it.
async fn while_frozen<F>(future: F, context: &str) -> F::Output
where
    F: std::future::Future,
{
    let mut future = std::pin::pin!(future);
    for _ in 0..DECISION_YIELDS {
        let polled =
            std::future::poll_fn(|context| std::task::Poll::Ready(future.as_mut().poll(context)))
                .await;
        match polled {
            std::task::Poll::Ready(output) => return output,
            std::task::Poll::Pending => tokio::task::yield_now().await,
        }
    }
    panic!("{context}: nothing answered while the clock was frozen")
}

/// Wait until this server's stop state has committed `phase`.
async fn stop_phase_reached(controller: &ScopedRetainedCallback, phase: &str) {
    while controller.stop.observed().phase != phase {
        tokio::task::yield_now().await;
    }
}

/// Prove the join is waiting on its deadline rather than giving up at once.
///
/// Runs with the clock frozen, which is the whole assertion: the deadline is
/// still in the future, and no turn of the runtime moves it closer.
async fn assert_still_waiting(controller: &ScopedRetainedCallback, context: &str) {
    for _ in 0..WAITING_YIELDS {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        published_callbacks(controller).len(),
        1,
        "{context}: the join gave up before its deadline"
    );
}

/// Move the frozen clock onto `deadline`, and no further than its own tick.
///
/// Every other forced window in these rows sits a whole grace beyond this
/// instant, so a step this small reaches the join's deadline and nothing else:
/// which owner answered first is decided by the table, not by the scheduler.
async fn advance_onto(deadline: tokio::time::Instant, context: &str) {
    let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
    assert!(
        !remaining.is_zero(),
        "{context}: the join deadline had already passed before the row advanced to it"
    );
    tokio::time::advance(remaining + TIMER_TICK).await;
}

/// Assert one published decision against the row that expected it.
fn assert_decision(
    decision: &WebSocketCallbackObservation,
    entered: &str,
    deadline: tokio::time::Instant,
    context: &str,
) {
    assert_eq!(
        decision.entered, entered,
        "{context}: the deadline was fixed from the wrong committed phase: {decision:?}"
    );
    assert_eq!(
        decision.deadline, deadline,
        "{context}: the join deadline is not the one the table fixes: {decision:?}"
    );
}

/// Assert the last decision is the disposition the row expected.
fn assert_disposition(
    decisions: &[WebSocketCallbackObservation],
    disposition: &str,
    shutdown: &str,
    context: &str,
) {
    let settled = decisions
        .last()
        .unwrap_or_else(|| panic!("{context}: nothing was published about the callback"));
    assert_eq!(
        settled.disposition,
        Some(disposition),
        "{context}: the callback settled as something else: {settled:?}"
    );
    assert_eq!(
        settled.shutdown,
        Some(shutdown),
        "{context}: the disposition named the wrong transition: {settled:?}"
    );
}

/// Assert every published deadline sits at or before the first one.
///
/// The whole no-rebase rule, read from the record production wrote rather than
/// from the two endpoints of it: a deadline that moved later at any point in
/// between would be in this list.
fn assert_never_rebased(decisions: &[WebSocketCallbackObservation], context: &str) {
    let first = decisions
        .first()
        .unwrap_or_else(|| panic!("{context}: nothing was published about the callback"));
    for decision in decisions {
        assert!(
            decision.deadline <= first.deadline,
            "{context}: a later event pushed the join deadline back: {decisions:?}"
        );
    }
}

/// The aggregate expiry this server's graceful commit minted.
fn aggregate_expiry(controller: &ScopedRetainedCallback, context: &str) -> tokio::time::Instant {
    controller
        .stop
        .observed()
        .aggregate_deadline
        .unwrap_or_else(|| panic!("{context}: the graceful commit minted no aggregate expiry"))
}

/// The instant this server's forced phase committed.
fn forced_commit(controller: &ScopedRetainedCallback, context: &str) -> tokio::time::Instant {
    controller
        .stop
        .observed()
        .forced_commit
        .unwrap_or_else(|| panic!("{context}: no forced phase committed"))
}

/// Table row: a peer terminal on a running server.
///
/// The whole first line of the table. Nothing has been asked of the server, so
/// the grace runs from the endpoint close itself, and the disposition reports
/// that no transition was behind it. Race-free by construction: a running
/// server has armed no deadline of its own, so the join's is the only timer in
/// the runtime.
async fn peer_terminal_on_a_running_server() {
    let context = "the peer terminal row";
    let (gate, parked) = callback_gate();
    let server = parked_server(&parked).await;
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(&server.controller, JOIN_EDGE, context);
    close_ws_peer(&mut peer, context).await;
    hold_at(&server.controller, JOIN_EDGE, context).await;

    let fixed = only_decision(&server.controller, context);
    assert_decision(
        &fixed,
        "none",
        fixed.endpoints_closed_at + FORCED_JOIN_GRACE,
        context,
    );

    let capture = capture_events(CANCELLED_EVENT);
    let clock = FrozenClock::freeze();
    release_point(&server.controller, JOIN_EDGE, context);
    // With the clock frozen the join is still waiting, which is what makes the
    // step onto the deadline evidence rather than a coincidence of ordering.
    assert_still_waiting(&server.controller, context).await;
    assert_live(&server.witnesses, 1, context);
    advance_onto(fixed.deadline, context).await;
    let decisions = await_decisions(&server.controller, 2, context).await;

    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    assert_disposition(&decisions, "cancelled", "none", context);
    assert_live(&server.witnesses, 0, context);
    assert_cancelled_event(&capture, "peer closed", "none", context);
    drop(gate);
    stop_row(server, clock).await;
}

/// Table row: a peer terminal, then a cancellation the deadline must ignore.
///
/// The no-rebase rule at its sharpest. The cancellation commits after the
/// endpoints closed, so the forced window it would open is strictly later than
/// the one already running, and the join keeps the earlier. The disposition
/// still reports the transition, because a local terminal reports whatever the
/// server committed while it waited.
async fn cancellation_after_a_peer_terminal_never_rebases() {
    let context = "the peer-then-cancel row";
    let (gate, parked) = callback_gate();
    let server = parked_server(&parked).await;
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(&server.controller, JOIN_EDGE, context);
    close_ws_peer(&mut peer, context).await;
    hold_at(&server.controller, JOIN_EDGE, context).await;

    let fixed = only_decision(&server.controller, context);
    let expected = fixed.endpoints_closed_at + FORCED_JOIN_GRACE;
    assert_decision(&fixed, "none", expected, context);

    let capture = capture_events(CANCELLED_EVENT);
    let clock = FrozenClock::freeze();
    // Spent before the escalation, so the forced window the server opens ends a
    // clear margin after the one this deadline already runs on. Without it the
    // two would land on the same timer tick, and a row that reached its
    // disposition would only be saying which owner the scheduler picked.
    tokio::time::advance(ESCALATION_OFFSET).await;
    server.handle.cancel();
    let escalated = forced_commit(&server.controller, context) + FORCED_JOIN_GRACE;
    assert!(
        escalated > expected + TIMER_TICK,
        "{context}: the row did not put the escalation clear of the fixed deadline"
    );
    release_point(&server.controller, JOIN_EDGE, context);
    assert_still_waiting(&server.controller, context).await;
    assert_live(&server.witnesses, 1, context);
    advance_onto(expected, context).await;
    let decisions = await_decisions(&server.controller, 2, context).await;

    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    assert_decision(&decisions[1], "none", expected, context);
    assert_disposition(&decisions, "cancelled", "cancelled", context);
    assert_live(&server.witnesses, 0, context);
    assert_cancelled_event(&capture, "peer closed", "cancelled", context);
    drop(gate);
    stop_row(server, clock).await;
}

/// Table row: a graceful stop, whose deadline is the aggregate's and not a
/// fresh grace.
///
/// The expiry itself, with nothing added to it. The fixed grace after the drain
/// belongs to the server, which spends it waiting for the connection above this
/// callback to report what the join decided; a callback holding a copy of that
/// grace would expire on the instant its connection is taken away.
///
/// Also the cooperative half of the contract: a callback that returns inside
/// its window settles as a completion and publishes no cancelled event at all.
async fn graceful_stop_borrows_the_aggregate_expiry() {
    let context = "the graceful row";
    let (gate, parked) = callback_gate();
    let server = parked_server(&parked).await;
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(&server.controller, JOIN_EDGE, context);
    let capture = capture_events(CANCELLED_EVENT);
    server.handle.shutdown();
    // The bridge owes this peer the full handshake, so the peer answers it.
    close_ws_peer(&mut peer, context).await;
    hold_at(&server.controller, JOIN_EDGE, context).await;

    let fixed = only_decision(&server.controller, context);
    let expiry = aggregate_expiry(&server.controller, context);
    assert_decision(&fixed, "graceful", expiry, context);
    assert!(
        fixed.deadline > fixed.endpoints_closed_at + FORCED_JOIN_GRACE,
        "{context}: the drain row was given the fixed grace instead of the aggregate: {fixed:?}"
    );

    // Frozen from here, so this row ends on the same clock discipline as the
    // rows that measure a deadline: nothing below waits out a real interval,
    // and the teardown resumes it.
    let clock = FrozenClock::freeze();
    // Released before the join begins, so the join finds a callback that has
    // already returned and no deadline is spent proving it.
    drop(gate);
    release_point(&server.controller, JOIN_EDGE, context);
    let decisions = await_decisions(&server.controller, 2, context).await;

    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    // The drain it entered, not the deadline it never reached: a callback that
    // returned inside its window has no expiry to name.
    assert_disposition(&decisions, "completed", "graceful", context);
    assert_live(&server.witnesses, 0, context);
    assert!(
        !capture.recorded(&[CANCELLED_EVENT]),
        "{context}: a cooperative callback published the cancelled event"
    );
    stop_row(server, clock).await;
}

/// Table row: a cancellation inside the drain, which brings the deadline
/// forward to the commit that caused it.
async fn cancellation_inside_a_drain_shortens_to_the_commit() {
    let context = "the graceful-to-cancel row";
    let (gate, parked) = callback_gate();
    let server = parked_server(&parked).await;
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(&server.controller, JOIN_EDGE, context);
    server.handle.shutdown();
    close_ws_peer(&mut peer, context).await;
    hold_at(&server.controller, JOIN_EDGE, context).await;

    let fixed = only_decision(&server.controller, context);
    let expiry = aggregate_expiry(&server.controller, context);
    assert_decision(&fixed, "graceful", expiry, context);

    let clock = FrozenClock::freeze();
    server.handle.cancel();
    let shortened = forced_commit(&server.controller, context) + FORCED_JOIN_GRACE;
    release_point(&server.controller, JOIN_EDGE, context);
    let narrowed = await_decisions(&server.controller, 2, context).await;

    assert_decision(&narrowed[1], "graceful", shortened, context);
    assert!(
        shortened < fixed.deadline,
        "{context}: the escalation did not bring the deadline forward: {narrowed:?}"
    );
    assert_never_rebased(&narrowed, context);

    drop(gate);
    let decisions = await_decisions(&server.controller, 3, context).await;
    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    assert_disposition(&decisions, "completed", "cancelled", context);
    assert_live(&server.witnesses, 0, context);
    stop_row(server, clock).await;
}

/// Table row: a cancellation that committed before this bridge applied any
/// server control.
///
/// The aggregate contributes nothing here — a cancellation mints none — so the
/// deadline is the fixed grace from the endpoint close, and the disposition
/// names the transition the bridge entered under.
async fn cancellation_before_control_uses_the_fixed_grace() {
    let context = "the cancel-first row";
    let (gate, parked) = callback_gate();
    let server = parked_server(&parked).await;
    let _peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(&server.controller, JOIN_EDGE, context);
    server.handle.cancel();
    hold_at(&server.controller, JOIN_EDGE, context).await;

    let fixed = only_decision(&server.controller, context);
    assert_decision(
        &fixed,
        "cancelled",
        fixed.endpoints_closed_at + FORCED_JOIN_GRACE,
        context,
    );
    assert!(
        controller_minted_no_aggregate(&server.controller),
        "{context}: a cancellation minted an aggregate expiry"
    );

    let clock = FrozenClock::freeze();
    drop(gate);
    release_point(&server.controller, JOIN_EDGE, context);
    let decisions = await_decisions(&server.controller, 2, context).await;

    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    assert_disposition(&decisions, "completed", "cancelled", context);
    assert_live(&server.witnesses, 0, context);
    stop_row(server, clock).await;
}

/// Table row: an aggregate deadline that expired before this bridge applied
/// graceful control.
///
/// The expired aggregate contributes no further wait, so this row reads exactly
/// like the cancellation row's arithmetic and names a different transition. The
/// bridge is held at its own terminal edge while the drain runs out, which is
/// what puts the expiry ahead of the endpoint close.
async fn expiry_before_control_uses_the_fixed_grace() {
    let context = "the timeout-first row";
    let (gate, parked) = callback_gate();
    let server = parked_server(&parked).await;
    let peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(
        &server.controller,
        WebSocketTerminalEdge::AfterCommit,
        context,
    );
    arm_point(&server.controller, JOIN_EDGE, context);
    server.handle.shutdown();
    hold_at(
        &server.controller,
        WebSocketTerminalEdge::AfterCommit,
        context,
    )
    .await;

    // Spent while the bridge is held short of its settlement, so the drain runs
    // out before this upgrade ever applies the graceful control it committed
    // its cause from. Everything after it runs on a frozen clock: the forced
    // window the expiry opens is what would otherwise take this connection away
    // mid-settlement, and an idle runtime would advance straight onto it.
    let clock = FrozenClock::freeze();
    tokio::time::advance(DRAIN_BOUND + TIMER_TICK).await;
    while_frozen(
        stop_phase_reached(&server.controller, "deadline-expired"),
        context,
    )
    .await;
    release_point(
        &server.controller,
        WebSocketTerminalEdge::AfterCommit,
        context,
    );
    while_frozen(
        server.controller.upgrades.wait_until_paused(JOIN_EDGE),
        context,
    )
    .await
    .unwrap_or_else(|error| panic!("{context}: waiting at the join edge failed: {error}"));

    let fixed = only_decision(&server.controller, context);
    assert_decision(
        &fixed,
        "deadline-expired",
        fixed.endpoints_closed_at + FORCED_JOIN_GRACE,
        context,
    );

    drop(gate);
    release_point(&server.controller, JOIN_EDGE, context);
    let decisions = await_decisions(&server.controller, 2, context).await;
    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    assert_disposition(&decisions, "completed", "deadline-expired", context);
    assert_live(&server.witnesses, 0, context);
    drop(peer);
    stop_expired_row(server, clock, context).await;
}

/// Whether this server minted no aggregate expiry at all.
fn controller_minted_no_aggregate(controller: &ScopedRetainedCallback) -> bool {
    controller.stop.observed().aggregate_deadline.is_none()
}

/// Assert the one WARN event a cancelled callback owes, and its closed fields.
fn assert_cancelled_event(capture: &TraceCapture, cause: &str, shutdown: &str, context: &str) {
    let events = capture.events();
    let event = only_event(&events, CANCELLED_EVENT, context);
    assert_cancelled_fields(event, cause, shutdown, context);
}

/// End one row whose server was never past its drain: it joins as stopped.
async fn stop_row(server: ParkedServer, clock: FrozenClock) {
    crate::common::assert_server_joined(cancel_and_join(server, clock).await);
}

/// End the row whose drain ran out: its server answers the expiry it committed.
///
/// The forced phase was fixed before teardown, so the cancellation below finds
/// a settled phase and cannot replace the result.
async fn stop_expired_row(server: ParkedServer, clock: FrozenClock, context: &str) {
    match cancel_and_join(server, clock).await {
        Ok(Err(camber::RuntimeError::Timeout)) => {}
        other => panic!("{context}: the server did not answer its committed expiry: {other:?}"),
    }
}

/// Take one row's server down under a live clock and join it.
///
/// Every row has already dropped its gate's release end by the time it gets
/// here, so the callback is on its way out rather than parked on a gate nothing
/// will release. Every row freezes the clock before it gets here and hands the
/// guard in, so time is resumed first and exactly once: a paused clock would
/// leave the server's own forced window frozen and the join below waiting on an
/// instant nothing advances to.
async fn cancel_and_join(
    server: ParkedServer,
    clock: FrozenClock,
) -> Result<Result<(), camber::RuntimeError>, tokio::time::error::Elapsed> {
    drop(clock);
    server.handle.cancel();
    tokio::time::timeout(ASYNC_EVENT_TIMEOUT, server.handle).await
}

// 3.T1, revised in 2.T6 — Invariant 8: the direct WebSocket callback future is
// polled inline by its bridge; every bridge terminal bounds its settlement, and
// callback completion or a cancelled disposition — published only after the
// future's captures are gone — precedes upgrade settlement.
//
// Deadline-table rows cover entry phases, later escalation, and cooperative
// return. The stalled-transport row proves cleanup cannot delay callback expiry.
//
// Each row owns a current-thread runtime, its listener, and its callback gate,
// so a paused clock or a cancelled server in one cannot decide another.
#[test]
fn callback_join_deadline_table_is_exact_and_never_rebases() {
    block_on_detached(local_terminal_cancels_callback_while_transport_is_pending());
    block_on_detached(peer_terminal_on_a_running_server());
    block_on_detached(cancellation_after_a_peer_terminal_never_rebases());
    block_on_detached(graceful_stop_borrows_the_aggregate_expiry());
    block_on_detached(cancellation_inside_a_drain_shortens_to_the_commit());
    block_on_detached(cancellation_before_control_uses_the_fixed_grace());
    block_on_detached(expiry_before_control_uses_the_fixed_grace());
}
