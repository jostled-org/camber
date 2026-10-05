//! Integration error construction for external integration tests.

use crate::{
    CleanupItem, IntegrationError, IntegrationFailure, IntegrationKind, IntegrationOperation,
    Retryability,
};
use std::error::Error;
use std::sync::Arc;

/// Builds one integration error through the production factory.
///
/// Values only: the driver runs the factory an adapter uses and returns the
/// error it built. It admits nothing, settles nothing, and cannot fabricate a
/// lifecycle settlement.
#[doc(hidden)]
#[must_use]
pub struct IntegrationErrorDriver {
    error: IntegrationError,
    cleanup: Vec<CleanupItem>,
}

impl IntegrationErrorDriver {
    /// Start an error with its four closed values.
    pub const fn new(
        kind: IntegrationKind,
        operation: IntegrationOperation,
        failure: IntegrationFailure,
        retryability: Retryability,
    ) -> Self {
        Self {
            error: IntegrationError::new(kind, operation, failure, retryability),
            cleanup: Vec::new(),
        }
    }

    /// Name the admitted instance, as admission would have assigned it.
    pub fn instance(mut self, id: u64) -> Self {
        self.error = self.error.with_instance(id);
        self
    }

    /// Hand over the third-party source, shared rather than copied.
    pub fn source(mut self, source: Arc<dyn Error + Send + Sync>) -> Self {
        self.error = self.error.with_source(source);
        self
    }

    /// Add one unresolved cleanup record.
    pub fn cleanup(
        mut self,
        domain: &str,
        record_id: Option<&str>,
        failure: IntegrationFailure,
    ) -> Self {
        self.cleanup.push(CleanupItem::new(
            domain.into(),
            record_id.map(Into::into),
            failure,
        ));
        self
    }

    /// The error the factory built.
    pub fn build(self) -> IntegrationError {
        match self.cleanup.is_empty() {
            true => self.error,
            false => self.error.with_cleanup(self.cleanup),
        }
    }
}
