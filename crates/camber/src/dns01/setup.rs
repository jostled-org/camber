//! Runtime-managed DNS-01: validated before the runtime exists, admitted and
//! provisioned inside it before the closure serves.
//!
//! `RuntimeBuilder::run` validates the whole configuration and constructs the
//! provider descriptor first, with no I/O. The TLS configuration it installs
//! resolves through a certificate slot the owner fills once its first
//! certificate is ready. After the runtime is established, startup admits the
//! owner as an integration entry, and the owner prepares every configured zone
//! and serves its first certificate before the closure runs. A failure ends
//! the run through the runtime's ordinary teardown.

use std::future::Future;
use std::sync::{Arc, OnceLock};

use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use tokio::sync::oneshot;

use super::acme::AcmeDns01;
use super::cloudflare::CloudflareProvider;
use super::failure::cancelled;
use super::order::{OrderPlan, StopRequest};
use super::owner::{Dns01Owner, admitted};
use super::provider::DnsProvider;
use crate::integration_lifecycle::integration;
use crate::runtime_state::{LifecycleSignals, RuntimeInner};
use crate::tls::{CertStore, build_tls_config};
use crate::{IntegrationError, IntegrationOperation, Retryability, RuntimeError};

/// Configuration for DNS-01 ACME stored on the `RuntimeBuilder`.
pub(crate) struct Dns01Setup {
    acme: AcmeDns01,
    api_token: Box<str>,
    /// The Cloudflare API base a test transport names; production when absent.
    base_url: Option<Box<str>>,
}

impl Dns01Setup {
    pub(crate) const fn new(acme: AcmeDns01, api_token: Box<str>) -> Self {
        Self {
            acme,
            api_token,
            base_url: None,
        }
    }

    /// Send provider requests to `base_url` instead of the production API.
    pub(crate) fn with_base_url(self, base_url: Option<Box<str>>) -> Self {
        Self { base_url, ..self }
    }

    /// Validate everything startup needs and construct the provider
    /// descriptor. Performs no I/O.
    ///
    /// # Errors
    ///
    /// `Config` for a refused domain set, `Provision` with `InvalidConfig` for
    /// a refused bound, and `ZoneLookup` with `InvalidConfig` for a refused
    /// token or provider base.
    pub(crate) fn validate(self) -> Result<Dns01Startup, RuntimeError> {
        let plan = self.acme.plan()?;
        let provider = cloudflare_provider(self.api_token, self.base_url)?;
        Ok(Dns01Startup {
            plan,
            provider,
            served: ServedCert::default(),
        })
    }
}

/// The one function startup constructs its provider through.
fn cloudflare_provider(
    api_token: Box<str>,
    base_url: Option<Box<str>>,
) -> Result<CloudflareProvider, RuntimeError> {
    match base_url {
        Some(base_url) => CloudflareProvider::with_base_url(api_token, base_url),
        None => CloudflareProvider::new(api_token),
    }
}

/// A validated DNS-01 startup, not yet admitted.
pub(crate) struct Dns01Startup {
    plan: OrderPlan,
    provider: CloudflareProvider,
    served: ServedCert,
}

impl Dns01Startup {
    /// The TLS configuration that serves this startup's certificate.
    ///
    /// # Errors
    ///
    /// `Tls` when the protocol versions cannot be configured.
    pub(crate) fn tls_config(&self) -> Result<Arc<rustls::ServerConfig>, RuntimeError> {
        build_tls_config(Arc::new(self.served.clone()))
    }

    /// Admit the owner to `runtime`, wait for its first certificate, and leave
    /// it renewing as a root-scope child.
    ///
    /// # Errors
    ///
    /// The admission refusal, or the first order's failure.
    pub(crate) async fn start(self, runtime: &Arc<RuntimeInner>) -> Result<(), RuntimeError> {
        let admission = IntegrationOperation::Provision;
        let mut owner = admitted(admission, || {
            Dns01Owner::admit_renewing(runtime, self.plan, self.provider, admission)
        })?;
        let served = self.served;
        let (first, first_served) = oneshot::channel();
        admit_owner_task(runtime, move |signals| async move {
            let request = StopRequest::new(signals, None);
            let key = match owner.initial(&request).await {
                Ok(key) => key,
                Err(error) => {
                    drop(first.send(Err(error)));
                    return;
                }
            };
            let store = CertStore::new(key);
            served.install(store.clone());
            drop(first.send(Ok(())));
            owner.renew(store, request).await;
        })?;
        first_served
            .await
            .unwrap_or_else(|_| Err(integration(stopped_before_ready())))
    }
}

/// Admit a renewal owner of `acme` and `provider` to `runtime` as its DNS-01
/// subsystem, renewing into `store`.
///
/// # Errors
///
/// A refused configuration, a claim another owner holds, the registry's
/// refusal, or the root scope's.
pub(crate) fn admit_renewal<P: DnsProvider + 'static>(
    runtime: &Arc<RuntimeInner>,
    acme: &AcmeDns01,
    provider: P,
    store: CertStore,
) -> Result<(), RuntimeError> {
    let admission = IntegrationOperation::Renew;
    let owner = admitted(admission, || {
        Dns01Owner::admit_renewing(runtime, acme.plan()?, provider, admission)
    })?;
    admit_owner_task(runtime, move |signals| {
        owner.renew(store, StopRequest::new(signals, None))
    })
}

/// Admit the task a DNS-01 owner runs in as `runtime`'s named subsystem,
/// built from that runtime's own lifecycle signals.
fn admit_owner_task<B, Fut>(runtime: &Arc<RuntimeInner>, build: B) -> Result<(), RuntimeError>
where
    B: FnOnce(LifecycleSignals) -> Fut,
    Fut: Future<Output = ()> + Send + 'static,
{
    crate::task::admit_signalled_subsystem_on(runtime, "dns01 renewal", build)
}

/// The first order's owner ended before it reported.
const fn stopped_before_ready() -> IntegrationError {
    cancelled(IntegrationOperation::Provision, Retryability::Safe)
}

/// The certificate slot the TLS configuration resolves through, filled once
/// the owner's first certificate is ready.
///
/// Empty only before the closure serves: startup waits for the owner to fill
/// it, and no server exists until then.
#[derive(Clone, Debug, Default)]
struct ServedCert {
    store: Arc<OnceLock<CertStore>>,
}

impl ServedCert {
    fn install(&self, store: CertStore) {
        // Filled exactly once, by the one owner that holds this slot.
        drop(self.store.set(store));
    }
}

impl ResolvesServerCert for ServedCert {
    fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        self.store
            .get()
            .and_then(|store| store.resolve(client_hello))
    }
}

/// Start the runtime's DNS-01 owner, when one is configured, under the signals
/// of the runtime it belongs to.
pub(crate) async fn start_dns01(
    runtime: &Arc<RuntimeInner>,
    startup: Option<Dns01Startup>,
) -> Result<(), RuntimeError> {
    match startup {
        Some(startup) => startup.start(runtime).await,
        None => Ok(()),
    }
}
