use std::sync::Arc;

use super::acme::AcmeDns01;
use super::cloudflare::CloudflareProvider;
use crate::tls::{CertStore, build_tls_config_from_resolver};

/// Configuration for DNS-01 ACME stored on the RuntimeBuilder.
pub(crate) struct Dns01Setup {
    pub(crate) acme: AcmeDns01,
    pub(crate) api_token: Box<str>,
    pub(crate) domain: Box<str>,
}

/// State produced by DNS-01 initialization — cert loaded, store ready.
pub(crate) struct Dns01State {
    pub(crate) acme: AcmeDns01,
    pub(crate) provider: CloudflareProvider,
    pub(crate) store: CertStore,
    pub(crate) tls_config: Arc<rustls::ServerConfig>,
}

fn log_cache_miss(result: &Result<Option<rustls::sign::CertifiedKey>, crate::RuntimeError>) {
    match result {
        Ok(None) => tracing::info!("no cached cert found, provisioning fresh certificate"),
        Err(err) => {
            tracing::warn!(%err, "failed to load cached cert, provisioning fresh certificate")
        }
        Ok(Some(_)) => {}
    }
}

/// Initialize DNS-01: create provider, load or provision cert, build TLS config.
pub(crate) async fn init_dns01(setup: Dns01Setup) -> Result<Dns01State, crate::RuntimeError> {
    let provider = CloudflareProvider::new(setup.api_token, &setup.domain).await?;

    let cert = match setup.acme.load_cached_cert() {
        Ok(Some(cert)) if !setup.acme.needs_renewal() => cert,
        Ok(Some(_)) => {
            tracing::info!("cached cert needs renewal, provisioning fresh certificate");
            setup.acme.provision_cert(&provider).await?
        }
        result => {
            log_cache_miss(&result);
            setup.acme.provision_cert(&provider).await?
        }
    };

    let store = CertStore::new(cert);
    let tls_config = build_tls_config_from_resolver(store.clone())?;

    Ok(Dns01State {
        acme: setup.acme,
        provider,
        store,
        tls_config,
    })
}
