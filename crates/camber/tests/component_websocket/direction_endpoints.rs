//! What one direct WebSocket's two public endpoints are, and what each of their
//! operations answers.
//!
//! Every row here runs against a real upgrade served through public
//! `Router::ws`, so the values under test are the ones the production direct
//! bridge builds and hands to a callback — not a pair of channels a test made.

#![cfg(feature = "ws")]

use std::marker::PhantomData;
use std::pin::pin;
use std::time::Duration;

use crate::common::{
    AFTER_COMMIT, BEFORE_WRITE, BINARY, DIRECTION_DEADLINE, FILLING_TEXT, FacadeReceives,
    HELD_TEXT, PING, PONG, assert_async_texts_in_order, assert_broken_pipe, assert_closed_with,
    assert_pending, assert_received_binary, assert_received_text, assert_receiver_drop_closes,
    async_direction_row, async_host_direction_row, bounded_receive, close_ws_peer, closed_cause,
    closed_receive_cause, expect_async_text, fill_outbound_behind_the_writer, lifecycle_event,
    on_ws_executors, read_async_ws_binary_frame, read_async_ws_frame, read_async_ws_text_frame,
    write_async_ws_frame, write_async_ws_text_frame,
};
use camber::RuntimeError;
use camber::http::{WsCloseCause, WsMessage, WsReceiver, WsSender};

/// A type-level question asked at run time: is `T` `Clone`?
///
/// The compiler answers it. Two blanket implementations both name this probe:
/// one accepts every `T`, and one accepts only `Clone` types and sits one
/// autoref step earlier in method resolution. So the answer is the `Clone`
/// implementation exactly when the compiler can select it, and the other
/// otherwise. A `WsReceiver` that gained a `Clone` implementation would start
/// answering `true` here and fail the row below, which no `assert!` about
/// values could ever notice.
struct CloneProbe<T>(PhantomData<T>);

impl<T> CloneProbe<T> {
    const fn new() -> Self {
        Self(PhantomData)
    }
}

trait ProbeByClone {
    fn is_clone(&self) -> bool {
        true
    }
}

impl<T: Clone> ProbeByClone for CloneProbe<T> {}

trait ProbeByAny {
    fn is_clone(&self) -> bool {
        false
    }
}

impl<T> ProbeByAny for &CloneProbe<T> {}

/// Compile-time proof that `T` can be cloned and shared across threads.
fn requires_clone_send_sync<T: Clone + Send + Sync>() {}

/// Compile-time proof that `T` can move to another thread.
fn requires_send<T: Send>() {}

// 1.T1.
#[test]
fn split_exposes_one_receiver_and_cloneable_senders() {
    on_ws_executors(|| async {
        requires_clone_send_sync::<WsSender>();
        requires_send::<WsReceiver>();
        assert!(
            CloneProbe::<WsSender>::new().is_clone(),
            "WsSender stopped being cloneable, so no send capability can fan out"
        );
        assert!(
            !(&CloneProbe::<WsReceiver>::new()).is_clone(),
            "WsReceiver gained a Clone implementation, so a connection can have two receive owners"
        );

        async_direction_row(8, |fixture, mut peer, connection| async move {
            let (sender, mut receiver) = connection.split();
            let sent: Box<[_]> = (0..3)
                .map(|index| {
                    let clone = sender.clone();
                    fixture.spawn_worker(&format!("sender-clone-{index}"), async move {
                        clone.send(&format!("from-clone-{index}")).await
                    })
                })
                .collect();
            for report in sent {
                report.take().await.expect("a sender clone was refused");
            }

            let mut seen = Vec::new();
            for _ in 0..3 {
                seen.push(read_async_ws_text_frame(&mut peer).await);
            }
            seen.sort();
            assert_eq!(
                &*seen.iter().map(AsRef::as_ref).collect::<Box<[&str]>>(),
                ["from-clone-0", "from-clone-1", "from-clone-2"].as_slice(),
                "three sender clones did not reach one outbound queue"
            );

            write_async_ws_text_frame(&mut peer, "to-the-one-owner").await;
            assert_received_text(
                bounded_receive(&mut receiver, "the sole receive owner").await,
                "to-the-one-owner",
                "the sole receive owner",
            );
        })
        .await;
    });
}

// 1.T2, configured-capacity rows.
#[test]
fn direction_queues_use_configured_capacity_and_typed_send_results() {
    on_ws_executors(|| async {
        assert_zero_normalizes_through_router().await;
        assert_zero_normalizes_through_host_router().await;
        assert_live_full_and_terminal_send_results().await;
        assert_control_frames_stay_on_the_transport().await;
    });
}

/// `Router::ws_buffer_size(0)` reaches both production queues as one.
async fn assert_zero_normalizes_through_router() {
    async_direction_row(0, |fixture, mut peer, connection| async move {
        let observed = fixture.observed();
        assert_eq!(
            (observed.outbound_capacity, observed.inbound_capacity),
            (1, 1),
            "Router::ws_buffer_size(0) did not normalize both production queues to one"
        );
        assert_text_exchange(&mut peer, &connection.sender(), "router-zero").await;
    })
    .await;
}

/// `HostRouter::ws_buffer_size(0)` does the same through its own path.
async fn assert_zero_normalizes_through_host_router() {
    async_host_direction_row(0, |fixture, mut peer, connection| async move {
        let observed = fixture.observed();
        assert_eq!(
            (observed.outbound_capacity, observed.inbound_capacity),
            (1, 1),
            "HostRouter::ws_buffer_size(0) did not normalize both production queues to one"
        );
        assert_text_exchange(&mut peer, &connection.sender(), "host-router-zero").await;
    })
    .await;
}

/// One text frame out and the same text back, so a normalized capacity is
/// proved to still carry traffic rather than only to read as one.
async fn assert_text_exchange(peer: &mut tokio::net::TcpStream, sender: &WsSender, row: &str) {
    sender
        .send(row)
        .await
        .expect("a normalized queue refused a send");
    expect_async_text(peer, row, "a normalized queue's frame").await;
}

/// A live full queue answers `ChannelFull`; a terminal one answers
/// `WebSocketClosed` with the fixed cause.
async fn assert_live_full_and_terminal_send_results() {
    async_direction_row(1, |fixture, peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        fill_outbound_behind_the_writer(&fixture, &sender).await;
        assert!(
            matches!(sender.try_send("refused"), Err(RuntimeError::ChannelFull)),
            "a live full outbound queue did not refuse a text try_send as full"
        );
        assert!(
            matches!(
                sender.try_send_binary(b"refused"),
                Err(RuntimeError::ChannelFull)
            ),
            "a live full outbound queue did not refuse a binary try_send as full"
        );

        fixture.release(BEFORE_WRITE);
        drop(peer);
        let cause = closed_receive_cause(
            bounded_receive(&mut receiver, "a disconnected peer's receive owner").await,
            "a disconnected peer's receive owner",
        );
        assert_eq!(
            closed_cause(sender.try_send("after"), "a terminal text try_send"),
            cause,
            "a terminal connection reported a text try_send as merely full"
        );
        assert_eq!(
            closed_cause(
                sender.try_send_binary(b"after"),
                "a terminal binary try_send"
            ),
            cause,
            "a terminal connection reported a binary try_send as merely full"
        );
    })
    .await;
}

/// Ping and pong stay on the transport; text and binary reach the receive
/// owner in the order the peer sent them.
async fn assert_control_frames_stay_on_the_transport() {
    async_direction_row(1, |_fixture, mut peer, connection| async move {
        let (_sender, mut receiver) = connection.split();
        write_async_ws_frame(&mut peer, PING, b"keepalive", "the peer's ping").await;
        write_async_ws_text_frame(&mut peer, "application-text").await;
        write_async_ws_frame(
            &mut peer,
            PONG,
            b"unsolicited",
            "the peer's unsolicited pong",
        )
        .await;
        write_async_ws_frame(&mut peer, BINARY, b"\x00\xff\x10", "the peer's binary").await;

        let (opcode, payload) = read_async_ws_frame(&mut peer).await;
        assert_eq!(
            (opcode, payload.as_ref()),
            (PONG, b"keepalive".as_slice()),
            "the transport did not answer the ping itself"
        );
        assert_received_text(
            bounded_receive(&mut receiver, "the receive owner past a control frame").await,
            "application-text",
            "a control frame displaced the application text",
        );
        assert_received_binary(
            bounded_receive(&mut receiver, "the timed receive").await,
            b"\x00\xff\x10",
            "the timed receive",
        );

        close_ws_peer(&mut peer, "the peer's close").await;
        assert_closed_with(
            bounded_receive(&mut receiver, "the receive owner after a peer close").await,
            WsCloseCause::PeerClosed,
            "a peer close",
        );
    })
    .await;
}

// 1.T3.
#[test]
fn direction_waiting_operations_suspend_until_they_can_finish() {
    on_ws_executors(|| async {
        assert_send_waits_for_capacity().await;
        assert_receive_waits_for_a_frame().await;
    });
}

/// A send against a full queue waits, and completes once capacity is
/// released.
///
/// One poll in the case's own task proves the wait. The same pinned send is
/// then driven to its admission, so the frame the peer reads last is the one
/// that waited.
async fn assert_send_waits_for_capacity() {
    async_direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, _receiver) = connection.split();
        fill_outbound_behind_the_writer(&fixture, &sender).await;
        let mut waiting = pin!(sender.send("waited"));
        assert_pending(waiting.as_mut(), "a send against a full queue").await;
        fixture.release(BEFORE_WRITE);
        lifecycle_event("the released send to be admitted", waiting)
            .await
            .expect("a released send failed");
        assert_async_texts_in_order(&mut peer, &[HELD_TEXT, FILLING_TEXT, "waited"]).await;
    })
    .await;
}

/// A receive on an empty queue waits, and answers once the peer sends
/// something.
async fn assert_receive_waits_for_a_frame() {
    async_direction_row(1, |_fixture, mut peer, connection| async move {
        let (_sender, mut receiver) = connection.split();
        let mut waiting = pin!(receiver.recv());
        assert_pending(waiting.as_mut(), "a receive on an empty queue").await;
        write_async_ws_text_frame(&mut peer, "waited-for").await;
        assert_received_text(
            lifecycle_event("the waiting receive to answer", waiting)
                .await
                .expect("a waiting receive failed"),
            "waited-for",
            "the waiting receive",
        );
    })
    .await;
}

// 1.T4.
#[test]
fn legacy_wsconn_methods_preserve_close_and_borrowed_binary_contract() {
    on_ws_executors(|| async {
        assert_facade_signatures_and_sender_delegation().await;
        assert_closed_facade_sends_report_broken_pipe().await;
        assert_borrowed_binary_is_copied_at_admission().await;
    });
}

/// Every shipped `WsConn` method still compiles and runs, and `sender()` hands
/// back an independent handle without consuming the facade.
async fn assert_facade_signatures_and_sender_delegation() {
    async_direction_row(8, |_fixture, mut peer, connection| async move {
        let sender = connection.sender();
        write_async_ws_text_frame(&mut peer, "to-the-facade").await;
        write_ws_binary_frame_pair(&mut peer).await;
        let taken = lifecycle_event(
            "the three facade receives",
            FacadeReceives::take(connection),
        )
        .await;
        let mut connection = taken.connection;
        assert_eq!(
            taken.text.as_deref(),
            Some("to-the-facade"),
            "WsConn::recv stopped taking the peer's text"
        );
        assert_eq!(
            taken.binary.as_deref(),
            Some(b"only-binary".as_slice()),
            "WsConn::recv_binary stopped skipping text"
        );
        assert!(
            matches!(&taken.either, Some(WsMessage::Text(text)) if text.as_ref() == "either-kind"),
            "WsConn::recv_message stopped taking either payload kind: {:?}",
            taken.either
        );

        sender
            .send("from-the-independent-handle")
            .await
            .expect("send");
        sender.try_send("try-text").expect("try_send");
        sender.send_binary(b"binary").await.expect("send_binary");
        sender
            .try_send_binary(b"try-binary")
            .expect("try_send_binary");
        connection
            .send("from-the-facade")
            .await
            .expect("facade send");
        connection
            .send_binary(b"facade-binary")
            .await
            .expect("facade send_binary");
        expect_async_text(
            &mut peer,
            "from-the-independent-handle",
            "the independent sender's frame on the bridge's own outbound queue",
        )
        .await;
        expect_async_text(&mut peer, "try-text", "the independent sender's try_send").await;
        assert_eq!(
            read_async_ws_binary_frame(&mut peer).await.as_ref(),
            b"binary"
        );
        assert_eq!(
            read_async_ws_binary_frame(&mut peer).await.as_ref(),
            b"try-binary"
        );
        expect_async_text(&mut peer, "from-the-facade", "the facade's send").await;
        assert_eq!(
            read_async_ws_binary_frame(&mut peer).await.as_ref(),
            b"facade-binary"
        );

        assert!(
            matches!(
                connection.recv_timeout(Duration::from_millis(20)).await,
                Err(RuntimeError::Timeout)
            ),
            "WsConn::recv_timeout stopped bounding a silent peer"
        );
    })
    .await;
}

/// A text frame the binary receiver must skip, then the two payloads the two
/// remaining facade receivers take.
async fn write_ws_binary_frame_pair(peer: &mut tokio::net::TcpStream) {
    write_async_ws_text_frame(peer, "skipped-by-recv-binary").await;
    write_async_ws_frame(peer, BINARY, b"only-binary", "the facade's binary").await;
    write_async_ws_text_frame(peer, "either-kind").await;
}

/// Through the facade, a closed connection is still a broken pipe.
///
/// The `None` is read against the cause that produced it. `WsConn::recv` maps
/// every refusal to `None` — a missing runtime, anything a later variant adds
/// — so the row commits the peer's close first and names
/// it, or the absence it asserts could be any of them.
async fn assert_closed_facade_sends_report_broken_pipe() {
    async_direction_row(4, |fixture, mut peer, mut connection| async move {
        fixture.arm(AFTER_COMMIT);
        close_ws_peer(&mut peer, "the facade peer's close").await;
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_eq!(
            fixture.observed().terminal,
            Some(WsCloseCause::PeerClosed),
            "the facade's connection ended for a reason other than the peer's close"
        );
        fixture.release(AFTER_COMMIT);
        assert_eq!(
            connection
                .recv_timeout(DIRECTION_DEADLINE)
                .await
                .expect("the facade's timed receive was refused"),
            None,
            "the facade did not map a closed connection back to None"
        );
        assert_broken_pipe(
            connection.send("after close").await,
            "a closed facade text send",
        );
        assert_broken_pipe(
            connection.send_binary(b"after close").await,
            "a closed facade binary send",
        );
    })
    .await;
}

/// A borrowed binary payload is copied when it is admitted, so mutating the
/// caller's buffer afterwards cannot change what the peer receives.
async fn assert_borrowed_binary_is_copied_at_admission() {
    async_direction_row(2, |fixture, mut peer, connection| async move {
        let mut payload = *b"admitted";
        fixture.arm(BEFORE_WRITE);
        connection
            .send_binary(&payload)
            .await
            .expect("a borrowed binary send was refused");
        fixture.wait_paused(BEFORE_WRITE).await;
        payload.copy_from_slice(b"mutated!");
        fixture.release(BEFORE_WRITE);
        assert_eq!(
            read_async_ws_binary_frame(&mut peer).await.as_ref(),
            b"admitted",
            "a borrowed binary send retained the caller's buffer instead of copying it"
        );
    })
    .await;
}

// 1.T5.
#[test]
fn direction_half_drop_and_terminal_cause_are_shared() {
    on_ws_executors(|| async {
        assert_one_clone_drop_keeps_the_connection_live().await;
        assert_last_sender_drop_closes_the_receiver().await;
        assert_receiver_drop_closes_every_sender().await;
        assert_peer_close_fixes_one_cause_for_every_half().await;
    });
}

/// Dropping one sender clone changes nothing.
async fn assert_one_clone_drop_keeps_the_connection_live() {
    async_direction_row(4, |_fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        drop(sender.clone());
        sender
            .send("still-live")
            .await
            .expect("a surviving sender was refused");
        expect_async_text(&mut peer, "still-live", "a surviving sender's frame").await;
        write_async_ws_text_frame(&mut peer, "still-receiving").await;
        assert_received_text(
            bounded_receive(&mut receiver, "a surviving receive owner").await,
            "still-receiving",
            "dropping one sender clone disturbed the receive owner",
        );
    })
    .await;
}

/// Dropping the last sender closes send admission and settles the receiver on
/// `SendersDropped`.
async fn assert_last_sender_drop_closes_the_receiver() {
    async_direction_row(4, |fixture, peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        drop(sender);
        assert_closed_with(
            bounded_receive(
                &mut receiver,
                "the receive owner after the last sender's drop",
            )
            .await,
            WsCloseCause::SendersDropped,
            "the last sender's drop",
        );
        assert_eq!(
            fixture.observed().terminal,
            Some(WsCloseCause::SendersDropped),
            "the bridge published a different cause than the receiver observed"
        );
        drop(peer);
    })
    .await;
}

/// Dropping the receive owner ends the connection, and every retained sender
/// reads `ReceiverDropped`.
async fn assert_receiver_drop_closes_every_sender() {
    async_direction_row(4, |_fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        let second = sender.clone();
        assert_receiver_drop_closes(&mut peer, receiver, &[&sender, &second]).await;
    })
    .await;
}

/// A peer close fixes one cause, and a later competing local drop cannot
/// rewrite what the surviving halves read.
async fn assert_peer_close_fixes_one_cause_for_every_half() {
    async_direction_row(4, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        let second = sender.clone();
        close_ws_peer(&mut peer, "the peer's close").await;
        assert_closed_with(
            bounded_receive(&mut receiver, "the receive owner after a peer close").await,
            WsCloseCause::PeerClosed,
            "a peer close reaching the receive owner",
        );
        drop(receiver);
        assert_eq!(
            closed_cause(sender.send("after").await, "a send after the peer closed"),
            WsCloseCause::PeerClosed,
            "a later local drop rewrote the committed cause"
        );
        assert_eq!(
            closed_cause(
                second.send("after").await,
                "a clone's send after the peer closed"
            ),
            WsCloseCause::PeerClosed,
            "two sender clones read different causes for one connection"
        );
        assert_eq!(
            fixture.observed().terminal,
            Some(WsCloseCause::PeerClosed),
            "the bridge published a different cause than its endpoints observed"
        );
    })
    .await;
}
