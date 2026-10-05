//! One operation's account of the records its cleanup still owes.
//!
//! An order writes a create intention here before it submits the create,
//! names the record's exact ID once the provider acknowledges it, and clears
//! the record once its delete is acknowledged. The register is shared with the
//! operation's report slot. A forced stop that drops the order mid-flight
//! therefore still finds every record the order could not resolve, and the
//! slot keeps them charged instead of releasing.
//!
//! Every transition is one short critical section; no lock is held across a
//! provider call.

use crate::error::MAX_CLEANUP_ITEMS;
use crate::runtime_state::recover_poisoned;
use crate::{
    CleanupItem, IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation,
    Retryability,
};
use std::sync::{Arc, Mutex, MutexGuard};

/// Where one challenge record stands.
enum State {
    /// The create was about to be submitted; its outcome is not known.
    Intended,
    /// The provider acknowledged the record; no delete has been sent.
    Created(Box<str>),
    /// A delete of the record is in flight.
    Deleting(Box<str>),
    /// The record's delete ended with this failure; the record stays.
    Failed(Box<str>, IntegrationFailure),
    /// Nothing of the record remains: its create was refused, or its delete
    /// was acknowledged.
    Resolved,
}

/// One record the order meant to create.
struct Record {
    domain: Box<str>,
    state: State,
}

impl Record {
    /// Whether cleanup still owes this record.
    const fn is_owed(&self) -> bool {
        !matches!(self.state, State::Resolved)
    }

    /// The account item this record owes, or `None` once it is resolved.
    ///
    /// A record whose ID the order never learned is named by its domain alone:
    /// a create submitted without an answer may or may not have reached the
    /// zone.
    fn unresolved(&self) -> Option<CleanupItem> {
        let (record_id, failure) = match &self.state {
            State::Intended => (None, IntegrationFailure::OutcomeUnknown),
            State::Created(id) => (Some(id.clone()), IntegrationFailure::Cancelled),
            State::Deleting(id) => (Some(id.clone()), IntegrationFailure::OutcomeUnknown),
            State::Failed(id, failure) => (Some(id.clone()), *failure),
            State::Resolved => return None,
        };
        Some(CleanupItem::new(self.domain.clone(), record_id, failure))
    }
}

/// One record's place in its register.
#[derive(Clone, Copy)]
pub(crate) struct RecordSlot(usize);

/// The records one operation's cleanup still owes.
///
/// Cheap to clone; every clone is the same register.
#[derive(Clone)]
pub(crate) struct CleanupRegister {
    kind: IntegrationKind,
    operation: IntegrationOperation,
    records: Arc<Mutex<Vec<Record>>>,
}

impl CleanupRegister {
    /// An empty register for one `operation` of a `kind` integration.
    pub(crate) fn new(kind: IntegrationKind, operation: IntegrationOperation) -> Self {
        Self {
            kind,
            operation,
            records: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn records(&self) -> MutexGuard<'_, Vec<Record>> {
        recover_poisoned(self.records.lock())
    }

    /// Record the intention to create `domain`'s record, before the create is
    /// submitted.
    ///
    /// `None` once the register holds [`MAX_CLEANUP_ITEMS`] records: the
    /// create must not be submitted, because the account could not name it.
    pub(crate) fn intend(&self, domain: &str) -> Option<RecordSlot> {
        let mut records = self.records();
        match records.len() < MAX_CLEANUP_ITEMS {
            true => {
                records.push(Record {
                    domain: domain.into(),
                    state: State::Intended,
                });
                Some(RecordSlot(records.len() - 1))
            }
            false => None,
        }
    }

    /// The provider acknowledged the record under `id`.
    pub(crate) fn acknowledge(&self, slot: RecordSlot, id: &str) {
        self.update(slot, |_| State::Created(id.into()));
    }

    /// A delete of the record is about to be sent.
    pub(crate) fn deleting(&self, slot: RecordSlot) {
        self.update(slot, |state| match state {
            State::Created(id) => State::Deleting(id),
            other => other,
        });
    }

    /// The record's delete ended with `failure`; the record stays.
    pub(crate) fn failed(&self, slot: RecordSlot, failure: IntegrationFailure) {
        self.update(slot, |state| match state {
            State::Deleting(id) => State::Failed(id, failure),
            other => other,
        });
    }

    /// Nothing of the record remains in the zone.
    pub(crate) fn resolve(&self, slot: RecordSlot) {
        self.update(slot, |_| State::Resolved);
    }

    /// Every record whose ID is known and whose delete has not been sent.
    pub(crate) fn created(&self) -> Box<[(RecordSlot, Box<str>)]> {
        self.records()
            .iter()
            .enumerate()
            .filter_map(|(index, record)| match &record.state {
                State::Created(id) => Some((RecordSlot(index), id.clone())),
                _ => None,
            })
            .collect()
    }

    /// The cleanup bound passed: every known record whose delete has not
    /// answered stays, as `Timeout`.
    pub(crate) fn expire(&self) {
        for record in self.records().iter_mut() {
            record.state = match std::mem::replace(&mut record.state, State::Resolved) {
                State::Created(id) | State::Deleting(id) => {
                    State::Failed(id, IntegrationFailure::Timeout)
                }
                other => other,
            };
        }
    }

    /// Whether any record is still unresolved.
    pub(crate) fn owes(&self) -> bool {
        self.records().iter().any(Record::is_owed)
    }

    /// The `CleanupIncomplete` failure naming every unresolved record, or
    /// `None` when nothing is owed.
    pub(crate) fn incomplete(&self) -> Option<IntegrationError> {
        let unresolved: Vec<CleanupItem> = self
            .records()
            .iter()
            .filter_map(Record::unresolved)
            .collect();
        match unresolved.is_empty() {
            true => None,
            false => Some(
                IntegrationError::new(
                    self.kind,
                    self.operation,
                    IntegrationFailure::CleanupIncomplete,
                    Retryability::Never,
                )
                .with_cleanup(unresolved),
            ),
        }
    }

    fn update(&self, slot: RecordSlot, change: impl FnOnce(State) -> State) {
        if let Some(record) = self.records().get_mut(slot.0) {
            let state = std::mem::replace(&mut record.state, State::Resolved);
            record.state = change(state);
        }
    }
}
