//! The cause table on real transport: per-response scope, the three
//! disconnect causes against completion, first-wins ordering, clone identity,
//! and the completion points that have no frame to key on.

use super::peer::{abandon_after_entry, read_response, send, start_request};
use super::routes::{probe_router, route_parked, route_parked_clones, route_responding};
use super::servers::{with_owned_handle, with_owned_server};
use camber::http::{DisconnectCause, Response};

/// The number of clones 6.T8 proves identity across.
const CLONE_COUNT: usize = 4;

#[test]
fn one_connection_close_does_not_resolve_another_connections_request() {
    let mut router = probe_router();
    let closed = route_parked(&mut router, "/closed");
    let untouched = route_parked(&mut router, "/untouched");

    let (closed_cause, untouched_open, untouched_cause) = with_owned_server(router, |addr| {
        // Opened and entered first, so it is provably in flight on its own
        // connection while the other one goes away.
        let untouched_client = start_request(addr, "/untouched");
        untouched.await_entry();

        let closed_cause = abandon_after_entry(addr, "/closed", &closed);
        let untouched_open = untouched.still_unresolved();
        // The quiet window's expiry is the negative claim, so it is worth only
        // as much as the receiver's liveness. Closing this peer too is the
        // positive observation over the same probe: it resolves, so the silence
        // above was a signal that had not resolved rather than a report nobody
        // was listening for.
        drop(untouched_client);
        (closed_cause, untouched_open, untouched.cause())
    });

    assert_eq!(
        closed_cause,
        DisconnectCause::PeerDisconnect,
        "the closed connection's request did not resolve PeerDisconnect"
    );
    assert!(
        untouched_open,
        "a peer closing one connection resolved a request in flight on another"
    );
    assert_eq!(
        untouched_cause,
        DisconnectCause::PeerDisconnect,
        "the untouched connection's own close never resolved, so the quiet \
         window above proves nothing"
    );
}

#[test]
fn buffered_causes_are_distinguishable_and_completion_is_not_a_disconnect() {
    let mut router = probe_router();
    let abandoned = route_parked(&mut router, "/abandoned");
    let finished = route_responding(&mut router, "/finished", || {
        Response::text(200, "a fully produced buffered body")
    });
    let interrupted = route_parked(&mut router, "/interrupted");

    let (peer, produced, shutdown) = with_owned_handle(router, |addr, handle| {
        let peer = abandon_after_entry(addr, "/abandoned", &abandoned);

        let response = send(addr, "GET", "/finished", "the buffered response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body.as_ref(), b"a fully produced buffered body");
        let produced = finished.cause();

        let interrupted_client = start_request(addr, "/interrupted");
        interrupted.await_entry();
        // Dropping the handle is the server's shutdown request; the in-flight
        // response has not been produced, so it resolves as a shutdown.
        drop(handle);
        let shutdown = interrupted.cause();
        drop(interrupted_client);
        (peer, produced, shutdown)
    });

    assert_eq!(peer, DisconnectCause::PeerDisconnect);
    assert_eq!(shutdown, DisconnectCause::ServerShutdown);
    // The guard dropped when the buffered body finished producing, and that
    // drop read as completion rather than as a disconnect.
    assert_eq!(
        produced,
        DisconnectCause::Completed,
        "a response that finished producing reported a disconnect cause"
    );
}

#[test]
fn shutdown_ordered_before_peer_close_resolves_server_shutdown() {
    let mut router = probe_router();
    let racing = route_parked(&mut router, "/racing");

    let cause = with_owned_handle(router, |addr, handle| {
        let client = start_request(addr, "/racing");
        racing.await_entry();
        // Order the two triggers from one thread. `ServerHandle::drop` sends
        // the abort synchronously, so the shutdown precondition is provably
        // true before the close is even started. Whether the close itself
        // landed before the guard dropped is NOT established here — nothing
        // outside the connection can observe that flag — so this proves the
        // shutdown row wins over a peer close it was ordered ahead of, not
        // over one that had already been recorded.
        drop(handle);
        drop(client);
        racing.cause()
    });

    assert_eq!(
        cause,
        DisconnectCause::ServerShutdown,
        "a request in flight when shutdown was requested resolved as something \
         other than a shutdown after its peer went away"
    );
}

#[test]
fn empty_body_response_resolves_completed_even_if_never_polled() {
    let mut router = probe_router();
    let empty = route_responding(&mut router, "/empty", || Response::empty(200));

    let (status, body_length, cause) = with_owned_server(router, |addr| {
        // The cause is taken before this client reads a byte. An empty body
        // reports end-of-stream, so Hyper never polls it: no frame exists to
        // key completion on, and nothing about delivery can have established
        // it either.
        let mut client = start_request(addr, "/empty");
        let cause = empty.cause();
        let response = read_response(&mut client);
        (response.status, response.body.len(), cause)
    });

    assert_eq!(status, 200);
    assert_eq!(body_length, 0, "the empty-body response carried a body");
    assert_eq!(
        cause,
        DisconnectCause::Completed,
        "an empty body Hyper never polls did not resolve Completed"
    );
}

#[test]
fn all_clones_of_a_request_signal_resolve_one_cause() {
    let mut router = probe_router();
    let cloned = route_parked_clones(&mut router, "/cloned", CLONE_COUNT);

    let causes = with_owned_server(router, |addr| {
        let client = start_request(addr, "/cloned");
        cloned.await_entry();
        drop(client);
        cloned.causes(CLONE_COUNT)
    });

    assert!(
        causes
            .iter()
            .all(|cause| *cause == DisconnectCause::PeerDisconnect),
        "clones of one request's signal reported different causes: {causes:?}"
    );
}

#[test]
fn head_and_fallback_error_responses_resolve_completed() {
    let mut router = probe_router();
    let stripped = route_responding(&mut router, "/stripped", || {
        Response::text(200, "a body the drained-body rule removes")
    });
    // An out-of-range status makes response construction fail, so the runtime
    // substitutes its own error response.
    let fallback = route_responding(&mut router, "/fallback", || {
        Response::text(999, "an unbuildable response")
    });

    let (head, error) = with_owned_server(router, |addr| {
        let head_response = send(addr, "HEAD", "/stripped", "the HEAD response");
        assert_eq!(head_response.status, 200);
        assert_eq!(head_response.body.len(), 0, "HEAD returned a body");
        let head = stripped.cause();

        let error_response = send(addr, "GET", "/fallback", "the fallback response");
        assert_eq!(error_response.status, 500);
        (head, fallback.cause())
    });

    assert_eq!(
        head,
        DisconnectCause::Completed,
        "a HEAD response whose body was stripped did not resolve Completed"
    );
    assert_eq!(
        error,
        DisconnectCause::Completed,
        "a substituted error response did not resolve Completed"
    );
}
