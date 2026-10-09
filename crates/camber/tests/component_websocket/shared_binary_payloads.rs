//! What the shipped shared-binary admission costs a caller, and what it does
//! with the handle it was given.
//!
//! Every row here enters through `Router::ws`, a real upgrade, and the public
//! sender the production callback was handed, so the queue, the pump, the frame
//! conversion, and the socket under each claim are the ones an application
//! reaches. The allocation oracle measures the caller's own thread across the
//! shipped operation alone — the immediate send, and the waiting send polled to
//! its admission; the shipped borrowed-slice helpers beside them are the
//! copying controls that show the oracle can tell the two apart.

#![cfg(feature = "ws")]
use crate::common::{
    AFTER_COMMIT, BEFORE_WRITE, FILLING_TEXT, FRAME_BUILT, HELD_TEXT, LARGE_PAYLOAD,
    PayloadWitness, SMALL_PAYLOAD, SharedPayloadFixture, assert_async_texts_in_order,
    assert_no_further_payload, assert_payload_bytes, closed_cause, fill_outbound_behind_the_writer,
    on_ws_executors, payload_bytes, shared_payload_row, witnessed_payload,
};
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
use crate::common::{
    Admission, FANOUTS, assert_borrowed_copies_per_recipient, assert_payload_flat,
};
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
use allocation_counter::AllocationInfo;
use camber::RuntimeError;
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
use camber::http::WsSender;
use camber::http::{Bytes, WsCloseCause};

/// The capacity every measured connection's outbound queue is given.
///
/// Wide enough that one admission per measured window never waits, and narrow
/// enough that the row still runs against a bounded queue rather than one that
/// happens never to fill.
const MEASURED_BUFFER: usize = 4;

/// The capacity a refusal row's outbound queue is given.
const REFUSAL_BUFFER: usize = 1;

/// The payload each row tags its bytes with, so a frame read at a peer names
/// the row that admitted it.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
const SHARED_TAG: u8 = 0x21;
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
const BORROWED_TAG: u8 = 0x22;
const IDENTITY_TAG: u8 = 0x23;
const FULL_TAG: u8 = 0x24;
const TERMINAL_TAG: u8 = 0x25;

/// The tag a payload admitted `through` carries.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
const fn admission_tag(through: Admission) -> u8 {
    match through.shares() {
        true => SHARED_TAG,
        false => BORROWED_TAG,
    }
}

// 1.T1, revised in 2.T8.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
#[test]
fn shared_binary_allocation_stays_payload_flat_across_fanout() {
    on_ws_executors(|| async {
        for recipients in FANOUTS {
            assert_admission_is_payload_flat(recipients).await;
        }
    });
}

/// One fanout's calibrated comparison: both shared paths pay the same for both
/// payload sizes, and both borrowed paths pay one copy per recipient.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
async fn assert_admission_is_payload_flat(recipients: usize) {
    shared_payload_row(MEASURED_BUFFER, recipients, move |mut row| async move {
        let senders = row.senders();
        for through in [
            Admission::Shared,
            Admission::SharedWaiting,
            Admission::Borrowed,
            Admission::BorrowedWaiting,
        ] {
            assert_admission_cost(&mut row, &senders, through).await;
        }
    })
    .await;
}

/// One admission's comparison across the two payload sizes: payload-flat for
/// a shared path, one copy per recipient for a borrowed control.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
async fn assert_admission_cost(
    row: &mut SharedPayloadFixture,
    senders: &[WsSender],
    through: Admission,
) {
    let small = calibrated(row, senders, through, SMALL_PAYLOAD).await;
    let large = calibrated(row, senders, through, LARGE_PAYLOAD).await;
    let what = format!(
        "{} admission to {} recipients",
        through.label(),
        senders.len()
    );
    match through.shares() {
        true => assert_payload_flat(&small, &large, &what),
        false => assert_borrowed_copies_per_recipient(&small, &large, senders.len(), &what),
    }
}

/// Warm every queue, admit one payload of exactly `len` bytes through
/// `through`, and take the exact bytes it owed every peer.
///
/// The drain is part of the row rather than an afterthought: it is what proves
/// the measured admission actually delivered, so a window that measured nothing
/// because nothing was admitted cannot pass.
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
async fn calibrated(
    row: &mut SharedPayloadFixture,
    senders: &[WsSender],
    through: Admission,
    len: usize,
) -> AllocationInfo {
    row.warm_queues_from(0).await;
    let payload = payload_bytes(len, admission_tag(through));
    let measured = through.measure(senders, &payload);
    row.take_peer_frames_from(0, &payload, "the measured admission")
        .await;
    measured
}

/// The production frame conversion keeps the caller's own backing allocation,
/// and lets it go once the bridge is done with it.
///
/// The numeric oracle above is the authority for what admission costs; this is
/// the authority for identity, because it holds the real `Message::Binary` the
/// production pump built and asks whether the caller's backing is still under
/// it. A conversion that copied would have dropped the last handle by here.
#[test]
fn shared_binary_frame_conversion_keeps_the_same_backing() {
    on_ws_executors(|| async {
        shared_payload_row(MEASURED_BUFFER, 1, |mut row| async move {
            let expected = payload_bytes(LARGE_PAYLOAD, IDENTITY_TAG);
            row.listener().arm(FRAME_BUILT);
            let mut witness = row.offer_clones(0, 1, &expected, "the converted frame's payload");
            row.listener().wait_paused(FRAME_BUILT).await;
            witness.assert_live("the production frame conversion");
            row.listener().release(FRAME_BUILT);
            row.take_peer_frames_from(0, &expected, "the converted frame")
                .await;
            row.end_every_connection();
            row.stop_and_join().await;
            witness.assert_released("the joined bridge").await;
        })
        .await;
    });
}

// 1.T2, revised in 2.T8. A waiting send dropped while it waits is
// `async_endpoints`'s cancellation row, not a refusal, so it is not repeated
// here.
#[test]
fn shared_binary_refusals_release_the_offered_handle() {
    on_ws_executors(|| async {
        assert_live_full_immediate_send_refuses_and_releases().await;
        assert_terminal_sends_refuse_and_release().await;
    });
}

/// A full queue on a live connection refuses the immediate shared send, keeps
/// nothing, and goes on writing only what it had already admitted.
async fn assert_live_full_immediate_send_refuses_and_releases() {
    shared_payload_row(REFUSAL_BUFFER, 1, |mut row| async move {
        let sender = row.sender(0).clone();
        fill_outbound_behind_the_writer(row.listener(), &sender).await;
        let expected = payload_bytes(SMALL_PAYLOAD, FULL_TAG);
        let (payload, mut witness) = witnessed_payload(&expected, "the refused shared payload");
        let sibling = payload.clone();
        let refused = sender.try_send_shared_binary(payload);
        assert!(
            matches!(refused, Err(RuntimeError::ChannelFull)),
            "a full queue on a live connection answered {refused:?}"
        );
        assert_released_leaving_the_sibling(&mut witness, sibling, &expected, "the full queue")
            .await;
        drop(sender);
        row.listener().release(BEFORE_WRITE);
        assert_only_the_filled_frames_arrive(&mut row).await;
    })
    .await;
}

/// Both shared operations on a connection whose cause is already fixed report
/// that cause, admit nothing, and keep nothing.
async fn assert_terminal_sends_refuse_and_release() {
    shared_payload_row(MEASURED_BUFFER, 1, |mut row| async move {
        let sender = row.sender(0).clone();
        row.listener().arm(AFTER_COMMIT);
        row.client(0).release_halves();
        row.listener().wait_paused(AFTER_COMMIT).await;
        let expected = payload_bytes(SMALL_PAYLOAD, TERMINAL_TAG);
        let (payload, mut witness) = witnessed_payload(&expected, "the terminal shared payload");
        let sibling = payload.clone();
        let waited = sender.send_shared_binary(payload.clone()).await;
        let immediate = sender.try_send_shared_binary(payload);
        assert_eq!(
            closed_cause(waited, "a shared waiting send past the end"),
            WsCloseCause::ReceiverDropped,
            "a shared waiting send past the end reported another cause"
        );
        assert_eq!(
            closed_cause(immediate, "a shared immediate send past the end"),
            WsCloseCause::ReceiverDropped,
            "a shared immediate send past the end reported another cause"
        );
        assert_released_leaving_the_sibling(
            &mut witness,
            sibling,
            &expected,
            "the terminal refusals",
        )
        .await;
        drop(sender);
        row.listener().release(AFTER_COMMIT);
        assert_no_further_payload(
            row.client(0).peer(),
            "a terminal refusal still reached the peer",
        )
        .await;
    })
    .await;
}

/// A sibling clone still exposes the exact original bytes, and the backing goes
/// only once every legitimate handle has.
///
/// The two halves belong together: a refusal that quietly retained its clone
/// would keep the backing alive past this, and a refusal that corrupted the
/// shared storage would fail the bytes first.
async fn assert_released_leaving_the_sibling(
    witness: &mut PayloadWitness,
    sibling: Bytes,
    expected: &[u8],
    what: &str,
) {
    assert_payload_bytes(
        &sibling,
        expected,
        &format!("the sibling clone after {what}"),
    );
    witness.assert_live("the sibling clone");
    drop(sibling);
    witness.assert_released(what).await;
}

/// Prove the peer took the two frames the queue had already admitted, and
/// nothing after them.
///
/// The row drops its own send handle before this runs, so releasing the
/// fixture's halves is what ends the connection: the transport's end is the
/// bounded way to say a refused payload never reached the wire.
async fn assert_only_the_filled_frames_arrive(row: &mut SharedPayloadFixture) {
    assert_async_texts_in_order(row.client(0).peer(), &[HELD_TEXT, FILLING_TEXT]).await;
    row.release_every_half();
    assert_no_further_payload(
        row.client(0).peer(),
        "a refused shared admission still reached the peer",
    )
    .await;
}
