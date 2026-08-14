//! What a validated handshake hands the bridge that will serve it.
//!
//! One connection's `101`, its permit, its disconnect handoff, and what
//! negotiation settled on — or the refusal that replaces all of it. Both bridge
//! kinds prepare through this file, so neither can restate the order those are
//! taken in or answer a refused handshake differently.

use super::super::Request;
use super::super::body::HyperResponseBody;
use super::super::disconnect::DisconnectSignal;
use super::super::rejection::Rejected;
use super::super::server_lifecycle::{ConnectionLifecycle, ConnectionPermit};
use super::handshake::{
    WsHandshakeError, WsUpgrade, extract_ws_subprotocol, ws_handshake_rejection,
    ws_switching_protocols,
};
use std::sync::Arc;

/// Extract the upgrade pair when the request contained valid WS upgrade headers.
fn ws_upgrade_pair(
    ws_upgrade: WsUpgrade,
) -> Result<(hyper::upgrade::OnUpgrade, Box<str>), WsHandshakeError> {
    match ws_upgrade {
        WsUpgrade::Ready(on_upgrade, accept_key) => Ok((on_upgrade, accept_key)),
        WsUpgrade::Rejected(error) => Err(error),
    }
}

/// What a validated handshake hands the bridge that will serve it.
///
/// The ordering both upgrade kinds depend on lives in the one function that
/// builds this: the permit is taken only once the `101` exists, so the arm that
/// cannot build one never holds a connection slot for an upgrade that will not
/// happen, and the disconnect handoff is captured before the request can move
/// into a bridge.
pub(super) struct WsHandoff<'a> {
    pub(super) on_upgrade: hyper::upgrade::OnUpgrade,
    pub(super) subprotocol: Option<&'a str>,
    pub(super) response: hyper::Response<HyperResponseBody>,
    pub(super) permit: Arc<ConnectionPermit>,
    pub(super) handoff: DisconnectSignal,
}

/// One refused upgrade, and what negotiation had established when it failed.
///
/// The subprotocol travels with the refusal because the request it was read
/// from moves into the bridge: this is the last point that can say whether
/// negotiation had selected one, and rejection context reports presence exactly
/// where an owner established it.
pub(in crate::http) struct WsRefusal {
    pub(in crate::http) rejected: Rejected,
    pub(in crate::http) subprotocol: Option<Box<str>>,
}

impl WsRefusal {
    /// A refusal found before subprotocol negotiation ran.
    fn unnegotiated(rejected: Rejected) -> Self {
        Self {
            rejected,
            subprotocol: None,
        }
    }

    /// A refusal found after negotiation settled on what it settled on.
    pub(super) fn negotiated(rejected: Rejected, subprotocol: Option<&str>) -> Self {
        Self {
            rejected,
            subprotocol: subprotocol.map(Box::from),
        }
    }
}

/// What a handshake attempt leaves the caller holding.
///
/// Both arms are what the peer gets, not success against error: a `101` whose
/// bridge is still to be built, or the refusal that replaces it. Written as
/// its own enum rather than a `Result` because that is what it means, and
/// because `clippy::result_large_err` does not apply to it.
pub(super) enum WsHandoffOutcome<'a> {
    /// The handshake stands; here is everything the `101` handoff needs.
    Ready(WsHandoff<'a>),
    /// The peer gets this instead: a rejected handshake, or a `101` that could
    /// not be built.
    Refused(WsRefusal),
}

/// Validate the handshake and build everything the `101` handoff needs.
///
/// Both upgrade kinds enter here, so neither can restate that ordering or
/// answer a refused handshake differently.
pub(super) fn prepare_ws_handoff<'a>(
    ws_upgrade: WsUpgrade,
    req: &'a Request,
    lifecycle: &ConnectionLifecycle,
) -> WsHandoffOutcome<'a> {
    let (on_upgrade, accept_key) = match ws_upgrade_pair(ws_upgrade) {
        Ok(pair) => pair,
        Err(error) => {
            return WsHandoffOutcome::Refused(WsRefusal::unnegotiated(ws_handshake_rejection(
                error,
            )));
        }
    };
    let subprotocol = extract_ws_subprotocol(req);
    let response = match ws_switching_protocols(accept_key.as_ref(), subprotocol) {
        Ok(response) => response,
        Err(error) => {
            return WsHandoffOutcome::Refused(WsRefusal::negotiated(
                Rejected::ws_upgrade_unbuildable(error),
                subprotocol,
            ));
        }
    };
    WsHandoffOutcome::Ready(WsHandoff {
        on_upgrade,
        subprotocol,
        response,
        permit: lifecycle.permit(),
        handoff: req.on_disconnect(),
    })
}
