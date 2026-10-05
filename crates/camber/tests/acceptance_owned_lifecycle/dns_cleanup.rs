//! 11.T2: a dropped waiter leaves the admitted DNS owner holding its provider
//! until every acknowledged record is deleted or named.
//!
//! Each row drops the public provision waiter, and the original `AcmeDns01`
//! with it, once the Cloudflare-shaped peer acknowledged a TXT create and the
//! ACME peer holds that record's challenge. No row keeps a provider clone: a
//! witness inside the owned provider records the peer's store at the moment
//! the owner drops it. Rows then drive graceful and forced runtime stops and
//! a saturated report history, and read the runtime's aggregate for the one
//! account each unresolved record belongs to.
#![cfg(feature = "dns01")]

use crate::dns_cleanup_peers::{
    AcmePeer, CfLog, CloudflarePeer, Create, DIAGNOSTIC, DNS_ROW_BOUND, DeleteAnswer, Fixture,
    Named, Script, Scripts, Stage, TWO_ZONES, aggregate_accounts, dns_accounts, expect_delivered,
    expect_exact_deletes, expect_retained, headroom, integration, is_busy, mark_served, named,
    run_observing, signal,
};
use crate::integration_registry::charged_failure;
use crate::integration_rows::{
    REPORT_BUDGET, Row, all, await_signal, bounded, expect, expect_eq, observed_verdict,
    run_rows_under,
};
use crate::scripted_peer::lock;
use camber::dns01::{AcmeDns01, CloudflareProvider, DnsProvider, RecordId};
use camber::runtime_test_support::{
    IntegrationLifecycleProbe, IntegrationProbeHandle, RuntimeController, runtime_schedule,
};
use camber::{
    AsyncJoinHandle, CleanupItem, IntegrationError, IntegrationFailure, IntegrationKind,
    RuntimeError, runtime,
};
use rustls::sign::CertifiedKey;
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// The runtime's aggregate shutdown deadline in the forced-stop row.
const FORCED_SHUTDOWN: Duration = Duration::from_secs(1);

/// A cleanup bound far past [`FORCED_SHUTDOWN`]: only the aggregate expiry
/// can end the forced row's cleanup in time.
const LONG_CLEANUP: Duration = Duration::from_secs(60);

/// The ACME peer holds the first challenge: exactly one record is
/// acknowledged when a row drops its waiter.
fn held_after_one_create(delete: Script) -> Scripts {
    Scripts {
        challenge: Script::answer().nth(1, Stage::Hold),
        delete,
        ..Scripts::default()
    }
}

/// What a provider saw as its owner dropped it.
#[derive(Default)]
struct DropWitness {
    /// The peer's log at the moment of the drop.
    at_drop: Mutex<Option<CfLog>>,
    /// Signalled once the provider is dropped.
    dropped: Mutex<Option<oneshot::Sender<()>>>,
}

impl DropWitness {
    fn at_drop(&self) -> Option<CfLog> {
        lock(&self.at_drop).clone()
    }
}

/// A real Cloudflare provider that records the peer's store when dropped.
///
/// It holds the peer's shared log, never another provider: the owner's copy
/// is the only one.
struct Witnessed {
    inner: CloudflareProvider,
    peer: CloudflarePeer,
    witness: Arc<DropWitness>,
}

impl Witnessed {
    fn new(fixture: &Fixture) -> Result<(Self, Arc<DropWitness>, oneshot::Receiver<()>), String> {
        let (dropped, dropped_rx) = oneshot::channel();
        let witness = Arc::new(DropWitness {
            dropped: Mutex::new(Some(dropped)),
            ..DropWitness::default()
        });
        let provider = Self {
            inner: fixture.peers.provider()?,
            peer: fixture.peers.cloudflare.clone(),
            witness: Arc::clone(&witness),
        };
        Ok((provider, witness, dropped_rx))
    }
}

impl DnsProvider for Witnessed {
    fn prepare(
        &mut self,
        domains: &[Arc<str>],
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        self.inner.prepare(domains)
    }

    fn create_txt_record(
        &self,
        fqdn: &str,
        value: &str,
    ) -> impl Future<Output = Result<RecordId, RuntimeError>> + Send {
        self.inner.create_txt_record(fqdn, value)
    }

    fn delete_txt_record(
        &self,
        record_id: &str,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        self.inner.delete_txt_record(record_id)
    }
}

impl Drop for Witnessed {
    fn drop(&mut self) {
        *lock(&self.witness.at_drop) = Some(self.peer.log());
        signal(&self.witness.dropped);
    }
}

/// Provision on the calling runtime, wait until the first record is
/// acknowledged and its challenge held, then drop the waiter, and with it the
/// configuration it owned.
fn drop_waiter_after_acknowledgement(
    acme: AcmeDns01,
    provider: Witnessed,
    directory: &AcmePeer,
) -> Row {
    let (waiter, held) = provision_until_held(acme, provider, directory);
    waiter.cancel();
    drop(waiter);
    held
}

/// Provision on the calling runtime and wait until the first record is
/// acknowledged and its challenge held. Answers the waiter and that wait.
fn provision_until_held(
    acme: AcmeDns01,
    provider: Witnessed,
    directory: &AcmePeer,
) -> (AsyncJoinHandle<Result<CertifiedKey, RuntimeError>>, Row) {
    let waiter = camber::spawn_async(async move { acme.provision_cert(provider).await });
    let held = bounded(
        "a challenge to be held after its record was acknowledged",
        DNS_ROW_BOUND,
        directory.until(|log| log.held >= 1),
    );
    (waiter, held)
}

/// The first record's account: its domain and the ID the peer acknowledged.
fn first_record(log: &CfLog) -> Result<(String, String), String> {
    let create = log
        .acknowledged()
        .first()
        .map(|create| (create.domain(), create.id.as_deref().map(str::to_owned)))
        .ok_or("no record was acknowledged")?;
    match create {
        (domain, Some(id)) => Ok((domain, id)),
        (_, None) => Err("the acknowledged record has no ID".to_owned()),
    }
}

// --- rows ------------------------------------------------------------------

/// A graceful stop that begins while the delete is held still lets the delete
/// finish: the record is gone before the owner drops the provider, cleanup
/// took no report account beyond the order's, and nothing is retained.
fn graceful_stop_deletes_before_the_provider_drops() -> Row {
    let fixture = Fixture::start(held_after_one_create(Script::every(Stage::Hold)))?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let (provider, witness, dropped) = Witnessed::new(&fixture)?;
    let directory = fixture.peers.acme.clone();
    let cloudflare = fixture.peers.cloudflare.clone();
    let (observed, teardown) = run_observing(runtime::builder(), move || -> Row {
        drop_waiter_after_acknowledgement(acme, provider, &directory)?;
        bounded(
            "the delete to reach the peer",
            DNS_ROW_BOUND,
            cloudflare.until(|log| !log.deletes.is_empty()),
        )?;
        // The owner's instance account and its order's account; cleanup
        // spends the order's, never another.
        let free = headroom();
        runtime::request_shutdown();
        cloudflare.release();
        all([
            expect_eq(
                "report headroom while cleanup runs",
                free,
                Ok(REPORT_BUDGET - 2),
            ),
            await_signal("the owner to drop its provider", DNS_ROW_BOUND, dropped),
        ])
    });
    let log = fixture.peers.cloudflare.log();
    let (_, id) = first_record(&log)?;
    let checks = all([
        observed_verdict(observed),
        expect_eq("records acknowledged", log.acknowledged().len(), 1),
        expect_eq(
            "the record deleted before the provider dropped",
            witness.at_drop().map(|at_drop| at_drop.deleted(&id)),
            Some(true),
        ),
        expect_exact_deletes(&log, &[]),
        expect_retained(&dns_accounts(&teardown)?, &Vec::new()),
        expect("a stopped order published", fixture.bundle().is_none()),
    ]);
    all([checks, fixture.finish()])
}

/// A delete refused after the waiter dropped is settled before the owner
/// drops the provider, and the runtime's aggregate names that exact ID once.
fn refused_delete_after_waiter_drop_is_named_once() -> Row {
    let fixture = Fixture::start(held_after_one_create(Script::every(Stage::Refuse)))?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let (provider, witness, dropped) = Witnessed::new(&fixture)?;
    let directory = fixture.peers.acme.clone();
    let (observed, teardown) = run_observing(runtime::builder(), move || -> Row {
        drop_waiter_after_acknowledgement(acme, provider, &directory)?;
        await_signal("the owner to drop its provider", DNS_ROW_BOUND, dropped)
    });
    let log = fixture.peers.cloudflare.log();
    let (domain, id) = first_record(&log)?;
    let expected: Named = vec![(domain.clone(), Some(id.clone()))];
    let accounts = dns_accounts(&teardown)?;
    let checks = all([
        observed_verdict(observed),
        expect_eq(
            "the delete's answer when the provider dropped",
            witness.at_drop().and_then(|at_drop| {
                at_drop
                    .deletes
                    .iter()
                    .find(|delete| *delete.id == *id)
                    .map(|delete| delete.answer)
            }),
            Some(DeleteAnswer::Refused),
        ),
        expect_exact_deletes(&log, &[]),
        expect_retained(&accounts, &expected),
        expect_eq(
            "the retained record's failure",
            accounts
                .iter()
                .flat_map(|account| account.cleanup().iter())
                .find(|item| item.domain() == domain)
                .map(CleanupItem::failure),
            Some(IntegrationFailure::PermissionDenied),
        ),
        expect("a stopped order published", fixture.bundle().is_none()),
    ]);
    all([checks, fixture.finish()])
}

/// A forced stop ends a delete the peer never answers at the aggregate
/// deadline, not after the cleanup bound; the owner drops the provider before
/// `run` returns, and the aggregate names the outstanding ID.
fn forced_stop_names_the_delete_it_cut_short() -> Row {
    let fixture = Fixture::start(held_after_one_create(Script::every(Stage::Hold)))?;
    let acme = fixture
        .configuration(&TWO_ZONES)?
        .cleanup_timeout(LONG_CLEANUP);
    let (provider, witness, _dropped) = Witnessed::new(&fixture)?;
    let directory = fixture.peers.acme.clone();
    let cloudflare = fixture.peers.cloudflare.clone();
    let controller = runtime_schedule();
    let builder = runtime::builder()
        .shutdown_timeout(FORCED_SHUTDOWN)
        .with_test_schedule(&controller);
    let (observed, teardown) = run_observing(builder, move || -> Row {
        drop_waiter_after_acknowledgement(acme, provider, &directory)?;
        bounded(
            "the delete to reach the peer",
            DNS_ROW_BOUND,
            cloudflare.until(|log| !log.deletes.is_empty()),
        )?;
        runtime::request_shutdown();
        Ok(())
    });
    let log = fixture.peers.cloudflare.log();
    let (domain, id) = first_record(&log)?;
    let checks = all([
        observed_verdict(observed),
        expect_cleanup_shared_expiry(&controller),
        expect_eq(
            "the delete attempted before the provider dropped",
            witness.at_drop().map(|at_drop| at_drop.attempted(&id)),
            Some(true),
        ),
        expect(
            "the unanswered record was deleted",
            log.records.contains_key(id.as_str()),
        ),
        expect_exact_deletes(&log, &[]),
        expect_retained(&dns_accounts(&teardown)?, &vec![(domain, Some(id))]),
        expect("a stopped order published", fixture.bundle().is_none()),
    ]);
    all([checks, fixture.finish()])
}

/// Cleanup reads the expiry minted by the runtime, not a fresh grace.
fn expect_cleanup_shared_expiry(controller: &RuntimeController) -> Row {
    let mint = controller
        .shutdown_deadline_mint()
        .ok_or("shutdown minted no deadline")?;
    let readings = controller.shutdown_deadline_readings();
    let cleanup: Box<[_]> = readings
        .iter()
        .filter(|reading| reading.participant() == "integration dns01 1")
        .map(|reading| reading.expiry())
        .collect();
    all([
        expect_eq(
            "shutdown deadline mints",
            controller.shutdown_deadline_mints(),
            1,
        ),
        expect_eq("configured aggregate grace", mint.grace(), FORCED_SHUTDOWN),
        expect(
            "cleanup never read its owner's aggregate expiry",
            !cleanup.is_empty(),
        ),
        expect(
            "cleanup read a different expiry",
            cleanup.iter().all(|expiry| *expiry == mint.expiry()),
        ),
    ])
}

/// Fill the report budget with retained failures a controlled integration
/// abandons, returning how many it took and the integration that holds them.
fn saturate_history() -> Result<(usize, IntegrationProbeHandle), String> {
    let probe = IntegrationLifecycleProbe::admit(IntegrationKind::Nats)
        .map_err(|error| format!("the saturating integration was refused: {error:?}"))?;
    let mut retained = 0;
    loop {
        match probe.run(async { Err(charged_failure()) }) {
            // Dropped unread: the failure stays in history.
            Ok(waiter) => {
                drop(waiter);
                retained += 1;
            }
            Err(error) if is_busy(&error) => return Ok((retained, probe)),
            Err(error) => return Err(format!("a saturating failure was refused: {error:?}")),
        }
        if retained > REPORT_BUDGET {
            return Err("the report budget never refused".to_owned());
        }
    }
}

/// With the report history saturated, a new order is `Busy` before any
/// request, yet the admitted order's cleanup still deletes its record.
fn saturated_history_lets_accepted_cleanup_finish() -> Row {
    let fixture = Fixture::start(held_after_one_create(Script::answer()))?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let refused_acme = fixture.configuration(&TWO_ZONES)?;
    let refused_provider = fixture.peers.provider()?;
    let (provider, witness, dropped) = Witnessed::new(&fixture)?;
    let directory = fixture.peers.acme.clone();
    let cloudflare = fixture.peers.cloudflare.clone();
    let (observed, teardown) = run_observing(runtime::builder(), move || -> Row {
        let (waiter, held) = provision_until_held(acme, provider, &directory);
        held?;
        let (retained, probe) = saturate_history()?;
        let requests_before = cloudflare.log().requests.len();
        let refused = bounded(
            "provisioning at saturation",
            DNS_ROW_BOUND,
            refused_acme.provision_cert(refused_provider),
        )?;
        let requests_after = cloudflare.log().requests.len();
        waiter.cancel();
        drop(waiter);
        let dropped = await_signal("the owner to drop its provider", DNS_ROW_BOUND, dropped);
        drop(probe);
        all([
            // The owner's two accounts and the saturating instance's own.
            expect_eq("failures that fit the history", retained, REPORT_BUDGET - 3),
            expect(
                &format!(
                    "provisioning at saturation answered {:?}",
                    refused.as_ref().err()
                ),
                refused.as_ref().err().is_some_and(is_busy),
            ),
            expect_eq(
                "requests the refused order sent",
                requests_after,
                requests_before,
            ),
            dropped,
        ])
    });
    let log = fixture.peers.cloudflare.log();
    let (_, id) = first_record(&log)?;
    let checks = all([
        observed_verdict(observed),
        expect_eq(
            "the record deleted before the provider dropped",
            witness.at_drop().map(|at_drop| at_drop.deleted(&id)),
            Some(true),
        ),
        expect_exact_deletes(&log, &[]),
        expect_retained(&dns_accounts(&teardown)?, &Vec::new()),
    ]);
    all([checks, fixture.finish()])
}

/// A refused delete the caller receives is one account: the caller's error
/// and the aggregate's retained entry name the same record under the same
/// instance, once each.
fn delivered_cleanup_failure_is_one_account() -> Row {
    let fixture = Fixture::start(Scripts {
        delete: Script::answer().nth(2, Stage::Refuse),
        ..Scripts::default()
    })?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = fixture.peers.provider()?;
    let (caller, teardown) = run_observing(runtime::builder(), move || {
        bounded(
            "provision_cert",
            DNS_ROW_BOUND,
            acme.provision_cert(provider),
        )
    });
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
    let expected: Named = vec![(domain, Some(refused))];
    let accounts = dns_accounts(&teardown)?;
    let delivered = match &caller {
        Some(Ok(Err(error))) => integration(error).cloned(),
        _ => None,
    };
    let checks = all([
        match &delivered {
            Some(error) => expect_delivered(error, &expected),
            None => Err(format!("the caller received {caller:?}")),
        },
        expect_retained(&accounts, &expected),
        expect_eq(
            "the retained account's instance",
            accounts
                .iter()
                .find(|account| !account.cleanup().is_empty())
                .and_then(|account| account.instance_id()),
            delivered.as_ref().and_then(IntegrationError::instance_id),
        ),
        expect(
            "the caller's error names no instance",
            delivered
                .as_ref()
                .is_some_and(|error| error.instance_id().is_some()),
        ),
        expect(
            "an order with an unresolved record published",
            fixture.bundle().is_none(),
        ),
    ]);
    all([checks, fixture.finish()])
}

/// Every record `error` names, whether it is the integration failure itself
/// or an aggregate holding DNS accounts.
fn records_named(error: &RuntimeError) -> Named {
    match error {
        RuntimeError::Integration(failure) => named(failure),
        RuntimeError::Lifecycle(_) => aggregate_accounts(error)
            .iter()
            .flat_map(|account| named(account))
            .collect(),
        _ => Vec::new(),
    }
}

/// Startup whose first order issues but cannot delete a record fails before
/// serving, names that record exactly once, and publishes nothing.
fn startup_with_unresolved_cleanup_fails_before_serving() -> Row {
    let fixture = Fixture::start(Scripts {
        delete: Script::answer().nth(1, Stage::Refuse),
        ..Scripts::default()
    })?;
    let served = Arc::new(AtomicBool::new(false));
    let outcome = runtime::builder()
        .tls_auto_dns01(fixture.configuration(&TWO_ZONES)?, "token".into())
        .with_test_dns_transport(&fixture.peers.cloudflare.uri())
        .run(mark_served(&served));
    let log = fixture.peers.cloudflare.log();
    let refused = log
        .deletes
        .first()
        .map(|delete| delete.id.to_string())
        .ok_or("the first delete never reached the peer")?;
    let occurrences = outcome.as_ref().err().map_or(0, |error| {
        records_named(error)
            .iter()
            .filter(|(_, id)| id.as_deref() == Some(refused.as_str()))
            .count()
    });
    let checks = all([
        expect(
            "the directory issued the certificate",
            fixture.peers.acme.log().issued.is_some(),
        ),
        expect(
            "startup served with a record unresolved",
            !served.load(Ordering::SeqCst),
        ),
        expect_eq("accounts naming the unresolved record", occurrences, 1),
        expect_exact_deletes(&log, &[]),
        expect(
            "startup published with a record unresolved",
            fixture.bundle().is_none(),
        ),
    ]);
    all([checks, fixture.finish()])
}

#[test]
fn dns_waiter_drop_keeps_provider_until_cleanup_is_named_or_deleted() {
    run_rows_under(
        DIAGNOSTIC,
        &[
            (
                "a graceful stop deletes before the provider drops",
                graceful_stop_deletes_before_the_provider_drops,
            ),
            (
                "a refused delete after waiter drop is named once",
                refused_delete_after_waiter_drop_is_named_once,
            ),
            (
                "a forced stop names the delete it cut short",
                forced_stop_names_the_delete_it_cut_short,
            ),
            (
                "saturated history lets accepted cleanup finish",
                saturated_history_lets_accepted_cleanup_finish,
            ),
            (
                "a delivered cleanup failure is one account",
                delivered_cleanup_failure_is_one_account,
            ),
            (
                "startup with unresolved cleanup fails before serving",
                startup_with_unresolved_cleanup_fails_before_serving,
            ),
        ],
    );
}
