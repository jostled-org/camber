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
    BEFORE_WRITE, CLOSE, DirectionTestFixture, FANOUTS, FRAME_BUILT, LARGE_PAYLOAD, PayloadWitness,
    SELECTED, SMALL_PAYLOAD, SharedPayloadFixture, abortive_direction_row, assert_payload_flat,
    fill_outbound_behind_the_writer, measure_shared_admission, offer_shared_clones, payload_bytes,
    read_ws_frame_raw, shared_payload_row, witnessed_payload, write_ws_close_frame,
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

// 1.T3
#[camber::test]
async fn shared_binary_fanout_delivers_exact_bytes_to_1_2_16_100_peers() {
    for recipients in FANOUTS {
        assert_fanout_delivers_exact_bytes(recipients).await;
    }
}

/// One payload, cloned into every recipient, survives its caller.
///
/// The original is dropped while one recipient's writer is still held, so the
/// only thing keeping the backing alive at that point is what the connections
/// themselves retained.
async fn assert_fanout_delivers_exact_bytes(recipients: usize) {
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
    })
    .await;
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
    })
    .await;
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

// 1.T5
#[camber::test]
async fn shared_binary_terminal_paths_release_every_payload_handle() {
    peer_disconnect_release_row().await;
    receiver_drop_release_row().await;
    graceful_shutdown_release_row().await;
    forced_cancellation_release_row().await;
}

/// A peer whose transport is reset: the converted frame and the queued clone
/// are both dropped, and the backing goes with them.
async fn peer_disconnect_release_row() {
    abortive_direction_row(TERMINAL_BUFFER, |fixture, peer, connection| async move {
        let (sender, receiver) = connection.split();
        let expected = payload_bytes(SMALL_PAYLOAD, DISCONNECT_TAG);
        let mut witness =
            stage_converted_and_queued(&fixture, &sender, &expected, "the peer-disconnect payload")
                .await;
        // The cause is fixed before the server is asked to stop, because a stop
        // requested in the same coordinator turn outranks a peer that went away
        // and the row would then be reading another cause's disposition.
        fixture.arm(SELECTED);
        drop(peer);
        fixture.wait_paused(SELECTED).await;
        fixture.release(SELECTED);
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
/// queued clone alike.
async fn receiver_drop_release_row() {
    shared_payload_row(TERMINAL_BUFFER, 1, |mut row| async move {
        let expected = payload_bytes(SMALL_PAYLOAD, RECEIVER_TAG);
        let mut witness = stage_converted_and_queued(
            row.listener(),
            row.sender(0),
            &expected,
            "the receiver-drop payload",
        )
        .await;
        // The cause is fixed before the peer goes, because a peer dropped in
        // the same coordinator turn outranks a receive owner that left and the
        // row would then be reading another cause's disposition.
        row.listener().arm(SELECTED);
        drop(row.client(0).take_receiver());
        row.listener().wait_paused(SELECTED).await;
        row.end_every_connection();
        row.listener().release(SELECTED);
        row.stop_and_join().await;
        assert_bridge_released(&row.listener().observed(), WsCloseCause::ReceiverDropped, 1);
        witness.assert_released("the receiver-drop bridge").await;
    })
    .await;
}

/// A graceful stop keeps the promise a successful shared send was given: the
/// converted frame and the queued clone both reach the peer, then the close.
///
/// The checkpoint's release is staged before the server is asked to stop, so
/// the coordinator's own turn advances the already-released outbound future
/// into the sink. Asking for the stop first can cancel that future while it
/// alone owns the converted frame, which would drop a frame a successful send
/// was promised.
async fn graceful_shutdown_release_row() {
    shared_payload_row(TERMINAL_BUFFER, 1, |mut row| async move {
        let expected = payload_bytes(SMALL_PAYLOAD, SHUTDOWN_TAG);
        let mut witness = stage_converted_and_queued(
            row.listener(),
            row.sender(0),
            &expected,
            "the shutdown payload",
        )
        .await;
        let turns = row.checkpoint_polls(FRAME_BUILT);
        row.listener().stage_release(FRAME_BUILT);
        row.listener().shutdown_server();
        row.take_peer_frames_from(0, &expected, "the converted frame");
        row.take_peer_frames_from(0, &expected, "the queued clone");
        assert!(
            row.checkpoint_polls(FRAME_BUILT) > turns,
            "the staged release never reached the held outbound future"
        );
        expect_peer_close(row.client(0).peer(), "a graceful stop sent no close frame");
        write_ws_close_frame(row.client(0).peer());
        row.release_every_half();
        row.listener()
            .join_server()
            .await
            .expect("the owned server completed");
        assert_bridge_released(&row.listener().observed(), WsCloseCause::ServerShutdown, 0);
        witness.assert_released("the drained bridge").await;
    })
    .await;
}

/// A cancelled server owes nothing, and lets go of everything.
///
/// The held checkpoint is never released as progress: the coordinator drops the
/// outbound future the moment it selects the cause, and that drop is what has to
/// release the converted frame.
async fn forced_cancellation_release_row() {
    shared_payload_row(TERMINAL_BUFFER, 1, |row| async move {
        let expected = payload_bytes(SMALL_PAYLOAD, CANCELLED_TAG);
        let mut witness = stage_converted_and_queued(
            row.listener(),
            row.sender(0),
            &expected,
            "the cancellation payload",
        )
        .await;
        row.listener().cancel_server();
        let completed = row.listener().join_server().await;
        assert!(
            matches!(completed, Err(RuntimeError::Cancelled)),
            "a cancelled server completed as {completed:?}"
        );
        assert_bridge_released(&row.listener().observed(), WsCloseCause::ServerCancelled, 1);
        witness.assert_released("the cancelled bridge").await;
    })
    .await;
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
