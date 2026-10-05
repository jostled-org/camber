//! 12.T1 and 12.T3: failed renewal cleanup stays charged across later
//! success, report saturation, and instance retirement.
//!
//! Rows enter public `spawn_renewal`, `RuntimeBuilder::tls_auto_dns01`
//! startup, and direct `provision_cert`. The runtime carries a test schedule,
//! so each renewal interval elapses when the row lets it. An owner begins its
//! next wait only after its order and that order's cleanup settled, and a
//! cancelled renewal answers only after its owner retired, so every count a
//! row reads follows an acknowledgement, never elapsed time.
//!
//! The oracle is the runtime's returned aggregate: every record a failed
//! cleanup left is named there once, under the owner that failed it, after
//! that owner retired.
#![cfg(feature = "dns01")]

use crate::dns_cleanup_peers::{
    CfLog, DNS_ROW_BOUND, Fixture, Named, Script, Scripts, Stage, TWO_ZONES, await_renewal_waits,
    dns_accounts, elapse_renewal, expect_exact_deletes, expected_unresolved, generation_expiring,
    headroom, hold_budget, is_busy, leaf_of, named, prior_generation, release_budget, renewal_pass,
    retire_renewal, run_observing, served_leaf, served_store,
};
use crate::integration_rows::{
    REPORT_BUDGET, Row, all, bounded, expect, expect_eq, observed_verdict, run_rows, tempdir,
};
use camber::runtime_test_support::{
    IntegrationLifecycleProbe, RuntimeController, runtime_schedule,
};
use camber::tls::CertStore;
use camber::{AsyncJoinHandle, IntegrationFailure, IntegrationKind, RuntimeError, runtime};
use std::path::Path;

/// One domain: each order raises one record.
const ONE: [&str; 1] = ["app.example.com"];

/// Days left on a cached leaf that is due for renewal.
const DUE_DAYS: u32 = 20;

/// The free accounts the churn row leaves outside its held budget.
const CHURN_SPARE: usize = 4;

/// The owners whose cleanup fails before the churn row's budget is full.
const CHURN_FAILURES: usize = CHURN_SPARE - 1;

/// The renewal handle a row cancels to retire its owner.
type Renewal = AsyncJoinHandle<Result<(), RuntimeError>>;

/// Every headroom reading against what it should be.
fn expect_headroom(readings: &[(&str, usize, usize)]) -> Row {
    all(readings
        .iter()
        .map(|(when, actual, expected)| expect_eq(&format!("headroom {when}"), *actual, *expected)))
}

/// The provider and directory requests both peers have seen.
fn effects(fixture: &Fixture) -> (usize, usize) {
    (
        fixture.peers.cloudflare.log().requests.len(),
        fixture.peers.acme.log().challenges,
    )
}

/// Run renewal pass `waits` against a full report budget. The refused pass
/// must send no provider or directory request.
fn refused_pass(controller: &RuntimeController, fixture: &Fixture, waits: usize) -> Row {
    let before = effects(fixture);
    let budget = hold_budget(0)?;
    renewal_pass(controller, waits, DNS_ROW_BOUND)?;
    let during = effects(fixture);
    release_budget(budget)?;
    expect_eq(
        "provider and directory requests of the refused pass",
        during,
        before,
    )
}

/// Admit and close `count` controlled instances, one after another.
fn churn(count: usize) -> Row {
    for nth in 0..count {
        let probe = IntegrationLifecycleProbe::admit(IntegrationKind::Nats)
            .map_err(|error| format!("churn instance {nth} was refused: {error:?}"))?;
        bounded("a churn instance's close", DNS_ROW_BOUND, probe.close())?
            .map_err(|error| format!("churn instance {nth}'s close failed: {error:?}"))?;
    }
    Ok(())
}

/// Each retained DNS-01 cleanup account's records, in the aggregate's order.
fn retained_records(teardown: &Result<(), RuntimeError>) -> Result<Vec<Named>, String> {
    Ok(dns_accounts(teardown)?
        .iter()
        .filter(|account| account.failure() == IntegrationFailure::CleanupIncomplete)
        .map(|account| named(account))
        .collect())
}

/// The DNS-01 accounts in the aggregate are in ascending integration
/// identity, each once.
fn expect_identity_order(teardown: &Result<(), RuntimeError>) -> Row {
    let Err(RuntimeError::Lifecycle(failures)) = teardown else {
        return Err("the runtime returned no lifecycle aggregate".to_owned());
    };
    let ids: Vec<u64> = failures
        .iter()
        .filter_map(|failure| match *failure.participant() {
            camber::LifecycleParticipant::Integration {
                kind: IntegrationKind::Dns01,
                id,
            } => Some(id),
            _ => None,
        })
        .collect();
    let mut ordered = ids.clone();
    ordered.sort_unstable();
    ordered.dedup();
    expect_eq("DNS-01 aggregate identities", ids, ordered)
}

/// The aggregate holds one cleanup account: the `records` records the
/// failed order in `failed` left, and no later order added to them.
fn expect_one_failed_account(
    failed: Option<CfLog>,
    records: usize,
    fixture: &Fixture,
    teardown: &Result<(), RuntimeError>,
) -> Row {
    let failed = failed.ok_or("the failed order was never observed")?;
    let expected = expected_unresolved(&failed, &[]);
    let log = fixture.peers.cloudflare.log();
    all([
        expect_eq("records the failed order left", expected.len(), records),
        expect_eq(
            "records every order left",
            expected_unresolved(&log, &[]),
            expected.clone(),
        ),
        expect_eq(
            "retained cleanup accounts",
            retained_records(teardown)?,
            vec![expected],
        ),
        expect_exact_deletes(&log, &[]),
    ])
}

// --- 12.T1 -------------------------------------------------------------------

/// A public renewal's failed cleanup survives a refused pass, a later
/// successful order, the owner's retirement, and instance churn past the
/// report capacity, and the aggregate names it once.
fn public_renewal_keeps_failed_history() -> Row {
    let fixture = Fixture::start(Scripts {
        delete: Script::answer().nth(1, Stage::Refuse).nth(2, Stage::Refuse),
        ..Scripts::default()
    })?;
    fixture.seed(&generation_expiring(&TWO_ZONES, DUE_DAYS)?)?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let store = served_store(&acme)?;
    let observing = store.clone();
    let prior_leaf = served_leaf(&observing);
    let prior_bundle = fixture.bundle();
    let provider = fixture.peers.provider()?;
    let controller = runtime_schedule();
    let mut failed = None;
    let (observed, teardown) = run_observing(
        runtime::builder().with_test_schedule(&controller),
        || -> Row {
            let base = headroom()?;
            let handle = acme.spawn_renewal(provider, store);
            renewal_pass(&controller, 1, DNS_ROW_BOUND)?;
            failed = Some(fixture.peers.cloudflare.log());
            let preserved = all([
                expect_eq(
                    "the live leaf after failed cleanup",
                    served_leaf(&observing),
                    prior_leaf,
                ),
                expect_eq(
                    "the cache after failed cleanup",
                    fixture.bundle(),
                    prior_bundle,
                ),
            ]);
            let after_failure = headroom()?;
            let refused = refused_pass(&controller, &fixture, 2);
            let after_refusal = headroom()?;
            renewal_pass(&controller, 3, DNS_ROW_BOUND)?;
            let after_success = headroom()?;
            let swapped = expect_eq(
                "the leaf the store serves after the successful order",
                served_leaf(&observing),
                fixture.bundle().and_then(|bundle| leaf_of(&bundle)),
            );
            let retired = retire_renewal(handle, DNS_ROW_BOUND);
            let after_retirement = headroom()?;
            let churned = churn(2 * REPORT_BUDGET);
            all([
                preserved,
                refused,
                swapped,
                retired,
                churned,
                expect_headroom(&[
                    ("after the failed cleanup", after_failure, base - 2),
                    ("after the refused pass", after_refusal, after_failure),
                    ("after the successful order", after_success, after_failure),
                    ("after retirement", after_retirement, base - 1),
                    ("after churn", headroom()?, base - 1),
                ]),
            ])
        },
    );
    let checks = all([
        observed_verdict(observed),
        expect_one_failed_account(failed, 2, &fixture, &teardown),
    ]);
    all([checks, fixture.finish()])
}

/// The runtime-managed owner's failed cleanup survives a refused pass and a
/// later successful order; the stop's aggregate names it once.
fn runtime_renewal_keeps_failed_history() -> Row {
    let fixture = Fixture::start(Scripts {
        delete: Script::answer().nth(1, Stage::Refuse),
        ..Scripts::default()
    })?;
    fixture.seed(&prior_generation(&TWO_ZONES)?)?;
    let controller = runtime_schedule();
    let builder = runtime::builder()
        .with_test_schedule(&controller)
        .tls_auto_dns01(fixture.configuration(&TWO_ZONES)?, "token".into())
        .with_test_dns_transport(&fixture.peers.cloudflare.uri());
    let mut failed = None;
    let (observed, teardown) = run_observing(builder, || -> Row {
        await_renewal_waits(&controller, 1, DNS_ROW_BOUND)?;
        let base = headroom()?;
        let due = generation_expiring(&TWO_ZONES, DUE_DAYS)?;
        fixture.seed(&due)?;
        renewal_pass(&controller, 1, DNS_ROW_BOUND)?;
        failed = Some(fixture.peers.cloudflare.log());
        let kept = fixture.bundle();
        let after_failure = headroom()?;
        let refused = refused_pass(&controller, &fixture, 2);
        let after_refusal = headroom()?;
        renewal_pass(&controller, 3, DNS_ROW_BOUND)?;
        all([
            expect_eq(
                "the cached generation after the failed order",
                kept,
                Some(due.into_bytes()),
            ),
            refused,
            expect_eq(
                "the cached leaf after the successful order",
                fixture.bundle().and_then(|bundle| leaf_of(&bundle)),
                fixture.peers.acme.log().issued,
            ),
            expect_headroom(&[
                ("after the failed cleanup", after_failure, base - 1),
                ("after the refused pass", after_refusal, after_failure),
                ("after the successful order", headroom()?, after_failure),
            ]),
        ])
    });
    let checks = all([
        observed_verdict(observed),
        expect_one_failed_account(failed, 1, &fixture, &teardown),
    ]);
    all([checks, fixture.finish()])
}

#[test]
fn renewal_keeps_failed_history_across_success_and_instance_retirement() {
    run_rows(&[
        (
            "a public renewal keeps its failed history",
            public_renewal_keeps_failed_history,
        ),
        (
            "the runtime renewal keeps its failed history",
            runtime_renewal_keeps_failed_history,
        ),
    ]);
}

// --- 12.T3 -------------------------------------------------------------------

/// A store no row reads, holding the cache's current generation.
fn placeholder_store(fixture: &Fixture) -> Result<CertStore, String> {
    served_store(&fixture.configuration(&ONE)?)
}

/// Admit a public renewal of the row's cache and let its first interval
/// elapse; `waits` intervals were begun before it.
fn admit_owner(
    controller: &RuntimeController,
    fixture: &Fixture,
    waits: usize,
) -> Result<Renewal, String> {
    let handle = fixture
        .configuration(&ONE)?
        .spawn_renewal(fixture.peers.provider()?, placeholder_store(fixture)?);
    elapse_renewal(controller, waits + 1, DNS_ROW_BOUND)?;
    Ok(handle)
}

/// Fail and retire [`CHURN_FAILURES`] owners, one after another. Each
/// retirement leaves exactly its failed cleanup charged. Answers the checks
/// and the intervals begun.
fn fail_and_retire_owners(
    controller: &RuntimeController,
    fixture: &Fixture,
) -> Result<(Vec<Row>, usize), String> {
    let mut checks = Vec::new();
    let mut waits = 0;
    for nth in 0..CHURN_FAILURES {
        let handle = admit_owner(controller, fixture, waits)?;
        await_renewal_waits(controller, waits + 2, DNS_ROW_BOUND)?;
        waits += 2;
        checks.push(retire_renewal(handle, DNS_ROW_BOUND));
        checks.push(expect_eq(
            &format!("headroom after failed owner {nth} retired"),
            headroom()?,
            CHURN_SPARE - (nth + 1),
        ));
    }
    Ok((checks, waits))
}

/// With one account left, an owner is admitted and takes it, and its order is
/// refused before it prepares. A new owner of another cache and a direct
/// order are then refused at admission. None of them sends a request.
fn refuse_at_saturation(
    controller: &RuntimeController,
    fixture: &Fixture,
    other: &Path,
    waits: usize,
) -> Result<(Row, usize), String> {
    let before = effects(fixture);
    let last = admit_owner(controller, fixture, waits)?;
    await_renewal_waits(controller, waits + 2, DNS_ROW_BOUND)?;
    let refused_order = effects(fixture);
    let refused_owner = bounded(
        "a new owner's refusal",
        DNS_ROW_BOUND,
        fixture
            .peers
            .configuration(other, &ONE)?
            .spawn_renewal(fixture.peers.provider()?, placeholder_store(fixture)?)
            .into_future(),
    )?;
    let refused_direct = bounded(
        "a direct order's refusal",
        DNS_ROW_BOUND,
        fixture
            .peers
            .configuration(other, &ONE)?
            .provision_cert(fixture.peers.provider()?),
    )?;
    let refused_owners = effects(fixture);
    let checks = all([
        expect_eq("requests of the refused order", refused_order, before),
        expect(
            "a new owner of a full budget was not refused as Busy",
            refused_owner.as_ref().is_err_and(is_busy),
        ),
        expect(
            "a direct order of a full budget was not refused as Busy",
            refused_direct.as_ref().is_err_and(is_busy),
        ),
        expect_eq("requests of the refused owners", refused_owners, before),
        retire_renewal(last, DNS_ROW_BOUND),
    ]);
    Ok((checks, waits + 2))
}

/// A control's order is admitted, then the budget saturates while its delete
/// is held; the delete still completes, and the control gives back
/// everything it took.
fn control_spares_reserved_cleanup(
    controller: &RuntimeController,
    fixture: &Fixture,
    waits: usize,
) -> Row {
    let before = headroom()?;
    let deletes = fixture.peers.cloudflare.log().deletes.len();
    let control = admit_owner(controller, fixture, waits)?;
    bounded(
        "the control's delete to be held",
        DNS_ROW_BOUND,
        fixture
            .peers
            .cloudflare
            .until(move |log| log.deletes.len() > deletes),
    )?;
    let saturated = hold_budget(0)?;
    fixture.peers.cloudflare.release();
    await_renewal_waits(controller, waits + 2, DNS_ROW_BOUND)?;
    release_budget(saturated)?;
    let during = headroom()?;
    let retired = retire_renewal(control, DNS_ROW_BOUND);
    all([
        retired,
        expect_headroom(&[
            ("while the control is live", during, before - 1),
            ("after the control retired", headroom()?, before),
        ]),
    ])
}

/// The aggregate names each failed owner's record once, in identity order,
/// and the control's record was deleted.
fn expect_churn_aggregate(log: &CfLog, teardown: &Result<(), RuntimeError>) -> Row {
    let mut expected: Vec<Named> = log
        .creates
        .iter()
        .take(CHURN_FAILURES)
        .map(|create| vec![(create.domain(), create.id.as_deref().map(str::to_owned))])
        .collect();
    expected.sort();
    let mut retained = retained_records(teardown)?;
    retained.sort();
    all([
        expect_eq("records created", log.creates.len(), CHURN_FAILURES + 1),
        expect(
            "the control's record outlived its order",
            log.creates
                .get(CHURN_FAILURES)
                .and_then(|create| create.id.as_deref())
                .is_some_and(|id| log.deleted(id)),
        ),
        expect_eq("retained cleanup identities", retained, expected),
        expect_eq(
            "retained DNS-01 accounts",
            dns_accounts(teardown)?.len(),
            CHURN_FAILURES,
        ),
        expect_identity_order(teardown),
        expect_exact_deletes(log, &[]),
    ])
}

/// Failed owners retire one after another until the report budget refuses a
/// new owner before it prepares. Every cleanup identity survives in the
/// aggregate once; a successful control releases what it took; and an order
/// admitted before saturation still deletes its record.
fn failed_owner_churn_preserves_every_identity() -> Row {
    let fixture = Fixture::start(Scripts {
        delete: (1..=CHURN_FAILURES)
            .fold(Script::answer(), |script, nth| {
                script.nth(nth, Stage::Refuse)
            })
            .nth(CHURN_FAILURES + 1, Stage::Hold),
        ..Scripts::default()
    })?;
    fixture.seed(&generation_expiring(&ONE, DUE_DAYS)?)?;
    let controller = runtime_schedule();
    let other = tempdir()?;
    let (observed, teardown) = run_observing(
        runtime::builder().with_test_schedule(&controller),
        || -> Row {
            let budget = hold_budget(CHURN_SPARE)?;
            let (mut checks, waits) = fail_and_retire_owners(&controller, &fixture)?;
            let (refusals, waits) =
                refuse_at_saturation(&controller, &fixture, other.path(), waits)?;
            checks.push(refusals);
            checks.push(release_budget(budget));
            checks.push(control_spares_reserved_cleanup(
                &controller,
                &fixture,
                waits,
            ));
            all(checks)
        },
    );
    all([
        observed_verdict(observed),
        expect_churn_aggregate(&fixture.peers.cloudflare.log(), &teardown),
        other
            .close()
            .map_err(|error| format!("remove the other cache: {error}")),
        fixture.finish(),
    ])
}

#[test]
fn dns_failed_instance_churn_preserves_every_cleanup_identity() {
    run_rows(&[(
        "failed owner churn preserves every identity",
        failed_owner_churn_preserves_every_identity,
    )]);
}
