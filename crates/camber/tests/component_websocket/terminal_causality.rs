//! Which fact a direct WebSocket reports when its own owners and its server's
//! stop state both have something to say.
//!
//! Both rows run against a real upgrade over a real socket, because the claim
//! is about a production commit edge: the bridge's one cause is fixed inside
//! the shared stop state's lock, so the answer is whichever fact committed
//! first — not whichever notification a coordinator turn happened to reach.

#![cfg(feature = "ws")]

use crate::common::{
    AFTER_COMMIT, BEFORE_COMMIT, DIRECTION_DEADLINE, DirectionTestFixture, abortive_direction_row,
    assert_closed_with, closed_cause,
};
use camber::http::{WsCloseCause, WsReceive, WsReceiver, WsSender};

// 4.T1
#[camber::test]
async fn websocket_terminal_commit_uses_shared_server_state_linearization() {
    committed_peer_cause_survives_a_later_cancellation().await;
    accepted_cancellation_overtakes_an_uncommitted_peer_cause().await;
}

/// A cause this bridge already committed is the first fact, and a cancellation
/// that commits afterwards cannot rewrite it.
///
/// The bridge is held on the far side of its own commit, which is the only
/// place a later stop is provably later. Both directions are then asked
/// separately, because the claim is that one immutable value is what each of
/// them reads.
async fn committed_peer_cause_survives_a_later_cancellation() {
    abortive_direction_row(1, |fixture, peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        fixture.arm(AFTER_COMMIT);
        drop(peer);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_committed(&fixture, WsCloseCause::PeerDisconnected);
        fixture.cancel_server();
        assert_send_reports(&sender, WsCloseCause::PeerDisconnected);
        fixture.release(AFTER_COMMIT);
        assert_receive_reports(&mut receiver, WsCloseCause::PeerDisconnected);
        drop((sender, receiver));
        assert_committed(&fixture, WsCloseCause::PeerDisconnected);
    })
    .await;
}

/// A cancellation committed in the shared stop state is the first fact, even
/// when this bridge is already holding another answer.
///
/// The bridge is held with the peer's disconnect offered and uncommitted. The
/// public `cancel` then returns, which means the forced phase is committed
/// before the hold is released — so a commit that trusted the offer in hand
/// would report the peer, and a commit taken inside the stop state reports the
/// server.
async fn accepted_cancellation_overtakes_an_uncommitted_peer_cause() {
    abortive_direction_row(1, |fixture, peer, connection| async move {
        let (sender, mut receiver) = connection.split();
        fixture.arm(BEFORE_COMMIT);
        drop(peer);
        fixture.wait_paused(BEFORE_COMMIT).await;
        fixture.cancel_server();
        fixture.arm(AFTER_COMMIT);
        fixture.release(BEFORE_COMMIT);
        fixture.wait_paused(AFTER_COMMIT).await;
        assert_committed(&fixture, WsCloseCause::ServerCancelled);
        assert_send_reports(&sender, WsCloseCause::ServerCancelled);
        fixture.release(AFTER_COMMIT);
        assert_receive_reports(&mut receiver, WsCloseCause::ServerCancelled);
        drop((sender, receiver));
        assert_committed(&fixture, WsCloseCause::ServerCancelled);
    })
    .await;
}

/// The one cause this listener's bridge published.
fn assert_committed(fixture: &DirectionTestFixture, expected: WsCloseCause) {
    let observed = fixture.observed();
    assert_eq!(
        observed.terminal,
        Some(expected),
        "the bridge committed another cause"
    );
    assert_eq!(
        observed.terminal_commits, 1,
        "the bridge committed more than one cause"
    );
}

/// The send half reports the committed cause.
///
/// Asked before the receive half, because a send is answered from the terminal
/// state directly while a receive is answered through the queue the settlement
/// closes — so this is the half that can be asked while the bridge is still
/// held at its commit edge.
fn assert_send_reports(sender: &WsSender, expected: WsCloseCause) {
    assert_eq!(
        closed_cause(sender.send("after the cause was fixed"), "the send half"),
        expected,
        "the send half reported another cause"
    );
}

/// The receive half reports the same committed cause.
fn assert_receive_reports(receiver: &mut WsReceiver, expected: WsCloseCause) {
    let received: WsReceive = receiver
        .recv_timeout(DIRECTION_DEADLINE)
        .expect("the receive half answered nothing before its bound");
    assert_closed_with(received, expected, "the receive half");
}
