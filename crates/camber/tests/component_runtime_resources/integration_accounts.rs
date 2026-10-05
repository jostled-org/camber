//! The runtime-wide integration report budget: 256 reserved plus retained
//! accounts, entered through the doc-hidden probe over the production owner.
//!
//! The probe drives the owner's own transitions — admission, operation
//! reservation, success, a published failure its waiter receives or abandons,
//! and close — and reads its counts. It holds no alternate model and cannot
//! set a count. No SDK, runtime, thread, or timer takes part: this proves the
//! accounting substrate only. Adapter and runtime wiring belong to later roots.
//!
//! Each row builds its own owner and returns its own failure, so one broken
//! transition cannot hide the others.

use crate::integration_rows::{REPORT_BUDGET, Row, expect_eq, run_rows};
use camber::runtime_test_support::{IntegrationAccountProbe, IntegrationErrorDriver};
use camber::{
    IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability,
};

/// An ordinary operation failure, built through the production factory.
fn ordinary(kind: IntegrationKind, operation: IntegrationOperation) -> IntegrationError {
    IntegrationErrorDriver::new(
        kind,
        operation,
        IntegrationFailure::Unavailable,
        Retryability::Safe,
    )
    .build()
}

/// A DNS order whose cleanup left these records unresolved.
fn failed_cleanup(
    operation: IntegrationOperation,
    records: &[(&str, Option<&str>)],
) -> IntegrationError {
    records
        .iter()
        .fold(
            IntegrationErrorDriver::new(
                IntegrationKind::Dns01,
                operation,
                IntegrationFailure::CleanupIncomplete,
                Retryability::Never,
            ),
            |driver, (domain, record_id)| {
                driver.cleanup(domain, *record_id, IntegrationFailure::Unavailable)
            },
        )
        .build()
}

/// A close that failed to settle.
fn failed_close(kind: IntegrationKind) -> IntegrationError {
    IntegrationErrorDriver::new(
        kind,
        IntegrationOperation::Close,
        IntegrationFailure::Timeout,
        Retryability::Never,
    )
    .build()
}

/// Name a refused admission or reservation as the row's failure.
fn refused(what: &'static str) -> impl FnOnce(IntegrationError) -> String {
    move |error| format!("{what} refused: {error}")
}

/// A refusal must be the typed Busy answer for the work it refused.
pub(crate) fn expect_busy(what: &str, refusal: &IntegrationError, kind: IntegrationKind) -> Row {
    expect_eq(
        &format!("{what} failure"),
        refusal.failure(),
        IntegrationFailure::Busy,
    )?;
    expect_eq(
        &format!("{what} retryability"),
        refusal.retryability(),
        Retryability::Safe,
    )?;
    expect_eq(&format!("{what} kind"), refusal.kind(), kind)
}

/// One retained account as the identity, operation, and failure it keeps.
fn account_row(
    error: &IntegrationError,
) -> (Option<u64>, IntegrationOperation, IntegrationFailure) {
    (error.instance_id(), error.operation(), error.failure())
}

/// The transferred accounts as rows, in one fixed order a test can compare.
///
/// The transfer promises no order, and the operation enum has none of its
/// own, so rows sort by identity and then by the operation's name.
fn sorted_account_rows(
    mut rows: Vec<(Option<u64>, IntegrationOperation, IntegrationFailure)>,
) -> Vec<(Option<u64>, IntegrationOperation, IntegrationFailure)> {
    rows.sort_by_cached_key(|(id, operation, _)| (*id, format!("{operation:?}")));
    rows
}

/// The cleanup records one account keeps, borrowed from it.
fn cleanup_rows(error: &IntegrationError) -> Vec<(&str, Option<&str>)> {
    error
        .cleanup()
        .iter()
        .map(|item| (item.domain(), item.record_id()))
        .collect()
}

// ── 3.T1 ──────────────────────────────────────────────────────────────

#[test]
fn integration_accounts_reserve_retire_and_transfer_once() {
    run_rows(&[
        (
            "budget refuses account 257",
            budget_refuses_account_after_256,
        ),
        ("refusal leaves no history", refusal_leaves_no_history),
        ("success releases its slot", success_releases_its_slot),
        (
            "delivered error releases on receipt",
            delivered_error_releases_on_receipt,
        ),
        ("unread result stays reserved", unread_result_stays_reserved),
        ("abandoned error is retained", abandoned_error_is_retained),
        (
            "failed cleanup stays charged",
            failed_cleanup_stays_charged_after_receipt,
        ),
        (
            "failed close outlives the instance",
            failed_close_outlives_the_instance,
        ),
        (
            "identities are monotonic",
            instance_identities_are_monotonic,
        ),
        ("transfer happens once", retained_accounts_transfer_once),
    ]);
}

/// One instance slot plus 255 operation slots fill the budget; both kinds of
/// reservation are then refused, and the count does not move.
fn budget_refuses_account_after_256() -> Row {
    let probe = IntegrationAccountProbe::new();
    let instance = probe
        .admit(IntegrationKind::Nats)
        .map_err(refused("first admission"))?;
    expect_eq("charged after admission", probe.charged(), 1)?;
    let mut held = Vec::with_capacity(REPORT_BUDGET - 1);
    for slot in 1..REPORT_BUDGET {
        held.push(
            instance
                .reserve(IntegrationOperation::Publish)
                .map_err(|error| format!("reservation {} refused: {error}", slot + 1))?,
        );
        expect_eq("charged while filling", probe.charged(), slot + 1)?;
    }
    expect_eq("charged at the budget", probe.charged(), REPORT_BUDGET)?;
    let operation = instance
        .reserve(IntegrationOperation::Publish)
        .err()
        .ok_or("operation reservation 257 was admitted")?;
    expect_busy("operation refusal", &operation, IntegrationKind::Nats)?;
    expect_eq(
        "operation refusal operation",
        operation.operation(),
        IntegrationOperation::Publish,
    )?;
    let admission = probe
        .admit(IntegrationKind::Sqs)
        .err()
        .ok_or("instance admission 257 was admitted")?;
    expect_busy("admission refusal", &admission, IntegrationKind::Sqs)?;
    expect_eq("charged after refusals", probe.charged(), REPORT_BUDGET)?;
    held.into_iter().for_each(|operation| operation.succeed());
    instance.close();
    expect_eq("charged after settlement", probe.charged(), 0)
}

/// A refusal has a caller and nothing else: no retained account, no transfer.
fn refusal_leaves_no_history() -> Row {
    let probe = IntegrationAccountProbe::new();
    let instance = probe
        .admit(IntegrationKind::Sqs)
        .map_err(refused("admission"))?;
    let held: Vec<_> = (1..REPORT_BUDGET)
        .map(|_| instance.reserve(IntegrationOperation::Receive))
        .collect::<Result<_, _>>()
        .map_err(refused("early filling"))?;
    for _ in 0..8 {
        let _refused = instance
            .reserve(IntegrationOperation::Receive)
            .err()
            .ok_or("over-budget reservation was admitted")?;
        let _refused = probe
            .admit(IntegrationKind::Dns01)
            .err()
            .ok_or("over-budget admission was admitted")?;
    }
    expect_eq("retained after refusals", probe.retained(), 0)?;
    held.into_iter().for_each(|operation| operation.succeed());
    instance.close();
    let transferred = probe.transfer();
    expect_eq(
        "refusals in the transfer",
        transferred
            .iter()
            .filter(|error| error.failure() == IntegrationFailure::Busy)
            .count(),
        0,
    )?;
    expect_eq("transferred accounts", transferred.len(), 0)
}

/// A successful operation frees its slot at settlement.
fn success_releases_its_slot() -> Row {
    let probe = IntegrationAccountProbe::new();
    let instance = probe
        .admit(IntegrationKind::Nats)
        .map_err(refused("admission"))?;
    let operation = instance
        .reserve(IntegrationOperation::Subscribe)
        .map_err(refused("reservation"))?;
    expect_eq("charged while reserved", probe.charged(), 2)?;
    operation.succeed();
    expect_eq("charged after success", probe.charged(), 1)?;
    expect_eq("retained after success", probe.retained(), 0)?;
    instance.close();
    expect_eq("charged after close", probe.charged(), 0)
}

/// An ordinary error is released by the waiter's receipt, not by publication.
fn delivered_error_releases_on_receipt() -> Row {
    let probe = IntegrationAccountProbe::new();
    let instance = probe
        .admit(IntegrationKind::Sqs)
        .map_err(refused("admission"))?;
    let published = instance
        .reserve(IntegrationOperation::Delete)
        .map_err(refused("reservation"))?
        .fail(ordinary(IntegrationKind::Sqs, IntegrationOperation::Delete));
    expect_eq("charged while published", probe.charged(), 2)?;
    let received = published.receive();
    expect_eq(
        "received operation",
        received.operation(),
        IntegrationOperation::Delete,
    )?;
    expect_eq(
        "received identity",
        received.instance_id(),
        Some(instance.instance_id()),
    )?;
    expect_eq("charged after receipt", probe.charged(), 1)?;
    expect_eq("retained after receipt", probe.retained(), 0)?;
    instance.close();
    expect_eq("transferred accounts", probe.transfer().len(), 0)
}

/// A result sitting unread in its channel still holds its reservation.
fn unread_result_stays_reserved() -> Row {
    let probe = IntegrationAccountProbe::new();
    let instance = probe
        .admit(IntegrationKind::Nats)
        .map_err(refused("admission"))?;
    let unread: Vec<_> = (0..4)
        .map(|_| {
            instance
                .reserve(IntegrationOperation::Publish)
                .map(|operation| {
                    operation.fail(ordinary(
                        IntegrationKind::Nats,
                        IntegrationOperation::Publish,
                    ))
                })
        })
        .collect::<Result<_, _>>()
        .map_err(refused("reservation"))?;
    expect_eq("charged with unread results", probe.charged(), 5)?;
    expect_eq("retained with unread results", probe.retained(), 0)?;
    unread
        .into_iter()
        .for_each(|published| drop(published.receive()));
    expect_eq("charged after reading", probe.charged(), 1)?;
    instance.close();
    Ok(())
}

/// A failure whose waiter is gone becomes history and stays charged.
fn abandoned_error_is_retained() -> Row {
    let probe = IntegrationAccountProbe::new();
    let instance = probe
        .admit(IntegrationKind::Sqs)
        .map_err(refused("admission"))?;
    let id = instance.instance_id();
    instance
        .reserve(IntegrationOperation::Publish)
        .map_err(refused("reservation"))?
        .fail(ordinary(
            IntegrationKind::Sqs,
            IntegrationOperation::Publish,
        ))
        .abandon();
    expect_eq("charged after abandonment", probe.charged(), 2)?;
    expect_eq("retained after abandonment", probe.retained(), 1)?;
    instance.close();
    expect_eq("charged after close", probe.charged(), 1)?;
    let transferred = probe.transfer();
    expect_eq(
        "transferred accounts",
        transferred.iter().map(account_row).collect::<Vec<_>>(),
        vec![(
            Some(id),
            IntegrationOperation::Publish,
            IntegrationFailure::Unavailable,
        )],
    )
}

/// Failed cleanup keeps its slot until shutdown, even after its waiter reads it.
fn failed_cleanup_stays_charged_after_receipt() -> Row {
    let probe = IntegrationAccountProbe::new();
    let instance = probe
        .admit(IntegrationKind::Dns01)
        .map_err(refused("admission"))?;
    let received = instance
        .reserve(IntegrationOperation::Provision)
        .map_err(refused("reservation"))?
        .fail(failed_cleanup(
            IntegrationOperation::Provision,
            &[("a.example.test", Some("rec-a")), ("b.example.test", None)],
        ))
        .receive();
    expect_eq(
        "received failure",
        received.failure(),
        IntegrationFailure::CleanupIncomplete,
    )?;
    expect_eq("charged after receipt", probe.charged(), 2)?;
    expect_eq("retained after receipt", probe.retained(), 1)?;
    instance.close();
    expect_eq("charged after close", probe.charged(), 1)?;
    let transferred = probe.transfer();
    expect_eq("transferred accounts", transferred.len(), 1)?;
    expect_eq(
        "transferred cleanup",
        cleanup_rows(&transferred[0]),
        vec![("a.example.test", Some("rec-a")), ("b.example.test", None)],
    )
}

/// A failed close retires the live instance but keeps its account charged.
fn failed_close_outlives_the_instance() -> Row {
    let probe = IntegrationAccountProbe::new();
    let instance = probe
        .admit(IntegrationKind::Nats)
        .map_err(refused("admission"))?;
    let id = instance.instance_id();
    instance.close_failed(failed_close(IntegrationKind::Nats));
    expect_eq("charged after failed close", probe.charged(), 1)?;
    expect_eq("retained after failed close", probe.retained(), 1)?;
    let transferred = probe.transfer();
    expect_eq(
        "transferred accounts",
        transferred.iter().map(account_row).collect::<Vec<_>>(),
        vec![(
            Some(id),
            IntegrationOperation::Close,
            IntegrationFailure::Timeout,
        )],
    )
}

/// Identities only rise, and a retired identity is never handed out again.
fn instance_identities_are_monotonic() -> Row {
    let probe = IntegrationAccountProbe::new();
    let mut previous: Option<u64> = None;
    for round in 0..3 {
        let batch: Vec<_> = [
            IntegrationKind::Nats,
            IntegrationKind::Sqs,
            IntegrationKind::Dns01,
        ]
        .into_iter()
        .map(|kind| probe.admit(kind))
        .collect::<Result<_, _>>()
        .map_err(|error| format!("round {round} admission refused: {error}"))?;
        previous = check_increasing_identities(
            batch.iter().map(|instance| instance.instance_id()),
            previous,
        )?;
        batch.into_iter().for_each(|instance| instance.close());
        expect_eq("charged after each round", probe.charged(), 0)?;
    }
    Ok(())
}

fn check_increasing_identities(
    ids: impl Iterator<Item = u64>,
    mut previous: Option<u64>,
) -> Result<Option<u64>, String> {
    for id in ids {
        if previous.is_some_and(|last| id <= last) {
            return Err(format!("identity {id} did not rise past {previous:?}"));
        }
        previous = Some(id);
    }
    Ok(previous)
}

/// The retained accounts leave once, with their identities; a second
/// transfer finds nothing.
fn retained_accounts_transfer_once() -> Row {
    let probe = IntegrationAccountProbe::new();
    let nats = probe
        .admit(IntegrationKind::Nats)
        .map_err(refused("admission"))?;
    let dns = probe
        .admit(IntegrationKind::Dns01)
        .map_err(refused("admission"))?;
    let (nats_id, dns_id) = (nats.instance_id(), dns.instance_id());
    nats.reserve(IntegrationOperation::Receive)
        .map_err(refused("reservation"))?
        .fail(ordinary(
            IntegrationKind::Nats,
            IntegrationOperation::Receive,
        ))
        .abandon();
    drop(
        dns.reserve(IntegrationOperation::Provision)
            .map_err(refused("reservation"))?
            .fail(failed_cleanup(
                IntegrationOperation::Provision,
                &[("c.example.test", Some("rec-c"))],
            ))
            .receive(),
    );
    nats.close_failed(failed_close(IntegrationKind::Nats));
    dns.close();
    let first = sorted_account_rows(probe.transfer().iter().map(account_row).collect());
    let expected = sorted_account_rows(vec![
        (
            Some(nats_id),
            IntegrationOperation::Receive,
            IntegrationFailure::Unavailable,
        ),
        (
            Some(nats_id),
            IntegrationOperation::Close,
            IntegrationFailure::Timeout,
        ),
        (
            Some(dns_id),
            IntegrationOperation::Provision,
            IntegrationFailure::CleanupIncomplete,
        ),
    ]);
    expect_eq("first transfer", first, expected)?;
    expect_eq("second transfer", probe.transfer().len(), 0)
}

// ── 3.T2 ──────────────────────────────────────────────────────────────

#[test]
fn saturated_accounts_allow_reserved_settlement_and_preserve_cleanup_ids() {
    run_rows(&[
        (
            "sequential settlement never accumulates",
            sequential_settlement_never_accumulates,
        ),
        (
            "history outlives instance retirement",
            history_outlives_instance_retirement,
        ),
        (
            "saturation still settles cleanup and close",
            saturation_still_settles_cleanup_and_close,
        ),
        (
            "later success keeps prior cleanup ids",
            later_success_keeps_prior_cleanup_ids,
        ),
        (
            "history exhaustion stays busy",
            history_exhaustion_stays_busy,
        ),
    ]);
}

/// Far more than 256 sequential attempts, each settled by success or a
/// received error, never leave more than the one live operation charged.
fn sequential_settlement_never_accumulates() -> Row {
    let probe = IntegrationAccountProbe::new();
    let instance = probe
        .admit(IntegrationKind::Sqs)
        .map_err(refused("admission"))?;
    for attempt in 0..(REPORT_BUDGET * 3) {
        let operation = instance
            .reserve(IntegrationOperation::Publish)
            .map_err(|error| format!("attempt {attempt} refused: {error}"))?;
        expect_eq("charged while one attempt is live", probe.charged(), 2)?;
        match attempt % 2 {
            0 => operation.succeed(),
            _ => drop(
                operation
                    .fail(ordinary(
                        IntegrationKind::Sqs,
                        IntegrationOperation::Publish,
                    ))
                    .receive(),
            ),
        }
        expect_eq("charged between attempts", probe.charged(), 1)?;
    }
    expect_eq("retained after the run", probe.retained(), 0)?;
    instance.close();
    expect_eq("transferred accounts", probe.transfer().len(), 0)
}

/// Retiring an instance frees its close reservation and nothing else: the
/// history its operations left stays charged across instance churn.
fn history_outlives_instance_retirement() -> Row {
    let probe = IntegrationAccountProbe::new();
    let mut retained_ids = Vec::new();
    for generation in 0..(REPORT_BUDGET / 2) {
        let instance = probe
            .admit(IntegrationKind::Nats)
            .map_err(|error| format!("generation {generation} admission refused: {error}"))?;
        retained_ids.push(instance.instance_id());
        instance
            .reserve(IntegrationOperation::Publish)
            .map_err(|error| format!("generation {generation} reservation refused: {error}"))?
            .fail(ordinary(
                IntegrationKind::Nats,
                IntegrationOperation::Publish,
            ))
            .abandon();
        instance.close();
        expect_eq("retained across churn", probe.retained(), generation + 1)?;
        expect_eq("charged across churn", probe.charged(), generation + 1)?;
    }
    let mut transferred: Vec<_> = probe
        .transfer()
        .iter()
        .filter_map(IntegrationError::instance_id)
        .collect();
    transferred.sort_unstable();
    expect_eq("transferred identities", transferred, retained_ids)
}

/// With the budget exhausted, accepted work still settles — the failed
/// cleanup and the close consume the reservations they already hold.
fn saturation_still_settles_cleanup_and_close() -> Row {
    let probe = IntegrationAccountProbe::new();
    let dns = probe
        .admit(IntegrationKind::Dns01)
        .map_err(refused("admission"))?;
    let order = dns
        .reserve(IntegrationOperation::Provision)
        .map_err(refused("order reservation"))?;
    let filler = probe
        .admit(IntegrationKind::Nats)
        .map_err(refused("filler admission"))?;
    let held: Vec<_> = (3..REPORT_BUDGET)
        .map(|_| filler.reserve(IntegrationOperation::Publish))
        .collect::<Result<_, _>>()
        .map_err(refused("early filling"))?;
    expect_eq("charged at saturation", probe.charged(), REPORT_BUDGET)?;
    let refused = dns
        .reserve(IntegrationOperation::Renew)
        .err()
        .ok_or("a new order was admitted at saturation")?;
    expect_busy("saturated renewal", &refused, IntegrationKind::Dns01)?;
    let received = order
        .fail(failed_cleanup(
            IntegrationOperation::Provision,
            &[("d.example.test", Some("rec-d"))],
        ))
        .receive();
    expect_eq(
        "settled order failure",
        received.failure(),
        IntegrationFailure::CleanupIncomplete,
    )?;
    expect_eq(
        "charged after cleanup settled",
        probe.charged(),
        REPORT_BUDGET,
    )?;
    dns.close_failed(failed_close(IntegrationKind::Dns01));
    expect_eq(
        "charged after close settled",
        probe.charged(),
        REPORT_BUDGET,
    )?;
    expect_eq("retained at saturation", probe.retained(), 2)?;
    held.into_iter().for_each(|operation| operation.succeed());
    filler.close();
    expect_eq("charged after filler settled", probe.charged(), 2)?;
    expect_eq("transferred accounts", probe.transfer().len(), 2)
}

/// A later successful renewal on the same owner cannot erase the record
/// identities an earlier order left unresolved.
fn later_success_keeps_prior_cleanup_ids() -> Row {
    let probe = IntegrationAccountProbe::new();
    let dns = probe
        .admit(IntegrationKind::Dns01)
        .map_err(refused("admission"))?;
    let id = dns.instance_id();
    drop(
        dns.reserve(IntegrationOperation::Provision)
            .map_err(refused("first order"))?
            .fail(failed_cleanup(
                IntegrationOperation::Provision,
                &[("e.example.test", Some("rec-e")), ("f.example.test", None)],
            ))
            .receive(),
    );
    for renewal in 0..3 {
        dns.reserve(IntegrationOperation::Renew)
            .map_err(|error| format!("renewal {renewal} refused: {error}"))?
            .succeed();
        expect_eq("retained after renewal", probe.retained(), 1)?;
    }
    dns.close();
    let transferred = probe.transfer();
    expect_eq(
        "transferred accounts",
        transferred.iter().map(account_row).collect::<Vec<_>>(),
        vec![(
            Some(id),
            IntegrationOperation::Provision,
            IntegrationFailure::CleanupIncomplete,
        )],
    )?;
    expect_eq(
        "transferred cleanup",
        cleanup_rows(&transferred[0]),
        vec![("e.example.test", Some("rec-e")), ("f.example.test", None)],
    )?;
    expect_eq("second transfer", probe.transfer().len(), 0)
}

/// History is never evicted: once it fills the budget, new work stays Busy,
/// and the refusals add no history of their own.
fn history_exhaustion_stays_busy() -> Row {
    let probe = IntegrationAccountProbe::new();
    for attempt in 0..REPORT_BUDGET {
        probe
            .admit(IntegrationKind::Sqs)
            .map_err(|error| format!("attempt {attempt} admission refused: {error}"))?
            .close_failed(failed_close(IntegrationKind::Sqs));
    }
    expect_eq("retained at exhaustion", probe.retained(), REPORT_BUDGET)?;
    for attempt in 0..(REPORT_BUDGET / 4) {
        let refused = probe
            .admit(IntegrationKind::Sqs)
            .err()
            .ok_or_else(|| format!("admission {attempt} succeeded past exhausted history"))?;
        expect_busy("exhausted admission", &refused, IntegrationKind::Sqs)?;
    }
    expect_eq("charged after refusals", probe.charged(), REPORT_BUDGET)?;
    let transferred = probe.transfer();
    expect_eq("transferred accounts", transferred.len(), REPORT_BUDGET)?;
    expect_eq(
        "transferred busy accounts",
        transferred
            .iter()
            .filter(|error| error.failure() == IntegrationFailure::Busy)
            .count(),
        0,
    )
}
