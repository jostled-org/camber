//! The effective budgets one matched route resolved to.
//!
//! One owner for the whole precedence chain. The runtime's policy contains the
//! server's, the server's contains the host router's, and the host router's
//! contains the child router's. Each layer narrows through the same call, so no
//! dimension can acquire a second, differently-shaped rule.

use super::request_budget::RequestBudget;
use super::server_policy::ServerPolicy;
use super::transfer_budget::TransferBudget;

/// The request and transfer budgets one layer configured, or inherits.
///
/// A layer that configured nothing holds the fully unbounded value, which
/// narrows to whatever contains it — inheriting an outer bound is exactly what
/// "unbounded here" means at an inner layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RouteBudgets {
    request: RequestBudget,
    upload: TransferBudget,
    download: TransferBudget,
}

impl Default for RouteBudgets {
    fn default() -> Self {
        Self {
            request: RequestBudget::unbounded(),
            upload: TransferBudget::unbounded(),
            download: TransferBudget::unbounded(),
        }
    }
}

impl RouteBudgets {
    /// The budgets a server policy supplies to every route under it.
    pub(super) fn from_policy(policy: ServerPolicy) -> Self {
        Self {
            request: policy.request_budget_value(),
            upload: policy.upload_budget_value(),
            download: policy.download_budget_value(),
        }
    }

    pub(super) fn with_request(self, request: RequestBudget) -> Self {
        Self { request, ..self }
    }

    pub(super) fn with_upload(self, upload: TransferBudget) -> Self {
        Self { upload, ..self }
    }

    pub(super) fn with_download(self, download: TransferBudget) -> Self {
        Self { download, ..self }
    }

    /// These budgets applied under the layer that contains them.
    pub(super) fn narrowed_by(self, outer: Self) -> Self {
        Self {
            request: self.request.narrowed_by(outer.request),
            upload: self.upload.narrowed_by(outer.upload),
            download: self.download.narrowed_by(outer.download),
        }
    }

    pub(super) const fn request(self) -> RequestBudget {
        self.request
    }

    pub(super) const fn upload(self) -> TransferBudget {
        self.upload
    }

    pub(super) const fn download(self) -> TransferBudget {
        self.download
    }
}
