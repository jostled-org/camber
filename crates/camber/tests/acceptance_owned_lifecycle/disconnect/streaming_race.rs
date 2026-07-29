//! Racing a final send against a peer close still yields one terminal cause.
//!
//! The signal is a set-once cell behind every clone, so the race cannot be
//! resolved twice or resolved differently per observer. What it can still do
//! is leak: the leaks this orders against are a connection permit that is
//! never returned and a producer the scope stops counting.

use super::fixture::{BOUND, with_drained_server};
use super::peer::{assert_permit_released, open_produced_stream};
use super::probes::{Admission, CauseProbe, Report, Stages, relay};
use super::routes::{probe_router, route_staged_stream};
use camber::http::{DisconnectCause, Router, StreamSender};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use tokio::sync::mpsc::UnboundedReceiver;

/// Observers over one request's signal. More than one, because "exactly one
/// terminal cause" is a claim about every holder agreeing.
const OBSERVERS: usize = 3;

/// The final chunk the producer races against the peer's close.
const FINAL: &[u8] = b"final";

/// Hold the producer until the test releases it, then send the final chunk and
/// report whether that send landed.
///
/// The report is the only evidence the race this case names ever happened. A
/// producer that was never admitted, never woke, or never reached the send
/// leaves every assertion below true over three clones of one set-once cell —
/// which is a claim a sibling case already proves without any race at all.
async fn send_final(
    mut barrier: UnboundedReceiver<()>,
    sender: StreamSender,
    landed: Arc<Report<bool>>,
) {
    barrier
        .recv()
        .await
        .expect("the racing producer's barrier was dropped before its release");
    landed.send(sender.send(FINAL).await.is_ok());
}

/// Register a streaming route whose producer waits on a test-owned barrier.
fn route_racing_stream(
    router: &mut Router,
    path: &str,
) -> (CauseProbe, Admission, Stages, Receiver<bool>) {
    let (report, sends) = relay::<bool>();
    let (probe, admission, release) =
        route_staged_stream(router, path, OBSERVERS, move |barrier, sender| {
            let report = Arc::clone(&report);
            send_final(barrier, sender, report)
        });
    (probe, admission, release, sends)
}

/// What the raced final send measured.
struct RaceOutcome {
    causes: Box<[DisconnectCause]>,
    send_landed: bool,
}

#[test]
fn final_send_race_yields_exactly_one_cause_and_no_leak() {
    let mut router = probe_router();
    let (racing, admission, release, sends) = route_racing_stream(&mut router, "/racing");

    let (observed, reached_zero) = with_drained_server(Some(1), router, |addr| {
        let client = open_produced_stream(addr, "/racing", &racing, "the racing stream's head");
        admission.assert_admitted();

        // Order both triggers from this one thread: the producer is released
        // and the peer goes away with nothing between them.
        release.advance();
        drop(client);

        // Required before anything is read off the signal: this is what proves
        // the producer ran and reached the send the race is named for.
        let send_landed = sends
            .recv_timeout(BOUND)
            .expect("the racing producer never reported its final send");
        let causes = racing.causes(OBSERVERS);
        // Probed while the listener is live: a permit the closed connection
        // kept would make this never serve.
        assert_permit_released(addr);
        RaceOutcome {
            causes,
            send_landed,
        }
    });

    // `causes` returns one entry per observer or fails on its own bound, so the
    // first is always there.
    let first = observed.causes[0];
    assert!(
        observed.causes.iter().all(|cause| *cause == first),
        "one request's signal reported more than one terminal cause: {:?}",
        observed.causes
    );
    // The correlation is the invariant, not the disjunction: a send the closed
    // channel refused means the body was already gone, which is the peer's
    // close and nothing else.
    match (observed.send_landed, first) {
        (false, DisconnectCause::PeerDisconnect) => {}
        (false, cause) => panic!(
            "the final send lost the race to the peer's close, but the signal resolved {cause:?}"
        ),
        (true, DisconnectCause::PeerDisconnect | DisconnectCause::Completed) => {}
        (true, cause) => panic!(
            "an accepted final send resolved outside the two causes the race orders between: {cause:?}"
        ),
    }
    assert!(
        reached_zero,
        "the drain never observed its child count reach zero after the race"
    );
}
