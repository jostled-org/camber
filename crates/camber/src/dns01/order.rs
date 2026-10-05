//! One DNS-01 order, run by the owner provisioning admitted.
//!
//! The order owns a validated snapshot of its configuration and borrows the
//! owner's provider for its whole length. Its deadline is fixed when it
//! starts; preparation, every ACME request, and every provider call wait at
//! most what remains of it, and no step restarts it. Preparation authorizes
//! the complete domain set before any TXT record is written.
//!
//! A stop — a lifecycle signal, or a direct waiter's drop — reaches the
//! individual awaits, never the order as a whole. That is what keeps cleanup
//! reachable: a stop returns through the cleanup instead of dropping a future
//! that owns it. Every create intention and acknowledged record ID lives in
//! the operation's cleanup register, which the report slot shares, so even a
//! forced stop that drops this frame leaves the unresolved records charged.
//!
//! The order's result is fixed before cleanup starts, and it is answered only
//! once cleanup settles. A certificate is published only when cleanup owes
//! nothing and the cache holds the validated generation.
//!
//! Its nested operations report their own terminals under the owner's
//! instance: one `ZoneLookup` for the whole preparation, however many zones it
//! queries, one per record create and delete, and one `CacheWrite` for the
//! publication.

use std::future::Future;
use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use instant_acme::{
    Account, AccountCredentials, AuthorizationStatus, ChallengeType, Identifier, NewAccount,
    NewOrder, OrderStatus, RetryPolicy,
};
use rustls::pki_types::CertificateDer;
use rustls::sign::CertifiedKey;
use serde::Deserialize;
use tokio::time::Instant;

use super::acme_progress::AcmeProgress;
use super::acme_transport::AcmeTransport;
use super::cache::{
    cache_io, invalid_certificate, publish_generation, read_optional, write_cache_file,
};
use super::certificate::{bundle_of, unix_now, validate};
use super::cleanup::{clean_up, settle};
use super::failure::{cancelled, failure, invalid_config, rejected};
use super::intention::create_record;
use super::provider::{DnsProvider, challenge_name};
use super::publication::Fault;
use crate::integration_lifecycle::{CleanupRegister, NestedTerminals};
use crate::lifecycle::{AggregateShutdown, ShutdownOwner};
use crate::runtime_state::{LatchSignal, LifecycleSignals, tick_until};
use crate::{
    IntegrationError, IntegrationFailure, IntegrationOperation, Retryability, RuntimeError,
};

/// The file in the cache directory that holds the ACME account credentials.
const ACCOUNT_FILE: &str = "account.json";

/// Publish serialized account credentials through the production cache path.
pub(crate) fn write_account_credentials(
    cache_dir: &Path,
    json: &[u8],
) -> Result<(), IntegrationError> {
    cache_io(|| write_cache_file(cache_dir, ACCOUNT_FILE, json, None))
}

/// The validated configuration one owner's orders run under.
///
/// Owned outright: it borrows neither the waiter nor the `AcmeDns01` it was
/// taken from, so either can be dropped while an order runs.
pub(super) struct OrderPlan {
    pub(super) domains: Arc<[Arc<str>]>,
    pub(super) cache_dir: Box<Path>,
    pub(super) email: Option<Box<str>>,
    pub(super) directory: Arc<str>,
    pub(super) roots: Arc<[CertificateDer<'static>]>,
    pub(super) operation_timeout: Duration,
    pub(super) cleanup_timeout: Duration,
}

/// What asks an owner to stop: its runtime's lifecycle signals, and the
/// caller's own request when it has one — a direct waiter's drop, or a
/// renewal handle's cancel.
#[derive(Clone)]
pub(super) struct StopRequest {
    signals: LifecycleSignals,
    caller: Option<LatchSignal>,
}

impl StopRequest {
    /// A stop under `signals`, which `caller` can also request.
    pub(super) const fn new(signals: LifecycleSignals, caller: Option<LatchSignal>) -> Self {
        Self { signals, caller }
    }

    /// Whether the caller, not the runtime, asked for the stop.
    pub(super) fn by_caller(&self) -> bool {
        self.caller.as_ref().is_some_and(LatchSignal::is_fired)
    }

    /// Whether a stop has been asked for.
    pub(super) fn is_requested(&self) -> bool {
        self.signals.is_fired() || self.by_caller()
    }

    /// Resolve once a stop is asked for.
    async fn requested(&self) {
        match &self.caller {
            Some(caller) => tokio::select! {
                () = self.signals.wait() => {}
                () = caller.wait() => {}
            },
            None => self.signals.wait().await,
        }
    }

    /// Wait for `elapsed`, or break as soon as a stop is asked for.
    pub(super) async fn tick(&self, elapsed: impl Future<Output = ()>) -> ControlFlow<()> {
        tick_until(elapsed, self.requested(), || self.is_requested()).await
    }
}

/// What ends an order early: its owner's [`StopRequest`]. Once a lifecycle
/// stop fixes the runtime's aggregate expiry, that expiry also cuts the
/// order's cleanup short.
pub(super) struct OrderStop {
    request: StopRequest,
    shutdown: Arc<AggregateShutdown>,
    owner: ShutdownOwner,
}

impl OrderStop {
    /// The stop of one order of `owner`, under `request` and its runtime's
    /// `shutdown`.
    pub(super) const fn new(
        request: StopRequest,
        shutdown: Arc<AggregateShutdown>,
        owner: ShutdownOwner,
    ) -> Self {
        Self {
            request,
            shutdown,
            owner,
        }
    }

    /// Resolve once a lifecycle stop has left less than `cap` of the
    /// runtime's aggregate expiry: the point past which cleanup cannot wait.
    ///
    /// A caller's stop is not a runtime stop, so it leaves the cap alone. The
    /// expiry is read under the owner's name, and a stop that has not fixed
    /// one yet narrows nothing; the forced settlement covers what is left.
    pub(super) async fn shutdown_bound(&self, cap: Duration) {
        self.request.signals.wait().await;
        tokio::time::sleep(self.shutdown.bounded(&self.owner, cap)).await;
    }

    /// Refuse to start `operation` once a stop has been asked for.
    ///
    /// # Errors
    ///
    /// `operation`'s `Cancelled`, carrying `interrupted` as its retryability.
    fn refuse_stopped(
        &self,
        operation: IntegrationOperation,
        interrupted: Retryability,
    ) -> Result<(), IntegrationError> {
        match self.request.is_requested() {
            true => Err(cancelled(operation, interrupted)),
            false => Ok(()),
        }
    }

    /// Await `step` until `deadline` or a stop, whichever comes first.
    ///
    /// An interrupted step is `operation`'s `Timeout` or `Cancelled`, carrying
    /// `interrupted` as its retryability.
    async fn step<T>(
        &self,
        operation: IntegrationOperation,
        interrupted: impl Fn() -> Retryability,
        deadline: Instant,
        step: impl Future<Output = Result<T, IntegrationError>>,
    ) -> Result<T, IntegrationError> {
        self.refuse_stopped(operation, interrupted())?;
        tokio::select! {
            biased;
            () = self.request.requested() => Err(cancelled(operation, interrupted())),
            outcome = tokio::time::timeout_at(deadline, step) => outcome.unwrap_or_else(|_| Err(failure(
                operation,
                IntegrationFailure::Timeout,
                interrupted(),
            ))),
        }
    }
}

/// Run one order for `plan` through `provider`, ending in a validated,
/// published certificate.
///
/// Every record the order meant to create is kept in `records`, and every
/// nested operation reports to `terminals`.
///
/// # Errors
///
/// `CleanupIncomplete` naming every record cleanup could not resolve, with
/// the order's own failure as its source when it failed. Otherwise the typed
/// failure of the step that ended the order, or of the cache publication.
pub(super) async fn run_order<P: DnsProvider>(
    plan: &OrderPlan,
    provider: &mut P,
    stop: &OrderStop,
    records: &CleanupRegister,
    terminals: &NestedTerminals,
) -> Result<CertifiedKey, IntegrationError> {
    let deadline = Instant::now() + plan.operation_timeout;
    let lookup = terminals.begin(IntegrationOperation::ZoneLookup);
    let prepared = stop
        .step(
            IntegrationOperation::ZoneLookup,
            || Retryability::Safe,
            deadline,
            prepare(provider, &plan.domains),
        )
        .await;
    terminals.settle(lookup, &prepared);
    prepared?;
    let mut order = open_order(plan, stop, deadline).await?;
    let issued = issue(&mut order, &*provider, records, terminals, stop, deadline).await;
    // Cleanup runs on every exit, over every record the order meant to
    // create: a TXT record raised for an order that failed or stopped is a
    // record nothing will ever consume.
    clean_up(&*provider, records, terminals, plan.cleanup_timeout, stop).await;
    let (cert_pem, key_pem) = settle(issued, records)?;
    let issued = publish_issued(
        &plan.cache_dir,
        &plan.domains,
        &cert_pem,
        &key_pem,
        None,
        |publication| terminals.run(IntegrationOperation::CacheWrite, publication),
    )?;
    issued.cache.map(|()| issued.key)
}

/// Call the provider's preparation for the complete domain set.
async fn prepare<P: DnsProvider>(
    provider: &mut P,
    domains: &[Arc<str>],
) -> Result<(), IntegrationError> {
    provider
        .prepare(domains)
        .await
        .map_err(|error| provider_failure(IntegrationOperation::ZoneLookup, error))
}

/// The SDK order and the request fact its cancellation path reads.
struct AcmeOrder {
    inner: instant_acme::Order,
    progress: AcmeProgress,
}

/// Register or reuse the ACME account and open an order for the plan's
/// domains.
///
/// Nothing has been raised in the zone yet, so a stop leaves no record to
/// clean up. State can still be raised at the directory, and that is why the
/// two awaits are not guarded alike. A stop dropped inside the account step
/// loses the credentials of an account the directory has already registered,
/// and the next pass registers another — the new-account rate-limit path
/// `save_new_credentials` exists to avoid. So a stop is refused before the
/// account step and the step itself is bounded only by the order's deadline.
/// The transport owns that bound: every request the step sends ends by the
/// deadline, and one sent after it fails at once.
async fn open_order(
    plan: &OrderPlan,
    stop: &OrderStop,
    deadline: Instant,
) -> Result<AcmeOrder, IntegrationError> {
    stop.refuse_stopped(IntegrationOperation::Provision, Retryability::Safe)?;
    let progress = AcmeProgress::default();
    let transport =
        AcmeTransport::new(&plan.roots, deadline, progress.clone()).map_err(|error| {
            invalid_config(IntegrationOperation::Provision).with_source(Arc::new(error))
        })?;
    let account = load_or_create_account(plan, transport, &progress).await?;
    let identifiers: Box<[Identifier]> = plan
        .domains
        .iter()
        .map(|domain| Identifier::Dns(domain.to_string()))
        .collect();
    let inner = stop
        .step(
            IntegrationOperation::Provision,
            || progress.retryability(),
            deadline,
            async { progress.finish(account.new_order(&NewOrder::new(&identifiers)).await) },
        )
        .await?;
    Ok(AcmeOrder { inner, progress })
}

/// Raise the challenges and finalize, each step bounded and stoppable.
///
/// `records` outlives both steps, so a stop returns here with every record
/// raised so far still in the caller's hands. Cleanup answers for TXT records;
/// it cannot resolve an uncertain write at the ACME directory.
async fn issue<P: DnsProvider>(
    order: &mut AcmeOrder,
    provider: &P,
    records: &CleanupRegister,
    terminals: &NestedTerminals,
    stop: &OrderStop,
    deadline: Instant,
) -> Result<(Box<str>, Box<str>), IntegrationError> {
    stop.step(
        IntegrationOperation::Provision,
        || order.progress.retryability(),
        deadline,
        create_dns_challenges(
            &mut order.inner,
            provider,
            records,
            terminals,
            &order.progress,
        ),
    )
    .await?;
    stop.step(
        IntegrationOperation::Provision,
        || order.progress.retryability(),
        deadline,
        finalize_order(&mut order.inner, deadline, &order.progress),
    )
    .await
}

/// Reuse the cached account registered on the plan's directory, or register
/// one there and cache it.
async fn load_or_create_account(
    plan: &OrderPlan,
    transport: AcmeTransport,
    progress: &AcmeProgress,
) -> Result<Account, IntegrationError> {
    let builder = Account::builder_with_http(Box::new(transport));
    match stored_credentials(plan)? {
        Some(creds) => progress.finish(builder.from_credentials(creds).await),
        None => {
            let contact = plan.email.as_ref().map(|email| format!("mailto:{email}"));
            let contact = contact.as_deref();
            let new_account = NewAccount {
                contact: contact.as_slice(),
                terms_of_service_agreed: true,
                only_return_existing: false,
            };
            let (account, creds) = progress.finish(
                builder
                    .create(&new_account, plan.directory.to_string(), None)
                    .await,
            )?;
            save_new_credentials(&plan.cache_dir, &creds);
            Ok(account)
        }
    }
}

/// The directory a cached account file names; the credentials keep it
/// private, so it is read from the same bytes.
#[derive(Deserialize)]
struct StoredDirectory {
    directory: Option<Box<str>>,
}

/// The cached account credentials, when they were registered on the plan's
/// directory.
///
/// `instant_acme` connects a stored account to the directory its credentials
/// name, so credentials of another directory — or of none, as older files
/// were written — would silently override the configuration. They read as
/// absent, and the order registers on the configured directory, replacing
/// the file.
///
/// # Errors
///
/// `CacheRead`: the typed I/O failure when the file exists but cannot be
/// read, and `Rejected` when its contents are not account credentials.
fn stored_credentials(plan: &OrderPlan) -> Result<Option<AccountCredentials>, IntegrationError> {
    let Some(data) = cache_io(|| read_optional(&plan.cache_dir.join(ACCOUNT_FILE)))? else {
        return Ok(None);
    };
    // Unreadable cached credentials are corrupt state, not a configuration
    // the caller wrote: the cache refuses them.
    let corrupt = |error: serde_json::Error| {
        rejected(IntegrationOperation::CacheRead).with_source(Arc::new(error))
    };
    let creds: AccountCredentials = serde_json::from_slice(&data).map_err(corrupt)?;
    let stored: StoredDirectory = serde_json::from_slice(&data).map_err(corrupt)?;
    match stored.directory.as_deref() == Some(&*plan.directory) {
        true => Ok(Some(creds)),
        false => {
            tracing::info!(
                stored = stored.directory.as_deref().unwrap_or("none"),
                configured = %plan.directory,
                "dns01 acme: cached account belongs to another directory; \
                 registering on the configured one"
            );
            Ok(None)
        }
    }
}

/// Persist the credentials of an account that has just been registered,
/// reporting a failure instead of destroying the account over it.
///
/// Error level, and no propagation: the registration already happened at the
/// directory and the returned account is usable for this order. Discarding it
/// over an unwritable cache would send the next pass back to register another
/// account, and the pass after that another, into the new-account rate limit.
fn save_new_credentials(cache_dir: &Path, credentials: &AccountCredentials) {
    if let Err(error) = save_credentials(cache_dir, credentials) {
        tracing::error!(
            %error,
            "dns01 acme: account registered but credentials not saved; \
             the next renewal pass will register a new account"
        );
    }
}

/// Write `credentials` to the account file in `cache_dir`.
///
/// # Errors
///
/// `CacheWrite`: `Rejected` when the credentials do not serialize, and the
/// typed I/O failure when the file could not be written.
fn save_credentials(
    cache_dir: &Path,
    credentials: &AccountCredentials,
) -> Result<(), IntegrationError> {
    let json = serde_json::to_vec(credentials)
        .map_err(|error| rejected(IntegrationOperation::CacheWrite).with_source(Arc::new(error)))?;
    write_account_credentials(cache_dir, &json)
}

/// Raise one `_acme-challenge` TXT record per pending authorization, keeping each one
/// in `records` from before its create is sent.
///
/// The register belongs to the caller precisely because this can fail
/// partway: every record already in it may be live in the zone, and the caller
/// must still clean it up, error or not. A wildcard authorization is proven
/// under its base name's challenge label.
async fn create_dns_challenges<P: DnsProvider>(
    order: &mut instant_acme::Order,
    provider: &P,
    records: &CleanupRegister,
    terminals: &NestedTerminals,
    progress: &AcmeProgress,
) -> Result<(), IntegrationError> {
    let mut auths = order.authorizations();
    while let Some(auth_result) = auths.next().await {
        let mut auth = progress.finish(auth_result)?;
        match auth.status {
            AuthorizationStatus::Valid => continue,
            AuthorizationStatus::Pending => {}
            AuthorizationStatus::Invalid
            | AuthorizationStatus::Revoked
            | AuthorizationStatus::Expired
            | AuthorizationStatus::Deactivated => {
                return Err(rejected(IntegrationOperation::Provision));
            }
        }
        let mut challenge = auth
            .challenge(ChallengeType::Dns01)
            .ok_or(rejected(IntegrationOperation::Provision))?;
        let Identifier::Dns(name) = challenge.identifier().identifier else {
            return Err(rejected(IntegrationOperation::Provision));
        };
        let fqdn = challenge_name(name);
        let dns_value = challenge.key_authorization().dns_value();
        create_record(provider, records, terminals, name, &fqdn, &dns_value).await?;
        progress.finish(challenge.set_ready().await)?;
    }
    Ok(())
}

async fn finalize_order(
    order: &mut instant_acme::Order,
    deadline: Instant,
    progress: &AcmeProgress,
) -> Result<(Box<str>, Box<str>), IntegrationError> {
    let retry = RetryPolicy::new().timeout(deadline.saturating_duration_since(Instant::now()));
    match progress.finish(order.poll_ready(&retry).await)? {
        OrderStatus::Ready => {}
        _ => return Err(rejected(IntegrationOperation::Provision)),
    }
    let key_pem: Box<str> = progress.finish(order.finalize().await)?.into();
    let cert_pem: Box<str> = progress
        .finish(order.poll_certificate(&retry).await)?
        .into();
    Ok((cert_pem, key_pem))
}

/// Validate an issued generation against `domains` and publish it to the
/// cache in `cache_dir`, failing at `fault`. `publish` runs the publication,
/// so a caller can report it as one operation.
///
/// The one path issuance and the publication fault probe share, so the probe
/// can neither skip validation nor write another format.
///
/// # Errors
///
/// `Provision` with `InvalidCertificate` when the issued generation fails
/// validation; nothing is published. A publication failure is not an error
/// here: it is carried on [`Issued`], because the key is valid either way.
pub(super) fn publish_issued(
    cache_dir: &Path,
    domains: &[Arc<str>],
    cert_pem: &str,
    key_pem: &str,
    fault: Fault,
    publish: impl FnOnce(&dyn Fn() -> Result<(), IntegrationError>) -> Result<(), IntegrationError>,
) -> Result<Issued, IntegrationError> {
    let bundle = bundle_of(cert_pem.as_bytes(), key_pem.as_bytes());
    let generation = validate(bundle, domains, unix_now())
        .map_err(|refusal| invalid_certificate(IntegrationOperation::Provision, refusal))?;
    let cache = publish(&|| cache_io(|| publish_generation(cache_dir, &generation, fault)));
    Ok(Issued {
        key: generation.into_key(),
        cache,
    })
}

/// An issued generation that passed validation, and the outcome of publishing
/// it to the cache.
///
/// Only a published key is served: on a `CacheWrite` failure the prior
/// generation stays on disk and the prior certificate stays served, so a
/// restart never finds a generation older than the one being served.
pub(crate) struct Issued {
    key: CertifiedKey,
    pub(crate) cache: Result<(), IntegrationError>,
}

/// Carry a provider callback's failure as `operation`'s integration failure.
///
/// A provider that reports a typed integration failure keeps it; any other
/// error is a refusal that keeps the provider's error as its source.
pub(super) fn provider_failure(
    operation: IntegrationOperation,
    error: RuntimeError,
) -> IntegrationError {
    match error {
        RuntimeError::Integration(typed) => Arc::unwrap_or_clone(typed),
        other => rejected(operation).with_source(Arc::new(other)),
    }
}
