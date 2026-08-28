//! Daemon-live proof that a retained callback is a child of its upgrade owner,
//! and that nothing above it settles until that child is joined or named.
//!
//! Every row runs a real background server over a real loopback peer and reads
//! only what production published. The ordering claims are read at the barrier
//! production itself writes: a released permit, a settled upgrade, and a settled
//! connection are each checked against the callback record that was already in
//! place when they arrived, so a row cannot pass by observing two facts that
//! merely both happened.
//!
//! The whole file runs inside one private child process. The peer-only row
//! leaves a real blocking callback parked in application code that Camber has
//! deliberately stopped waiting for, and no test process can take that thread
//! back. The child says its assertions passed while that callback is still
//! there, and the parent reaps it.

#![cfg(feature = "ws")]

use std::net::SocketAddr;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camber::http::mock::{
    ConnectionOwnershipEvent, ConnectionOwnershipObservation, ScopedRetainedCallback,
    WebSocketCallbackObservation, retained_callback,
};
use camber::http::{Request, Router, ServerHandle, WsConn};

use crate::common::{
    TraceCapture, assert_address_reused, assert_callbacks_own, await_live, callback_gate,
    capture_events, close_ws_peer, contain_in_child, park_until_released, published_callbacks,
    transferred_upgrades, upgraded_ws_peer,
};

/// The private mode this file's one child runs under.
const CHILD_MODE: &str = "websocket-callback-ownership";

/// What the child prints once every row has passed.
const ASSERTIONS_COMPLETE: &str = "CALLBACK_OWNERSHIP_ROWS_COMPLETE";

/// How long the parent waits for that marker, and for the reap that follows.
const CHILD_BOUND: Duration = Duration::from_secs(60);

/// How long one live observation has before the row fails.
const LIVE_BOUND: Duration = Duration::from_secs(10);

/// The drain bound the rows that need one wait out for real.
///
/// Short, because a live row spends it: it is the interval between a graceful
/// stop and the expiry that ends it, and nothing here needs it to be long to be
/// the aggregate rather than the fixed grace.
const DRAIN_BOUND: Duration = Duration::from_millis(300);

/// The route every row offers its bridge on.
const SOCKET_ROUTE: &str = "/ws";

/// The one WARN event an outstanding callback publishes.
const OUTSTANDING_EVENT: &str = "name=camber.websocket.callback.outstanding";

/// Whether a row's callback answers the endpoints its bridge closes.
#[derive(Clone, Copy, Eq, PartialEq)]
enum Callback {
    /// It reads until its receive queue closes, then returns.
    Cooperative,
    /// It parks in application code and never answers anything.
    Parked,
}

/// A router whose one bridge answers `callback`.
fn callback_router(callback: Callback, parked: &Arc<Mutex<Receiver<()>>>) -> Router {
    let parked = Arc::clone(parked);
    let mut router = Router::new();
    router.ws(SOCKET_ROUTE, move |_request: &Request, mut conn: WsConn| {
        match callback {
            Callback::Cooperative => while conn.recv().is_some() {},
            Callback::Parked => park_until_released(&parked),
        }
        Ok(())
    });
    router
}

/// One row's server: a real listener, an observer over it, and its handle.
struct LiveServer {
    addr: SocketAddr,
    controller: ScopedRetainedCallback,
    handle: ServerHandle,
}

fn live_server(callback: Callback, parked: &Arc<Mutex<Receiver<()>>>) -> LiveServer {
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
    let handle = camber::http::server(callback_router(callback, parked))
        .policy(policy)
        .serve_background(listener)
        .expect("the callback fixture requires a Tokio runtime");
    LiveServer {
        addr,
        controller,
        handle,
    }
}

/// The one upgrade a single-connection row's connection took as its child.
fn transferred_child(observed: &ConnectionOwnershipObservation, context: &str) -> (u64, u64) {
    let transferred = transferred_upgrades(observed);
    assert_eq!(
        transferred.len(),
        1,
        "{context}: exactly one upgrade transfer was expected: {transferred:?}"
    );
    transferred[0]
}

/// The records that carry a settled disposition.
fn settled_callbacks(controller: &ScopedRetainedCallback) -> Box<[WebSocketCallbackObservation]> {
    published_callbacks(controller)
        .iter()
        .copied()
        .filter(|decided| decided.disposition.is_some())
        .collect()
}

/// Assert every record this listener published names `owner`.
///
/// The parent half of Invariant 5, read from two writers: the connection
/// recorded the transfer, the bridge recorded the callback, and a callback that
/// sits beneath a different upgrade — or beneath none — disagrees here.
fn assert_owned_by(controller: &ScopedRetainedCallback, owner: (u64, u64), context: &str) {
    assert_callbacks_own(&published_callbacks(controller), owner, context);
}

/// The callback disposition this listener published, if one has been.
fn disposition(controller: &ScopedRetainedCallback) -> Option<WebSocketCallbackObservation> {
    settled_callbacks(controller).first().copied()
}

/// Require a disposition to be in place already, at a barrier that follows it.
///
/// The ordering claim of Invariant 8, read the one way that is not two
/// independent observations: production publishes the disposition and only then
/// performs `barrier`, so a `barrier` that is visible with no disposition
/// behind it is the violation itself.
fn assert_disposition_precedes(
    controller: &ScopedRetainedCallback,
    barrier: &str,
    context: &str,
) -> WebSocketCallbackObservation {
    disposition(controller).unwrap_or_else(|| {
        panic!(
            "{context}: {barrier} without the callback having been joined or named: {:?}",
            published_callbacks(controller)
        )
    })
}

/// What one row expects its callback to have settled as.
struct Expected {
    disposition: &'static str,
    /// The closed set of transitions this row's disposition may name.
    ///
    /// One name wherever a barrier fixes it. The escalation row is the one that
    /// has none: a cooperative callback that has already returned is joined
    /// before the abort it raced is ever heard, so the drain it entered under
    /// and the cancellation that overtook it are both truthful answers. The
    /// exact table lives in 3.T1, which holds the join at its own edge and can
    /// fix the order; here the result set is closed and the cleanup below is
    /// identical for either member of it.
    shutdown: &'static [&'static str],
    outstanding_event: bool,
}

/// Drive one row's shared assertions from the peer terminal to address reuse.
///
/// Every row ends the same way and differs only in what it asked the server for
/// before it got here, so the ordering claims are stated once: the permit comes
/// back after the disposition, the upgrade settles under the connection that
/// transferred it, the connection settles after its child, and the address is
/// bindable again.
async fn assert_callback_bounds_settlement(
    server: LiveServer,
    capture: &TraceCapture,
    expected: &Expected,
    context: &str,
) {
    let controller = &server.controller;
    await_live(
        || controller.terminals.observed().permit_released,
        LIVE_BOUND,
        &format!("{context}: the connection permit never came back"),
    )
    .await;
    let settled = assert_disposition_precedes(controller, "the permit came back", context);
    assert_eq!(
        settled.disposition,
        Some(expected.disposition),
        "{context}: the callback settled as something else: {settled:?}"
    );
    let named = settled
        .shutdown
        .unwrap_or_else(|| panic!("{context}: the disposition named no transition: {settled:?}"));
    assert!(
        expected.shutdown.contains(&named),
        "{context}: the disposition named a transition outside this row's result set {:?}: {settled:?}",
        expected.shutdown
    );

    let (connection, upgrade) = transferred_child(&controller.connections.observed(), context);
    assert_owned_by(controller, (connection, upgrade), context);
    let settled_in_time = tokio::time::timeout(LIVE_BOUND, async {
        while !controller.connections.observed().contains(
            ConnectionOwnershipEvent::ConnectionUpgradeSettled {
                connection,
                upgrade,
            },
        ) {
            tokio::task::yield_now().await;
        }
    })
    .await
    .is_ok();
    assert!(
        settled_in_time,
        "{context}: the upgrade child never settled under its connection: events={:?} callbacks={:?} stop={:?}",
        controller.connections.observed().events,
        controller.upgrades.callbacks(),
        controller.stop.observed(),
    );
    assert_disposition_precedes(controller, "the upgrade settled", context);
    await_live(
        || {
            controller
                .connections
                .observed()
                .contains(ConnectionOwnershipEvent::ServerConnectionSettled { connection })
        },
        LIVE_BOUND,
        &format!("{context}: the connection never settled"),
    )
    .await;
    assert_disposition_precedes(controller, "the connection settled", context);

    assert_eq!(
        capture.recorded(&[OUTSTANDING_EVENT]),
        expected.outstanding_event,
        "{context}: the outstanding event's presence is not what this row owes: {:?}",
        capture.events()
    );
    if expected.outstanding_event {
        // Checked against the transition the record named rather than against
        // the row's set, so the event and the observation have to agree about
        // the one callback they both describe.
        assert_outstanding_fields(capture, named, context);
    }

    stop_and_reuse(server, context).await;
}

/// End one row's server, and require the address it served on back.
///
/// The last two steps every row takes, and the only ones that are the same
/// whichever claim the row made: the server is cancelled rather than drained
/// because the claim is already established by the time this runs, and the
/// address is the one fact an out-of-process observer could still check.
async fn stop_and_reuse(server: LiveServer, context: &str) {
    let addr = server.addr;
    server.handle.cancel();
    let _stopped = tokio::time::timeout(LIVE_BOUND, server.handle).await;
    assert_address_reused(addr, context).await;
}

/// Assert the closed fields of the one outstanding event a row published.
fn assert_outstanding_fields(capture: &TraceCapture, shutdown: &str, context: &str) {
    let events = capture.events();
    let event = crate::common::only_event(&events, OUTSTANDING_EVENT, context);
    crate::common::assert_field_value(
        event,
        "disposition",
        "outstanding-after-forced-grace",
        context,
    );
    crate::common::assert_field_value(event, "shutdown", shutdown, context);
    assert!(
        crate::common::field_value(event, "cause").is_some(),
        "{context}: the event names no committed cause: {event}"
    );
}

/// Row: a peer terminal on a running server, answered by a callback that never
/// answers anything.
///
/// The only row whose callback is genuinely retained past its deadline, and the
/// only one that needs the containment this file runs under. A running server
/// has opened no forced window of its own, so the join's deadline is the only
/// bound in play and the disposition is the contract rather than a race.
async fn peer_terminal_outstanding_callback() {
    let context = "the live peer-only row";
    let (_gate, parked) = callback_gate();
    let capture = capture_events(OUTSTANDING_EVENT);
    let server = live_server(Callback::Parked, &parked);
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    close_ws_peer(&mut peer, context).await;
    assert_callback_bounds_settlement(
        server,
        &capture,
        &Expected {
            disposition: "outstanding-after-forced-grace",
            shutdown: &["none"],
            outstanding_event: true,
        },
        context,
    )
    .await;
}

/// Row: a cooperative callback on a peer terminal.
///
/// The negative half of the row above, on the same transport path: a callback
/// that reads its closed receive queue and returns settles as a completion, and
/// Camber says nothing about it.
async fn peer_terminal_cooperative_callback() {
    let context = "the live cooperative row";
    let (_gate, parked) = callback_gate();
    let capture = capture_events(OUTSTANDING_EVENT);
    let server = live_server(Callback::Cooperative, &parked);
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    close_ws_peer(&mut peer, context).await;
    assert_callback_bounds_settlement(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["none"],
            outstanding_event: false,
        },
        context,
    )
    .await;
}

/// Row: a cancellation that reaches the bridge before any other transition.
///
/// The callback is cooperative here on purpose. A cancelled server opens its
/// own forced window at the commit, and the join's window opens a few
/// microseconds later at the endpoint close; both round to the same timer tick,
/// so an outstanding disposition on this path would be reporting which owner
/// the scheduler reached first. What is deterministic — and what this row
/// asserts — is that the join happens, that it names the transition it entered
/// under, and that nothing above it settles first.
async fn cancel_first_transition() {
    let context = "the live cancel-first row";
    let (_gate, parked) = callback_gate();
    let capture = capture_events(OUTSTANDING_EVENT);
    let server = live_server(Callback::Cooperative, &parked);
    let _peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    server.handle.cancel();
    assert_callback_bounds_settlement(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["cancelled"],
            outstanding_event: false,
        },
        context,
    )
    .await;
}

/// Row: a graceful stop escalated to a cancellation before its drain expires.
async fn graceful_to_cancel_transition() {
    let context = "the live graceful-to-cancel row";
    let (_gate, parked) = callback_gate();
    let capture = capture_events(OUTSTANDING_EVENT);
    let server = live_server(Callback::Cooperative, &parked);
    let _peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    server.handle.shutdown();
    await_live(
        || server.controller.stop.observed().phase != "running",
        LIVE_BOUND,
        &format!("{context}: the graceful phase never committed"),
    )
    .await;
    server.handle.cancel();
    assert_callback_bounds_settlement(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["cancelled", "graceful"],
            outstanding_event: false,
        },
        context,
    )
    .await;
}

/// Row: a graceful stop whose drain runs out against a peer that never answers
/// the close it was sent.
///
/// The transition is `graceful`, not the expiry the server went on to reach.
/// The bridge gives its callback's endpoints up when the drain commits, so the
/// entry is fixed there, and a callback that returned cooperatively expired
/// nothing: it reports the drain it entered under. The peer's silence decides
/// how the *server* ends, and this file makes no claim about that — the one
/// entry that reports an expiry is the row that was still outstanding at it.
async fn timeout_first_transition() {
    let context = "the live timeout-first row";
    let (_gate, parked) = callback_gate();
    let capture = capture_events(OUTSTANDING_EVENT);
    let server = live_server(Callback::Cooperative, &parked);
    // Held open and deliberately silent: the bridge owes this peer a close
    // handshake, and a peer that never answers is what makes the drain expire
    // rather than complete.
    let _peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    server.handle.shutdown();
    assert_callback_bounds_settlement(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["graceful"],
            outstanding_event: false,
        },
        context,
    )
    .await;
}

/// Row: a callback that entered under a drain and was still there at the expiry
/// the drain fixed for it.
///
/// The fourth member of the closed `ShutdownObservation` vocabulary, and the
/// only entry that reaches it. `deadline-expired` is not what a drain that ran
/// out reports — the row above shows a drain running out and still reporting
/// the transition its callback entered under. It is what a callback reports
/// when the aggregate expiry it borrowed is the thing that ended its own join,
/// so it takes a callback that never returns and a server that has committed
/// the drain that lends it that expiry.
///
/// The second row whose callback is genuinely retained past its deadline, and
/// the second reason this file runs under containment. It is ordered ahead of
/// the peer-only row rather than after it so the one outstanding event each of
/// them owes stays one: a capture opened before this row would see two.
async fn drain_deadline_outstanding_callback() {
    let context = "the live drain-expiry row";
    let (_gate, parked) = callback_gate();
    let capture = capture_events(OUTSTANDING_EVENT);
    let server = live_server(Callback::Parked, &parked);
    let _peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    server.handle.shutdown();
    // The entry is fixed when the bridge gives the callback's endpoints up, from
    // whatever the server has committed by then, so the drain has to be
    // committed first. Closing before it would fix the entry on a running
    // server, which is the peer-only row's `none` rather than this row's claim.
    await_live(
        || server.controller.stop.observed().phase == "graceful",
        LIVE_BOUND,
        &format!("{context}: the graceful phase never committed"),
    )
    .await;
    assert_callback_bounds_settlement(
        server,
        &capture,
        &Expected {
            disposition: "outstanding-after-forced-grace",
            shutdown: &["deadline-expired"],
            outstanding_event: true,
        },
        context,
    )
    .await;
}

/// Row: two live upgrades on one server, each with a callback of its own.
///
/// The uniqueness half of Invariant 5, which no single-connection row can see.
/// With one upgrade in play, a record naming the wrong parent and a record
/// naming the right one read identically; with two, a callback claimed by both
/// upgrades leaves one of them owning none, and a callback naming an upgrade
/// nothing transferred leaves both owning none. Each peer closes its own
/// connection, so the two dispositions are two, and the mapping between them
/// and the two transfers has to be one-to-one.
async fn distinct_upgrades_keep_distinct_callbacks() {
    let context = "the live two-upgrade row";
    let (_gate, parked) = callback_gate();
    let capture = capture_events(OUTSTANDING_EVENT);
    let server = live_server(Callback::Cooperative, &parked);
    let mut first = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    let mut second = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    let controller = &server.controller;
    await_live(
        || transferred_upgrades(&controller.connections.observed()).len() == UPGRADES,
        LIVE_BOUND,
        &format!("{context}: both upgrades were never transferred to a connection"),
    )
    .await;
    close_ws_peer(&mut first, context).await;
    close_ws_peer(&mut second, context).await;

    let transferred = transferred_upgrades(&controller.connections.observed());
    await_live(
        || {
            transferred.iter().all(|(connection, upgrade)| {
                controller.connections.observed().contains(
                    ConnectionOwnershipEvent::ConnectionUpgradeSettled {
                        connection: *connection,
                        upgrade: *upgrade,
                    },
                )
            })
        },
        LIVE_BOUND,
        &format!("{context}: both upgrade children never settled under their connections"),
    )
    .await;
    assert_one_callback_per_upgrade(controller, &transferred, context);
    assert!(
        !capture.recorded(&[OUTSTANDING_EVENT]),
        "{context}: a cooperative callback published the outstanding event: {:?}",
        capture.events()
    );

    stop_and_reuse(server, context).await;
}

/// How many upgrades the two-upgrade row puts in play.
const UPGRADES: usize = 2;

/// Assert the settled callbacks and the transferred upgrades map one-to-one.
///
/// Every settled record has to be accounted for by exactly one transfer, and
/// every transfer by exactly one record. Counting only the totals would let two
/// records naming one upgrade pass, and checking only that each record names
/// some transferred upgrade would let the other upgrade own nothing.
fn assert_one_callback_per_upgrade(
    controller: &ScopedRetainedCallback,
    transferred: &[(u64, u64)],
    context: &str,
) {
    assert_eq!(
        transferred.len(),
        UPGRADES,
        "{context}: this row needs {UPGRADES} transferred upgrades: {transferred:?}"
    );
    let settled = settled_callbacks(controller);
    assert_eq!(
        settled.len(),
        UPGRADES,
        "{context}: {UPGRADES} settled callbacks were expected: {settled:?}"
    );
    for owner in transferred {
        let owned = settled
            .iter()
            .filter(|decided| (decided.connection, decided.upgrade) == *owner)
            .count();
        assert_eq!(
            owned, 1,
            "{context}: the upgrade {owner:?} does not own exactly one settled callback: {settled:?}"
        );
    }
}

/// Run one row on a runtime of its own.
///
/// A runtime per row, because a row stops its server: two rows sharing one
/// would have the first row's forced abort deciding the second row's deadlines.
fn row<F>(body: F)
where
    F: AsyncFnOnce(),
{
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("the callback rows need a Tokio runtime");
    runtime.block_on(body());
}

/// Every row, in the child that contains them.
fn run_rows() {
    row(peer_terminal_cooperative_callback);
    row(distinct_upgrades_keep_distinct_callbacks);
    row(cancel_first_transition);
    row(graceful_to_cancel_transition);
    row(timeout_first_transition);
    // Last, because these are the rows that leave the callbacks this child is
    // contained for. Each owes exactly one outstanding event, and a capture is
    // fed everything published after it was opened, so neither may run before a
    // row that counts them.
    row(drain_deadline_outstanding_callback);
    row(peer_terminal_outstanding_callback);
}

// 3.T2 — Invariants 5, 7, and 8: the retained callback is a child of the
// upgrade owner its connection transferred, and neither the upgrade nor the
// connection settles — and no permit comes back — until that callback is joined
// or its bounded outstanding disposition is emitted.
//
// Parentage is read from two independent writers: the connection records the
// transfer, the bridge records the callback under the identity it was built
// with, and the two have to agree. The two-upgrade row is what closes the
// uniqueness half — with one upgrade in play a misattributed callback is
// indistinguishable from a correct one.
//
// Daemon-live over real transports, with process isolation. The peer-only row
// leaves a real blocking callback behind on purpose, so the child reports its
// assertions while that callback is still parked and the parent reaps it. The
// kill is cleanup, never evidence: it happens only after the marker arrives.
#[test]
fn callback_join_and_disposition_matrix_crosses_real_transport_boundaries() {
    contain_in_child(
        "websocket_callback_ownership::callback_join_and_disposition_matrix_crosses_real_transport_boundaries",
        CHILD_MODE,
        ASSERTIONS_COMPLETE,
        CHILD_BOUND,
        run_rows,
    );
}
