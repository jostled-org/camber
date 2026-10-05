//! The runtime-wide budget of integration report accounts.
//!
//! One slot holds one operation's terminal failure, or reserves room for it.
//! Live reservations and retained history count together against one fixed
//! budget, so the accounts a runtime keeps are bounded however long it runs.
//!
//! Admission reserves the instance's close slot before any I/O; each operation
//! reserves its own slot before submission. Settlement never takes a new slot:
//! success releases, a received ordinary failure releases, and an abandoned
//! failure, a failed cleanup, or a failed close becomes history. An operation
//! that keeps a cleanup register settles from it when a forced stop or an
//! unwind drops its work, so records it still owes become history too.
//! History is never evicted. It leaves only when teardown transfers it, once.
//!
//! Every transition is one short critical section over plain counts; no lock
//! is held across I/O. Each retained failure is a copy of the failure its
//! waiter receives. The copy is cheap: its fields are plain values and shared
//! `Arc`s.

#[cfg(feature = "dns01")]
use super::cleanup::CleanupRegister;
use crate::runtime_state::recover_poisoned;
use crate::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
};
use std::sync::{Arc, Mutex, MutexGuard};

/// The fixed runtime-wide budget of reserved plus retained report accounts.
const REPORT_BUDGET: usize = 256;

/// The first identity admission assigns.
const FIRST_INSTANCE_ID: u64 = 1;

/// What the budget holds right now.
struct Ledger {
    /// Slots reserved for a result that has not settled.
    reserved: usize,
    /// Failures kept until teardown transfers them, each under the instance
    /// that owns it.
    retained: Vec<(u64, IntegrationError)>,
    /// The identity admission assigns next, or `None` once every identity
    /// has been handed out.
    next_instance: Option<u64>,
}

impl Ledger {
    /// Reserved plus retained accounts.
    fn charged(&self) -> usize {
        self.reserved + self.retained.len()
    }

    /// Whether one more account fits.
    fn has_room(&self) -> bool {
        self.charged() < REPORT_BUDGET
    }
}

/// The owner of the runtime's integration report accounts.
pub(crate) struct ReportAccounts {
    ledger: Mutex<Ledger>,
}

impl ReportAccounts {
    /// An empty budget.
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            ledger: Mutex::new(Ledger {
                reserved: 0,
                retained: Vec::new(),
                next_instance: Some(FIRST_INSTANCE_ID),
            }),
        })
    }

    fn ledger(&self) -> MutexGuard<'_, Ledger> {
        recover_poisoned(self.ledger.lock())
    }

    /// Admit one instance through its `admission` operation: assign its
    /// identity and reserve its close slot.
    ///
    /// # Errors
    ///
    /// `Busy` when the budget is full and `LimitExceeded` once every identity
    /// is spent, both under `admission`. A refusal reserves nothing and leaves
    /// no history.
    pub(crate) fn admit(
        self: &Arc<Self>,
        kind: IntegrationKind,
        admission: IntegrationOperation,
    ) -> Result<InstanceAccount, IntegrationError> {
        let id = self.reserve_instance(kind, admission)?;
        Ok(InstanceAccount {
            kind,
            accounts: Arc::clone(self),
            close: Slot::held(Arc::clone(self), id),
        })
    }

    /// Take the next identity and one slot together, or neither.
    fn reserve_instance(
        &self,
        kind: IntegrationKind,
        admission: IntegrationOperation,
    ) -> Result<u64, IntegrationError> {
        let mut ledger = self.ledger();
        match (ledger.has_room(), ledger.next_instance) {
            (false, _) => Err(busy(kind, admission)),
            (true, None) => Err(IntegrationError::new(
                kind,
                admission,
                IntegrationFailure::LimitExceeded,
                Retryability::Never,
            )),
            (true, Some(id)) => {
                ledger.next_instance = id.checked_add(1);
                ledger.reserved += 1;
                Ok(id)
            }
        }
    }

    /// Reserve one slot for `kind`'s `operation`, if the budget has room.
    fn reserve_slot(
        &self,
        kind: IntegrationKind,
        operation: IntegrationOperation,
    ) -> Result<(), IntegrationError> {
        let mut ledger = self.ledger();
        match ledger.has_room() {
            true => {
                ledger.reserved += 1;
                Ok(())
            }
            false => Err(busy(kind, operation)),
        }
    }

    /// Settle one reservation with nothing to keep.
    fn release(&self) {
        let mut ledger = self.ledger();
        ledger.reserved = ledger.reserved.saturating_sub(1);
    }

    /// Settle one reservation into history under `instance`, which `error`
    /// takes, in the same critical section, so the charge never drops between
    /// the two.
    fn retain(&self, instance: u64, error: IntegrationError) {
        let error = error.with_instance(instance);
        let mut ledger = self.ledger();
        ledger.reserved = ledger.reserved.saturating_sub(1);
        ledger.retained.push((instance, error));
    }

    /// Reserved plus retained accounts.
    pub(crate) fn charged(&self) -> usize {
        self.ledger().charged()
    }

    /// Retained failures awaiting transfer.
    pub(crate) fn retained(&self) -> usize {
        self.ledger().retained.len()
    }

    /// Hand every retained failure over with its instance, once. A second
    /// transfer finds none.
    pub(crate) fn transfer(&self) -> Box<[(u64, IntegrationError)]> {
        std::mem::take(&mut self.ledger().retained).into_boxed_slice()
    }

    /// Advance the next identity without reusing an issued identity.
    ///
    /// A test driver's way to reach the last identity without admitting every
    /// one before it. Admission still decides the outcome: the checked
    /// increment after `u64::MAX` is what spends the identities.
    pub(crate) fn seed_next_instance(&self, id: u64) -> Result<(), crate::RuntimeError> {
        let mut ledger = self.ledger();
        match ledger.next_instance {
            Some(next) if id >= next => {
                ledger.next_instance = Some(id);
                Ok(())
            }
            _ => Err(crate::RuntimeError::Config(
                "integration identity seed must not reuse an issued or exhausted identity".into(),
            )),
        }
    }
}

/// The typed refusal a full budget answers with.
///
/// Nothing was submitted, so repeating the work is safe.
pub(crate) const fn busy(
    kind: IntegrationKind,
    operation: IntegrationOperation,
) -> IntegrationError {
    IntegrationError::new(
        kind,
        operation,
        IntegrationFailure::Busy,
        Retryability::Safe,
    )
}

/// Whether a failure stays charged after its waiter reads it.
///
/// Unresolved cleanup records are the caller's to resolve, so they stay in
/// history until teardown reports them, whoever else saw them first.
fn outlives_receipt(error: &IntegrationError) -> bool {
    error.failure() == IntegrationFailure::CleanupIncomplete || !error.cleanup().is_empty()
}

/// One charged slot of one instance. Dropping it releases the charge, unless
/// its operation's cleanup still owes records; retaining it moves the charge
/// into history under that instance.
struct Slot {
    accounts: Option<Arc<ReportAccounts>>,
    instance: u64,
    /// The records the slot's operation must still clean up, when it keeps
    /// any.
    #[cfg(feature = "dns01")]
    cleanup: Option<CleanupRegister>,
}

impl Slot {
    const fn held(accounts: Arc<ReportAccounts>, instance: u64) -> Self {
        Self {
            accounts: Some(accounts),
            instance,
            #[cfg(feature = "dns01")]
            cleanup: None,
        }
    }

    /// Keep `error` as history under this slot's charge and instance.
    fn retain(mut self, error: IntegrationError) {
        if let Some(accounts) = self.accounts.take() {
            accounts.retain(self.instance, error);
        }
    }

    /// What a slot dropped without a result still owes: the records its
    /// operation's cleanup left unresolved.
    #[cfg(feature = "dns01")]
    fn unresolved(&self) -> Option<IntegrationError> {
        self.cleanup.as_ref().and_then(CleanupRegister::incomplete)
    }

    /// Without a cleanup register, a slot dropped without a result owes
    /// nothing.
    #[cfg(not(feature = "dns01"))]
    const fn unresolved(&self) -> Option<IntegrationError> {
        None
    }
}

impl Drop for Slot {
    /// A slot dropped without a result was dropped with its work: by success,
    /// by a forced stop, or by an unwind. Records its cleanup still owes keep
    /// the charge as `CleanupIncomplete`; otherwise the charge is released.
    fn drop(&mut self) {
        let Some(accounts) = self.accounts.take() else {
            return;
        };
        match self.unresolved() {
            Some(incomplete) => {
                accounts.retain(self.instance, incomplete);
            }
            None => accounts.release(),
        }
    }
}

/// One admitted integration instance and its reserved close account.
///
/// Dropping it without a close settles the close as successful.
#[must_use = "an instance holds its close reservation until it closes"]
pub struct InstanceAccount {
    kind: IntegrationKind,
    accounts: Arc<ReportAccounts>,
    /// The close slot, charged under this instance's identity.
    close: Slot,
}

impl InstanceAccount {
    /// The runtime-local identity admission assigned.
    #[must_use]
    pub const fn instance_id(&self) -> u64 {
        self.close.instance
    }

    /// The integration kind admission charged this instance to.
    #[must_use]
    pub const fn kind(&self) -> IntegrationKind {
        self.kind
    }

    /// Reserve one operation's report account before it submits anything.
    ///
    /// # Errors
    ///
    /// `Busy` when the budget is full. The refusal reserves nothing and leaves
    /// no history.
    pub fn reserve(
        &self,
        operation: IntegrationOperation,
    ) -> Result<OperationAccount, IntegrationError> {
        self.accounts
            .reserve_slot(self.kind, operation)
            .map_err(|refused| refused.with_instance(self.instance_id()))?;
        Ok(OperationAccount {
            slot: Slot::held(Arc::clone(&self.accounts), self.instance_id()),
        })
    }

    /// Settle a successful close, releasing the close account.
    pub fn close(self) {}

    /// Settle a failed close into the account admission reserved for it.
    ///
    /// The live instance retires; its failed-close account stays charged
    /// until teardown transfers it.
    pub fn close_failed(self, error: IntegrationError) {
        self.close.retain(error);
    }
}

/// One operation's reserved report account.
///
/// Dropping it without a result settles it as successful.
#[must_use = "an operation holds its reservation until it settles"]
pub struct OperationAccount {
    /// The reservation, charged under the owning instance's identity.
    slot: Slot,
}

impl OperationAccount {
    /// Settle success, releasing the reservation.
    pub fn succeed(self) {}

    /// Charge the records `cleanup` still owes to this account when it is
    /// dropped without a result.
    ///
    /// The operation's work shares the register. A forced stop or an unwind
    /// that drops the work therefore cannot release an account whose records
    /// are still in the zone.
    #[cfg(feature = "dns01")]
    pub(crate) fn with_cleanup(mut self, cleanup: CleanupRegister) -> Self {
        self.slot.cleanup = Some(cleanup);
        self
    }

    /// Publish a failure to the operation's waiter.
    ///
    /// The failure takes the instance's identity. An ordinary failure keeps
    /// its reservation until the waiter receives it; a failed cleanup moves
    /// into history now and stays charged after receipt.
    pub fn fail(self, error: IntegrationError) -> PublishedFailure {
        let error = error.with_instance(self.slot.instance);
        match outlives_receipt(&error) {
            true => {
                self.slot.retain(error.clone());
                PublishedFailure { error, slot: None }
            }
            false => PublishedFailure {
                error,
                slot: Some(self.slot),
            },
        }
    }
}

/// A failure published to its waiter and not yet read.
///
/// Publication is not delivery: the reservation stays charged until the
/// waiter receives the failure. Dropping it unread is abandonment.
#[must_use = "an unread failure is abandoned into history when dropped"]
pub struct PublishedFailure {
    error: IntegrationError,
    /// The reservation still owed, or `None` once the failure is history.
    slot: Option<Slot>,
}

impl PublishedFailure {
    /// The waiter receives the failure, releasing an ordinary reservation.
    #[must_use]
    pub fn receive(mut self) -> IntegrationError {
        drop(self.slot.take());
        self.error.clone()
    }

    /// The waiter is gone: the failure becomes retained history.
    pub fn abandon(self) {}
}

impl Drop for PublishedFailure {
    fn drop(&mut self) {
        if let Some(slot) = self.slot.take() {
            slot.retain(self.error.clone());
        }
    }
}
