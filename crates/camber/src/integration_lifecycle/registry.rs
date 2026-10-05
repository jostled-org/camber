//! The runtime's registry of live integration instances.
//!
//! One registry per runtime, holding at most [`LIVE_LIMIT`] entries. Admission
//! is atomic with root-scope closure: it reads the root scope's admission under
//! the registry's own lock, and the stop that root closure starts takes that
//! lock after the scope has closed. An admission is therefore either refused or
//! reached by the stop; none lands between the two unowned.
//!
//! An entry keeps its live slot until it settles. Teardown then names any
//! entry whose work it could not get back, and transfers the retained report
//! accounts into the runtime aggregate once, before any resource shuts down.

use super::access::{IntegrationAccess, integration};
use super::accounts::{ReportAccounts, busy};
use super::entry::IntegrationEntry;
use crate::lifecycle::{
    LifecycleFailureKind, LifecycleFailureLog, LifecycleParticipant, LifecyclePhase,
};
use crate::runtime_state::{RuntimeInner, recover_poisoned};
use crate::{IntegrationKind, IntegrationOperation, RuntimeError};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard};

/// The fixed number of live integration instances one runtime admits.
const LIVE_LIMIT: usize = 64;

/// What the registry's lock guards.
struct Live {
    /// Set once root closure started the registry stop; admission refuses
    /// from then on.
    stopping: bool,
    entries: BTreeMap<u64, Arc<IntegrationEntry>>,
}

/// The runtime's owner of its live integration instances.
pub(crate) struct IntegrationRegistry {
    live: Mutex<Live>,
    accounts: Arc<ReportAccounts>,
}

impl IntegrationRegistry {
    /// An empty registry with an empty report budget.
    pub(crate) fn new() -> Self {
        Self {
            live: Mutex::new(Live {
                stopping: false,
                entries: BTreeMap::new(),
            }),
            accounts: ReportAccounts::new(),
        }
    }

    fn live(&self) -> MutexGuard<'_, Live> {
        recover_poisoned(self.live.lock())
    }

    /// Admit one instance of `kind` to `runtime` through its `admission`
    /// operation, before any I/O, running at most `operation_limit`
    /// operations at once.
    ///
    /// # Errors
    ///
    /// `ScopeClosed` once root admission has closed, `Busy` under `admission`
    /// at the live limit or a full report budget, and `LimitExceeded` once
    /// identities are spent. A refusal admits nothing and reserves nothing.
    pub(crate) fn admit(
        &self,
        runtime: &Arc<RuntimeInner>,
        kind: IntegrationKind,
        admission: IntegrationOperation,
        operation_limit: usize,
    ) -> Result<IntegrationAccess, RuntimeError> {
        let mut live = self.live();
        if live.stopping || !runtime.admits_children() {
            return Err(RuntimeError::ScopeClosed);
        }
        if live.entries.len() >= LIVE_LIMIT {
            return Err(integration(busy(kind, admission)));
        }
        let account = self.accounts.admit(kind, admission).map_err(integration)?;
        let entry = IntegrationEntry::new(account, Arc::downgrade(runtime), operation_limit);
        live.entries.insert(entry.id(), Arc::clone(&entry));
        Ok(IntegrationAccess::new(entry))
    }

    /// Advance admission to an unused identity, refusing rewind or exhaustion.
    pub(crate) fn seed_next_identity(&self, id: u64) -> Result<(), RuntimeError> {
        self.accounts.seed_next_instance(id)
    }

    /// Release a settled entry's live slot.
    pub(super) fn retire(&self, id: u64) {
        self.live().entries.remove(&id);
    }

    /// Refuse further admission and commit closing on every live entry.
    ///
    /// Root closure calls this, so the stop begins before the root drain
    /// waits. Idempotent. The entries are closed outside the registry's lock,
    /// because an entry that settles at once retires itself through it.
    pub(crate) fn stop(&self) {
        for entry in self.stopped_entries() {
            entry.stop();
        }
    }

    /// Latch the stop and take the entries it must reach.
    fn stopped_entries(&self) -> Box<[Arc<IntegrationEntry>]> {
        let mut live = self.live();
        live.stopping = true;
        live.entries.values().cloned().collect()
    }

    /// Settle what the drain left behind and hand the history to `log`.
    ///
    /// Runs after the root drain and its forced stop, before resources shut
    /// down. An entry still live here has work the forced stop could not get
    /// back, so it is named rather than waited for: no new grace is taken. The
    /// retained accounts then leave once, each under its own instance.
    pub(crate) fn settle_into(&self, log: &mut LifecycleFailureLog) {
        for entry in self.stopped_entries() {
            entry.name_outstanding();
        }
        let mut accounts = self.accounts.transfer();
        accounts.sort_by_key(|(id, _)| *id);
        for (id, error) in accounts {
            log.record(
                LifecycleParticipant::Integration {
                    kind: error.kind(),
                    id,
                },
                LifecyclePhase::GracefulDrain,
                LifecycleFailureKind::Operation(Arc::new(integration(error))),
            );
        }
    }
}
