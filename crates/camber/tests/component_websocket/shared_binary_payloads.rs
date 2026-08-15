//! What the shipped shared-binary admission costs a caller, and what it does
//! with the handle it was given.
//!
//! Every row here enters through `Router::ws`, a real upgrade, and the public
//! sender the production callback was handed, so the queue, the pump, the frame
//! conversion, and the socket under each claim are the ones an application
//! reaches. The allocation oracle measures the caller's own thread across the
//! shipped operation alone; the shipped borrowed-slice helper beside it is the
//! copying control that shows the oracle can tell the two apart.

#![cfg(feature = "ws")]
// The allocation oracle installs its own global allocator, and two global
// allocators do not link. Built only where that probe is, exactly as every other
// measured row in the suite is.
#![cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]

use crate::common::{
    BEFORE_WRITE, FANOUTS, FRAME_BUILT, LARGE_PAYLOAD, PayloadWitness, SELECTED, SMALL_PAYLOAD,
    SharedPayloadFixture, assert_borrowed_copies_per_recipient, assert_no_further_payload,
    assert_payload_bytes, assert_payload_flat, closed_cause, fill_outbound_behind_the_writer,
    measure_borrowed_admission, measure_shared_admission, payload_bytes, read_ws_text_frame,
    shared_payload_row, witnessed_payload,
};
use allocation_counter::AllocationInfo;
use camber::RuntimeError;
use camber::http::{Bytes, WsCloseCause, WsSender};

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
const SHARED_TAG: u8 = 0x21;
const BORROWED_TAG: u8 = 0x22;
const IDENTITY_TAG: u8 = 0x23;
const FULL_TAG: u8 = 0x24;
const TERMINAL_TAG: u8 = 0x25;
const CURRENT_THREAD_TAG: u8 = 0x26;

/// The two frames a filled capacity-one queue is holding.
const HELD_TEXT: &str = "held-by-the-writer";
const FILLING_TEXT: &str = "fills-the-only-slot";

/// Which shipped admission one measured window runs through.
#[derive(Clone, Copy)]
enum Admission {
    /// `try_send_shared_binary`, which takes one `Bytes` handle by value.
    Shared,
    /// `try_send_binary`, which copies the borrowed slice once at admission.
    Borrowed,
}

impl Admission {
    /// The tag a payload admitted this way carries.
    const fn tag(self) -> u8 {
        match self {
            Self::Shared => SHARED_TAG,
            Self::Borrowed => BORROWED_TAG,
        }
    }
}

// 1.T1
#[camber::test]
async fn shared_binary_allocation_stays_payload_flat_across_fanout() {
    for recipients in FANOUTS {
        assert_admission_is_payload_flat(recipients).await;
    }
    assert_frame_conversion_keeps_the_same_backing().await;
}

/// One fanout's calibrated comparison: the shared path pays the same for both
/// payload sizes, and the borrowed path pays one copy per recipient.
async fn assert_admission_is_payload_flat(recipients: usize) {
    shared_payload_row(MEASURED_BUFFER, recipients, |mut row| async move {
        let senders = row.senders();
        let shared_small = calibrated(&mut row, &senders, Admission::Shared, SMALL_PAYLOAD);
        let shared_large = calibrated(&mut row, &senders, Admission::Shared, LARGE_PAYLOAD);
        let borrowed_small = calibrated(&mut row, &senders, Admission::Borrowed, SMALL_PAYLOAD);
        let borrowed_large = calibrated(&mut row, &senders, Admission::Borrowed, LARGE_PAYLOAD);
        assert_payload_flat(
            &shared_small,
            &shared_large,
            &format!("shared admission to {recipients} recipients"),
        );
        assert_borrowed_copies_per_recipient(
            &borrowed_small,
            &borrowed_large,
            recipients,
            &format!("borrowed admission to {recipients} recipients"),
        );
    })
    .await;
}

/// Warm every queue, admit one payload of exactly `len` bytes through
/// `through`, and take the exact bytes it owed every peer.
///
/// The drain is part of the row rather than an afterthought: it is what proves
/// the measured admission actually delivered, so a window that measured nothing
/// because nothing was admitted cannot pass.
fn calibrated(
    row: &mut SharedPayloadFixture,
    senders: &[WsSender],
    through: Admission,
    len: usize,
) -> AllocationInfo {
    row.warm_queues_from(0);
    let payload = payload_bytes(len, through.tag());
    let measured = match through {
        Admission::Shared => measure_shared_admission(senders, &Bytes::copy_from_slice(&payload)),
        Admission::Borrowed => measure_borrowed_admission(senders, &payload),
    };
    row.take_peer_frames_from(0, &payload, "the measured admission");
    measured
}

/// The production frame conversion keeps the caller's own backing allocation,
/// and lets it go once the bridge is done with it.
///
/// The numeric oracle above is the authority for what admission costs; this is
/// the authority for identity, because it holds the real `Message::Binary` the
/// production pump built and asks whether the caller's backing is still under
/// it. A conversion that copied would have dropped the last handle by here.
async fn assert_frame_conversion_keeps_the_same_backing() {
    shared_payload_row(MEASURED_BUFFER, 1, |mut row| async move {
        let expected = payload_bytes(LARGE_PAYLOAD, IDENTITY_TAG);
        row.listener().arm(FRAME_BUILT);
        let mut witness = row.offer_clones(0, 1, &expected, "the converted frame's payload");
        row.listener().wait_paused(FRAME_BUILT).await;
        witness.assert_live("the production frame conversion");
        row.listener().release(FRAME_BUILT);
        row.take_peer_frames_from(0, &expected, "the converted frame");
        row.end_every_connection();
        row.stop_and_join().await;
        witness.assert_released("the joined bridge").await;
    })
    .await;
}

// 1.T2
#[camber::test]
async fn shared_binary_refusals_release_the_offered_handle() {
    assert_live_full_immediate_send_refuses_and_releases().await;
    assert_terminal_sends_refuse_and_release().await;
    assert_current_thread_waiting_send_refuses_and_releases().await;
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
        let refused = sender.try_send_shared_binary(payload.clone());
        assert!(
            matches!(refused, Err(RuntimeError::ChannelFull)),
            "a full queue on a live connection answered {refused:?}"
        );
        drop(payload);
        assert_released_leaving_the_sibling(&mut witness, sibling, &expected, "the full queue")
            .await;
        drop(sender);
        row.listener().release(BEFORE_WRITE);
        assert_only_the_filled_frames_arrive(&mut row);
    })
    .await;
}

/// Both shared operations on a connection whose cause is already fixed report
/// that cause, admit nothing, and keep nothing.
async fn assert_terminal_sends_refuse_and_release() {
    shared_payload_row(MEASURED_BUFFER, 1, |mut row| async move {
        let sender = row.sender(0).clone();
        row.listener().arm(SELECTED);
        row.client(0).release_halves();
        row.listener().wait_paused(SELECTED).await;
        let expected = payload_bytes(SMALL_PAYLOAD, TERMINAL_TAG);
        let (payload, mut witness) = witnessed_payload(&expected, "the terminal shared payload");
        let sibling = payload.clone();
        let waited = sender.send_shared_binary(payload.clone());
        let immediate = sender.try_send_shared_binary(payload.clone());
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
        drop(payload);
        assert_released_leaving_the_sibling(
            &mut witness,
            sibling,
            &expected,
            "the terminal refusals",
        )
        .await;
        drop(sender);
        row.listener().release(SELECTED);
        assert_no_further_payload(
            row.client(0).peer(),
            "a terminal refusal still reached the peer",
        );
    })
    .await;
}

/// A current-thread caller is refused before it waits, and the handle it
/// offered goes with the wait that never happened.
async fn assert_current_thread_waiting_send_refuses_and_releases() {
    shared_payload_row(REFUSAL_BUFFER, 1, |mut row| async move {
        let sender = row.sender(0).clone();
        fill_outbound_behind_the_writer(row.listener(), &sender).await;
        let expected = payload_bytes(SMALL_PAYLOAD, CURRENT_THREAD_TAG);
        let (payload, mut witness) =
            witnessed_payload(&expected, "the current-thread shared payload");
        let sibling = payload.clone();
        let offered = payload.clone();
        drop(payload);
        let refused = row
            .listener()
            .spawn_worker("current-thread-shared-send", move || {
                current_thread_send(&sender, offered)
            });
        let refused = refused.take();
        assert!(
            matches!(refused, Err(RuntimeError::BlockingInAsyncContext)),
            "a current-thread shared send was allowed to wait: {refused:?}"
        );
        assert_released_leaving_the_sibling(
            &mut witness,
            sibling,
            &expected,
            "the current-thread refusal",
        )
        .await;
        row.listener().release(BEFORE_WRITE);
        assert_only_the_filled_frames_arrive(&mut row);
    })
    .await;
}

/// One shared waiting send made from a current-thread Tokio runtime.
///
/// The runtime is built inside the worker so the refusal is the sender's own
/// answer to its caller's flavour, rather than to whatever the row's task
/// happened to be running on.
fn current_thread_send(sender: &WsSender, payload: Bytes) -> Result<(), RuntimeError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("build the current-thread runtime");
    runtime.block_on(async move { sender.send_shared_binary(payload) })
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
fn assert_only_the_filled_frames_arrive(row: &mut SharedPayloadFixture) {
    assert_eq!(
        &*read_ws_text_frame(row.client(0).peer()),
        HELD_TEXT,
        "the released writer did not write the frame it was holding"
    );
    assert_eq!(
        &*read_ws_text_frame(row.client(0).peer()),
        FILLING_TEXT,
        "the released writer did not write the frame that filled its queue"
    );
    row.release_every_half();
    assert_no_further_payload(
        row.client(0).peer(),
        "a refused shared admission still reached the peer",
    );
}
