use std::future::Future;
use std::io::Write;
use std::ops::ControlFlow;
use std::path::Path;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use instant_acme::{
    Account, AccountCredentials, ChallengeType, Identifier, LetsEncrypt, NewAccount, NewOrder,
    OrderStatus, RetryPolicy,
};
use rustls::sign::CertifiedKey;
use tokio::task::JoinHandle;

use super::provider::{DnsProvider, RecordId};
use crate::RuntimeError;
use crate::config::AcmeBase;
use crate::runtime_state::LifecycleSignals;
use crate::tls::{CertStore, parse_certified_key};

/// Renew once the cached certificate is within this many days of its assumed
/// expiry. The gap to [`LE_CERT_LIFETIME_DAYS`] — 60 days — is the window in
/// which every renewal attempt happens, so it must stay wide enough to absorb
/// repeated failures at [`RENEWAL_CHECK_INTERVAL`].
const RENEWAL_THRESHOLD_DAYS: i64 = 30;
const RENEWAL_CHECK_INTERVAL: Duration = Duration::from_secs(12 * 60 * 60);
/// ASSUMPTION, not a fact read off the certificate: Let's Encrypt's standard
/// leaf lifetime. `write_expiry` stamps `now + this`, and `needs_renewal`
/// compares that stamp against [`RENEWAL_THRESHOLD_DAYS`] — so the two
/// constants together, not the issued `notAfter`, decide when renewal starts.
///
/// A shorter real lifetime than this would push the first renewal attempt past
/// the certificate's actual expiry. Deriving the true `notAfter` requires
/// parsing the issued leaf, which needs an x509 dependency this crate does not
/// carry.
const LE_CERT_LIFETIME_DAYS: i64 = 90;
const CHALLENGE_TIMEOUT: Duration = Duration::from_secs(300);

/// DNS-01 ACME certificate provisioning via instant-acme.
///
/// Wraps [`AcmeBase`] with the DNS-01-specific provisioning, caching, and
/// renewal logic.
pub struct AcmeDns01 {
    base: AcmeBase,
}

impl AcmeDns01 {
    /// Create a new DNS-01 ACME configuration.
    ///
    /// `tool_name` sets the default cache directory to `~/.config/{tool_name}/certs/`.
    pub fn new(tool_name: &str, domains: impl IntoIterator<Item = impl Into<Box<str>>>) -> Self {
        Self {
            base: AcmeBase::new(tool_name, domains),
        }
    }

    /// Set the contact email for ACME registration.
    pub fn email(mut self, email: impl Into<Box<str>>) -> Self {
        self.base = self.base.email(email);
        self
    }

    /// Set the directory for caching certificates and account keys.
    pub fn cache_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.base = self.base.cache_dir(path);
        self
    }

    /// Use Let's Encrypt staging directory (for testing).
    pub fn staging(mut self, staging: bool) -> Self {
        self.base = self.base.staging(staging);
        self
    }

    /// Return the configured cache directory path.
    pub fn cache_path(&self) -> &Path {
        self.base.cache_path()
    }

    /// Run the full ACME DNS-01 flow: order -> challenge -> TXT record -> finalize -> cert.
    ///
    /// Every `_acme-challenge` record this raises is removed before it returns:
    /// on success, on failure, and when a lifecycle signal stops the order
    /// partway. A stop is reported as an error here, because a caller outside a
    /// renewal loop has no second outcome to act on. The runtime's renewal loop
    /// runs the same flow under its own signals and tells the two apart.
    pub async fn provision_cert<P: DnsProvider>(
        &self,
        provider: &P,
    ) -> Result<CertifiedKey, RuntimeError> {
        match self
            .provision_signalled(provider, &LifecycleSignals::current())
            .await
        {
            ControlFlow::Continue(result) => result,
            ControlFlow::Break(()) => Err(RuntimeError::Acme(
                "certificate provisioning stopped: the runtime is shutting down".into(),
            )),
        }
    }

    /// Run the DNS-01 flow under `signals`, reporting a lifecycle stop as
    /// `Break` rather than as a failed attempt.
    ///
    /// The signals reach the individual awaits inside the flow, never the flow
    /// as a whole. That is what keeps cleanup reachable: the accumulated record
    /// ids and the DELETEs that consume them live in this frame, so a stop
    /// returns *through* the cleanup instead of dropping a future that owns
    /// both. Cleanup itself runs unguarded — one bounded round of DELETEs costs
    /// the drain far less than the `CHALLENGE_TIMEOUT` the guard exists to stop
    /// spending.
    ///
    /// One window stays open: a stop landing while a `create_txt_record` call
    /// is in flight loses the id of that one record, because the provider only
    /// names it in the response. Every record already acknowledged is removed.
    pub(crate) async fn provision_signalled<P: DnsProvider>(
        &self,
        provider: &P,
        signals: &LifecycleSignals,
    ) -> ControlFlow<(), Result<CertifiedKey, RuntimeError>> {
        let mut order = match self.open_order(signals).await {
            Ok(order) => order,
            Err(stop) => return stop.into_flow(),
        };

        let mut created: Vec<RecordId> = Vec::new();
        let outcome = run_guarded_order(&mut order, provider, &mut created, signals).await;

        // Cleanup runs on every exit, over whatever was created before it: a
        // TXT record raised for an order that failed or stopped is a record
        // nothing will ever consume. Leaving it behind lets the 12-hour renewal
        // retry pile a fresh `_acme-challenge` set onto the zone every pass.
        cleanup_txt_records(provider, &created).await;

        match outcome {
            Err(stop) => stop.into_flow(),
            Ok((cert_pem, key_pem)) => {
                self.cache_issued_cert(&cert_pem, &key_pem);
                ControlFlow::Continue(parse_certified_key(cert_pem.as_bytes(), key_pem.as_bytes()))
            }
        }
    }

    /// Register or reuse the ACME account and open an order for the configured
    /// domains.
    ///
    /// Nothing has been raised in the zone yet, so a stop leaves no record to
    /// clean up. State can still be raised at the DIRECTORY, and that is why
    /// the two awaits are not guarded alike. A stop dropped inside
    /// `load_or_create_account` loses the credentials of an account Let's
    /// Encrypt has already registered, and the next 12-hour pass takes the same
    /// `NotFound` branch and registers another — the new-account rate-limit
    /// path `save_new_credentials` exists to avoid. So a stop is refused before
    /// the account step and the step itself runs unguarded: one bounded
    /// directory round trip costs the drain far less than that. Only the order
    /// step, which raises nothing anywhere, is raced.
    async fn open_order(
        &self,
        signals: &LifecycleSignals,
    ) -> Result<instant_acme::Order, ProvisionStop> {
        match signals.is_fired() {
            true => return Err(ProvisionStop::Signalled),
            false => {}
        }
        let account = self.load_or_create_account().await?;
        let identifiers: Box<[Identifier]> = self
            .base
            .domains
            .iter()
            .map(|d| Identifier::Dns(d.to_string()))
            .collect();
        guarded_step(signals, account.new_order(&NewOrder::new(&identifiers))).await
    }

    /// Load a cached certificate from disk if present and not expired.
    pub fn load_cached_cert(&self) -> Result<Option<CertifiedKey>, RuntimeError> {
        let cert_path = self.base.cache_dir.join("cert.pem");
        let key_path = self.base.cache_dir.join("key.pem");

        match cache_io(|| read_cached_pems(&cert_path, &key_path))? {
            Some(pems) => Ok(Some(parse_certified_key(&pems.cert, &pems.key)?)),
            None => Ok(None),
        }
    }

    /// Check if the cached cert needs renewal (expires within 30 days).
    ///
    /// No usable expiry stamp reads as "renew": the alternative is skipping a
    /// renewal the certificate may need. An expiry file that exists but cannot
    /// be read or parsed is reported at warn level rather than swallowed —
    /// that answer repeats on every renewal pass.
    pub fn needs_renewal(&self) -> bool {
        let expiry_path = self.base.cache_dir.join("expiry");
        match cache_io(|| read_expiry_secs(&expiry_path)) {
            Some(expiry_secs) => (expiry_secs - now_unix_secs()) / 86400 < RENEWAL_THRESHOLD_DAYS,
            None => true,
        }
    }

    /// Spawn a background task that renews the cert before expiry and swaps it
    /// into the given CertStore.
    ///
    /// The returned handle is caller-owned: this task is not admitted to the
    /// runtime's root scope, because the scope cannot single-own a handle it
    /// also hands back. The runtime's own renewal is scope-owned instead.
    ///
    /// The task has TWO completion paths, and which ones exist depends on where
    /// it was spawned:
    ///
    /// - inside a Camber runtime, shutdown latches or the root scope closes —
    ///   the loop ends between attempts, or at the first guarded await inside
    ///   an order once its challenge records have been removed, so a 300-second
    ///   challenge cannot hold a 30-second drain open;
    /// - outside a Camber runtime the captured signals are inert, nothing can
    ///   fire them, and the loop renews forever.
    ///
    /// Resolution of this handle therefore does NOT mean renewal failed. A
    /// failed attempt is logged at the attempt and the loop continues to the
    /// next interval; the failure is never carried on the handle. Under Camber
    /// the handle resolves on every clean shutdown.
    ///
    /// # Panics
    ///
    /// Panics if called outside a Tokio runtime context. `tokio::spawn` has no
    /// runtime to admit the loop to and panics before this returns. Call it
    /// from inside `runtime::run`, or from any other Tokio runtime context.
    pub fn spawn_renewal<P: DnsProvider + 'static>(
        self,
        provider: P,
        store: CertStore,
    ) -> JoinHandle<()> {
        tokio::spawn(dns01_renewal_loop(
            self,
            provider,
            store,
            LifecycleSignals::current(),
        ))
    }

    /// Persist a freshly issued certificate, reporting a cache failure instead
    /// of destroying the certificate over it.
    ///
    /// The certificate is valid in memory whether or not the cache accepted it,
    /// and only the swap makes it serveable. Propagating the write error would
    /// turn a full or read-only cache directory into a renewal that "failed":
    /// the process would keep serving the OLD certificate while burning a
    /// Let's Encrypt duplicate-certificate issuance every renewal pass, forever.
    /// Losing the cache costs one re-issue at the next restart instead.
    fn cache_issued_cert(&self, cert_pem: &str, key_pem: &str) {
        if let Err(error) = self.cache_cert(cert_pem, key_pem) {
            tracing::warn!(
                %error,
                "dns01 acme: certificate issued but not cached; it will be re-issued on restart"
            );
        }
    }

    fn cache_cert(&self, cert_pem: &str, key_pem: &str) -> Result<(), RuntimeError> {
        cache_io(|| {
            std::fs::create_dir_all(&self.base.cache_dir)?;
            std::fs::write(self.base.cache_dir.join("cert.pem"), cert_pem)?;
            let key_path = self.base.cache_dir.join("key.pem");
            std::fs::write(&key_path, key_pem)?;
            restrict_key_permissions(&key_path)?;
            write_expiry(&self.base.cache_dir)
        })
    }

    async fn load_or_create_account(&self) -> Result<Account, RuntimeError> {
        let creds_path = self.base.cache_dir.join("account.json");

        match cache_io(|| std::fs::read(&creds_path)) {
            Ok(data) => {
                let creds: AccountCredentials = serde_json::from_slice(&data).map_err(|e| {
                    RuntimeError::Acme(format!("failed to parse account credentials: {e}").into())
                })?;
                Account::builder()
                    .map_err(acme_err)?
                    .from_credentials(creds)
                    .await
                    .map_err(acme_err)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let (account, creds) = create_account(&self.base.email, self.base.staging).await?;
                self.save_new_credentials(&creds);
                Ok(account)
            }
            Err(err) => Err(err.into()),
        }
    }

    /// Persist the credentials of an account that has just been registered,
    /// reporting a failure instead of destroying the account over it.
    ///
    /// Error level, and no propagation: the registration already happened at
    /// the ACME directory and the returned account is usable for this order.
    /// Discarding it over an unwritable cache would send the next 12-hour pass
    /// back down the same `NotFound` branch to register ANOTHER account, and
    /// the pass after that another, into Let's Encrypt's new-account rate
    /// limit.
    fn save_new_credentials(&self, credentials: &AccountCredentials) {
        match self.save_credentials(credentials) {
            Ok(()) => {}
            Err(error) => tracing::error!(
                %error,
                "dns01 acme: account registered but credentials not saved; \
                 the next renewal pass will register a new account"
            ),
        }
    }

    fn save_credentials(&self, credentials: &AccountCredentials) -> Result<(), RuntimeError> {
        let json = serde_json::to_vec(credentials).map_err(|e| {
            RuntimeError::Acme(format!("failed to serialize account credentials: {e}").into())
        })?;
        let account_path = self.base.cache_dir.join("account.json");
        cache_io(|| {
            std::fs::create_dir_all(&self.base.cache_dir)?;
            write_credentials_file(&account_path, &json)
        })
    }
}

pub(crate) fn write_credentials_file(path: &Path, contents: &[u8]) -> Result<(), RuntimeError> {
    let pending = PendingCredentials::create(path)?;
    pending.commit(path, contents)?;
    Ok(())
}

struct PendingCredentials {
    file: std::fs::File,
    path: PathBuf,
    committed: bool,
}

impl PendingCredentials {
    fn create(destination: &Path) -> Result<Self, std::io::Error> {
        let parent = destination.parent().unwrap_or_else(|| Path::new("."));
        let file_name = destination
            .file_name()
            .and_then(std::ffi::OsStr::to_str)
            .unwrap_or("account.json");
        for attempt in 0..16 {
            let path = parent.join(format!(
                ".{file_name}.{:016x}.{attempt}.tmp",
                crate::prng::next_u64(),
            ));
            match open_private_file(&path) {
                Ok(file) => {
                    return Ok(Self {
                        file,
                        path,
                        committed: false,
                    });
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not allocate a unique credentials cache file",
        ))
    }

    fn commit(mut self, destination: &Path, contents: &[u8]) -> Result<(), std::io::Error> {
        self.file.write_all(contents)?;
        self.file.sync_all()?;
        std::fs::rename(&self.path, destination)?;
        self.committed = true;
        sync_parent_directory(destination)?;
        Ok(())
    }
}

impl Drop for PendingCredentials {
    fn drop(&mut self) {
        match self.committed {
            true => {}
            false => remove_pending_credentials(&self.path),
        }
    }
}

fn remove_pending_credentials(path: &Path) {
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            path = %path.display(),
            %error,
            "failed to remove temporary ACME credentials file"
        ),
    }
}

fn open_private_file(path: &Path) -> Result<std::fs::File, std::io::Error> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)
}

#[cfg(unix)]
fn sync_parent_directory(path: &Path) -> Result<(), std::io::Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::File::open(parent)?.sync_all()
}

#[cfg(not(unix))]
fn sync_parent_directory(path: &Path) -> Result<(), std::io::Error> {
    tracing::debug!(
        path = %path.display(),
        "dns01 acme: parent directory sync is unavailable on this platform"
    );
    Ok(())
}

/// Why a provisioning step ended early.
///
/// A lifecycle stop ends the renewal loop; a failure ends only this attempt.
/// Both unwind to the same cleanup point, so the flow carries one type and its
/// caller separates them once, after the zone is clean.
enum ProvisionStop {
    Signalled,
    Failed(RuntimeError),
}

impl ProvisionStop {
    /// Report the stop the way the renewal loop reads it.
    fn into_flow<T>(self) -> ControlFlow<(), Result<T, RuntimeError>> {
        match self {
            Self::Signalled => ControlFlow::Break(()),
            Self::Failed(error) => ControlFlow::Continue(Err(error)),
        }
    }
}

impl From<RuntimeError> for ProvisionStop {
    fn from(error: RuntimeError) -> Self {
        Self::Failed(error)
    }
}

impl From<instant_acme::Error> for ProvisionStop {
    fn from(error: instant_acme::Error) -> Self {
        Self::Failed(acme_err(error))
    }
}

/// Await one step of the order, ending it early when a lifecycle signal fires.
async fn guarded_step<T, E, Fut>(signals: &LifecycleSignals, step: Fut) -> Result<T, ProvisionStop>
where
    Fut: Future<Output = Result<T, E>>,
    ProvisionStop: From<E>,
{
    match signals.guard(step).await {
        ControlFlow::Break(()) => Err(ProvisionStop::Signalled),
        ControlFlow::Continue(result) => result.map_err(ProvisionStop::from),
    }
}

/// Run the two long steps of an open order under `signals`.
///
/// Each is guarded on its own and `txt_records` belongs to the caller, so a
/// stop returns here with every id raised so far still in the caller's hands.
async fn run_guarded_order<P: DnsProvider>(
    order: &mut instant_acme::Order,
    provider: &P,
    txt_records: &mut Vec<RecordId>,
    signals: &LifecycleSignals,
) -> Result<(Box<str>, Box<str>), ProvisionStop> {
    guarded_step(signals, create_dns_challenges(order, provider, txt_records)).await?;
    guarded_step(signals, finalize_order(order)).await
}

/// Run one blocking cache-file operation without stalling the async poll path.
///
/// Every caller below is reachable from public API a user may drive from either
/// runtime flavor — or from no runtime at all — so the flavor check is what
/// keeps a few filesystem syscalls from becoming a panic raised out of library
/// code. `crate::task::block_in_place` is that check; this name records why the
/// cache reaches for it.
fn cache_io<T>(operation: impl FnOnce() -> T) -> T {
    crate::task::block_in_place(operation)
}

/// The cached certificate and its key, as read off disk.
///
/// Boxed slices: the bytes are read once and only ever parsed, and
/// `std::fs::read` sizes its buffer to the file, so freezing it costs nothing.
struct CachedPems {
    cert: Box<[u8]>,
    key: Box<[u8]>,
}

/// Read the cached certificate and key together, or report the pair absent.
///
/// Both halves are required: a certificate without its key parses into nothing
/// usable, so a half-written cache reads as no cache.
fn read_cached_pems(cert_path: &Path, key_path: &Path) -> Result<Option<CachedPems>, RuntimeError> {
    match (cert_path.exists(), key_path.exists()) {
        (true, true) => Ok(Some(CachedPems {
            cert: std::fs::read(cert_path)?.into_boxed_slice(),
            key: std::fs::read(key_path)?.into_boxed_slice(),
        })),
        _ => Ok(None),
    }
}

/// Renew the cert before expiry and swap it into `store`, until a lifecycle
/// signal ends the loop.
///
/// Both awaits break on the signals. The renewal interval is measured in
/// hours, so without `tick` a scope-owned renewal would hold the drain open for
/// half a day. An order in flight stops for the same reason at a smaller scale:
/// `CHALLENGE_TIMEOUT` is 300s against a 30s shutdown timeout, so an unstoppable
/// renewal would turn a clean shutdown into a scope drain timeout. The signals
/// go *into* `provision_signalled` rather than around it, so the order stops at
/// one of its own awaits and still removes the `_acme-challenge` records it
/// raised — a dropped order future would strand them in the zone.
pub(crate) async fn dns01_renewal_loop<P: DnsProvider + 'static>(
    acme: AcmeDns01,
    provider: P,
    store: CertStore,
    signals: LifecycleSignals,
) {
    while let ControlFlow::Continue(()) = signals.tick(RENEWAL_CHECK_INTERVAL).await {
        match acme.needs_renewal() {
            false => continue,
            true => {}
        }

        tracing::info!("dns01 acme: cert renewal triggered");
        match acme.provision_signalled(&provider, &signals).await {
            ControlFlow::Break(()) => return,
            ControlFlow::Continue(Ok(new_cert)) => {
                store.swap(new_cert);
                tracing::info!("dns01 acme: cert renewed and swapped");
            }
            // The cause is a structured FIELD, not part of the message: one
            // condition with an interpolated cause becomes a distinct message
            // string per failure, which is what an operator filters on.
            ControlFlow::Continue(Err(error)) => {
                tracing::warn!(%error, "dns01 acme: renewal failed");
            }
        }
    }
}

async fn create_account(
    email: &Option<Box<str>>,
    staging: bool,
) -> Result<(Account, AccountCredentials), RuntimeError> {
    let contact_str: Option<String> = email.as_ref().map(|e| format!("mailto:{e}"));
    // `Option::as_slice` views the option's own storage as a 0- or 1-element
    // slice, so the contact list `NewAccount` borrows costs no allocation.
    let contact = contact_str.as_deref();
    let new_account = NewAccount {
        contact: contact.as_slice(),
        terms_of_service_agreed: true,
        only_return_existing: false,
    };
    let url = match staging {
        true => LetsEncrypt::Staging.url(),
        false => LetsEncrypt::Production.url(),
    };
    Account::builder()
        .map_err(acme_err)?
        .create(&new_account, url.into(), None)
        .await
        .map_err(acme_err)
}

/// Raise one `_acme-challenge` TXT record per authorization, recording each id
/// in `txt_records` as it is created.
///
/// The accumulator belongs to the caller precisely because this can fail
/// partway: every id already written to it names a live record the caller must
/// still clean up, error or not.
async fn create_dns_challenges<P: DnsProvider>(
    order: &mut instant_acme::Order,
    provider: &P,
    txt_records: &mut Vec<RecordId>,
) -> Result<(), RuntimeError> {
    let mut auths = order.authorizations();

    while let Some(auth_result) = auths.next().await {
        let mut auth = auth_result.map_err(acme_err)?;
        let mut challenge = auth
            .challenge(ChallengeType::Dns01)
            .ok_or_else(|| RuntimeError::Acme("no DNS-01 challenge offered".into()))?;

        let fqdn = format!("_acme-challenge.{}", challenge.identifier());
        let dns_value = challenge.key_authorization().dns_value();

        let record_id = provider.create_txt_record(&fqdn, &dns_value).await?;
        txt_records.push(record_id);

        challenge.set_ready().await.map_err(acme_err)?;
    }

    Ok(())
}

async fn finalize_order(
    order: &mut instant_acme::Order,
) -> Result<(Box<str>, Box<str>), RuntimeError> {
    let retry = RetryPolicy::new().timeout(CHALLENGE_TIMEOUT);
    let status = order.poll_ready(&retry).await.map_err(acme_err)?;
    match status {
        OrderStatus::Ready => {}
        other => {
            return Err(RuntimeError::Acme(
                format!("order in unexpected state: {other:?}").into(),
            ));
        }
    }

    let key_pem: Box<str> = order.finalize().await.map_err(acme_err)?.into();
    let cert_pem: Box<str> = order
        .poll_certificate(&retry)
        .await
        .map_err(acme_err)?
        .into();
    Ok((cert_pem, key_pem))
}

/// Remove every `_acme-challenge` record an order raised, reporting each one
/// that could not be removed.
///
/// The record id and the cause are structured FIELDS. Interpolating them would
/// give one condition a distinct message string per record, which is what an
/// operator filters and counts on when a zone is left with stranded challenge
/// records.
async fn cleanup_txt_records<P: DnsProvider>(provider: &P, record_ids: &[RecordId]) {
    for id in record_ids {
        if let Err(error) = provider.delete_txt_record(id).await {
            tracing::warn!(record = %id, %error, "dns01 acme: TXT record cleanup failed");
        }
    }
}

/// Read the cached certificate's expiry stamp, or report why it is unusable.
///
/// A missing stamp is the ordinary "nothing cached yet" answer and stays
/// silent. Every other failure — an unreadable file, contents that are not a
/// Unix timestamp — makes each 12-hour pass provision a fresh certificate and
/// walk into the Let's Encrypt duplicate-certificate rate limit. That is an
/// operator-visible fault, so it is reported rather than collapsed into the
/// same silent `None` as the absent file.
fn read_expiry_secs(path: &Path) -> Option<i64> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "dns01 acme: cert expiry unreadable");
            return None;
        }
    };

    match contents.trim().parse::<i64>() {
        Ok(secs) => Some(secs),
        Err(e) => {
            tracing::warn!(path = %path.display(), error = %e, "dns01 acme: cert expiry malformed");
            None
        }
    }
}

fn write_expiry(cache_dir: &Path) -> Result<(), RuntimeError> {
    let expiry = now_unix_secs() + (LE_CERT_LIFETIME_DAYS * 86400);
    std::fs::write(cache_dir.join("expiry"), expiry.to_string())?;
    Ok(())
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Set file permissions to 0600 (owner read/write only) for private key files.
#[cfg(unix)]
fn restrict_key_permissions(path: &Path) -> Result<(), RuntimeError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms)?;
    Ok(())
}

/// Report the file left at default permissions. Windows ACLs require a
/// different approach, so the path is named rather than silently ignored.
#[cfg(not(unix))]
fn restrict_key_permissions(path: &Path) -> Result<(), RuntimeError> {
    tracing::debug!(
        path = %path.display(),
        "dns01 acme: key permissions left at platform default"
    );
    Ok(())
}

fn acme_err(e: instant_acme::Error) -> RuntimeError {
    RuntimeError::Acme(format!("{e}").into())
}
