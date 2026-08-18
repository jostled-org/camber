//! One registered resource's frozen identity, its callback gate, and the health
//! its coordinator publishes.

use super::Resource;
use crate::lifecycle::{ResourceFailure, ResourceFailureKind, ResourcePhase};
use crate::runtime_state::recover_poisoned;
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

/// One registered resource, frozen for the life of the runtime.
///
/// The resource is erased ONCE, into an `Arc`, so a lifecycle worker that
/// outlived the wait a coordinator gave it still holds the resource it was
/// invoked on. A registry of `Box<dyn Resource>` could not promise that: the
/// only owner would be the registry, and the registry dies with the runtime.
///
/// The name is `Arc<str>` for the same reason — the entry, the aggregate that
/// retains a failure, and the operator event that renders it all read one
/// string.
pub(crate) struct ResourceEntry {
    name: Arc<str>,
    resource: Arc<dyn Resource>,
    gate: CallbackGate,
    /// What this resource's last completed callback did, published by the
    /// coordinator that waited on it.
    ///
    /// One synchronized value rather than an atomic status beside a typed
    /// failure: both would be written on the same transition and read on the
    /// same request, and two owners of one fact can disagree — a resource that
    /// recovers between the two reads would report `error` with no failure to
    /// name. `RwLock` because `/health` reads far more often than a coordinator
    /// writes.
    published: RwLock<Option<ResourceFailure>>,
}

impl ResourceEntry {
    /// Freeze one registered resource.
    pub(crate) fn new(resource: Arc<dyn Resource>) -> Self {
        Self {
            name: Arc::from(resource.name()),
            resource,
            gate: CallbackGate::new(),
            published: RwLock::new(None),
        }
    }

    /// The registered name every report of this resource is written under.
    pub(crate) fn name(&self) -> &Arc<str> {
        &self.name
    }

    /// The resource itself, for the worker that is about to invoke it.
    pub(crate) fn resource(&self) -> &dyn Resource {
        self.resource.as_ref()
    }

    /// Take this resource's one callback slot, or report that it is held.
    ///
    /// Instant, because the two phases that call it have somewhere else to be:
    /// a startup pass has the rest of the registry to visit, and a periodic
    /// pass skips a resource whose earlier probe is still running rather than
    /// queueing a second one behind it.
    pub(crate) fn try_claim(self: &Arc<Self>) -> Option<CallbackClaim> {
        let mut active = self.gate.active();
        match *active {
            true => None,
            false => {
                *active = true;
                Some(CallbackClaim {
                    entry: Arc::clone(self),
                })
            }
        }
    }

    /// Wait up to `limit` for this resource's callback slot, then take it.
    ///
    /// Teardown's form. A probe that was abandoned microseconds earlier is
    /// still finishing, and reporting that resource as unable to enter shutdown
    /// would name a hazard that resolved itself. A probe that is genuinely hung
    /// holds the slot for the whole of `limit` and is reported as the block it
    /// is.
    ///
    /// Blocking, and only ever called from the teardown thread, which has no
    /// runtime entered and nothing else to do until this resource is decided.
    pub(crate) fn claim_within(self: &Arc<Self>, limit: Duration) -> Option<CallbackClaim> {
        let active = self.gate.active();
        let (mut active, _) = recover_poisoned(self.gate.released.wait_timeout_while(
            active,
            limit,
            |active| *active,
        ));
        match *active {
            true => None,
            false => {
                *active = true;
                Some(CallbackClaim {
                    entry: Arc::clone(self),
                })
            }
        }
    }

    /// Publish what one callback did, and hand back the failure it names.
    ///
    /// The single writer of this resource's health. Success clears whatever an
    /// earlier phase published, so a resource that recovers stops being
    /// reported unwell without a second path having to remember to clear it.
    pub(crate) fn publish(
        &self,
        phase: ResourcePhase,
        outcome: Option<ResourceFailureKind>,
    ) -> Option<ResourceFailure> {
        let failure = outcome.map(|kind| ResourceFailure::new(Arc::clone(&self.name), phase, kind));
        recover_poisoned(self.published.write()).clone_from(&failure);
        failure
    }

    /// The bounded failure name `/health` renders for this resource, or `None`
    /// while its last callback succeeded.
    ///
    /// Only the closed label leaves: a cause's arbitrary text belongs in the
    /// operator event that carried it, never in a response body a peer reads.
    pub(crate) fn published_failure(&self) -> Option<&'static str> {
        recover_poisoned(self.published.read())
            .as_ref()
            .map(|failure| failure.kind().label())
    }
}

/// The one-callback-at-a-time gate a resource is entered through.
///
/// A plain flag would answer "is a callback running" but could not be waited
/// on, and teardown has to wait: the alternative is calling `shutdown`
/// concurrently with a probe that never returned.
struct CallbackGate {
    active: Mutex<bool>,
    released: Condvar,
}

impl CallbackGate {
    const fn new() -> Self {
        Self {
            active: Mutex::new(false),
            released: Condvar::new(),
        }
    }

    /// The gate's flag.
    ///
    /// Recovered rather than refused, for the reason the runtime's own locks
    /// are: the only writer that can poison this is a coordinator thread, and
    /// refusing here would wedge every later phase of a runtime that is
    /// already trying to stop.
    fn active(&self) -> std::sync::MutexGuard<'_, bool> {
        recover_poisoned(self.active.lock())
    }
}

/// One resource's callback slot, held for exactly as long as the callback runs.
///
/// Owned by the WORKER, not by the coordinator that started it. That is the
/// whole of the abandonment contract: a coordinator that stops waiting drops
/// only its wait, and the resource stays claimed until the callback it was
/// invoked on returns.
pub(crate) struct CallbackClaim {
    entry: Arc<ResourceEntry>,
}

impl CallbackClaim {
    /// The entry this claim was taken on.
    pub(crate) fn entry(&self) -> &ResourceEntry {
        &self.entry
    }
}

impl Drop for CallbackClaim {
    fn drop(&mut self) {
        let mut active = self.entry.gate.active();
        *active = false;
        self.entry.gate.released.notify_all();
    }
}
