//! A streaming producer that has emitted nothing still learns its peer left.
//!
//! An idle producer has no send to fail on, so a pull-shaped contract would
//! leave it holding whatever the response reserved until something else woke
//! it. The signal is pushed instead: the response body's guard resolves when
//! Hyper drops it, and the producer is waiting on that.

use super::fixture::BOUND;
use super::peer::open_produced_stream;
use super::probes::{Admission, CauseProbe, relay};
use super::routes::{probe_router, route_stream};
use super::servers::with_owned_server;
use camber::http::{DisconnectCause, Router};
use std::sync::Arc;
use std::sync::mpsc::Receiver;

/// Register a streaming route whose producer emits no chunk and waits.
///
/// The producer holds the sender, so the body cannot reach end of stream and
/// nothing but a lost peer can resolve this response. It reports the cause it
/// read itself: the invariant under test is that THE PRODUCER observes the
/// cause, which a watcher on a different task and a different clone cannot
/// stand in for.
fn route_idle_stream(
    router: &mut Router,
    path: &str,
) -> (CauseProbe, Admission, Receiver<DisconnectCause>) {
    let (report, observed) = relay::<DisconnectCause>();
    let (probe, admission) = route_stream(router, path, 1, move |sender, signal| {
        let report = Arc::clone(&report);
        async move {
            // The sender is held for the response's whole lifetime: the body
            // cannot reach end of stream while this producer owns it.
            report.send(signal.cancelled().await);
            drop(sender);
        }
    });
    (probe, admission, observed)
}

#[test]
fn idle_streaming_producer_observes_cause_within_bounded_deadline() {
    let mut router = probe_router();
    let (idle, admission, observed) = route_idle_stream(&mut router, "/idle");

    let (by_producer, by_watcher) = with_owned_server(router, |addr| {
        // The head is produced before any chunk exists, so what follows is
        // provably an idle producer rather than one between sends.
        let client = open_produced_stream(addr, "/idle", &idle, "the idle stream's response head");
        admission.assert_admitted();
        drop(client);
        let by_producer = observed
            .recv_timeout(BOUND)
            .expect("the idle producer never reported the cause it observed");
        (by_producer, idle.cause())
    });

    assert_eq!(
        by_producer,
        DisconnectCause::PeerDisconnect,
        "an idle streaming producer did not observe its peer leaving"
    );
    assert_eq!(
        by_watcher, by_producer,
        "a second clone of the response's signal reported a different cause"
    );
}
