//! The integration report budget, entered from external component tests.

use crate::integration_lifecycle::{InstanceAccount, ReportAccounts};
use crate::{IntegrationError, IntegrationKind, IntegrationOperation};
use std::sync::Arc;

/// Drives one production report-account owner and reads its counts.
///
/// Every transition is the owner's own: admission here, and reservation,
/// settlement, receipt, abandonment, and close on the handles it returns. The
/// probe keeps no model of its own and cannot set a count. It owns no runtime,
/// thread, or timer, so it proves the accounting and nothing wired around it.
#[doc(hidden)]
pub struct IntegrationAccountProbe {
    accounts: Arc<ReportAccounts>,
}

impl IntegrationAccountProbe {
    /// A fresh owner with an empty budget.
    #[must_use]
    pub fn new() -> Self {
        Self {
            accounts: ReportAccounts::new(),
        }
    }

    /// Admit one instance through the owner, as a connect admits it.
    ///
    /// # Errors
    ///
    /// The owner's refusal: `Busy` at a full budget, `LimitExceeded` once
    /// identities are spent.
    pub fn admit(&self, kind: IntegrationKind) -> Result<InstanceAccount, IntegrationError> {
        self.accounts.admit(kind, IntegrationOperation::Connect)
    }

    /// Reserved plus retained accounts. Read-only.
    #[must_use]
    pub fn charged(&self) -> usize {
        self.accounts.charged()
    }

    /// Retained failures awaiting transfer. Read-only.
    #[must_use]
    pub fn retained(&self) -> usize {
        self.accounts.retained()
    }

    /// The owner's one-time transfer of its retained failures.
    #[must_use]
    pub fn transfer(&self) -> Box<[IntegrationError]> {
        self.accounts
            .transfer()
            .into_iter()
            .map(|(_, error)| error)
            .collect()
    }
}

impl Default for IntegrationAccountProbe {
    fn default() -> Self {
        Self::new()
    }
}
