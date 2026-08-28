//! An upgrade the supervisor would not own, refused before any `101`.
//!
//! The refusal runs against a real owned server driven through its lifecycle
//! checkpoints, so what this proves about close, dispatch, and completion is
//! what the owner actually did — not what a dispatch path returned.

#![cfg(feature = "ws")]

use crate::common::{
    Collapsed, Journal, ReadyServer, UNAVAILABLE_BODY, assert_classification, only,
    recording_mapper,
};

use camber::http::mock::{
    ConnectionOwnerEdge, ScopedSupervisedRegistration, ServerStopEdge, UpgradeOwnerEdge,
    supervised_registration,
};
use camber::http::{
    Rejection, RejectionContext, RejectionKind, RejectionProtocol, Request, Router, WsConn,
};
use camber::runtime;
use std::future::IntoFuture;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{SyncSender, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// The suite every observation in this module is recorded under.
const ORIGIN: &str = "acceptance_owned_lifecycle";

/// The route the refused upgrade asks for, and the identity it must report.
const SOCKET: &str = "/ws";

/// The subprotocol the refused handshake offers, and Camber selects.
///
/// Offered on a refusal raised after negotiation settled, which is the only
/// point that can say a subprotocol was selected: the request it was read from
/// moves into the bridge, and the bridge is what this refusal cancels.
const SUBPROTOCOL: &str = "camber.v1";

/// A WebSocket route whose handler reports whether a bridge ever reached it.
fn counted_socket(dispatched: &Arc<AtomicUsize>) -> Router {
    let mut router = Router::new();
    let dispatched = Arc::clone(dispatched);
    router.ws(SOCKET, move |_request: &Request, mut connection: WsConn| {
        dispatched.fetch_add(1, Ordering::SeqCst);
        while connection.recv().is_some() {}
        Ok(())
    });
    router
}

/// A WebSocket router whose handler reports whether a bridge ever reached it.
fn counted_router(journal: &Journal, dispatched: &Arc<AtomicUsize>) -> Router {
    counted_socket(dispatched).rejection_mapper(recording_mapper(journal, ORIGIN))
}

/// Complete one valid upgrade and leave its bridge holding the transport.
///
/// The zero each case asserts afterwards is a claim that no bridge reached the
/// handler, and a counter nothing had ever moved would report that zero whether
/// the registrar refused the upgrade or the route were simply misspelled. This
/// is the calibration those zeros are read as deltas from: the same route, the
/// same server, and a handshake the supervisor does own.
///
/// Driven before any checkpoint is armed, so the supervisor runs it the way it
/// runs any upgrade and the stepped sequence begins at a free loop. The
/// transport is handed back rather than given up here, because when the bridge
/// ends is a thing one of the two callers has to observe.
async fn dispatch_one_upgrade(
    addr: SocketAddr,
    dispatched: &Arc<AtomicUsize>,
) -> tokio::net::TcpStream {
    let mut peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    peer.write_all(crate::common::ws_upgrade_request(SOCKET).as_bytes())
        .await
        .unwrap();
    let committed = crate::read_http_head(&mut peer).await;
    assert!(
        committed.starts_with("HTTP/1.1 101"),
        "the calibrating handshake commits its upgrade: {committed}"
    );
    // Yielded rather than slept on: the handoff happens after the `101` is
    // written, so the count is read until it moves, under this module's own
    // bound and with no wait of its own to get wrong.
    crate::bounded(
        async {
            while dispatched.load(Ordering::SeqCst) == 0 {
                tokio::task::yield_now().await;
            }
        },
        "the calibrating upgrade never reached the WebSocket handler",
    )
    .await;
    peer
}

/// The calibrating upgrade, given up as soon as it has dispatched.
///
/// The handler reads until its peer is gone, so dropping the transport is what
/// lets that bridge finish. Nothing here waits on it: this case has no limit to
/// contend for, so the calibrating connection outliving its own drop by an
/// instant changes nothing the refusal below turns on.
async fn calibrate_dispatch(addr: SocketAddr, dispatched: &Arc<AtomicUsize>) -> usize {
    drop(dispatch_one_upgrade(addr, dispatched).await);
    calibrated(dispatched)
}

/// The count one calibrating upgrade left behind.
///
/// Exactly one, asserted rather than merely read: a baseline taken from a
/// counter that had moved twice would let a second dispatch below read as none.
fn calibrated(dispatched: &Arc<AtomicUsize>) -> usize {
    let seen = dispatched.load(Ordering::SeqCst);
    assert_eq!(seen, 1, "the calibrating upgrade dispatched one bridge");
    seen
}

/// The calibrating upgrade, given up and then joined through the supervisor.
///
/// The limited case cannot simply drop it: its whole premise is that the one
/// permit this server has is the one the idle connection takes, so a calibrating
/// connection whose task had not yet released its own would park the idle peer
/// instead. The supervisor observing that task's join is the point the permit is
/// provably back, and it is a checkpoint rather than a wait.
async fn calibrate_limited_dispatch(
    owners: &ScopedSupervisedRegistration,
    addr: SocketAddr,
    dispatched: &Arc<AtomicUsize>,
) -> usize {
    let peer = dispatch_one_upgrade(addr, dispatched).await;
    owners
        .stop
        .pause_once(ServerStopEdge::SupervisorSelectedTask)
        .unwrap();
    drop(peer);
    crate::wait_until_paused_bounded(
        owners,
        ServerStopEdge::SupervisorSelectedTask,
        "the calibrating bridge's task was never joined",
    )
    .await;
    owners
        .stop
        .release(ServerStopEdge::SupervisorSelectedTask)
        .unwrap();
    calibrated(dispatched)
}

/// One refusal stopped inside policy, and the release that lets it finish.
///
/// Holding the mapper holds the connection: the refusal it is shaping has not
/// reached the peer, so the transport — and the slot the connection was
/// admitted on — is still that connection's own. The two signals are the whole
/// handshake; each carries this root's own observation bound, which turns a
/// signal that never comes into a failure instead of a hung executable.
///
/// The halves are different primitives because they are waited on from
/// different worlds. Entry is observed from an `async fn` on a runtime worker,
/// so it is a Tokio one-shot the observer awaits rather than a blocking receive
/// that would pin that worker for the whole bound. The release is waited on
/// inside production's own synchronous rejection mapper, where blocking is the
/// point: it is what holds the connection.
struct HeldMapping {
    /// Taken by the one wait that consumes it.
    entered: Option<tokio::sync::oneshot::Receiver<()>>,
    release: SyncSender<()>,
}

impl HeldMapping {
    /// Wait until policy has the refusal in hand and is holding it.
    async fn await_entry(&mut self) {
        let entered = self
            .entered
            .take()
            .expect("the held mapping's entry was already awaited");
        crate::bounded(entered, "policy never began mapping the refusal")
            .await
            .expect("policy gave up its entry signal without ever mapping the refusal");
    }

    /// Let the held refusal finish and reach its peer.
    fn release(&self) {
        self.release
            .send(())
            .expect("the held mapping was gone before it could be released");
    }
}

/// A router whose policy stops inside the one refusal it is given.
fn held_mapping_router(journal: &Journal, dispatched: &Arc<AtomicUsize>) -> (Router, HeldMapping) {
    let (reached, entered) = tokio::sync::oneshot::channel();
    let reached = Mutex::new(Some(reached));
    let (release, resume) = sync_channel(1);
    let resume = Mutex::new(resume);
    let record = recording_mapper(journal, ORIGIN);
    let router = counted_socket(dispatched).rejection_mapper(
        move |rejection: &Rejection, context: &RejectionContext| {
            let answer = record(rejection, context);
            reached
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
                .expect("policy was given a second refusal to hold")
                .send(())
                .expect("the case stopped observing policy");
            resume
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .recv_timeout(crate::OBSERVATION_DEADLINE)
                .expect("the held refusal was never released");
            answer
        },
    );
    (
        router,
        HeldMapping {
            entered: Some(entered),
            release,
        },
    )
}

// 4.T3: a refused upgrade registration is mapped, and changes no ownership.
#[camber::test]
async fn upgrade_registration_rejection_keeps_close_permit_and_completion_contract() {
    let journal = Journal::default();
    let dispatched = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let owners = supervised_registration(addr).unwrap();
    // Adopted rather than held bare: `ServerHandle` has no `Drop`, so an
    // assertion failing anywhere below leaves the supervisor task and its
    // listener with no explicit cancel. The guard owes that cancel and the join
    // on every exit, and it stands until the case sends its own cancel.
    let served = ReadyServer::adopt(
        addr,
        camber::http::serve_background(listener, counted_router(&journal, &dispatched))
            .expect("owned server requires a Tokio runtime"),
    );

    let calibrated = calibrate_dispatch(addr, &dispatched).await;

    // The ticket reaches the supervisor, then forced control beats it: the
    // registrar refuses an upgrade it can no longer own. The handshake offers a
    // subprotocol, so the refusal is raised past a settled negotiation.
    let mut client = crate::prepare_offered_submitted_upgrade(
        &owners,
        addr,
        &crate::common::ws_upgrade_request_with(SOCKET, &[("Sec-WebSocket-Protocol", SUBPROTOCOL)]),
    )
    .await;
    owners
        .stop
        .pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    // The guard steps aside here, and only here: from this line on the case has
    // sent its own cancel, so no exit path can leave the server uncancelled.
    let handle = served.into_handle();
    handle.cancel();
    owners
        .stop
        .release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    crate::wait_until_paused_bounded(
        &owners,
        ServerStopEdge::SupervisorSelectedControl,
        "registration control selection timed out",
    )
    .await;
    crate::apply_selected_event_then_release_transfer(
        &owners,
        ServerStopEdge::SupervisorSelectedControl,
        "forced control was not applied before releasing the submitted upgrade",
    )
    .await;

    crate::assert_refused_upgrade_wire(&mut client, "the registration-refused upgrade").await;
    crate::assert_cancelled(handle.await);

    assert_refusal_context(&journal, "upgrade registration refusal", Some(SUBPROTOCOL));
    assert_eq!(
        dispatched.load(Ordering::SeqCst) - calibrated,
        0,
        "no bridge is dispatched for an upgrade that was never admitted"
    );
}

/// The category and established context one refused registration reported.
///
/// The category is [`RejectionKind::InternalService`] because a supervisor
/// refusal is Camber's own failure to serve rather than anything the peer did.
///
/// `subprotocol` is what negotiation had settled on when the refusal was
/// raised, so a handshake that offered one and a handshake that offered none
/// are the same assertion with different expected presence.
fn assert_refusal_context(journal: &Journal, label: &str, subprotocol: Option<&str>) {
    let seen = only(journal, label);
    assert_classification(
        &seen,
        &Collapsed {
            kind: RejectionKind::InternalService,
            status: 503,
            message: UNAVAILABLE_BODY,
        },
        label,
    );
    assert_eq!(
        seen.route.as_deref(),
        Some(SOCKET),
        "the selected route is established"
    );
    assert_eq!(
        seen.protocol,
        Some(RejectionProtocol::WebSocket),
        "the selected dispatch class is established"
    );
    assert_eq!(
        seen.subprotocol.as_deref(),
        subprotocol,
        "the refusal reports exactly what negotiation had selected"
    );
}

/// Accept one peer that has sent nothing, and leave it holding the permit.
///
/// The request is withheld deliberately. This connection's permit is taken at
/// admission, so holding the handshake back is what lets a second peer reach
/// the limit's wait before the upgrade that will be refused exists at all.
async fn accept_idle_peer_holding_the_permit(
    owners: &ScopedSupervisedRegistration,
    addr: SocketAddr,
) -> tokio::net::TcpStream {
    owners
        .stop
        .pause_once(ServerStopEdge::SupervisorSelectedAccept)
        .unwrap();
    let peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    crate::wait_until_paused_bounded(
        owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "the idle peer's accept selection timed out",
    )
    .await;
    crate::apply_selected(
        owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "the idle peer's post-accept boundary timed out",
    )
    .await;
    crate::select_next(
        owners,
        ServerStopEdge::SupervisorSelectedPermit,
        "the idle peer's permit selection timed out",
    )
    .await;
    crate::apply_selected(
        owners,
        ServerStopEdge::SupervisorSelectedPermit,
        "the idle peer's post-permit boundary timed out",
    )
    .await;
    peer
}

/// Accept a second peer, which the limit can only park.
async fn park_second_peer(
    owners: &ScopedSupervisedRegistration,
    addr: SocketAddr,
) -> tokio::net::TcpStream {
    let peer = tokio::net::TcpStream::connect(addr).await.unwrap();
    crate::select_next(
        owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "the parked peer's accept selection timed out",
    )
    .await;
    crate::apply_selected(
        owners,
        ServerStopEdge::SupervisorSelectedAccept,
        "the parked peer's post-accept boundary timed out",
    )
    .await;
    peer
}

/// Wait until the parked peer is blocked on the limit, not on the network.
///
/// The checkpoint is the whole observation: reaching it means the immediate
/// acquisition failed, so the only permit this server has is the one the idle
/// connection took at admission and still holds.
async fn observe_parked_permit_wait(owners: &ScopedSupervisedRegistration) {
    owners
        .connections
        .pause_once(ConnectionOwnerEdge::PermitWaitPending)
        .unwrap();
    owners
        .upgrades
        .pause_once(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .unwrap();
    owners
        .stop
        .release(ServerStopEdge::BeforeSupervisorSelect)
        .unwrap();
    crate::wait_until_paused_bounded(
        owners,
        ConnectionOwnerEdge::PermitWaitPending,
        "the parked peer was admitted while the upgrade still held the only permit",
    )
    .await;
}

/// Offer the handshake, then let forced control refuse the transfer it asks for.
///
/// The cancel lands while the connection is held at its own transfer edge, so
/// the answer it gives refuses an upgrade whose admission has already closed —
/// the same refusal the unlimited case drives, reached without giving the permit
/// up first.
async fn refuse_the_offered_upgrade(
    owners: &ScopedSupervisedRegistration,
    client: &mut tokio::net::TcpStream,
    handle: &camber::http::ServerHandle,
) {
    client
        .write_all(crate::common::ws_upgrade_request(SOCKET).as_bytes())
        .await
        .unwrap();
    crate::wait_until_paused_bounded(
        owners,
        UpgradeOwnerEdge::BeforeTransferAcknowledge,
        "the offered upgrade never reached its connection's transfer edge",
    )
    .await;
    handle.cancel();
    owners
        .stop
        .pause_once(ServerStopEdge::SupervisorSelectedControl)
        .unwrap();
    owners
        .upgrades
        .release(UpgradeOwnerEdge::BeforeTransferAcknowledge)
        .unwrap();
}

/// Enter the abort while the refusal is still held inside policy.
///
/// This is the observation the permit carries. The abort forces down every
/// connection with no answer of its own outstanding, and a connection that has
/// just refused an upgrade has one, so a connection that gave the slot up at
/// mapping time would be aborted here — with its refusal still unwritten —
/// instead of answering it.
async fn begin_abort_over_the_held_refusal(
    owners: &ScopedSupervisedRegistration,
    mapping: &mut HeldMapping,
) {
    mapping.await_entry().await;
    crate::wait_until_paused_bounded(
        owners,
        ServerStopEdge::SupervisorSelectedControl,
        "forced control was never selected while the refusal was held",
    )
    .await;
    crate::apply_selected(
        owners,
        ServerStopEdge::SupervisorSelectedControl,
        "forced control was not applied while the refusal was held",
    )
    .await;
    mapping.release();
}

/// The two waits the permit case still owes a release when it reads the refusal.
///
/// The parked peer's permit wait and the supervisor's select boundary are armed
/// long before the case releases them, and `release` is the only way out of a
/// pause — the lifecycle seam has no `disarm`. An assertion failing between the
/// two points leaves production held at both, so the case would run into this
/// server's thirty-second shutdown deadline instead of failing where it failed.
const HELD_PERMIT_WAIT: ConnectionOwnerEdge = ConnectionOwnerEdge::PermitWaitPending;
const HELD_SELECT_BOUNDARY: ServerStopEdge = ServerStopEdge::BeforeSupervisorSelect;

/// Release both held waits, on every exit path.
///
/// The successful path releases both itself and asserts that it did, so this
/// finds nothing left. A release with nothing paused reports an error, which is
/// this guard finding its work already done rather than a fault to raise —
/// raising it during an unwind would replace the case's own failure.
///
/// The two are released by name rather than from one list: they belong to
/// different owners, so no single vocabulary can spell both.
struct HeldWaits<'a>(&'a ScopedSupervisedRegistration);

impl Drop for HeldWaits<'_> {
    fn drop(&mut self) {
        let _ = self.0.connections.release(HELD_PERMIT_WAIT);
        let _ = self.0.stop.release(HELD_SELECT_BOUNDARY);
    }
}

// 4.T3: the refused registration leaves the connection permit where it was.
//
// Under a limit of one, the parked peer can be served only by the permit the
// upgrade's connection holds, and the checkpoint it parks at says so without a
// clock. The abort is then entered while the refusal is still inside policy: it
// forces every owned task down, but only once each connection it refused has
// released its permit, so the peer receives its whole `503` rather than losing
// its transport mid-answer.
#[test]
fn refused_upgrade_registration_holds_its_connection_permit() {
    runtime::builder()
        .connection_limit(1)
        .shutdown_timeout(Duration::from_secs(30))
        .run(|| {
            runtime::block_on(async {
                let journal = Journal::default();
                let dispatched = Arc::new(AtomicUsize::new(0));
                let (router, mut mapping) = held_mapping_router(&journal, &dispatched);
                let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
                let addr = listener.local_addr().unwrap();
                let owners = supervised_registration(addr).unwrap();
                // Adopted rather than held bare, for the reason the unlimited
                // case gives: `ServerHandle` has no `Drop`, so the guard owes the
                // cancel and the join until this case sends its own cancel.
                let served = ReadyServer::adopt(
                    addr,
                    camber::http::serve_background(listener, router)
                        .expect("owned server requires a Tokio runtime"),
                );
                // Declared after the server so it drops first: an unwind has to
                // release the waits before anything waits on this server's join.
                let held = HeldWaits(&owners);

                let calibrated = calibrate_limited_dispatch(&owners, addr, &dispatched).await;

                let mut client = accept_idle_peer_holding_the_permit(&owners, addr).await;
                let parked = park_second_peer(&owners, addr).await;
                observe_parked_permit_wait(&owners).await;
                let handle = served.into_handle();
                refuse_the_offered_upgrade(&owners, &mut client, &handle).await;
                begin_abort_over_the_held_refusal(&owners, &mut mapping).await;
                crate::assert_refused_upgrade_wire(&mut client, "the permit-held refused upgrade")
                    .await;

                // Both waits are released only here: the parked peer's outlived
                // the whole refusal, so no part of mapping handed the slot on.
                // Asserted rather than left to `held`, because a release that
                // reports nothing paused is a wait production never reached.
                owners
                    .connections
                    .release(ConnectionOwnerEdge::PermitWaitPending)
                    .unwrap();
                owners
                    .stop
                    .release(ServerStopEdge::BeforeSupervisorSelect)
                    .unwrap();
                drop(held);
                drop(parked);

                assert_refusal_context(&journal, "permit-held registration refusal", None);
                assert_eq!(
                    dispatched.load(Ordering::SeqCst) - calibrated,
                    0,
                    "no bridge is dispatched for an upgrade that was never admitted"
                );
                // Bounded well inside this server's shutdown deadline, because
                // the abort forces nothing down until every connection it
                // refused has released its permit: a refusal that kept the slot
                // would end this server on that deadline instead of here.
                crate::assert_cancelled(
                    crate::bounded(
                        handle.into_future(),
                        "the refused connection never released the permit the abort waits for",
                    )
                    .await,
                );
            });
        })
        .expect("the connection-limited fixture runtime ran to completion");
}
