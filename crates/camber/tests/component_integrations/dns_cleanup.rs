//! 11.T1: DNS cleanup keeps every create intention and fails exactly the
//! records it could not delete.
//!
//! Every row enters public `AcmeDns01::provision_cert` under its own runtime.
//! A local ACME peer carries the order through issuance, and a
//! Cloudflare-shaped peer commits each TXT record before it answers, so the
//! peer's store is the oracle: each committed record is either deleted by its
//! exact ID or named in the order's cleanup account, the account names nothing
//! else, and an unrelated record under the same challenge name survives. A
//! row with an unresolved record expects `CleanupIncomplete` instead of a
//! published certificate.
#![cfg(feature = "dns01")]

use crate::dns_cleanup_peers::{
    Create, DIAGNOSTIC, DNS_ROW_BOUND, Fixture, Named, Script, Scripts, Stage, TWO_ZONES,
    chain_leaf, dns_accounts, expect_delivered, expect_exact_deletes, expect_no_values,
    expect_retained, expected_unresolved, integration, prior_generation, run_observing,
};
use crate::integration_rows::{
    Refusal, Row, all, bounded, expect, expect_eq, observed_verdict, refusal, rejected,
    run_rows_under,
};
use camber::dns01::{AcmeDns01, CloudflareProvider, DnsProvider, RecordId};
use camber::{
    CleanupItem, IntegrationError, IntegrationFailure, IntegrationOperation, RuntimeError, runtime,
};
use rustls::sign::CertifiedKey;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

/// An order bound a held request outlives.
const SHORT_ORDER: Duration = Duration::from_secs(3);

/// The most domains one order admits, and so the most records one cleanup
/// account names.
const MOST_DOMAINS: usize = 100;

/// What one provisioning left behind.
struct Observed {
    /// What the waiter received, or the hang guard's verdict.
    caller: Result<Result<CertifiedKey, RuntimeError>, String>,
    /// The runtime's teardown: success, or the aggregate it returned.
    teardown: Result<(), RuntimeError>,
}

impl Observed {
    /// The error the caller received, if any.
    fn error(&self) -> Option<&RuntimeError> {
        match &self.caller {
            Ok(Err(error)) => Some(error),
            _ => None,
        }
    }

    /// The caller's integration failure, when the caller received one.
    fn delivered(&self) -> Option<&IntegrationError> {
        self.error().and_then(integration)
    }

    /// The caller's typed refusal, when the caller received one.
    fn refused(&self) -> Option<Refusal> {
        self.error().and_then(refusal)
    }

    /// The failure the caller's account records for `domain`.
    fn item_failure(&self, domain: &str) -> Option<IntegrationFailure> {
        self.delivered()
            .and_then(|error| error.cleanup().iter().find(|item| item.domain() == domain))
            .map(CleanupItem::failure)
    }
}

/// Provision `provider` under `acme` on a fresh runtime and wait for it.
fn provision<P: DnsProvider + 'static>(acme: &AcmeDns01, provider: P) -> Observed {
    let (caller, teardown) = run_observing(runtime::builder(), || {
        bounded(
            "provision_cert",
            DNS_ROW_BOUND,
            acme.provision_cert(provider),
        )
    });
    Observed {
        caller: observed_verdict(caller),
        teardown,
    }
}

/// The checks every row shares: exact deletes, the unrelated record kept, the
/// same account delivered and retained, no challenge value repeated, and no
/// publication when a record is unresolved.
fn expect_account(
    fixture: &Fixture,
    observed: &Observed,
    unseen: &[Box<str>],
    expected: &Named,
) -> Row {
    let log = fixture.peers.cloudflare.log();
    let accounts = dns_accounts(&observed.teardown)?;
    let mut checks = vec![
        expect_eq(
            "the unresolved records the peer's store implies",
            &expected_unresolved(&log, unseen),
            expected,
        ),
        expect_exact_deletes(&log, unseen),
        expect_retained(&accounts, expected),
    ];
    checks.extend(
        accounts
            .iter()
            .map(|account| expect_no_values(account, &log)),
    );
    if let Some(error) = observed.delivered() {
        checks.push(expect_delivered(error, expected));
        checks.push(expect_no_values(error, &log));
    }
    if !expected.is_empty() {
        checks.push(expect(
            "a certificate was served with records unresolved",
            !matches!(observed.caller, Ok(Ok(_))),
        ));
    }
    all(checks)
}

/// The `n`th create's record, as an account names a record whose ID the
/// order never learned: by its domain alone.
fn unknown(fixture: &Fixture, n: usize) -> Result<(String, Option<String>), String> {
    fixture
        .peers
        .cloudflare
        .log()
        .creates
        .get(n - 1)
        .map(|create| (create.domain(), None))
        .ok_or_else(|| format!("create {n} never reached the peer"))
}

// --- rows ------------------------------------------------------------------

/// An issued order deletes both records by ID before it publishes, and
/// serves the leaf the directory issued.
fn issued_order_deletes_before_publication() -> Row {
    let fixture = Fixture::start(Scripts::default())?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let observed = provision(&acme, fixture.peers.provider()?);
    let log = fixture.peers.cloudflare.log();
    let issued = fixture.peers.acme.log().issued;
    let served = match &observed.caller {
        Ok(Ok(key)) => chain_leaf(&key.cert),
        _ => None,
    };
    let checks = all([
        expect_eq("the served leaf", served, issued.clone()),
        expect(
            &format!(
                "the caller's answer: {:?}",
                observed.caller.as_ref().map(Result::is_ok)
            ),
            matches!(observed.caller, Ok(Ok(_))),
        ),
        expect_eq("records acknowledged", log.acknowledged().len(), 2),
        expect(
            "a delete arrived after the certificate was published",
            log.deletes.iter().all(|delete| !delete.published()),
        ),
        expect(
            "the issued certificate was published",
            fixture.bundle().is_some(),
        ),
        expect_account(&fixture, &observed, &[], &Vec::new()),
    ]);
    all([checks, fixture.finish()])
}

/// A directory that refuses the second challenge ends the order as the
/// caller's refusal, and both acknowledged records are deleted.
fn acme_refusal_deletes_every_record() -> Row {
    let fixture = Fixture::start(Scripts {
        challenge: Script::answer().nth(2, Stage::Refuse),
        ..Scripts::default()
    })?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let observed = provision(&acme, fixture.peers.provider()?);
    let checks = all([
        expect_eq(
            "the refused order",
            observed.refused(),
            Some(rejected(IntegrationOperation::Provision)),
        ),
        expect_eq(
            "records acknowledged",
            fixture.peers.cloudflare.log().acknowledged().len(),
            2,
        ),
        expect("a refused order published", fixture.bundle().is_none()),
        expect_account(&fixture, &observed, &[], &Vec::new()),
    ]);
    all([checks, fixture.finish()])
}

/// An order whose finalize outlives it still deletes both records: cleanup
/// has its own bound once the order's result is fixed.
fn expired_order_still_deletes_its_records() -> Row {
    let fixture = Fixture::start(Scripts {
        finalize: Stage::Hold,
        ..Scripts::default()
    })?;
    let acme = fixture
        .configuration(&TWO_ZONES)?
        .operation_timeout(SHORT_ORDER);
    let observed = provision(&acme, fixture.peers.provider()?);
    let checks = all([
        expect_eq(
            "the expired order",
            observed.refused(),
            Some((
                IntegrationOperation::Provision,
                camber::IntegrationFailure::Timeout,
                camber::Retryability::OutcomeUnknown,
            )),
        ),
        expect_eq(
            "records acknowledged",
            fixture.peers.cloudflare.log().acknowledged().len(),
            2,
        ),
        expect("an expired order published", fixture.bundle().is_none()),
        expect_account(&fixture, &observed, &[], &Vec::new()),
    ]);
    all([checks, fixture.finish()])
}

/// A create whose answer is lost after the peer committed it is an unknown
/// record: named by its domain with no ID, never deleted by name, and the
/// acknowledged record is still deleted by ID.
fn lost_create_answer_is_an_unknown_record() -> Row {
    let fixture = Fixture::start(Scripts {
        create: Script::answer().nth(2, Stage::Lose),
        ..Scripts::default()
    })?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let observed = provision(&acme, fixture.peers.provider()?);
    let expected = vec![unknown(&fixture, 2)?];
    let domain = expected[0].0.clone();
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        expect_eq(
            "the lost create's cleanup failure",
            observed.item_failure(&domain),
            Some(IntegrationFailure::OutcomeUnknown),
        ),
        expect(
            "the lost create's committed record was deleted",
            log.created_id(2)
                .is_some_and(|id| log.records.contains_key(&id)),
        ),
        expect(
            "an order with an unknown record published",
            fixture.bundle().is_none(),
        ),
        expect_account(&fixture, &observed, &[], &expected),
    ]);
    all([checks, fixture.finish()])
}

/// A create still unanswered when the order's bound passes is an unknown
/// record, named by its domain with no ID.
fn create_outliving_the_order_is_an_unknown_record() -> Row {
    let fixture = Fixture::start(Scripts {
        create: Script::answer().nth(2, Stage::Hold),
        ..Scripts::default()
    })?;
    let acme = fixture
        .configuration(&TWO_ZONES)?
        .operation_timeout(SHORT_ORDER);
    let observed = provision(&acme, fixture.peers.provider()?);
    let expected = vec![unknown(&fixture, 2)?];
    let domain = expected[0].0.clone();
    let checks = all([
        expect(
            &format!(
                "the interrupted create's cleanup failure: {:?}",
                observed.item_failure(&domain)
            ),
            matches!(
                observed.item_failure(&domain),
                Some(IntegrationFailure::Timeout | IntegrationFailure::OutcomeUnknown)
            ),
        ),
        expect(
            "an order with an unknown record published",
            fixture.bundle().is_none(),
        ),
        expect_account(&fixture, &observed, &[], &expected),
    ]);
    all([checks, fixture.finish()])
}

/// A refused delete after a successful issuance is `CleanupIncomplete` naming
/// that exact ID. Nothing is published: the prior valid generation stays.
fn failed_delete_publishes_nothing() -> Row {
    let fixture = Fixture::start(Scripts {
        delete: Script::answer().nth(2, Stage::Refuse),
        ..Scripts::default()
    })?;
    let prior = prior_generation(&TWO_ZONES)?;
    fixture.seed(&prior)?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let observed = provision(&acme, fixture.peers.provider()?);
    let log = fixture.peers.cloudflare.log();
    let refused = log
        .deletes
        .get(1)
        .map(|delete| delete.id.to_string())
        .ok_or("the second delete never reached the peer")?;
    let domain = log
        .creates
        .iter()
        .find(|create| create.id.as_deref() == Some(&*refused))
        .map(Create::domain)
        .ok_or("the refused delete named no record the order created")?;
    let expected = vec![(domain.clone(), Some(refused))];
    let checks = all([
        expect(
            "the directory issued the certificate",
            fixture.peers.acme.log().issued.is_some(),
        ),
        expect_eq(
            "the refused delete's cleanup failure",
            observed.item_failure(&domain),
            Some(IntegrationFailure::PermissionDenied),
        ),
        expect_eq(
            "the prior generation",
            fixture.bundle(),
            Some(prior.into_bytes()),
        ),
        expect_account(&fixture, &observed, &[], &expected),
    ]);
    all([checks, fixture.finish()])
}

/// A provider that panics once its second create was acknowledged: the order
/// never learns that record's ID.
struct PanicsAfterSecondCreate {
    inner: CloudflareProvider,
    creates: AtomicUsize,
}

impl DnsProvider for PanicsAfterSecondCreate {
    fn prepare(
        &mut self,
        domains: &[Arc<str>],
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        self.inner.prepare(domains)
    }

    async fn create_txt_record(&self, fqdn: &str, value: &str) -> Result<RecordId, RuntimeError> {
        let created = self.inner.create_txt_record(fqdn, value).await;
        if self.creates.fetch_add(1, Ordering::SeqCst) == 1 {
            panic!("the provider panicked after the peer acknowledged its create");
        }
        created
    }

    fn delete_txt_record(
        &self,
        record_id: &str,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        self.inner.delete_txt_record(record_id)
    }
}

/// A provider panic mid-order leaves the record it swallowed named by domain,
/// and the record the order learned deleted or named by ID.
fn provider_panic_keeps_every_record_account() -> Row {
    let fixture = Fixture::start(Scripts::default())?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = PanicsAfterSecondCreate {
        inner: fixture.peers.provider()?,
        creates: AtomicUsize::new(0),
    };
    let observed = provision(&acme, provider);
    let log = fixture.peers.cloudflare.log();
    let swallowed: Box<[Box<str>]> = log.created_id(2).into_iter().collect();
    let expected = expected_unresolved(&log, &swallowed);
    let checks = all([
        expect(
            &format!(
                "the panicked order answered {:?}",
                observed.caller.as_ref().map(Result::is_ok)
            ),
            matches!(observed.caller, Ok(Err(_))),
        ),
        expect(
            "the swallowed record is named by its domain",
            expected.contains(&unknown(&fixture, 2)?),
        ),
        expect("a panicked order published", fixture.bundle().is_none()),
        expect_account(&fixture, &observed, &swallowed, &expected),
    ]);
    all([checks, fixture.finish()])
}

/// Dropping the waiter while the second create is held names that create by
/// its domain; the acknowledged record is deleted by ID.
fn cancelled_order_names_the_interrupted_create() -> Row {
    let fixture = Fixture::start(Scripts {
        create: Script::answer().nth(2, Stage::Hold),
        ..Scripts::default()
    })?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = fixture.peers.provider()?;
    let cloudflare = fixture.peers.cloudflare.clone();
    let (reached, teardown) = run_observing(runtime::builder(), move || {
        let waiter = camber::spawn_async(async move { acme.provision_cert(provider).await });
        let reached = bounded(
            "the second create to reach the peer",
            DNS_ROW_BOUND,
            cloudflare.until(|log| log.creates.len() >= 2),
        );
        waiter.cancel();
        drop(waiter);
        reached
    });
    let observed = Observed {
        caller: Err("the waiter was dropped".to_owned()),
        teardown,
    };
    let expected = vec![unknown(&fixture, 2)?];
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        observed_verdict(reached),
        expect_eq(
            "the acknowledged record was deleted",
            log.created_id(1).map(|id| log.deleted(&id)),
            Some(true),
        ),
        expect_account(&fixture, &observed, &[], &expected),
    ]);
    all([checks, fixture.finish()])
}

/// An order of the most domains whose every delete is refused names every
/// record once, by its exact ID.
fn full_order_names_every_record() -> Row {
    let names: Box<[String]> = (0..MOST_DOMAINS)
        .map(|index| format!("d{index:02}.example.com"))
        .collect();
    let domains: Box<[&str]> = names.iter().map(String::as_str).collect();
    let fixture = Fixture::start(Scripts {
        delete: Script::every(Stage::Refuse),
        ..Scripts::default()
    })?;
    let acme = fixture.configuration(&domains)?;
    let observed = provision(&acme, fixture.peers.provider()?);
    let log = fixture.peers.cloudflare.log();
    let mut expected: Named = log
        .acknowledged()
        .iter()
        .map(|create| (create.domain(), create.id.as_deref().map(str::to_owned)))
        .collect();
    expected.sort();
    let checks = all([
        expect_eq("records acknowledged", expected.len(), MOST_DOMAINS),
        expect_eq(
            "records the caller was handed",
            observed.delivered().map(|error| error.cleanup().len()),
            Some(MOST_DOMAINS),
        ),
        expect(
            "an order with unresolved records published",
            fixture.bundle().is_none(),
        ),
        expect_account(&fixture, &observed, &[], &expected),
    ]);
    all([checks, fixture.finish()])
}

#[test]
fn dns_cleanup_preserves_unknown_creates_and_exact_delete_failures() {
    run_rows_under(
        DIAGNOSTIC,
        &[
            (
                "an issued order deletes every record before publication",
                issued_order_deletes_before_publication,
            ),
            (
                "an ACME refusal deletes every acknowledged record",
                acme_refusal_deletes_every_record,
            ),
            (
                "an expired order still deletes its records",
                expired_order_still_deletes_its_records,
            ),
            (
                "a lost create answer is an unknown record",
                lost_create_answer_is_an_unknown_record,
            ),
            (
                "a create outliving the order is an unknown record",
                create_outliving_the_order_is_an_unknown_record,
            ),
            (
                "a failed delete publishes nothing",
                failed_delete_publishes_nothing,
            ),
            (
                "a provider panic keeps every record account",
                provider_panic_keeps_every_record_account,
            ),
            (
                "a cancelled order names the interrupted create",
                cancelled_order_names_the_interrupted_create,
            ),
            (
                "a full order names every record",
                full_order_names_every_record,
            ),
        ],
    );
}
