//! WebSocket: the `101` handoff is where the HTTP response lifetime ends.
//!
//! The upgraded peer's lifetime belongs to the WebSocket close contract, not to
//! the HTTP response, so Camber's last observation of the request is the
//! transport handoff. Every scenario here enters through the production
//! middleware gate, which runs before that handoff — a watcher armed there
//! survives the service future Hyper drops, exactly as a real producer's would.
//!
//! The gate must never await the signal itself: `Completed` is set at the
//! handoff the gate has to pass to reach, so awaiting there would deadlock the
//! request being observed.
//!
//! A `101` carries an empty body, and the response body's generic
//! empty-response rule is deliberately not allowed to claim it. The handoff in
//! `ws_proxy` owns that transition instead, which is why removing it reports
//! `StreamReset` for a successful upgrade rather than leaving these cases green
//! by default.

use super::fixture::{BOUND, try_bounded};
use super::peer::{PeerClose, closed_within_bound, send};
use super::probes::{CauseProbe, relay};
use super::routes::{Hold, gate_probe, probe_router};
use super::servers::{SyncServer, await_lifecycle_pause, with_scripted_server};
use camber::RuntimeError;
use camber::http::mock::LifecycleCheckpoint;
use camber::http::{DisconnectCause, DisconnectSignal, Request, Router, WsConn};
use camber::runtime;
use std::future::IntoFuture;
use std::net::TcpStream;
use std::sync::mpsc::Receiver;
use std::time::Instant;

/// The route whose upgrade succeeds.
const WS_PATH: &str = "/ws";

/// The route the server refuses a handshake on.
const REJECT_PATH: &str = "/ws-reject";

/// The route whose request is held in flight so its peer can abandon it.
const ABORT_PATH: &str = "/ws-abort";

/// The route whose upgrade the supervisor rejects before acknowledging it.
const REFUSED_PATH: &str = "/ws-refused";

/// What a WebSocket handler read from its signal after its peer closed.
#[derive(Debug, Eq, PartialEq)]
enum AfterClose {
    /// The terminal cause the signal reported.
    Resolved(DisconnectCause),
    /// The signal was still unresolved when the handler's bound expired, which
    /// is a different failure from a cause that changed.
    Unresolved,
    /// The peer never closed within the bound, so no cause read after it would
    /// belong to the close this observation is named for.
    PeerNeverClosed,
    /// Receiving from the upgraded transport failed before its peer closed.
    ReceiveFailed(Box<str>),
}

/// Register a WebSocket route that reads its signal only after the upgraded
/// peer has gone away.
///
/// This is the retroactive-change probe: the handler runs inside the bridge,
/// long after the handoff, and reads the terminal cause once the peer's close
/// has already been processed.
fn route_ws_after_close(router: &mut Router, path: &str) -> Receiver<AfterClose> {
    let (report, causes) = relay::<AfterClose>();
    router.ws(path, move |req: &Request, mut conn: WsConn| {
        let signal = req.on_disconnect();
        // A drain that expired instead of reaching the close is reported as
        // its own outcome: the cause read after it would be the one the
        // handoff established, which is what this probe must not assume.
        report.send(match drain_until_close(&mut conn) {
            Ok(()) => cause_after_close(&signal),
            Err(RuntimeError::Timeout) => AfterClose::PeerNeverClosed,
            Err(error) => AfterClose::ReceiveFailed(error.to_string().into_boxed_str()),
        });
        Ok(())
    });
    causes
}

/// Read the terminal cause from inside the bridge, bounded.
///
/// An unresolved signal must fail the case that asked, not park the blocking
/// thread this runs on.
fn cause_after_close(signal: &DisconnectSignal) -> AfterClose {
    match try_bounded(signal.cancelled()) {
        Some(cause) => AfterClose::Resolved(cause),
        None => AfterClose::Unresolved,
    }
}

/// Register a WebSocket route that stays open until its peer closes.
///
/// Nothing is reported from here: every case that registers this route holds
/// its request short of the handler, so reaching the drain at all would already
/// have failed an earlier assertion.
fn route_ws_until_close(router: &mut Router, path: &str) {
    router.ws(path, |_req: &Request, mut conn: WsConn| {
        drain_until_close(&mut conn)
    });
}

/// Read the upgraded peer until it closes, bounded.
///
/// Every receive gets only what remains of one deadline. A peer that keeps
/// sending cannot renew the budget, and a silent peer cannot park the handler
/// beyond it.
fn drain_until_close(conn: &mut WsConn) -> Result<(), RuntimeError> {
    let deadline = Instant::now() + BOUND;
    loop {
        match conn.recv_timeout(deadline.saturating_duration_since(Instant::now()))? {
            Some(_) => {}
            None => return Ok(()),
        }
    }
}

/// What one committed upgrade reported, in the order it was observed.
struct UpgradeHandoff {
    head: Box<str>,
    at_handoff: DisconnectCause,
    after_peer_close: AfterClose,
}

/// Read the `101` off `peer`, take the cause the handoff established, then
/// close the upgraded peer and read the cause the handler saw afterwards.
///
/// Both entry points commit their `101` through the same handoff and make the
/// same observations over it. Where the sequence starts is the only difference
/// between them, so each caller supplies a peer that has already reached its
/// own starting point and nothing else.
fn observe_upgrade(
    mut peer: TcpStream,
    accepted: &CauseProbe,
    after_close: &Receiver<AfterClose>,
) -> UpgradeHandoff {
    let head = crate::common::read_until_double_crlf(&mut peer);
    let at_handoff = accepted.cause();
    crate::common::write_ws_close_frame(&mut peer);
    let after_peer_close = after_close
        .recv_timeout(BOUND)
        .expect("the WebSocket handler never reported the cause it read after its peer closed");
    drop(peer);
    UpgradeHandoff {
        head,
        at_handoff,
        after_peer_close,
    }
}

/// Require that `observed` switched protocols, resolved `Completed` at the
/// handoff, and kept that cause across its peer's close.
///
/// `entry` names the entry point that served the upgrade, so a failure states
/// which of the two branches produced it.
fn assert_upgrade_completed(observed: &UpgradeHandoff, entry: &str) {
    assert!(
        observed.head.starts_with("HTTP/1.1 101"),
        "the {entry} upgrade never switched protocols: {:?}",
        observed.head
    );
    assert_eq!(
        observed.at_handoff,
        DisconnectCause::Completed,
        "a successful {entry} upgrade did not resolve Completed at the 101 handoff"
    );
    assert_eq!(
        observed.after_peer_close,
        AfterClose::Resolved(DisconnectCause::Completed),
        "the {entry} upgrade's peer closing did not leave the cause established at the handoff"
    );
}

/// The router both entry points serve their upgrade from: the gate that watches
/// the upgraded request, and the route that reads its cause after the close.
fn upgrade_router() -> (Router, CauseProbe, Receiver<AfterClose>) {
    let mut router = probe_router();
    let accepted = gate_probe(&mut router, WS_PATH, Hold::PassThrough);
    let after_close = route_ws_after_close(&mut router, WS_PATH);
    (router, accepted, after_close)
}

/// What the three upgrade journeys reported, in the order they were observed.
struct HandoffOutcome {
    unresolved_before_handoff: bool,
    handoff: UpgradeHandoff,
    rejection_status: u16,
    rejected: DisconnectCause,
    abandoned: DisconnectCause,
}

#[test]
fn websocket_upgrade_resolves_at_handoff_or_actual_cause() {
    let (mut router, accepted, after_close) = upgrade_router();
    let refused = gate_probe(&mut router, REJECT_PATH, Hold::PassThrough);
    let held = gate_probe(&mut router, ABORT_PATH, Hold::InFlight);
    // Reached only through a handshake the server refuses, so its handler is
    // never invoked.
    route_ws_until_close(&mut router, REJECT_PATH);
    // Held in flight by its gate, so its handler is never invoked either.
    route_ws_until_close(&mut router, ABORT_PATH);

    let observed = with_scripted_server(router, |addr, handle, controller| {
        // A successful upgrade, held short of the acknowledgement that commits
        // it. Nothing has been handed off yet, so nothing may have resolved.
        controller
            .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
            .expect("the upgrade acknowledgement could not be armed");
        let peer = crate::common::start_upgrade(addr, WS_PATH);
        await_lifecycle_pause(controller, LifecycleCheckpoint::BeforeUpgradeAcknowledge);
        accepted.await_entry();
        let unresolved_before_handoff = accepted.still_unresolved();
        controller
            .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
            .expect("the held upgrade could not be released");

        // Released: the transport is handed to the WebSocket subsystem and the
        // HTTP response is over.
        let handoff = observe_upgrade(peer, &accepted, &after_close);

        // A handshake the server refuses produces a complete HTTP response.
        let rejection = send(addr, "GET", REJECT_PATH, "the refused handshake's response");
        let rejected = refused.cause();

        // A peer that abandons the handshake leaves a request in flight.
        //
        // The hold is the middleware gate, not the acknowledgement checkpoint:
        // once `hyper::upgrade::on` has taken the request, Hyper stops treating
        // read EOF as a reason to drop the connection future, so a peer dropped
        // at the checkpoint resolves nothing until the server is torn down.
        //
        // Held in the gate, this request never enters the upgrade machinery, so
        // no `101` is ever constructed and 8.1's exclusion is not what this
        // exercises. What it covers is 8.T1(c): a peer that aborts an upgrade
        // request mid-flight resolves through the ordinary cause table.
        let abandoning = crate::common::start_upgrade(addr, ABORT_PATH);
        held.await_entry();
        drop(abandoning);
        let abandoned = held.cause();

        drop(handle);
        HandoffOutcome {
            unresolved_before_handoff,
            handoff,
            rejection_status: rejection.status,
            rejected,
            abandoned,
        }
    });

    assert!(
        observed.unresolved_before_handoff,
        "the upgrade's signal resolved before its 101 was handed off"
    );
    assert_upgrade_completed(&observed.handoff, "owned-entry");
    assert_eq!(
        observed.rejection_status, 400,
        "the invalid handshake was not refused with a normal HTTP response"
    );
    assert_eq!(
        observed.rejected,
        DisconnectCause::Completed,
        "a refused handshake's produced error response resolved as a disconnect"
    );
    assert_eq!(
        observed.abandoned,
        DisconnectCause::PeerDisconnect,
        "a peer abandoning the handshake mid-flight did not resolve PeerDisconnect"
    );
}

/// The synchronous entry point commits its `101` through the same handoff.
///
/// `serve_listener` builds every connection with no upgrade registrar, so its
/// upgrades take the detached-bridge branch — the one arm no owned fixture can
/// reach, because an owned server always supplies a registrar. Without this
/// case, deleting the handoff from that branch leaves every other WebSocket
/// proof green while a real synchronous upgrade reports `StreamReset`.
///
/// There is no acknowledgement to pause at here: the branch has no registrar to
/// hold. Ordering is deterministic anyway, because the handoff resolves before
/// the response is handed back to Hyper — so a cause read after the `101` is on
/// the wire has already passed the point being proven.
#[test]
fn synchronous_entry_websocket_upgrade_resolves_completed_at_handoff() {
    let (router, accepted, after_close) = upgrade_router();

    let mut server = SyncServer::start(router);
    let peer = crate::common::start_upgrade(server.addr(), WS_PATH);
    let observed = observe_upgrade(peer, &accepted, &after_close);
    server.assert_served();

    assert_upgrade_completed(&observed, "synchronous-entry");
}

/// What the supervisor-rejected upgrade reported.
struct RejectionOutcome {
    head: Box<str>,
    cause: DisconnectCause,
    closed: PeerClose,
}

/// An upgrade the supervisor rejects before acknowledgement never commits a
/// `101`, and its `503` is an ordinary produced response.
///
/// The bridge is already registered with `OwnedHttpTasks` when the supervisor
/// decides, so rejection means abort-and-join — and the connection permit rides
/// inside that bridge future. The owner joining within the bound is therefore
/// the permit probe for this boundary: a bridge that was never joined leaves
/// `OwnedHttpTasks` non-empty and the owner never returns. The live-listener
/// permit probe cannot be used here, because the same shutdown that produces
/// the rejection closes the accept path before any second connection could be
/// served.
#[test]
fn supervisor_rejection_before_websocket_handoff_completes_http_error() {
    let mut router = probe_router();
    let refused = gate_probe(&mut router, REFUSED_PATH, Hold::PassThrough);
    route_ws_until_close(&mut router, REFUSED_PATH);

    let observed = with_scripted_server(router, |addr, handle, controller| {
        controller
            .pause_once(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
            .expect("the upgrade acknowledgement could not be armed");
        let mut peer = crate::common::start_upgrade(addr, REFUSED_PATH);
        await_lifecycle_pause(controller, LifecycleCheckpoint::BeforeUpgradeAcknowledge);
        refused.await_entry();
        // The supervisor reads admission when it resumes, so requesting
        // shutdown while it is held is what makes the rejection deterministic
        // rather than a race against the drain.
        runtime::request_shutdown();
        controller
            .release(LifecycleCheckpoint::BeforeUpgradeAcknowledge)
            .expect("the held upgrade could not be released into shutdown");

        let head = crate::common::read_until_double_crlf(&mut peer);
        let cause = refused.cause();
        let closed = closed_within_bound(&mut peer);
        join_rejected_owner(handle);
        RejectionOutcome {
            head,
            cause,
            closed,
        }
    });

    assert!(
        observed.head.starts_with("HTTP/1.1 503"),
        "the rejected upgrade did not produce a 503: {:?}",
        observed.head
    );
    assert!(
        observed
            .head
            .to_ascii_lowercase()
            .contains("connection: close"),
        "the rejected upgrade omitted Connection: close: {:?}",
        observed.head
    );
    assert_eq!(
        observed.cause,
        DisconnectCause::Completed,
        "the supervisor's 503 did not resolve the response lifetime as a produced response"
    );
    assert_eq!(
        observed.closed,
        PeerClose::Closed,
        "the rejected upgrade's transport did not reach end of stream"
    );
}

/// Join the rejected server's owner, keeping its two failures apart.
///
/// A bridge that was never joined and a bridge that was joined with an error
/// are different defects, and collapsing them discards the only description of
/// the second.
fn join_rejected_owner(handle: camber::http::ServerHandle) {
    match try_bounded(handle.into_future()) {
        Some(Ok(())) => {}
        Some(Err(error)) => panic!("the rejected server owner returned an error: {error}"),
        None => panic!("the rejected server owner did not join within the bound"),
    }
}
