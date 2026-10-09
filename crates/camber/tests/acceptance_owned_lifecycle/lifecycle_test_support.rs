//! Acceptance proof that the public lifecycle contract owes nothing to the
//! test-support module.
//!
//! Every other owned-lifecycle row in this root holds production somewhere,
//! which is what makes them exact. This one holds nothing. It builds the server
//! a normal consumer builds, upgrades a peer over a real socket, asks the
//! public handle to cancel, and reads only what a consumer can read: the flat
//! result the join hands back, the one cause both halves of the connection
//! report, the address the completed server gave up, and the fresh server that
//! admits over it under the same limit.
//!
//! No controller is registered here and no edge is armed. If the published
//! contract needed a controller installed to hold, this row would be the one
//! that noticed.

use std::net::SocketAddr;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::{Request, Response, Router, ServerPolicy, WsCloseCause};

use crate::common::{
    assert_closed_with, assert_transport_eof, closed_cause, parking_router, rebind_within, request,
    upgraded_ws_peer,
};

/// How long any public result in this row has to arrive.
///
/// Every wait here is on an in-process localhost transport that either settles
/// promptly or is never going to. It is generous rather than tight because a
/// bound that expired under load would report a scheduling delay as a broken
/// contract; what an expired bound does report is a result that never came,
/// which is a failure either way.
const RESULT_BOUND: Duration = Duration::from_secs(10);

/// The drain the cancelled server is configured with.
///
/// Never reached: forced cancellation gives up whatever the aggregate had left,
/// so a row whose claim is the cancelled result must not be able to reach a
/// deadline instead.
const WIDE_DRAIN: Duration = Duration::from_secs(30);

/// The route the upgraded peer takes.
const WS_PATH: &str = "/ws";

/// The route the readmitting server answers on.
const READMIT_PATH: &str = "/readmitted";

/// The one connection either server admits at a time.
///
/// One, because readmission is the claim: a server that let a second connection
/// in beside the first would prove nothing about the permit the first gave back.
const ADMISSION_LIMIT: usize = 1;

/// Serve `router` on a fresh ephemeral port under the row's own policy.
///
/// The policy is the row's because both halves of the claim depend on it: the
/// limit is what makes readmission mean something, and the drain is what a
/// forced cancellation has to be seen to give up rather than wait out.
fn serve_limited(router: Router, listener: tokio::net::TcpListener) -> camber::http::ServerHandle {
    let policy = ServerPolicy::default()
        .connection_limit(ADMISSION_LIMIT)
        .expect("a positive connection limit")
        .shutdown_timeout(WIDE_DRAIN)
        .expect("a positive drain bound");
    camber::http::server(router)
        .policy(policy)
        .serve_background(listener)
        .expect("the owned server requires a Tokio runtime")
}

/// Serve the same address again and admit one connection over it.
///
/// The permit and the address are released by the same completion, and this is
/// what says both are actually free: a limit of one admits this request only if
/// nothing the cancelled server owned is still counted against it.
async fn assert_readmits_under_the_same_limit(addr: SocketAddr) {
    let listener = rebind_within(addr, RESULT_BOUND)
        .await
        .unwrap_or_else(|error| {
            panic!("the cancelled server still held {addr} after {RESULT_BOUND:?}: {error}")
        });
    let mut router = Router::new();
    router.get(READMIT_PATH, |_request: &Request| async move {
        Response::text(200, "readmitted")
    });
    let handle = serve_limited(router, listener);

    let answered = tokio::task::spawn_blocking(move || {
        request(addr, "GET", READMIT_PATH, &[], &[], RESULT_BOUND)
    })
    .await
    .expect("the readmission worker panicked")
    .unwrap_or_else(|error| panic!("the reused address never admitted a connection: {error}"));
    assert_eq!(
        answered.status, 200,
        "the readmitted connection answered {answered:?}"
    );

    let stopped = tokio::time::timeout(RESULT_BOUND, handle.shutdown_and_join())
        .await
        .expect("the readmitting server never joined");
    assert!(
        stopped.is_ok(),
        "the readmitting server reported {stopped:?} rather than a clean stop"
    );
}

// 16.T2 — Invariant 21's other side: every public lifecycle claim crosses a
// public API and a real transport boundary, and none of them needs a test
// controller installed to hold.
//
// The row registers nothing, arms nothing, and reads nothing private. It
// upgrades a peer over a real socket, calls the public `cancel`, and then makes
// the four claims a consumer can make: the join is one flat `Cancelled`, both
// halves of the connection report the one cause the bridge committed, the peer
// is left with a transport that ended rather than a close it was promised, and
// the address the completed server gave up admits again under the same limit of
// one.
//
// The flat result is asserted first on purpose. It is the claim the seam-break
// mutation removes — a `cancel` that publishes its abort without committing the
// phase — and asserting it ahead of the causes is what makes that mutation
// report the contract it broke rather than a symptom downstream of it.
#[camber::test]
async fn public_lifecycle_results_hold_with_test_controller_module_unlinked() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the cancelled server's listener");
    let addr = listener.local_addr().expect("read the listener's address");

    // The shared parking router: the callback holds nothing once it has handed
    // its connection over, so its gate decides when it returns. Building the
    // router arms nothing and registers no controller.
    let (router, mut handoff) = parking_router(WS_PATH);
    let handle = serve_limited(router, listener);
    // A plain socket through the shared handshake, which registers nothing and
    // arms nothing: the `101` it reads was framed by the server, so the bridge
    // behind it exists by the time it returns.
    let mut peer = upgraded_ws_peer(addr, WS_PATH, "the cancelled server's peer").await;

    let (sender, mut receiver) = handoff.connection().await.split();

    // The public command, and nothing else. It returns before anything here
    // reads a result, so what follows is read from a server the caller has
    // already been answered by.
    handle.cancel();
    drop(handoff.take_gate());

    let joined = tokio::time::timeout(RESULT_BOUND, handle.join()).await;
    assert!(
        matches!(joined, Ok(Err(RuntimeError::Cancelled))),
        "accepted cancellation was not the committed flat result: the join \
         reported {joined:?}"
    );

    // One cause, shared. Both halves of a connection end on the fact the bridge
    // committed, so a row that read two different answers would be reading two
    // terminals rather than one.
    assert_eq!(
        closed_cause(
            sender.send("after-the-cancellation").await,
            "a send past the cancellation"
        ),
        WsCloseCause::ServerCancelled,
        "the send half reported another cause"
    );
    assert_closed_with(
        receiver
            .recv_timeout(RESULT_BOUND)
            .await
            .unwrap_or_else(|error| panic!("the receive half never ended: {error}")),
        WsCloseCause::ServerCancelled,
        "the two halves of one connection reported different causes",
    );
    drop((sender, receiver));

    // A cancelled server owes the peer no close frame, so what the peer is owed
    // is the end itself: a byte, or a read that never ends, fails here.
    assert_transport_eof(&mut peer, "the cancelled server's peer").await;
    assert_readmits_under_the_same_limit(addr).await;
}
