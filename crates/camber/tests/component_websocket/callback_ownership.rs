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
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camber::__private::FORCED_JOIN_GRACE;
use camber::http::mock::{
    ScopedRetainedCallback, UpgradeOwnerEdge, WebSocketCallbackObservation, WebSocketTerminalEdge,
    retained_callback,
};
use camber::http::{Request, Router, ServerHandle, WsConn};

use crate::common::{
    ASYNC_EVENT_TIMEOUT, TraceCapture, arm_point, assert_callbacks_own, callback_gate,
    capture_events, close_ws_peer, park_until_released, published_callbacks, release_point,
    transferred_upgrades, upgraded_ws_peer,
};

/// The route every row offers its bridge on.
const SOCKET_ROUTE: &str = "/ws";

/// The drain bound every row's server is built with.
///
/// Long enough that a graceful row's aggregate expiry is unmistakably far from
/// the fixed forced-join grace, so a deadline built from the wrong one of the
/// two cannot pass by arithmetic coincidence.
const DRAIN_BOUND: Duration = Duration::from_secs(20);

/// The one WARN event an outstanding callback publishes.
const OUTSTANDING_EVENT: &str = "name=camber.websocket.callback.outstanding";

/// How much of a held join's grace a row spends before escalating.
///
/// Most of it, so the forced window the escalation opens ends well clear of the
/// deadline already running. It is the row's own arrangement and never a wait:
/// the clock is frozen, and this is spent in one step.
const ESCALATION_OFFSET: Duration = Duration::from_millis(60);

/// A router whose one bridge parks its callback on `parked`.
fn parked_callback_router(parked: &Arc<Mutex<Receiver<()>>>) -> Router {
    let parked = Arc::clone(parked);
    let mut router = Router::new();
    router.ws(SOCKET_ROUTE, move |_request: &Request, _conn: WsConn| {
        park_until_released(&parked);
        Ok(())
    });
    router
}

/// One row's server: a real listener, an observer over it, and its handle.
struct ParkedServer {
    addr: SocketAddr,
    controller: ScopedRetainedCallback,
    handle: ServerHandle,
}

fn parked_server(parked: &Arc<Mutex<Receiver<()>>>) -> ParkedServer {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the callback fixture");
    listener
        .set_nonblocking(true)
        .expect("the callback fixture's listener takes a Tokio reactor");
    let listener =
        tokio::net::TcpListener::from_std(listener).expect("adopt the callback fixture's listener");
    let addr = listener.local_addr().expect("read the fixture's address");
    let controller = retained_callback(addr).expect("register the callback observer");
    let policy = camber::http::ServerPolicy::default()
        .shutdown_timeout(DRAIN_BOUND)
        .expect("a positive drain bound");
    let handle = camber::http::server(parked_callback_router(parked))
        .policy(policy)
        .serve_background(listener)
        .expect("the callback fixture requires a Tokio runtime");
    ParkedServer {
        addr,
        controller,
        handle,
    }
}

/// Wait until the bridge is held at `edge`, under a real-clock bound.
///
/// The edge every row waits at sits inside the settlement of a retained
/// callback, so a bridge that never arrives is a bridge with no retained
/// callback to settle — which is what the failure says rather than reporting a
/// missing pause.
async fn hold_at(controller: &ScopedRetainedCallback, edge: UpgradeOwnerEdge, context: &str) {
    tokio::time::timeout(
        ASYNC_EVENT_TIMEOUT,
        controller.upgrades.wait_until_paused(edge),
    )
    .await
    .unwrap_or_else(|_| panic!("{context}: the bridge never held a retained callback at {edge:?}"))
    .unwrap_or_else(|error| panic!("{context}: waiting at {edge:?} failed: {error}"));
}

/// The one upgrade this listener's connection took as its child.
fn transferred_child(controller: &ScopedRetainedCallback, context: &str) -> (u64, u64) {
    let transferred = transferred_upgrades(&controller.connections.observed());
    assert_eq!(
        transferred.len(),
        1,
        "{context}: exactly one upgrade transfer was expected: {transferred:?}"
    );
    transferred[0]
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

/// The granularity the runtime's timer rounds a deadline up to.
///
/// A sleep does not fire at its own instant but at the first tick at or after
/// it, so a row that advanced exactly onto a deadline would wait on a timer
/// that has not been reached. The margin is what crosses that rounding, and it
/// is why the exactness of a deadline is asserted on the instant production
/// published rather than on the tick a row happened to stop at.
const TIMER_TICK: Duration = Duration::from_millis(1);

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

/// Run one row on a runtime of its own.
///
/// A runtime per row, because a row pauses its clock and stops its server: two
/// rows sharing one runtime would have the first row's frozen time and forced
/// abort deciding the second row's deadline.
fn row<F>(body: F)
where
    F: AsyncFnOnce(),
{
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("the callback rows need a current-thread runtime");
    runtime.block_on(body());
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
    let server = parked_server(&parked);
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    close_ws_peer(&mut peer, context).await;
    hold_at(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
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

    let capture = capture_events(OUTSTANDING_EVENT);
    tokio::time::pause();
    release_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    // With the clock frozen the join is still waiting, which is what makes the
    // step onto the deadline evidence rather than a coincidence of ordering.
    assert_still_waiting(&server.controller, context).await;
    advance_onto(fixed.deadline, context).await;
    let decisions = await_decisions(&server.controller, 2, context).await;

    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    assert_disposition(
        &decisions,
        "outstanding-after-forced-grace",
        "none",
        context,
    );
    assert_outstanding_event(&capture, "peer closed", "none", context);
    drop(gate);
    stop_row(server).await;
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
    let server = parked_server(&parked);
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    close_ws_peer(&mut peer, context).await;
    hold_at(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    )
    .await;

    let fixed = only_decision(&server.controller, context);
    let expected = fixed.endpoints_closed_at + FORCED_JOIN_GRACE;
    assert_decision(&fixed, "none", expected, context);

    let capture = capture_events(OUTSTANDING_EVENT);
    tokio::time::pause();
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
    release_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    assert_still_waiting(&server.controller, context).await;
    advance_onto(expected, context).await;
    let decisions = await_decisions(&server.controller, 2, context).await;

    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    assert_decision(&decisions[1], "none", expected, context);
    assert_disposition(
        &decisions,
        "outstanding-after-forced-grace",
        "cancelled",
        context,
    );
    assert_outstanding_event(&capture, "peer closed", "cancelled", context);
    drop(gate);
    stop_row(server).await;
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
/// its window settles as a completion and publishes no outstanding event at
/// all.
async fn graceful_stop_borrows_the_aggregate_expiry() {
    let context = "the graceful row";
    let (gate, parked) = callback_gate();
    let server = parked_server(&parked);
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    let capture = capture_events(OUTSTANDING_EVENT);
    server.handle.shutdown();
    // The bridge owes this peer the full handshake, so the peer answers it.
    close_ws_peer(&mut peer, context).await;
    hold_at(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    )
    .await;

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
    tokio::time::pause();
    // Released before the join begins, so the join finds a callback that has
    // already returned and no deadline is spent proving it.
    drop(gate);
    release_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    let decisions = await_decisions(&server.controller, 2, context).await;

    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    // The drain it entered, not the deadline it never reached: a callback that
    // returned inside its window has no expiry to name.
    assert_disposition(&decisions, "completed", "graceful", context);
    assert!(
        !capture.recorded(&[OUTSTANDING_EVENT]),
        "{context}: a cooperative callback published the outstanding event"
    );
    stop_row(server).await;
}

/// Table row: a cancellation inside the drain, which brings the deadline
/// forward to the commit that caused it.
async fn cancellation_inside_a_drain_shortens_to_the_commit() {
    let context = "the graceful-to-cancel row";
    let (gate, parked) = callback_gate();
    let server = parked_server(&parked);
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    server.handle.shutdown();
    close_ws_peer(&mut peer, context).await;
    hold_at(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    )
    .await;

    let fixed = only_decision(&server.controller, context);
    let expiry = aggregate_expiry(&server.controller, context);
    assert_decision(&fixed, "graceful", expiry, context);

    tokio::time::pause();
    server.handle.cancel();
    let shortened = forced_commit(&server.controller, context) + FORCED_JOIN_GRACE;
    release_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
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
    stop_row(server).await;
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
    let server = parked_server(&parked);
    let _peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    server.handle.cancel();
    hold_at(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    )
    .await;

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

    tokio::time::pause();
    drop(gate);
    release_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    let decisions = await_decisions(&server.controller, 2, context).await;

    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    assert_disposition(&decisions, "completed", "cancelled", context);
    stop_row(server).await;
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
    let server = parked_server(&parked);
    let peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    arm_point(
        &server.controller,
        WebSocketTerminalEdge::AfterCommit,
        context,
    );
    arm_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    server.handle.shutdown();
    tokio::time::timeout(
        ASYNC_EVENT_TIMEOUT,
        server
            .controller
            .terminals
            .wait_until_paused(WebSocketTerminalEdge::AfterCommit),
    )
    .await
    .unwrap_or_else(|_| panic!("{context}: the bridge never committed its terminal"))
    .unwrap_or_else(|error| panic!("{context}: waiting on the terminal edge failed: {error}"));

    // Spent while the bridge is held short of its settlement, so the drain runs
    // out before this upgrade ever applies the graceful control it committed
    // its cause from. Everything after it runs on a frozen clock: the forced
    // window the expiry opens is what would otherwise take this connection away
    // mid-settlement, and an idle runtime would advance straight onto it.
    tokio::time::pause();
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
        server
            .controller
            .upgrades
            .wait_until_paused(UpgradeOwnerEdge::BeforeCallbackJoin),
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
    release_point(
        &server.controller,
        UpgradeOwnerEdge::BeforeCallbackJoin,
        context,
    );
    let decisions = await_decisions(&server.controller, 2, context).await;
    assert_never_rebased(&decisions, context);
    assert_owned_by_the_transferred_upgrade(&server.controller, &decisions, context);
    assert_disposition(&decisions, "completed", "deadline-expired", context);
    drop(peer);
    stop_row(server).await;
}

/// Whether this server minted no aggregate expiry at all.
fn controller_minted_no_aggregate(controller: &ScopedRetainedCallback) -> bool {
    controller.stop.observed().aggregate_deadline.is_none()
}

/// Assert the one WARN event an outstanding callback owes, and its closed
/// fields.
fn assert_outstanding_event(capture: &TraceCapture, cause: &str, shutdown: &str, context: &str) {
    let events = capture.events();
    assert_eq!(
        events.len(),
        1,
        "{context}: exactly one outstanding event was expected: {events:?}"
    );
    let event = events[0].as_ref();
    crate::common::assert_field_value(event, "name", &OUTSTANDING_EVENT[5..], context);
    crate::common::assert_field_value(
        event,
        "disposition",
        "outstanding-after-forced-grace",
        context,
    );
    crate::common::assert_field_value(event, "shutdown", shutdown, context);
    assert!(
        event.contains(&format!("cause={cause}")),
        "{context}: the event does not name the committed cause: {event}"
    );
}

/// End one row: take its server down under a live clock.
///
/// Every row has already dropped its gate sender by the time it gets here, so
/// the callback is on its way out rather than parked on a channel nothing will
/// close. Time is resumed first, because a paused clock would leave the
/// server's own forced window frozen and the join below waiting on an instant
/// nothing advances to.
async fn stop_row(server: ParkedServer) {
    tokio::time::resume();
    server.handle.cancel();
    let _stopped = tokio::time::timeout(ASYNC_EVENT_TIMEOUT, server.handle).await;
}

// 3.T1 — Invariant 8: the direct WebSocket callback join handle is retained;
// every bridge terminal bounds its join, and callback completion or an emitted
// outstanding-callback disposition precedes upgrade settlement.
//
// Six rows, one per line of the spec's deadline table plus the two rules that
// cut across it: a later transition may bring the deadline forward and never
// push it back, and a cooperative return publishes no outstanding event.
//
// Each row owns its runtime, its listener, and its callback gate, so a paused
// clock or a cancelled server in one cannot decide another.
#[test]
fn callback_join_deadline_table_is_exact_and_never_rebases() {
    row(peer_terminal_on_a_running_server);
    row(cancellation_after_a_peer_terminal_never_rebases);
    row(graceful_stop_borrows_the_aggregate_expiry);
    row(cancellation_inside_a_drain_shortens_to_the_commit);
    row(cancellation_before_control_uses_the_fixed_grace);
    row(expiry_before_control_uses_the_fixed_grace);
}
