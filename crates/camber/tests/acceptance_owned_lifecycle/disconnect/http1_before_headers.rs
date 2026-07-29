//! HTTP/1: a peer that goes away mid-handler resolves the response signal
//! before a single header byte can be written.

use super::peer::half_close_after_entry;
use super::routes::{probe_router, route_parked};
use super::servers::with_owned_server;
use camber::http::DisconnectCause;

#[test]
fn on_disconnect_resolves_before_headers_when_http1_peer_closes() {
    let mut router = probe_router();
    let parked = route_parked(&mut router, "/parked");

    // The peer half-closes rather than dropping, so its read side survives to
    // witness the silence: the observation point this case is named for is
    // asserted on the wire, not inferred from a parked handler.
    let cause = with_owned_server(router, |addr| {
        half_close_after_entry(addr, "/parked", &parked)
    });

    assert_eq!(
        cause,
        DisconnectCause::PeerDisconnect,
        "an HTTP/1 peer close did not resolve the in-flight response as PeerDisconnect"
    );
}
