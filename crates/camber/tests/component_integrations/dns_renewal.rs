//! 12.T2: one renewal owner per cache, one 12-hour interval, the actual
//! 30-day threshold, and the prior certificate kept after a failure.
//!
//! Every row enters the real renewal owner through public `spawn_renewal` or
//! `RuntimeBuilder::tls_auto_dns01` startup. The runtime carries a test
//! schedule, so each interval the owner waits elapses when the row lets it,
//! and the owner still names the interval it asked for. An owner begins its
//! next wait only after its order and that order's cleanup settled, so a
//! begun wait is the row's acknowledgement of the previous pass.
#![cfg(feature = "dns01")]

use crate::dns_cleanup_peers::{
    BUNDLE, CfLog, CloudflarePeer, DNS_ROW_BOUND, DeleteAnswer, Fixture, RENEWAL_INTERVAL, Script,
    Scripts, Stage, TWO_ZONES, await_renewal_waits, dns_accounts, elapse_renewal,
    expect_exact_deletes, expect_retained, generation_expiring, leaf_of, prior_generation,
    renewal_pass, retire_renewal, run_observing, seed_cache, served_leaf, served_store,
};
use crate::integration_rows::{
    Row, all, bounded, clean_run, expect, expect_eq, invalid_config, observed_verdict, refused,
    run_rows, tempdir,
};
use crate::scripted_peer::lock;
use camber::dns01::{AcmeDns01, CloudflareProvider, DnsProvider, RecordId};
use camber::runtime_test_support::{RuntimeController, runtime_schedule};
use camber::{IntegrationFailure, IntegrationOperation, RuntimeError, runtime};
use std::collections::BTreeSet;
use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Days left on a cached leaf that is not yet due: just past 30.
const NOT_DUE_DAYS: u32 = 31;

/// Days left on a cached leaf that is due: just inside 30.
const DUE_DAYS: u32 = 29;

/// The zone that serves `www.example.org` before and after the peer moves it
/// between the two orders.
const ORG_ZONE: &str = "zone-org";
const MOVED_ORG_ZONE: &str = "zone-org-moved";

type PrepareLog = Arc<Mutex<Vec<CfLog>>>;

/// A real Cloudflare provider that records the peer's log at each
/// preparation.
struct PrepareWitness {
    inner: CloudflareProvider,
    peer: CloudflarePeer,
    prepares: PrepareLog,
}

impl PrepareWitness {
    fn new(fixture: &Fixture) -> Result<(Self, PrepareLog), String> {
        let prepares = Arc::new(Mutex::new(Vec::new()));
        let provider = Self {
            inner: fixture.peers.provider()?,
            peer: fixture.peers.cloudflare.clone(),
            prepares: Arc::clone(&prepares),
        };
        Ok((provider, prepares))
    }
}

impl DnsProvider for PrepareWitness {
    fn prepare(
        &mut self,
        domains: &[Arc<str>],
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        lock(&self.prepares).push(self.peer.log());
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

/// A provider that counts its preparations and refuses each one, so no
/// order it starts reaches a directory.
struct CountingRefusal(Arc<AtomicUsize>);

impl DnsProvider for CountingRefusal {
    fn prepare(&mut self, _: &[Arc<str>]) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        self.0.fetch_add(1, Ordering::SeqCst);
        std::future::ready(Err(RuntimeError::Cancelled))
    }

    fn create_txt_record(
        &self,
        _: &str,
        _: &str,
    ) -> impl Future<Output = Result<RecordId, RuntimeError>> + Send {
        std::future::ready(Err(RuntimeError::Cancelled))
    }

    fn delete_txt_record(&self, _: &str) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        std::future::ready(Err(RuntimeError::Cancelled))
    }
}

fn snapshots(prepares: &Mutex<Vec<CfLog>>) -> Vec<CfLog> {
    lock(prepares).clone()
}

/// Every interval each owner waited was the production 12 hours.
fn expect_intervals(controller: &RuntimeController) -> Row {
    let waits = controller.renewal_waits();
    all([
        expect("no renewal interval was waited", !waits.is_empty()),
        expect_eq(
            "renewal intervals other than 12 hours",
            waits
                .iter()
                .filter(|wait| **wait != RENEWAL_INTERVAL)
                .count(),
            0,
        ),
    ])
}

/// The zone lookups one preparation sent: from request `start` up to the
/// order's first TXT create.
fn lookups(requests: &[Box<str>], start: usize) -> Vec<&str> {
    requests
        .iter()
        .skip(start)
        .map(|request| &**request)
        .take_while(|request| !request.starts_with("POST "))
        .filter(|request| request.starts_with("GET /zones?name="))
        .collect()
}

/// The record writes among `requests` that address `zone`, without IDs.
fn writes_to(requests: &[Box<str>], zone: &str) -> Vec<String> {
    let path = format!(" /zones/{zone}/");
    requests
        .iter()
        .filter(|request| request.contains(&path))
        .map(|request| request.split('/').take(4).collect::<Vec<_>>().join("/"))
        .collect()
}

/// The second order looked up its own zones and wrote only through the map
/// that preparation published: `example.org` moved zones between the orders,
/// and each delete removed a distinct record. `second_start` is the request
/// count when the second order prepared.
fn expect_replaced_authority(log: &CfLog, second_start: Option<usize>) -> Row {
    let first_lookups = lookups(&log.requests, 0);
    let first = second_start.and_then(|start| log.requests.get(..start));
    let second = second_start.and_then(|start| log.requests.get(start..));
    let removed: BTreeSet<&str> = log
        .deletes
        .iter()
        .filter(|delete| delete.answer == DeleteAnswer::Deleted)
        .map(|delete| &*delete.id)
        .collect();
    all([
        expect(
            "the first preparation looked up no zone",
            !first_lookups.is_empty(),
        ),
        expect_eq(
            "zones the second preparation looked up",
            second_start.map(|start| lookups(&log.requests, start)),
            Some(first_lookups.clone()),
        ),
        expect_eq(
            "the first order's writes to the zone serving example.org",
            first.map(|first| writes_to(first, ORG_ZONE)),
            Some(vec![
                format!("POST /zones/{ORG_ZONE}/dns_records"),
                format!("DELETE /zones/{ORG_ZONE}/dns_records"),
            ]),
        ),
        expect_eq(
            "the second order's writes to the zone example.org left",
            second.map(|second| writes_to(second, ORG_ZONE)),
            Some(Vec::new()),
        ),
        expect_eq(
            "the second order's writes to the zone example.org moved to",
            second.map(|second| writes_to(second, MOVED_ORG_ZONE)),
            Some(vec![
                format!("POST /zones/{MOVED_ORG_ZONE}/dns_records"),
                format!("DELETE /zones/{MOVED_ORG_ZONE}/dns_records"),
            ]),
        ),
        expect_eq("deletes sent", log.deletes.len(), 4),
        expect_eq("distinct records the deletes removed", removed.len(), 4),
    ])
}

/// Every order's records were deleted by exact ID, and teardown retained no
/// cleanup account.
fn expect_settled(log: &CfLog, teardown: &Result<(), RuntimeError>) -> Row {
    all([
        expect_exact_deletes(log, &[]),
        expect_retained(&dns_accounts(teardown)?, &Vec::new()),
    ])
}

// --- rows ------------------------------------------------------------------

/// A leaf 31 days from expiry starts no order; one 29 days out starts one.
/// Each interval is 12 hours, and the second order prepares only after the
/// first order's records were deleted, with zone lookups of its own. The peer
/// moves `example.org` to a new zone between the orders, so the second order
/// writes only through the map its own preparation published.
fn threshold_interval_and_sequential_preparation() -> Row {
    let fixture = Fixture::start(Scripts::default())?;
    fixture.seed(&generation_expiring(&TWO_ZONES, NOT_DUE_DAYS)?)?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let store = served_store(&acme)?;
    let (provider, prepares) = PrepareWitness::new(&fixture)?;
    let controller = runtime_schedule();
    let cloudflare = fixture.peers.cloudflare.clone();
    let observing = store.clone();
    let (observed, teardown) = run_observing(
        runtime::builder().with_test_schedule(&controller),
        || -> Row {
            let handle = acme.spawn_renewal(provider, store);
            renewal_pass(&controller, 1, DNS_ROW_BOUND)?;
            let not_due = expect_eq(
                "provider requests for a leaf 31 days from expiry",
                cloudflare.log().requests,
                Vec::new(),
            );
            fixture.seed(&generation_expiring(&TWO_ZONES, DUE_DAYS)?)?;
            renewal_pass(&controller, 2, DNS_ROW_BOUND)?;
            let first = cloudflare.log();
            let renewed = fixture.bundle().and_then(|bundle| leaf_of(&bundle));
            let swapped = expect_eq(
                "the leaf the store serves after the first order",
                served_leaf(&observing),
                renewed,
            );
            cloudflare.move_zone("example.org", MOVED_ORG_ZONE);
            fixture.seed(&generation_expiring(&TWO_ZONES, DUE_DAYS)?)?;
            renewal_pass(&controller, 3, DNS_ROW_BOUND)?;
            all([
                not_due,
                expect_eq("records the first order created", first.creates.len(), 2),
                swapped,
                expect_intervals(&controller),
                retire_renewal(handle, DNS_ROW_BOUND),
            ])
        },
    );
    let log = fixture.peers.cloudflare.log();
    let prepared = snapshots(&prepares);
    let second = prepared.get(1);
    let second_start = second.map(|at| at.requests.len());
    let checks = all([
        observed_verdict(observed),
        expect_eq("preparations", prepared.len(), 2),
        expect_eq("records created", log.creates.len(), 4),
        expect_eq(
            "records the first order left when the second prepared",
            second.map(|at| at.records.len()),
            Some(1),
        ),
        expect_eq(
            "first-order deletes answered when the second prepared",
            second.map(|at| {
                at.deletes
                    .iter()
                    .all(|delete| delete.answer == DeleteAnswer::Deleted)
                    && at.deletes.len() == 2
            }),
            Some(true),
        ),
        expect_replaced_authority(&log, second_start),
        expect_settled(&log, &teardown),
    ]);
    all([checks, fixture.finish()])
}

/// A renewal that fails keeps the certificate the store served and the
/// generation the cache held, deletes its records, and retries at the next
/// interval.
fn failed_renewal_keeps_the_prior_valid_certificate() -> Row {
    let fixture = Fixture::start(Scripts {
        finalize: Stage::Refuse,
        ..Scripts::default()
    })?;
    let prior = generation_expiring(&TWO_ZONES, DUE_DAYS)?;
    fixture.seed(&prior)?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let store = served_store(&acme)?;
    let observing = store.clone();
    let controller = runtime_schedule();
    let provider = fixture.peers.provider()?;
    let (observed, teardown) = run_observing(
        runtime::builder().with_test_schedule(&controller),
        || -> Row {
            let handle = acme.spawn_renewal(provider, store);
            renewal_pass(&controller, 1, DNS_ROW_BOUND)?;
            let after_one = fixture.peers.cloudflare.log().creates.len();
            renewal_pass(&controller, 2, DNS_ROW_BOUND)?;
            all([
                expect_eq("records the first failed order created", after_one, 2),
                expect_eq(
                    "records the retry created",
                    fixture.peers.cloudflare.log().creates.len(),
                    4,
                ),
                retire_renewal(handle, DNS_ROW_BOUND),
            ])
        },
    );
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        observed_verdict(observed),
        expect_eq(
            "the cached generation after failed renewals",
            fixture.bundle(),
            Some(prior.clone().into_bytes()),
        ),
        expect_eq(
            "the leaf the store serves after failed renewals",
            served_leaf(&observing),
            leaf_of(prior.as_bytes()),
        ),
        expect_eq("issued leaves", fixture.peers.acme.log().issued, None),
        expect_settled(&log, &teardown),
        expect_eq("records the failed orders left", log.records.len(), 1),
    ]);
    all([checks, fixture.finish()])
}

/// Cancelling the handle while an order holds an acknowledged record stops
/// the order, deletes the record, and only then answers `Cancelled`. No
/// interval begins while the order runs.
fn cancel_settles_the_order_before_answering() -> Row {
    let fixture = Fixture::start(Scripts {
        challenge: Script::answer().nth(1, Stage::Hold),
        ..Scripts::default()
    })?;
    fixture.seed(&generation_expiring(&TWO_ZONES, DUE_DAYS)?)?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let store = served_store(&acme)?;
    let controller = runtime_schedule();
    let provider = fixture.peers.provider()?;
    let directory = fixture.peers.acme.clone();
    let (observed, teardown) = run_observing(
        runtime::builder().with_test_schedule(&controller),
        || -> Row {
            let handle = acme.spawn_renewal(provider, store);
            elapse_renewal(&controller, 1, DNS_ROW_BOUND)?;
            bounded(
                "the order to hold a challenge",
                DNS_ROW_BOUND,
                directory.until(|log| log.held >= 1),
            )?;
            let waits_during_order = controller.renewal_waits().len();
            all([
                expect_eq("intervals begun while the order ran", waits_during_order, 1),
                retire_renewal(handle, DNS_ROW_BOUND),
            ])
        },
    );
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        observed_verdict(observed),
        expect_eq("records acknowledged", log.acknowledged().len(), 1),
        expect(
            "an acknowledged record outlived the cancelled order",
            log.acknowledged()
                .iter()
                .filter_map(|create| create.id.as_deref())
                .all(|id| log.deleted(id)),
        ),
        expect_settled(&log, &teardown),
    ]);
    all([checks, fixture.finish()])
}

/// The runtime-managed owner waits the same 12-hour interval, renews a due
/// leaf, and refuses a public renewal of the same cache before any provider
/// request; a renewal of another cache is admitted beside it.
fn runtime_owner_shares_the_interval_and_refuses_a_second_owner() -> Row {
    let fixture = Fixture::start(Scripts::default())?;
    let prior = prior_generation(&TWO_ZONES)?;
    fixture.seed(&prior)?;
    let other = tempdir()?;
    let controller = runtime_schedule();
    let builder = runtime::builder()
        .with_test_schedule(&controller)
        .tls_auto_dns01(fixture.configuration(&TWO_ZONES)?, "token".into())
        .with_test_dns_transport(&fixture.peers.cloudflare.uri());
    let same = fixture.configuration(&TWO_ZONES)?;
    let elsewhere = fixture.peers.configuration(other.path(), &TWO_ZONES)?;
    let store = served_store(&same)?;
    let same_provider = fixture.peers.provider()?;
    let other_provider = fixture.peers.provider()?;
    let (observed, teardown) = run_observing(builder, || -> Row {
        await_renewal_waits(&controller, 1, DNS_ROW_BOUND)?;
        let second_owner = bounded(
            "the second owner's refusal",
            DNS_ROW_BOUND,
            same.spawn_renewal(same_provider, store.clone())
                .into_future(),
        )?;
        let busy = expect_eq(
            "the refusal of a second owner of the runtime's cache",
            refused(second_owner).map(|(operation, failure, _)| (operation, failure)),
            Some((IntegrationOperation::Renew, IntegrationFailure::Busy)),
        );
        let requests_after_refusal = fixture.peers.cloudflare.log().requests;
        let beside = elsewhere.spawn_renewal(other_provider, store);
        await_renewal_waits(&controller, 2, DNS_ROW_BOUND)?;
        let admitted = retire_renewal(beside, DNS_ROW_BOUND);
        fixture.seed(&generation_expiring(&TWO_ZONES, DUE_DAYS)?)?;
        renewal_pass(&controller, 2, DNS_ROW_BOUND)?;
        all([
            busy,
            expect_eq(
                "provider requests the refused owner sent",
                requests_after_refusal,
                Vec::new(),
            ),
            admitted,
            expect_intervals(&controller),
        ])
    });
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        observed_verdict(observed),
        expect_eq("records the runtime owner created", log.creates.len(), 2),
        expect_eq(
            "the cached leaf after the runtime owner renewed",
            fixture.bundle().and_then(|bundle| leaf_of(&bundle)),
            fixture.peers.acme.log().issued,
        ),
        expect(
            "the cancelled owner published into its cache",
            !other.path().join(BUNDLE).exists(),
        ),
        expect_settled(&log, &teardown),
    ]);
    all([
        checks,
        other
            .close()
            .map_err(|error| format!("remove the other cache: {error}")),
        fixture.finish(),
    ])
}

/// A cache path that cannot be made absolute names no cache to claim: the
/// renewal is refused as configuration before any provider request.
fn an_unnameable_cache_is_refused_before_any_effect() -> Row {
    let names = ["app.example.com"];
    let seeded = tempdir()?;
    seed_cache(seeded.path(), &prior_generation(&names)?)?;
    let store = served_store(&AcmeDns01::new("camber", names).cache_dir(seeded.path()))?;
    let prepares = Arc::new(AtomicUsize::new(0));
    let provider = CountingRefusal(Arc::clone(&prepares));
    let acme = AcmeDns01::new("camber", names).cache_dir("");
    let answered = clean_run(runtime::builder().run(move || {
        bounded(
            "the unnameable cache's renewal",
            DNS_ROW_BOUND,
            acme.spawn_renewal(provider, store).into_future(),
        )
    }))?;
    all([
        expect_eq(
            "the renewal's refusal",
            refused(answered),
            Some(invalid_config(IntegrationOperation::Renew)),
        ),
        expect_eq(
            "preparations the refused renewal sent",
            prepares.load(Ordering::SeqCst),
            0,
        ),
        seeded
            .close()
            .map_err(|error| format!("remove the seeded cache: {error}")),
    ])
}

#[test]
fn renewal_uses_one_owner_one_interval_and_valid_prior_certificate() {
    run_rows(&[
        (
            "the threshold, the interval, and sequential preparation",
            threshold_interval_and_sequential_preparation,
        ),
        (
            "a failed renewal keeps the prior valid certificate",
            failed_renewal_keeps_the_prior_valid_certificate,
        ),
        (
            "cancel settles the order before answering",
            cancel_settles_the_order_before_answering,
        ),
        (
            "the runtime owner shares the interval and refuses a second owner",
            runtime_owner_shares_the_interval_and_refuses_a_second_owner,
        ),
        (
            "an unnameable cache is refused before any effect",
            an_unnameable_cache_is_refused_before_any_effect,
        ),
    ]);
}
