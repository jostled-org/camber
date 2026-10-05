//! Faulted DNS-01 certificate publication, through issuance's own publisher.

use crate::RuntimeError;
use crate::dns01::{AcmeDns01, PublicationStage};
use crate::integration_lifecycle::integration;

/// The stage a [`DnsCachePublicationProbe`] faults.
#[doc(hidden)]
pub type DnsCachePublicationStage = PublicationStage;

/// Publishes one issued generation through the exact helper issuance uses, and
/// fails it at one stage.
///
/// The generation is validated as issuance validates it, and it is written in
/// the one bundle format by the one publisher. The probe chooses only the
/// faulted stage; it cannot skip validation, write another format, or replace
/// the publisher.
#[doc(hidden)]
pub struct DnsCachePublicationProbe;

impl DnsCachePublicationProbe {
    /// Validate `cert_pem` and `key_pem` for `acme`'s domains and publish them
    /// to its cache, failing at `stage`.
    ///
    /// # Errors
    ///
    /// `Config` for a refused domain set, `Provision` with
    /// `InvalidCertificate` for a refused generation, and otherwise the
    /// publisher's `CacheWrite` failure at `stage`.
    pub fn fail_at(
        acme: &AcmeDns01,
        cert_pem: &str,
        key_pem: &str,
        stage: DnsCachePublicationStage,
    ) -> Result<(), RuntimeError> {
        let domains = acme.validated_domains()?;
        acme.publish_issued(&domains, cert_pem, key_pem, Some(stage))?
            .cache
            .map_err(integration)
    }
}
