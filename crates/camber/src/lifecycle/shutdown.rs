//! The one deadline every framework-owned shutdown participant shares.
//!
//! A runtime and the servers inside it stop together, so they stop against one
//! clock. The first graceful transition — a shutdown request, a server's own
//! control transition, or the user closure returning — mints one absolute
//! expiry from the configured grace, and every later transition reads that same
//! instant back. A participant may narrow it with a bound of its own; none of
//! them may restart it, which is what kept a nested owner from turning one
//! thirty-second budget into thirty seconds per layer.
//!
//! Explicit cancellation is the one transition that mints nothing: the owner a
//! caller cancelled enters forced termination immediately, under the fixed
//! forced-join grace below rather than under a second copy of the aggregate's.

use super::LifecycleParticipant;
use crate::runtime_test_support::{ParticipantDisposition, RuntimeSchedule};
use std::sync::{Arc, OnceLock};
use std::time::Duration;
use tokio::time::Instant;

/// How long a forced stop waits for the owners it just aborted.
///
/// Abort drops a yielding owner at its next `.await`, so its join resolves well
/// inside this window. An owner the executor cannot stop never resolves, which
/// is why the grace bounds the wait rather than the wait bounding itself. It is
/// deliberately small and fixed: it bounds a forced stop, it is not a second
/// aggregate grace.
pub const FORCED_JOIN_GRACE: Duration = Duration::from_millis(100);

/// The one aggregate shutdown a runtime and its owned servers share.
///
/// Cheap to clone through its `Arc` and safe to read from any owner: the expiry
/// is written once through a `OnceLock`, so a second minter loses the race
/// rather than replacing the value, and cancellation is a latch that only ever
/// moves one way.
pub(crate) struct AggregateShutdown {
    /// The configured grace one transition mints an expiry from.
    grace: Duration,
    /// The one absolute expiry, written by the first graceful transition.
    expiry: OnceLock<Instant>,
    /// Where mints, readings, and settlements are published for a test that
    /// attached a controller. `None` in every other run, which makes every
    /// observation below inert.
    observer: Option<Arc<RuntimeSchedule>>,
}

impl AggregateShutdown {
    /// Start one shutdown coordinator over `grace`.
    pub(crate) fn new(grace: Duration, observer: Option<Arc<RuntimeSchedule>>) -> Arc<Self> {
        Arc::new(Self {
            grace,
            expiry: OnceLock::new(),
            observer,
        })
    }

    /// Mint the one expiry from `at`, unless a transition already did.
    ///
    /// The whole no-restart rule lives here: every graceful transition calls
    /// this, and only the first one writes. It answers nothing, because a
    /// transition's job is to start the clock and a participant reads it back
    /// through [`Self::read`] under its own name.
    pub(crate) fn mint_at(&self, at: Instant) {
        let mut minted = false;
        let expiry = *self.expiry.get_or_init(|| {
            minted = true;
            at + self.grace
        });
        if minted && let Some(observer) = self.observer.as_ref() {
            observer.record_deadline_mint(at, expiry);
        }
    }

    /// The shared expiry as `participant` reads it, minting nothing.
    ///
    /// An owner that reaches shutdown before any transition has been published
    /// reads `None` and keeps its own bound; it never mints one on the
    /// transition's behalf.
    pub(crate) fn read(&self, participant: &LifecycleParticipant) -> Option<Instant> {
        let expiry = *self.expiry.get()?;
        if let Some(observer) = self.observer.as_ref() {
            observer.record_deadline_reading(participant, expiry);
        }
        Some(expiry)
    }

    /// Mint from `at` if nothing has been minted yet, and answer as
    /// `participant` reads it.
    pub(crate) fn read_or_mint(&self, participant: &LifecycleParticipant, at: Instant) -> Instant {
        self.mint_at(at);
        // Filled by construction: the mint above leaves the cell written, so
        // the fallback restates the same arithmetic rather than inventing a
        // second deadline.
        self.read(participant).unwrap_or(at + self.grace)
    }

    /// How long `participant` may still wait, bounded by its own `local` limit.
    ///
    /// Reads the shared expiry under this participant's name and narrows
    /// `local` by what is left, through [`narrowed`].
    pub(crate) fn bounded(&self, participant: &LifecycleParticipant, local: Duration) -> Duration {
        narrowed(local, self.remaining(participant))
    }

    /// The time left before the shared expiry, or `None` when no transition has
    /// minted one.
    pub(crate) fn remaining(&self, participant: &LifecycleParticipant) -> Option<Duration> {
        self.read(participant)
            .map(|expiry| expiry.saturating_duration_since(Instant::now()))
    }

    /// Publish how one participant was disposed of.
    ///
    /// Production decides the disposition; the observation only records the
    /// decision so a case can read the whole inventory instead of inferring it
    /// from what the aggregate happened to name.
    pub(crate) fn settle(
        &self,
        participant: &LifecycleParticipant,
        disposition: ParticipantDisposition,
    ) {
        if let Some(observer) = self.observer.as_ref() {
            observer.record_settlement(participant, disposition);
        }
    }
}

/// The bound a `local` limit actually runs under, given what the aggregate has
/// left.
///
/// The one definition of that arithmetic. [`AggregateShutdown::bounded`] applies
/// it to whatever bound a participant brought and
/// [`ResourceBudget::phase_deadline`](crate::ResourceBudget::phase_deadline) to
/// a configured phase deadline, so a second copy has nothing to drift from.
///
/// The narrower of the two always wins, so a resource phase deadline shorter
/// than what the aggregate has left keeps its own bound and one longer than the
/// aggregate is cut down to it. `None` is an aggregate no transition has minted
/// yet, which narrows nothing.
///
/// A spent aggregate does not cut a participant to nothing. Every owner still
/// gets the fixed forced-join grace, which is what a forced stop gives an owner
/// it has just told to end — an owner handed zero would be reported outstanding
/// without ever having been asked, which describes the order the coordinator
/// visited its participants in rather than anything that owner did. `local`
/// still wins whenever it is the narrower of the two, so a caller's own shorter
/// bound is never widened by this floor.
pub(crate) fn narrowed(local: Duration, remaining: Option<Duration>) -> Duration {
    match remaining {
        Some(remaining) => local.min(remaining.max(FORCED_JOIN_GRACE)),
        None => local,
    }
}
