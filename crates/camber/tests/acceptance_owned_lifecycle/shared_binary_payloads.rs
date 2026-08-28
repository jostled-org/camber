//! What one shared immutable payload does across real connections, and what
//! every terminal owner does with the handles it is still holding.
//!
//! Every row here serves a real route, upgrades real peers, and frames over real
//! sockets, because the claims are about a transport: that one `Bytes` value
//! reaches every recipient intact after its caller has let go of it, that a
//! saturated recipient cannot spend a sibling's capacity or make a sibling
//! allocate, and that no cause leaves a queued, pending, or framed payload
//! handle behind.

#![cfg(feature = "ws")]
// The allocation oracle installs its own global allocator, and two global
// allocators do not link. Built only where that probe is, exactly as every other
// measured row in the suite is.
#![cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]

use crate::common::{
    AFTER_COMMIT, BEFORE_COMMIT, BEFORE_WRITE, BEFORE_WRITE_EDGE, CLOSE, DirectionTestFixture,
    FANOUTS, FRAME_BUILT, LARGE_PAYLOAD, PayloadWitness, SMALL_PAYLOAD, SharedPayloadFixture,
    abortive_direction_row, assert_payload_flat, fill_outbound_behind_the_writer,
    measure_shared_admission, offer_shared_clones, payload_bytes, read_ws_frame_raw,
    shared_payload_row, witnessed_payload, write_ws_close_frame,
};
use allocation_counter::AllocationInfo;
use camber::RuntimeError;
use camber::http::mock::WebSocketDirectionObservation;
use camber::http::{Bytes, WsCloseCause, WsSender};
use std::net::TcpStream;

/// The capacity a fanout row's outbound queues are given.
///
/// One admission per recipient, and room for the pump to take it, so a delivery
/// claim is never a claim about waiting for capacity.
const FANOUT_BUFFER: usize = 4;

/// How many clones of one payload a terminal row admits.
///
/// The pump converts the first into its transport frame and holds it on its own
/// stack, and the second stays in the bounded queue. One row then covers both
/// the framed handle and the queued one.
const CONVERTED_AND_QUEUED: usize = 2;

/// The capacity a terminal row's outbound queue is given.
///
/// Exactly [`CONVERTED_AND_QUEUED`], so both clones are admitted without a wait
/// and the queue is still the bounded one production built.
const TERMINAL_BUFFER: usize = CONVERTED_AND_QUEUED;

/// How many real connections the saturation row opens.
///
/// One parked recipient and fifteen siblings: enough that a per-recipient copy
/// of the large payload would be fifteen megabytes over the bound, and few
/// enough that the row's own frame reads stay quick.
const SATURATION_PEERS: usize = 16;

/// The payload each row tags its bytes with.
const FANOUT_TAG: u8 = 0x31;
const PARKED_TAG: u8 = 0x32;
const SIBLING_SMALL_TAG: u8 = 0x33;
const SIBLING_LARGE_TAG: u8 = 0x34;
const DISCONNECT_TAG: u8 = 0x35;
const RECEIVER_TAG: u8 = 0x36;
const SHUTDOWN_TAG: u8 = 0x37;
const CANCELLED_TAG: u8 = 0x38;
const BACKPRESSURE_TAG: u8 = 0x39;

// 1.T3
#[camber::test]
async fn shared_binary_fanout_delivers_exact_bytes_to_1_2_16_100_peers() {
    for recipients in FANOUTS {
        assert_fanout_delivers_exact_bytes(recipients);
    }
}

/// One payload, cloned into every recipient, survives its caller.
///
/// The original is dropped while one recipient's writer is still held, so the
/// only thing keeping the backing alive at that point is what the connections
/// themselves retained.
fn assert_fanout_delivers_exact_bytes(recipients: usize) {
    shared_payload_row(FANOUT_BUFFER, recipients, |mut row| async move {
        let expected = payload_bytes(LARGE_PAYLOAD, FANOUT_TAG);
        let (payload, mut witness) = witnessed_payload(&expected, "the fanned-out payload");
        row.listener().arm(BEFORE_WRITE);
        // The waiting operation, because a fanout producer is what it is for:
        // one payload offered to every open recipient in turn, each admission
        // returning where that recipient's own bounded queue took the handle.
        for index in 0..row.recipients() {
            row.sender(index)
                .send_shared_binary(payload.clone())
                .expect("admit one shared clone");
        }
        drop(payload);
        row.listener().wait_paused(BEFORE_WRITE).await;
        // One writer is held, so this says the admitted clones outlived the
        // caller's own handle — not which recipient is still holding one.
        witness.assert_live("the admitted clones after the original went");
        row.listener().release(BEFORE_WRITE);
        row.take_peer_frames_from(0, &expected, "the fanned-out payload");
        row.end_every_connection();
        row.stop_and_join().await;
        witness.assert_released("every completed bridge").await;
    });
}

// 1.T4
#[camber::test]
async fn saturated_shared_binary_recipient_does_not_impede_siblings() {
    shared_payload_row(1, SATURATION_PEERS, |mut row| async move {
        let senders = row.senders();
        assert_saturated(&row, &senders[0]).await;
        let siblings = &senders[1..];
        row.warm_queues_from(1);
        let (small, mut small_witness) =
            admit_and_take(&mut row, siblings, SIBLING_SMALL_TAG, SMALL_PAYLOAD);
        let (large, mut large_witness) =
            admit_and_take(&mut row, siblings, SIBLING_LARGE_TAG, LARGE_PAYLOAD);
        assert_payload_flat(&small, &large, "sibling admission behind a saturated peer");
        // Releasing succeeds only against a checkpoint still paused, so this is
        // the row's proof that the saturated recipient's writer never moved
        // while every sibling delivered.
        row.listener().release(BEFORE_WRITE);
        small_witness
            .assert_released("the small sibling payload")
            .await;
        large_witness
            .assert_released("the large sibling payload")
            .await;
    });
}

/// Fill one recipient's capacity-one queue behind its held writer, and require
/// its next immediate shared send is refused for backpressure.
async fn assert_saturated(row: &SharedPayloadFixture, parked: &WsSender) {
    fill_outbound_behind_the_writer(row.listener(), parked).await;
    let refused = parked.try_send_shared_binary(Bytes::copy_from_slice(&payload_bytes(
        SMALL_PAYLOAD,
        PARKED_TAG,
    )));
    assert!(
        matches!(refused, Err(RuntimeError::ChannelFull)),
        "a saturated recipient answered {refused:?} rather than backpressure"
    );
}

/// Measure one owner-backed admission to every sibling, and take the exact
/// bytes it owed each of them.
///
/// The witness comes back with the measurement because the row asks two things
/// of the same payload: what admitting it cost the sibling caller thread, and
/// whether every handle on it is released once the connections are done.
fn admit_and_take(
    row: &mut SharedPayloadFixture,
    siblings: &[WsSender],
    tag: u8,
    len: usize,
) -> (AllocationInfo, PayloadWitness) {
    let expected = payload_bytes(len, tag);
    let (payload, witness) = witnessed_payload(&expected, "the sibling payload");
    let measured = measure_shared_admission(siblings, &payload);
    drop(payload);
    row.take_peer_frames_from(1, &expected, "the sibling payload");
    (measured, witness)
}

// 1.T5, revised by 4.T4
//
// Every row states its cause through a public or protocol barrier rather than
// through whichever event a coordinator turn reached first, and every row still
// owes the same account of what its cause released.
#[camber::test]
async fn websocket_backpressure_payload_and_close_contracts_survive_causal_cutover() {
    saturated_admission_refusal_row().await;
    peer_disconnect_release_row().await;
    receiver_drop_release_row();
    graceful_shutdown_release_row();
    forced_cancellation_release_row();
}

/// A bounded queue answers backpressure, the frames it did take still reach the
/// peer, and the close that follows is the peer's own.
///
/// The queue is filled behind a held writer, so the refusal is the production
/// bound rather than a slow reader: every admission this row is owed has been
/// taken, and the next one has nowhere to go. The cause is then stated through
/// the peer's close and the echo that answers it, so the row's cleanup claim
/// rests on a protocol acknowledgement rather than on a teardown race.
async fn saturated_admission_refusal_row() {
    shared_payload_row(TERMINAL_BUFFER, 1, |mut row| async move {
        let expected = payload_bytes(SMALL_PAYLOAD, BACKPRESSURE_TAG);
        let (payload, mut witness) = witnessed_payload(&expected, "the saturating payload");
        row.listener().arm(BEFORE_WRITE);
        for _ in 0..=TERMINAL_BUFFER {
            row.sender(0)
                .send_shared_binary(payload.clone())
                .expect("admit one clone into the bounded queue");
        }
        let refused = row.sender(0).try_send_shared_binary(payload.clone());
        assert!(
            matches!(refused, Err(RuntimeError::ChannelFull)),
            "a saturated outbound queue answered {refused:?} rather than backpressure"
        );
        drop(payload);
        row.listener().wait_paused(BEFORE_WRITE).await;
        row.listener().release(BEFORE_WRITE);
        for _ in 0..=TERMINAL_BUFFER {
            row.take_peer_frames_from(0, &expected, "an admitted clone");
        }
        row.listener().arm(AFTER_COMMIT);
        write_ws_close_frame(row.client(0).peer());
        row.listener().wait_paused(AFTER_COMMIT).await;
        row.listener().release(AFTER_COMMIT);
        expect_peer_close(row.client(0).peer(), "the peer close was never echoed");
        row.release_every_half();
        row.end_every_connection();
        row.stop_and_join().await;
        assert_bridge_released(&row.listener().observed(), WsCloseCause::PeerClosed, 0);
        witness.assert_released("the saturated bridge").await;
    });
}

/// A peer whose transport is reset: the converted frame and the queued clone
/// are both dropped, and the backing goes with them.
///
/// Run on the case's own runtime rather than on a row runtime of its own,
/// because its stop is the only graceful one there: the three rows below take a
/// runtime each so they cannot spend one another's aggregate grace, and this
/// row has nothing to share that grace with.
async fn peer_disconnect_release_row() {
    abortive_direction_row(TERMINAL_BUFFER, |fixture, peer, connection| async move {
        let (sender, receiver) = connection.split();
        let expected = payload_bytes(SMALL_PAYLOAD, DISCONNECT_TAG);
        let mut witness =
            stage_converted_and_queued(&fixture, &sender, &expected, "the peer-disconnect payload")
                .await;
        // The cause is committed before the server is asked to stop, so the
        // stop is the later fact and this row reads the peer's disposition. A
        // stop asked for first would be the earlier one, and the row would then
        // be reading a cause it did not stage.
        fixture.arm(AFTER_COMMIT);
        drop(peer);
        fixture.wait_paused(AFTER_COMMIT).await;
        fixture.release(AFTER_COMMIT);
        fixture.shutdown_server();
        fixture
            .join_server()
            .await
            .expect("the owned server completed");
        assert_bridge_released(&fixture.observed(), WsCloseCause::PeerDisconnected, 1);
        drop((sender, receiver));
        witness.assert_released("the disconnected bridge").await;
    })
    .await;
}

/// The unique receive owner going away cancels the converted frame and the
/// queued clone alike, and a graceful stop that lands afterwards does not turn
/// that into a drain.
///
/// The bridge is held short of its commit with the receive owner's departure in
/// hand, and the public `shutdown` then returns — so the graceful phase is
/// committed in the shared stop state before the commit this bridge takes
/// inside it. A graceful stop closes admission and lets open connections
/// finish, so this connection reports why it ended, and the disposition that
/// follows is the cancelling one its own cause owes. A bridge that let the stop
/// speak for an open connection would drain both handles to the peer instead.
fn receiver_drop_release_row() {
    shared_payload_row(TERMINAL_BUFFER, 1, |mut row| async move {
        let expected = payload_bytes(SMALL_PAYLOAD, RECEIVER_TAG);
        let mut witness = stage_converted_and_queued(
            row.listener(),
            row.sender(0),
            &expected,
            "the receiver-drop payload",
        )
        .await;
        row.listener().arm(BEFORE_COMMIT);
        drop(row.client(0).take_receiver());
        row.listener().wait_paused(BEFORE_COMMIT).await;
        row.listener().shutdown_server();
        row.listener().arm(AFTER_COMMIT);
        row.listener().release(BEFORE_COMMIT);
        row.listener().wait_paused(AFTER_COMMIT).await;
        assert_eq!(
            row.listener().observed().terminal,
            Some(WsCloseCause::ReceiverDropped),
            "the receiver-drop row fixed another cause"
        );
        row.end_every_connection();
        row.listener().release(AFTER_COMMIT);
        row.listener()
            .join_server()
            .await
            .expect("the owned server completed");
        assert_bridge_released(&row.listener().observed(), WsCloseCause::ReceiverDropped, 1);
        witness.assert_released("the receiver-drop bridge").await;
    });
}

/// A graceful stop keeps the promise a successful shared send was given: the
/// converted frame and the queued clone both reach the peer, then the close.
///
/// The hold is advanced off the conversion before the stop is asked for. A pump
/// held at [`FRAME_BUILT`] owns that frame on its running future's own stack,
/// and the coordinator drops that future the moment it answers — so a stop
/// published while the bridge's turn is still in flight takes the frame with
/// it. That turn is in flight for as long as it takes to poll the sources after
/// the write side, and a wait that ends at the held future's first look ends
/// inside it, so no order the row asks for closes the window. Production never
/// stands there at all: `hand_over` builds the frame and gives it to the sink
/// within one turn, and only this checkpoint holds the two apart.
///
/// Held at [`BEFORE_WRITE`] the frame is the pump's own, which is where every
/// admitted frame waits. The release is then staged rather than woken, so the
/// stop lands in the same turn: whichever of the write side and the graceful
/// drain reaches the frame first, the peer is owed it and gets it.
fn graceful_shutdown_release_row() {
    shared_payload_row(TERMINAL_BUFFER, 1, |mut row| async move {
        let expected = payload_bytes(SMALL_PAYLOAD, SHUTDOWN_TAG);
        let mut witness = stage_converted_and_queued(
            row.listener(),
            row.sender(0),
            &expected,
            "the shutdown payload",
        )
        .await;
        row.listener().arm(BEFORE_WRITE);
        row.listener().release(FRAME_BUILT);
        row.listener().wait_paused(BEFORE_WRITE).await;
        row.listener().release_without_waking(BEFORE_WRITE_EDGE);
        row.listener().shutdown_server();
        row.take_peer_frames_from(0, &expected, "the converted frame");
        row.take_peer_frames_from(0, &expected, "the queued clone");
        expect_peer_close(row.client(0).peer(), "a graceful stop sent no close frame");
        write_ws_close_frame(row.client(0).peer());
        row.release_every_half();
        row.listener()
            .join_server()
            .await
            .expect("the owned server completed");
        assert_bridge_released(&row.listener().observed(), WsCloseCause::ServerShutdown, 0);
        witness.assert_released("the drained bridge").await;
    });
}

/// A cancelled server owes nothing, and lets go of everything.
///
/// The held checkpoint is never released as progress: the coordinator drops the
/// outbound future the moment it selects the cause, and that drop is what has to
/// release the converted frame.
fn forced_cancellation_release_row() {
    shared_payload_row(TERMINAL_BUFFER, 1, |row| async move {
        let expected = payload_bytes(SMALL_PAYLOAD, CANCELLED_TAG);
        let mut witness = stage_converted_and_queued(
            row.listener(),
            row.sender(0),
            &expected,
            "the cancellation payload",
        )
        .await;
        row.listener().select_server_cancellation().await;
        row.listener().release(AFTER_COMMIT);
        let completed = row.listener().join_server().await;
        assert!(
            matches!(completed, Err(RuntimeError::Cancelled)),
            "a cancelled server completed as {completed:?}"
        );
        assert_bridge_released(&row.listener().observed(), WsCloseCause::ServerCancelled, 1);
        witness.assert_released("the cancelled bridge").await;
    });
}

/// Admit two clones of one owner-backed payload and hold the bridge with the
/// first already converted into its transport frame.
///
/// Two clones rather than one: the converted frame sits on the outbound
/// future's own stack and the second stays in the bounded queue, so one row
/// covers both the framed handle and the queued one.
async fn stage_converted_and_queued(
    fixture: &DirectionTestFixture,
    sender: &WsSender,
    expected: &[u8],
    subject: &str,
) -> PayloadWitness {
    fixture.arm(FRAME_BUILT);
    let witness = offer_shared_clones(sender, CONVERTED_AND_QUEUED, expected, subject);
    fixture.wait_paused(FRAME_BUILT).await;
    // The paused bridge holds the converted frame and the queue holds the
    // sibling, so this says a handle survived admission, not which one.
    // 1.T1's single-clone identity row owns the conversion itself.
    witness.assert_live("the bridge paused with both admitted clones");
    witness
}

/// Require one completed bridge fixed the cause the row staged, applied the
/// disposition that cause owes, settled both directions, and gave its
/// connection permit back.
fn assert_bridge_released(
    observed: &WebSocketDirectionObservation,
    cause: WsCloseCause,
    cancelled: usize,
) {
    assert_eq!(
        observed.terminal,
        Some(cause),
        "the row fixed another cause"
    );
    assert_eq!(
        observed.outbound_cancelled, cancelled,
        "the {cause:?} row cancelled the wrong number of admitted frames"
    );
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

/// Require the next frame one peer takes is the protocol close.
fn expect_peer_close(peer: &mut TcpStream, what: &str) {
    let (opcode, _) = read_ws_frame_raw(peer);
    assert_eq!(opcode, CLOSE, "{what}: the peer took opcode {opcode:#x}");
}
