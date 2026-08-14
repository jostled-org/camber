//! What one direct WebSocket's two public endpoints are, and what each of their
//! operations answers.
//!
//! Every row here runs against a real upgrade served through public
//! `Router::ws`, so the values under test are the ones the production direct
//! bridge builds and hands to a callback — not a pair of channels a test made.

#![cfg(feature = "ws")]

use std::marker::PhantomData;
use std::time::Duration;

use crate::common::{
    BEFORE_WRITE, CLOSE, DIRECTION_DEADLINE, assert_broken_pipe, assert_closed_with,
    assert_received_text, closed_cause, direction_row, fill_outbound_behind_the_writer,
    host_direction_row, read_ws_binary_frame, read_ws_frame_raw, read_ws_text_frame, receive_once,
    received_message, write_masked_frame, write_ws_close_frame, write_ws_text_frame,
};
use camber::RuntimeError;
use camber::http::mock::LifecycleCheckpoint;
use camber::http::{WsCloseCause, WsMessage, WsReceive, WsReceiver, WsSender};

/// The checkpoint that holds the coordinator once its cause is fixed.
const SELECTED: LifecycleCheckpoint = LifecycleCheckpoint::WebSocketTerminalSelected;

/// The WebSocket ping and pong opcodes.
const PING: u8 = 0x09;
const PONG: u8 = 0x0a;

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

fn assert_received_binary(received: WsReceive, expected: &[u8], what: &str) {
    match received_message(received, what) {
        WsMessage::Binary(data) => assert_eq!(data.as_ref(), expected, "{what}"),
        WsMessage::Text(text) => panic!("{what} took text {text:?} instead of {expected:?}"),
    }
}

/// One bounded receive, so no row here waits on a frame that is never coming.
///
/// `WsReceiver::recv` has no deadline, and every call below runs on the case's
/// own task: a regression that stops delivering a message or a terminal cause
/// would park the case forever, and the whole harness with it, instead of
/// failing it.
fn receive(receiver: &mut WsReceiver, what: &str) -> WsReceive {
    receiver
        .recv_timeout(DIRECTION_DEADLINE)
        .unwrap_or_else(|error| panic!("{what} was refused: {error}"))
}

/// The cause a closed receive settled on, or a failure naming what it took
/// instead.
fn closed_receive_cause(received: WsReceive, what: &str) -> WsCloseCause {
    match received {
        WsReceive::Closed(cause) => cause,
        WsReceive::Message(message) => panic!("{what} took {message:?} instead of closing"),
    }
}

// 1.T1.
#[camber::test]
async fn split_exposes_one_receiver_and_cloneable_senders() {
    requires_clone_send_sync::<WsSender>();
    requires_send::<WsReceiver>();
    assert!(
        (&CloneProbe::<WsSender>::new()).is_clone(),
        "WsSender stopped being cloneable, so no send capability can fan out"
    );
    assert!(
        !(&CloneProbe::<WsReceiver>::new()).is_clone(),
        "WsReceiver gained a Clone implementation, so a connection can have two receive owners"
    );

    direction_row(8, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        let sent: Box<[_]> = (0..3)
            .map(|index| {
                let clone = sender.clone();
                fixture.spawn_worker(&format!("sender-clone-{index}"), move || {
                    clone.send(&format!("from-clone-{index}"))
                })
            })
            .collect();
        sent.iter()
            .for_each(|report| report.take().expect("a sender clone was refused"));

        let mut seen: Vec<Box<str>> = (0..3).map(|_| read_ws_text_frame(&mut peer)).collect();
        seen.sort();
        assert_eq!(
            &*seen.iter().map(AsRef::as_ref).collect::<Box<[&str]>>(),
            ["from-clone-0", "from-clone-1", "from-clone-2"].as_slice(),
            "three sender clones did not reach one outbound queue"
        );

        write_ws_text_frame(&mut peer, "to-the-one-owner");
        assert_received_text(
            receive(&mut receiver, "the sole receive owner"),
            "to-the-one-owner",
            "the sole receive owner",
        );
    })
    .await;
}

// 1.T2, configured-capacity rows.
#[camber::test]
async fn direction_queues_use_configured_capacity_and_typed_send_results() {
    assert_zero_normalizes_through_router().await;
    assert_zero_normalizes_through_host_router().await;
    assert_live_full_and_terminal_send_results().await;
    assert_control_frames_stay_on_the_transport().await;
}

/// `Router::ws_buffer_size(0)` reaches both production queues as one.
async fn assert_zero_normalizes_through_router() {
    direction_row(0, |fixture, mut peer, connection| async move {
        let observed = fixture.observed();
        assert_eq!(
            (observed.outbound_capacity, observed.inbound_capacity),
            (1, 1),
            "Router::ws_buffer_size(0) did not normalize both production queues to one"
        );
        assert_text_exchange(&mut peer, &connection.sender(), "router-zero");
    })
    .await;
}

/// `HostRouter::ws_buffer_size(0)` does the same through its own path.
async fn assert_zero_normalizes_through_host_router() {
    host_direction_row(0, |fixture, mut peer, connection| async move {
        let observed = fixture.observed();
        assert_eq!(
            (observed.outbound_capacity, observed.inbound_capacity),
            (1, 1),
            "HostRouter::ws_buffer_size(0) did not normalize both production queues to one"
        );
        assert_text_exchange(&mut peer, &connection.sender(), "host-router-zero");
    })
    .await;
}

/// One text frame out and the same text back, so a normalized capacity is
/// proved to still carry traffic rather than only to read as one.
fn assert_text_exchange(peer: &mut std::net::TcpStream, sender: &WsSender, row: &str) {
    sender.send(row).expect("a normalized queue refused a send");
    assert_eq!(
        &*read_ws_text_frame(peer),
        row,
        "a normalized queue did not deliver its frame"
    );
}

/// A live full queue answers `ChannelFull`; a terminal one answers
/// `WebSocketClosed` with the fixed cause.
async fn assert_live_full_and_terminal_send_results() {
    direction_row(1, |fixture, peer, connection| async move {
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
            receive(&mut receiver, "a disconnected peer's receive owner"),
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
    direction_row(1, |_fixture, mut peer, connection| async move {
        let (_sender, mut receiver) = connection.split();
        write_masked_frame(&mut peer, PING, b"keepalive");
        write_ws_text_frame(&mut peer, "application-text");
        write_masked_frame(&mut peer, PONG, b"unsolicited");
        write_masked_frame(&mut peer, 0x02, b"\x00\xff\x10");

        let (opcode, payload) = read_ws_frame_raw(&mut peer);
        assert_eq!(
            (opcode, payload.as_ref()),
            (PONG, b"keepalive".as_slice()),
            "the transport did not answer the ping itself"
        );
        assert_received_text(
            receive(&mut receiver, "the receive owner past a control frame"),
            "application-text",
            "a control frame displaced the application text",
        );
        assert_received_binary(
            receive(&mut receiver, "the timed receive"),
            b"\x00\xff\x10",
            "the timed receive",
        );

        write_ws_close_frame(&mut peer);
        assert_closed_with(
            receive(&mut receiver, "the receive owner after a peer close"),
            WsCloseCause::PeerClosed,
            "a peer close",
        );
    })
    .await;
}

// 1.T3.
#[camber::test]
async fn direction_blocking_operations_follow_runtime_flavor_policy() {
    assert_plain_thread_waits_for_capacity().await;
    assert_plain_thread_receive_waits_for_a_frame().await;
    assert_multi_thread_send_frees_its_worker().await;
    assert_multi_thread_receive_frees_its_worker().await;
    assert_current_thread_refuses_before_waiting().await;
    assert_timed_receive_separates_missing_clock_from_expiry().await;
}

/// Off every runtime, a send waits on the caller's own thread and completes
/// once capacity is released.
async fn assert_plain_thread_waits_for_capacity() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (sender, _receiver) = connection.split();
        fill_outbound_behind_the_writer(&fixture, &sender).await;
        let waiting =
            fixture.spawn_entered_worker("plain-thread-send", move || sender.send("waited"));
        assert!(
            !waiting.settled(),
            "a send against a full queue returned before capacity was released"
        );
        fixture.release(BEFORE_WRITE);
        waiting.take().expect("a released plain-thread send failed");
        let seen: Box<[Box<str>]> = (0..3).map(|_| read_ws_text_frame(&mut peer)).collect();
        assert_eq!(
            seen.last().map(AsRef::as_ref),
            Some("waited"),
            "the released send never reached the peer: {seen:?}"
        );
    })
    .await;
}

/// Off every runtime, a receive waits on the caller's own thread and answers
/// once the peer sends something.
async fn assert_plain_thread_receive_waits_for_a_frame() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (_sender, receiver) = connection.split();
        let waiting =
            fixture.spawn_entered_worker("plain-thread-receive", move || receive_once(receiver));
        assert!(
            !waiting.settled(),
            "a receive on an empty queue answered before the peer sent anything"
        );
        write_ws_text_frame(&mut peer, "waited-for-on-a-plain-thread");
        assert_received_text(
            waiting
                .take()
                .expect("a released plain-thread receive failed"),
            "waited-for-on-a-plain-thread",
            "the plain-thread receive",
        );
    })
    .await;
}

/// On a multi-thread runtime, a send waiting for capacity hands its worker
/// back, so another task on that one worker runs while it waits.
async fn assert_multi_thread_send_frees_its_worker() {
    direction_row(1, |fixture, peer, connection| async move {
        let (sender, _receiver) = connection.split();
        fill_outbound_behind_the_writer(&fixture, &sender).await;
        let (progressed, progress) = std::sync::mpsc::channel();
        let waiting = fixture.spawn_worker("multi-thread-send", move || {
            on_one_worker(progressed, move || sender.send("waited"))
        });
        expect_progress(&progress, "send");
        assert!(
            !waiting.settled(),
            "a send against a full queue returned before capacity was released"
        );
        fixture.release(BEFORE_WRITE);
        waiting.take().expect("a released multi-thread send failed");
        drop(peer);
    })
    .await;
}

/// On a multi-thread runtime, a receive waiting for a frame hands its worker
/// back the same way a waiting send does.
async fn assert_multi_thread_receive_frees_its_worker() {
    direction_row(1, |fixture, mut peer, connection| async move {
        let (_sender, receiver) = connection.split();
        let (progressed, progress) = std::sync::mpsc::channel();
        let waiting = fixture.spawn_worker("multi-thread-receive", move || {
            on_one_worker(progressed, move || receive_once(receiver))
        });
        expect_progress(&progress, "receive");
        assert!(
            !waiting.settled(),
            "a receive on an empty queue answered before the peer sent anything"
        );
        write_ws_text_frame(&mut peer, "waited-for-on-one-worker");
        assert_received_text(
            waiting
                .take()
                .expect("a released multi-thread receive failed"),
            "waited-for-on-one-worker",
            "the multi-thread receive",
        );
    })
    .await;
}

/// Run one blocking endpoint operation as a task on a single-worker runtime,
/// with a second task behind it that reports if it ever gets to run.
///
/// The ordering is explicit rather than left to the scheduler. The waiting task
/// signals that it has started and then calls the operation with no await point
/// in between, so it cannot give the worker up on its own; the reporting task is
/// only spawned after that signal arrives. The one worker is therefore already
/// committed to the operation, and anything the reporting task does afterwards
/// is room `block_in_place` made for it.
fn on_one_worker<T, W>(progressed: std::sync::mpsc::Sender<()>, wait: W) -> T
where
    T: Send + 'static,
    W: FnOnce() -> T + Send + 'static,
{
    single_worker_runtime().block_on(async move {
        let (started, start) = tokio::sync::oneshot::channel();
        let waiting = tokio::spawn(async move {
            let _ = started.send(());
            wait()
        });
        start.await.expect("the waiting task never started");
        drop(tokio::spawn(async move {
            let _ = progressed.send(());
        }));
        waiting.await.expect("the waiting task panicked")
    })
}

/// Wait for the reporting task's word that it ran on the worker the blocked
/// operation was supposed to release.
fn expect_progress(progress: &std::sync::mpsc::Receiver<()>, operation: &str) {
    progress
        .recv_timeout(DIRECTION_DEADLINE)
        .unwrap_or_else(|_| {
            panic!("no other task ran on the one worker a waiting {operation} should have released")
        });
}

/// One Tokio runtime with exactly one worker, so only `block_in_place` can let
/// a second task run while the first waits.
fn single_worker_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("build the single-worker runtime")
}

/// On a current-thread runtime every blocking operation is refused before it
/// waits, because waiting there stops the only thread that could make progress.
async fn assert_current_thread_refuses_before_waiting() {
    direction_row(1, |fixture, peer, connection| async move {
        let (sender, receiver) = connection.split();
        fill_outbound_behind_the_writer(&fixture, &sender).await;
        let refused = fixture.spawn_worker("current-thread-operations", move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("build the current-thread runtime");
            runtime.block_on(current_thread_results(sender, receiver))
        });
        let (sent, received, timed) = refused.take();
        assert!(
            matches!(sent, Err(RuntimeError::BlockingInAsyncContext)),
            "a current-thread send was allowed to wait: {sent:?}"
        );
        assert!(
            matches!(received, Err(RuntimeError::BlockingInAsyncContext)),
            "a current-thread receive was allowed to wait: {received:?}"
        );
        assert!(
            matches!(timed, Err(RuntimeError::BlockingInAsyncContext)),
            "a current-thread timed receive was allowed to wait: {timed:?}"
        );
        fixture.release(BEFORE_WRITE);
        drop(peer);
    })
    .await;
}

/// What all three blocking operations answer on a current-thread runtime.
async fn current_thread_results(
    sender: WsSender,
    mut receiver: WsReceiver,
) -> (
    Result<(), RuntimeError>,
    Result<WsReceive, RuntimeError>,
    Result<WsReceive, RuntimeError>,
) {
    let sent = sender.send("refused-before-waiting");
    let received = receiver.recv();
    let timed = receiver.recv_timeout(Duration::from_millis(1));
    (sent, received, timed)
}

/// A timed receive tells a missing Tokio clock from an expired one.
async fn assert_timed_receive_separates_missing_clock_from_expiry() {
    direction_row(1, |fixture, peer, connection| async move {
        let (_sender, receiver) = connection.split();
        let clockless = fixture.spawn_worker("clockless-timed-receive", move || {
            let mut receiver = receiver;
            let absent = receiver.recv_timeout(Duration::from_millis(1));
            let expired = single_worker_runtime()
                .block_on(async move { receiver.recv_timeout(Duration::from_millis(20)) });
            (absent, expired)
        });
        let (absent, expired) = clockless.take();
        assert!(
            matches!(absent, Err(RuntimeError::NoRuntime)),
            "a timed receive with no Tokio clock did not report NoRuntime: {absent:?}"
        );
        assert!(
            matches!(expired, Err(RuntimeError::Timeout)),
            "a timed receive with a real deadline did not report Timeout: {expired:?}"
        );
        drop(peer);
    })
    .await;
}

// 1.T4.
#[camber::test]
async fn legacy_wsconn_methods_preserve_close_and_borrowed_binary_contract() {
    assert_facade_signatures_and_sender_delegation().await;
    assert_closed_facade_sends_report_broken_pipe().await;
    assert_borrowed_binary_is_copied_at_admission().await;
}

/// Every shipped `WsConn` method still compiles and runs, and `sender()` hands
/// back an independent handle without consuming the facade.
async fn assert_facade_signatures_and_sender_delegation() {
    direction_row(8, |fixture, mut peer, connection| async move {
        let sender = connection.sender();
        write_ws_text_frame(&mut peer, "to-the-facade");
        write_ws_binary_frame_pair(&mut peer);
        let taken = fixture.spawn_facade_receives(connection);
        let taken = taken.take();
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

        sender.send("from-the-independent-handle").expect("send");
        sender.try_send("try-text").expect("try_send");
        sender.send_binary(b"binary").expect("send_binary");
        sender
            .try_send_binary(b"try-binary")
            .expect("try_send_binary");
        connection.send("from-the-facade").expect("facade send");
        connection
            .send_binary(b"facade-binary")
            .expect("facade send_binary");
        assert_eq!(
            &*read_ws_text_frame(&mut peer),
            "from-the-independent-handle",
            "the independent sender did not use the bridge's own outbound queue"
        );
        assert_eq!(&*read_ws_text_frame(&mut peer), "try-text");
        assert_eq!(read_ws_binary_frame(&mut peer).as_ref(), b"binary");
        assert_eq!(read_ws_binary_frame(&mut peer).as_ref(), b"try-binary");
        assert_eq!(&*read_ws_text_frame(&mut peer), "from-the-facade");
        assert_eq!(read_ws_binary_frame(&mut peer).as_ref(), b"facade-binary");

        assert!(
            matches!(
                connection.recv_timeout(Duration::from_millis(20)),
                Err(RuntimeError::Timeout)
            ),
            "WsConn::recv_timeout stopped bounding a silent peer"
        );
    })
    .await;
}

/// A text frame the binary receiver must skip, then the two payloads the two
/// remaining facade receivers take.
fn write_ws_binary_frame_pair(peer: &mut std::net::TcpStream) {
    write_ws_text_frame(peer, "skipped-by-recv-binary");
    write_masked_frame(peer, 0x02, b"only-binary");
    write_ws_text_frame(peer, "either-kind");
}

/// Through the facade, a closed connection is still a broken pipe.
///
/// The `None` is read against the cause that produced it. `WsConn::recv` maps
/// every refusal to `None` — a current-thread runtime, a missing one, anything
/// a later variant adds — so the row commits the peer's close first and names
/// it, or the absence it asserts could be any of them.
async fn assert_closed_facade_sends_report_broken_pipe() {
    direction_row(4, |fixture, mut peer, connection| async move {
        let mut connection = connection;
        fixture.arm(SELECTED);
        write_ws_close_frame(&mut peer);
        fixture.wait_paused(SELECTED).await;
        assert_eq!(
            fixture.observed().terminal,
            Some(WsCloseCause::PeerClosed),
            "the facade's connection ended for a reason other than the peer's close"
        );
        fixture.release(SELECTED);
        assert_eq!(
            connection
                .recv_timeout(DIRECTION_DEADLINE)
                .expect("the facade's timed receive was refused"),
            None,
            "the facade did not map a closed connection back to None"
        );
        assert_broken_pipe(connection.send("after close"), "a closed facade text send");
        assert_broken_pipe(
            connection.send_binary(b"after close"),
            "a closed facade binary send",
        );
    })
    .await;
}

/// A borrowed binary payload is copied when it is admitted, so mutating the
/// caller's buffer afterwards cannot change what the peer receives.
async fn assert_borrowed_binary_is_copied_at_admission() {
    direction_row(2, |fixture, mut peer, connection| async move {
        let mut payload = *b"admitted";
        fixture.arm(BEFORE_WRITE);
        connection
            .send_binary(&payload)
            .expect("a borrowed binary send was refused");
        fixture.wait_paused(BEFORE_WRITE).await;
        payload.copy_from_slice(b"mutated!");
        fixture.release(BEFORE_WRITE);
        assert_eq!(
            read_ws_binary_frame(&mut peer).as_ref(),
            b"admitted",
            "a borrowed binary send retained the caller's buffer instead of copying it"
        );
    })
    .await;
}

// 1.T5.
#[camber::test]
async fn direction_half_drop_and_terminal_cause_are_shared() {
    assert_one_clone_drop_keeps_the_connection_live().await;
    assert_last_sender_drop_closes_the_receiver().await;
    assert_receiver_drop_closes_every_sender().await;
    assert_peer_close_fixes_one_cause_for_every_half().await;
}

/// Dropping one sender clone changes nothing.
async fn assert_one_clone_drop_keeps_the_connection_live() {
    direction_row(4, |_fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        drop(sender.clone());
        sender
            .send("still-live")
            .expect("a surviving sender was refused");
        assert_eq!(&*read_ws_text_frame(&mut peer), "still-live");
        write_ws_text_frame(&mut peer, "still-receiving");
        assert_received_text(
            receive(&mut receiver, "a surviving receive owner"),
            "still-receiving",
            "dropping one sender clone disturbed the receive owner",
        );
    })
    .await;
}

/// Dropping the last sender closes send admission and settles the receiver on
/// `SendersDropped`.
async fn assert_last_sender_drop_closes_the_receiver() {
    direction_row(4, |fixture, peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        drop(sender);
        assert_closed_with(
            receive(
                &mut receiver,
                "the receive owner after the last sender's drop",
            ),
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
    direction_row(4, |_fixture, mut peer, connection| async move {
        let (sender, receiver) = connection.split();
        let second = sender.clone();
        drop(receiver);
        let (opcode, _) = read_ws_frame_raw(&mut peer);
        assert_eq!(
            opcode, CLOSE,
            "a dropped receive owner did not attempt a normal close"
        );
        assert_eq!(
            closed_cause(sender.send("after"), "a send after the receiver dropped"),
            WsCloseCause::ReceiverDropped,
            "a retained sender did not read the receiver's drop"
        );
        assert_eq!(
            closed_cause(
                second.send("after"),
                "a clone's send after the receiver dropped"
            ),
            WsCloseCause::ReceiverDropped,
            "one sender clone read a different cause than another"
        );
    })
    .await;
}

/// A peer close fixes one cause, and a later competing local drop cannot
/// rewrite what the surviving halves read.
async fn assert_peer_close_fixes_one_cause_for_every_half() {
    direction_row(4, |fixture, mut peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        let second = sender.clone();
        write_ws_close_frame(&mut peer);
        assert_closed_with(
            receive(&mut receiver, "the receive owner after a peer close"),
            WsCloseCause::PeerClosed,
            "a peer close reaching the receive owner",
        );
        drop(receiver);
        assert_eq!(
            closed_cause(sender.send("after"), "a send after the peer closed"),
            WsCloseCause::PeerClosed,
            "a later local drop rewrote the committed cause"
        );
        assert_eq!(
            closed_cause(second.send("after"), "a clone's send after the peer closed"),
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
