//! What a waiting WebSocket operation leaves behind when its future is dropped,
//! and how one timed receive spends its deadline.
//!
//! Every row runs against a real upgrade served through public `Router::ws`,
//! so the queues, the writer, and the receive owner under each claim are the
//! ones the production direct bridge builds. A pending operation is proved
//! pending by one poll in the case's own task, never by elapsed time, and what
//! it did or did not leave behind is read from the peer's wire in order, with a
//! sentinel as the last frame.

#![cfg(feature = "ws")]

use std::future::Future;
use std::pin::{Pin, pin};
use std::task::Poll;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::{WsCloseCause, WsConn, WsMessage, WsReceive, WsReceiver, WsSender};
use futures_util::FutureExt;
use tokio::time::Instant;

use crate::common::{
    AFTER_COMMIT, BEFORE_WRITE, BINARY, DIRECTION_DEADLINE, DirectionTestFixture, FILLING_TEXT,
    FrozenClock, HELD_TEXT, QUEUED, TEXT, TIMER_TICK, abortive_direction_row,
    assert_async_texts_in_order, assert_closed_with, assert_payload_bytes, assert_pending,
    assert_received_text, assert_receiver_drop_closes, async_direction_row, block_on_detached,
    bounded_receive, close_ws_peer, closed_cause, fill_outbound_behind_the_writer, lifecycle_event,
    on_ws_executors, witnessed_payload, write_async_ws_frame, write_async_ws_text_frame,
};

/// The frame whose arrival proves nothing cancelled was written before it.
const SENTINEL: &str = "the-sentinel";

/// What a failed write of a frame the row queues reports itself as.
const QUEUED_CONTEXT: &str = "a frame the row queues";

/// The shared payload a cancelled send offered.
const CANCELLED_SHARED: &[u8] = b"cancelled-shared-payload";

/// The deadline every paused-clock row gives its timed receive.
///
/// Long enough that the real time a row spends on loopback I/O between its
/// frozen steps cannot reach it: the bound on that I/O is
/// [`DIRECTION_DEADLINE`], and this is many times that.
const PAUSED_TIMEOUT: Duration = Duration::from_secs(60);

// 2.T3 — I4/I5/I10.
#[test]
fn async_send_cancellation_releases_payload_without_admission() {
    on_ws_executors(|| async {
        async_direction_row(1, |fixture, mut peer, connection| async move {
            let (sender, receiver) = connection.split();
            let sibling = sender.clone();
            fill_outbound_behind_the_writer(&fixture, &sender).await;
            assert!(
                matches!(
                    sender.try_send("refused-while-full"),
                    Err(RuntimeError::ChannelFull)
                ),
                "a live full queue did not refuse an immediate send as full"
            );
            cancel_pending(
                sender.send("cancelled-text"),
                "a text send against a full queue",
            )
            .await;
            let borrowed = *b"cancelled-borrowed-binary";
            cancel_pending(
                sender.send_binary(&borrowed),
                "a borrowed binary send against a full queue",
            )
            .await;
            cancel_shared_send(&sender).await;

            fixture.release(BEFORE_WRITE);
            lifecycle_event("the sentinel to be admitted", sibling.send(SENTINEL))
                .await
                .expect("a sibling sender was refused after the cancellations");
            assert_async_texts_in_order(&mut peer, &[HELD_TEXT, FILLING_TEXT, SENTINEL]).await;

            sender
                .send("reused-after-cancellation")
                .await
                .expect("the sender that cancelled three sends was refused");
            assert_async_texts_in_order(&mut peer, &["reused-after-cancellation"]).await;
            assert_terminal_sends_report_the_cause(&mut peer, receiver, &sender).await;
        })
        .await;
    });
}

/// Poll one waiting operation to `Pending`, then drop it.
///
/// The drop is this function's return: the operation is pinned on its frame
/// and nothing else ever polls it.
async fn cancel_pending<F>(operation: F, what: &str)
where
    F: Future,
    F::Output: std::fmt::Debug,
{
    let mut operation = pin!(operation);
    assert_pending(operation.as_mut(), what).await;
}

/// Drop a waiting shared send, and prove it let go of the handle it held.
///
/// The row's own sibling clone goes first, so the backing is live only through
/// the pending future: the witness reading live then is the future holding the
/// payload, and reading released after the drop is the future having let it
/// go rather than having handed it on.
async fn cancel_shared_send(sender: &WsSender) {
    let (payload, mut witness) =
        witnessed_payload(CANCELLED_SHARED, "the cancelled shared payload");
    let sibling = payload.clone();
    {
        let mut waiting = pin!(sender.send_shared_binary(payload));
        assert_pending(waiting.as_mut(), "a shared send against a full queue").await;
        assert_payload_bytes(
            &sibling,
            CANCELLED_SHARED,
            "the sibling clone of a pending send",
        );
        drop(sibling);
        witness.assert_live("the pending shared send");
    }
    witness.assert_released("the dropped shared send").await;
}

/// End the connection by dropping its receive owner, and require that both
/// send forms then report that cause rather than fullness.
async fn assert_terminal_sends_report_the_cause(
    peer: &mut tokio::net::TcpStream,
    receiver: WsReceiver,
    sender: &WsSender,
) {
    assert_receiver_drop_closes(peer, receiver, &[sender]).await;
    assert_eq!(
        closed_cause(sender.try_send("after"), "an immediate send past the end"),
        WsCloseCause::ReceiverDropped,
        "an immediate send past the end reported another cause"
    );
}

// 2.T4 — I5.
//
// Only the unfiltered facade receive is cancelled here. A filtered facade
// receive (`recv`, `recv_binary`, `recv_timeout`) that is dropped after it
// skipped a message of the other kind has consumed that message on purpose:
// the skip is the filter's answer, so cancellation safety for a filtered
// receive covers the next message of the kind it asked for, not the ones it
// already passed over.
#[test]
fn cancelled_receive_delivers_the_next_message_once() {
    on_ws_executors(|| async {
        async_direction_row(4, |fixture, mut peer, mut connection| async move {
            cancel_pending(
                connection.recv_message(),
                "an unfiltered facade receive on an empty queue",
            )
            .await;
            write_tagged_then_sentinel(&mut peer, "after-the-facade").await;
            for expected in ["after-the-facade", SENTINEL] {
                let received =
                    lifecycle_event("the facade receive", connection.recv_message()).await;
                assert!(
                    matches!(&received, Some(WsMessage::Text(text)) if text.as_ref() == expected),
                    "the facade receive took {received:?} instead of {expected:?}"
                );
            }

            let (_sender, mut receiver) = connection.split();
            cancel_pending(receiver.recv(), "a typed receive on an empty queue").await;
            write_tagged_then_sentinel(&mut peer, "after-the-typed").await;
            for expected in ["after-the-typed", SENTINEL] {
                let received = lifecycle_event("the typed receive", receiver.recv()).await;
                assert_received_text(
                    received.expect("the typed receive was refused"),
                    expected,
                    "the typed receive after a cancelled one",
                );
            }

            cancel_pending(
                receiver.recv_timeout(DIRECTION_DEADLINE),
                "a timed typed receive on an empty queue",
            )
            .await;
            write_tagged_then_sentinel(&mut peer, "after-the-timed").await;
            for expected in ["after-the-timed", SENTINEL] {
                assert_received_text(
                    bounded_receive(&mut receiver, "the timed receive after a cancelled one").await,
                    expected,
                    "the timed receive after a cancelled one",
                );
            }

            woken_receive_takes_its_message_or_none(&fixture, &mut peer, &mut receiver).await;
        })
        .await;
    });
}

/// A typed receive woken by its message answers with it, or leaves it queued.
///
/// The edge holds the inbound pump with the tagged message queued and the
/// sentinel not yet read, so the second poll runs with exactly one message
/// available. A receive that takes nothing until the poll that returns is free
/// to answer either way. One that dequeued the message and went back to
/// waiting loses it when dropped, and the sentinel arrives in its place.
async fn woken_receive_takes_its_message_or_none(
    fixture: &DirectionTestFixture,
    peer: &mut tokio::net::TcpStream,
    receiver: &mut WsReceiver,
) {
    let remaining: &[&str] = {
        let mut receiving = pin!(receiver.recv());
        assert_pending(receiving.as_mut(), "a typed receive on an empty queue").await;
        fixture.arm(QUEUED);
        write_tagged_then_sentinel(peer, "after-the-wake").await;
        fixture.wait_paused(QUEUED).await;
        match futures_util::poll!(receiving.as_mut()) {
            Poll::Ready(received) => {
                assert_received_text(
                    received.expect("the woken typed receive was refused"),
                    "after-the-wake",
                    "the woken typed receive",
                );
                &[SENTINEL]
            }
            Poll::Pending => &["after-the-wake", SENTINEL],
        }
    };
    fixture.release(QUEUED);
    for expected in remaining {
        assert_received_text(
            bounded_receive(receiver, "the typed receive after a woken one").await,
            expected,
            "the typed receive after a woken one",
        );
    }
}

/// The peer's tagged message, then the sentinel that proves it came once.
async fn write_tagged_then_sentinel(peer: &mut tokio::net::TcpStream, tagged: &str) {
    write_async_ws_text_frame(peer, tagged).await;
    write_async_ws_text_frame(peer, SENTINEL).await;
}

// 2.T4 — I6.
//
// Each row owns a current-thread runtime, so its clock can be frozen and a
// cancelled server in one cannot decide another. Peers are async: a blocking
// read here would stop the only thread the bridge runs on.
#[test]
fn receive_timeout_spends_one_deadline_without_closing() {
    block_on_detached(typed_timeout_answers_ready_first_and_expires_once());
    block_on_detached(facade_filtering_keeps_the_first_deadline());
    on_ws_executors(|| async {
        live_timeouts_leave_receivers_reusable().await;
        discarding_cause_answers_before_the_clock().await;
    });
}

/// Exercise timer wakeups and receiver reuse on both live executor shapes.
async fn live_timeouts_leave_receivers_reusable() {
    async_direction_row(4, |_fixture, mut peer, mut connection| async move {
        write_async_ws_frame(&mut peer, BINARY, b"filtered", "the skipped binary").await;
        assert!(matches!(
            lifecycle_event(
                "the filtered timeout",
                connection.recv_timeout(Duration::from_millis(20))
            )
            .await,
            Err(RuntimeError::Timeout)
        ));
        write_async_ws_text_frame(&mut peer, "after-facade-timeout").await;
        assert_eq!(
            lifecycle_event("the facade after timeout", connection.recv())
                .await
                .as_deref(),
            Some("after-facade-timeout")
        );
        let (sender, mut receiver) = connection.split();
        assert!(matches!(
            lifecycle_event(
                "the typed timeout",
                receiver.recv_timeout(Duration::from_millis(20))
            )
            .await,
            Err(RuntimeError::Timeout)
        ));
        write_async_ws_text_frame(&mut peer, "after-typed-timeout").await;
        assert_received_text(
            bounded_receive(&mut receiver, "the receiver after timeout").await,
            "after-typed-timeout",
            "the receiver stayed live",
        );
        drop(sender);
    })
    .await;
}

/// A typed timed receive answers what is ready before it reads the clock,
/// refuses only a wait it has no clock for, expires once, and leaves the
/// connection live.
async fn typed_timeout_answers_ready_first_and_expires_once() {
    abortive_direction_row(4, |fixture, mut peer, connection| async move {
        let (_sender, mut receiver) = connection.split();
        assert!(
            matches!(
                zero_duration(&mut receiver).await,
                Err(RuntimeError::Timeout)
            ),
            "a zero-duration receive on a live empty queue did not time out"
        );
        assert!(
            matches!(clockless(&mut receiver), Some(Err(RuntimeError::NoRuntime))),
            "a receive that had to wait with no runtime was not refused as NoRuntime"
        );

        fixture
            .queue_from_async_peer(&mut peer, TEXT, b"queued-for-zero", QUEUED_CONTEXT)
            .await;
        assert_received_text(
            zero_duration(&mut receiver)
                .await
                .expect("a zero-duration receive refused a queued message"),
            "queued-for-zero",
            "a zero-duration receive over a queued message",
        );
        fixture
            .queue_from_async_peer(&mut peer, TEXT, b"queued-off-runtime", QUEUED_CONTEXT)
            .await;
        assert_received_text(
            ready_off_runtime(&mut receiver, "a queued message"),
            "queued-off-runtime",
            "a receive with no runtime over a queued message",
        );

        assert_typed_receive_expires_once(&mut receiver).await;
        write_async_ws_text_frame(&mut peer, "after-expiry").await;
        assert_received_text(
            bounded_receive(&mut receiver, "the receive after an expiry").await,
            "after-expiry",
            "the receive after an expiry",
        );

        close_ws_peer(&mut peer, "the peer's close").await;
        let ended = lifecycle_event("the receive owner to see the close", receiver.recv()).await;
        assert_closed_with(
            ended.expect("an untimed receive was refused"),
            WsCloseCause::PeerClosed,
            "the untimed receive over a peer close",
        );
        assert_closed_with(
            zero_duration(&mut receiver)
                .await
                .expect("a zero-duration receive refused an ended connection"),
            WsCloseCause::PeerClosed,
            "a zero-duration receive over an ended connection",
        );
        assert_closed_with(
            ready_off_runtime(&mut receiver, "an ended connection"),
            WsCloseCause::PeerClosed,
            "a receive with no runtime over an ended connection",
        );
    })
    .await;
}

/// One zero-duration typed receive, under the suite's bound.
async fn zero_duration(receiver: &mut WsReceiver) -> Result<WsReceive, RuntimeError> {
    lifecycle_event(
        "a zero-duration receive",
        receiver.recv_timeout(Duration::ZERO),
    )
    .await
}

/// Run `poll_once` on a thread with no runtime entered.
///
/// A scoped plain thread rather than the case's own: the case runs inside a
/// runtime, and the claim is about a caller that has none. The receive is built
/// and polled on that thread, and one poll is all it gets, so the thread's join
/// is immediate.
fn off_runtime<T: Send>(poll_once: impl FnOnce() -> T + Send) -> T {
    std::thread::scope(|scope| {
        scope
            .spawn(poll_once)
            .join()
            .unwrap_or_else(|unwound| std::panic::resume_unwind(unwound))
    })
}

/// One typed timed receive polled once with no runtime entered.
fn clockless(receiver: &mut WsReceiver) -> Option<Result<WsReceive, RuntimeError>> {
    off_runtime(|| receiver.recv_timeout(DIRECTION_DEADLINE).now_or_never())
}

/// One facade timed receive polled once with no runtime entered.
fn facade_clockless(connection: &mut WsConn) -> Option<Result<Option<Box<str>>, RuntimeError>> {
    off_runtime(|| connection.recv_timeout(DIRECTION_DEADLINE).now_or_never())
}

/// What a receive with no runtime answered, when it could answer at once.
fn ready_off_runtime(receiver: &mut WsReceiver, what: &str) -> WsReceive {
    match clockless(receiver) {
        Some(Ok(received)) => received,
        other => panic!("a receive with no runtime over {what} answered {other:?}"),
    }
}

/// A timed typed receive on a live empty queue is still waiting one tick
/// before its deadline, and times out one tick after it.
async fn assert_typed_receive_expires_once(receiver: &mut WsReceiver) {
    let _frozen = FrozenClock::freeze();
    let started = Instant::now();
    let mut receiving = pin!(receiver.recv_timeout(PAUSED_TIMEOUT));
    assert_pending(receiving.as_mut(), "a timed receive on a live empty queue").await;
    assert_expires_at(
        receiving.as_mut(),
        started + PAUSED_TIMEOUT,
        "the typed receive",
    )
    .await;
}

/// Step a frozen clock across `deadline` and require that `receiving` expires
/// exactly there.
///
/// Every step is taken only after the receive has been seen pending, so the
/// clock never moves while the receive could still be answering something.
async fn assert_expires_at<F, T>(mut receiving: Pin<&mut F>, deadline: Instant, what: &str)
where
    F: Future<Output = Result<T, RuntimeError>>,
    T: std::fmt::Debug,
{
    let remaining = deadline.saturating_duration_since(Instant::now());
    assert!(
        remaining > TIMER_TICK,
        "{what}: the row reached the deadline before stepping onto it"
    );
    tokio::time::advance(remaining - TIMER_TICK).await;
    assert_pending(receiving.as_mut(), &format!("{what} one tick early")).await;
    tokio::time::advance(TIMER_TICK * 2).await;
    match futures_util::poll!(receiving) {
        Poll::Ready(Err(RuntimeError::Timeout)) => {}
        other => panic!("{what} answered {other:?} one tick after its deadline"),
    }
}

/// A filtered facade receive spends the deadline its first poll fixed, however
/// many skipped messages arrive before it.
///
/// The skips land half a deadline in, so a loop that restarted the deadline
/// per message would still be waiting where the first one expires.
async fn facade_filtering_keeps_the_first_deadline() {
    abortive_direction_row(4, |fixture, mut peer, mut connection| async move {
        assert_filtered_receive_expires_once(&fixture, &mut peer, &mut connection).await;
        write_async_ws_text_frame(&mut peer, "after-expiry").await;
        assert_eq!(
            lifecycle_event("the facade receive after expiry", connection.recv())
                .await
                .as_deref(),
            Some("after-expiry"),
            "the facade connection did not stay live past its expiry"
        );
    })
    .await;
}

/// The filtered receive, its two skipped binary messages, and its one expiry.
async fn assert_filtered_receive_expires_once(
    fixture: &DirectionTestFixture,
    peer: &mut tokio::net::TcpStream,
    connection: &mut WsConn,
) {
    let frozen = FrozenClock::freeze();
    let started = Instant::now();
    let mut receiving = pin!(connection.recv_timeout(PAUSED_TIMEOUT));
    assert_pending(receiving.as_mut(), "a filtered receive on an empty queue").await;
    tokio::time::advance(PAUSED_TIMEOUT / 2).await;
    // Real time for the loopback writes: a frozen runtime that idled on the
    // socket would advance itself onto the receive's deadline.
    drop(frozen);
    fixture
        .queue_from_async_peer(peer, BINARY, b"skipped-first", QUEUED_CONTEXT)
        .await;
    fixture
        .queue_from_async_peer(peer, BINARY, b"skipped-second", QUEUED_CONTEXT)
        .await;
    assert_pending(
        receiving.as_mut(),
        "a filtered receive past two binary messages",
    )
    .await;
    let _frozen = FrozenClock::freeze();
    assert_expires_at(
        receiving.as_mut(),
        started + PAUSED_TIMEOUT,
        "the filtered receive",
    )
    .await;
}

/// A cause that discards the queue answers before the clock is asked, and
/// before the message it discarded.
///
/// The receive is made with no runtime at all and with a message queued, so a
/// receive that asked the clock first would answer `NoRuntime`, and one that
/// asked the queue first would hand out the discarded message.
async fn discarding_cause_answers_before_the_clock() {
    abortive_direction_row(4, |fixture, mut peer, connection| async move {
        let (_sender, mut receiver) = connection.split();
        fixture
            .queue_from_async_peer(
                &mut peer,
                TEXT,
                b"discarded-by-cancellation",
                QUEUED_CONTEXT,
            )
            .await;
        fixture.select_server_cancellation().await;
        assert_closed_with(
            ready_off_runtime(&mut receiver, "a cancelled connection"),
            WsCloseCause::ServerCancelled,
            "a receive with no runtime over a cancelled connection",
        );
        assert_closed_with(
            zero_duration(&mut receiver)
                .await
                .expect("a zero-duration receive refused a cancelled connection"),
            WsCloseCause::ServerCancelled,
            "a zero-duration receive over a cancelled connection",
        );
        fixture.release(AFTER_COMMIT);
    })
    .await;
}

// 2.T4 — I6, through the compatibility facade.
//
// The facade fixes its deadline the same way the typed receive does, so a
// caller with no runtime gets one poll there too. The filtered loop runs inside
// that one poll: a skipped message does not cost the caller its answer.
#[test]
fn facade_receive_timeout_off_runtime_answers_only_what_is_ready() {
    on_ws_executors(|| async {
        abortive_direction_row(4, |fixture, mut peer, mut connection| async move {
            fixture
                .queue_from_async_peer(&mut peer, BINARY, b"skipped-off-runtime", QUEUED_CONTEXT)
                .await;
            fixture
                .queue_from_async_peer(&mut peer, TEXT, b"taken-off-runtime", QUEUED_CONTEXT)
                .await;
            let ready = facade_clockless(&mut connection);
            assert!(
                matches!(&ready, Some(Ok(Some(text))) if text.as_ref() == "taken-off-runtime"),
                "a facade receive with no runtime over a skipped binary \
                 and a queued text answered {ready:?}"
            );
            let waiting = facade_clockless(&mut connection);
            assert!(
                matches!(waiting, Some(Err(RuntimeError::NoRuntime))),
                "a facade receive that had to wait with no runtime answered {waiting:?}"
            );

            close_ws_peer(&mut peer, "the peer's close").await;
            assert_eq!(
                lifecycle_event("the facade to see the close", connection.recv()).await,
                None,
                "the facade's untimed receive did not end on the peer's close"
            );
            let ended = facade_clockless(&mut connection);
            assert!(
                matches!(ended, Some(Ok(None))),
                "a facade receive with no runtime over an ended connection answered {ended:?}"
            );
        })
        .await;
    });
}
