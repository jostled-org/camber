use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use instant_acme::LetsEncrypt;
use rustls::pki_types::CertificateDer;
use rustls::pki_types::pem::PemObject;
use rustls::sign::CertifiedKey;

use super::cache::{read_cached, renewal_needed};
use super::certificate::Generation;
use super::failure;
use super::order::{Issued, OrderPlan, publish_issued};
use super::owner::{Dns01Owner, admitted};
use super::provider::DnsProvider;
use super::publication::Fault;
use super::transport;
use crate::config::{AcmeBase, Challenge};
use crate::error::valid_duration;
use crate::integration_lifecycle::integration;
use crate::runtime_state::LifecycleSignals;
use crate::task::AsyncJoinHandle;
use crate::tls::CertStore;
use crate::{IntegrationOperation, RuntimeError};

/// The default bound on one whole order.
const DEFAULT_OPERATION_TIMEOUT: Duration = Duration::from_secs(300);
/// The default bound on one order's challenge cleanup.
const DEFAULT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

/// DNS-01 ACME certificate provisioning via instant-acme.
///
/// Wraps [`AcmeBase`] with the DNS-01-specific provisioning, caching, and
/// renewal logic. Configuration is local and inert: nothing is validated
/// against a network, and nothing runs, until provisioning admits it.
pub struct AcmeDns01 {
    base: AcmeBase,
    /// The ACME directory, when it is not Let's Encrypt's.
    directory: Option<Arc<str>>,
    /// Roots trusted for the directory in addition to the platform's.
    roots: Arc<[CertificateDer<'static>]>,
    operation_timeout: Duration,
    cleanup_timeout: Duration,
}

impl AcmeDns01 {
    /// Create a new DNS-01 ACME configuration.
    ///
    /// `tool_name` sets the default cache directory to `~/.config/{tool_name}/certs/`.
    pub fn new(tool_name: &str, domains: impl IntoIterator<Item = impl Into<Box<str>>>) -> Self {
        Self {
            base: AcmeBase::new(tool_name, domains),
            directory: None,
            roots: Arc::default(),
            operation_timeout: DEFAULT_OPERATION_TIMEOUT,
            cleanup_timeout: DEFAULT_CLEANUP_TIMEOUT,
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
    ///
    /// A [`AcmeDns01::directory_url`] takes precedence.
    pub fn staging(mut self, staging: bool) -> Self {
        self.base = self.base.staging(staging);
        self
    }

    /// Bound one whole order: preparation, every ACME and provider request,
    /// and issuance. Default 300 seconds.
    ///
    /// Checked when provisioning admits the configuration: it must be
    /// positive and at most 24 hours.
    #[must_use]
    pub const fn operation_timeout(mut self, timeout: Duration) -> Self {
        self.operation_timeout = timeout;
        self
    }

    /// Bound one order's challenge cleanup, which starts once the order's
    /// result is fixed. Default 5 seconds.
    ///
    /// Checked when provisioning admits the configuration: it must be
    /// positive and at most 24 hours.
    #[must_use]
    pub const fn cleanup_timeout(mut self, timeout: Duration) -> Self {
        self.cleanup_timeout = timeout;
        self
    }

    /// Use the ACME directory at `url` instead of Let's Encrypt's.
    ///
    /// # Errors
    ///
    /// `Provision` with `InvalidConfig` unless `url` is an absolute `https`
    /// URL with a host and no credentials or fragment.
    pub fn directory_url(mut self, url: &str) -> Result<Self, RuntimeError> {
        match valid_directory(url) {
            true => {
                self.directory = Some(url.into());
                Ok(self)
            }
            false => Err(invalid_config()),
        }
    }

    /// Trust every PEM certificate in `pem` as a root for this instance's
    /// directory, in addition to the platform roots.
    ///
    /// Roots are additive. They never disable certificate or hostname
    /// verification.
    ///
    /// # Errors
    ///
    /// `Provision` with `InvalidConfig` when `pem` holds no certificate or
    /// one that does not parse.
    pub fn add_root_certificate(mut self, pem: &[u8]) -> Result<Self, RuntimeError> {
        let parsed = parse_roots(pem).ok_or_else(invalid_config)?;
        self.roots = self
            .roots
            .iter()
            .cloned()
            .chain(parsed.into_vec())
            .collect();
        Ok(self)
    }

    /// Return the configured cache directory path.
    pub fn cache_path(&self) -> &Path {
        self.base.cache_path()
    }

    /// Validate the complete configuration for a DNS-01 order.
    ///
    /// Performs no I/O. An empty set, more than 100 domains, a malformed name,
    /// an IP literal, a wildcard that is not the whole leftmost label, or a
    /// name that repeats another after canonicalization returns
    /// `RuntimeError::Config`. A bound that is zero or over 24 hours returns
    /// `Provision` with `InvalidConfig`. Call it before loading provider
    /// credentials; provisioning repeats it before any effect.
    pub fn validate(&self) -> Result<(), RuntimeError> {
        self.plan().map(drop)
    }

    /// Run the full ACME DNS-01 flow under the current runtime: prepare the
    /// provider, order, answer each challenge, finalize, and remove every
    /// challenge record.
    ///
    /// The configuration is validated first, as [`AcmeDns01::validate`]
    /// does; a refused configuration touches no credential, cache, or
    /// provider. Admission follows, before any effect. The admitted owner then
    /// takes `provider` and a snapshot of this configuration: neither borrows
    /// the caller, so dropping this future, or this value, requests
    /// cancellation without dropping the provider before the order settles.
    ///
    /// The provider is prepared for the complete domain set before any TXT
    /// record is written. Every challenge record the order raises is deleted
    /// by its exact ID, or named, before it settles: on success, on failure,
    /// and on a stop. The certificate is published only when cleanup owes
    /// nothing and the cache holds the validated generation.
    ///
    /// # Errors
    ///
    /// `Config` or `InvalidConfig` for a refused configuration; `NoRuntime`
    /// outside a Camber runtime, `ScopeClosed` once its admission closed, and
    /// `Busy` at the live limit or a full report budget, all before any
    /// effect. After admission, `CleanupIncomplete` when a challenge record
    /// could not be deleted or its create lost its answer; its `cleanup()`
    /// names each such record, and the order's own failure, if any, is its
    /// source. Otherwise the typed failure of the step that ended the order:
    /// `ZoneLookup` for preparation, `CreateTxt` for a challenge record,
    /// `Provision` for the ACME exchange, and `CacheWrite` for publication. A
    /// runtime stop is `Cancelled`.
    pub async fn provision_cert<P: DnsProvider + 'static>(
        &self,
        provider: P,
    ) -> Result<CertifiedKey, RuntimeError> {
        let admission = IntegrationOperation::Provision;
        let (owner, signals) = admitted(admission, || {
            let plan = self.plan()?;
            let runtime = crate::runtime::runtime_context()?;
            let owner = Dns01Owner::admit(&runtime, plan, provider, admission)?;
            Ok((owner, LifecycleSignals::from_runtime(&runtime)))
        })?;
        owner.provision(signals)?.wait().await
    }

    /// The validated snapshot one owner's orders run under.
    ///
    /// # Errors
    ///
    /// `Config` for a refused domain set, and `Provision` with
    /// `InvalidConfig` for a refused bound.
    pub(super) fn plan(&self) -> Result<OrderPlan, RuntimeError> {
        let domains = self.validated_domains()?;
        match (
            valid_duration(self.operation_timeout),
            valid_duration(self.cleanup_timeout),
        ) {
            (true, true) => {}
            _ => return Err(invalid_config()),
        }
        Ok(OrderPlan {
            domains,
            cache_dir: self.base.cache_dir.clone().into_boxed_path(),
            email: self.base.email.clone(),
            directory: self
                .directory
                .clone()
                .unwrap_or_else(|| self.lets_encrypt()),
            roots: Arc::clone(&self.roots),
            operation_timeout: self.operation_timeout,
            cleanup_timeout: self.cleanup_timeout,
        })
    }

    /// The Let's Encrypt directory the staging flag selects.
    fn lets_encrypt(&self) -> Arc<str> {
        match self.base.staging {
            true => LetsEncrypt::Staging.url().into(),
            false => LetsEncrypt::Production.url().into(),
        }
    }

    /// The configured domain set, validated as a DNS-01 order admits it.
    pub(crate) fn validated_domains(&self) -> Result<Arc<[Arc<str>]>, RuntimeError> {
        self.base.validated_domains(Challenge::Dns01)
    }

    /// Validate an issued generation against `domains` and publish it to this
    /// configuration's cache, failing at `fault`.
    ///
    /// # Errors
    ///
    /// `Provision` with `InvalidCertificate` when the generation fails
    /// validation; nothing is published.
    pub(crate) fn publish_issued(
        &self,
        domains: &[Arc<str>],
        cert_pem: &str,
        key_pem: &str,
        fault: Fault,
    ) -> Result<Issued, RuntimeError> {
        publish_issued(
            &self.base.cache_dir,
            domains,
            cert_pem,
            key_pem,
            fault,
            |publication| publication(),
        )
        .map_err(integration)
    }

    /// Load the cached certificate generation, validated against the configured
    /// domains and the current time.
    ///
    /// The bundle `certificate.pem` is read whenever it exists. A legacy
    /// `cert.pem`/`key.pem` pair is read only when it does not, and a valid pair
    /// is migrated to a bundle before it is returned. `Ok(None)` means nothing
    /// is cached.
    ///
    /// # Errors
    ///
    /// `RuntimeError::Config` for a refused domain set.
    /// `RuntimeError::Integration` with `CacheRead` and `InvalidCertificate`
    /// when the leaf does not parse, its key belongs to another leaf, its
    /// validity window does not contain the current time, or its SANs do not
    /// cover every configured domain. A corrupt bundle is this error even when a
    /// valid legacy pair exists. `CacheRead` for an unreadable file, and
    /// `CacheWrite` when a valid legacy pair could not be migrated.
    pub fn load_cached_cert(&self) -> Result<Option<CertifiedKey>, RuntimeError> {
        Ok(self.cached_generation()?.map(Generation::into_key))
    }

    /// Whether the cached certificate needs renewal: its leaf's own `notAfter`
    /// is less than 30 days away.
    ///
    /// No cached generation, and a generation [`AcmeDns01::load_cached_cert`]
    /// refuses, both read as "renew". The alternative is skipping a renewal the
    /// certificate needs. A refusal is reported at warn level, because that
    /// answer repeats on every renewal pass.
    pub fn needs_renewal(&self) -> bool {
        renewal_needed(self.cached_generation())
    }

    /// Read the cached generation for the validated domain set.
    fn cached_generation(&self) -> Result<Option<Generation>, RuntimeError> {
        let domains = self.validated_domains()?;
        read_cached(&self.base.cache_dir, &domains).map_err(integration)
    }

    /// Renew the certificate before expiry and swap it into `store`, as an
    /// operation the current runtime admits.
    ///
    /// The configuration is validated and the renewal owner admitted before
    /// this returns; a refusal arrives through the handle. The owner takes
    /// `provider`. Every 12 hours it reads the cached leaf, and a leaf whose
    /// `notAfter` is less than 30 days away starts one order. The order
    /// prepares the provider, and the next order starts only after this
    /// order's cleanup settled. A failed renewal keeps the served
    /// certificate and retries at the next check; it does not end the task. A
    /// renewal the report budget refuses does no provider work.
    ///
    /// A failed cleanup stays charged to the runtime after the owner retires,
    /// and a later successful renewal does not clear it: the runtime's
    /// aggregate names each record it could not delete.
    ///
    /// One runtime renews one cache through one owner. While the runtime's own
    /// `tls_auto_dns01` owner, or another `spawn_renewal`, renews the same
    /// cache directory, this one is refused.
    ///
    /// The handle resolves with `Ok(())` once the runtime stops the renewal.
    /// [`AsyncJoinHandle::cancel`] asks the owner to stop: an order in
    /// progress stops and settles its cleanup, then the handle resolves with
    /// `Err(Cancelled)`. The root scope retains the task either way, so
    /// dropping the handle does not detach it.
    ///
    /// # Errors
    ///
    /// Through the handle: a refused configuration, `NoRuntime` outside a
    /// Camber runtime, `ScopeClosed` once its admission closed, `Busy` at the
    /// live limit, a full report budget, or while another owner of this
    /// runtime renews the same cache, and `Cancelled` after `cancel`.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// use camber::RuntimeError;
    /// use camber::dns01::{AcmeDns01, CloudflareProvider};
    /// use camber::tls::CertStore;
    ///
    /// # fn main() -> Result<(), RuntimeError> {
    /// camber::runtime::run(|| {
    ///     camber::runtime::block_on(async {
    ///         let acme = AcmeDns01::new("myapp", ["example.com"]);
    ///         let key = acme
    ///             .provision_cert(CloudflareProvider::new("token".into())?)
    ///             .await?;
    ///         let store = CertStore::new(key);
    ///         let renewal = acme.spawn_renewal(CloudflareProvider::new("token".into())?, store);
    ///         // Serve with the store's clone, then stop renewing. An order in
    ///         // progress cleans up its records before the handle answers.
    ///         renewal.cancel();
    ///         match renewal.await {
    ///             Err(RuntimeError::Cancelled) => Ok(()),
    ///             other => other.and_then(|renewed| renewed),
    ///         }
    ///     })
    /// })?
    /// # }
    /// ```
    pub fn spawn_renewal<P: DnsProvider + 'static>(
        self,
        provider: P,
        store: CertStore,
    ) -> AsyncJoinHandle<Result<(), RuntimeError>> {
        let admission = IntegrationOperation::Renew;
        let renewing = admitted(admission, || {
            let plan = self.plan()?;
            let runtime = crate::runtime::runtime_context()?;
            let owner = Dns01Owner::admit_renewing(&runtime, plan, provider, admission)?;
            Ok((owner, runtime))
        });
        match renewing {
            Ok((owner, runtime)) => owner.spawn_renewal(&runtime, store),
            Err(error) => AsyncJoinHandle::refused(error),
        }
    }
}

/// The refusal of a configuration value provisioning cannot admit.
fn invalid_config() -> RuntimeError {
    integration(failure::invalid_config(IntegrationOperation::Provision))
}

/// A directory is an absolute `https` URL with a host, and no credentials or
/// fragment.
fn valid_directory(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|url| {
        url.scheme() == "https"
            && url.host_str().is_some_and(|host| !host.is_empty())
            && transport::anonymous(&url)
    })
}

/// Every certificate in `pem`, or `None` when it holds none or one does not
/// parse as X.509.
fn parse_roots(pem: &[u8]) -> Option<Box<[CertificateDer<'static>]>> {
    let roots = CertificateDer::pem_slice_iter(pem)
        .collect::<Result<Box<[_]>, _>>()
        .ok()?;
    let parsed = roots
        .iter()
        .all(|root| x509_parser::parse_x509_certificate(root.as_ref()).is_ok());
    match (roots.is_empty(), parsed) {
        (false, true) => Some(roots),
        _ => None,
    }
}
