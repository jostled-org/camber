//! One renewing owner per DNS-01 cache in a runtime.
//!
//! The public `spawn_renewal` and the runtime-managed owner both renew into a
//! configured cache. Two owners of one cache would run overlapping orders and
//! race each other's publication, so a renewing owner claims its cache when
//! it is admitted and gives the claim back when it drops. The claim set is
//! private to DNS-01 and names caches only; it is no general registry.

use std::collections::BTreeSet;
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::failure::invalid_config;
use crate::integration_lifecycle::{busy, integration};
use crate::runtime_state::recover_poisoned;
use crate::{IntegrationKind, IntegrationOperation, RuntimeError};

/// The caches a runtime's renewing DNS-01 owners hold.
#[derive(Default)]
pub(crate) struct RenewalClaims {
    held: Arc<Mutex<BTreeSet<Arc<Path>>>>,
}

impl RenewalClaims {
    /// Claim `cache_dir` for one renewing owner admitted through `admission`.
    ///
    /// The cache is named by its absolute path, so a relative and an absolute
    /// spelling of one directory are one claim.
    ///
    /// # Errors
    ///
    /// `InvalidConfig` under `admission`, carrying the I/O error, when
    /// `cache_dir` cannot be made absolute, such as an empty path: a
    /// relative spelling would let one cache hold two claims. `Busy` under
    /// `admission` while another owner of this runtime holds the cache. A
    /// refusal claims nothing.
    pub(super) fn claim(
        &self,
        cache_dir: &Path,
        admission: IntegrationOperation,
    ) -> Result<RenewalClaim, RuntimeError> {
        let cache: Arc<Path> = std::path::absolute(cache_dir)
            .map_err(|error| integration(invalid_config(admission).with_source(Arc::new(error))))?
            .into();
        match recover_poisoned(self.held.lock()).insert(Arc::clone(&cache)) {
            true => Ok(RenewalClaim {
                held: Arc::clone(&self.held),
                cache,
            }),
            false => Err(integration(busy(IntegrationKind::Dns01, admission))),
        }
    }
}

/// One owner's claim on its cache, given back when the owner drops.
pub(super) struct RenewalClaim {
    held: Arc<Mutex<BTreeSet<Arc<Path>>>>,
    cache: Arc<Path>,
}

impl Drop for RenewalClaim {
    fn drop(&mut self) {
        recover_poisoned(self.held.lock()).remove(&*self.cache);
    }
}
