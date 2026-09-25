#![cfg(feature = "ws")]

//! What becomes of a backend the proxy already connected.
//!
//! Negotiation settles the backend before the peer hears anything, so every
//! way the handoff that follows can fail leaves a live backend transport in
//! the request's hands. These rows walk that perimeter: a peer that went away,
//! a stop that committed, a transfer the peer never saw — and the one that
//! succeeds, where the transport must survive intact, read-ahead and all.
//!
//! Each refusal row owns its runtime. The aggregate shutdown grace is minted
//! by the first graceful transition anywhere in a runtime and never restarted,
//! so rows that stop a server cannot share one. The connection limit of one
//! that comes with it is what makes a released permit observable: a probe
//! after the refusal is served only by a server that got its slot back.
//!
//! The whole perimeter one proxied upgrade crosses, and what witnesses each
//! cell's cleanup:
//!
//! | Cell | Witness | Owner |
//! | --- | --- | --- |
//! | pre-connect failure — no transport exists | the scripted backend accepted nothing, and `finish` proves its address free | `backend_negotiation`, `websocket_forwarding` |
//! | connected, unvalidated — the answer broke its handshake | the backend's `Released`, after a downstream refusal | `backend_negotiation` |
//! | validated, unregistered — the connection would not own the bridge | the backend's `Released`, the owner's join, and the permit probe | this file's 7.T1 |
//! | registered, uncommitted — the peer never saw the `101` | the backend's `Released` with no frame ever exchanged | this file's 7.T1 |
//! | committed bridge — the handoff succeeded | the close answered on both halves, then `Released` and the returned permit | this file's 7.T2 |

use crate::backend_negotiation::{
    BUFFERED, CloseReply, NegotiatingProxy, assert_bridge_closed, assert_greetings_exchanged,
    assert_mapped_before_upgrade, assert_offer_arrived, offer, read_bridged_text, route_of,
};
use crate::common::{
    CLOSE_AFTER_RESPONSE, Collapsed, assert_address_reused, assert_classification, assert_http_ok,
    attach_dispatch_probe, await_committed_stop, lifecycle_event, read_async_http_head,
    read_async_http_head_or_eof, request_on_new_peer, reserve_registered, status_from_raw,
};
use crate::ws_backend_script::{
    BACKEND_FOLLOWUP, BackendEvent, BackendScript, ScriptedWsBackend, switching_without_selection,
};
use camber::RuntimeError;
use camber::http::mock::{
    ConnectionOwnerEdge, ScopedConnectionOwner, ScopedRegistrationSelection, UpgradeOwnerEdge,
    connection_owner, registration_selection,
};
use camber::http::{RejectionKind, Request, Response, Router};
use camber::runtime;
use std::future::Future;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

/// The accept index every row's one scripted backend connection is served at.
const ROW: usize = 0;

/// The path a probe asks for to prove the refused connection freed its permit.
const PROBE: &str = "/probe";

/// The header bound every row's runtime serves under.
///
/// The values `#[camber::test]` would have established, so a row isolated in a
/// runtime of its own proves what it would have proved inside a case runtime.
const ROW_HEADER_TIMEOUT: Duration = Duration::from_secs(5);

/// The grace every row's one stop drains under.
const ROW_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// The classification a connection that would not own the upgrade keeps.
const REGISTRATION_REFUSED: Collapsed<'static> = Collapsed {
    kind: RejectionKind::InternalService,
    status: 503,
    message: "service unavailable",
};

// ── 7.T1 ───────────────────────────────────────────────────────────

/// How one row refuses the handoff of a backend it has already connected.
#[derive(Clone, Copy)]
enum Refusal {
    /// The peer goes away while the connection is about to answer the offer.
    PeerAbandoned,
    /// The peer goes away after the transfer is recorded and before the answer
    /// that would have released its `101`.
    RecordedTransferAbandoned,
    /// A graceful stop commits before the connection answers the offer.
    GracefulStop,
    /// A forced stop commits before the connection answers the offer.
    ForcedStop,
    /// The peer's write half closes and a graceful stop commits, in no order
    /// either the row or the server fixes.
    AbandonedUnderStop,
}

/// What the peer may read once the handoff has been refused.
///
/// A set rather than a value wherever two independent facts became ready
/// together: the row admits either member and no `101` under any of them.
#[derive(Clone, Copy)]
enum PeerOutcome {
    /// The peer is gone; nothing may be claimed about a head it cannot read.
    Gone,
    /// The peer reads the configured mapper's refusal, and never a `101`.
    Mapped,
    /// The peer kept its read half, so either the mapper's refusal or a closed
    /// transport is admissible — and a `101` is neither.
    MappedOrClosed,
}

impl Refusal {
    /// Where the connected backend is held while the refusal is arranged.
    fn hold(self) -> UpgradeOwnerEdge {
        match self {
            // The one edge from which the transfer is already recorded and the
            // peer provably cannot have seen the answer that releases a `101`.
            Self::RecordedTransferAbandoned => UpgradeOwnerEdge::AfterTransferRecorded,
            Self::PeerAbandoned
            | Self::GracefulStop
            | Self::ForcedStop
            | Self::AbandonedUnderStop => UpgradeOwnerEdge::BeforeTransferAcknowledge,
        }
    }

    /// Whether the row waits for its peer's closure to be observed before it
    /// releases the held handoff.
    ///
    /// Only the rows that give the peer up entirely: the observation is what
    /// makes the refusal the connection's own decision rather than a race with
    /// the answer it was about to give.
    fn awaits_peer_closure(self) -> bool {
        match self {
            Self::PeerAbandoned | Self::RecordedTransferAbandoned => true,
            Self::GracefulStop | Self::ForcedStop | Self::AbandonedUnderStop => false,
        }
    }

    /// What this row's peer may read afterwards.
    fn peer_outcome(self) -> PeerOutcome {
        match self {
            Self::PeerAbandoned | Self::RecordedTransferAbandoned => PeerOutcome::Gone,
            Self::GracefulStop | Self::ForcedStop => PeerOutcome::Mapped,
            Self::AbandonedUnderStop => PeerOutcome::MappedOrClosed,
        }
    }

    /// Whether the server outlives the refusal, and so can answer the probe
    /// that proves the refused connection gave its permit back.
    fn server_survives(self) -> bool {
        match self {
            Self::PeerAbandoned | Self::RecordedTransferAbandoned => true,
            Self::GracefulStop | Self::ForcedStop | Self::AbandonedUnderStop => false,
        }
    }

    /// What the owner returns once this row's stop has drained.
    fn owner_end(self) -> OwnerEnd {
        match self {
            Self::ForcedStop => OwnerEnd::Cancelled,
            Self::PeerAbandoned
            | Self::RecordedTransferAbandoned
            | Self::GracefulStop
            | Self::AbandonedUnderStop => OwnerEnd::Clean,
        }
    }
}

/// How one row's server owner must have ended.
#[derive(Clone, Copy)]
enum OwnerEnd {
    Clean,
    Cancelled,
}

/// A router proxying to `backend`, beside a probe the permit claim reads.
fn refused_handoff_router(backend: &str) -> Router {
    let mut router = Router::new();
    router.proxy(BUFFERED, backend);
    router.get(PROBE, |_: &Request| async { Response::text(200, "probe") });
    router
}

/// Run one row under a runtime, a connection limit, and a thread of its own.
fn isolated_row<F: Future<Output = ()>>(
    label: &'static str,
    row: impl FnOnce() -> F + Send + 'static,
) {
    std::thread::spawn(move || {
        runtime::builder()
            .connection_limit(1)
            .header_timeout(ROW_HEADER_TIMEOUT)
            .shutdown_timeout(ROW_SHUTDOWN_TIMEOUT)
            .run(|| runtime::block_on(row()))
            .unwrap_or_else(|error| panic!("{label}: the row runtime failed: {error:?}"));
    })
    .join()
    .unwrap_or_else(|_| panic!("{label}: the row panicked"));
}

/// Run one refusal row in isolation.
fn refusal_row(refusal: Refusal, label: &'static str) {
    isolated_row(label, move || drive_refused_handoff(refusal, label));
}

/// Hold a connected, validated backend at the handoff, refuse it, and require
/// that the backend was released and no peer was ever told otherwise.
async fn drive_refused_handoff(refusal: Refusal, label: &str) {
    let mut backend = ScriptedWsBackend::bind(Box::new([BackendScript::Negotiated(
        switching_without_selection,
    )]))
    .await;
    let (listener, proxy_addr, controller) =
        reserve_registered(registration_selection).into_owned_parts();
    controller
        .upgrades
        .pause_once(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("pause after the offer is submitted");
    controller
        .upgrades
        .pause_once(refusal.hold())
        .expect("pause the held handoff");
    match refusal.awaits_peer_closure() {
        true => controller
            .upgrades
            .pause_once(UpgradeOwnerEdge::PeerClosed)
            .expect("pause after the peer's closure is observed"),
        false => {}
    }
    let proxy = NegotiatingProxy::serve_on(listener, refused_handoff_router(&backend.http_url()));

    let peer = offer(proxy_addr, BUFFERED, &[]).await;
    assert_offer_arrived(&mut backend, ROW, label).await;
    controller
        .upgrades
        .wait_until_paused(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .await
        .expect("the offer reaches its connection");
    controller
        .upgrades
        .release(UpgradeOwnerEdge::AfterHandoffSubmitted)
        .expect("release the submitted offer");
    controller
        .upgrades
        .wait_until_paused(refusal.hold())
        .await
        .expect("the handoff reaches its held edge");

    let peer = refuse_held_handoff(refusal, &controller, &proxy, peer, label).await;
    assert_peer_outcome(refusal, peer, &proxy, label).await;
    // The backend is let go, and nothing ever bridged it: `finish` below fails
    // on any report no row consumed, and only a bridge produces one.
    backend
        .expect(
            BackendEvent::Released(ROW),
            &format!("{label}: the connected backend was released"),
        )
        .await;

    match refusal.server_survives() {
        true => {
            assert_http_ok(proxy_addr, PROBE, &format!("{label}: the permit probe")).await;
            assert_owner_joined(proxy.stop(), OwnerEnd::Clean, label).await;
        }
        false => assert_owner_joined(proxy.join(), refusal.owner_end(), label).await,
    }
    backend.finish(label).await;
    assert_address_reused(proxy_addr, &format!("{label}: the proxy listener")).await;
    drop(controller);
}

/// Refuse the held handoff the way this row does, and hand back the peer the
/// row left, if it left one.
async fn refuse_held_handoff(
    refusal: Refusal,
    controller: &ScopedRegistrationSelection,
    proxy: &NegotiatingProxy,
    mut peer: TcpStream,
    label: &str,
) -> Option<TcpStream> {
    let kept = match refusal {
        Refusal::PeerAbandoned | Refusal::RecordedTransferAbandoned => {
            drop(peer);
            None
        }
        Refusal::GracefulStop => {
            proxy.handle().shutdown();
            await_committed_stop(controller, label).await;
            Some(peer)
        }
        Refusal::ForcedStop => {
            proxy.handle().cancel();
            await_committed_stop(controller, label).await;
            Some(peer)
        }
        Refusal::AbandonedUnderStop => {
            // Half-closed rather than dropped: the peer stops writing and keeps
            // its read half, so the row can still say what it was — or was not
            // — told while its closure and the stop were both on their way.
            peer.shutdown()
                .await
                .expect("half-close the abandoning peer");
            proxy.handle().shutdown();
            await_committed_stop(controller, label).await;
            Some(peer)
        }
    };
    match refusal.awaits_peer_closure() {
        true => {
            lifecycle_event(
                "the owned reader observes the peer's closure",
                controller
                    .upgrades
                    .wait_until_paused(UpgradeOwnerEdge::PeerClosed),
            )
            .await
            .expect("the owned reader observes the peer's closure");
            controller
                .upgrades
                .release(UpgradeOwnerEdge::PeerClosed)
                .expect("release the observed peer closure");
        }
        false => {}
    }
    controller
        .upgrades
        .release(refusal.hold())
        .expect("release the held handoff");
    kept
}

/// Require that the peer this row left, if any, read what its outcome admits.
async fn assert_peer_outcome(
    refusal: Refusal,
    peer: Option<TcpStream>,
    proxy: &NegotiatingProxy,
    label: &str,
) {
    let outcome = refusal.peer_outcome();
    let mut peer = match (outcome, peer) {
        (PeerOutcome::Gone, None) => return,
        (PeerOutcome::Gone, Some(_)) | (_, None) => {
            panic!("{label}: the row and its outcome disagree about a peer")
        }
        (_, Some(peer)) => peer,
    };
    // End of stream is an answer for the row whose peer closed its own write
    // half: it admits a server that dropped the exchange, and what it never
    // admits is a `101`.
    let read = read_async_http_head_or_eof(&mut peer, "the refused downstream head").await;
    let head = match (outcome, read) {
        (_, Some(head)) => head,
        (PeerOutcome::MappedOrClosed, None) => return,
        (_, None) => panic!("{label}: the peer was told nothing at all"),
    };
    let seen = assert_mapped_before_upgrade(&head, proxy, &route_of(BUFFERED), label);
    assert_classification(&seen, &REGISTRATION_REFUSED, label);
}

/// Require the owner joined, and joined the way this row's stop says.
///
/// The join is what says the connection settled: it cannot end — and so cannot
/// give its permit back — before the handshake and upgrade owners beneath it
/// have.
async fn assert_owner_joined(
    join: impl Future<Output = Result<(), RuntimeError>>,
    expected: OwnerEnd,
    label: &str,
) {
    let result = lifecycle_event("the proxy owner join", join).await;
    let joined = match expected {
        OwnerEnd::Clean => result.is_ok(),
        OwnerEnd::Cancelled => matches!(result, Err(RuntimeError::Cancelled)),
    };
    assert!(joined, "{label}: the proxy owner joined as {result:?}");
}

/// 7.T1
///
/// Every refused handoff releases the backend it had already connected, and
/// none of them commits a downstream upgrade. The rows differ only in what
/// refuses the handoff; what each one requires of the backend, the owners, the
/// permit, and the listener is the same.
#[test]
fn connected_backend_is_released_on_every_refused_handoff() {
    refusal_row(Refusal::PeerAbandoned, "the peer abandoned its held offer");
    refusal_row(
        Refusal::RecordedTransferAbandoned,
        "the peer abandoned a recorded transfer",
    );
    refusal_row(Refusal::GracefulStop, "a graceful stop refused the handoff");
    refusal_row(Refusal::ForcedStop, "a forced stop refused the handoff");
    refusal_row(
        Refusal::AbandonedUnderStop,
        "abandonment and a graceful stop arrived together",
    );
}

// ── 7.T2 ───────────────────────────────────────────────────────────

/// 7.T2
///
/// The one handoff that succeeds. The backend writes its `101` and its first
/// frame in a single write, and that frame reaches the peer exactly once —
/// Hyper's upgraded stream carries the bytes read behind the head, and the
/// bridge frames over that stream rather than over a socket rebuilt beneath
/// it. Frames then cross both ways, the close is answered, and the permit the
/// bridge holds is given back only once the bridge has settled.
#[test]
fn validated_backend_preserves_read_ahead_and_owned_handoff() {
    let label = "the owned proxied handoff";
    isolated_row(label, move || drive_owned_handoff(label));
}

async fn drive_owned_handoff(label: &str) {
    let mut backend = ScriptedWsBackend::bind(Box::new([BackendScript::ReadAhead(
        switching_without_selection,
    )]))
    .await;
    let (listener, proxy_addr, controller) =
        reserve_registered(connection_owner).into_owned_parts();
    let mut router = Router::new();
    router.proxy(BUFFERED, &backend.http_url());
    let mut dispatched = attach_dispatch_probe(&mut router);
    let proxy = NegotiatingProxy::serve_on(listener, router);

    let mut peer = offer(proxy_addr, BUFFERED, &[]).await;
    assert_offer_arrived(&mut backend, ROW, label).await;
    let head = read_async_http_head(&mut peer, "the upgraded downstream head").await;
    assert_eq!(status_from_raw(&head), 101, "{label}: {head}");

    assert_read_ahead_arrives_once(&mut peer, &mut backend, label).await;
    let waiting = hold_a_permit_waiter(&controller, proxy_addr, label).await;
    assert!(
        matches!(
            dispatched.try_recv(),
            Err(tokio::sync::oneshot::error::TryRecvError::Empty)
        ),
        "{label}: a second request dispatched while the bridge held the permit"
    );

    // The peer must read a close frame back before its transport ends. A bare
    // shutdown would leave it reading `1006`, the code for a connection that
    // simply dropped, and nothing else downstream tells it the bridge ended on
    // purpose — so an acknowledgement this row admitted as absent is the one
    // regression the committed-bridge cell exists to catch.
    assert_bridge_closed(&mut peer, &mut backend, ROW, CloseReply::Required, label).await;
    controller
        .release(ConnectionOwnerEdge::PermitWaitPending)
        .expect("release the pending permit wait");
    assert_permit_returned(waiting, label).await;

    assert_owner_joined(proxy.stop(), OwnerEnd::Clean, label).await;
    backend.finish(label).await;
    assert_address_reused(proxy_addr, &format!("{label}: the proxy listener")).await;
    drop(controller);
}

/// Require the frame that arrived behind the `101` to reach the peer once, and
/// a later frame each way to cross the live bridge.
///
/// The follow-up is what makes "once" countable: it is the next frame the peer
/// reads, so a greeting delivered twice arrives where this is expected.
async fn assert_read_ahead_arrives_once(
    peer: &mut TcpStream,
    backend: &mut ScriptedWsBackend,
    label: &str,
) {
    // The greeting is the frame the backend wrote behind its head.
    assert_greetings_exchanged(peer, backend, ROW, label).await;

    let payload = read_bridged_text(peer, "the backend's later frame", label).await;
    assert_eq!(
        payload.as_ref(),
        BACKEND_FOLLOWUP,
        "{label}: the frame behind the head reached the peer more than once"
    );
}

/// Park a second peer on the permit the live bridge is holding.
async fn hold_a_permit_waiter(
    controller: &ScopedConnectionOwner,
    addr: std::net::SocketAddr,
    label: &str,
) -> TcpStream {
    controller
        .pause_once(ConnectionOwnerEdge::PermitWaitPending)
        .expect("pause when the permit wait becomes pending");
    let waiting = lifecycle_event(
        label,
        request_on_new_peer(addr, "/second", CLOSE_AFTER_RESPONSE),
    )
    .await;
    controller
        .wait_until_paused(ConnectionOwnerEdge::PermitWaitPending)
        .await
        .expect("the permit acquisition returned pending");
    waiting
}

/// Require the parked waiter to be served once the bridge has settled.
async fn assert_permit_returned(mut waiting: TcpStream, label: &str) {
    let head = read_async_http_head(&mut waiting, "the permit waiter's response").await;
    assert_eq!(
        status_from_raw(&head),
        200,
        "{label}: the settled bridge did not give its permit back: {head}"
    );
}
