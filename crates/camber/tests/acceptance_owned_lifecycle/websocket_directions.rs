//! What a live direct WebSocket does while both of its directions are running,
//! and what it does to every owner when one of them ends.
//!
//! Every row here serves a real route, performs a real upgrade, and frames over
//! a real socket, because the claims are about a transport: that one direction
//! makes progress while the other is stuck, that a successful send has promised
//! only admission, and that no cause leaves a pump, a queue, or a connection
//! permit behind.

#![cfg(feature = "ws")]

use std::net::{SocketAddr, TcpStream};
use std::sync::mpsc::{Receiver, Sender};
use std::time::Duration;

use crate::common::{
    AFTER_COMMIT, BEFORE_COMMIT, BEFORE_WRITE, BINARY, BridgeHold, CLOSE, CallbackPark,
    CallbackRelease, DIRECTION_DEADLINE, DIRECTION_PATH, DirectionTestFixture, EXPIRING_STOP,
    FILLING_TEXT, PARKED_PATH, PayloadWitness, QUEUED, RawFrame, TEXT, abortive_direction_row,
    abortive_direction_row_with_shutdown, assert_closed_with, assert_no_further_payload,
    assert_pending, assert_received_text, assert_within_one_deadline, async_direction_row,
    async_direction_row_with_shutdown, async_returning_direction_row, bounded_receive,
    callback_gate, close_ws_peer, closed_cause, direction_peer, expect_async_close,
    expect_async_text, fill_outbound_behind_the_writer, lifecycle_event, on_ws_executors,
    park_until_released, payload_bytes, read_async_ws_frame_or_eof, receive_once,
    transferred_upgrades, try_read_ws_frame_raw, witnessed_payload, write_async_ws_text_frame,
    write_ws_text_frame,
};
use crate::disconnect::fixture::{DRIVER_AND_PRODUCER, with_drain_window};
use crate::disconnect::peer::send;
use crate::disconnect::routes::probe_router;
use crate::disconnect::servers::SyncServer;
use crate::spawn_probe::{AWAITING_PEER, ChildParts, SpawnProbe, await_peer};
use camber::RuntimeError;
use camber::http::mock::{ConnectionOwnershipEvent, WebSocketDirectionEdge, WebSocketTerminalEdge};
use camber::http::{Request, Response, Router, WsCloseCause, WsConn, WsReceiver, WsSender};

/// The edge that holds the inbound direction with one peer item in hand.
const ARRIVED_EDGE: WebSocketDirectionEdge = WebSocketDirectionEdge::InboundFrameArrived;
/// The same edge, named for the fixture that arms and waits on it.
const ARRIVED: BridgeHold = BridgeHold::Direction(ARRIVED_EDGE);
/// The edge that holds a graceful bridge before it awaits the peer close.
const CLOSE_AWAIT: BridgeHold = BridgeHold::Terminal(WebSocketTerminalEdge::BeforePeerCloseAwait);

/// The frame every terminal row admits and never lets reach the peer on its own.
const HELD: &str = "admitted-before-the-end";
/// The peer message every terminal row leaves in the receive queue.
const QUEUED_INBOUND: &str = "queued-before-the-end";

/// The bound a row whose server must never reach its deadline runs under.
///
/// Deliberately longer than [`crate::common::DIRECTION_DEADLINE`]: a row that
/// claims a cancellation is answered where the bridge is parked fails by
/// expiring its own bounded join, rather than passing slowly on a deadline that
/// would have answered it anyway.
const UNREACHED_SHUTDOWN: Duration = Duration::from_secs(30);

// 2.T1
#[test]
fn full_inbound_queue_does_not_block_admitted_outbound_frame() {
    on_ws_executors(|| async {
        async_direction_row(1, |fixture, mut peer, connection| async move {
            let sender = connection.sender();
            fixture.arm(QUEUED);
            write_async_ws_text_frame(&mut peer, FILLING_TEXT).await;
            fixture.wait_paused(QUEUED).await;
            fixture.release(QUEUED);
            // Nothing consumes this connection's receive queue, so the pump that
            // picks this frame up can never place it: from here the inbound
            // direction is stuck for the rest of the row. The pump is held with that
            // frame in hand, because a count taken before it read would be 1 whether
            // the inbound direction is stuck at a full queue or simply behind.
            fixture.arm(ARRIVED);
            write_async_ws_text_frame(&mut peer, "held-at-a-full-queue").await;
            fixture.wait_paused(ARRIVED).await;
            sender
                .send("admitted-outbound")
                .await
                .expect("admit one outbound frame");
            expect_async_text(
                &mut peer,
                "admitted-outbound",
                "inbound queue blocked admitted outbound progress",
            )
            .await;
            assert_eq!(
                fixture.observed().inbound_admitted,
                1,
                "the second peer frame reached the receive queue, \
                 so the inbound direction was never full"
            );
            fixture.release(ARRIVED);
            drop(connection);
        })
        .await;
    });
}

// 2.T2
#[test]
fn full_outbound_queue_does_not_block_receive_owner() {
    on_ws_executors(|| async {
        async_direction_row(1, |fixture, mut peer, connection| async move {
            let (sender, mut receiver) = connection.split();
            fill_outbound_behind_the_writer(&fixture, &sender).await;
            // Polled once here, in the row's own task, so the pending answer is the
            // send itself declining to finish rather than a task not yet scheduled.
            let mut blocked = std::pin::pin!(sender.send("waits-for-capacity"));
            assert_pending(blocked.as_mut(), "a send into the full outbound queue").await;
            write_async_ws_text_frame(&mut peer, "inbound-while-outbound-is-full").await;
            let what = "the receive owner behind a full outbound queue";
            assert_received_text(
                bounded_receive(&mut receiver, what).await,
                "inbound-while-outbound-is-full",
                what,
            );
            assert_pending(
                blocked.as_mut(),
                "the held send before its writer was released",
            )
            .await;
            fixture.release(BEFORE_WRITE);
            lifecycle_event("the released writer to admit the waiting send", blocked)
                .await
                .expect("the released writer never completed the waiting send");
            // The sender goes with the row's scope, after the send that borrows it.
            drop(receiver);
        })
        .await;
    });
}

// 2.T3, and the merged owner of the direct permit, callback-boundary, graceful,
// and forced rows the component suite used to hold.
#[test]
fn direct_terminal_matrix_fixes_disposition_and_releases_every_owner() {
    on_ws_executors(|| async {
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
    async_direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_async_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(AFTER_COMMIT);
        close_ws_peer(&mut peer, "the closing peer").await;
        release_committed(&fixture, &sender, WsCloseCause::PeerClosed).await;
        expect_async_close(&mut peer, "a peer close was not echoed").await;
        assert_delivered(&mut receiver, WsCloseCause::PeerClosed).await;
        drop((sender, receiver));
        assert_owners_released(&fixture, WsCloseCause::PeerClosed, 1).await;
    })
    .await;
}

/// A peer whose transport is reset: no close is possible, and everything the
/// peer sent before the reset is still owed to the application.
async fn peer_reset_row() {
    abortive_direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_async_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(AFTER_COMMIT);
        drop(peer);
        release_committed(&fixture, &sender, WsCloseCause::PeerDisconnected).await;
        assert_delivered(&mut receiver, WsCloseCause::PeerDisconnected).await;
        drop((sender, receiver));
        assert_owners_released(&fixture, WsCloseCause::PeerDisconnected, 1).await;
    })
    .await;
}

/// A frame this transport cannot parse is the peer disconnecting, not a message.
async fn invalid_frame_row() {
    async_direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_async_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(AFTER_COMMIT);
        write_async_unmasked_frame(&mut peer).await;
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::PeerDisconnected);
        fixture.release(AFTER_COMMIT);
        fixture.release(BEFORE_WRITE);
        assert_delivered(&mut receiver, WsCloseCause::PeerDisconnected).await;
        drop((sender, receiver));
        assert_owners_released(&fixture, WsCloseCause::PeerDisconnected, 1).await;
    })
    .await;
}

/// A write that fails on a transport the peer reset: the production sink
/// reports it, and nothing here injects it.
async fn outbound_write_failure_row() {
    abortive_direction_row(1, |fixture, peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        hold_admitted_frame(&fixture, &sender).await;
        fixture.arm(AFTER_COMMIT);
        drop(peer);
        fixture.release(BEFORE_WRITE);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::PeerDisconnected);
        fixture.release(AFTER_COMMIT);
        assert_closed_receive(&mut receiver, WsCloseCause::PeerDisconnected).await;
        drop((sender, receiver));
        assert_write_failure_released(&fixture);
    })
    .await;
}

/// A graceful stop keeps the promise a successful send was given, and closes.
async fn graceful_row() {
    async_direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_async_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(AFTER_COMMIT);
        fixture.shutdown_server();
        release_committed(&fixture, &sender, WsCloseCause::ServerShutdown).await;
        expect_async_text(
            &mut peer,
            HELD,
            "a graceful stop cancelled an admitted frame",
        )
        .await;
        expect_async_close(&mut peer, "a graceful stop sent no close frame").await;
        close_ws_peer(&mut peer, "the gracefully stopped peer").await;
        assert_delivered(&mut receiver, WsCloseCause::ServerShutdown).await;
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
    async_direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_async_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.select_server_cancellation().await;
        assert_closed_send(&sender, WsCloseCause::ServerCancelled).await;
        fixture.release(AFTER_COMMIT);
        fixture.release(BEFORE_WRITE);
        assert_closed_receive(&mut receiver, WsCloseCause::ServerCancelled).await;
        assert_no_further_payload(
            &mut peer,
            "a cancelled server still wrote an admitted frame",
        )
        .await;
        drop((sender, receiver));
        assert_cancelled_owners_released(&fixture, WsCloseCause::ServerCancelled, 1).await;
    })
    .await;
}

/// A connection with nothing left to read it ends, and drops what it was
/// holding.
async fn receiver_drop_row() {
    async_direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        stage_async_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(AFTER_COMMIT);
        drop(receiver);
        release_committed(&fixture, &sender, WsCloseCause::ReceiverDropped).await;
        assert_no_further_payload(
            &mut peer,
            "a dropped receive owner still wrote an admitted frame",
        )
        .await;
        drop(sender);
        assert_owners_released(&fixture, WsCloseCause::ReceiverDropped, 1).await;
    })
    .await;
}

/// A connection with nothing left to write it drains what it has, then closes.
async fn senders_drop_row() {
    async_direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        stage_async_admitted_and_queued(&fixture, &mut peer, &sender).await;
        fixture.arm(AFTER_COMMIT);
        drop(sender);
        // The writer is released before the wait: a pump holding a frame is not
        // looking at its queue, so it learns its last sender is gone only once
        // that frame is written — which is what draining before the close means.
        fixture.release(BEFORE_WRITE);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_terminal(&fixture, WsCloseCause::SendersDropped);
        fixture.release(AFTER_COMMIT);
        expect_async_text(
            &mut peer,
            HELD,
            "a last-sender drop cancelled an admitted frame",
        )
        .await;
        expect_async_close(&mut peer, "a last-sender drop sent no close frame").await;
        assert_delivered(&mut receiver, WsCloseCause::SendersDropped).await;
        drop(receiver);
        assert_owners_released(&fixture, WsCloseCause::SendersDropped, 0).await;
    })
    .await;
}

/// Wait until both of this listener's bridges have fixed the settlement
/// deadline their callback answers to.
///
/// The commit is not this barrier. It fixes the cause; the deadline is fixed a
/// step later, where the settlement closes the endpoints a pending callback
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
        "the parked route's bridge never fixed its callback settlement deadline",
    )
    .await;
    for record in fixture.callbacks() {
        assert_eq!(
            record.entered, "none",
            "a bridge fixed its callback deadline under a server transition: {record:?}"
        );
    }
}

/// How many connections have a bridge that published a callback settlement
/// deadline.
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
#[test]
fn callback_return_with_retained_halves_keeps_bridge_owned() {
    on_ws_executors(|| async {
        async_returning_direction_row(1, |fixture, mut peer, mut handoff| async move {
            // A second connection whose callback keeps its own connection and stays
            // suspended for the whole row. Its bridge owns that future, so the owned
            // server's completion has to mean the future is gone.
            let parked_peer = fixture.connect_async(PARKED_PATH).await;
            handoff.wait_parked().await;
            let (sender, mut receiver) = handoff.halves().await;
            handoff.wait_returned().await;
            sender
                .send("after-the-callback-returned")
                .await
                .expect("the returned callback closed its connection");
            expect_async_text(
                &mut peer,
                "after-the-callback-returned",
                "a callback return stopped a retained sender",
            )
            .await;
            write_async_ws_text_frame(&mut peer, "into-retained-halves").await;
            let what = "the receiver a returned callback left behind";
            assert_received_text(
                bounded_receive(&mut receiver, what).await,
                "into-retained-halves",
                what,
            );
            fixture.arm(AFTER_COMMIT);
            drop(receiver);
            fixture.wait_paused(AFTER_COMMIT).await;
            assert_terminal(&fixture, WsCloseCause::ReceiverDropped);
            assert_closed_send(&sender, WsCloseCause::ReceiverDropped).await;
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
            // The parked callback never answered anything, and its gate is still
            // held, so nothing but its bridge could have ended it: the settlement
            // deadline dropped the future, and the joined completion is what says
            // that drop has already happened.
            assert!(
                handoff.parked_exited(),
                "owner completion left a still-pending callback future alive"
            );
            assert_callback_dispositions(&fixture, &["cancelled", "completed"]);
        })
        .await;
    });
}

/// Require exactly these callback dispositions, in any order.
///
/// Both bridges of the returning row publish one each: the callback that
/// returned completed, and the one still suspended at its deadline was dropped.
fn assert_callback_dispositions(fixture: &DirectionTestFixture, expected: &[&str]) {
    let mut decided = callback_dispositions(fixture);
    decided.sort_unstable();
    assert_eq!(
        &*decided, expected,
        "the bridges published other callback dispositions"
    );
}

/// Every callback disposition this listener's bridges have published, in
/// publication order.
fn callback_dispositions(fixture: &DirectionTestFixture) -> Box<[&'static str]> {
    fixture
        .callbacks()
        .iter()
        .filter_map(|record| record.disposition)
        .collect()
}

// 2.T5
#[test]
fn graceful_shutdown_drains_closes_and_joins_direction_pumps() {
    on_ws_executors(drained_close_row);
    on_ws_executors(silent_peer_row);
}

/// Everything a graceful stop owes an answering peer: the frames it admitted,
/// the close after them, and both pumps joined before the permit goes back.
async fn drained_close_row() {
    async_direction_row_with_shutdown(
        2,
        UNREACHED_SHUTDOWN,
        |fixture, mut peer, connection| async move {
            let (sender, receiver) = connection.split();
            fixture.arm(BEFORE_WRITE);
            sender
                .send("first-admitted")
                .await
                .expect("admit the first frame");
            fixture.wait_paused(BEFORE_WRITE).await;
            sender
                .send("second-admitted")
                .await
                .expect("admit the second frame");
            fixture.arm(AFTER_COMMIT);
            fixture.shutdown_server();
            fixture.wait_paused(AFTER_COMMIT).await;
            assert_closed_send(&sender, WsCloseCause::ServerShutdown).await;
            fixture.release(AFTER_COMMIT);
            fixture.release(BEFORE_WRITE);
            expect_async_text(
                &mut peer,
                "first-admitted",
                "the drain lost its first frame",
            )
            .await;
            expect_async_text(
                &mut peer,
                "second-admitted",
                "the drain lost its second frame",
            )
            .await;
            expect_async_close(&mut peer, "the drain sent no close frame").await;
            close_ws_peer(&mut peer, "the drained peer").await;
            drop((sender, receiver));
            assert_stopped_owners_released(&fixture, WsCloseCause::ServerShutdown, 0).await;
        },
    )
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
    async_direction_row_with_shutdown(
        1,
        EXPIRING_STOP,
        |fixture, mut peer, connection| async move {
            let (sender, receiver) = connection.split();
            fixture.arm(CLOSE_AWAIT);
            fixture.arm(AFTER_COMMIT);
            let requested = tokio::time::Instant::now();
            fixture.shutdown_server();
            fixture.wait_paused(AFTER_COMMIT).await;
            assert_terminal(&fixture, WsCloseCause::ServerShutdown);
            fixture.release(AFTER_COMMIT);
            expect_async_close(&mut peer, "a graceful stop sent no close frame").await;
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
        },
    )
    .await;
}

// 2.T6
#[test]
fn forced_cancellation_wakes_operations_and_releases_permit() {
    on_ws_executors(forced_cancellation_row);
    on_ws_executors(cancelled_close_await_row);
    on_ws_executors(unsettling_bridge_row);
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
    async_direction_row_with_shutdown(
        1,
        UNREACHED_SHUTDOWN,
        |fixture, mut peer, connection| async move {
            let (sender, receiver) = connection.split();
            fill_outbound_behind_the_writer(&fixture, &sender).await;
            let waiting = sender.clone();
            let blocked = fixture.spawn_worker("cancelled-send", async move {
                waiting.send("never-admitted").await
            });
            let receiving = fixture.spawn_worker("cancelled-receive", receive_once(receiver));
            fixture.select_server_cancellation().await;
            assert_permit_still_held(&fixture);
            fixture.release(AFTER_COMMIT);
            assert_eq!(
                closed_cause(blocked.take().await, "the blocked send"),
                WsCloseCause::ServerCancelled,
                "a blocked send was not woken with the cancellation"
            );
            assert_closed_with(
                receiving
                    .take()
                    .await
                    .expect("the blocked receive was woken"),
                WsCloseCause::ServerCancelled,
                "the receive a cancellation woke",
            );
            drop(sender);
            assert_no_further_payload(&mut peer, "a cancelled server still wrote a queued frame")
                .await;
            assert_cancelled_owners_released(&fixture, WsCloseCause::ServerCancelled, 2).await;
        },
    )
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
/// The two blocked operations are deliberately not read here. A worker task
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
    async_direction_row_with_shutdown(
        1,
        UNREACHED_SHUTDOWN,
        |fixture, mut peer, connection| async move {
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
            expect_async_close(&mut peer, "a graceful stop sent no close frame").await;
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
        },
    )
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
    async_direction_row_with_shutdown(1, EXPIRING_STOP, |fixture, peer, connection| async move {
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
    on_ws_executors(|| async {
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
    abortive_direction_row_with_shutdown(
        1,
        UNREACHED_SHUTDOWN,
        |fixture, peer, connection| async move {
            let (sender, mut receiver) = connection.split();
            let mut witness = hold_witnessed_frame(&fixture, &sender, CANCELLED_TAG).await;
            fixture.arm(BEFORE_COMMIT);
            drop(peer);
            fixture.wait_paused(BEFORE_COMMIT).await;
            fixture.cancel_server();
            fixture.arm(AFTER_COMMIT);
            fixture.release(BEFORE_COMMIT);
            release_committed(&fixture, &sender, WsCloseCause::ServerCancelled).await;
            assert_closed_receive(&mut receiver, WsCloseCause::ServerCancelled).await;
            drop((sender, receiver));
            assert_cancelled_owners_released(&fixture, WsCloseCause::ServerCancelled, 1).await;
            assert_ordered_row_settled(&fixture, WsCloseCause::ServerCancelled);
            witness.assert_released("the cancelled bridge").await;
        },
    )
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
    async_direction_row_with_shutdown(
        1,
        UNREACHED_SHUTDOWN,
        |fixture, mut peer, connection| async move {
            let (sender, mut receiver) = connection.split();
            let mut witness = hold_witnessed_frame(&fixture, &sender, SHUTDOWN_TAG).await;
            fixture.arm(BEFORE_COMMIT);
            close_ws_peer(&mut peer, "the peer closing under a graceful stop").await;
            fixture.wait_paused(BEFORE_COMMIT).await;
            fixture.shutdown_server();
            fixture.arm(AFTER_COMMIT);
            fixture.release(BEFORE_COMMIT);
            release_committed(&fixture, &sender, WsCloseCause::PeerClosed).await;
            expect_async_close(&mut peer, "the acknowledged peer close was never echoed").await;
            assert_no_further_payload(
                &mut peer,
                "the peer-closed bridge kept its transport past the close it echoed",
            )
            .await;
            assert_closed_receive(&mut receiver, WsCloseCause::PeerClosed).await;
            drop((sender, receiver, peer));
            assert_stopped_owners_released(&fixture, WsCloseCause::PeerClosed, 1).await;
            assert_ordered_row_settled(&fixture, WsCloseCause::PeerClosed);
            witness
                .assert_released("the peer-closed bridge under a graceful stop")
                .await;
        },
    )
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
    async_direction_row_with_shutdown(
        1,
        UNREACHED_SHUTDOWN,
        |fixture, mut peer, connection| async move {
            let (sender, mut receiver) = connection.split();
            let mut witness = hold_witnessed_frame(&fixture, &sender, CLOSED_TAG).await;
            fixture.arm(AFTER_COMMIT);
            close_ws_peer(&mut peer, "the peer closing before a cancellation").await;
            fixture.wait_paused(AFTER_COMMIT).await;
            assert_terminal(&fixture, WsCloseCause::PeerClosed);
            fixture.cancel_server();
            assert_closed_send(&sender, WsCloseCause::PeerClosed).await;
            fixture.release(AFTER_COMMIT);
            fixture.release(BEFORE_WRITE);
            assert_closed_receive(&mut receiver, WsCloseCause::PeerClosed).await;
            assert_no_further_payload(
                &mut peer,
                "the committed peer close still wrote the frame it cancelled",
            )
            .await;
            drop((sender, receiver, peer));
            assert_cancelled_owners_released(&fixture, WsCloseCause::PeerClosed, 1).await;
            assert_ordered_row_settled(&fixture, WsCloseCause::PeerClosed);
            witness
                .assert_released("the peer-closed bridge under a later cancel")
                .await;
        },
    )
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
    async_direction_row_with_shutdown(
        1,
        UNREACHED_SHUTDOWN,
        |fixture, peer, connection| async move {
            let (sender, receiver) = connection.split();
            let mut witness = hold_witnessed_frame(&fixture, &sender, RECEIVER_TAG).await;
            fixture.arm(BEFORE_COMMIT);
            drop(receiver);
            fixture.wait_paused(BEFORE_COMMIT).await;
            drop(peer);
            fixture.arm(AFTER_COMMIT);
            fixture.release(BEFORE_COMMIT);
            release_committed(&fixture, &sender, WsCloseCause::ReceiverDropped).await;
            drop(sender);
            assert_owners_released(&fixture, WsCloseCause::ReceiverDropped, 1).await;
            assert_ordered_row_settled(&fixture, WsCloseCause::ReceiverDropped);
            witness
                .assert_released("the receive-owner-loss bridge")
                .await;
        },
    )
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
    async_direction_row_with_shutdown(
        1,
        UNREACHED_SHUTDOWN,
        |fixture, mut peer, connection| async move {
            let (sender, receiver) = connection.split();
            let mut witness = hold_witnessed_frame(&fixture, &sender, RELEASED_TAG).await;
            fixture.arm(AFTER_COMMIT);
            drop((sender, receiver));
            fixture.release(BEFORE_WRITE);
            fixture.wait_paused(AFTER_COMMIT).await;
            assert_terminal(&fixture, WsCloseCause::SendersDropped);
            fixture.release(AFTER_COMMIT);
            expect_async_peer_payload(
                &mut peer,
                RELEASED_TAG,
                "a whole-connection release cancelled an admitted frame",
            )
            .await;
            expect_async_close(&mut peer, "a whole-connection release sent no close frame").await;
            assert_no_further_payload(
                &mut peer,
                "the released-connection bridge kept its transport past its close",
            )
            .await;
            drop(peer);
            assert_owners_released(&fixture, WsCloseCause::SendersDropped, 0).await;
            assert_ordered_row_settled(&fixture, WsCloseCause::SendersDropped);
            witness
                .assert_released("the released-connection bridge")
                .await;
        },
    )
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
        .await
        .expect("admit the held outbound payload");
    fixture.wait_paused(BEFORE_WRITE).await;
    witness
}

/// Everything one ordered row's committed cause had to settle.
///
/// Stated once because every row in the table owes the same list: both
/// directions settled, the connection permit back, the callback either
/// completed or cancelled at its deadline, and the upgrade recorded as its connection's child and
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
fn assert_ordered_row_settled(fixture: &DirectionTestFixture, cause: WsCloseCause) {
    assert_settlement_observed(fixture, cause);
    assert_callback_settled(fixture, cause);
    assert_upgrade_settled_under_its_connection(fixture, cause);
}

/// The callback's settlement ended in the closed disposition vocabulary.
///
/// Either answer is a settlement: a callback that returned completed, and one
/// still pending at its deadline was dropped there. What may not happen is a
/// bridge that published no decision at all.
fn assert_callback_settled(fixture: &DirectionTestFixture, cause: WsCloseCause) {
    let decided = callback_dispositions(fixture);
    assert_eq!(
        decided.len(),
        1,
        "the {cause:?} row published {} callback dispositions rather than one",
        decided.len()
    );
    assert!(
        matches!(decided[0], "completed" | "cancelled"),
        "the {cause:?} row named callback disposition {:?}, outside the closed set",
        decided[0]
    );
}

/// The upgrade this row served was its connection's child, and settled there.
///
/// The row's upgrade is the first one transferred. A row whose server keeps
/// running proves its permit by upgrading a second peer, so a later transfer is
/// that probe rather than a second owner of this row.
fn assert_upgrade_settled_under_its_connection(
    fixture: &DirectionTestFixture,
    cause: WsCloseCause,
) {
    let observed = fixture.ownership();
    let (connection, upgrade) = *transferred_upgrades(&observed)
        .first()
        .unwrap_or_else(|| panic!("the {cause:?} row transferred no upgrade to its connection"));
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
        on_ws_executors(|| async {
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
        Err(std::env::VarError::NotPresent) => 1,
        Err(error @ std::env::VarError::NotUnicode(_)) => {
            panic!("CAMBER_CAUSALITY_ITERATIONS is not a count: {error}")
        }
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
            .await
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
        assert_closed_send(&sender, cause).await;
        assert_closed_receive(&mut receiver, cause).await;
        drop((sender, receiver));
        assert_ordered_row_settled(&fixture, cause);
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
    on_ws_executors(|| async {
        async_direction_row_with_shutdown(
            1,
            UNREACHED_SHUTDOWN,
            |fixture, mut peer, connection| async move {
                let (sender, receiver) = connection.split();
                fixture.arm(AFTER_COMMIT);
                fixture.shutdown_server();
                fixture.wait_paused(AFTER_COMMIT).await;
                assert_terminal(&fixture, WsCloseCause::ServerShutdown);
                fixture.cancel_server();
                fixture.release(AFTER_COMMIT);
                close_ws_peer(&mut peer, "the escalated peer").await;
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
                assert_closed_send(&sender, WsCloseCause::ServerShutdown).await;
                assert_eq!(
                    fixture.observed().terminal_commits,
                    1,
                    "the escalation offered the bridge a second cause"
                );
                drop((sender, peer));
            },
        )
        .await;
    });
}

// 3.T1, revised by async-first-websockets 2.T7
//
// The callback's authority is probed at two sites: the factory that builds its
// future, and the future itself after it has suspended in a receive the peer
// acknowledged. Both admit while root admission is open, and the future is
// refused once it has closed.
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
    /// The peer whose upgrade started the callback, and that resumes it.
    peer: TcpStream,
    handoff: AuthorityHandoff,
    admitted: camber::JoinHandle<&'static str>,
}

/// Admit one child from the factory and one from the suspended future while the
/// root scope is still open, then leave the future suspended again.
///
/// Runs inside the runtime closure, so every step here happens on the near side
/// of the close transition: both spawns are issued and taken, and the second is
/// running, before anything asks the runtime to stop admitting.
fn admit_before_admission_closes(addr: SocketAddr, handoff: AuthorityHandoff) -> AuthorityRow {
    // The capture site, read before the upgrade. It is what the callback's
    // authority below is carried from, and reading it here is what makes the
    // bare row's opposite answer mean something.
    assert_carrier(
        addr,
        CARRIER_HELD,
        "an owned server started inside a Camber runtime had no authority to carry",
    );
    let mut peer = direction_peer(addr, AUTHORITY_PATH);
    assert_factory_admitted(&handoff, "owned Camber callback factory");
    let admitted = resume_into_spawn(&mut peer, &handoff);
    // A refused spawn never runs its closure, so a closure that reports itself
    // running was admitted — by this runtime, which is the only one there is.
    assert!(
        handoff.first().entered(),
        "owned Camber callback lost runtime authority across its suspension"
    );
    // The callback suspends again before the window opens, so the spawn it
    // makes when the probe resumes it is issued from a future that was already
    // waiting when admission closed.
    acknowledged_suspension(&mut peer);
    AuthorityRow {
        peer,
        handoff,
        admitted,
    }
}

/// What the drain window observed about a callback that had runtime authority.
struct AuthorityWindow {
    /// What the callback's late `camber::spawn` answered.
    late: Result<&'static str, RuntimeError>,
    /// Whether that refused closure stayed unrun.
    late_never_ran: bool,
    /// What the child admitted after the first suspension answered.
    child: Result<&'static str, RuntimeError>,
}

/// Resume the same callback once root admission has closed, and ask it for a
/// late child.
///
/// The window is the proof of both halves of the contract at once: the drain is
/// holding exactly the supervisor driver and this callback's admitted child, so
/// the child is counted by runtime completion and the callback itself is not.
fn refuse_after_admission_closes(mut row: AuthorityRow) -> AuthorityWindow {
    write_ws_text_frame(&mut row.peer, LATE_SIGNAL);
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

// 3.T2, revised by async-first-websockets 2.T7
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

        assert_callback_admits_nothing(&mut peer, &handoff);

        // The gate going is the callback's return, and its return drops the
        // connection: the close is the bridge answering that, so the transport
        // the refusals were read across was live to the end.
        drop(handoff);
        expect_peer_close(
            &mut peer,
            "the bare-Tokio bridge never closed its transport",
        );
        server.shutdown();
        // Bounded like every other join in the file. This server has no Camber
        // runtime over it, so no `shutdown_timeout` governs the wait and an
        // unbounded one would hang the harness rather than fail this row.
        crate::common::lifecycle_event("the bare-Tokio server to complete", server.join())
            .await
            .expect("the bare-Tokio server completed");
    });
}

// 4.T1, revised by async-first-websockets 2.T7
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

    // The callback holds the same authority at both sites and across both of
    // its suspensions, because one supervisor owns both serving families and
    // there is no detached branch left to lose it.
    assert_factory_admitted(&handoff, "the synchronous callback factory");
    let admitted = resume_into_spawn(&mut peer, &handoff);
    assert!(
        handoff.first().entered(),
        "the synchronous callback lost its runtime authority across its suspension"
    );
    handoff.first().release_and_finish();
    assert_eq!(
        admitted.join().expect("the admitted child never completed"),
        AUTHORITY_CHILD,
        "the synchronous callback's admitted child did not run under the captured runtime"
    );
    acknowledged_suspension(&mut peer);
    write_ws_text_frame(&mut peer, LATE_SIGNAL);
    assert_eq!(
        handoff.late().expect("the late child never completed"),
        AUTHORITY_CHILD,
        "the synchronous callback lost its authority across its second suspension"
    );

    drop(handoff);
    expect_peer_close(
        &mut peer,
        "the synchronous bridge never closed its transport",
    );
    server.assert_served();
}

/// An executor with no Camber runtime over it.
///
/// Multi-thread, because the row reads its peer on a blocking socket from the
/// thread that drives this executor, and the server it owns has to keep making
/// progress on workers of its own while that read waits.
fn bare_executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build the row's bare Tokio executor")
}

/// Every spawn one callback issues is refused for want of a runtime, and no
/// closure runs.
///
/// The factory's, the future's after its acknowledged suspension, and the
/// future's after a second one: a carrier that reached only one of those sites
/// would be refused at the others for some other reason, and this says the
/// reason is the same at all three.
fn assert_callback_admits_nothing(peer: &mut TcpStream, handoff: &AuthorityHandoff) {
    let built = handoff.built();
    assert!(
        matches!(built, Err(RuntimeError::NoRuntime)),
        "a callback factory with no Camber runtime admitted a task: {built:?}"
    );
    assert!(
        handoff.factory().never_ran(),
        "a refused factory closure ran without a runtime to run under"
    );
    let admitted = resume_into_spawn(peer, handoff).join();
    assert!(
        matches!(admitted, Err(RuntimeError::NoRuntime)),
        "a suspended callback with no Camber runtime admitted a task: {admitted:?}"
    );
    assert!(
        handoff.first().never_ran(),
        "a refused closure ran without a runtime to run under"
    );
    acknowledged_suspension(peer);
    write_ws_text_frame(peer, LATE_SIGNAL);
    let late = handoff.late();
    assert!(
        matches!(late, Err(RuntimeError::NoRuntime)),
        "a callback with no Camber runtime admitted a later task: {late:?}"
    );
    assert!(
        handoff.second().never_ran(),
        "a refused later closure ran without a runtime to run under"
    );
}

/// Require the factory's spawn to have been admitted and to finish.
fn assert_factory_admitted(handoff: &AuthorityHandoff, subject: &str) {
    assert!(
        handoff.factory().entered(),
        "{subject} lost runtime authority"
    );
    handoff.factory().release_and_finish();
    let built = handoff.built();
    assert!(
        matches!(built, Ok(AUTHORITY_CHILD)),
        "{subject}'s child did not run under the serving runtime: {built:?}"
    );
}

/// Take the callback's acknowledgement, resume it, and take the handle its
/// spawn produced.
fn resume_into_spawn(
    peer: &mut TcpStream,
    handoff: &AuthorityHandoff,
) -> camber::JoinHandle<&'static str> {
    acknowledged_suspension(peer);
    write_ws_text_frame(peer, ADMIT_SIGNAL);
    handoff.admitted()
}

/// Read the frame a callback sends just before it waits for the peer.
///
/// The callback admits the frame and then polls its receive in the same turn,
/// before the bridge beside it can write anything. A frame the peer has read is
/// therefore a callback already suspended on an empty receive queue, and the
/// spawn it makes once resumed is made across that suspension.
fn acknowledged_suspension(peer: &mut TcpStream) {
    expect_peer_text(
        peer,
        AWAITING_PEER,
        "the callback never acknowledged its suspension",
    );
}

/// The route every runtime-authority row registers.
const AUTHORITY_PATH: &str = "/authority";

/// The value a callback child answers with once it has run to completion.
const AUTHORITY_CHILD: &str = "the callback's child ran";

/// What the peer answers to resume the callback into its admitted spawn.
const ADMIT_SIGNAL: &str = "admit";

/// What the peer answers to resume the callback into its late spawn.
const LATE_SIGNAL: &str = "late";

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

/// What a direct callback reports about the runtime authority it was given.
///
/// The callback issues one `camber::spawn` from its factory, one from its
/// future after the peer resumes it, and one after the peer resumes it again.
/// All three are the same shape, so the three serving paths differ only in what
/// their runtime answers.
struct AuthorityHandoff {
    built: Receiver<camber::JoinHandle<&'static str>>,
    admitted: Receiver<camber::JoinHandle<&'static str>>,
    late: Receiver<camber::JoinHandle<&'static str>>,
    factory: SpawnProbe,
    first: SpawnProbe,
    second: SpawnProbe,
    /// The release end of the gate the callback parks on once it is done.
    ///
    /// Held for its `Drop`, which is what lets the callback return once the
    /// row is done with its connection.
    #[expect(dead_code, reason = "held for its Drop, which unparks the callback")]
    parked: CallbackRelease,
}

impl AuthorityHandoff {
    /// What joining the factory's spawn answered.
    ///
    /// Read only once the factory's closure has been released or refused, so
    /// the join answers rather than waits.
    fn built(&self) -> Result<&'static str, RuntimeError> {
        Self::issued(&self.built, "the callback factory never issued its spawn").join()
    }

    /// The handle the resumed callback's first `camber::spawn` produced.
    fn admitted(&self) -> camber::JoinHandle<&'static str> {
        Self::issued(
            &self.admitted,
            "the resumed callback never issued its spawn",
        )
    }

    /// What joining the late spawn answered.
    ///
    /// The late child is released first. It is refused wherever admission has
    /// closed or there is no runtime, and so never runs there, but a spawn that
    /// was wrongly admitted would park — and this row must fail on the outcome
    /// rather than hang on it.
    fn late(&self) -> Result<&'static str, RuntimeError> {
        self.second.release();
        Self::issued(
            &self.late,
            "the resumed callback never issued its late spawn",
        )
        .join()
    }

    /// The handle one of the callback's spawns produced, under the bound.
    fn issued(
        handles: &Receiver<camber::JoinHandle<&'static str>>,
        missing: &str,
    ) -> camber::JoinHandle<&'static str> {
        handles.recv_timeout(DIRECTION_DEADLINE).expect(missing)
    }

    fn factory(&self) -> &SpawnProbe {
        &self.factory
    }

    fn first(&self) -> &SpawnProbe {
        &self.first
    }

    fn second(&self) -> &SpawnProbe {
        &self.second
    }
}

/// The callback's end of an [`AuthorityHandoff`].
///
/// Cloned once per future, because the factory is an `Fn` and each future has
/// to own what it reports through across its suspensions.
#[derive(Clone)]
struct AuthorityReports {
    admitted: Sender<camber::JoinHandle<&'static str>>,
    late: Sender<camber::JoinHandle<&'static str>>,
    parked: CallbackPark,
}

/// Add a direct route whose callback asks its own runtime what it may admit.
///
/// Takes the router rather than building one: each serving path needs its
/// owner's readiness route beside this one, and a second definition of that
/// route here would be a second thing to keep in step.
fn authority_router(mut router: Router, path: &str) -> (Router, AuthorityHandoff) {
    let (factory_parts, factory) = ChildParts::new();
    let (first_parts, first) = ChildParts::new();
    let (second_parts, second) = ChildParts::new();
    let (built_tx, built) = std::sync::mpsc::channel();
    let (admitted, admitted_rx) = std::sync::mpsc::channel();
    let (late, late_rx) = std::sync::mpsc::channel();
    let (parked, parked_rx) = callback_gate();
    let reports = AuthorityReports {
        admitted,
        late,
        parked: parked_rx,
    };
    router.ws(path, move |_request: &Request, connection: WsConn| {
        // The factory's own site: this spawn is issued before any future exists.
        let issued = built_tx.send(camber::spawn(factory_parts.body(AUTHORITY_CHILD)));
        let resumed = first_parts.body(AUTHORITY_CHILD);
        let late = second_parts.body(AUTHORITY_CHILD);
        let reports = reports.clone();
        async move {
            issued.map_err(|_| RuntimeError::ChannelClosed)?;
            authority_callback(connection, reports, resumed, late).await
        }
    });
    (
        router,
        AuthorityHandoff {
            built,
            admitted: admitted_rx,
            late: late_rx,
            factory,
            first,
            second,
            parked,
        },
    )
}

/// The future one runtime-authority callback returns.
///
/// Each spawn follows a receive the callback acknowledged before it waited, so
/// both are issued by a future that has already been suspended at least once.
async fn authority_callback<A, L>(
    mut connection: WsConn,
    reports: AuthorityReports,
    admitted: A,
    late: L,
) -> Result<(), RuntimeError>
where
    A: FnOnce() -> &'static str + Send + 'static,
    L: FnOnce() -> &'static str + Send + 'static,
{
    await_peer(&mut connection).await?;
    reports
        .admitted
        .send(camber::spawn(admitted))
        .map_err(|_| RuntimeError::ChannelClosed)?;
    await_peer(&mut connection).await?;
    reports
        .late
        .send(camber::spawn(late))
        .map_err(|_| RuntimeError::ChannelClosed)?;
    park_until_released(&reports.parked).await;
    Ok(())
}

/// One admitted outbound frame held at the writer, and one peer message already
/// in the receive queue.
///
/// Every terminal row starts here, because these two are exactly what the
/// disposition table decides the fate of: a send that returned success without
/// reaching the peer, and a message that arrived before the connection ended.
async fn stage_async_admitted_and_queued(
    fixture: &DirectionTestFixture,
    peer: &mut tokio::net::TcpStream,
    sender: &WsSender,
) {
    hold_admitted_frame(fixture, sender).await;
    fixture
        .queue_from_async_peer(
            peer,
            TEXT,
            QUEUED_INBOUND.as_bytes(),
            "the queued peer message",
        )
        .await;
}

/// Write a text frame with its mask bit clear, which no client may send.
async fn write_async_unmasked_frame(peer: &mut tokio::net::TcpStream) {
    use tokio::io::AsyncWriteExt;
    lifecycle_event(
        "the unmasked frame",
        peer.write_all(&RawFrame::complete(TEXT, b"bad").encode()),
    )
    .await
    .expect("write an unmasked client frame");
}

/// Admit one outbound frame and hold the writer with it in hand.
async fn hold_admitted_frame(fixture: &DirectionTestFixture, sender: &WsSender) {
    fixture.arm(BEFORE_WRITE);
    sender
        .send(HELD)
        .await
        .expect("admit the held outbound frame");
    fixture.wait_paused(BEFORE_WRITE).await;
}

/// Every owner one terminal row's cause had to let go of, on a live server.
///
/// The connection permit is proved by a second handshake: this runtime admits
/// one connection at a time, so a peer that completes its upgrade could only
/// have done so on a permit the ended bridge gave back.
async fn assert_owners_released(
    fixture: &DirectionTestFixture,
    cause: WsCloseCause,
    cancelled: usize,
) {
    drop(fixture.connect_async(DIRECTION_PATH).await);
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

/// Require the row's cause, then hand back what the bridge settled with.
///
/// The cause is fixed once committed, so the second read names the same one.
fn assert_release_state(
    fixture: &DirectionTestFixture,
    cause: WsCloseCause,
) -> camber::http::mock::WebSocketDirectionObservation {
    assert_terminal(fixture, cause);
    fixture.observed()
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

/// Read the cause a bridge held at its commit fixed, then let it go on.
///
/// Read twice: through the bridge's own record, and through a send, which asks
/// production's terminal state. The held writer goes with the commit, so what
/// each row reads next is what that cause did with the frame it was holding.
async fn release_committed(fixture: &DirectionTestFixture, sender: &WsSender, cause: WsCloseCause) {
    fixture.wait_paused(AFTER_COMMIT).await;
    assert_terminal(fixture, cause);
    assert_closed_send(sender, cause).await;
    fixture.release(AFTER_COMMIT);
    fixture.release(BEFORE_WRITE);
}

/// A send on a connection whose cause is already fixed reports that cause.
async fn assert_closed_send(sender: &WsSender, expected: WsCloseCause) {
    assert_eq!(
        closed_cause(sender.send("after-the-end").await, "a send past the end"),
        expected,
        "a send past the end reported another cause"
    );
}

/// A delivering cause hands over what was queued before it, then itself.
async fn assert_delivered(receiver: &mut WsReceiver, cause: WsCloseCause) {
    assert_received_text(
        bounded_receive(receiver, "the queued message").await,
        QUEUED_INBOUND,
        "the queued message",
    );
    assert_closed_with(
        bounded_receive(receiver, "the terminal cause").await,
        cause,
        "the delivery",
    );
}

/// A discarding cause hands over nothing but itself.
async fn assert_closed_receive(receiver: &mut WsReceiver, cause: WsCloseCause) {
    assert_closed_with(
        bounded_receive(receiver, "the terminal cause").await,
        cause,
        "the discarding cause",
    );
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
async fn expect_async_peer_payload(peer: &mut tokio::net::TcpStream, tag: u8, what: &str) {
    let (opcode, payload) = read_async_ws_frame_or_eof(peer, what)
        .await
        .expect("the peer closed before its payload");
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
