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
    AFTER_COMMIT, BEFORE_COMMIT, BEFORE_WRITE, BridgeHold, CLOSE, DIRECTION_DEADLINE,
    DIRECTION_PATH, DirectionTestFixture, EXPIRING_STOP, PARKED_PATH, PayloadWitness,
    abortive_direction_row, assert_broken_pipe, assert_closed_with, assert_received_text,
    assert_within_one_deadline, closed_cause, direction_peer, direction_row,
    fill_outbound_behind_the_writer, park_until_released, payload_bytes, read_ws_text_frame,
    receive_once, returning_direction_row, try_read_ws_frame_raw, witnessed_payload,
    write_ws_close_frame, write_ws_text_frame,
};
use crate::disconnect::fixture::{DRIVER_AND_PRODUCER, with_drain_window};
use crate::disconnect::peer::send;
use crate::disconnect::routes::probe_router;
use crate::disconnect::servers::SyncServer;
use camber::RuntimeError;
use camber::http::mock::{ConnectionOwnershipEvent, WebSocketDirectionEdge, WebSocketTerminalEdge};
use camber::http::{
    Request, Response, Router, WsCloseCause, WsConn, WsReceive, WsReceiver, WsSender,
};
use camber::runtime;

/// The edge that holds the inbound direction with one peer item in hand.
const ARRIVED_EDGE: WebSocketDirectionEdge = WebSocketDirectionEdge::InboundFrameArrived;
/// The same edge, named for the fixture that arms and waits on it.
const ARRIVED: BridgeHold = BridgeHold::Direction(ARRIVED_EDGE);
/// The edge that holds the inbound direction once a peer message is queued.
const QUEUED: BridgeHold = BridgeHold::Direction(WebSocketDirectionEdge::InboundFrameQueued);
/// The edge that holds a graceful bridge before it awaits the peer close.
const CLOSE_AWAIT: BridgeHold = BridgeHold::Terminal(WebSocketTerminalEdge::BeforePeerCloseAwait);

/// The WebSocket text opcode, as it appears on the wire.
const TEXT: u8 = 0x01;
/// The WebSocket binary opcode, which every witnessed payload arrives under.
const BINARY: u8 = 0x02;

/// The frame every terminal row admits and never lets reach the peer on its own.
const HELD: &str = "admitted-before-the-end";
/// The peer message every terminal row leaves in the receive queue.
const QUEUED_INBOUND: &str = "queued-before-the-end";

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
/// The executor is sized from the production count `#[camber::test]` sizes its
/// own from, which is what every other row in this file runs on.
/// `runtime::builder` defaults to the server shape — four workers per core —
/// and a test entry is exactly what that default is documented not to be for: a
/// row here serves one connection at a time, while nine other cases in this
/// binary hold runtimes of their own. The surplus workers buy the rows nothing
/// and cost every checkpoint rendezvous they wait on, because each one is a task
/// wake that has to reach a core. Calling the helper rather than restating it
/// means a retune there moves this row with it.
fn direction_runtime<R, Fut>(shutdown: Duration, row: R)
where
    R: FnOnce() -> Fut,
    Fut: Future<Output = ()>,
{
    runtime::builder()
        .worker_threads(runtime::tokio_default_worker_threads())
        .connection_limit(1)
        .shutdown_timeout(shutdown)
        .run(|| runtime::block_on(row()))
        .expect("the direction runtime completed");
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
        fixture.arm(AFTER_COMMIT);
        write_ws_close_frame(&mut peer);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::PeerClosed);
        assert_closed_send(&sender, WsCloseCause::PeerClosed);
        fixture.release(AFTER_COMMIT);
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
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::PeerDisconnected);
        assert_closed_send(&sender, WsCloseCause::PeerDisconnected);
        fixture.release(AFTER_COMMIT);
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
        fixture.arm(AFTER_COMMIT);
        write_unmasked_frame(&mut peer);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::PeerDisconnected);
        fixture.release(AFTER_COMMIT);
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
        fixture.arm(AFTER_COMMIT);
        drop(peer);
        fixture.release(BEFORE_WRITE);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::PeerDisconnected);
        fixture.release(AFTER_COMMIT);
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
        fixture.arm(AFTER_COMMIT);
        fixture.shutdown_server();
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::ServerShutdown);
        assert_closed_send(&sender, WsCloseCause::ServerShutdown);
        fixture.release(AFTER_COMMIT);
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
        fixture.select_server_cancellation().await;
        assert_closed_send(&sender, WsCloseCause::ServerCancelled);
        fixture.release(AFTER_COMMIT);
        fixture.release(BEFORE_WRITE);
        assert_closed_receive(&mut receiver, WsCloseCause::ServerCancelled);
        expect_transport_end(
            &mut peer,
            "a cancelled server still wrote an admitted frame",
        );
        drop((sender, receiver));
        assert_cancelled_owners_released(&fixture, WsCloseCause::ServerCancelled, 1).await;
    })
    .await;
}

/// A connection with nothing left to read it ends, and drops what it was
/// holding.
async fn receiver_drop_row() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        stage_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(AFTER_COMMIT);
        drop(receiver);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::ReceiverDropped);
        assert_closed_send(&sender, WsCloseCause::ReceiverDropped);
        fixture.release(AFTER_COMMIT);
        fixture.release(BEFORE_WRITE);
        expect_transport_end(
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
        fixture.arm(AFTER_COMMIT);
        drop(sender);
        // The writer is released before the wait: a pump holding a frame is not
        // looking at its queue, so it learns its last sender is gone only once
        // that frame is written — which is what draining before the close means.
        fixture.release(BEFORE_WRITE);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::SendersDropped);
        fixture.release(AFTER_COMMIT);
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

/// Wait until both of this listener's bridges have fixed the join deadline
/// their retained callback answers to.
///
/// The commit is not this barrier. It fixes the cause; the deadline is fixed a
/// step later, where the settlement closes the endpoints a blocked callback
/// wakes on, and it is fixed from whatever the server had committed by then. So
/// a row that asked for its stop between those two steps would give its parked
/// callback the whole drain plus the grace, and end on the aggregate expiry
/// rather than on the claim it is making.
///
/// The second record is the parked route's, because the returning route's is
/// already published by the time a row calls this. Read from the listener's own
/// record rather than inferred from a peer being dropped: dropping a socket is
/// when the peer went away, not when the bridge answered it.
async fn await_second_callback_deadline(fixture: &DirectionTestFixture) {
    crate::common::await_live(
        || deadline_owners(fixture) >= 2,
        DIRECTION_DEADLINE,
        "the parked route's bridge never fixed its callback join deadline",
    )
    .await;
    for record in fixture.callbacks() {
        assert_eq!(
            record.entered, "none",
            "a bridge fixed its callback deadline under a server transition: {record:?}"
        );
    }
}

/// How many connections have a bridge that published a callback join deadline.
///
/// Counted rather than collected: a bridge publishes its record when it fixes
/// the deadline and again when it disposes of the callback, so the records have
/// to be reduced to their distinct connections — but the only caller is a yield
/// loop asking how many there are, and building, sorting and deduping a fresh
/// set on every turn to read its length allocates once per turn for a number.
/// Two bridges are in play, so the pairwise scan is cheaper than the set.
fn deadline_owners(fixture: &DirectionTestFixture) -> usize {
    let records = fixture.callbacks();
    records
        .iter()
        .enumerate()
        .filter(|(seen, record)| {
            !records[..*seen]
                .iter()
                .any(|earlier| earlier.connection == record.connection)
        })
        .count()
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
        fixture.arm(AFTER_COMMIT);
        drop(receiver);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::ReceiverDropped);
        assert_closed_send(&sender, WsCloseCause::ReceiverDropped);
        fixture.release(AFTER_COMMIT);
        drop(sender);
        // The parked connection's peer goes first: its bridge owes that peer a
        // close handshake it would never answer, and this row is not about how
        // long a server waits for one.
        drop(parked_peer);
        // Waited for, not assumed. That bridge's own terminal is what bounds
        // its parked callback: a peer that went away on a running server gives
        // the callback the fixed forced-join grace, while a graceful stop that
        // got there first would give it the whole drain and end this row on the
        // deadline instead of on the claim it is making.
        await_second_callback_deadline(&fixture).await;
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
        fixture.arm(AFTER_COMMIT);
        fixture.shutdown_server();
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_closed_send(&sender, WsCloseCause::ServerShutdown);
        fixture.release(AFTER_COMMIT);
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
        fixture.arm(AFTER_COMMIT);
        let requested = tokio::time::Instant::now();
        fixture.shutdown_server();
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::ServerShutdown);
        fixture.release(AFTER_COMMIT);
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
        fixture.select_server_cancellation().await;
        assert_permit_still_held(&fixture);
        fixture.release(AFTER_COMMIT);
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
        expect_transport_end(&mut peer, "a cancelled server still wrote a queued frame");
        assert_cancelled_owners_released(&fixture, WsCloseCause::ServerCancelled, 2).await;
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
        fixture.arm(AFTER_COMMIT);
        fixture.shutdown_server();
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::ServerShutdown);
        fixture.release(AFTER_COMMIT);
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
        fixture.select_server_cancellation().await;
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

// 4.T2
//
// The rows deliberately hold their bridge after asking the server to stop. A
// forced-abort deadline must stay beyond the fixture's observation bound, or
// runner load can take the bridge away before the proof reads what it
// committed.
#[test]
fn ordered_websocket_causes_cross_public_and_protocol_barriers() {
    direction_runtime(UNREACHED_SHUTDOWN, || async {
        accepted_cancellation_precedes_a_released_peer().await;
        acknowledged_peer_close_stands_under_a_graceful_stop().await;
        acknowledged_peer_close_precedes_a_later_cancellation().await;
        local_receive_loss_precedes_a_later_peer_eof().await;
        whole_connection_release_drains_before_its_normal_close().await;
    });
}

/// A public cancellation that has returned is the earlier fact, even against a
/// peer release this connection had already noticed.
///
/// The peer is released first and the bridge is held short of the commit that
/// would fix it, so the offer in hand is the peer's own. `cancel` then returns,
/// which means the forced phase is committed in the shared stop state before
/// the commit this bridge takes inside it. A bridge that trusted the offer it
/// was holding would report the peer; one ordered against the accepted command
/// reports the server.
///
/// Stated in that order rather than cancelling first because only this order is
/// decidable: with the cancellation published and nothing else offered, the
/// control watch is the only source that can answer, and the row would prove
/// the notification rather than the commit.
async fn accepted_cancellation_precedes_a_released_peer() {
    abortive_direction_row(1, |fixture, peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        let mut witness = hold_witnessed_frame(&fixture, &sender, CANCELLED_TAG).await;
        fixture.arm(BEFORE_COMMIT);
        drop(peer);
        fixture.wait_paused(BEFORE_COMMIT).await;
        fixture.cancel_server();
        fixture.arm(AFTER_COMMIT);
        fixture.release(BEFORE_COMMIT);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::ServerCancelled);
        assert_closed_send(&sender, WsCloseCause::ServerCancelled);
        fixture.release(AFTER_COMMIT);
        fixture.release(BEFORE_WRITE);
        assert_closed_receive(&mut receiver, WsCloseCause::ServerCancelled);
        drop((sender, receiver));
        assert_cancelled_owners_released(&fixture, WsCloseCause::ServerCancelled, 1).await;
        assert_ordered_row_settled(&fixture, WsCloseCause::ServerCancelled).await;
        witness.assert_released("the cancelled bridge").await;
    })
    .await;
}

/// A graceful stop closes admission; it does not decide why an open connection
/// ended.
///
/// The peer's close is decoded and offered, and the bridge is held short of the
/// commit that would fix it. The public stop then returns, so the graceful
/// phase is committed in the shared stop state before the commit this bridge
/// takes inside it. A graceful phase closes admission and lets what is open
/// finish, so the fact this connection was already holding is the one it
/// reports. The echoed close the peer takes afterwards is the protocol
/// acknowledgement that the bridge answered the peer rather than the stop.
async fn acknowledged_peer_close_stands_under_a_graceful_stop() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        let mut witness = hold_witnessed_frame(&fixture, &sender, SHUTDOWN_TAG).await;
        fixture.arm(BEFORE_COMMIT);
        write_ws_close_frame(&mut peer);
        fixture.wait_paused(BEFORE_COMMIT).await;
        fixture.shutdown_server();
        fixture.arm(AFTER_COMMIT);
        fixture.release(BEFORE_COMMIT);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::PeerClosed);
        assert_closed_send(&sender, WsCloseCause::PeerClosed);
        fixture.release(AFTER_COMMIT);
        fixture.release(BEFORE_WRITE);
        expect_peer_close(&mut peer, "the acknowledged peer close was never echoed");
        expect_transport_end(
            &mut peer,
            "the peer-closed bridge kept its transport past the close it echoed",
        );
        assert_closed_receive(&mut receiver, WsCloseCause::PeerClosed);
        drop((sender, receiver, peer));
        assert_stopped_owners_released(&fixture, WsCloseCause::PeerClosed, 1).await;
        assert_ordered_row_settled(&fixture, WsCloseCause::PeerClosed).await;
        witness
            .assert_released("the peer-closed bridge under a graceful stop")
            .await;
    })
    .await;
}

/// A cause the bridge already committed is immutable, and a later cancellation
/// cannot rewrite it.
///
/// This row's peer outlives its barrier, so the frame the committed cause was
/// holding is asked about there too. A `PeerClosed` connection cancels what a
/// successful send admitted, so the peer takes no such frame and the transport
/// ends. Whether the echoed close gets out first is the cancellation's to
/// decide: it is published before the flush this cause owes, so the peer's read
/// accepts either answer and requires the end.
async fn acknowledged_peer_close_precedes_a_later_cancellation() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        let mut witness = hold_witnessed_frame(&fixture, &sender, CLOSED_TAG).await;
        fixture.arm(AFTER_COMMIT);
        write_ws_close_frame(&mut peer);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::PeerClosed);
        fixture.cancel_server();
        assert_closed_send(&sender, WsCloseCause::PeerClosed);
        fixture.release(AFTER_COMMIT);
        fixture.release(BEFORE_WRITE);
        assert_closed_receive(&mut receiver, WsCloseCause::PeerClosed);
        expect_transport_end(
            &mut peer,
            "the committed peer close still wrote the frame it cancelled",
        );
        drop((sender, receiver, peer));
        assert_cancelled_owners_released(&fixture, WsCloseCause::PeerClosed, 1).await;
        assert_ordered_row_settled(&fixture, WsCloseCause::PeerClosed).await;
        witness
            .assert_released("the peer-closed bridge under a later cancel")
            .await;
    })
    .await;
}

/// A local fact offered before the commit is what commits, and a peer that goes
/// away afterwards changes nothing.
///
/// The receive owner leaves while the bridge is held short of its commit, so
/// the offer in hand is the application's own. The peer's transport then ends
/// while that offer is still uncommitted, which is the one arrangement where a
/// bridge that re-weighed its sources would answer differently.
async fn local_receive_loss_precedes_a_later_peer_eof() {
    direction_row(1, |fixture, peer, connection| async move {
        let (sender, receiver) = connection.split();
        let mut witness = hold_witnessed_frame(&fixture, &sender, RECEIVER_TAG).await;
        fixture.arm(BEFORE_COMMIT);
        drop(receiver);
        fixture.wait_paused(BEFORE_COMMIT).await;
        drop(peer);
        fixture.arm(AFTER_COMMIT);
        fixture.release(BEFORE_COMMIT);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::ReceiverDropped);
        assert_closed_send(&sender, WsCloseCause::ReceiverDropped);
        fixture.release(AFTER_COMMIT);
        fixture.release(BEFORE_WRITE);
        drop(sender);
        assert_owners_released(&fixture, WsCloseCause::ReceiverDropped, 1);
        assert_ordered_row_settled(&fixture, WsCloseCause::ReceiverDropped).await;
        witness
            .assert_released("the receive-owner-loss bridge")
            .await;
    })
    .await;
}

/// An application that lets go of the whole connection at once is owed the
/// drain its admitted frames were promised.
///
/// Releasing both halves in one moment is not a receive owner leaving. Nothing
/// is left to send into this connection either, so it ends for the reason the
/// write side still owes something about: the frames already admitted are
/// written, and a normal close follows them. The peer's own reads are the
/// barrier — the payload arrives before the close — so the order this row
/// states is protocol-visible rather than a coordinator turn.
///
/// The writer is released after both halves go, so the pump learns its last
/// sender is gone only once the held frame is written. A receive side that
/// answered anyway would take the cause while that frame was still unwritten,
/// and cancel it.
async fn whole_connection_release_drains_before_its_normal_close() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        let mut witness = hold_witnessed_frame(&fixture, &sender, RELEASED_TAG).await;
        fixture.arm(AFTER_COMMIT);
        drop((sender, receiver));
        fixture.release(BEFORE_WRITE);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::SendersDropped);
        fixture.release(AFTER_COMMIT);
        expect_peer_payload(
            &mut peer,
            RELEASED_TAG,
            "a whole-connection release cancelled an admitted frame",
        );
        expect_peer_close(&mut peer, "a whole-connection release sent no close frame");
        expect_transport_end(
            &mut peer,
            "the released-connection bridge kept its transport past its close",
        );
        drop(peer);
        assert_owners_released(&fixture, WsCloseCause::SendersDropped, 0);
        assert_ordered_row_settled(&fixture, WsCloseCause::SendersDropped).await;
        witness
            .assert_released("the released-connection bridge")
            .await;
    })
    .await;
}

/// The payload tag each ordered row admits its held frame under.
const CANCELLED_TAG: u8 = 0x41;
const SHUTDOWN_TAG: u8 = 0x42;
const CLOSED_TAG: u8 = 0x43;
const RECEIVER_TAG: u8 = 0x44;
const RACE_TAG: u8 = 0x45;
const RELEASED_TAG: u8 = 0x46;

/// How large a payload every ordered row's held frame carries.
///
/// Small, because the claim on it is that the handle is released rather than
/// that the bytes were cheap to move.
const WITNESSED_PAYLOAD: usize = 64;

/// Admit one witnessed shared payload and hold the outbound direction with it.
///
/// A shared payload rather than a text frame, because every row here also owes
/// the claim that no terminal path leaves a payload handle behind — and only an
/// owner-backed payload has a handle to watch.
async fn hold_witnessed_frame(
    fixture: &DirectionTestFixture,
    sender: &WsSender,
    tag: u8,
) -> PayloadWitness {
    let bytes = payload_bytes(WITNESSED_PAYLOAD, tag);
    let (payload, witness) = witnessed_payload(&bytes, "the held ordered-row payload");
    fixture.arm(BEFORE_WRITE);
    sender
        .send_shared_binary(payload)
        .expect("admit the held outbound payload");
    fixture.wait_paused(BEFORE_WRITE).await;
    witness
}

/// Everything one ordered row's committed cause had to settle.
///
/// Stated once because every row in the table owes the same list: both
/// directions settled, the connection permit back, the retained callback either
/// joined or named, and the upgrade recorded as its connection's child and
/// settled there. A row that spelled its own could quietly owe less.
///
/// The queue disposition the cause fixed is owed too, and it is asserted one
/// step earlier — every row reaches this through the release helper its own
/// server allows, and each of those states the admitted frames the cause
/// cancelled or drained. A row whose barrier leaves its peer alive states it
/// twice. The count is one; the frames that peer did or did not take before its
/// transport ended are the other. The permit is read the same way: a
/// still-running
/// server proves it by admitting a second peer, and a stopping or cancelled one
/// admits nothing, so its completion is the barrier and the release the bridge
/// published is what the row reads.
async fn assert_ordered_row_settled(fixture: &DirectionTestFixture, cause: WsCloseCause) {
    assert_settlement_observed(fixture, cause);
    assert_callback_settled(fixture, cause);
    assert_upgrade_settled_under_its_connection(fixture, cause);
}

/// The retained callback ended in the closed disposition vocabulary.
///
/// Either answer is a settlement: a callback that returned was joined, and one
/// that would not return is named against the grace it outlasted. What may not
/// happen is a bridge that published no decision at all.
fn assert_callback_settled(fixture: &DirectionTestFixture, cause: WsCloseCause) {
    let callbacks = fixture.callbacks();
    let decided = callbacks
        .iter()
        .filter_map(|record| record.disposition)
        .collect::<Box<[_]>>();
    assert_eq!(
        decided.len(),
        1,
        "the {cause:?} row published {} callback dispositions rather than one",
        decided.len()
    );
    assert!(
        matches!(decided[0], "completed" | "outstanding-after-forced-grace"),
        "the {cause:?} row named callback disposition {:?}, outside the closed set",
        decided[0]
    );
}

/// The upgrade this row served was its connection's child, and settled there.
fn assert_upgrade_settled_under_its_connection(
    fixture: &DirectionTestFixture,
    cause: WsCloseCause,
) {
    let observed = fixture.ownership();
    let transferred = observed
        .events
        .iter()
        .find_map(|event| match event {
            ConnectionOwnershipEvent::ConnectionUpgradeTransferred {
                connection,
                upgrade,
            } => Some((*connection, *upgrade)),
            _ => None,
        })
        .unwrap_or_else(|| panic!("the {cause:?} row transferred no upgrade to its connection"));
    let (connection, upgrade) = transferred;
    assert!(
        observed.contains(ConnectionOwnershipEvent::ConnectionUpgradeSettled {
            connection,
            upgrade,
        }),
        "the {cause:?} row's upgrade never settled under the connection that took it"
    );
    assert!(
        observed.contains(ConnectionOwnershipEvent::ServerConnectionSettled { connection }),
        "the {cause:?} row's connection never settled under its server"
    );
}

// 4.T3
//
// Not the causal cutover's evidence: 4.T2 owns that. This is the retained
// regression over the case that has no barrier at all, where both results are
// legitimate and the cleanup owed is the same either way.
#[test]
fn unordered_peer_cancel_race_accepts_closed_set_and_releases_every_owner() {
    for _ in 0..causality_iterations() {
        direction_runtime(MATRIX_SHUTDOWN, || async {
            unordered_peer_cancel_iteration().await;
        });
    }
}

/// How many independent iterations the unordered race runs.
///
/// One by default, so an ordinary run pays for one. The indexed flake proof
/// raises it, and a value that is not a positive count is a proof asking for
/// something it cannot get rather than a silent fallback to one.
fn causality_iterations() -> usize {
    match std::env::var("CAMBER_CAUSALITY_ITERATIONS") {
        Err(_) => 1,
        Ok(value) => {
            let requested = value.parse::<usize>().unwrap_or_else(|error| {
                panic!("CAMBER_CAUSALITY_ITERATIONS is not a count: {error}")
            });
            assert!(
                requested > 0,
                "CAMBER_CAUSALITY_ITERATIONS must be a positive repetition count"
            );
            requested
        }
    }
}

/// One complete setup, race, and teardown of the unordered peer/cancel case.
///
/// The peer's reset and the public cancellation are published with nothing
/// ordering them, so either is genuinely capable of committing first. The row
/// accepts exactly the two results that can, and requires the same cleanup for
/// both — which is the whole claim: an unordered race may decide the cause and
/// may not decide what is released.
async fn unordered_peer_cancel_iteration() {
    abortive_direction_row(1, |fixture, peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        let bytes = payload_bytes(WITNESSED_PAYLOAD, RACE_TAG);
        let (payload, mut witness) = witnessed_payload(&bytes, "the raced payload");
        sender
            .send_shared_binary(payload)
            .expect("admit the raced payload");
        drop(peer);
        fixture.cancel_server();
        let completed = fixture.join_server().await;
        assert!(
            matches!(completed, Err(RuntimeError::Cancelled)),
            "a cancelled server completed as {completed:?}"
        );
        let cause = fixture
            .observed()
            .terminal
            .expect("the raced bridge committed no cause");
        assert!(
            matches!(
                cause,
                WsCloseCause::PeerDisconnected | WsCloseCause::ServerCancelled
            ),
            "an unordered peer/cancel race committed {cause:?}, outside its closed result set"
        );
        assert_closed_send(&sender, cause);
        assert_closed_receive(&mut receiver, cause);
        drop((sender, receiver));
        assert_settlement_observed(&fixture, cause);
        assert_callback_settled(&fixture, cause);
        assert_upgrade_settled_under_its_connection(&fixture, cause);
        witness.assert_released("the raced bridge").await;
    })
    .await;
}

/// Deadline escalation cannot rewrite a cause an earlier commit already fixed.
///
/// The cause is read back through the endpoint rather than through the
/// observation the first assertion already took: that observation is a snapshot
/// of a record the bridge writes once, so re-reading it can only answer what it
/// answered before, whatever the escalation did. A send asks production's own
/// terminal state instead. The commit count is the other half — a second cause
/// the record kept out is invisible in the cause alone, and this says the
/// escalation never offered one.
#[test]
fn a_committed_cause_survives_a_later_escalation() {
    direction_runtime(UNREACHED_SHUTDOWN, || async {
        direction_row(1, |fixture, mut peer, connection| async move {
            let (sender, receiver) = connection.split();
            fixture.arm(AFTER_COMMIT);
            fixture.shutdown_server();
            fixture.wait_paused(AFTER_COMMIT).await;
            assert_terminal(&fixture, WsCloseCause::ServerShutdown);
            fixture.cancel_server();
            fixture.release(AFTER_COMMIT);
            write_ws_close_frame(&mut peer);
            drop(receiver);
            // The join is a barrier rather than a claim: everything the
            // escalation could do to this bridge has happened by the time its
            // server completes, so the two assertions below are read after it.
            // Which of the two stops names that completion is the subject of
            // the rows above, not of this one — but a stop that expired would
            // mean the escalation left the bridge behind, and that is this
            // row's business.
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
    });
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

// 4.T1
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
    fixture.arm(AFTER_COMMIT);
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

/// The same owners, for a row whose server was cancelled under it.
///
/// A cancelled server reports its own cancellation, and every owner below it
/// still has to be let go of first: the completion waited on here is reached
/// only once the bridge has settled both directions and given its permit back,
/// so reading those observations after it is reading them in that order.
///
/// The bridge's cause is the caller's to name rather than this helper's. A
/// cancellation that reached a connection with nothing else to report is that
/// connection's cause; one that arrived after the connection had already
/// committed does not rewrite it, and both are rows the same completion barrier
/// serves.
async fn assert_cancelled_owners_released(
    fixture: &DirectionTestFixture,
    cause: WsCloseCause,
    cancelled: usize,
) {
    let completed = fixture.join_server().await;
    assert!(
        matches!(completed, Err(RuntimeError::Cancelled)),
        "a cancelled server completed as {completed:?}"
    );
    assert_released(fixture, cause, cancelled);
    assert_settlement_observed(fixture, cause);
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

/// The one cause this row's bridge committed.
///
/// Named by the cause the row staged rather than by the bridge alone, so a
/// report says which row's barrier failed and not merely that some row's did.
fn assert_terminal(fixture: &DirectionTestFixture, expected: WsCloseCause) {
    assert_eq!(
        fixture.observed().terminal,
        Some(expected),
        "the {expected:?} row fixed another cause"
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

/// Require the next frame one peer takes is the witnessed payload a row
/// admitted, byte for byte.
///
/// The bytes and not only the opcode, because a drain claim is that the frame
/// the application handed over is the frame the peer got — a binary frame of
/// some other length would satisfy the opcode and still lose it.
fn expect_peer_payload(peer: &mut TcpStream, tag: u8, what: &str) {
    let (opcode, payload) = expect_peer_frame(peer, what);
    assert_eq!(opcode, BINARY, "{what}: the peer took opcode {opcode:#x}");
    assert_eq!(
        &*payload,
        &*payload_bytes(WITNESSED_PAYLOAD, tag),
        "{what}: the peer took other bytes"
    );
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

/// Read a peer's transport to its end, and require nothing but closes on the way.
///
/// Two claims at once, because one read answers both. Nothing the row has not
/// already accounted for reached the peer, and the bridge let its transport go
/// afterwards.
///
/// Closes are skipped rather than stopped at. A close is either the
/// acknowledgement the row read for itself, or the one a cause owed with nothing
/// else to say. A bridge that wrote its close and then the frame it was supposed
/// to drop would pass on the first frame alone.
///
/// The end itself has to arrive. A read that expired is a bridge that neither
/// wrote what it owed nor let the transport go — the leak these rows exist to
/// catch — so it fails here rather than reading as the silence it wanted.
fn expect_transport_end(peer: &mut TcpStream, what: &str) {
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
