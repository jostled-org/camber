//! Controlled ACME and Cloudflare-shaped peers for the DNS-01 cleanup rows.
//!
//! Each peer runs on the fixture's own Tokio runtime and records a request
//! before it answers, so a row waits on the peer's acknowledgement of a stage,
//! never on elapsed time. The ACME peer speaks HTTPS under a test root and
//! follows RFC 8555 far enough for one order: it signs a real leaf over the
//! order's CSR key. The Cloudflare-shaped peer keeps a TXT store seeded with an
//! unrelated record under a challenge name the order also uses, and answers
//! each create and delete by its script.
//!
//! The oracle reads the peer's store, not Camber's report: a record the peer
//! committed is either deleted by its exact ID or named in the order's cleanup
//! account, and the account names nothing else.

pub use crate::dns_cache_files::BUNDLE;
use crate::dns_cache_files::{DAY, leaf, validity};
pub use crate::integration_rows::run_observing;
use crate::integration_rows::{
    REPORT_BUDGET, Row, all, bounded, expect, expect_eq, integration_accounts, tempdir,
};
use crate::scripted_peer::{bind_loopback, lock};
use bytes::Bytes;
use camber::dns01::{AcmeDns01, CloudflareProvider};
use camber::runtime_test_support::{
    IntegrationLifecycleProbe, IntegrationProbeHandle, OperationWaiter, RuntimeController,
};
use camber::tls::CertStore;
use camber::{
    AsyncJoinHandle, IntegrationError, IntegrationFailure, IntegrationKind, RuntimeError,
};
use http_body_util::{BodyExt, Full};
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use instant_acme::AuthorizationStatus;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tempfile::TempDir;
use tokio::sync::{oneshot, watch};

/// The declared red diagnostic both cleanup matrices fail under.
pub const DIAGNOSTIC: &str = "M9 DNS cleanup lost an admitted record account";

/// The unrelated TXT record the zone holds before any order runs.
pub const SENTINEL_ID: &str = "unrelated-sentinel";

/// The sentinel's name: the challenge name `app.example.com` also uses.
pub const SENTINEL_NAME: &str = "_acme-challenge.app.example.com";

/// The sentinel's value.
pub const SENTINEL_VALUE: &str = "camber-unrelated-sentinel";

/// The label every challenge record sits under.
const CHALLENGE_LABEL: &str = "_acme-challenge.";

/// The zones the Cloudflare-shaped peer knows at start, by name.
const ZONES: [(&str, &str); 2] = [("example.com", "zone-com"), ("example.org", "zone-org")];

/// Two domains, one in each of the peer's zones.
pub const TWO_ZONES: [&str; 2] = ["app.example.com", "www.example.org"];

/// The hang guard every bounded DNS row step runs under; never a timing
/// assertion.
pub const DNS_ROW_BOUND: Duration = Duration::from_secs(30);

/// A startup closure that records it served.
pub fn mark_served(served: &Arc<AtomicBool>) -> impl FnOnce() + use<> {
    let served = Arc::clone(served);
    move || served.store(true, Ordering::SeqCst)
}

/// Send the one-shot signal in `slot`, if it was not sent already.
pub fn signal(slot: &Mutex<Option<oneshot::Sender<()>>>) {
    if let Some(sender) = lock(slot).take() {
        sender.send(()).unwrap_or_default();
    }
}

// --- scripts -------------------------------------------------------------

/// How a peer answers one scripted request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Commit the effect and answer at once.
    Answer,
    /// Refuse explicitly, committing nothing.
    Refuse,
    /// Commit the effect, then close the connection with no answer.
    Lose,
    /// Commit the effect, and answer only once the row releases the peer.
    Hold,
}

/// The stage each request of one kind receives, by its one-based order.
#[derive(Clone, Debug)]
pub struct Script {
    default: Stage,
    nth: BTreeMap<usize, Stage>,
}

impl Script {
    /// Every request answered.
    #[must_use]
    pub const fn answer() -> Self {
        Self::every(Stage::Answer)
    }

    /// Every request receives `stage`.
    #[must_use]
    pub const fn every(stage: Stage) -> Self {
        Self {
            default: stage,
            nth: BTreeMap::new(),
        }
    }

    /// The `n`th request, counting from one, receives `stage`.
    #[must_use]
    pub fn nth(mut self, n: usize, stage: Stage) -> Self {
        self.nth.insert(n, stage);
        self
    }

    fn at(&self, n: usize) -> Stage {
        self.nth.get(&n).copied().unwrap_or(self.default)
    }
}

/// Every scripted answer both peers give.
#[derive(Clone, Debug)]
pub struct Scripts {
    /// ACME challenge-ready posts.
    pub challenge: Script,
    /// The ACME finalize post.
    pub finalize: Stage,
    /// Cloudflare TXT creates.
    pub create: Script,
    /// Cloudflare TXT deletes.
    pub delete: Script,
}

impl Default for Scripts {
    fn default() -> Self {
        Self {
            challenge: Script::answer(),
            finalize: Stage::Answer,
            create: Script::answer(),
            delete: Script::answer(),
        }
    }
}

// --- what the peers saw --------------------------------------------------

/// What the ACME peer received and answered.
#[derive(Clone, Debug, Default)]
pub struct AcmeLog {
    /// Read-only authorization requests received.
    pub authorizations: usize,
    /// Challenge-ready posts received.
    pub challenges: usize,
    /// Requests the peer is holding unanswered.
    pub held: usize,
    /// The leaf the peer issued, as DER, once finalize was answered.
    pub issued: Option<Vec<u8>>,
}

/// One TXT create the Cloudflare-shaped peer received.
#[derive(Clone, Debug)]
pub struct Create {
    /// The challenge name the create asked for.
    pub name: Box<str>,
    /// The TXT value; a secret no account may repeat.
    pub content: Box<str>,
    /// The ID of the record the peer committed; `None` when it refused.
    pub id: Option<Box<str>>,
    /// Whether the peer sent the acknowledgement that names the ID.
    pub answered: bool,
}

impl Create {
    /// The configured domain this create's challenge name serves.
    #[must_use]
    pub fn domain(&self) -> String {
        domain_of(&self.name)
    }
}

/// How the peer answered one delete.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeleteAnswer {
    /// Received and not yet answered.
    Pending,
    /// The record existed in that zone and was removed.
    Deleted,
    /// Refused; the record stays.
    Refused,
    /// No such record in that zone.
    Missing,
}

/// One TXT delete the Cloudflare-shaped peer received.
#[derive(Clone, Debug)]
pub struct Delete {
    /// The record ID the delete named.
    pub id: Box<str>,
    /// How the peer answered.
    pub answer: DeleteAnswer,
    /// The bundle the order's cache held when the delete arrived.
    pub cached: Option<Vec<u8>>,
}

impl Delete {
    /// Whether the order's cache held a published generation when the delete
    /// arrived.
    #[must_use]
    pub const fn published(&self) -> bool {
        self.cached.is_some()
    }
}

/// One TXT record the zone holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Txt {
    /// The zone that holds it.
    pub zone: Box<str>,
    /// Its name.
    pub name: Box<str>,
    /// Its value.
    pub content: Box<str>,
}

/// What the Cloudflare-shaped peer received, and what its zones hold now.
#[derive(Clone, Debug, Default)]
pub struct CfLog {
    /// Every request, as method and path with query, in arrival order.
    pub requests: Vec<Box<str>>,
    /// Every create, in arrival order.
    pub creates: Vec<Create>,
    /// Every delete, in arrival order.
    pub deletes: Vec<Delete>,
    /// The TXT records the zones hold now, by ID.
    pub records: BTreeMap<Box<str>, Txt>,
    /// Every name the peer published to its [`ZoneMirror`], recorded before
    /// the publication was sent.
    pub mirrored: BTreeSet<Box<str>>,
    /// Every publication the mirror refused.
    pub mirror_failures: Vec<String>,
}

impl CfLog {
    /// Creates the peer committed and acknowledged to its caller.
    #[must_use]
    pub fn acknowledged(&self) -> Vec<&Create> {
        self.creates
            .iter()
            .filter(|create| create.answered && create.id.is_some())
            .collect()
    }

    /// Whether the peer removed record `id` on a delete that named it.
    #[must_use]
    pub fn deleted(&self, id: &str) -> bool {
        self.deletes
            .iter()
            .any(|delete| &*delete.id == id && delete.answer == DeleteAnswer::Deleted)
    }

    /// Whether any delete named record `id`.
    #[must_use]
    pub fn attempted(&self, id: &str) -> bool {
        self.deletes.iter().any(|delete| &*delete.id == id)
    }

    /// The ID of the `n`th create's record, counting from one.
    #[must_use]
    pub fn created_id(&self, n: usize) -> Option<Box<str>> {
        self.creates
            .get(n.checked_sub(1)?)
            .and_then(|create| create.id.clone())
    }
}

/// The unrelated record the zone is seeded with.
fn sentinel() -> Txt {
    Txt {
        zone: "zone-com".into(),
        name: SENTINEL_NAME.into(),
        content: SENTINEL_VALUE.into(),
    }
}

/// The challenge name `domain`'s record sits under.
#[must_use]
pub fn challenge_name(domain: &str) -> String {
    format!("{CHALLENGE_LABEL}{domain}")
}

/// The ID of the zone `domain` belongs to when the peer starts, if any.
#[must_use]
pub fn zone_id_of(domain: &str) -> Option<&'static str> {
    ZONES
        .iter()
        .find(|(zone, _)| {
            domain == *zone
                || domain
                    .strip_suffix(zone)
                    .is_some_and(|label| label.ends_with('.'))
        })
        .map(|(_, id)| *id)
}

/// The configured domain a challenge name serves.
#[must_use]
pub fn domain_of(name: &str) -> String {
    name.strip_prefix(CHALLENGE_LABEL)
        .unwrap_or(name)
        .to_owned()
}

// --- the account oracle --------------------------------------------------

/// One cleanup account's unresolved records, as domain and record ID, sorted.
pub type Named = Vec<(String, Option<String>)>;

/// The records `error` names as unresolved, sorted.
#[must_use]
pub fn named(error: &IntegrationError) -> Named {
    let mut named: Named = error
        .cleanup()
        .iter()
        .map(|item| {
            (
                item.domain().to_owned(),
                item.record_id().map(str::to_owned),
            )
        })
        .collect();
    named.sort();
    named
}

/// The integration failure `error` carries, if it is one.
#[must_use]
pub fn integration(error: &RuntimeError) -> Option<&IntegrationError> {
    match error {
        RuntimeError::Integration(failure) => Some(&**failure),
        _ => None,
    }
}

/// Every DNS-01 integration account a runtime's teardown returned.
///
/// # Errors
///
/// When teardown returned something other than success or an aggregate.
pub fn dns_accounts(
    teardown: &Result<(), RuntimeError>,
) -> Result<Vec<Arc<IntegrationError>>, String> {
    match teardown {
        Ok(()) => Ok(Vec::new()),
        Err(aggregate @ RuntimeError::Lifecycle(_)) => Ok(aggregate_accounts(aggregate)),
        Err(other) => Err(format!("the runtime tore down with {other:?}")),
    }
}

/// Every DNS-01 integration account a lifecycle aggregate holds; none for any
/// other error.
#[must_use]
pub fn aggregate_accounts(error: &RuntimeError) -> Vec<Arc<IntegrationError>> {
    let Ok(accounts) = integration_accounts(error) else {
        return Vec::new();
    };
    accounts
        .filter_map(|(kind, _, cause)| match (kind, cause) {
            (IntegrationKind::Dns01, RuntimeError::Integration(account)) => {
                Some(Arc::clone(account))
            }
            _ => None,
        })
        .collect()
}

/// The records a cleanup account must name, read off the peer's store.
///
/// A committed record whose ID Camber learned and did not delete is named by
/// that ID. A committed record whose acknowledgement never reached Camber,
/// or reached only a provider that swallowed it (`unseen`), is named by its
/// domain alone.
#[must_use]
pub fn expected_unresolved(log: &CfLog, unseen: &[Box<str>]) -> Named {
    let mut expected: Named = log
        .creates
        .iter()
        .filter_map(|create| {
            let id = create.id.as_ref()?;
            match create.answered && !unseen.contains(id) {
                true if log.deleted(id) => None,
                true => Some((create.domain(), Some(id.to_string()))),
                false => Some((create.domain(), None)),
            }
        })
        .collect();
    expected.sort();
    expected
}

/// Every delete named an ID this order learned, and the unrelated record
/// survived untouched.
pub fn expect_exact_deletes(log: &CfLog, unseen: &[Box<str>]) -> Row {
    let learned: BTreeSet<&str> = log
        .acknowledged()
        .iter()
        .filter_map(|create| create.id.as_deref())
        .filter(|id| !unseen.iter().any(|unseen| &**unseen == *id))
        .collect();
    let foreign: Vec<&str> = log
        .deletes
        .iter()
        .map(|delete| &*delete.id)
        .filter(|id| !learned.contains(id))
        .collect();
    all([
        expect_eq(
            "the unrelated record",
            log.records.get(SENTINEL_ID),
            Some(&sentinel()),
        ),
        expect_eq(
            "deletes of IDs the order never learned",
            foreign,
            Vec::new(),
        ),
    ])
}

/// Neither a caller's error nor an account repeats a TXT value.
pub fn expect_no_values(error: &IntegrationError, log: &CfLog) -> Row {
    let rendered = format!("{error} {error:?}");
    expect(
        "an account repeated a challenge value",
        log.creates
            .iter()
            .all(|create| !rendered.contains(&*create.content)),
    )
}

/// The one retained account must name exactly `expected` as
/// `CleanupIncomplete`; with nothing expected, no DNS account may carry a
/// record or claim incomplete cleanup.
pub fn expect_retained(accounts: &[Arc<IntegrationError>], expected: &Named) -> Row {
    let carrying: Vec<&Arc<IntegrationError>> = accounts
        .iter()
        .filter(|account| {
            account.failure() == IntegrationFailure::CleanupIncomplete
                || !account.cleanup().is_empty()
        })
        .collect();
    match expected.is_empty() {
        true => expect_eq(
            "retained accounts naming records",
            carrying
                .iter()
                .map(|account| named(account))
                .collect::<Vec<_>>(),
            Vec::new(),
        ),
        false => all([
            expect_eq("retained cleanup accounts", carrying.len(), 1),
            expect_eq(
                "the retained account's failure",
                carrying.first().map(|account| account.failure()),
                Some(IntegrationFailure::CleanupIncomplete),
            ),
            expect_eq(
                "the retained account's records",
                carrying.first().map(|account| named(account)),
                Some(expected.clone()),
            ),
        ]),
    }
}

/// The caller's integration error must name exactly `expected` as
/// `CleanupIncomplete`; with nothing expected it names no record.
pub fn expect_delivered(error: &IntegrationError, expected: &Named) -> Row {
    match expected.is_empty() {
        true => all([
            expect_eq("records the caller was handed", named(error), Vec::new()),
            expect(
                "the caller was told cleanup is incomplete with nothing unresolved",
                error.failure() != IntegrationFailure::CleanupIncomplete,
            ),
        ]),
        false => all([
            expect_eq(
                "the caller's failure",
                error.failure(),
                IntegrationFailure::CleanupIncomplete,
            ),
            expect_eq("the caller's records", named(error), expected.clone()),
        ]),
    }
}

// --- report budget -------------------------------------------------------

/// Whether `error` is the registry's `Busy` refusal.
#[must_use]
pub fn is_busy(error: &RuntimeError) -> bool {
    integration(error).is_some_and(|failure| failure.failure() == IntegrationFailure::Busy)
}

/// Reserve report accounts until the budget refuses, count them, and give
/// every one back before answering.
///
/// A controlled instance takes the first account and each held operation one
/// more, so the count is the budget's free headroom.
///
/// # Errors
///
/// A refusal other than `Busy`, or a held operation that failed.
pub async fn report_headroom() -> Result<usize, String> {
    let mut budget = match HeldBudget::admit() {
        Ok(budget) => budget,
        Err(error) if is_busy(&error) => return Ok(0),
        Err(error) => return Err(format!("the headroom probe was refused: {error:?}")),
    };
    while budget.held.len() < REPORT_BUDGET {
        match budget.hold_one() {
            Ok(()) => {}
            Err(error) if is_busy(&error) => break,
            Err(error) => return Err(format!("a held operation was refused: {error:?}")),
        }
    }
    let counted = budget.held.len() + 1;
    budget.release().await?;
    Ok(counted)
}

/// Free report accounts a controlled instance holds until released.
///
/// The instance takes one account and each held operation one more. Dropped
/// without a release, it opens its gate, so no held operation outlives a
/// failed row.
pub struct HeldBudget {
    probe: IntegrationProbeHandle,
    gate: Release,
    held: Vec<OperationWaiter<()>>,
}

impl HeldBudget {
    /// Admit a controlled instance that holds nothing yet.
    fn admit() -> Result<Self, RuntimeError> {
        Ok(Self {
            probe: IntegrationLifecycleProbe::admit(IntegrationKind::Nats)?,
            gate: Release::new(),
            held: Vec::new(),
        })
    }

    /// Hold one more account behind the gate.
    fn hold_one(&mut self) -> Result<(), RuntimeError> {
        let opened = self.gate.opened();
        let waiter = self.probe.run(async move {
            opened.await;
            Ok(())
        })?;
        self.held.push(waiter);
        Ok(())
    }

    /// Hold every free account except `spare`.
    ///
    /// # Errors
    ///
    /// When fewer than `spare + 1` accounts are free, or a reservation is
    /// refused.
    pub async fn hold(spare: usize) -> Result<Self, String> {
        let free = report_headroom().await?;
        let operations = free
            .checked_sub(spare + 1)
            .ok_or_else(|| format!("{free} free accounts cannot leave {spare} spare"))?;
        let mut budget = Self::admit()
            .map_err(|error| format!("the holding instance was refused: {error:?}"))?;
        budget.held.reserve_exact(operations);
        for _ in 0..operations {
            budget
                .hold_one()
                .map_err(|error| format!("a held operation was refused: {error:?}"))?;
        }
        Ok(budget)
    }

    /// Give every held account back.
    ///
    /// # Errors
    ///
    /// When a held operation or the instance's close failed.
    pub async fn release(mut self) -> Row {
        self.gate.open();
        for waiter in std::mem::take(&mut self.held) {
            waiter
                .wait()
                .await
                .map_err(|error| format!("a held operation failed: {error:?}"))?;
        }
        self.probe
            .close()
            .await
            .map_err(|error| format!("the controlled instance's close failed: {error:?}"))
    }
}

impl Drop for HeldBudget {
    fn drop(&mut self) {
        self.gate.open();
    }
}

/// [`report_headroom`] on the calling runtime, under [`DNS_ROW_BOUND`].
///
/// # Errors
///
/// When the probe failed or passed its bound.
pub fn headroom() -> Result<usize, String> {
    bounded("the report headroom", DNS_ROW_BOUND, report_headroom())?
}

/// [`HeldBudget::hold`] on the calling runtime, under [`DNS_ROW_BOUND`].
///
/// # Errors
///
/// When the hold failed or passed its bound.
pub fn hold_budget(spare: usize) -> Result<HeldBudget, String> {
    bounded("the held budget", DNS_ROW_BOUND, HeldBudget::hold(spare))?
}

/// [`HeldBudget::release`] on the calling runtime, under [`DNS_ROW_BOUND`].
///
/// # Errors
///
/// When the release failed or passed its bound.
pub fn release_budget(budget: HeldBudget) -> Row {
    bounded("the held budget's release", DNS_ROW_BOUND, budget.release())?
}

// --- the renewal clock ---------------------------------------------------

/// The interval every renewal owner waits between checks.
pub const RENEWAL_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);

/// Wait until renewal owners began `waits` interval waits, under `bound`.
///
/// An owner begins its next wait only once its previous order, cleanup
/// included, settled, so this is that order's settlement acknowledgement.
///
/// # Errors
///
/// When the waits did not begin within `bound`.
pub fn await_renewal_waits(controller: &RuntimeController, waits: usize, bound: Duration) -> Row {
    bounded(
        &format!("renewal wait {waits} to begin"),
        bound,
        controller.until_renewal_waits(waits),
    )
}

/// Wait for renewal wait `waits` to begin, then let the oldest running
/// interval elapse.
///
/// # Errors
///
/// When the wait did not begin, or no interval was running.
pub fn elapse_renewal(controller: &RuntimeController, waits: usize, bound: Duration) -> Row {
    await_renewal_waits(controller, waits, bound)?;
    controller
        .elapse_renewal_interval()
        .map_err(|error| format!("elapse renewal interval {waits}: {error:?}"))
}

/// Elapse interval `waits`, then wait under `bound` for the owner to begin
/// the next: the pass that interval started has settled.
///
/// # Errors
///
/// When either wait did not begin, or no interval was running.
pub fn renewal_pass(controller: &RuntimeController, waits: usize, bound: Duration) -> Row {
    elapse_renewal(controller, waits, bound)?;
    await_renewal_waits(controller, waits + 1, bound)
}

/// Cancel a public renewal and read its answer, under `bound`: `Cancelled`
/// once its owner stopped, cleaned up, and retired.
///
/// # Errors
///
/// When the renewal did not answer, or answered anything else.
pub fn retire_renewal(handle: AsyncJoinHandle<Result<(), RuntimeError>>, bound: Duration) -> Row {
    handle.cancel();
    let answer = bounded(
        "the cancelled renewal to answer",
        bound,
        handle.into_future(),
    )?;
    expect_eq(
        "the cancelled renewal's answer",
        answer
            .and_then(|renewal| renewal)
            .map_err(|error| format!("{error:?}")),
        Err(format!("{:?}", RuntimeError::Cancelled)),
    )
}

/// The certificate store a renewal swaps into, loaded from `acme`'s cache.
///
/// # Errors
///
/// When the cache holds no generation, or one the cache refuses.
pub fn served_store(acme: &AcmeDns01) -> Result<CertStore, String> {
    match acme.load_cached_cert() {
        Ok(Some(key)) => Ok(CertStore::new(key)),
        Ok(None) => Err("the seeded generation was not cached".to_owned()),
        Err(error) => Err(format!("the seeded generation was refused: {error:?}")),
    }
}

/// The leaf `store` serves, as DER.
#[must_use]
pub fn served_leaf(store: &CertStore) -> Option<Vec<u8>> {
    chain_leaf(&store.load().cert)
}

/// The leaf a chain starts with, as DER.
#[must_use]
pub fn chain_leaf(chain: &[CertificateDer<'_>]) -> Option<Vec<u8>> {
    chain.first().map(|leaf| leaf.as_ref().to_vec())
}

/// The leaf a generation bundle carries, as DER.
#[must_use]
pub fn leaf_of(bundle: &[u8]) -> Option<Vec<u8>> {
    CertificateDer::pem_slice_iter(bundle)
        .next()
        .and_then(Result::ok)
        .map(|leaf| leaf.as_ref().to_vec())
}

// --- the cache -----------------------------------------------------------

/// A DNS-01 configuration of `domains` over `cache_dir`, ordering from the
/// directory at `directory` and trusting `root_pem` for it.
///
/// # Errors
///
/// The configuration's own refusal.
pub fn directory_configuration(
    cache_dir: &Path,
    domains: &[&str],
    directory: &str,
    root_pem: &[u8],
) -> Result<AcmeDns01, String> {
    AcmeDns01::new("camber", domains.iter().copied())
        .email("admin@example.com")
        .cache_dir(cache_dir)
        .directory_url(directory)
        .and_then(|acme| acme.add_root_certificate(root_pem))
        .map_err(|error| format!("configuration: {error:?}"))
}

/// The bundle published in `cache_dir`, if any.
///
/// # Panics
///
/// When the bundle exists but cannot be read: reading it as unpublished
/// would pass a "nothing published" row for a reason it does not claim.
#[must_use]
pub fn cached_bundle(cache_dir: &Path) -> Option<Vec<u8>> {
    let path = cache_dir.join(BUNDLE);
    published_bundle(&path).unwrap_or_else(|error| panic!("read {}: {error}", path.display()))
}

/// The bundle at `path`; `None` only when no bundle was published there:
/// the path is absent, or a row obstructed it with a directory.
fn published_bundle(path: &Path) -> Result<Option<Vec<u8>>, std::io::Error> {
    match std::fs::read(path) {
        Ok(bundle) => Ok(Some(bundle)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) if path.is_dir() => Ok(None),
        Err(error) => Err(error),
    }
}

/// Publish `bundle` as `cache_dir`'s generation.
///
/// # Errors
///
/// When the cache cannot be written.
pub fn seed_cache(cache_dir: &Path, bundle: &str) -> Row {
    std::fs::create_dir_all(cache_dir)
        .and_then(|()| std::fs::write(cache_dir.join(BUNDLE), bundle))
        .map_err(|error| format!("seed the cached generation: {error}"))
}

// --- the fixture ---------------------------------------------------------

/// One row's cache root and the peers its order talks to.
pub struct Fixture {
    root: TempDir,
    cache: Box<Path>,
    /// Both peers.
    pub peers: Peers,
}

impl Fixture {
    /// Start both peers under `scripts` over a fresh cache root.
    ///
    /// # Errors
    ///
    /// When the root or a peer cannot be built.
    pub fn start(scripts: Scripts) -> Result<Self, String> {
        let root = tempdir()?;
        let cache = root.path().join("cache").into_boxed_path();
        let peers = Peers::start(scripts, &cache)?;
        Ok(Self { root, cache, peers })
    }

    /// The cache directory the order publishes into.
    #[must_use]
    pub fn cache(&self) -> &Path {
        &self.cache
    }

    /// A DNS-01 configuration of `domains` over this row's cache.
    ///
    /// # Errors
    ///
    /// The configuration's own refusal.
    pub fn configuration(&self, domains: &[&str]) -> Result<AcmeDns01, String> {
        self.peers.configuration(&self.cache, domains)
    }

    /// The published bundle's bytes, if any.
    ///
    /// # Panics
    ///
    /// When the bundle exists but cannot be read.
    #[must_use]
    pub fn bundle(&self) -> Option<Vec<u8>> {
        cached_bundle(&self.cache)
    }

    /// Publish `bundle` as the cache's prior generation.
    ///
    /// # Errors
    ///
    /// When the cache cannot be written.
    pub fn seed(&self, bundle: &str) -> Row {
        seed_cache(&self.cache, bundle)
    }

    /// Stop both peers, then remove the cache root.
    pub fn finish(self) -> Row {
        let Self { root, peers, .. } = self;
        all([
            peers.finish(),
            root.close()
                .map_err(|error| format!("remove the cache root: {error}")),
        ])
    }
}

// --- the peers -----------------------------------------------------------

/// Both peers, on the fixture's own runtime.
pub struct Peers {
    runtime: Option<tokio::runtime::Runtime>,
    /// The ACME directory.
    pub acme: AcmePeer,
    /// The Cloudflare-shaped provider API.
    pub cloudflare: CloudflarePeer,
}

impl Peers {
    /// Start both peers under `scripts`. A delete records whether the cache
    /// in `cache_dir` held a published generation when it arrived.
    ///
    /// # Errors
    ///
    /// When the runtime, the test PKI, or a listener cannot be built.
    pub fn start(scripts: Scripts, cache_dir: &Path) -> Result<Self, String> {
        let runtime = peer_runtime()?;
        let pki = Pki::new("camber dns cleanup root")?;
        let tls = pki.server_config()?;
        let cloudflare = CloudflarePeer::start(
            &runtime,
            scripts.create,
            scripts.delete,
            cache_dir.join(BUNDLE).into_boxed_path(),
            None,
        )?;
        let acme = AcmePeer::start(&runtime, tls, pki, scripts.challenge, scripts.finalize)?;
        Ok(Self {
            runtime: Some(runtime),
            acme,
            cloudflare,
        })
    }

    /// A DNS-01 configuration of `domains` over `cache_dir`, pointed at this
    /// ACME peer and trusting its root.
    ///
    /// # Errors
    ///
    /// The configuration's own refusal.
    pub fn configuration(&self, cache_dir: &Path, domains: &[&str]) -> Result<AcmeDns01, String> {
        directory_configuration(
            cache_dir,
            domains,
            &self.acme.directory_url(),
            self.acme.shared.pki.root_pem().as_bytes(),
        )
    }

    /// A real Cloudflare provider pointed at this peer.
    ///
    /// # Errors
    ///
    /// The descriptor's own refusal.
    pub fn provider(&self) -> Result<CloudflareProvider, String> {
        self.cloudflare.provider("token")
    }

    /// Stop both peers within a bound; held requests end unanswered.
    pub fn finish(mut self) -> Row {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_timeout(PEER_SHUTDOWN_BOUND);
        }
        Ok(())
    }
}

impl Drop for Peers {
    fn drop(&mut self) {
        if let Some(runtime) = self.runtime.take() {
            runtime.shutdown_background();
        }
    }
}

/// How long a finishing peer runtime waits for its tasks; held requests end
/// unanswered.
const PEER_SHUTDOWN_BOUND: Duration = Duration::from_secs(5);

/// The runtime a fixture's peers run on.
fn peer_runtime() -> Result<tokio::runtime::Runtime, String> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .map_err(|error| format!("peer runtime: {error}"))
}

/// A shared log a peer appends to and rows wait on.
///
/// Every write bumps the version after the log changed, so a waiter that
/// checks the log after subscribing never misses the write that satisfies it.
struct Journal<T> {
    log: Arc<Mutex<T>>,
    version: watch::Sender<u64>,
}

impl<T: Clone + Default + Send + 'static> Journal<T> {
    fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(T::default())),
            version: watch::Sender::new(0),
        }
    }

    fn write<R>(&self, change: impl FnOnce(&mut T) -> R) -> R {
        let changed = {
            let mut log = lock(&self.log);
            change(&mut *log)
        };
        self.version.send_modify(|version| *version += 1);
        changed
    }

    fn read(&self) -> T {
        lock(&self.log).clone()
    }

    fn until<F>(&self, holds: F) -> impl Future<Output = ()> + Send + 'static + use<T, F>
    where
        F: Fn(&T) -> bool + Send + 'static,
    {
        let log = Arc::clone(&self.log);
        let mut version = self.version.subscribe();
        async move {
            loop {
                let held = {
                    let log = lock(&log);
                    holds(&*log)
                };
                if held {
                    return;
                }
                // A peer gone before the claim held ends the wait; the row's
                // own assertions then name what never arrived.
                if version.changed().await.is_err() {
                    return;
                }
            }
        }
    }
}

/// A gate a peer's held requests wait behind.
struct Release(watch::Sender<bool>);

impl Release {
    fn new() -> Self {
        Self(watch::Sender::new(false))
    }

    fn open(&self) {
        self.0.send_replace(true);
    }

    async fn wait(&self) {
        self.opened().await;
    }

    /// A wait that owns its subscription, so it outlives the borrow of the
    /// gate.
    fn opened(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut gate = self.0.subscribe();
        async move {
            drop(gate.wait_for(|open| *open).await);
        }
    }
}

type Answer = Result<Response<Full<Bytes>>, std::io::Error>;

/// Close the connection with no answer.
fn lost() -> Answer {
    Err(std::io::Error::other("the test peer drops this answer"))
}

fn respond(status: StatusCode, headers: &[(&str, String)], body: Bytes) -> Answer {
    let mut response = Response::builder().status(status);
    for (name, value) in headers {
        response = response.header(*name, value);
    }
    response
        .body(Full::new(body))
        .map_err(std::io::Error::other)
}

fn respond_json(status: StatusCode, headers: &[(&str, String)], body: &Value) -> Answer {
    respond(status, headers, Bytes::from(body.to_string()))
}

/// Serve `service` on `listener`, over TLS when `tls` is set.
fn serve<F, Fut>(
    runtime: &tokio::runtime::Runtime,
    listener: std::net::TcpListener,
    tls: Option<tokio_rustls::TlsAcceptor>,
    keep_alive: bool,
    service: F,
) -> Result<(), String>
where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Answer> + Send + 'static,
{
    let listener = {
        let _entered = runtime.enter();
        tokio::net::TcpListener::from_std(listener).map_err(|error| format!("listener: {error}"))?
    };
    runtime.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let tls = tls.clone();
            let service = service.clone();
            tokio::spawn(async move {
                match tls {
                    Some(acceptor) => tls_connection(acceptor, stream, keep_alive, service).await,
                    None => connection(stream, keep_alive, service).await,
                }
            });
        }
    });
    Ok(())
}

async fn tls_connection<F, Fut>(
    acceptor: tokio_rustls::TlsAcceptor,
    stream: tokio::net::TcpStream,
    keep_alive: bool,
    service: F,
) where
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Answer> + Send + 'static,
{
    if let Ok(stream) = acceptor.accept(stream).await {
        connection(stream, keep_alive, service).await;
    }
}

async fn connection<I, F, Fut>(io: I, keep_alive: bool, service: F)
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    F: Fn(Request<Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: Future<Output = Answer> + Send + 'static,
{
    let served = hyper::server::conn::http1::Builder::new()
        .keep_alive(keep_alive)
        .serve_connection(TokioIo::new(io), hyper::service::service_fn(service));
    // A lost answer ends the connection with an error; that is the script.
    drop(served.await);
}

/// The request's method, path, query, and collected body.
async fn read(
    request: Request<Incoming>,
) -> Result<(Method, String, String, Bytes), std::io::Error> {
    let method = request.method().clone();
    let path = request.uri().path().to_owned();
    let query = request.uri().query().unwrap_or_default().to_owned();
    let body = request
        .into_body()
        .collect()
        .await
        .map_err(std::io::Error::other)?
        .to_bytes();
    Ok((method, path, query, body))
}

/// The segments of a request path, without its outer slashes.
fn path_segments(path: &str) -> Vec<&str> {
    path.trim_matches('/').split('/').collect()
}

// --- Cloudflare-shaped peer ----------------------------------------------

/// A DNS server that answers what the Cloudflare-shaped peer's zones hold.
///
/// The peer publishes a name's complete value set after every change to it,
/// one publication at a time, and before it answers the request that changed
/// it. A create is acknowledged only once DNS answers its record.
pub trait ZoneMirror: Send + Sync + 'static {
    /// Make `name` answer exactly `values`; none removes the name.
    fn publish<'a>(
        &'a self,
        name: &'a str,
        values: &'a [Box<str>],
    ) -> impl Future<Output = Result<(), String>> + Send + 'a;
}

/// The mirror of a peer no DNS server watches: the peer holds none, so it
/// never publishes.
pub struct NoMirror;

impl ZoneMirror for NoMirror {
    async fn publish(&self, _: &str, _: &[Box<str>]) -> Result<(), String> {
        Ok(())
    }
}

struct CfShared<M> {
    create: Script,
    delete: Script,
    published: Box<Path>,
    next_id: AtomicU64,
    zones: Mutex<BTreeMap<Box<str>, Box<str>>>,
    log: Journal<CfLog>,
    release: Release,
    mirror: Option<Arc<M>>,
    /// Serializes publications, so each one reads the store it publishes.
    mirroring: tokio::sync::Mutex<()>,
}

/// The Cloudflare-shaped provider API. Cheap to clone; every clone is the
/// same peer.
pub struct CloudflarePeer<M = NoMirror> {
    addr: SocketAddr,
    shared: Arc<CfShared<M>>,
}

impl<M> Clone for CloudflarePeer<M> {
    fn clone(&self) -> Self {
        Self {
            addr: self.addr,
            shared: Arc::clone(&self.shared),
        }
    }
}

impl<M: ZoneMirror> CloudflarePeer<M> {
    fn start(
        runtime: &tokio::runtime::Runtime,
        create: Script,
        delete: Script,
        published: Box<Path>,
        mirror: Option<Arc<M>>,
    ) -> Result<Self, String> {
        let log = Journal::new();
        log.write(|log: &mut CfLog| log.records.insert(SENTINEL_ID.into(), sentinel()));
        let shared = Arc::new(CfShared {
            create,
            delete,
            published,
            next_id: AtomicU64::new(1),
            zones: Mutex::new(
                ZONES
                    .iter()
                    .map(|(name, id)| ((*name).into(), (*id).into()))
                    .collect(),
            ),
            log,
            release: Release::new(),
            mirror,
            mirroring: tokio::sync::Mutex::new(()),
        });
        // Every answer closes its connection, so no later request reuses one
        // a lost answer ended.
        let (listener, addr) = bind_loopback()?;
        serve(runtime, listener, None, false, {
            let shared = Arc::clone(&shared);
            move |request| cloudflare(Arc::clone(&shared), request)
        })?;
        Ok(Self { addr, shared })
    }

    /// The API base a provider is pointed at.
    #[must_use]
    pub fn uri(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// A real Cloudflare provider holding `token`, pointed at this peer.
    ///
    /// # Errors
    ///
    /// The descriptor's own refusal.
    pub fn provider(&self, token: &str) -> Result<CloudflareProvider, String> {
        CloudflareProvider::with_base_url(token.into(), self.uri().into())
            .map_err(|error| format!("descriptor: {error:?}"))
    }

    /// What the peer has seen so far.
    #[must_use]
    pub fn log(&self) -> CfLog {
        self.shared.log.read()
    }

    /// Resolve once `holds` is true of the peer's log.
    pub fn until<F>(&self, holds: F) -> impl Future<Output = ()> + Send + 'static + use<M, F>
    where
        F: Fn(&CfLog) -> bool + Send + 'static,
    {
        self.shared.log.until(holds)
    }

    /// Let every held request finish.
    pub fn release(&self) {
        self.shared.release.open();
    }

    /// Answer later lookups of zone `name` with the ID `id`.
    pub fn move_zone(&self, name: &str, id: &str) {
        lock(&self.shared.zones).insert(name.into(), id.into());
    }

    /// Publish every name the zones hold now to the mirror.
    async fn mirror_store(&self) {
        let names: BTreeSet<Box<str>> = self
            .log()
            .records
            .into_values()
            .map(|record| record.name)
            .collect();
        for name in names {
            mirror(&self.shared, &name).await;
        }
    }

    /// Remove every name the peer ever published from the mirror.
    ///
    /// # Errors
    ///
    /// Each removal the mirror refused.
    async fn clear_mirror(&self) -> Row {
        let Some(zone_mirror) = &self.shared.mirror else {
            return Ok(());
        };
        let _serial = self.shared.mirroring.lock().await;
        let mut refused = Vec::new();
        for name in self.log().mirrored {
            if let Err(reason) = zone_mirror.publish(&name, &[]).await {
                refused.push(format!("{name}: {reason}"));
            }
        }
        expect_eq("mirrored names left behind", refused, Vec::new())
    }
}

/// Publish `name`'s complete value set to the peer's mirror, if it has one.
///
/// The publication runs as its own task: a caller that hangs up drops the
/// request's handler, and must not leave the name half-written.
async fn mirror<M: ZoneMirror>(shared: &Arc<CfShared<M>>, name: &str) {
    if shared.mirror.is_none() {
        return;
    }
    let shared = Arc::clone(shared);
    let name = name.to_owned();
    let publication = tokio::spawn(async move { publish_mirrored(&shared, &name).await });
    match publication.await {
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        // Cancelled only when the peer's runtime shuts down.
        Ok(()) | Err(_) => {}
    }
}

/// Publish `name`'s complete value set to the peer's mirror.
///
/// The name is recorded before the publication is sent, so teardown removes
/// it even when the answer is lost. A refusal is logged, never answered.
async fn publish_mirrored<M: ZoneMirror>(shared: &CfShared<M>, name: &str) {
    let Some(zone_mirror) = &shared.mirror else {
        return;
    };
    let _serial = shared.mirroring.lock().await;
    let values = shared.log.write(|log| {
        log.mirrored.insert(name.into());
        log.records
            .values()
            .filter(|record| &*record.name == name)
            .map(|record| record.content.clone())
            .collect::<Vec<_>>()
    });
    if let Err(reason) = zone_mirror.publish(name, &values).await {
        shared
            .log
            .write(|log| log.mirror_failures.push(format!("{name}: {reason}")));
    }
}

/// A Cloudflare-shaped peer alone on its own runtime, mirroring its zones
/// into a real DNS server.
pub struct MirroredPeer<M: ZoneMirror> {
    runtime: Option<tokio::runtime::Runtime>,
    /// The provider API.
    pub cloudflare: CloudflarePeer<M>,
}

impl<M: ZoneMirror> MirroredPeer<M> {
    /// Start the peer under `create` and `delete`, then publish its seeded
    /// store to `zone_mirror`. A delete records the generation the cache in
    /// `cache_dir` held when it arrived.
    ///
    /// # Errors
    ///
    /// When the runtime or listener cannot be built.
    pub fn start(
        zone_mirror: Arc<M>,
        create: Script,
        delete: Script,
        cache_dir: &Path,
    ) -> Result<Self, String> {
        let runtime = peer_runtime()?;
        let cloudflare = CloudflarePeer::start(
            &runtime,
            create,
            delete,
            cache_dir.join(BUNDLE).into_boxed_path(),
            Some(zone_mirror),
        )?;
        runtime.block_on(cloudflare.mirror_store());
        Ok(Self {
            runtime: Some(runtime),
            cloudflare,
        })
    }

    /// A real Cloudflare provider with a dummy token, pointed at this peer.
    ///
    /// # Errors
    ///
    /// The descriptor's own refusal.
    pub fn provider(&self) -> Result<CloudflareProvider, String> {
        self.cloudflare.provider("camber-local-token")
    }

    /// Remove everything the peer mirrored, then stop it within a bound;
    /// held requests end unanswered.
    ///
    /// # Errors
    ///
    /// Each mirrored name the mirror would not remove.
    pub fn finish(mut self) -> Row {
        self.cloudflare.release();
        let Some(runtime) = self.runtime.take() else {
            return Ok(());
        };
        let cleared = runtime.block_on(self.cloudflare.clear_mirror());
        runtime.shutdown_timeout(PEER_SHUTDOWN_BOUND);
        cleared
    }
}

impl<M: ZoneMirror> Drop for MirroredPeer<M> {
    fn drop(&mut self) {
        let Some(runtime) = self.runtime.take() else {
            return;
        };
        self.cloudflare.release();
        // The fallback after an unwind: still remove what the peer mirrored,
        // unless the drop runs where blocking is not allowed.
        if tokio::runtime::Handle::try_current().is_err()
            && let Err(reason) = runtime.block_on(self.cloudflare.clear_mirror())
        {
            eprintln!("the mirrored peer left records after an unwind: {reason}");
        }
        runtime.shutdown_background();
    }
}

fn envelope(status: StatusCode, success: bool, result: &Value, code: Option<u32>) -> Answer {
    let errors = code.map_or_else(|| json!([]), |code| json!([{ "code": code }]));
    respond_json(
        status,
        &[("content-type", "application/json".to_owned())],
        &json!({ "success": success, "result": result, "errors": errors }),
    )
}

async fn cloudflare<M: ZoneMirror>(shared: Arc<CfShared<M>>, request: Request<Incoming>) -> Answer {
    let (method, path, query, body) = read(request).await?;
    shared.log.write(|log| {
        let query = match query.is_empty() {
            true => String::new(),
            false => format!("?{query}"),
        };
        log.requests
            .push(format!("{method} {path}{query}").into_boxed_str());
    });
    let segments = path_segments(&path);
    match (method, segments.as_slice()) {
        (Method::GET, ["zones"]) => lookup(&shared, &query),
        (Method::POST, ["zones", zone, "dns_records"]) => create(&shared, zone, &body).await,
        (Method::DELETE, ["zones", zone, "dns_records", id]) => delete(&shared, zone, id).await,
        _ => envelope(StatusCode::NOT_FOUND, false, &Value::Null, Some(7003)),
    }
}

fn lookup<M>(shared: &CfShared<M>, query: &str) -> Answer {
    let name = query
        .split('&')
        .find_map(|pair| pair.strip_prefix("name="))
        .unwrap_or_default();
    let zones: Vec<Value> = lock(&shared.zones)
        .get_key_value(name)
        .map(|(zone, id)| json!({ "id": id, "name": zone }))
        .into_iter()
        .collect();
    envelope(StatusCode::OK, true, &Value::Array(zones), None)
}

async fn create<M: ZoneMirror>(shared: &Arc<CfShared<M>>, zone: &str, body: &[u8]) -> Answer {
    let record: Value = serde_json::from_slice(body).map_err(std::io::Error::other)?;
    let name: Box<str> = record["name"].as_str().unwrap_or_default().into();
    let content: Box<str> = record["content"].as_str().unwrap_or_default().into();
    // One write numbers the create, commits its record, and logs it, so a
    // row that sees the create also sees the record it committed.
    let (nth, stage, id) = shared.log.write(|log| {
        let nth = log.creates.len() + 1;
        let stage = shared.create.at(nth);
        let id = match stage {
            Stage::Refuse => None,
            Stage::Answer | Stage::Lose | Stage::Hold => Some(
                format!("txt-{}", shared.next_id.fetch_add(1, Ordering::SeqCst)).into_boxed_str(),
            ),
        };
        if let Some(id) = &id {
            log.records.insert(
                id.clone(),
                Txt {
                    zone: zone.into(),
                    name: name.clone(),
                    content: content.clone(),
                },
            );
        }
        log.creates.push(Create {
            name: name.clone(),
            content,
            id: id.clone(),
            answered: false,
        });
        (nth, stage, id)
    });
    if id.is_some() {
        mirror(shared, &name).await;
    }
    match (stage, id) {
        (Stage::Answer, Some(id)) => {
            shared.log.write(|log| log.creates[nth - 1].answered = true);
            envelope(StatusCode::OK, true, &json!({ "id": id }), None)
        }
        (Stage::Hold, _) => {
            shared.release.wait().await;
            lost()
        }
        (Stage::Lose, _) => lost(),
        _ => envelope(StatusCode::BAD_REQUEST, false, &Value::Null, Some(1004)),
    }
}

async fn delete<M: ZoneMirror>(shared: &Arc<CfShared<M>>, zone: &str, id: &str) -> Answer {
    // An unreadable bundle loses the answer rather than log the delete as
    // arriving before publication.
    let cached = published_bundle(&shared.published)?;
    let (nth, stage) = shared.log.write(|log| {
        log.deletes.push(Delete {
            id: id.into(),
            answer: DeleteAnswer::Pending,
            cached,
        });
        let nth = log.deletes.len();
        (nth, shared.delete.at(nth))
    });
    if stage == Stage::Hold {
        shared.release.wait().await;
    }
    let (answer, removed) = shared.log.write(|log| {
        let matches_zone = log
            .records
            .get(id)
            .is_some_and(|record| &*record.zone == zone);
        let (answer, removed) = match (stage, matches_zone) {
            (Stage::Refuse, _) => (DeleteAnswer::Refused, None),
            (_, true) => (DeleteAnswer::Deleted, log.records.remove(id)),
            (_, false) => (DeleteAnswer::Missing, None),
        };
        log.deletes[nth - 1].answer = answer;
        (answer, removed)
    });
    if let Some(record) = removed {
        mirror(shared, &record.name).await;
    }
    match (answer, stage) {
        (_, Stage::Lose) => lost(),
        (DeleteAnswer::Deleted, _) => envelope(StatusCode::OK, true, &json!({ "id": id }), None),
        (DeleteAnswer::Refused, _) => {
            envelope(StatusCode::FORBIDDEN, false, &Value::Null, Some(10000))
        }
        _ => envelope(StatusCode::NOT_FOUND, false, &Value::Null, Some(81044)),
    }
}

// --- ACME peer -----------------------------------------------------------

struct AcmeShared {
    base: Box<str>,
    pki: Pki,
    challenge: Script,
    finalize: Stage,
    nonce: AtomicU64,
    identifiers: Mutex<Vec<String>>,
    authorization_states: Mutex<BTreeMap<Box<str>, AuthorizationStatus>>,
    authorization_answer: Mutex<Stage>,
    certificate: Mutex<Option<Bytes>>,
    log: Journal<AcmeLog>,
    release: Release,
}

/// The ACME directory. Cheap to clone; every clone is the same peer.
#[derive(Clone)]
pub struct AcmePeer {
    shared: Arc<AcmeShared>,
}

impl AcmePeer {
    fn start(
        runtime: &tokio::runtime::Runtime,
        tls: Arc<rustls::ServerConfig>,
        pki: Pki,
        challenge: Script,
        finalize: Stage,
    ) -> Result<Self, String> {
        let (listener, addr) = bind_loopback()?;
        let shared = Arc::new(AcmeShared {
            base: format!("https://localhost:{}", addr.port()).into_boxed_str(),
            pki,
            challenge,
            finalize,
            nonce: AtomicU64::new(1),
            identifiers: Mutex::new(Vec::new()),
            authorization_states: Mutex::new(BTreeMap::new()),
            authorization_answer: Mutex::new(Stage::Answer),
            certificate: Mutex::new(None),
            log: Journal::new(),
            release: Release::new(),
        });
        serve(
            runtime,
            listener,
            Some(tokio_rustls::TlsAcceptor::from(tls)),
            true,
            {
                let shared = Arc::clone(&shared);
                move |request| acme(Arc::clone(&shared), request)
            },
        )?;
        Ok(Self { shared })
    }

    /// The directory URL, under the name the peer's leaf certifies.
    #[must_use]
    pub fn directory_url(&self) -> String {
        format!("{}/directory", self.shared.base)
    }

    /// Set a domain's authorization state before starting its order.
    pub fn authorization_state(&self, domain: &str, status: AuthorizationStatus) {
        lock(&self.shared.authorization_states).insert(domain.into(), status);
    }

    /// Select the answer to read-only authorization requests before the order starts.
    pub fn authorization_answer(&self, stage: Stage) {
        *lock(&self.shared.authorization_answer) = stage;
    }

    /// What the peer has seen so far.
    #[must_use]
    pub fn log(&self) -> AcmeLog {
        self.shared.log.read()
    }

    /// Resolve once `holds` is true of the peer's log.
    pub fn until<F>(&self, holds: F) -> impl Future<Output = ()> + Send + 'static + use<F>
    where
        F: Fn(&AcmeLog) -> bool + Send + 'static,
    {
        self.shared.log.until(holds)
    }
}

impl AcmeShared {
    fn authorization_state(&self, index: &str) -> (String, AuthorizationStatus) {
        let name = index
            .parse::<usize>()
            .ok()
            .and_then(|index| lock(&self.identifiers).get(index).cloned())
            .unwrap_or_default();
        let status = lock(&self.authorization_states)
            .get(name.as_str())
            .copied()
            .unwrap_or(AuthorizationStatus::Pending);
        (name, status)
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// A fresh nonce and `content_type`.
    fn typed_headers(&self, content_type: &str) -> Vec<(&'static str, String)> {
        let nonce = self.nonce.fetch_add(1, Ordering::SeqCst);
        vec![
            ("replay-nonce", format!("nonce-{nonce}")),
            ("content-type", content_type.to_owned()),
        ]
    }

    fn headers(&self, location: Option<String>) -> Vec<(&'static str, String)> {
        let mut headers = self.typed_headers("application/json");
        if let Some(location) = location {
            headers.push(("location", location));
        }
        headers
    }

    fn order(&self, status: StatusCode) -> Answer {
        let identifiers = lock(&self.identifiers);
        let certificate = lock(&self.certificate).is_some();
        let status_text = match certificate {
            true => "valid",
            false => "ready",
        };
        let mut order = json!({
            "status": status_text,
            "identifiers": identifiers
                .iter()
                .map(|name| json!({ "type": "dns", "value": name }))
                .collect::<Vec<_>>(),
            "authorizations": (0..identifiers.len())
                .map(|index| self.url(&format!("/authz/{index}")))
                .collect::<Vec<_>>(),
            "finalize": self.url("/finalize"),
        });
        if certificate {
            order["certificate"] = json!(self.url("/certificate"));
        }
        respond_json(status, &self.headers(Some(self.url("/order/1"))), &order)
    }

    fn problem(&self) -> Answer {
        respond_json(
            StatusCode::FORBIDDEN,
            &self.typed_headers("application/problem+json"),
            &json!({
                "type": "urn:ietf:params:acme:error:unauthorized",
                "detail": "the test peer refuses this request",
                "status": 403,
            }),
        )
    }

    async fn hold(&self) {
        self.log.write(|log| log.held += 1);
        self.release.wait().await;
    }

    /// Play `stage` before an answer: the early answer it ends with, or
    /// `None` once the request may be answered.
    async fn staged(&self, stage: Stage) -> Option<Answer> {
        match stage {
            Stage::Refuse => Some(self.problem()),
            Stage::Lose => Some(lost()),
            Stage::Hold => {
                self.hold().await;
                None
            }
            Stage::Answer => None,
        }
    }

    /// The dns-01 challenge at `index`, in `status`.
    fn challenge_object(&self, index: &str, status: &str) -> Value {
        json!({
            "type": "dns-01",
            "url": self.url(&format!("/challenge/{index}")),
            "token": format!("token-{index}"),
            "status": status,
        })
    }
}

async fn acme(shared: Arc<AcmeShared>, request: Request<Incoming>) -> Answer {
    let (method, path, _, body) = read(request).await?;
    let segments = path_segments(&path);
    match (method, segments.as_slice()) {
        (Method::GET, ["directory"]) => respond_json(
            StatusCode::OK,
            &[("content-type", "application/json".to_owned())],
            &json!({
                "newNonce": shared.url("/nonce"),
                "newAccount": shared.url("/account"),
                "newOrder": shared.url("/order"),
            }),
        ),
        (Method::HEAD, ["nonce"]) => respond(StatusCode::OK, &shared.headers(None), Bytes::new()),
        (Method::POST, ["account"]) => respond_json(
            StatusCode::CREATED,
            &shared.headers(Some(shared.url("/account/1"))),
            &json!({ "status": "valid" }),
        ),
        (Method::POST, ["order"]) => {
            let payload = jws_payload(&body)?;
            let identifiers: Vec<String> = payload["identifiers"]
                .as_array()
                .map(|identifiers| {
                    identifiers
                        .iter()
                        .filter_map(|identifier| identifier["value"].as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            *lock(&shared.identifiers) = identifiers;
            // A new order starts unissued: a renewal's second order must
            // answer its own challenges, not inherit the first one's leaf.
            *lock(&shared.certificate) = None;
            shared.order(StatusCode::CREATED)
        }
        (Method::POST, ["order", _]) => shared.order(StatusCode::OK),
        (Method::POST, ["authz", index]) => authorization(&shared, index).await,
        (Method::POST, ["challenge", index]) => challenge(&shared, index).await,
        (Method::POST, ["finalize"]) => finalize(&shared, &body).await,
        (Method::POST, ["certificate"]) => {
            let chain = lock(&shared.certificate).clone().unwrap_or_default();
            respond(
                StatusCode::OK,
                &shared.typed_headers("application/pem-certificate-chain"),
                chain,
            )
        }
        _ => shared.problem(),
    }
}

async fn authorization(shared: &AcmeShared, index: &str) -> Answer {
    shared.log.write(|log| log.authorizations += 1);
    let stage = *lock(&shared.authorization_answer);
    if let Some(answer) = shared.staged(stage).await {
        return answer;
    }
    let (name, status) = shared.authorization_state(index);
    let challenge_status = match status {
        AuthorizationStatus::Valid => "valid",
        _ => "pending",
    };
    respond_json(
        StatusCode::OK,
        &shared.headers(None),
        &json!({
            "identifier": { "type": "dns", "value": name },
            "status": status,
            "challenges": [shared.challenge_object(index, challenge_status)],
        }),
    )
}

async fn challenge(shared: &AcmeShared, index: &str) -> Answer {
    let nth = shared.log.write(|log| {
        log.challenges += 1;
        log.challenges
    });
    if shared.authorization_state(index).1 != AuthorizationStatus::Pending {
        return shared.problem();
    }
    if let Some(answer) = shared.staged(shared.challenge.at(nth)).await {
        return answer;
    }
    respond_json(
        StatusCode::OK,
        &shared.headers(None),
        &shared.challenge_object(index, "processing"),
    )
}

async fn finalize(shared: &AcmeShared, body: &[u8]) -> Answer {
    if let Some(answer) = shared.staged(shared.finalize).await {
        return answer;
    }
    let payload = jws_payload(body)?;
    let csr =
        base64url(payload["csr"].as_str().unwrap_or_default()).map_err(std::io::Error::other)?;
    let (leaf, chain) = shared
        .pki
        .issue(&csr, lock(&shared.identifiers).clone())
        .map_err(std::io::Error::other)?;
    *lock(&shared.certificate) = Some(Bytes::from(chain));
    shared.log.write(|log| log.issued = Some(leaf));
    shared.order(StatusCode::OK)
}

/// The JSON a flattened JWS carries; `null` for a POST-as-GET.
fn jws_payload(body: &[u8]) -> Result<Value, std::io::Error> {
    let jws: Value = serde_json::from_slice(body).map_err(std::io::Error::other)?;
    match jws["payload"].as_str().unwrap_or_default() {
        "" => Ok(Value::Null),
        payload => {
            let decoded = base64url(payload).map_err(std::io::Error::other)?;
            serde_json::from_slice(&decoded).map_err(std::io::Error::other)
        }
    }
}

/// Decode unpadded base64url.
fn base64url(text: &str) -> Result<Vec<u8>, String> {
    let mut decoded = Vec::with_capacity(text.len() * 3 / 4);
    let (mut buffer, mut bits) = (0_u32, 0_u32);
    for byte in text.bytes() {
        let value = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            other => return Err(format!("not base64url: {other:#x}")),
        };
        buffer = (buffer << 6) | u32::from(value);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            decoded.push(u8::try_from((buffer >> bits) & 0xff).map_err(|error| error.to_string())?);
            buffer &= (1 << bits) - 1;
        }
    }
    Ok(decoded)
}

// --- test PKI ------------------------------------------------------------

/// A test root, the `localhost` leaf the ACME peer serves under, and the
/// issuer of every order's leaf.
pub struct Pki {
    issuer: rcgen::Issuer<'static, rcgen::KeyPair>,
    root_pem: Box<str>,
}

fn pki_error(what: &'static str) -> impl Fn(rcgen::Error) -> String {
    move |error| format!("{what}: {error}")
}

/// The days an issued leaf stays valid: past the 30-day renewal threshold.
const ISSUED_DAYS: u32 = 60;

/// A CSR's public key, signed over as it stands.
struct CsrKey(Vec<u8>);

impl rcgen::PublicKeyData for CsrKey {
    fn der_bytes(&self) -> &[u8] {
        &self.0
    }

    fn algorithm(&self) -> &'static rcgen::SignatureAlgorithm {
        // instant-acme generates its order key with rcgen's default.
        &rcgen::PKCS_ECDSA_P256_SHA256
    }
}

impl Pki {
    /// A fresh root named `common_name`.
    ///
    /// # Errors
    ///
    /// When rcgen refuses the root.
    pub fn new(common_name: &str) -> Result<Self, String> {
        let key = rcgen::KeyPair::generate().map_err(pki_error("root key"))?;
        let mut params = rcgen::CertificateParams::new(Vec::new()).map_err(pki_error("root"))?;
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, common_name);
        params.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::CrlSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let root = params.self_signed(&key).map_err(pki_error("root sign"))?;
        Ok(Self {
            root_pem: root.pem().into_boxed_str(),
            issuer: rcgen::Issuer::new(params, key),
        })
    }

    /// The root's PEM, for a client to trust.
    #[must_use]
    pub fn root_pem(&self) -> &str {
        &self.root_pem
    }

    /// A server leaf for `names` over `key`, signed by the root.
    fn leaf(
        &self,
        names: Vec<String>,
        key: &impl rcgen::PublicKeyData,
    ) -> Result<rcgen::Certificate, String> {
        let mut params = rcgen::CertificateParams::new(names).map_err(pki_error("leaf"))?;
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        validity(&mut params, -DAY, i64::from(ISSUED_DAYS) * DAY)?;
        params
            .signed_by(key, &self.issuer)
            .map_err(pki_error("leaf sign"))
    }

    /// The TLS configuration a peer serves `localhost` under.
    ///
    /// # Errors
    ///
    /// When rcgen or rustls refuses the leaf.
    pub fn server_config(&self) -> Result<Arc<rustls::ServerConfig>, String> {
        let key = rcgen::KeyPair::generate().map_err(pki_error("server key"))?;
        let leaf = self.leaf(vec!["localhost".to_owned()], &key)?;
        let certs = CertificateDer::pem_slice_iter(leaf.pem().as_bytes())
            .collect::<Result<Vec<_>, _>>()
            .map_err(|error| format!("server pem: {error}"))?;
        let key = PrivateKeyDer::from_pem_slice(key.serialize_pem().as_bytes())
            .map_err(|error| format!("server key pem: {error}"))?;
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .map_err(|error| format!("versions: {error}"))?
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map(Arc::new)
        .map_err(|error| format!("server config: {error}"))
    }

    /// Sign a leaf for `names` over the key in `csr`, returning the leaf's
    /// DER and the chain an ACME server serves.
    fn issue(&self, csr: &[u8], names: Vec<String>) -> Result<(Vec<u8>, String), String> {
        use x509_parser::prelude::FromDer;
        let (_, request) =
            x509_parser::certification_request::X509CertificationRequest::from_der(csr)
                .map_err(|error| format!("csr: {error}"))?;
        let key = CsrKey(
            AsRef::<[u8]>::as_ref(
                &request
                    .certification_request_info
                    .subject_pki
                    .subject_public_key,
            )
            .to_vec(),
        );
        let leaf = self.leaf(names, &key)?;
        Ok((
            leaf.der().to_vec(),
            format!("{}{}", leaf.pem(), self.root_pem),
        ))
    }
}

/// A self-signed generation for `names`, valid now, as its bundle bytes.
///
/// # Errors
///
/// When rcgen refuses the parameters.
pub fn prior_generation(names: &[&str]) -> Result<String, String> {
    generation_expiring(names, ISSUED_DAYS)
}

/// A self-signed generation for `names`, valid now and until `days` from
/// now, as its bundle bytes. Fewer than 30 days is due for renewal.
///
/// # Errors
///
/// When the clock or rcgen refuses the parameters.
pub fn generation_expiring(names: &[&str], days: u32) -> Result<String, String> {
    leaf(names, -DAY, i64::from(days) * DAY).map(|generation| generation.bundle())
}
