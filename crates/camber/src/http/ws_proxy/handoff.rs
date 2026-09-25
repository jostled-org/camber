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
use super::handshake::{WsHandshakeOffer, WsProtocolOffers, WsSelection};
use super::ownership::{BridgeAttachment, own_upgrade_bridge};
use std::sync::Arc;

/// What a validated handshake hands the bridge that will serve it.
///
/// The ordering both upgrade kinds depend on lives in the one function that
/// builds this: the permit is taken only once the `101` exists, so the arm that
/// cannot build one never holds a connection slot for an upgrade that will not
/// happen, and the disconnect handoff is captured before the request can move
/// into a bridge. The client's offers travel beside the upgrade with the
/// selection the `101` names, owned rather than borrowed from the request, so a
/// refusal after the request has moved into a bridge can still name what the
/// `101` selected.
pub(super) struct WsHandoff {
    on_upgrade: hyper::upgrade::OnUpgrade,
    offers: WsProtocolOffers,
    selection: WsSelection,
    response: hyper::Response<HyperResponseBody>,
    permit: Arc<ConnectionPermit>,
    handoff: DisconnectSignal,
}

impl WsHandoff {
    /// Register the bridge `build_bridge` makes from this handoff's upgrade and
    /// permit, and return the `101` it earned.
    ///
    /// Both upgrade kinds register here. The offers stay behind while the
    /// bridge takes the rest, so a registration refusal still names what the
    /// `101` had selected.
    pub(super) async fn register<F, Fut>(
        self,
        lifecycle: &ConnectionLifecycle,
        build_bridge: F,
    ) -> Result<hyper::Response<HyperResponseBody>, WsRefusal>
    where
        F: FnOnce(hyper::upgrade::OnUpgrade, Arc<ConnectionPermit>, BridgeAttachment) -> Fut,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let Self {
            on_upgrade,
            offers,
            selection,
            response,
            permit,
            handoff,
        } = self;
        own_upgrade_bridge(lifecycle, response, &handoff, move |attachment| {
            build_bridge(on_upgrade, permit, attachment)
        })
        .await
        .map_err(|rejected| WsRefusal::negotiated(rejected, offers.named(selection)))
    }
}

/// One refused upgrade, and what negotiation had established when it failed.
///
/// The subprotocol travels with the refusal because the request it was read
/// from moves into the bridge: this is the last point that can say whether
/// negotiation had selected one, and rejection context reports presence exactly
/// where an owner established it.
pub(in crate::http) struct WsRefusal {
    pub(in crate::http) rejected: Box<Rejected>,
    pub(in crate::http) subprotocol: Option<Box<str>>,
}

impl WsRefusal {
    /// A refusal found before negotiation selected anything.
    pub(super) fn unnegotiated(rejected: Rejected) -> Self {
        Self {
            rejected: Box::new(rejected),
            subprotocol: None,
        }
    }

    /// A refusal found after negotiation settled on what it settled on.
    fn negotiated(rejected: Rejected, subprotocol: Option<&str>) -> Self {
        Self {
            rejected: Box::new(rejected),
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
pub(super) enum WsHandoffOutcome {
    /// The handshake stands; here is everything the `101` handoff needs.
    Ready(WsHandoff),
    /// The peer gets this instead: a rejected handshake, or a `101` that could
    /// not be built.
    Refused(WsRefusal),
}

/// Build everything the `101` handoff for an admitted offer needs.
///
/// Both upgrade kinds enter here, so neither can restate that ordering or
/// answer a refused handshake differently. The `101` names what `selection`
/// names: the first offer for a direct bridge, and whatever the backend chose
/// for a proxied one.
pub(super) fn prepare_ws_handoff(
    offer: WsHandshakeOffer,
    selection: WsSelection,
    req: &Request,
    lifecycle: &ConnectionLifecycle,
) -> WsHandoffOutcome {
    let response = match offer.switching_protocols(selection) {
        Ok(response) => response,
        Err(error) => {
            return WsHandoffOutcome::Refused(WsRefusal::negotiated(
                Rejected::ws_upgrade_unbuildable(error),
                offer.protocols().named(selection),
            ));
        }
    };
    let (on_upgrade, offers) = offer.into_transfer();
    WsHandoffOutcome::Ready(WsHandoff {
        on_upgrade,
        offers,
        selection,
        response,
        permit: lifecycle.permit(),
        handoff: req.on_disconnect(),
    })
}
