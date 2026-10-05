//! One challenge record's create, under an intention the order keeps.
//!
//! The intention is written before the create is submitted, so a create
//! interrupted after submission is an unknown record rather than an absent
//! one. The provider's acknowledgement names the exact ID cleanup deletes. Only
//! a typed refusal that says nothing was created withdraws the intention;
//! every other failure, an unwind, and an interrupted await leave the record
//! named by its domain.
//!
//! Each create is one `CreateTxt` terminal, naming the acknowledged ID. A
//! create refused before it was sent records no duration, and one the order
//! cuts short is cancelled.

use std::sync::Arc;

use super::failure::{failure, outcome_unknown};
use super::order::provider_failure;
use super::provider::DnsProvider;
use crate::integration_lifecycle::{CleanupRegister, NestedTerminals};
use crate::{
    IntegrationError, IntegrationFailure, IntegrationOperation, Retryability, RuntimeError,
};

/// Create `domain`'s challenge record at `fqdn`, recording its intention in
/// `records` first and its ID once the provider acknowledges it, and
/// reporting the create to `terminals`.
///
/// # Errors
///
/// `CreateTxt` with `LimitExceeded` when the register is full, before
/// anything is sent. Otherwise the provider's typed failure; an unwind is
/// `OutcomeUnknown`.
pub(super) async fn create_record<P: DnsProvider>(
    provider: &P,
    records: &CleanupRegister,
    terminals: &NestedTerminals,
    domain: &str,
    fqdn: &str,
    value: &str,
) -> Result<(), IntegrationError> {
    let operation = IntegrationOperation::CreateTxt;
    let Some(slot) = records.intend(domain) else {
        let full = failure(
            operation,
            IntegrationFailure::LimitExceeded,
            Retryability::Never,
        );
        terminals.refused(operation, &full);
        return Err(full);
    };
    let owed = terminals.begin(operation);
    let outcome = crate::task::catch_panic_async(provider.create_txt_record(fqdn, value)).await;
    let refused = matches!(&outcome, Ok(Err(error)) if created_nothing(error));
    let (owed, created) = match (callback_answer(operation, outcome), refused) {
        (Ok(id), _) => {
            records.acknowledge(slot, &id);
            (owed.with_record(&id), Ok(()))
        }
        (Err(error), true) => {
            records.resolve(slot);
            (owed, Err(error))
        }
        (Err(error), false) => (owed, Err(error)),
    };
    terminals.settle(owed, &created);
    created
}

/// Whether a failed create proves nothing reached the zone: a typed refusal
/// whose outcome is known.
///
/// An untyped provider error says nothing about the zone, so it keeps the
/// record unknown.
fn created_nothing(error: &RuntimeError) -> bool {
    match error {
        RuntimeError::Integration(typed) => {
            typed.retryability() != Retryability::OutcomeUnknown
                && typed.failure() != IntegrationFailure::OutcomeUnknown
        }
        _ => false,
    }
}

/// A guarded provider callback's answer as `operation`'s: the provider's own
/// failure typed, and an unwind as `OutcomeUnknown`.
pub(super) fn callback_answer<T>(
    operation: IntegrationOperation,
    outcome: Result<Result<T, RuntimeError>, RuntimeError>,
) -> Result<T, IntegrationError> {
    match outcome {
        Ok(answer) => answer.map_err(|error| provider_failure(operation, error)),
        Err(panicked) => Err(unwound(operation, panicked)),
    }
}

/// The failure of a provider callback that unwound: a write it was making may
/// or may not have reached the zone.
fn unwound(operation: IntegrationOperation, panicked: RuntimeError) -> IntegrationError {
    outcome_unknown(operation).with_source(Arc::new(panicked))
}
