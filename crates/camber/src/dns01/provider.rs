use crate::RuntimeError;

/// Unique identifier for a DNS record returned by the provider.
pub type RecordId = Box<str>;

/// Provider-agnostic interface for DNS record management.
///
/// Used by ACME DNS-01 challenges to create and clean up TXT records.
/// Implementations must be safe to share across threads.
pub trait DnsProvider: Send + Sync {
    /// Create a TXT record at the given FQDN with the specified value.
    /// Returns the record's unique identifier for later deletion.
    fn create_txt_record(
        &self,
        fqdn: &str,
        value: &str,
    ) -> impl std::future::Future<Output = Result<RecordId, RuntimeError>> + Send;

    /// Delete a previously created TXT record by its identifier.
    fn delete_txt_record(
        &self,
        record_id: &str,
    ) -> impl std::future::Future<Output = Result<(), RuntimeError>> + Send;
}
