use std::future::Future;
use std::sync::Arc;

use crate::RuntimeError;

/// The label every DNS-01 challenge record sits under.
const CHALLENGE_LABEL: &str = "_acme-challenge";

/// The challenge record name that proves `base`.
pub(super) fn challenge_name(base: &str) -> Box<str> {
    format!("{CHALLENGE_LABEL}.{base}").into_boxed_str()
}

/// Unique identifier for a DNS record returned by the provider.
pub type RecordId = Box<str>;

/// Provider-agnostic interface for DNS record management.
///
/// Used by ACME DNS-01 challenges to create and clean up TXT records.
/// Implementations must be safe to share across threads.
///
/// Camber owns the provider once provisioning admits it: it calls
/// [`DnsProvider::prepare`] with the order's complete validated domain set
/// before any record write, and it bounds every call by the order's deadline.
/// These callbacks are provider primitives, not separate managed operations.
/// Camber cannot preempt provider code that never yields, so a provider must
/// return its own errors honestly.
pub trait DnsProvider: Send + Sync {
    /// Establish the authority for one order's complete domain set.
    ///
    /// `domains` are canonical: lowercase, no trailing dot, and a wildcard
    /// only as a whole leftmost `*.` label. Preparation may query provider
    /// metadata. It must not create challenge records or start detached work.
    /// A failure leaves no usable authority behind: the order stops before any
    /// record is written.
    fn prepare(
        &mut self,
        domains: &[Arc<str>],
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send;

    /// Create a TXT record at the given FQDN with the specified value.
    /// Returns the record's unique identifier for later deletion.
    ///
    /// A name outside the prepared authority must be refused before anything
    /// is sent.
    ///
    /// A failure must say whether a record can exist. Return
    /// `RuntimeError::Integration` with a retryability other than
    /// `OutcomeUnknown` only when nothing was created. Camber keeps any other
    /// error, and an unwind, as a record of unknown outcome and names it in the
    /// order's cleanup account.
    fn create_txt_record(
        &self,
        fqdn: &str,
        value: &str,
    ) -> impl Future<Output = Result<RecordId, RuntimeError>> + Send;

    /// Delete a previously created TXT record by its identifier.
    ///
    /// Camber calls this only with an ID this provider acknowledged, never with
    /// a challenge name. A failure leaves the record named in the order's
    /// cleanup account.
    fn delete_txt_record(
        &self,
        record_id: &str,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send;
}
