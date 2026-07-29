use crate::common;
use crate::disconnect::fixture::{BOUND, DRIVER_ONLY, with_drain_window};
use crate::disconnect::peer::send;
use crate::disconnect::routes::{READY_PATH, probe_router};
use std::net::SocketAddr;

/// What the window between closure return and the drain's end observed.
struct LiveWindow {
    /// The address the owned server was still serving on.
    addr: SocketAddr,
    /// The status a request sent after the closure returned came back with.
    status: u16,
}

/// Closure return is not a server shutdown event: the owned server is still
/// serving real traffic while the drain waits on its supervisor driver.
///
/// `ServerControl` is private, so liveness is proven over the wire rather than
/// by reading a flag. The harness is the disconnect tree's drain-window
/// fixture: it arms the same driver checkpoint, holds the server handle across
/// the window, and drives teardown to the terminal count — so this case states
/// only what it measures inside that window.
#[test]
fn owned_server_is_live_after_closure_return_and_its_driver_blocks_the_drain() {
    let observed = with_drain_window(
        None,
        probe_router(),
        |_| (),
        DRIVER_ONLY,
        |addr, _| {
            // Teardown has begun and the supervisor driver still blocks it.
            // The request travels in from a thread with no runtime context, so
            // only the server itself can answer it.
            let response = send(
                addr,
                "GET",
                READY_PATH,
                "the owned server's answer after closure return",
            );
            LiveWindow {
                addr,
                status: response.status,
            }
        },
    );

    let window = observed
        .probed
        .expect("the drain never reported the supervisor driver holding it open");
    assert_eq!(
        window.status, 200,
        "a request sent after closure return did not reach the router"
    );
    assert!(
        observed.reached_zero,
        "the drain never reported the supervisor driver's count cleared"
    );

    // The exact refusal, not merely an error: this port returns to the
    // ephemeral pool once the drain closes its listener, so a sibling test that
    // rebinds it would answer a bare `is_err()` check with a different failure.
    let refused = common::request(window.addr, "GET", READY_PATH, &[], &[], BOUND)
        .expect_err("the owned server outlived the scope drain");
    assert_eq!(
        refused.kind(),
        std::io::ErrorKind::ConnectionRefused,
        "the drained listener's port did not refuse: {refused}"
    );
}
