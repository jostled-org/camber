//! A separately spawned producer that holds a signal clone stays counted.
//!
//! Holding a clone of the response's signal is what a real producer does with
//! its own resources at stake. It must not buy the producer an exit from the
//! root scope: the drain still counts it until it returns.

use super::fixture::{DRIVER_AND_PRODUCER, with_drain_window};
use super::peer::{assert_permit_released, open_produced_stream};
use super::probes::{Admission, CauseProbe};
use super::routes::{probe_router, route_stream};
use camber::http::{DisconnectSignal, Router, StreamSender};

/// The chunk the producer emits before it parks on the signal.
const CHUNK: &[u8] = b"chunk";

/// Emit one chunk, then hold the clone until this response's lifetime ends.
async fn produce_then_wait(sender: StreamSender, signal: DisconnectSignal) {
    let _ = sender.send(CHUNK).await;
    signal.cancelled().await;
}

/// Register a streaming route whose producer is a separately spawned child.
fn route_spawned_producer(router: &mut Router, path: &str) -> (CauseProbe, Admission) {
    route_stream(router, path, 1, produce_then_wait)
}

#[test]
fn spawned_producer_holding_clone_is_counted_until_return() {
    let mut router = probe_router();
    let (spawned, admission) = route_spawned_producer(&mut router, "/spawned");

    let observed = with_drain_window(
        Some(1),
        router,
        |addr| {
            let client = open_produced_stream(
                addr,
                "/spawned",
                &spawned,
                "the spawned producer's response",
            );
            // The count this case is about is the producer's, so a refused
            // producer would make the whole window vacuous.
            admission.assert_admitted();
            // Returned so the peer stays live: the producer must still be
            // parked on the signal when the drain counts it.
            client
        },
        DRIVER_AND_PRODUCER,
        |addr, client| {
            // The drain has now counted the producer. Closing the peer is what
            // resolves the signal it parked on, so it can return and the count
            // can clear.
            drop(client);
            // Probed after the streamed connection closed and while the
            // listener is still live, so it measures that connection's permit
            // and not the readiness probe's. Under connection_limit(1) a
            // retained permit makes this never serve.
            assert_permit_released(addr);
        },
    );

    assert!(
        observed.probed.is_some(),
        "the drain never counted the spawned producer alongside the driver"
    );
    assert!(
        observed.reached_zero,
        "the drain never observed its child count reach zero after the producer returned"
    );
}
