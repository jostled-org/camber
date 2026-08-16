//! What a live direct WebSocket does while both of its directions are running,
//! and what it does to every owner when one of them ends.
//!
//! Every row here serves a real route, performs a real upgrade, and frames over
//! a real socket, because the claims are about a transport: that one direction
//! makes progress while the other is stuck, that a successful send has promised
//! only admission, and that no cause leaves a pump, a queue, or a connection
//! permit behind.

#![cfg(feature = "ws")]

use std::future::Future;
use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::common::{
    BEFORE_WRITE, CLOSE, DIRECTION_DEADLINE, DIRECTION_PATH, DirectionTestFixture, EXPIRING_STOP,
    PARKED_PATH, abortive_direction_row, assert_broken_pipe, assert_closed_with,
    assert_received_text, assert_within_one_deadline, closed_cause, direction_peer, direction_row,
    fill_outbound_behind_the_writer, park_until_released, read_ws_text_frame, receive_once,
    returning_direction_row, staged_direction_row, try_read_ws_frame_raw, write_ws_close_frame,
    write_ws_text_frame,
};
use crate::disconnect::fixture::{DRIVER_AND_PRODUCER, with_drain_window};
use crate::disconnect::peer::send;
use crate::disconnect::routes::probe_router;
use crate::disconnect::servers::SyncServer;
use camber::RuntimeError;
use camber::http::mock::LifecycleCheckpoint;
use camber::http::{
    Request, Response, Router, WsCloseCause, WsConn, WsReceive, WsReceiver, WsSender,
};
use camber::runtime;

/// The checkpoint that holds the inbound pump with one peer frame in hand.
const ARRIVED: LifecycleCheckpoint = LifecycleCheckpoint::WebSocketInboundFrameArrived;
/// The checkpoint that holds the inbound pump once a peer message is queued.
const QUEUED: LifecycleCheckpoint = LifecycleCheckpoint::WebSocketInboundFrameQueued;
/// The checkpoint that holds the coordinator before it looks for a cause.
const BEFORE_SELECTION: LifecycleCheckpoint = LifecycleCheckpoint::WebSocketBeforeTerminalSelection;
/// The checkpoint that holds a coordinator turn before it polls any cause.
const BEFORE_TERMINAL_POLL: LifecycleCheckpoint = LifecycleCheckpoint::WebSocketBeforeTerminalPoll;
/// The checkpoint that holds a graceful bridge before it awaits the peer close.
const CLOSE_AWAIT: LifecycleCheckpoint = LifecycleCheckpoint::WebSocketBeforePeerCloseAwait;
/// The checkpoint that holds the coordinator once its cause is fixed.
const SELECTED: LifecycleCheckpoint = LifecycleCheckpoint::WebSocketTerminalSelected;

/// The WebSocket text opcode, as it appears on the wire.
const TEXT: u8 = 0x01;

/// The frame every terminal row admits and never lets reach the peer on its own.
const HELD: &str = "admitted-before-the-end";
/// The peer message every terminal row leaves in the receive queue.
const QUEUED_INBOUND: &str = "queued-before-the-end";

/// What a precedence row arms before its peer connects.
///
/// The coordinator reaches this checkpoint on its way to the connection the
/// callback hands out, so a row holding that connection is already too late to
/// arm it.
const STAGED_TURN: [LifecycleCheckpoint; 1] = [BEFORE_SELECTION];

/// The bound the matrix rows' own runtime shuts down under.
const MATRIX_SHUTDOWN: Duration = Duration::from_secs(5);

/// The bound a row whose server must never reach its deadline runs under.
///
/// Deliberately longer than [`crate::common::DIRECTION_DEADLINE`]: a row that
/// claims a cancellation is answered where the bridge is parked fails by
/// expiring its own bounded join, rather than passing slowly on a deadline that
/// would have answered it anyway.
const UNREACHED_SHUTDOWN: Duration = Duration::from_secs(30);

/// Run one direction row on a server runtime of its own.
///
/// The shutdown bound belongs to the row and not to the file: a row that waits
/// for a deadline to expire and a row that must never reach one need opposite
/// values, and only a runtime of their own can carry them.
///
/// The executor is sized the way `#[camber::test]` sizes its own, which is what
/// every other row in this file runs on. `runtime::builder` defaults to the
/// server shape — four workers per core — and a test entry is exactly what that
/// default is documented not to be for: a row here serves one connection at a
/// time, while nine other cases in this binary hold runtimes of their own. The
/// surplus workers buy the rows nothing and cost every checkpoint rendezvous
/// they wait on, because each one is a task wake that has to reach a core.
fn direction_runtime<R, Fut>(shutdown: Duration, row: R)
where
    R: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    runtime::builder()
        .worker_threads(test_worker_threads())
        .connection_limit(1)
        .shutdown_timeout(shutdown)
        .run(|| runtime::block_on(row()))
        .expect("the direction runtime completed");
}

/// The worker count a test-shaped executor takes: one per core, as Tokio's own
/// default would, and as `#[camber::test]` does.
fn test_worker_threads() -> usize {
    std::thread::available_parallelism().map_or(1, |cores| cores.get())
}

// 2.T1
#[camber::test]
async fn full_inbound_queue_does_not_block_admitted_outbound_frame() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let sender = connection.sender();
        fixture.arm(QUEUED);
        write_ws_text_frame(&mut peer, "fills-the-only-slot");
        fixture.wait_paused(QUEUED).await;
        fixture.release(QUEUED);
        // Nothing consumes this connection's receive queue, so the pump that
        // picks this frame up can never place it: from here the inbound
        // direction is stuck for the rest of the row. The pump is held with that
        // frame in hand, because a count taken before it read would be 1 whether
        // the inbound direction is stuck at a full queue or simply behind.
        fixture.arm(ARRIVED);
        write_ws_text_frame(&mut peer, "held-at-a-full-queue");
        fixture.wait_paused(ARRIVED).await;
        sender
            .send("admitted-outbound")
            .expect("admit one outbound frame");
        expect_peer_text(
            &mut peer,
            "admitted-outbound",
            "inbound queue blocked admitted outbound progress",
        );
        assert_eq!(
            fixture.observed().inbound_admitted,
            1,
            "the second peer frame reached the receive queue, so the inbound direction was never full"
        );
        fixture.release(ARRIVED);
        drop(connection);
    })
    .await;
}

// 2.T2
#[camber::test]
async fn full_outbound_queue_does_not_block_receive_owner() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        fill_outbound_behind_the_writer(&fixture, &sender).await;
        let waiting = sender.clone();
        // The worker reports reaching its send before it makes it, so the
        // unsettled result below is a send that is waiting rather than a thread
        // that has not run.
        let blocked = fixture
            .spawn_entered_worker("outbound-send", move || waiting.send("waits-for-capacity"));
        write_ws_text_frame(&mut peer, "inbound-while-outbound-is-full");
        let received = receiver
            .recv_timeout(DIRECTION_DEADLINE)
            .expect("full outbound queue blocked the receive owner");
        assert_received_text(
            received,
            "inbound-while-outbound-is-full",
            "the receive owner behind a full outbound queue",
        );
        assert!(
            !blocked.settled(),
            "the held send completed before its writer was released"
        );
        fixture.release(BEFORE_WRITE);
        blocked
            .take()
            .expect("the released writer never completed the waiting send");
        drop((sender, receiver));
    })
    .await;
}

// 2.T3, and the merged owner of the direct permit, callback-boundary, graceful,
// and forced rows the component suite used to hold.
#[test]
fn direct_terminal_matrix_fixes_disposition_and_releases_every_owner() {
    direction_runtime(MATRIX_SHUTDOWN, || async {
        peer_close_row().await;
        peer_reset_row().await;
        invalid_frame_row().await;
        outbound_write_failure_row().await;
        graceful_row().await;
        cancelled_row().await;
        receiver_drop_row().await;
        senders_drop_row().await;
    });
}

/// A peer close frame: the cause the peer chose, its own close echoed back, and
/// the messages it sent before it delivered.
async fn peer_close_row() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(SELECTED);
        write_ws_close_frame(&mut peer);
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::PeerClosed);
        assert_closed_send(&sender, WsCloseCause::PeerClosed);
        fixture.release(SELECTED);
        fixture.release(BEFORE_WRITE);
        expect_peer_close(&mut peer, "a peer close was not echoed");
        assert_delivered(&mut receiver, WsCloseCause::PeerClosed);
        drop((sender, receiver));
        assert_owners_released(&fixture, WsCloseCause::PeerClosed, 1);
    })
    .await;
}

/// A peer whose transport is reset: no close is possible, and everything the
/// peer sent before the reset is still owed to the application.
async fn peer_reset_row() {
    abortive_direction_row(1, |fixture, peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_over_abortive_peer(&fixture, peer, &sender).await;
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::PeerDisconnected);
        assert_closed_send(&sender, WsCloseCause::PeerDisconnected);
        fixture.release(SELECTED);
        fixture.release(BEFORE_WRITE);
        assert_delivered(&mut receiver, WsCloseCause::PeerDisconnected);
        drop((sender, receiver));
        assert_owners_released(&fixture, WsCloseCause::PeerDisconnected, 1);
    })
    .await;
}

/// A frame this transport cannot parse is the peer disconnecting, not a message.
async fn invalid_frame_row() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(SELECTED);
        write_unmasked_frame(&mut peer);
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::PeerDisconnected);
        fixture.release(SELECTED);
        fixture.release(BEFORE_WRITE);
        assert_delivered(&mut receiver, WsCloseCause::PeerDisconnected);
        drop((sender, receiver));
        assert_owners_released(&fixture, WsCloseCause::PeerDisconnected, 1);
    })
    .await;
}

/// A write that fails on a transport the peer reset: the production sink
/// reports it, and nothing here injects it.
async fn outbound_write_failure_row() {
    abortive_direction_row(1, |fixture, peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        fixture.arm(BEFORE_WRITE);
        sender.send(HELD).expect("admit the held outbound frame");
        fixture.wait_paused(BEFORE_WRITE).await;
        fixture.arm(SELECTED);
        drop(peer);
        fixture.release(BEFORE_WRITE);
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::PeerDisconnected);
        fixture.release(SELECTED);
        assert_closed_receive(&mut receiver, WsCloseCause::PeerDisconnected);
        drop((sender, receiver));
        assert_write_failure_released(&fixture);
    })
    .await;
}

/// A graceful stop keeps the promise a successful send was given, and closes.
async fn graceful_row() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(SELECTED);
        fixture.shutdown_server();
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::ServerShutdown);
        assert_closed_send(&sender, WsCloseCause::ServerShutdown);
        fixture.release(SELECTED);
        fixture.release(BEFORE_WRITE);
        expect_peer_text(
            &mut peer,
            HELD,
            "a graceful stop cancelled an admitted frame",
        );
        expect_peer_close(&mut peer, "a graceful stop sent no close frame");
        write_ws_close_frame(&mut peer);
        assert_delivered(&mut receiver, WsCloseCause::ServerShutdown);
        drop((sender, receiver));
        assert_stopped_owners_released(&fixture, WsCloseCause::ServerShutdown, 0).await;
    })
    .await;
}

/// A cancelled server owes nothing: no admitted frame, no queued message, and
/// no close.
///
/// The cancellation is settled by the same coordinator every other row asserts
/// against. A server that aborts publishes that abort to its bridges before it
/// forces anything, so this row reads the cause the coordinator committed, the
/// frames its disposition dropped, and the permit it gave back — not the answer
/// a taken-away bridge would have left behind.
async fn cancelled_row() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_admitted_and_queued(&fixture, &mut peer, &sender).await;
        select_server_cancellation(&fixture).await;
        assert_closed_send(&sender, WsCloseCause::ServerCancelled);
        fixture.release(SELECTED);
        fixture.release(BEFORE_WRITE);
        assert_closed_receive(&mut receiver, WsCloseCause::ServerCancelled);
        expect_no_admitted_frame(
            &mut peer,
            "a cancelled server still wrote an admitted frame",
        );
        drop((sender, receiver));
        assert_cancelled_owners_released(&fixture, 1).await;
    })
    .await;
}

/// A connection with nothing left to read it ends, and drops what it was
/// holding.
async fn receiver_drop_row() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        stage_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(SELECTED);
        drop(receiver);
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::ReceiverDropped);
        assert_closed_send(&sender, WsCloseCause::ReceiverDropped);
        fixture.release(SELECTED);
        fixture.release(BEFORE_WRITE);
        expect_no_admitted_frame(
            &mut peer,
            "a dropped receive owner still wrote an admitted frame",
        );
        drop(sender);
        assert_owners_released(&fixture, WsCloseCause::ReceiverDropped, 1);
    })
    .await;
}

/// A connection with nothing left to write it drains what it has, then closes.
async fn senders_drop_row() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(SELECTED);
        drop(sender);
        // The writer is released before the wait: a pump holding a frame is not
        // looking at its queue, so it learns its last sender is gone only once
        // that frame is written — which is what draining before the close means.
        fixture.release(BEFORE_WRITE);
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::SendersDropped);
        fixture.release(SELECTED);
        expect_peer_text(
            &mut peer,
            HELD,
            "a last-sender drop cancelled an admitted frame",
        );
        expect_peer_close(&mut peer, "a last-sender drop sent no close frame");
        assert_delivered(&mut receiver, WsCloseCause::SendersDropped);
        drop(receiver);
        assert_owners_released(&fixture, WsCloseCause::SendersDropped, 0);
    })
    .await;
}

// 2.T4
#[camber::test]
async fn callback_return_with_retained_halves_keeps_bridge_owned() {
    returning_direction_row(1, |fixture, mut peer, mut handoff| async move {
        // A second connection whose callback keeps its own connection and sits
        // in the blocking pool for the whole row. What the owned server claims
        // when it completes is about its bridges, not about this.
        let parked_peer = fixture.connect(PARKED_PATH);
        handoff.wait_parked().await;
        let (sender, mut receiver) = handoff.halves().await;
        handoff.wait_returned().await;
        sender
            .send("after-the-callback-returned")
            .expect("the returned callback closed its connection");
        expect_peer_text(
            &mut peer,
            "after-the-callback-returned",
            "a callback return stopped a retained sender",
        );
        write_ws_text_frame(&mut peer, "into-retained-halves");
        let received = receiver
            .recv_timeout(DIRECTION_DEADLINE)
            .expect("a callback return stopped a retained receiver");
        assert_received_text(
            received,
            "into-retained-halves",
            "the receiver a returned callback left behind",
        );
        fixture.arm(SELECTED);
        drop(receiver);
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::ReceiverDropped);
        assert_closed_send(&sender, WsCloseCause::ReceiverDropped);
        fixture.release(SELECTED);
        drop(sender);
        // The parked connection's peer goes first: its bridge owes that peer a
        // close handshake it would never answer, and this row is not about how
        // long a server waits for one.
        drop(parked_peer);
        fixture.shutdown_server();
        fixture
            .join_server()
            .await
            .expect("the owned server completed");
        let observed = fixture.observed();
        assert!(
            observed.permit_released,
            "the owned server completed without its bridge releasing the connection permit"
        );
        assert!(
            !handoff.parked_exited(),
            "owner completion claimed a still-parked blocking callback had exited"
        );
        // The other half of what that completion did not claim: the callback is
        // still in its frame, and the connection it carried across the
        // completion is over. Its own send is the only place that can be read
        // from, because the callback is the only thing holding it.
        handoff.release_parked();
        assert_broken_pipe(
            handoff.parked_send().await,
            "a callback-side connection retained across owner completion",
        );
    })
    .await;
}

// 2.T5
#[test]
fn graceful_shutdown_drains_closes_and_joins_direction_pumps() {
    direction_runtime(MATRIX_SHUTDOWN, drained_close_row);
    direction_runtime(EXPIRING_STOP, silent_peer_row);
}

/// Everything a graceful stop owes an answering peer: the frames it admitted,
/// the close after them, and both pumps joined before the permit goes back.
async fn drained_close_row() {
    direction_row(2, |fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        fixture.arm(BEFORE_WRITE);
        sender
            .send("first-admitted")
            .expect("admit the first frame");
        fixture.wait_paused(BEFORE_WRITE).await;
        sender
            .send("second-admitted")
            .expect("admit the second frame");
        fixture.arm(SELECTED);
        fixture.shutdown_server();
        fixture.wait_paused(SELECTED).await;
        assert_closed_send(&sender, WsCloseCause::ServerShutdown);
        fixture.release(SELECTED);
        fixture.release(BEFORE_WRITE);
        expect_peer_text(
            &mut peer,
            "first-admitted",
            "the drain lost its first frame",
        );
        expect_peer_text(
            &mut peer,
            "second-admitted",
            "the drain lost its second frame",
        );
        expect_peer_close(&mut peer, "the drain sent no close frame");
        write_ws_close_frame(&mut peer);
        drop((sender, receiver));
        assert_stopped_owners_released(&fixture, WsCloseCause::ServerShutdown, 0).await;
    })
    .await;
}

/// A peer that takes the close a graceful stop sent it and answers nothing.
///
/// From the moment that close is on the wire, the bridge is waiting for one
/// back that is never coming, and it is the only thing left holding this
/// server. What ends it is the server's own graceful deadline expiring into an
/// abort — and this row is about the bridge hearing that abort where it waits.
/// It settles both directions and gives its permit back within the one deadline
/// the stop was given, rather than being taken away by a second one.
async fn silent_peer_row() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        fixture.arm(CLOSE_AWAIT);
        fixture.arm(SELECTED);
        let requested = tokio::time::Instant::now();
        fixture.shutdown_server();
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::ServerShutdown);
        fixture.release(SELECTED);
        expect_peer_close(&mut peer, "a graceful stop sent no close frame");
        fixture.wait_paused(CLOSE_AWAIT).await;
        fixture.release(CLOSE_AWAIT);
        let completed = fixture.join_server().await;
        assert!(
            matches!(completed, Err(RuntimeError::Timeout)),
            "a graceful stop a peer never answered completed as {completed:?}"
        );
        assert_within_one_deadline(requested, "a graceful stop its peer never answered");
        assert_settled_itself(&fixture);
        drop((sender, receiver, peer));
    })
    .await;
}

// 2.T6
#[test]
fn forced_cancellation_wakes_operations_and_releases_permit() {
    direction_runtime(MATRIX_SHUTDOWN, forced_cancellation_row);
    direction_runtime(UNREACHED_SHUTDOWN, cancelled_close_await_row);
    direction_runtime(EXPIRING_STOP, unsettling_bridge_row);
}

/// One send held at a full outbound queue, one receive held on an empty one,
/// and a cancellation that has to wake both.
///
/// The row is written around the coordinator's own two steps. While it is held
/// at its committed cause, neither blocked operation has been woken and the
/// permit is still held; both happen when it settles, and the server completes
/// only after that. So the order this claims — cause, then wake, then pumps,
/// then permit, then completion — is read at production transitions rather than
/// inferred from a finished server.
async fn forced_cancellation_row() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        fill_outbound_behind_the_writer(&fixture, &sender).await;
        let waiting = sender.clone();
        let blocked =
            fixture.spawn_worker("cancelled-send", move || waiting.send("never-admitted"));
        let receiving = fixture.spawn_worker("cancelled-receive", move || receive_once(receiver));
        select_server_cancellation(&fixture).await;
        assert_permit_still_held(&fixture);
        fixture.release(SELECTED);
        assert_eq!(
            closed_cause(blocked.take(), "the blocked send"),
            WsCloseCause::ServerCancelled,
            "a blocked send was not woken with the cancellation"
        );
        assert_closed_with(
            receiving.take().expect("the blocked receive was woken"),
            WsCloseCause::ServerCancelled,
            "the receive a cancellation woke",
        );
        drop(sender);
        expect_no_admitted_frame(&mut peer, "a cancelled server still wrote a queued frame");
        assert_cancelled_owners_released(&fixture, 2).await;
    })
    .await;
}

/// The connection permit is still this bridge's at the moment its cause is
/// fixed.
///
/// The other half of the claim its release makes. Read while the coordinator is
/// held at its committed cause, so the release that follows is observed as
/// something the settlement did rather than something that had already
/// happened — which is what "joins both pumps before it releases the permit"
/// means on a connection nobody gets to reuse afterwards.
///
/// The two blocked operations are deliberately not read here. A worker thread
/// that had not reached its endpoint call yet would be answered by the
/// committed cause instead of waiting for it, so "not yet woken" is a claim
/// about this row's own scheduling rather than about production.
fn assert_permit_still_held(fixture: &DirectionTestFixture) {
    assert!(
        !fixture.observed().permit_released,
        "the connection permit went back before the cancellation was applied"
    );
}

/// A cancellation that arrives while the bridge is already waiting for a close.
///
/// Cancellation is the immediate escape hatch, and a bridge parked on an answer
/// its peer will never send is exactly where it has to reach. This server's own
/// deadline is longer than the bound this row waits under, so a cancellation
/// answered only by that deadline fails here rather than passing late.
async fn cancelled_close_await_row() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        fixture.arm(CLOSE_AWAIT);
        fixture.arm(SELECTED);
        fixture.shutdown_server();
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::ServerShutdown);
        fixture.release(SELECTED);
        // The close reaching the peer is what says the bridge is past its own
        // write. The checkpoint proves it has reached the wait for an answer.
        // The peer sends none.
        expect_peer_close(&mut peer, "a graceful stop sent no close frame");
        fixture.wait_paused(CLOSE_AWAIT).await;
        fixture.cancel_server();
        fixture.release(CLOSE_AWAIT);
        let completed = fixture.join_server().await;
        assert!(
            matches!(completed, Err(RuntimeError::Cancelled)),
            "a server cancelled during a close wait completed as {completed:?}"
        );
        assert_settled_itself(&fixture);
        drop((sender, receiver, peer));
    })
    .await;
}

/// A bridge that cannot answer the abort it was given.
///
/// The other half of what makes sparing a registered bridge safe. This one is
/// held at the cause it committed — a checkpoint no production transition
/// releases — so it never reaches the settlement that would let it hear the
/// cancellation. Nothing but the deadline that abort has carried since it began
/// can end this server, and that deadline still does.
async fn unsettling_bridge_row() {
    direction_row(1, |fixture, peer, connection| async move {
        let (sender, receiver) = connection.split();
        let requested = tokio::time::Instant::now();
        select_server_cancellation(&fixture).await;
        let completed = fixture.join_server().await;
        assert!(
            matches!(completed, Err(RuntimeError::Cancelled)),
            "a server holding a bridge that could not settle completed as {completed:?}"
        );
        assert_within_one_deadline(requested, "a stop holding a bridge that could not settle");
        assert!(
            !fixture.observed().permit_released,
            "a bridge held at its committed cause still published a release, so its own settlement is what ended this server rather than the deadline"
        );
        drop((sender, receiver, peer));
    })
    .await;
}

/// Publish cancellation before the coordinator polls any terminal source.
///
/// Holding only [`SELECTED`] observes a cause after the coordinator chose it;
/// it does not order the server request before that choice. The poll gate makes
/// cancellation ready first, then releases one turn whose documented
/// precedence must select it.
async fn select_server_cancellation(fixture: &DirectionTestFixture) {
    fixture.arm(BEFORE_TERMINAL_POLL);
    fixture.cancel_server();
    fixture.wait_paused(BEFORE_TERMINAL_POLL).await;
    fixture.arm(SELECTED);
    fixture.release(BEFORE_TERMINAL_POLL);
    fixture.wait_paused(SELECTED).await;
    assert_terminal(fixture, WsCloseCause::ServerCancelled);
}

/// The bridge answered the abort itself, rather than being taken away by it.
///
/// Both observations are published by the settlement's own transitions, so a
/// bridge aborted where it was parked leaves neither behind. Together they say
/// the abort reached a bridge that was already waiting on its peer.
fn assert_settled_itself(fixture: &DirectionTestFixture) {
    let observed = fixture.observed();
    assert!(
        observed.inbound_settled,
        "the abort never reached the direction waiting for the peer's close"
    );
    assert!(
        observed.permit_released,
        "the bridge was taken away holding its connection permit rather than giving it back"
    );
}

// 2.T7
// The cancellation rows deliberately hold the coordinator after asking the
// server to abort. Its forced-abort deadline must stay beyond the fixture's
// observation bound, or runner load can take the bridge away before the proof
// reads the cause it selected.
#[test]
fn equal_ready_terminal_events_use_documented_precedence() {
    direction_runtime(UNREACHED_SHUTDOWN, || async {
        cancellation_outranks_peer_close().await;
        shutdown_outranks_peer_close().await;
        shutdown_outranks_last_sender_drop().await;
        peer_close_outranks_receiver_drop().await;
        peer_disconnect_outranks_receiver_drop().await;
        receiver_drop_outranks_last_sender_drop().await;
        a_committed_cause_survives_a_later_escalation().await;
    });
}

/// A cancelled server outranks every other event, including a close the peer
/// already delivered.
async fn cancellation_outranks_peer_close() {
    direction_row(1, |fixture, mut peer, connection| async move {
        stage_peer_close(&fixture, &mut peer).await;
        fixture.arm(SELECTED);
        fixture.cancel_server();
        release_equal_ready_turn(&fixture);
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::ServerCancelled);
        fixture.release(SELECTED);
        drop(connection);
    })
    .await;
}

/// A stop the server asked for outranks a close the peer already delivered.
async fn shutdown_outranks_peer_close() {
    direction_row(1, |fixture, mut peer, connection| async move {
        stage_peer_close(&fixture, &mut peer).await;
        fixture.arm(SELECTED);
        fixture.shutdown_server();
        release_equal_ready_turn(&fixture);
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::ServerShutdown);
        fixture.release(SELECTED);
        drop(connection);
    })
    .await;
}

/// The same stop outranks an application that ran out of senders in that turn.
async fn shutdown_outranks_last_sender_drop() {
    staged_direction_row(1, &STAGED_TURN, |fixture, peer, connection| async move {
        let (sender, receiver) = connection.split();
        stage_one_turn(&fixture).await;
        drop(sender);
        fixture.shutdown_server();
        assert_selected_in_one_turn(&fixture, WsCloseCause::ServerShutdown).await;
        drop((receiver, peer));
    })
    .await;
}

/// What the peer did outranks what the application stopped doing.
async fn peer_close_outranks_receiver_drop() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        stage_peer_close(&fixture, &mut peer).await;
        fixture.arm(SELECTED);
        drop(receiver);
        release_equal_ready_turn(&fixture);
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::PeerClosed);
        fixture.release(SELECTED);
        drop(sender);
    })
    .await;
}

/// A transport that ended outranks an application that stopped reading.
async fn peer_disconnect_outranks_receiver_drop() {
    direction_row(1, |fixture, peer, connection| async move {
        let (sender, receiver) = connection.split();
        stage_peer_disconnect(&fixture, peer).await;
        fixture.arm(SELECTED);
        drop(receiver);
        release_equal_ready_turn(&fixture);
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::PeerDisconnected);
        fixture.release(SELECTED);
        drop(sender);
    })
    .await;
}

/// A connection with nothing reading it is over for a stronger reason than one
/// with nothing left to write.
async fn receiver_drop_outranks_last_sender_drop() {
    staged_direction_row(1, &STAGED_TURN, |fixture, peer, connection| async move {
        stage_one_turn(&fixture).await;
        // Both halves go while the coordinator is held, so neither event can
        // decide a turn of its own before the other exists.
        drop(connection);
        assert_selected_in_one_turn(&fixture, WsCloseCause::ReceiverDropped).await;
        drop(peer);
    })
    .await;
}

/// Deadline escalation cannot rewrite a cause an earlier turn already fixed.
///
/// The cause is read back through the endpoint rather than through the
/// observation the first assertion already took: that observation is a snapshot
/// of a record the bridge writes once, so re-reading it can only answer what it
/// answered before, whatever the escalation did. A send asks production's own
/// terminal state instead. The commit count is the other half — a second cause
/// the record kept out is invisible in the cause alone, and this says the
/// escalation never offered one.
async fn a_committed_cause_survives_a_later_escalation() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        fixture.arm(SELECTED);
        fixture.shutdown_server();
        fixture.wait_paused(SELECTED).await;
        assert_terminal(&fixture, WsCloseCause::ServerShutdown);
        fixture.cancel_server();
        fixture.release(SELECTED);
        write_ws_close_frame(&mut peer);
        drop(receiver);
        // The join is a barrier rather than a claim: everything the escalation
        // could do to this bridge has happened by the time its server completes,
        // so the two assertions below are read after it. Which of the two stops
        // names that completion is the subject of the rows above, not of this
        // one — but a stop that expired would mean the escalation left the
        // bridge behind, and that is this row's business.
        let completed = fixture.join_server().await;
        assert!(
            !matches!(completed, Err(RuntimeError::Timeout)),
            "the escalated stop expired instead of completing: {completed:?}"
        );
        assert_closed_send(&sender, WsCloseCause::ServerShutdown);
        assert_eq!(
            fixture.observed().terminal_commits,
            1,
            "the escalation offered the bridge a second cause"
        );
        drop((sender, peer));
    })
    .await;
}

// 3.T1
#[test]
fn owned_camber_callback_carries_runtime_authority() {
    let (router, handoff) =
        authority_router(carrier_router(probe_router(), CARRIER_PATH), AUTHORITY_PATH);
    let window = with_drain_window(
        None,
        router,
        move |addr| admit_before_admission_closes(addr, handoff),
        DRIVER_AND_PRODUCER,
        |_addr, row| refuse_after_admission_closes(row),
    );

    let observed = window.probed.expect(
        "the drain never counted the callback's admitted child beside the supervisor driver",
    );
    assert!(
        matches!(observed.child, Ok(AUTHORITY_CHILD)),
        "the callback's admitted child answered {:?}",
        observed.child
    );
    assert!(
        matches!(observed.late, Err(RuntimeError::ScopeClosed)),
        "a spawn issued after root admission closed answered {:?}",
        observed.late
    );
    assert!(
        observed.late_never_ran,
        "the refused closure ran anyway after admission closed"
    );
    assert!(
        window.reached_zero,
        "the runtime returned without draining the child its callback admitted"
    );
}

/// Everything the owned-authority row carries from inside the runtime closure
/// into the drain window.
struct AuthorityRow {
    /// The peer whose upgrade put the callback in the blocking pool.
    ///
    /// Held rather than read: the connection it opened is what the row is
    /// about, and closing this socket early would end that connection under the
    /// window.
    #[expect(dead_code, reason = "held so the callback's connection stays up")]
    peer: TcpStream,
    /// The callback's connection, held for the same reason.
    #[expect(dead_code, reason = "held so the callback's connection stays up")]
    connection: WsConn,
    handoff: AuthorityHandoff,
    admitted: camber::JoinHandle<&'static str>,
}

/// Enter the direct callback and admit its child while the root scope is still
/// open.
///
/// Runs inside the runtime closure, so every step here happens on the near side
/// of the close transition: the spawn is issued, taken, and running before
/// anything asks the runtime to stop admitting.
fn admit_before_admission_closes(addr: SocketAddr, handoff: AuthorityHandoff) -> AuthorityRow {
    // The capture site, read before the upgrade. It is what the callback's
    // authority below is carried from, and reading it here is what makes the
    // synchronous row's opposite answer mean something.
    assert_carrier(
        addr,
        CARRIER_HELD,
        "an owned server started inside a Camber runtime had no authority to carry",
    );
    let peer = direction_peer(addr, AUTHORITY_PATH);
    let admitted = handoff.admitted();
    // A refused spawn never runs its closure, so a closure that reports itself
    // running was admitted — by this runtime, which is the only one there is.
    assert!(
        handoff.first().entered(),
        "owned Camber callback lost runtime authority"
    );
    let connection = handoff.connection();
    AuthorityRow {
        peer,
        connection,
        handoff,
        admitted,
    }
}

/// What the drain window observed about a callback that had runtime authority.
struct AuthorityWindow {
    /// What the callback's second `camber::spawn` answered.
    late: Result<&'static str, RuntimeError>,
    /// Whether that refused closure stayed unrun.
    late_never_ran: bool,
    /// What the first, admitted child answered.
    child: Result<&'static str, RuntimeError>,
}

/// Ask the same callback for a second child once root admission has closed.
///
/// The window is the proof of both halves of the contract at once: the drain is
/// holding exactly the supervisor driver and this callback's admitted child, so
/// the child is counted by runtime completion and the callback itself is not.
fn refuse_after_admission_closes(row: AuthorityRow) -> AuthorityWindow {
    row.handoff.proceed();
    let late = row.handoff.late();
    let late_never_ran = row.handoff.second().never_ran();
    row.handoff.first().release_and_finish();
    let child = row.admitted.join();
    AuthorityWindow {
        late,
        late_never_ran,
        child,
    }
}

// 3.T2
#[test]
fn owned_bare_tokio_callback_has_no_camber_runtime() {
    bare_executor().block_on(async {
        let (router, handoff) = authority_router(Router::new(), AUTHORITY_PATH);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the bare Tokio listener");
        let addr = listener
            .local_addr()
            .expect("the bare Tokio listener address");
        let server = camber::http::serve_background(listener, router)
            .expect("owned server requires a Tokio runtime");
        let mut peer = direction_peer(addr, AUTHORITY_PATH);

        let mut connection = assert_callback_admits_nothing(&handoff);
        exchange_authority_frames(&mut peer, &mut connection);

        drop(connection);
        expect_peer_close(
            &mut peer,
            "the bare-Tokio bridge never closed its transport",
        );
        drop(handoff);
        server.shutdown();
        // Bounded like every other join in the file. This server has no Camber
        // runtime over it, so no `shutdown_timeout` governs the wait and an
        // unbounded one would hang the harness rather than fail this row.
        crate::common::lifecycle_event("the bare-Tokio server to complete", server.join())
            .await
            .expect("the bare-Tokio server completed");
    });
}

// 3.T3
#[test]
fn synchronous_serving_carries_one_supervisor_authority() {
    let (router, handoff) =
        authority_router(carrier_router(probe_router(), CARRIER_PATH), AUTHORITY_PATH);
    // Its serve thread runs `serve_listener` inside a Camber runtime of its
    // own, and that runtime now reaches the connection tasks: synchronous
    // serving is the same supervisor the owned entry points use, which carries
    // the runtime it captured into every connection it spawns. The row asserts
    // it at the capture site, so a synchronous path that stopped carrying
    // authority turns this red rather than leaving the callback below to be
    // read as a suppression it never proved.
    let mut server = SyncServer::start(router);
    assert_carrier(
        server.addr(),
        CARRIER_HELD,
        "the synchronous serving path lost the Camber authority its supervisor captured",
    );
    let mut peer = direction_peer(server.addr(), AUTHORITY_PATH);

    // The callback holds the same authority, because one supervisor owns both
    // serving families and there is no detached branch left to lose it.
    let admitted = handoff.admitted();
    assert!(
        handoff.first().entered(),
        "the synchronous callback lost its runtime authority"
    );
    let mut connection = handoff.connection();
    // The exchange's receive is bounded, and a bound needs a clock. This server
    // owns its runtime on another thread, so the case brings a clock of its
    // own.
    bare_executor().block_on(async { exchange_authority_frames(&mut peer, &mut connection) });
    handoff.first().release_and_finish();
    assert_eq!(
        admitted.join().expect("the admitted child never completed"),
        AUTHORITY_CHILD,
        "the synchronous callback's admitted child did not run under the captured runtime"
    );

    drop(connection);
    expect_peer_close(
        &mut peer,
        "the synchronous bridge never closed its transport",
    );
    drop(handoff);
    server.assert_served();
}

/// An executor with no Camber runtime over it.
///
/// Multi-thread, because that is the only flavor Camber's blocking endpoint
/// operations may wait on, and both rows below run one.
fn bare_executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build the row's bare Tokio executor")
}

/// Both of one callback's spawns are refused for want of a runtime, neither
/// closure runs, and the connection it was given is handed back live.
///
/// Shared by the two serving paths that carry no Camber authority. They differ
/// in who owns the server and nothing else, so a second spelling of these four
/// assertions would only be a second thing to keep in step.
fn assert_callback_admits_nothing(handoff: &AuthorityHandoff) -> WsConn {
    let admitted = handoff.admitted().join();
    assert!(
        matches!(admitted, Err(RuntimeError::NoRuntime)),
        "a callback with no Camber runtime admitted a task: {admitted:?}"
    );
    assert!(
        handoff.first().never_ran(),
        "a refused closure ran without a runtime to run under"
    );
    let connection = handoff.connection();
    handoff.proceed();
    let late = handoff.late();
    assert!(
        matches!(late, Err(RuntimeError::NoRuntime)),
        "a callback with no Camber runtime admitted a later task: {late:?}"
    );
    assert!(
        handoff.second().never_ran(),
        "a refused later closure ran without a runtime to run under"
    );
    connection
}

/// The route every runtime-authority row registers.
const AUTHORITY_PATH: &str = "/authority";

/// The value a callback child answers with once it has run to completion.
const AUTHORITY_CHILD: &str = "the callback's child ran";

/// The route a runtime-authority row probes to see what the connection task
/// serving it may admit.
const CARRIER_PATH: &str = "/carrier";

/// What that route answers when its connection task carries serving authority.
const CARRIER_HELD: &str = "carried";

/// What it answers when the connection task carries none.
const CARRIER_NONE: &str = "NoRuntime";

/// Add the route that reports the runtime authority its own connection task
/// carries.
///
/// A callback row can only observe authority from inside the callback, and two
/// independent things produce absence there: a serving path that never carried
/// any, and a bridge that withheld one it had. This route reads the connection
/// task — the site `own_upgrade_bridge` captures from — so a row states which
/// of the two it is looking at rather than assuming one.
fn carrier_router(mut router: Router, path: &str) -> Router {
    router.get(path, |_request: &Request| async {
        Response::text(200, carrier_label(&probe_admission().await))
    });
    router
}

/// What one trivial admission from the calling context answered.
async fn probe_admission() -> Result<(), RuntimeError> {
    use std::future::IntoFuture;

    camber::spawn_async(std::future::ready(()))
        .into_future()
        .await
}

/// Name that answer, so a row that fails reports what the task carried.
fn carrier_label(admitted: &Result<(), RuntimeError>) -> &'static str {
    match admitted {
        Ok(()) => CARRIER_HELD,
        Err(RuntimeError::NoRuntime) => CARRIER_NONE,
        Err(_) => "refused for a reason other than runtime absence",
    }
}

/// Require the serving path's own connection task to carry exactly `expected`.
fn assert_carrier(addr: SocketAddr, expected: &str, subject: &str) {
    let probed = send(addr, "GET", CARRIER_PATH, "the carrier probe");
    assert_eq!(probed.status, 200, "the carrier probe missed its route");
    let carried = String::from_utf8_lossy(&probed.body);
    assert_eq!(
        &*carried, expected,
        "{subject}: its connection task answered {carried}"
    );
}

/// One task a direct callback tried to admit, seen from outside the callback.
///
/// Both spawns a runtime-authority callback issues have this shape, so one type
/// answers the two questions every row asks of them: whether the closure ran at
/// all, and — for a closure that did — whether it was still running when the
/// runtime counted it.
struct SpawnProbe {
    entered: Receiver<()>,
    release: Sender<()>,
    finished: Receiver<()>,
}

impl SpawnProbe {
    /// Whether the closure reported itself running, waiting under the bound for
    /// it to.
    ///
    /// A refused spawn never runs its closure, so an answer here is itself the
    /// admission: the runtime that took the spawn is the one running it. The
    /// answer is a value rather than an assertion because what a missing one
    /// means belongs to the row — this type does not know which runtime, or
    /// which absence of one, its callback was serving under.
    fn entered(&self) -> bool {
        self.entered.recv_timeout(DIRECTION_DEADLINE).is_ok()
    }

    /// Whether the closure has reported running, without waiting for it to.
    ///
    /// Read only after the spawn's refusal is already in hand: a refusal drops
    /// the closure unrun, so the answer is settled rather than raced.
    fn never_ran(&self) -> bool {
        self.entered.try_recv().is_err()
    }

    /// Let a running closure leave, and wait for it to.
    ///
    /// Nothing to release if the closure never ran, which is every row but the
    /// admitted one — so a closed gate is an answer, not a failure.
    fn release(&self) {
        let _ = self.release.send(());
    }

    /// Release the closure and require it to finish.
    fn release_and_finish(&self) {
        self.release();
        self.finished
            .recv_timeout(DIRECTION_DEADLINE)
            .expect("the callback's admitted child never finished");
    }
}

/// The callback's end of one [`SpawnProbe`].
///
/// Every part is clonable because the callback that admits the child is an
/// `Fn`: each admission builds its closure fresh rather than moving one in.
struct ChildParts {
    entered: Sender<()>,
    release: Arc<Mutex<Receiver<()>>>,
    finished: Sender<()>,
}

impl ChildParts {
    fn new() -> (Self, SpawnProbe) {
        let (entered, entered_rx) = std::sync::mpsc::channel();
        let (release, release_rx) = std::sync::mpsc::channel();
        let (finished, finished_rx) = std::sync::mpsc::channel();
        (
            Self {
                entered,
                release: Arc::new(Mutex::new(release_rx)),
                finished,
            },
            SpawnProbe {
                entered: entered_rx,
                release,
                finished: finished_rx,
            },
        )
    }

    /// One admission's closure: report entry, hold, report completion.
    ///
    /// It holds rather than returns, so a runtime that admitted it is still
    /// counting it when that runtime's own completion looks.
    fn body(&self) -> impl FnOnce() -> &'static str + Send + 'static {
        let entered = self.entered.clone();
        let release = Arc::clone(&self.release);
        let finished = self.finished.clone();
        move || {
            let _ = entered.send(());
            park_until_released(&release);
            let _ = finished.send(());
            AUTHORITY_CHILD
        }
    }
}

/// What a direct callback reports about the runtime authority it was given.
///
/// The callback issues one `camber::spawn` before it hands its connection out
/// and a second one when the case says admission has closed. Both are the same
/// shape, so the three serving paths differ only in what their runtime answers.
struct AuthorityHandoff {
    connections: Receiver<WsConn>,
    admitted: Receiver<camber::JoinHandle<&'static str>>,
    proceed: Sender<()>,
    late: Receiver<Result<&'static str, RuntimeError>>,
    first: SpawnProbe,
    second: SpawnProbe,
    /// The other end of the channel the callback parks on.
    ///
    /// Never sent on: it is held for its `Drop`, which is what lets the
    /// callback return once the row is done with its connection.
    #[expect(dead_code, reason = "held for its Drop, which unparks the callback")]
    parked: Sender<()>,
}

impl AuthorityHandoff {
    /// The connection the production callback was given.
    fn connection(&self) -> WsConn {
        self.connections
            .recv_timeout(DIRECTION_DEADLINE)
            .expect("the direct callback never handed out its connection")
    }

    /// The handle the callback's first `camber::spawn` produced.
    fn admitted(&self) -> camber::JoinHandle<&'static str> {
        self.admitted
            .recv_timeout(DIRECTION_DEADLINE)
            .expect("the direct callback never issued its first spawn")
    }

    /// Tell the callback to issue its second spawn.
    fn proceed(&self) {
        self.proceed
            .send(())
            .expect("the direct callback stopped waiting for its second spawn");
    }

    /// What joining that second spawn answered.
    ///
    /// The second child is released first. It is refused on every path this
    /// contract allows and so never runs, but a spawn that was wrongly admitted
    /// would park — and this row must fail on the outcome rather than hang on
    /// it.
    fn late(&self) -> Result<&'static str, RuntimeError> {
        self.second.release();
        self.late
            .recv_timeout(DIRECTION_DEADLINE)
            .expect("the direct callback never reported its second spawn")
    }

    fn first(&self) -> &SpawnProbe {
        &self.first
    }

    fn second(&self) -> &SpawnProbe {
        &self.second
    }
}

/// Add a direct route whose callback asks its own runtime what it may admit.
///
/// Takes the router rather than building one: each serving path needs its
/// owner's readiness route beside this one, and a second definition of that
/// route here would be a second thing to keep in step.
fn authority_router(mut router: Router, path: &str) -> (Router, AuthorityHandoff) {
    let (first_parts, first) = ChildParts::new();
    let (second_parts, second) = ChildParts::new();
    let (connections_tx, connections) = std::sync::mpsc::channel();
    let (admitted_tx, admitted) = std::sync::mpsc::channel();
    let (proceed, proceed_rx) = std::sync::mpsc::channel();
    let (late_tx, late) = std::sync::mpsc::channel();
    let (parked, parked_rx) = std::sync::mpsc::channel();
    let proceed_rx = Mutex::new(proceed_rx);
    let parked_rx = Mutex::new(parked_rx);
    router.ws(path, move |_request: &Request, connection: WsConn| {
        admitted_tx
            .send(camber::spawn(first_parts.body()))
            .map_err(|_| RuntimeError::ChannelClosed)?;
        connections_tx
            .send(connection)
            .map_err(|_| RuntimeError::ChannelClosed)?;
        park_until_released(&proceed_rx);
        late_tx
            .send(camber::spawn(second_parts.body()).join())
            .map_err(|_| RuntimeError::ChannelClosed)?;
        park_until_released(&parked_rx);
        Ok(())
    });
    (
        router,
        AuthorityHandoff {
            connections,
            admitted,
            proceed,
            late,
            first,
            second,
            parked,
        },
    )
}

/// Prove one connection still carries frames in both directions.
///
/// The row that calls this has just asserted what its callback could not
/// admit. That claim is only worth making about a connection that still works,
/// so the transport is exercised rather than assumed.
fn exchange_authority_frames(peer: &mut TcpStream, connection: &mut WsConn) {
    connection
        .send("server-to-peer")
        .expect("send through the live connection");
    assert_eq!(
        &*read_ws_text_frame(peer),
        "server-to-peer",
        "the peer never received the frame the connection sent"
    );
    write_ws_text_frame(peer, "peer-to-server");
    let received = connection
        .recv_timeout(DIRECTION_DEADLINE)
        .expect("receive through the live connection");
    assert_eq!(
        received.as_deref(),
        Some("peer-to-server"),
        "the connection never received the peer's frame"
    );
}

/// Put the peer's close frame in the inbound pump's hand.
///
/// Releasing the parsed frame wakes the coordinator into the poll gate. The
/// helper waits for that pause before returning, so the row can publish its
/// second event and then release exactly one equal-ready turn.
async fn stage_peer_close(fixture: &DirectionTestFixture, peer: &mut TcpStream) {
    fixture.arm(ARRIVED);
    write_ws_close_frame(peer);
    fixture.wait_paused(ARRIVED).await;
    fixture.arm(BEFORE_TERMINAL_POLL);
    fixture.release(ARRIVED);
    fixture.wait_paused(BEFORE_TERMINAL_POLL).await;
}

/// Put the peer's departure in the inbound pump's hand.
///
/// The same staging as [`stage_peer_close`], over the other thing one read can
/// answer with. The peer's socket is gone before the pump is released, so the
/// item the pump is holding is the end of the transport rather than a frame.
async fn stage_peer_disconnect(fixture: &DirectionTestFixture, peer: TcpStream) {
    fixture.arm(ARRIVED);
    drop(peer);
    fixture.wait_paused(ARRIVED).await;
    fixture.arm(BEFORE_TERMINAL_POLL);
    fixture.release(ARRIVED);
    fixture.wait_paused(BEFORE_TERMINAL_POLL).await;
}

/// Release one coordinator poll after the row publishes its competing event.
fn release_equal_ready_turn(fixture: &DirectionTestFixture) {
    fixture.release(BEFORE_TERMINAL_POLL);
}

/// Hold the coordinator before it looks for a cause, so a row can make two
/// terminal events ready for the same turn.
///
/// The coordinator polls every source once it resumes, so both events are
/// answered in one turn and the precedence between them is what decides the
/// cause — not which source a scheduler happened to reach first.
async fn stage_one_turn(fixture: &DirectionTestFixture) {
    fixture.wait_paused(BEFORE_SELECTION).await;
}

/// Let the staged turn run, and read the cause it selected.
async fn assert_selected_in_one_turn(fixture: &DirectionTestFixture, expected: WsCloseCause) {
    fixture.arm(SELECTED);
    fixture.release(BEFORE_SELECTION);
    fixture.wait_paused(SELECTED).await;
    assert_terminal(fixture, expected);
    fixture.release(SELECTED);
}

/// One admitted outbound frame held at the writer, and one peer message already
/// in the receive queue.
///
/// Every terminal row starts here, because these two are exactly what the
/// disposition table decides the fate of: a send that returned success without
/// reaching the peer, and a message that arrived before the connection ended.
async fn stage_admitted_and_queued(
    fixture: &DirectionTestFixture,
    peer: &mut TcpStream,
    sender: &WsSender,
) {
    hold_admitted_frame(fixture, sender).await;
    fixture.arm(QUEUED);
    write_ws_text_frame(peer, QUEUED_INBOUND);
    await_queued_message(fixture).await;
}

/// The same staging over a peer that then resets its transport.
///
/// Only the write differs: a reset needs a Tokio socket, and a Tokio socket
/// writes its frame asynchronously. Everything either side of that is the same
/// staging, so it is the same two calls.
async fn stage_over_abortive_peer(
    fixture: &DirectionTestFixture,
    mut peer: tokio::net::TcpStream,
    sender: &WsSender,
) {
    hold_admitted_frame(fixture, sender).await;
    fixture.arm(QUEUED);
    crate::common::write_async_ws_frame(
        &mut peer,
        TEXT,
        QUEUED_INBOUND.as_bytes(),
        "the abortive peer's queued message",
    )
    .await;
    await_queued_message(fixture).await;
    fixture.arm(SELECTED);
    drop(peer);
}

/// Admit one outbound frame and hold the writer with it in hand.
async fn hold_admitted_frame(fixture: &DirectionTestFixture, sender: &WsSender) {
    fixture.arm(BEFORE_WRITE);
    sender.send(HELD).expect("admit the held outbound frame");
    fixture.wait_paused(BEFORE_WRITE).await;
}

/// Wait until the peer's message is in the receive queue, then let the pump go.
async fn await_queued_message(fixture: &DirectionTestFixture) {
    fixture.wait_paused(QUEUED).await;
    fixture.release(QUEUED);
}

/// Every owner one terminal row's cause had to let go of, on a live server.
///
/// The connection permit is proved by a second handshake: this runtime admits
/// one connection at a time, so a peer that completes its upgrade could only
/// have done so on a permit the ended bridge gave back.
fn assert_owners_released(fixture: &DirectionTestFixture, cause: WsCloseCause, cancelled: usize) {
    drop(fixture.connect(DIRECTION_PATH));
    assert_released(fixture, cause, cancelled);
}

/// The same owners, for a row whose cause was the server stopping.
///
/// A stopping server accepts nothing further, so completion itself is what says
/// the permit went back rather than a second handshake.
async fn assert_stopped_owners_released(
    fixture: &DirectionTestFixture,
    cause: WsCloseCause,
    cancelled: usize,
) {
    fixture
        .join_server()
        .await
        .expect("the owned server completed");
    assert_released(fixture, cause, cancelled);
    assert_settlement_observed(fixture, cause);
}

/// The same owners, for a row whose cause was the server being cancelled.
///
/// A cancelled server reports its own cancellation, and every owner below it
/// still has to be let go of first: the completion waited on here is reached
/// only once the bridge has settled both directions and given its permit back,
/// so reading those observations after it is reading them in that order.
async fn assert_cancelled_owners_released(fixture: &DirectionTestFixture, cancelled: usize) {
    let completed = fixture.join_server().await;
    assert!(
        matches!(completed, Err(RuntimeError::Cancelled)),
        "a cancelled server completed as {completed:?}"
    );
    assert_released(fixture, WsCloseCause::ServerCancelled, cancelled);
    assert_settlement_observed(fixture, WsCloseCause::ServerCancelled);
}

fn assert_released(fixture: &DirectionTestFixture, cause: WsCloseCause, cancelled: usize) {
    let observed = assert_release_state(fixture, cause);
    assert_eq!(
        observed.outbound_cancelled, cancelled,
        "the {cause:?} row cancelled the wrong number of admitted frames"
    );
}

/// A reset may land before or after the sink accepts the sole pending frame.
///
/// Before acceptance, settlement cancels the frame and reports one. After
/// acceptance, the transport owns it and settlement reports zero. Both paths
/// must release every bridge owner, and neither may account for another frame.
fn assert_write_failure_released(fixture: &DirectionTestFixture) {
    let observed = assert_release_state(fixture, WsCloseCause::PeerDisconnected);
    assert!(
        observed.outbound_cancelled <= 1,
        "the failed write cancelled more than its sole admitted frame"
    );
}

fn assert_release_state(
    fixture: &DirectionTestFixture,
    cause: WsCloseCause,
) -> camber::http::mock::WebSocketDirectionObservation {
    let observed = fixture.observed();
    assert_eq!(
        observed.terminal,
        Some(cause),
        "the row fixed another cause"
    );
    observed
}

/// A stopped server has no second handshake to prove its bridge settled.
///
/// Its join is the ownership barrier, and these observations distinguish a
/// coordinator settlement from a task that was taken away while still owning
/// a direction or permit.
fn assert_settlement_observed(fixture: &DirectionTestFixture, cause: WsCloseCause) {
    let observed = fixture.observed();
    assert!(
        observed.outbound_settled,
        "the {cause:?} outbound pump never settled"
    );
    assert!(
        observed.inbound_settled,
        "the {cause:?} inbound pump never settled"
    );
    assert!(
        observed.permit_released,
        "the {cause:?} bridge kept its connection permit"
    );
}

fn assert_terminal(fixture: &DirectionTestFixture, expected: WsCloseCause) {
    assert_eq!(
        fixture.observed().terminal,
        Some(expected),
        "the bridge fixed another cause"
    );
}

/// A send on a connection whose cause is already fixed reports that cause.
fn assert_closed_send(sender: &WsSender, expected: WsCloseCause) {
    assert_eq!(
        closed_cause(sender.send("after-the-end"), "a send past the end"),
        expected,
        "a send past the end reported another cause"
    );
}

/// A delivering cause hands over what was queued before it, then itself.
fn assert_delivered(receiver: &mut WsReceiver, cause: WsCloseCause) {
    assert_received_text(
        receive(receiver, "the queued message"),
        QUEUED_INBOUND,
        "the queued message",
    );
    assert_closed_with(
        receive(receiver, "the terminal cause"),
        cause,
        "the delivery",
    );
}

/// A discarding cause hands over nothing but itself.
fn assert_closed_receive(receiver: &mut WsReceiver, cause: WsCloseCause) {
    assert_closed_with(
        receive(receiver, "the terminal cause"),
        cause,
        "the discarding cause",
    );
}

/// One bounded receive, so no row here waits on a message that is never coming.
///
/// `WsReceiver::recv` has no deadline and these calls run on the row's own task:
/// a bridge that stopped delivering a queued message or a terminal cause would
/// park the row, and the whole harness behind it, instead of failing it.
fn receive(receiver: &mut WsReceiver, what: &str) -> WsReceive {
    receiver
        .recv_timeout(DIRECTION_DEADLINE)
        .unwrap_or_else(|error| panic!("{what} was refused: {error}"))
}

/// Read one text frame from a peer, failing with the row's own claim.
///
/// Every direction peer's read is bounded, so a frame that never comes surfaces
/// as an I/O failure inside the frame reader. Naming that failure beside the
/// row's claim is what lets a report say both what did not happen and what the
/// transport did instead.
fn expect_peer_text(peer: &mut TcpStream, expected: &str, what: &str) {
    let (opcode, payload) = expect_peer_frame(peer, what);
    assert_eq!(opcode, TEXT, "{what}: the peer took opcode {opcode:#x}");
    assert_eq!(&*String::from_utf8_lossy(&payload), expected, "{what}");
}

fn expect_peer_close(peer: &mut TcpStream, what: &str) {
    let (opcode, _) = expect_peer_frame(peer, what);
    assert_eq!(opcode, CLOSE, "{what}");
}

/// One frame a peer is owed, or the row's failure naming the read that failed.
fn expect_peer_frame(peer: &mut TcpStream, what: &str) -> (u8, Box<[u8]>) {
    try_read_ws_frame_raw(peer).unwrap_or_else(|error| {
        panic!(
            "{what}: the peer's read answered {:?}: {error}",
            error.kind()
        )
    })
}

/// Assert the peer never sees the frame a cancelling cause dropped.
///
/// A transport that simply ended is as good an answer as a close frame: both say
/// the admitted frame was not written. The transport is read to that end rather
/// than stopping at the first frame, because a bridge that wrote its close and
/// then the frame it was supposed to drop would pass on the close alone.
///
/// The end itself has to arrive. A read that expired is a bridge that neither
/// wrote the frame nor let the transport go — the leak this row exists to catch
/// — so it fails here rather than reading as the silence it wanted.
fn expect_no_admitted_frame(peer: &mut TcpStream, what: &str) {
    let ended = loop {
        match try_read_ws_frame_raw(peer) {
            Ok((CLOSE, _)) => {}
            Ok((opcode, payload)) => {
                panic!("{what}: the peer took opcode {opcode:#x} with payload {payload:?}")
            }
            Err(error) => break error,
        }
    };
    assert_transport_ended(&ended, what);
}

/// The read that ends a transport, told from the read that ran out of time.
fn assert_transport_ended(error: &std::io::Error, what: &str) {
    match error.kind() {
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset => {}
        kind => {
            panic!("{what}: the peer's transport answered {kind:?} rather than ending: {error}")
        }
    }
}

/// Write a frame no server-role WebSocket may accept.
///
/// A client frame must be masked. An unmasked one is a protocol error the
/// transport reports rather than a message, which is the inbound-error ingress
/// this row is about.
fn write_unmasked_frame(peer: &mut TcpStream) {
    use std::io::Write;
    peer.write_all(&[0x81, 0x03, b'b', b'a', b'd'])
        .expect("write an unmasked client frame");
    peer.flush().expect("flush an unmasked client frame");
}
