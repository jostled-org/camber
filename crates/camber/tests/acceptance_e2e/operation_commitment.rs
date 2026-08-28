//! What an admitted operation commits when nothing ever answers its peer.
//!
//! Every other producer root states which owner took an operation's response
//! head. This one states the outcome none of them can: an operation that was
//! admitted, classified, and reading its payload when its peer left, when the
//! stream it opened was reset, or when its server was cancelled — and so reached
//! no producer at all. Absence is the fact here. The cell holds the cause that
//! ended the operation, and it never holds an origin, because no owner produced
//! a head to name one with.
//!
//! Each row reads that cell after every owner has settled and the server has
//! joined, which is past every finalizer this listener runs. An origin read
//! before that point would only say that nothing had happened yet; read after
//! it, the same absence says no later owner manufactured a producer for a
//! response that never existed.

use crate::common;
use crate::http as wire;

use camber::http::mock::{
    InboundTerminal, ResponseCommit, ScopedStoppedCommitment, ServerStopEdge,
};
use camber::http::{BodyAdmission, BodyAdmissionContext, Request, Response, Router, ServerHandle};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// The route every row admits a payload on and never gets to answer.
const HELD_ROUTE: &str = "/held";

/// The origin this root's mapper records under.
///
/// No row expects to see it. It is named so a mapper that did run is reported as
/// this listener's own rather than as an unattributed entry.
const ROUTE_OWNER: &str = "absent-origin-route";

/// The payload maximum every row admits under.
///
/// Far above what any row sends: a crossing would be a framework refusal, which
/// is a producer, and every row here is about the operations that reach none.
const CEILING: usize = 64 * 1024;

/// How long any one wait, exchange, or teardown here may take.
const BOUND: Duration = Duration::from_secs(10);

/// One row's listener, its observer, and what its owners hand back.
///
/// One listener per row, because two of the three endings take the server with
/// them and every row asks whether the address it served on came back.
struct AbsentOriginFixture {
    controller: Arc<ScopedStoppedCommitment>,
    addr: SocketAddr,
    journal: common::Journal,
    /// Every admitted payload's permit, counted where it is released.
    released: Arc<AtomicUsize>,
    /// Every time the route's own producer ran.
    answered: Arc<AtomicUsize>,
}

/// The handler every row would have been answered by.
///
/// A real producer rather than a hold: each row ends its operation while the
/// payload is still arriving, so the claim is that the owner which *would* have
/// committed never ran. A route with nothing to commit could not state that.
fn echoing_producer(
    answered: &Arc<AtomicUsize>,
) -> impl Fn(&Request) -> Pin<Box<dyn Future<Output = Response> + Send>> + Send + Sync + 'static {
    let answered = Arc::clone(answered);
    move |request: &Request| {
        answered.fetch_add(1, Ordering::SeqCst);
        let body: Box<str> = request.body().into();
        Box::pin(async move { Response::text(200, &body).expect("valid echo status") })
    }
}

/// The one router every row serves, and the counters it reports through.
fn admitting_router(fixture: &AbsentOriginFixture) -> Router {
    let mut router = Router::new();
    router.post(HELD_ROUTE, echoing_producer(&fixture.answered));
    let released = Arc::clone(&fixture.released);
    router
        .max_request_body(CEILING)
        .body_admission(move |_context: &BodyAdmissionContext<'_>| {
            Ok(BodyAdmission::with_permit(
                CEILING,
                wire::permit_probe(&released),
            ))
        })
        .rejection_mapper(common::recording_mapper(&fixture.journal, ROUTE_OWNER))
}

impl AbsentOriginFixture {
    /// Reserve one observed listener and build the router that will serve it.
    fn reserved() -> (Self, wire::ObservedPort<ScopedStoppedCommitment>, Router) {
        let port = wire::reserve_stopped_commitment();
        let fixture = Self {
            controller: port.controller(),
            addr: port.addr(),
            journal: common::journal(),
            released: Arc::new(AtomicUsize::new(0)),
            answered: Arc::new(AtomicUsize::new(0)),
        };
        let router = admitting_router(&fixture);
        (fixture, port, router)
    }

    /// Serve one row through the guarded fixture, which stops it on request.
    fn served() -> (Self, wire::ObservedServer<ScopedStoppedCommitment>) {
        let (fixture, port, router) = Self::reserved();
        let server = port.serve(router);
        (fixture, server)
    }

    /// Serve one row through the raw handle its own ending needs.
    ///
    /// The cancellation row forces the stop itself, at the moment its operation
    /// is in the payload owner's hands, and then joins on what that left. A
    /// guarded fixture stops gracefully and only at teardown, so it can express
    /// neither half.
    fn served_owned() -> (Self, ServerHandle) {
        let (fixture, port, router) = Self::reserved();
        let (listener, _, _) = port.into_owned_parts();
        let handle = camber::http::serve_background(listener, router)
            .expect("the owned server requires a Tokio runtime");
        (fixture, handle)
    }

    /// Open one peer that promises a payload and never finishes sending it.
    fn stall_http1_payload(&self, label: &str) -> std::net::TcpStream {
        let mut peer = wire::connect(self.addr)
            .unwrap_or_else(|error| panic!("{label}: the peer could not connect: {error}"));
        wire::write_stalled_body(&mut peer, Some(wire::KEEP_CONNECTION), "POST", HELD_ROUTE)
            .unwrap_or_else(|error| {
                panic!("{label}: the promise of a payload was not sent: {error}")
            });
        peer
    }

    /// Wait, bounded, until the payload owner has this operation in hand.
    ///
    /// The row's ending is only about an admitted operation once one exists, and
    /// the payload owner's own read is the last thing that happens before the
    /// producer would run. A row that ended its peer before that could be
    /// reading an empty cell because nothing was ever admitted.
    async fn await_admitted_payload(&self, label: &str) {
        let controller = Arc::clone(&self.controller);
        assert!(
            wire::arrived(BOUND, move || controller
                .commitment
                .operations_observed()
                .body
                >= 1)
            .await,
            "{label}: the payload owner never read this operation's identity",
        );
    }

    /// Wait, bounded, until this row's server stop is held at `edge`.
    async fn hold_stop(&self, label: &str, edge: ServerStopEdge) {
        wire::wait_until_paused_within(
            &self.controller,
            edge,
            BOUND,
            &format!("{label}: the stop owner never reached {edge:?}"),
        )
        .await;
    }

    /// Wait, bounded, until a cause has taken this operation's commitment.
    ///
    /// Which cause it is belongs to [`Self::assert_origin_absent`]; all this
    /// says is that the cell is no longer empty, which is what makes a held
    /// abort safe to release.
    async fn await_committed_cause(&self, label: &str) {
        let controller = Arc::clone(&self.controller);
        assert!(
            wire::arrived(BOUND, move || controller
                .commitment
                .observed()
                .committed
                .is_some())
            .await,
            "{label}: no cause took this operation's commitment",
        );
    }

    /// Assert this row's operation committed one of `causes` and no origin.
    ///
    /// Read after the server has joined, so what the cell holds is what it was
    /// left holding rather than what it held mid-flight. The cause is named
    /// rather than merely required to be a cause: a row that accepted any
    /// non-origin would pass on an operation ended by something other than the
    /// peer or the stop it drove.
    fn assert_origin_absent(&self, label: &str, causes: &[InboundTerminal]) {
        let observed = self.controller.commitment.observed();
        assert!(
            matches!(
                observed.committed,
                Some(ResponseCommit::Cause(terminal)) if causes.contains(&terminal),
            ),
            "{label}: this ending commits one of {causes:?} and names no origin: {observed:?}",
        );
        assert_eq!(
            observed.commits, 1,
            "{label}: one operation settles exactly one commitment: {observed:?}",
        );
        let operations = self.controller.commitment.operations_observed();
        assert_eq!(
            operations.admitted, 1,
            "{label}: the ending found exactly one admitted operation: {operations:?}",
        );
        assert_eq!(
            self.answered.load(Ordering::SeqCst),
            0,
            "{label}: the route's own producer never ran",
        );
        // A peer that goes with payload still owed is read either as its own
        // departure or as a source that failed on a transport already gone. The
        // second reading is classified, so a mapper may run — and its head
        // reaches a cell the cause above already holds. That is the whole
        // "manufactures no head" claim, stated as arithmetic: every mapping this
        // ending produced is counted among the owners that arrived late, and
        // none of them is the owner that took the cell.
        let mapped = common::drain(&self.journal);
        assert!(
            mapped.len() <= 1,
            "{label}: an ending nothing can answer is classified at most once: {mapped:?}",
        );
        assert_eq!(
            observed.late,
            mapped.len(),
            "{label}: a mapper that ran here found the cell already held: {observed:?} {mapped:?}",
        );
    }

    /// Assert the permit this operation admitted came back exactly once.
    fn assert_permit_returned(&self, label: &str) {
        wire::assert_released(&self.released, 1, label);
    }

    /// Assert the address this row served on is bindable again.
    async fn assert_address_reusable(&self, label: &str) {
        let rebound = wire::rebind_within(self.addr, BOUND).await;
        assert!(
            rebound.is_ok(),
            "{label}: the address this row served on stayed held: {rebound:?}",
        );
    }
}

/// Invariant 9
///
/// An admitted operation that ends before any response producer commits leaves
/// its response origin absent, and every owner behind it still settles.
///
/// Three endings, one per way an operation can be taken from its producers: the
/// peer leaves, the peer resets the stream it opened, and the server is
/// cancelled underneath both. None of them can be answered — the cause arrives
/// while the payload is still being read, so no head is ever produced — and each
/// leaves the same nothing behind: a committed cause with no origin in it, every
/// mapping counted among the owners that arrived too late to take the cell, the
/// admitted permit returned, and the address free.
///
/// Every row drives a real ending of its own. A row that completed its payload
/// would hold this claim with the whole commitment unwired, because a producer
/// that answered normally and a producer that never ran are told apart only by
/// what the cell was left holding.
#[tokio::test(flavor = "multi_thread")]
async fn operation_ending_before_response_head_keeps_origin_absent() {
    assert_peer_disconnect_leaves_no_origin().await;
    assert_stream_reset_leaves_no_origin().await;
    assert_server_cancellation_leaves_no_origin().await;
}

/// The two readings one departing peer's own read can answer with.
///
/// The peer is gone either way. Which of the two the payload owner sees is the
/// transport's to decide — the signal arrives while the read is parked, or the
/// read itself fails on a socket that is already closed — and neither is an
/// origin, which is the whole of what these rows claim.
const PEER_LEFT: [InboundTerminal; 2] =
    [InboundTerminal::Disconnect, InboundTerminal::SourceFailure];

/// A peer that promises a payload and goes away before finishing it.
async fn assert_peer_disconnect_leaves_no_origin() {
    let label = "http/1 peer disconnect";
    let (fixture, server) = AbsentOriginFixture::served();
    let peer = fixture.stall_http1_payload(label);
    fixture.await_admitted_payload(label).await;
    drop(peer);

    fixture.assert_permit_returned(label);
    server
        .shutdown_bounded(BOUND)
        .expect("the disconnect fixture tore down");
    fixture.assert_origin_absent(label, &PEER_LEFT);
    fixture.assert_address_reusable(label).await;
}

/// A peer that resets the one stream it opened, leaving its payload unsent.
///
/// The reset reaches the payload owner as the end of this operation's response
/// lifetime, exactly as a closed HTTP/1 transport does. Either it arrives while
/// the read is parked, or the read fails on a stream that is already gone, so
/// both terminals are this ending — and neither is an origin.
async fn assert_stream_reset_leaves_no_origin() {
    let label = "http/2 stream reset";
    let (fixture, server) = AbsentOriginFixture::served();

    let mut client = common::PersistentH2Client::connect(fixture.addr, BOUND).await;
    let mut upload = client
        .open_paced("POST", HELD_ROUTE, "localhost", &[])
        .await;
    assert_eq!(
        upload.offer(b"partial", BOUND).await,
        common::H2Offer::Sent,
        "{label}: the first payload frame reached the server",
    );
    fixture.await_admitted_payload(label).await;
    upload.reset();
    drop(upload);
    // Settled rather than aborted: the claim is that the peer's reset reached
    // the server, and an aborted connection can take the queued `RST_STREAM`
    // down with it. The stream just reset is the only one this connection
    // opened, so nothing is left for the connection to wait on.
    client.close_settled().await;

    fixture.assert_permit_returned(label);
    server
        .shutdown_bounded(BOUND)
        .expect("the reset fixture tore down");
    fixture.assert_origin_absent(label, &PEER_LEFT);
    fixture.assert_address_reusable(label).await;
}

/// A server cancelled while one admitted payload is still arriving.
///
/// The command publishes one transition, and both the operation reading it and
/// the supervisor aborting on it are woken by that single publish. So the
/// supervisor is held where it selected the transition and released only once
/// the cancellation has taken the cell: an abort let go any earlier drops the
/// operation mid-turn, and dropped work commits nothing — which is a race
/// between two woken tasks rather than the absent origin this row is about.
async fn assert_server_cancellation_leaves_no_origin() {
    let label = "forced server cancellation";
    let (fixture, handle) = AbsentOriginFixture::served_owned();
    let peer = fixture.stall_http1_payload(label);
    fixture.await_admitted_payload(label).await;

    // Armed before the command, so the supervisor stops on the selection rather
    // than past it. A held supervisor applies no phase and aborts no connection.
    let supervisor = ServerStopEdge::SupervisorSelectedControl;
    wire::arm_point(&fixture.controller, supervisor, label);

    handle.cancel();

    fixture.hold_stop(label, supervisor).await;
    fixture.await_committed_cause(label).await;
    wire::release_point(&fixture.controller, supervisor, label);
    assert!(
        tokio::time::timeout(BOUND, handle).await.is_ok(),
        "{label}: the cancelled server joined",
    );
    drop(peer);

    fixture.assert_permit_returned(label);
    fixture.assert_origin_absent(label, &[InboundTerminal::ForcedCancellation]);
    fixture.assert_address_reusable(label).await;
}
