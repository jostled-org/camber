//! What a served handler reports back to the test that registered it.
//!
//! One structural fact shapes all of them. Hyper drops Camber's per-request
//! future when the peer goes away or resets the stream — that drop IS the
//! observation — so a handler cannot report its own cancellation. Every probe
//! therefore hands a clone of the signal to a watcher task that outlives the
//! handler, exactly as a real streaming producer would.

use super::fixture::{BOUND, QUIET};
use camber::RuntimeError;
use camber::http::{DisconnectCause, DisconnectSignal};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// A `std::sync::mpsc::Sender` is `Send` but not `Sync`, and a route handler
/// must be both.
pub(super) struct Report<T>(Mutex<Sender<T>>);

impl<T> Report<T> {
    fn new(sender: Sender<T>) -> Self {
        Self(Mutex::new(sender))
    }

    /// A closed receiver means the test already finished with this probe.
    pub(super) fn send(&self, value: T) {
        let sender = self.0.lock().unwrap_or_else(|error| error.into_inner());
        let _ = sender.send(value);
    }
}

/// A one-way report from a served handler to the test that registered it.
///
/// The same shape every probe here uses, for the cases that need to report
/// something other than a terminal cause.
pub(super) fn relay<T>() -> (Arc<Report<T>>, Receiver<T>) {
    let (sender, receiver) = channel();
    (Arc::new(Report::new(sender)), receiver)
}

/// The test-side half of "a handler for this subject has started".
///
/// Split out of [`CauseProbe`] because the gRPC case needs the same barrier
/// over a tonic handler that owns no disconnect signal of its own.
pub(super) struct EntryBarrier(Receiver<()>);

impl EntryBarrier {
    /// Block until a handler for this subject reported entry.
    ///
    /// This is the barrier the fixtures gate on: acting on a request before its
    /// handler exists would prove nothing about an in-flight response.
    pub(super) fn await_entry(&self) {
        self.0
            .recv_timeout(BOUND)
            .expect("no handler entered the probed subject within the bound");
    }
}

/// The two halves of an entry barrier.
pub(super) fn entry_barrier() -> (Arc<Report<()>>, EntryBarrier) {
    let (report, entries) = relay::<()>();
    (report, EntryBarrier(entries))
}

/// The handler-side half of a probe. Shared with the sibling cases that arm a
/// probe from somewhere other than a route handler.
pub(super) struct ProbeSinks {
    entered: Arc<Report<()>>,
    resolved: Arc<Report<(Box<str>, DisconnectCause)>>,
}

impl ProbeSinks {
    /// Hand `clones` clones of the signal to watchers that survive this
    /// handler's future being dropped, and report entry once every one of them
    /// is awaiting its clone.
    ///
    /// `subject` is the watched request's own path, and every report carries
    /// it. A probe that reads one cause then states which request produced it
    /// rather than inheriting whatever its filter admitted.
    ///
    /// `clones` must be at least one: the last watcher to reach its await is
    /// what reports entry, so a call that spawns none never reports.
    ///
    /// The watchers are raw `tokio::spawn`, deliberately: a watcher admitted to
    /// the root scope would be counted by the drain, and the exact occupancy
    /// checkpoints these journeys pause on — `DRIVER_ONLY` and
    /// `DRIVER_AND_PRODUCER` — are stated in children the drain must see. An
    /// admitted watcher would shift every one of those counts.
    pub(super) fn watch(self: &Arc<Self>, subject: &str, signal: DisconnectSignal, clones: usize) {
        // Stated here rather than left to the caller's entry barrier: a probe
        // that armed nothing never reports, and its silence would surface as a
        // handler that never entered — a fault in this harness told in
        // production's words.
        assert!(
            clones > 0,
            "a probe that arms no watcher can never report entry"
        );
        let unarmed = Arc::new(AtomicUsize::new(clones));
        for _ in 0..clones {
            tokio::spawn(watch_one(
                Arc::clone(self),
                Arc::clone(&unarmed),
                subject.into(),
                signal.clone(),
            ));
        }
    }
}

/// One watcher: poll its clone once, report entry if it is the last of its
/// siblings to get there, then hold the clone until the response lifetime ends.
///
/// The first poll comes before the report, not after. An unpolled watcher has
/// registered nothing with the signal, so its silence and an unresolved
/// signal's silence read the same to [`CauseProbe::still_unresolved`] — and
/// entry that means "the task started" rather than "the clone is being awaited"
/// would let every caller of that window measure the wrong thing. One biased
/// poll against a ready future separates the two: a signal already resolved
/// yields its cause here, and anything else has parked a waker by the time
/// entry is reported.
async fn watch_one(
    sinks: Arc<ProbeSinks>,
    unarmed: Arc<AtomicUsize>,
    subject: Box<str>,
    signal: DisconnectSignal,
) {
    let cancelled = signal.cancelled();
    tokio::pin!(cancelled);
    let polled = tokio::select! {
        biased;
        cause = &mut cancelled => Some(cause),
        () = std::future::ready(()) => None,
    };
    match unarmed.fetch_sub(1, Ordering::SeqCst) {
        1 => sinks.entered.send(()),
        _ => {}
    }
    let cause = match polled {
        Some(cause) => cause,
        None => cancelled.await,
    };
    sinks.resolved.send((subject, cause))
}

/// The test-side half of a probe over one route's disconnect signal.
pub(super) struct CauseProbe {
    entry: EntryBarrier,
    resolved: Receiver<(Box<str>, DisconnectCause)>,
}

impl CauseProbe {
    pub(super) fn pair() -> (Arc<ProbeSinks>, Self) {
        let (entered, entry) = entry_barrier();
        let (reported, resolved) = relay::<(Box<str>, DisconnectCause)>();
        let sinks = Arc::new(ProbeSinks {
            entered,
            resolved: reported,
        });
        (sinks, Self { entry, resolved })
    }

    /// Block until a handler for this route has started and every watcher it
    /// armed is awaiting the signal.
    pub(super) fn await_entry(&self) {
        self.entry.await_entry();
    }

    /// The watched request's path and the terminal cause its signal resolved
    /// to.
    ///
    /// For the probes whose filter is what decides which request they watch: a
    /// cause read without its path is a cause attributed to whichever request
    /// reported first.
    pub(super) fn watched_cause(&self) -> (Box<str>, DisconnectCause) {
        self.resolved
            .recv_timeout(BOUND)
            .expect("the disconnect signal never resolved within the bound")
    }

    /// The terminal cause this route's signal resolved to.
    pub(super) fn cause(&self) -> DisconnectCause {
        self.watched_cause().1
    }

    /// The causes reported by `count` watchers over one request's signal.
    ///
    /// A missing report already fails on `cause`'s own bound, so the arity
    /// claim worth making here is the other one: the quiet window after the
    /// last report is what an extra watcher falsifies.
    pub(super) fn causes(&self, count: usize) -> Box<[DisconnectCause]> {
        let causes: Box<[DisconnectCause]> = (0..count).map(|_| self.cause()).collect();
        assert!(
            self.still_unresolved(),
            "more watchers reported than the {count} this request armed: {causes:?}"
        );
        causes
    }

    /// Whether the signal is still unresolved after watching it for `QUIET`.
    ///
    /// The window's expiry is the pass, so on its own this reports silence
    /// rather than liveness. Every caller either opens the window inside a
    /// production pause that provably cannot advance, or follows it with a
    /// positive observation over the same probe.
    ///
    /// A dead channel is neither answer. It means every holder of this
    /// request's sinks is gone — this harness's own plumbing collapsed — and
    /// reporting it as silence would let each caller state that collapse in
    /// production's words.
    pub(super) fn still_unresolved(&self) -> bool {
        match self.resolved.recv_timeout(QUIET) {
            Err(RecvTimeoutError::Timeout) => true,
            Ok(_) => false,
            Err(RecvTimeoutError::Disconnected) => panic!(
                "every watcher over this request's signal was dropped before the quiet window closed"
            ),
        }
    }
}

/// The root scope's answer to one streaming route's producer spawn.
///
/// A refused producer drops its sender, the stream body ends immediately, and
/// the response resolves `Completed` — the same outcome a fully produced
/// stream reaches. Every claim about what a producer did is therefore vacuous
/// until this reports admission.
pub(super) struct Admission(Receiver<Option<RuntimeError>>);

impl Admission {
    /// Build the two halves of one route's admission report.
    pub(super) fn pair() -> (Arc<Report<Option<RuntimeError>>>, Self) {
        let (admitted, admissions) = relay::<Option<RuntimeError>>();
        (admitted, Self(admissions))
    }

    /// The refusal the root scope reported, or `None` for an admitted producer.
    pub(super) fn outcome(&self) -> Option<RuntimeError> {
        self.0
            .recv_timeout(BOUND)
            .expect("the streaming route never reported its producer's admission")
    }

    /// Require that the root scope admitted the producer.
    pub(super) fn assert_admitted(&self) {
        match self.outcome() {
            None => {}
            Some(error) => panic!("the root scope refused the streaming producer: {error}"),
        }
    }
}

/// Releases a staged producer one stage at a time from the test thread.
///
/// `UnboundedSender::send` is synchronous, so the test orders a stage against
/// its own socket reads without a runtime of its own.
pub(super) struct Stages(UnboundedSender<()>);

impl Stages {
    /// Release the next stage.
    pub(super) fn advance(&self) {
        self.0
            .send(())
            .expect("the staged receiver stopped listening for its release");
    }
}

/// The producer half of a staged release, handed to the one request that
/// reaches its route.
#[derive(Clone)]
pub(super) struct StageReceiver(Arc<Mutex<Option<UnboundedReceiver<()>>>>);

impl StageReceiver {
    /// Take the receiver. `None` on any request after the first, which is one
    /// more than these routes are asked to serve.
    pub(super) fn take(&self) -> Option<UnboundedReceiver<()>> {
        self.0
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

/// The two halves of a test-ordered producer release.
pub(super) fn staged() -> (StageReceiver, Stages) {
    let (sender, receiver) = unbounded_channel::<()>();
    (
        StageReceiver(Arc::new(Mutex::new(Some(receiver)))),
        Stages(sender),
    )
}
