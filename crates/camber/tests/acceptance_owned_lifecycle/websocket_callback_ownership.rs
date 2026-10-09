//! Daemon-live proof that an async callback is part of its upgrade owner, and
//! that its captured state is gone before anything above it settles.
//!
//! Every row runs a real background server over a real loopback peer and reads
//! only what production published. The callback future captures a drop witness:
//! a value whose `Drop` records what the listener had published at that
//! instant. So the ordering claim is read at the destruction itself rather than
//! inferred afterwards — a witness that saw its permit back, its upgrade
//! settled, or its own disposition published was dropped too late, whatever the
//! row reads once everything has finished.
//!
//! The whole file runs inside one private child process. The cancelled-callback
//! event is a process-wide WARN with no identity on it, so a capture in a shared
//! test binary would count the cancellations of every other row beside it. The
//! child exits once its rows pass: nothing here outlives its bridge any more.

#![cfg(feature = "ws")]

use std::net::SocketAddr;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::mock::{
    ConnectionOwnershipEvent, ScopedRetainedCallback, UpgradeOwnerController, UpgradeOwnerEdge,
    WebSocketCallbackObservation,
};
use camber::http::{Request, Router, ServerHandle, WsCloseCause, WsConn, WsReceiver, WsSender};

use crate::common::{
    CallbackPark, CallbackRelease, DropWitnesses, Observers, RetainedCallbackListener, TEXT,
    TraceCapture, abortive_upgraded_ws_peer, assert_address_reused, assert_callbacks_own,
    assert_cancelled_fields, assert_graceful_close_then_eof, assert_received_text,
    assert_refusal_body_then_eof, await_committed_stop, await_live, bounded_receive, callback_gate,
    capture_events, close_ws_peer, drain_until_closed, expect_async_text, lifecycle_event,
    on_ws_executors, only_event, park_until_released, published_callbacks, read_async_http_head,
    run_in_child, settled_callbacks, settled_count, stall_callback_transport, start_ws_upgrade,
    status_from_raw, transferred_child, transferred_upgrades, upgraded_ws_peer,
    write_async_ws_frame,
};

/// The private mode this file's one child runs under.
const CHILD_MODE: &str = "websocket-callback-ownership";

/// What the child prints once every row has passed.
const ASSERTIONS_COMPLETE: &str = "CALLBACK_SETTLEMENT_ROWS_COMPLETE";

/// How long the parent waits for that marker and the child's exit.
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

/// The one WARN event a callback dropped at its deadline publishes.
const CANCELLED_EVENT: &str = "name=camber.websocket.callback.cancelled";

/// The message a row's peer resumes its callback with.
const RESUME: &str = "resume-the-callback";

/// What one row's callback does with the connection it is given.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Callback {
    /// It answers one message and returns.
    Returns,
    /// It takes one message and returns an error.
    Errors,
    /// Its factory panics before any future exists.
    FactoryPanics,
    /// It takes one message and then panics.
    PanicsAfterSuspension,
    /// It reads until its receive queue closes, then returns.
    Drains,
    /// It suspends on a gate nothing in Camber can open.
    Parked,
    /// It hands both halves to the row and returns.
    Escapes,
    /// It releases both endpoints to the row but stays pending on unrelated work.
    EscapesAndParks,
}

/// What a row's callback needs beyond its connection.
#[derive(Clone)]
struct CallbackParts {
    callback: Callback,
    parked: CallbackPark,
    escaped: tokio::sync::mpsc::Sender<(WsSender, WsReceiver)>,
}

/// A router whose one bridge runs `parts.callback` with a witness captured.
fn callback_router(parts: CallbackParts, witnesses: DropWitnesses) -> Router {
    let mut router = Router::new();
    router.ws(SOCKET_ROUTE, move |_request: &Request, conn: WsConn| {
        let witness = witnesses.enter();
        // The connection and the witness are both the factory's own locals
        // here, so the unwind is what drops them.
        if parts.callback == Callback::FactoryPanics {
            panic!("the callback factory panicked")
        }
        let parts = parts.clone();
        async move {
            let _witness = witness;
            run_callback(parts, conn).await
        }
    });
    router
}

/// The body every non-panicking factory's future runs.
async fn run_callback(parts: CallbackParts, mut conn: WsConn) -> Result<(), RuntimeError> {
    match parts.callback {
        Callback::Returns => {
            let message = conn.recv().await.ok_or(RuntimeError::ChannelClosed)?;
            conn.send(&message).await
        }
        Callback::Errors => {
            conn.recv().await;
            Err(RuntimeError::InvalidArgument(
                "the callback refused its peer's message".into(),
            ))
        }
        Callback::PanicsAfterSuspension => {
            conn.recv().await;
            panic!("the callback panicked after its suspension")
        }
        Callback::Drains => {
            drain_until_closed(&mut conn).await;
            Ok(())
        }
        Callback::Parked => {
            park_until_released(&parts.parked).await;
            Ok(())
        }
        Callback::Escapes => escape(&parts, conn).await,
        Callback::EscapesAndParks => {
            escape(&parts, conn).await?;
            park_until_released(&parts.parked).await;
            Ok(())
        }
        Callback::FactoryPanics => unreachable!("the factory panicked before building a future"),
    }
}

/// Hand both halves of `conn` to the row.
async fn escape(parts: &CallbackParts, conn: WsConn) -> Result<(), RuntimeError> {
    parts
        .escaped
        .send(conn.split())
        .await
        .map_err(|_| RuntimeError::ChannelClosed)
}

/// One row's server: a real listener, an observer over it, and its handle.
struct LiveServer {
    addr: SocketAddr,
    controller: ScopedRetainedCallback,
    handle: ServerHandle,
    witnesses: DropWitnesses,
    escaped: tokio::sync::mpsc::Receiver<(WsSender, WsReceiver)>,
    /// The release end of the gate a parked callback waits on.
    ///
    /// Held for the whole row and dropped with it: nothing a row asks of its
    /// server may be answered by this gate opening.
    _parked: CallbackRelease,
}

async fn live_server(callback: Callback) -> LiveServer {
    let bound = RetainedCallbackListener::bind().await;
    let witnesses = DropWitnesses::new(Observers::of(&bound.controller));
    let (release, parked) = callback_gate();
    let (escaped_tx, escaped) = tokio::sync::mpsc::channel(2);
    let parts = CallbackParts {
        callback,
        parked,
        escaped: escaped_tx,
    };
    let (addr, controller, handle) =
        bound.serve(callback_router(parts, witnesses.clone()), DRAIN_BOUND);
    LiveServer {
        addr,
        controller,
        handle,
        witnesses,
        escaped,
        _parked: release,
    }
}

impl LiveServer {
    /// Take the two halves an escaping callback handed out, bounded.
    async fn escaped_halves(&mut self, context: &str) -> (WsSender, WsReceiver) {
        lifecycle_event(context, self.escaped.recv())
            .await
            .unwrap_or_else(|| panic!("{context}: the callback never handed its halves out"))
    }
}

/// Require that every witness went before anything it must precede was
/// published.
///
/// `earlier` holds, per witness in the order they went, how many upgrade
/// settlements and dispositions other connections may already have published.
/// A single-connection row passes one zero. `permit_open` is whether the permit
/// is still this row's to see: the listener records one flag for every bridge,
/// so a row with a second connection cannot read it per callback.
fn assert_dropped_first(
    witnesses: &DropWitnesses,
    earlier: &[usize],
    permit_open: bool,
    context: &str,
) {
    let dropped = witnesses.dropped();
    assert_eq!(
        dropped.len(),
        earlier.len(),
        "{context}: {} callback captures were expected to be dropped: {dropped:?}",
        earlier.len()
    );
    for (at, earlier) in dropped.iter().zip(earlier) {
        assert_eq!(
            (at.upgrades_settled, at.dispositions),
            (*earlier, *earlier),
            "{context}: a callback's captures outlived its upgrade's settlement or its own disposition: {at:?}"
        );
        assert!(
            !permit_open || !at.permit_released,
            "{context}: the connection permit came back while the callback's captures were alive: {at:?}"
        );
        assert_ne!(
            at.phase, "finished",
            "{context}: the server finished while the callback's captures were alive: {at:?}"
        );
    }
}

/// What one row expects its callback to have settled as.
struct Expected {
    disposition: &'static str,
    /// The closed set of transitions this row's disposition may name.
    ///
    /// One name wherever a barrier fixes it. The escalation row is the one that
    /// has none: a cooperative callback that has already returned completes
    /// before the abort it raced is ever heard, so the drain it entered under
    /// and the cancellation that overtook it are both truthful answers.
    shutdown: &'static [&'static str],
    /// The cause the bridge committed, which no callback outcome may rewrite.
    cause: WsCloseCause,
}

/// Drive one single-connection row's shared assertions from the permit to
/// address reuse.
///
/// Every row ends the same way and differs only in what it asked for before it
/// got here: the callback's captures went before the permit, the upgrade, and
/// its own disposition; the disposition names the right owner and transition;
/// the cause is the bridge's; and the address is bindable again.
async fn assert_settled_before_completion(
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
    assert_eq!(
        server.witnesses.entered(),
        1,
        "{context}: the factory was not entered exactly once"
    );
    assert_dropped_first(&server.witnesses, &[0], true, context);
    let settled = assert_settled_once_as(controller, expected.disposition, context);
    let named = settled[0]
        .shutdown
        .unwrap_or_else(|| panic!("{context}: the disposition named no transition: {settled:?}"));
    assert!(
        expected.shutdown.contains(&named),
        "{context}: the disposition named {named}, outside {:?}",
        expected.shutdown
    );
    let owner = transferred_child(controller, context);
    assert_callbacks_own(&published_callbacks(controller), owner, context);
    await_upgrade_and_connection_settled(controller, owner, context).await;
    assert_eq!(
        controller.terminals.observed().terminal,
        Some(expected.cause),
        "{context}: the callback's outcome rewrote the bridge's cause"
    );
    assert_cancelled_event(capture, expected, named, context);
    stop_and_reuse(server, context).await;
}

/// Require exactly one settled callback on this listener, settled as
/// `disposition`, and hand the settled records back.
fn assert_settled_once_as(
    controller: &ScopedRetainedCallback,
    disposition: &'static str,
    context: &str,
) -> Box<[WebSocketCallbackObservation]> {
    let settled = settled_callbacks(&published_callbacks(controller));
    assert_eq!(
        settled
            .iter()
            .map(|record| record.disposition)
            .collect::<Box<[_]>>(),
        Box::from([Some(disposition)]),
        "{context}: the callback settled as something else: {settled:?}"
    );
    settled
}

/// Wait for one upgrade to settle under its connection, and the connection
/// under its server.
async fn await_upgrade_and_connection_settled(
    controller: &ScopedRetainedCallback,
    (connection, upgrade): (u64, u64),
    context: &str,
) {
    await_live(
        || {
            controller.connections.observed().contains(
                ConnectionOwnershipEvent::ConnectionUpgradeSettled {
                    connection,
                    upgrade,
                },
            )
        },
        LIVE_BOUND,
        &format!("{context}: the upgrade child never settled under its connection"),
    )
    .await;
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
}

/// Require the cancelled event exactly when the row's disposition is
/// `cancelled`, and with that disposition's own fields.
fn assert_cancelled_event(
    capture: &TraceCapture,
    expected: &Expected,
    shutdown: &str,
    context: &str,
) {
    let cancelled = expected.disposition == "cancelled";
    assert_eq!(
        capture.recorded(&[CANCELLED_EVENT]),
        cancelled,
        "{context}: the cancelled event's presence is not what this row owes: {:?}",
        capture.events()
    );
    if cancelled {
        let events = capture.events();
        let event = only_event(&events, CANCELLED_EVENT, context);
        assert_cancelled_fields(event, expected.cause, shutdown, context);
    }
}

/// End one row's server, and require the address it served on back.
///
/// The server is cancelled rather than drained because the claim is already
/// established by the time this runs, and the address is the one fact an
/// out-of-process observer could still check. Whichever stop the row committed
/// first keeps its result, so the join may answer any stop result — but it must
/// answer within the bound, and never with a fault.
async fn stop_and_reuse(server: LiveServer, context: &str) {
    let addr = server.addr;
    server.handle.cancel();
    let stopped = tokio::time::timeout(LIVE_BOUND, server.handle)
        .await
        .unwrap_or_else(|_| panic!("{context}: the cancelled server never joined"));
    assert!(
        matches!(
            stopped,
            Ok(()) | Err(RuntimeError::Cancelled | RuntimeError::Timeout)
        ),
        "{context}: the server joined with a fault rather than a stop result: {stopped:?}"
    );
    assert_address_reused(addr, context).await;
}

/// Resume a callback suspended in its receive.
async fn resume(peer: &mut tokio::net::TcpStream, context: &str) {
    write_async_ws_frame(peer, TEXT, RESUME.as_bytes(), context).await;
}

/// The single-connection row every callback outcome shares: upgrade, resume,
/// let the callback end on its own, and answer the close its connection owes.
///
/// The cause is the facade's last owner going with the callback, whatever the
/// callback returned: an error and a panic are reported, never committed.
async fn callback_outcome_row(callback: Callback, context: &str) {
    let capture = capture_events(CANCELLED_EVENT);
    let server = live_server(callback).await;
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    if callback != Callback::FactoryPanics {
        resume(&mut peer, context).await
    }
    if callback == Callback::Returns {
        expect_async_text(&mut peer, RESUME, context).await
    }
    assert_graceful_close_then_eof(&mut peer, context).await;
    assert_settled_before_completion(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["none"],
            cause: WsCloseCause::SendersDropped,
        },
        context,
    )
    .await;
}

/// Rows: a normal return, a returned error, a factory panic, and a future panic
/// after a real suspension.
async fn callback_outcomes_settle_alike() {
    callback_outcome_row(Callback::Returns, "the live return row").await;
    callback_outcome_row(Callback::Errors, "the live returned-error row").await;
    callback_outcome_row(Callback::FactoryPanics, "the live factory-panic row").await;
    callback_outcome_row(
        Callback::PanicsAfterSuspension,
        "the live suspended-panic row",
    )
    .await;
}

/// Row: the peer closes, and a cooperative callback reads that and returns.
async fn peer_close_row() {
    let context = "the live peer-close row";
    let capture = capture_events(CANCELLED_EVENT);
    let server = live_server(Callback::Drains).await;
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    close_ws_peer(&mut peer, context).await;
    assert_settled_before_completion(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["none"],
            cause: WsCloseCause::PeerClosed,
        },
        context,
    )
    .await;
}

/// Row: the peer's transport is reset, and the callback reads that and returns.
async fn peer_reset_row() {
    let context = "the live peer-reset row";
    let capture = capture_events(CANCELLED_EVENT);
    let server = live_server(Callback::Drains).await;
    let peer = abortive_upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    drop(peer);
    assert_settled_before_completion(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["none"],
            cause: WsCloseCause::PeerDisconnected,
        },
        context,
    )
    .await;
}

/// Row: the callback hands its halves out and parks, and the row drops the
/// receive half behind a stalled write.
///
/// The dropped receiver is a local terminal, and the stalled write keeps the
/// transport's close pending. The parked callback is cancelled in that window,
/// so its captures and its disposition must both precede the permit and the
/// inbound settlement the pending transport still holds.
async fn stalled_local_terminal_row() {
    let context = "the live stalled-local-terminal row";
    let capture = capture_events(CANCELLED_EVENT);
    let mut server = live_server(Callback::EscapesAndParks).await;
    let peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    let (sender, receiver) = server.escaped_halves(context).await;
    stall_callback_transport(&sender, receiver, &server.controller, context).await;
    await_live(
        || server.witnesses.dropped_count() != 0,
        LIVE_BOUND,
        &format!("{context}: the parked callback's captures were never dropped"),
    )
    .await;
    assert_dropped_first(&server.witnesses, &[0], true, context);
    let terminals = server.controller.terminals.observed();
    assert!(
        !terminals.permit_released,
        "{context}: pending transport released its permit"
    );
    assert!(
        !terminals.inbound_settled,
        "{context}: pending close finished early"
    );
    assert_eq!(
        server.controller.stop.observed().phase,
        "running",
        "{context}: the server left its running phase without a stop"
    );
    await_live(
        || settled_count(&published_callbacks(&server.controller)) != 0,
        LIVE_BOUND,
        &format!("{context}: the dropped callback never published its disposition"),
    )
    .await;
    assert_settled_once_as(&server.controller, "cancelled", context);
    drop((sender, peer));
    assert_settled_before_completion(
        server,
        &capture,
        &Expected {
            disposition: "cancelled",
            shutdown: &["none"],
            cause: WsCloseCause::ReceiverDropped,
        },
        context,
    )
    .await;
}

/// Row: a graceful stop the peer answers, and a callback that returns once its
/// receive queue closes.
async fn graceful_completion_row() {
    let context = "the live graceful row";
    let capture = capture_events(CANCELLED_EVENT);
    let server = live_server(Callback::Drains).await;
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    server.handle.shutdown();
    assert_graceful_close_then_eof(&mut peer, context).await;
    assert_settled_before_completion(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["graceful"],
            cause: WsCloseCause::ServerShutdown,
        },
        context,
    )
    .await;
}

/// Row: a graceful stop whose silent peer runs the drain out, under a callback
/// that is still suspended at the expiry.
///
/// The one row whose callback is dropped where it stands. The drain is
/// committed before the bridge closes the callback's endpoints, so the deadline
/// it fixes is the aggregate expiry, and a callback still pending there is the
/// one that ran it out.
async fn silent_peer_at_graceful_expiry_row() {
    let context = "the live drain-expiry row";
    let capture = capture_events(CANCELLED_EVENT);
    let server = live_server(Callback::Parked).await;
    // Held open and deliberately silent: the bridge owes this peer a close
    // handshake, and a peer that never answers is what makes the drain expire.
    let _peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    server.handle.shutdown();
    await_committed_stop(&server.controller.stop, context).await;
    assert_settled_before_completion(
        server,
        &capture,
        &Expected {
            disposition: "cancelled",
            shutdown: &["deadline-expired"],
            cause: WsCloseCause::ServerShutdown,
        },
        context,
    )
    .await;
}

/// Row: a cancellation that reaches the bridge before any other transition.
///
/// The callback is cooperative here on purpose. A cancelled server arms its own
/// forced deadline when it commits, and the callback's opens a moment later at
/// the endpoint close; both round to the same timer tick, so a pending callback
/// on this path would report which owner the scheduler reached first. The
/// parent-abort row below owns the case where the server's deadline wins.
async fn forced_cancellation_row() {
    let context = "the live cancel row";
    let capture = capture_events(CANCELLED_EVENT);
    let server = live_server(Callback::Drains).await;
    let _peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    server.handle.cancel();
    assert_settled_before_completion(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["cancelled"],
            cause: WsCloseCause::ServerCancelled,
        },
        context,
    )
    .await;
}

/// Row: a graceful stop escalated to a cancellation before its drain expires.
async fn graceful_to_cancel_row() {
    let context = "the live graceful-to-cancel row";
    let capture = capture_events(CANCELLED_EVENT);
    let server = live_server(Callback::Drains).await;
    let _peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    server.handle.shutdown();
    // The bridge's own commit, not only the server's: a cancellation that
    // reached the bridge first would be the cause instead of the escalation.
    let terminals = &server.controller.terminals;
    await_live(
        || terminals.observed().terminal.is_some(),
        LIVE_BOUND,
        &format!("{context}: the bridge never committed the graceful stop"),
    )
    .await;
    server.handle.cancel();
    assert_settled_before_completion(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["cancelled", "graceful"],
            cause: WsCloseCause::ServerShutdown,
        },
        context,
    )
    .await;
}

/// Row: the connection that owns a still-pending callback is taken away by its
/// parent.
///
/// The bridge is held where it would begin settling, so its own deadline can
/// never drop the callback. Only the cancelled server's forced deadline can end
/// this connection, and it ends it by aborting the task the callback lives in.
/// That abort is the whole disposition: the callback goes with its bridge, and
/// nothing publishes a settlement it never reached.
async fn parent_abort_row() {
    let context = "the live parent-abort row";
    let capture = capture_events(CANCELLED_EVENT);
    let server = live_server(Callback::Parked).await;
    let _peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    server
        .controller
        .upgrades
        .pause_once(UpgradeOwnerEdge::BeforeCallbackSettle)
        .expect("arm the bridge's settlement edge");
    server.handle.cancel();
    lifecycle_event(
        context,
        server
            .controller
            .upgrades
            .wait_until_paused(UpgradeOwnerEdge::BeforeCallbackSettle),
    )
    .await
    .expect("the bridge reaches its settlement edge");
    let completed = lifecycle_event(context, server.handle.join()).await;
    assert!(
        matches!(completed, Err(RuntimeError::Cancelled)),
        "{context}: a cancelled server completed as {completed:?}"
    );
    assert_dropped_first(&server.witnesses, &[0], false, context);
    let published = published_callbacks(&server.controller);
    assert_eq!(
        settled_count(&published),
        0,
        "{context}: a callback taken away with its bridge published a settlement: {published:?}"
    );
    assert!(
        !capture.recorded(&[CANCELLED_EVENT]),
        "{context}: the parent's abort was reported as a deadline cancellation"
    );
    assert_address_reused(server.addr, context).await;
}

/// Row: two live upgrades on one server, each with a callback of its own.
///
/// The uniqueness half of the ownership claim, which no single-connection row
/// can see. Each peer closes in turn, so each callback's captures must go while
/// only the other's upgrade has settled, and the two settled records map
/// one-to-one onto the two transfers.
async fn distinct_upgrades_row() {
    let context = "the live two-upgrade row";
    let capture = capture_events(CANCELLED_EVENT);
    let server = live_server(Callback::Drains).await;
    let mut first = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    let mut second = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    let controller = &server.controller;
    await_live(
        || transferred_upgrades(&controller.connections.observed()).len() == UPGRADES,
        LIVE_BOUND,
        &format!("{context}: both upgrades were never transferred to a connection"),
    )
    .await;
    let transferred = transferred_upgrades(&controller.connections.observed());
    for (index, peer) in [&mut first, &mut second].into_iter().enumerate() {
        close_ws_peer(peer, context).await;
        await_live(
            || settled_count(&published_callbacks(controller)) > index,
            LIVE_BOUND,
            &format!("{context}: peer {index}'s callback never settled"),
        )
        .await;
    }
    for owner in transferred.iter() {
        await_upgrade_and_connection_settled(controller, *owner, context).await;
    }
    assert_dropped_first(&server.witnesses, &[0, 1], false, context);
    assert_one_callback_per_upgrade(controller, &transferred, context);
    assert!(
        !capture.recorded(&[CANCELLED_EVENT]),
        "{context}: a cooperative callback was cancelled: {:?}",
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
    let settled = settled_callbacks(&published_callbacks(controller));
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

/// Row: a callback that moves both halves into application work and returns.
///
/// Its captures go at its return while the connection lives on in the row's
/// hands. The halves still carry a frame each way, and only letting go of them
/// ends the connection — for the cause that release owes, not one the
/// callback's return committed.
async fn escaped_endpoints_row() {
    let context = "the live escaped-endpoints row";
    let capture = capture_events(CANCELLED_EVENT);
    let mut server = live_server(Callback::Escapes).await;
    let mut peer = upgraded_ws_peer(server.addr, SOCKET_ROUTE, context).await;
    let (sender, mut receiver) = server.escaped_halves(context).await;
    await_live(
        || server.witnesses.dropped_count() == 1,
        LIVE_BOUND,
        &format!("{context}: the returned callback's captures were never dropped"),
    )
    .await;
    assert_eq!(
        server.controller.terminals.observed().terminal,
        None,
        "{context}: the callback's return ended the connection its halves still hold"
    );
    sender
        .send(RESUME)
        .await
        .expect("a returned callback's sender still admits");
    expect_async_text(&mut peer, RESUME, context).await;
    resume(&mut peer, context).await;
    assert_received_text(
        bounded_receive(&mut receiver, context).await,
        RESUME,
        context,
    );
    drop((sender, receiver));
    assert_graceful_close_then_eof(&mut peer, context).await;
    assert_settled_before_completion(
        server,
        &capture,
        &Expected {
            disposition: "completed",
            shutdown: &["none"],
            cause: WsCloseCause::SendersDropped,
        },
        context,
    )
    .await;
}

/// Row: an upgrade the stopping server refuses before it acknowledges it.
///
/// The registration is held short of its acknowledgement until the graceful
/// phase is committed, so the connection answers it with a refusal instead of a
/// `101`. A refused upgrade starts no application code, so the factory is never
/// entered and there is no future to drop.
async fn rejected_registration_row() {
    let context = "the live rejected-registration row";
    let server = live_server(Callback::Drains).await;
    let upgrades = &server.controller.upgrades;
    for edge in [
        UpgradeOwnerEdge::AfterHandoffSubmitted,
        UpgradeOwnerEdge::BeforeTransferAcknowledge,
    ] {
        upgrades
            .pause_once(edge)
            .expect("arm the registration edge");
    }
    let mut pending = start_ws_upgrade(server.addr, SOCKET_ROUTE, context).await;
    hold_unacknowledged(upgrades, context).await;
    server.handle.shutdown();
    await_committed_stop(&server.controller.stop, context).await;
    upgrades
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .expect("release the held registration into the stop");
    let head = read_async_http_head(&mut pending, context).await;
    assert_eq!(
        status_from_raw(&head),
        503,
        "{context}: the stopping server answered {head}"
    );
    assert_refusal_body_then_eof(&mut pending, "service unavailable", context).await;
    assert_eq!(
        server.witnesses.entered(),
        0,
        "{context}: a refused upgrade entered the callback factory"
    );
    assert_eq!(
        server.witnesses.dropped_count(),
        0,
        "{context}: a refused upgrade dropped callback state it should never have built"
    );
    let addr = server.addr;
    let completed = lifecycle_event(context, server.handle.join()).await;
    assert!(
        completed.is_ok(),
        "{context}: the stopping server completed as {completed:?}"
    );
    assert_address_reused(addr, context).await;
}

/// Let the registration reach the channel, then hold it before it is
/// acknowledged.
async fn hold_unacknowledged(upgrades: &UpgradeOwnerController, context: &str) {
    lifecycle_event(
        context,
        upgrades.wait_until_paused(UpgradeOwnerEdge::AfterHandoffSubmitted),
    )
    .await
    .expect("the registration reaches the channel");
    upgrades
        .release(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("release the submitted registration");
    lifecycle_event(
        context,
        upgrades.wait_until_paused(UpgradeOwnerEdge::BeforeTransferAcknowledge),
    )
    .await
    .expect("the registration reaches its acknowledgement edge");
}

/// Every row, in the child that isolates their events, each on runtimes of its
/// own.
///
/// A runtime per row, because a row stops its server: two rows sharing one
/// would have the first row's forced abort deciding the second row's deadlines.
fn run_rows() {
    on_ws_executors(callback_outcomes_settle_alike);
    on_ws_executors(peer_close_row);
    on_ws_executors(peer_reset_row);
    on_ws_executors(stalled_local_terminal_row);
    on_ws_executors(graceful_completion_row);
    on_ws_executors(silent_peer_at_graceful_expiry_row);
    on_ws_executors(forced_cancellation_row);
    on_ws_executors(graceful_to_cancel_row);
    on_ws_executors(distinct_upgrades_row);
    on_ws_executors(escaped_endpoints_row);
    on_ws_executors(rejected_registration_row);
    on_ws_executors(parent_abort_row);
}

// async-first-websockets 2.T6 — Invariants 8 and 11: a callback's captured
// state is dropped before its upgrade settles, its permit comes back, its
// disposition is published, or its server finishes, on every path that ends
// it. A refused upgrade never enters the factory at all.
//
// Parentage is read from two independent writers: the connection records the
// transfer, the bridge records the callback under the identity it was built
// with, and the two have to agree. The two-upgrade row closes the uniqueness
// half — with one upgrade in play a misattributed callback is
// indistinguishable from a correct one.
#[test]
fn async_callback_settlement_drops_state_before_upgrade_completion() {
    run_in_child(
        "websocket_callback_ownership::async_callback_settlement_drops_state_before_upgrade_completion",
        CHILD_MODE,
        ASSERTIONS_COMPLETE,
        CHILD_BOUND,
        run_rows,
    );
}
