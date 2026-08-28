//! Daemon-live proof that a public stop command is authoritative when it
//! returns.
//!
//! Both rows run a real background server over a real TCP peer. Every barrier
//! that decides an assertion is public: a command has returned, or a peer has
//! been answered. Nothing here reads a private schedule to decide what the
//! server should report, and no sleep is used as ordering evidence.
//!
//! Both rows hold the supervisor, and only to keep their own window open. A
//! supervisor that has applied a forced stop aborts the connection still owing
//! an answer, and settles the server on the same pass. A row that reads the
//! commit after that races the settlement rather than reading what the command
//! fixed. The cancelled row stops the supervisor on its control selection until
//! its peer has been answered; the escalated row steps it through the graceful
//! transition and stops it at its select boundary. No hold commits a phase,
//! mints a deadline, or fixes a result, so every fact a row asserts is still the
//! one a public command committed.

use std::sync::atomic::Ordering;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::mock::{
    ConnectionOwnerController, ConnectionOwnershipEvent, ServerStopController, ServerStopEdge,
};

use crate::common::{
    HELD_ROUTE, HeldServer, assert_address_reused, assert_admission_closed, held_server,
    read_peer_to_eof, registered_connections, request_on_new_peer, wait_until_paused_within,
};

/// How long a live observation has to arrive before the row fails.
const LIVE_BOUND: Duration = Duration::from_secs(10);

/// The drain bound both rows configure.
///
/// Short enough that a row which accidentally waited the whole grace out would
/// still finish, and long enough that no row can reach it while a peer is being
/// released promptly. A result of `Timeout` from either row is a failure, so the
/// value is a ceiling on the case rather than an ordering device.
const DRAIN: Duration = Duration::from_secs(20);

/// Assert that every connection this server registered also settled.
///
/// The connection owner holds the permit for the whole accepted transport
/// lifetime, so a registered connection with no settlement is a permit that was
/// never handed back.
fn assert_connections_settled(connections: &ConnectionOwnerController, context: &str) {
    let observed = connections.observed();
    let registered = registered_connections(&observed);
    assert!(
        !registered.is_empty(),
        "{context}: no connection owner was ever registered"
    );
    for connection in registered.iter().copied() {
        assert!(
            observed.contains(ConnectionOwnershipEvent::ServerConnectionSettled { connection }),
            "{context}: connection {connection} never settled, so it never released its permit"
        );
    }
}

// 1.T2 — Invariant 2: an accepted cancellation before terminal commitment
// yields the flat server result `RuntimeError::Cancelled`.
//
// The barrier is the public command's own return. The peer is still connected
// and its handler is still held when `cancel()` is called, so the committed
// phase read immediately afterwards is a fact the command fixed rather than one
// the supervisor selected after being woken.
#[camber::test]
async fn accepted_cancel_commits_before_wakeup_and_flat_join_reports_cancelled() {
    let server = held_server(4, DRAIN);
    let stop = server.stop();
    let mut peer = request_on_new_peer(server.addr, HELD_ROUTE, "close").await;
    server.await_entry("the cancelled row's held request").await;

    let running = stop.observed();
    assert_eq!(running.phase, "running");
    assert_eq!(running.outcome, "pending");
    assert!(!running.cancel_commanded);

    // Armed before the command, so the supervisor stops on the selection rather
    // than past it. A held supervisor applies no phase, closes no admission, and
    // aborts no connection, so the commit this row reads and the answer its
    // handler still owes are both the command's own.
    stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .expect("arm the supervisor's control selection");

    server.handle.cancel();

    // Read before anything is released and before the server is joined. There
    // is no supervisor turn between the command returning and this line.
    let commanded = stop.observed();
    assert_eq!(
        commanded.phase, "cancelled",
        "cancel() returned without committing its control fact"
    );
    assert!(
        commanded.cancel_commanded,
        "cancel() did not record that a caller asked"
    );
    assert_eq!(
        commanded.outcome, "pending",
        "cancel() must not fix the flat result before the children settle"
    );
    assert_eq!(commanded.commits, 1);
    assert_eq!(
        commanded.aggregate_deadline, None,
        "a cancellation mints no aggregate deadline"
    );

    // Repeating a compatible command is idempotent, whatever else has happened.
    server.handle.cancel();
    assert_eq!(stop.observed().commits, 1);

    hold_at(
        &stop,
        ServerStopEdge::SupervisorSelectedControl,
        "the supervisor never took the cancellation",
    )
    .await;

    server.release.add_permits(1);
    let answer = read_peer_to_eof(&mut peer, "cancelled held peer").await;
    assert!(
        answer.starts_with("HTTP/1.1 200"),
        "the held peer's answer was refused rather than served: {answer}"
    );
    assert_eq!(server.served.load(Ordering::SeqCst), 1);

    // The peer has its answer, so nothing is left for the forced abort behind
    // the cancellation to take away.
    stop.release(ServerStopEdge::SupervisorSelectedControl)
        .expect("release the supervisor's control selection");

    let result = tokio::time::timeout(LIVE_BOUND, server.handle)
        .await
        .expect("the cancelled server must join");
    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "an accepted cancellation must yield the flat cancelled result"
    );
    assert_eq!(stop.observed().outcome, "cancelled");

    assert_connections_settled(&server.controller.connections, "cancelled server");
    drop(peer);
    assert_address_reused(server.addr, "cancelled server").await;
}

// 1.T4 — Invariant 3 live wiring: an accepted graceful command closes admission
// and drains owned work within the shared aggregate deadline, and a later
// accepted cancellation before terminal commitment yields cancellation.
//
// Every step that decides an assertion crosses a public barrier: a second peer
// is served only on the permit the first connection owner handed back, the
// graceful command returns, a later peer observes closed admission, the drain
// answers the work it had already admitted, the cancellation command returns,
// and the join reports one flat result. The supervisor is held across the
// middle of that list for the reason the module header gives — the escalation
// has to be the thing that ends the drain, not the loser of a race with it.
#[camber::test]
async fn graceful_admission_drain_and_cancel_escalation_cross_public_barriers() {
    // One permit, so a second peer can only ever be served after the first
    // connection owner has released it.
    let server = held_server(1, DRAIN);
    let stop = server.stop();
    let mut peer = request_on_new_peer(server.addr, HELD_ROUTE, "close").await;
    server.await_entry("the escalated row's held request").await;

    // Parked: accepted by the kernel, waiting on the one permit the first peer
    // holds. It cannot enter a handler until that permit comes back.
    let mut parked = request_on_new_peer(server.addr, HELD_ROUTE, "close").await;

    server.release.add_permits(1);
    let answer = read_peer_to_eof(&mut peer, "first held peer").await;
    assert!(
        answer.starts_with("HTTP/1.1 200"),
        "the first peer was refused rather than served: {answer}"
    );

    // Permit reacquisition, proved by the only thing that can consume it: the
    // parked peer's handler entering while the server is still running.
    server
        .await_entry("the parked peer never reacquired the released permit")
        .await;

    let minted = commit_graceful_and_hold_the_supervisor(&server, &stop).await;

    // Admission is closed for anything that was not already accepted: the
    // graceful transition gave the listener up before the hold took effect.
    assert_admission_closed(server.addr, LIVE_BOUND).await;

    // The drain still answers the work it had already admitted. The connection
    // that answers needs nothing from the held supervisor to do it.
    server.release.add_permits(1);
    let parked_answer = read_peer_to_eof(&mut parked, "drained parked peer").await;
    assert!(
        parked_answer.starts_with("HTTP/1.1 200"),
        "the drain refused work it had already admitted: {parked_answer}"
    );

    escalate_and_assert_deadline_reuse(&server, &stop, minted);

    // The escalation has committed, so the drain can be allowed to reach its
    // own terminal pass and report what that commit fixed.
    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .expect("release the supervisor's select boundary");

    let result = tokio::time::timeout(LIVE_BOUND, server.handle)
        .await
        .expect("the escalated server must join");
    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "escalating a drain yields cancellation rather than a restarted grace"
    );
    let settled = stop.observed();
    assert_eq!(settled.outcome, "cancelled");
    assert_eq!(
        settled.aggregate_deadline,
        Some(minted),
        "settling the escalated drain minted no further deadline"
    );
    assert_eq!(server.served.load(Ordering::SeqCst), 2);

    assert_connections_settled(&server.controller.connections, "escalated server");
    drop(peer);
    drop(parked);
    assert_address_reused(server.addr, "escalated server").await;
}

/// Ask for the graceful stop, hand back the one expiry its commit minted, and
/// leave the supervisor held at its own select boundary.
///
/// The two moments have to be in this order. The supervisor applies the
/// transition first, and applying it is what gives the listener up; it is held
/// only afterwards, on the pass that would otherwise reap the drained
/// connection and settle. So what follows sees closed admission and a drain
/// that cannot end underneath it.
async fn commit_graceful_and_hold_the_supervisor(
    server: &HeldServer,
    stop: &ServerStopController,
) -> tokio::time::Instant {
    stop.pause_once(ServerStopEdge::SupervisorSelectedControl)
        .expect("arm the supervisor's control selection");
    let minted = commit_graceful_and_read_minted_expiry(server, stop).await;
    hold_at(
        stop,
        ServerStopEdge::SupervisorSelectedControl,
        "the supervisor never took the graceful transition",
    )
    .await;

    // Armed while the selection still holds the loop, so releasing it cannot
    // run the supervisor past the boundary this row needs it to stop at.
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .expect("arm the supervisor's select boundary");
    stop.release(ServerStopEdge::SupervisorSelectedControl)
        .expect("release the supervisor's control selection");
    hold_at(
        stop,
        ServerStopEdge::BeforeSupervisorSelect,
        "the supervisor never reached its select boundary",
    )
    .await;
    minted
}

/// Wait, within [`LIVE_BOUND`], until the stop owner is held at `edge`.
async fn hold_at(stop: &ServerStopController, edge: ServerStopEdge, context: &str) {
    wait_until_paused_within(stop, edge, LIVE_BOUND, context).await;
}

/// Ask for the graceful stop, and hand back the one expiry its commit minted.
///
/// The running reading is taken first, inside this step, because the claim the
/// minted expiry makes is only meaningful against a server that had none.
async fn commit_graceful_and_read_minted_expiry(
    server: &HeldServer,
    stop: &ServerStopController,
) -> tokio::time::Instant {
    let running = stop.observed();
    assert_eq!(running.phase, "running");
    assert_eq!(
        running.aggregate_deadline, None,
        "a running server has minted no aggregate deadline"
    );

    let requested_at = tokio::time::Instant::now();
    server.handle.shutdown();
    let graceful = stop.observed();
    assert_eq!(
        graceful.phase, "graceful",
        "shutdown() returned without committing its control fact"
    );
    assert_eq!(graceful.commits, 1);

    // The one aggregate expiry, minted by the graceful commit itself. Read here
    // rather than inferred from the result: a drain that ends in cancellation
    // reports the same flat error whether or not a deadline was ever minted.
    let minted = graceful
        .aggregate_deadline
        .expect("the graceful commit mints the one aggregate deadline");
    assert!(
        minted > requested_at && minted <= tokio::time::Instant::now() + DRAIN,
        "the minted expiry is the configured drain from the commit, not some other clock"
    );
    minted
}

/// Escalate the drain, and require it to keep the expiry it was already under.
///
/// The command commits before returning, and it does not restart any grace: the
/// result is cancellation, not the drain bound expiring.
fn escalate_and_assert_deadline_reuse(
    server: &HeldServer,
    stop: &ServerStopController,
    minted: tokio::time::Instant,
) {
    server.handle.cancel();
    let escalated = stop.observed();
    assert_eq!(escalated.phase, "cancelled");
    assert!(escalated.cancel_commanded);
    assert_eq!(
        escalated.commits, 2,
        "graceful then cancellation is exactly two committed phases"
    );
    assert_eq!(
        escalated.aggregate_deadline,
        Some(minted),
        "the escalation reused the one minted expiry rather than starting a second grace"
    );
}
