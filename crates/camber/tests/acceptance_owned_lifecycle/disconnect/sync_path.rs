//! The synchronous `serve_listener` entry carries the same liveness wrapper
//! and per-request guard as the owned path.
//!
//! `serve_listener` blocks and detaches its per-connection tasks, so it needs
//! a harness the owned fixture cannot provide: without this case, wiring only
//! `serve_owned_*` would leave every owned-path proof green.

use super::peer::half_close_after_entry;
use super::routes::{probe_router, route_parked};
use super::servers::SyncServer;
use camber::http::DisconnectCause;

#[test]
fn sync_entry_path_resolves_disconnect_before_headers() {
    let mut router = probe_router();
    let parked = route_parked(&mut router, "/parked");

    let mut server = SyncServer::start(router);
    // Half-closed, so the read side stays open through the quiet window that
    // asserts no header byte was written on this path either.
    let cause = half_close_after_entry(server.addr(), "/parked", &parked);
    server.assert_served();

    assert_eq!(
        cause,
        DisconnectCause::PeerDisconnect,
        "the synchronous serve path did not resolve an in-flight response on peer close"
    );
}
