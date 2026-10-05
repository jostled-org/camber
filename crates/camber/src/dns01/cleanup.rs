//! The cleanup that follows every order: exact deletes under one bound.
//!
//! It runs once the order's result is fixed, on success, failure, and stop
//! alike. Each record the provider acknowledged is deleted by its exact ID. A
//! record whose create lost its answer is never deleted by name, because the
//! challenge name can also hold records this order did not create. It stays in
//! the account instead.
//!
//! A stop does not interrupt cleanup: cleanup is what a stop leaves to do. The
//! configured cap bounds it, cut short by the runtime's aggregate expiry once
//! a stop has fixed one. A record whose delete has not answered by then is
//! named, never assumed deleted.
//!
//! Each delete is one `DeleteTxt` terminal naming its exact record ID. A
//! delete still outstanding when the bound cuts cleanup short is cancelled.

use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use super::intention::callback_answer;
use super::order::OrderStop;
use super::provider::DnsProvider;
use crate::integration_lifecycle::{CleanupRegister, NestedTerminals};
use crate::{IntegrationError, IntegrationOperation};

/// Delete every acknowledged record in `records` within `cap` and the
/// runtime's aggregate expiry, reporting each delete to `terminals`.
pub(super) async fn clean_up<P: DnsProvider>(
    provider: &P,
    records: &CleanupRegister,
    terminals: &NestedTerminals,
    cap: Duration,
    stop: &OrderStop,
) {
    let deadline = Instant::now() + cap;
    tokio::select! {
        biased;
        () = delete_created(provider, records, terminals) => {}
        () = tokio::time::sleep_until(deadline) => records.expire(),
        () = stop.shutdown_bound(cap) => records.expire(),
    }
}

/// Delete each acknowledged record by its exact ID, recording each answer.
///
/// A delete that unwound may or may not have reached the zone.
async fn delete_created<P: DnsProvider>(
    provider: &P,
    records: &CleanupRegister,
    terminals: &NestedTerminals,
) {
    let operation = IntegrationOperation::DeleteTxt;
    for (slot, id) in records.created() {
        records.deleting(slot);
        let owed = terminals.begin(operation).with_record(&id);
        let deleted = callback_answer(
            operation,
            crate::task::catch_panic_async(provider.delete_txt_record(&id)).await,
        );
        match &deleted {
            Ok(()) => records.resolve(slot),
            Err(error) => records.failed(slot, error.failure()),
        }
        terminals.settle(owed, &deleted);
    }
}

/// The order's answer once cleanup settled: `issued` when nothing is owed,
/// and otherwise `CleanupIncomplete` naming every unresolved record.
///
/// A failed order keeps its own failure as the source, so the caller can
/// still read why the order ended.
pub(super) fn settle<T>(
    issued: Result<T, IntegrationError>,
    records: &CleanupRegister,
) -> Result<T, IntegrationError> {
    match (records.incomplete(), issued) {
        (None, issued) => issued,
        (Some(incomplete), Ok(_)) => Err(incomplete),
        (Some(incomplete), Err(failed)) => Err(incomplete.with_source(Arc::new(failed))),
    }
}
