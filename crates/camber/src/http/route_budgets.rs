//! The request and transfer budgets one layer configures, and their resolution.
//!
//! One owner for the whole precedence chain. The runtime's policy contains the
//! server's, the server's contains the host router's, the host router's
//! contains the child router's, and the router's contains a specialized route
//! registration. Each edge resolves through [`PolicyValues::contained_by`], so
//! no dimension can acquire a second, differently-shaped rule.

use super::policy_dimension::{DimensionSet, DimensionSources, PolicyDimension};
use super::policy_report::PolicyValue;
use super::policy_resolution::{Located, PolicyValues, yielded};
use super::policy_sources::{PolicyLayer, PolicyLayers};
use super::request_budget::RequestBudget;
use super::transfer_budget::TransferBudget;

/// The request and transfer budgets one layer configured.
///
/// A layer that configured nothing holds the fully unbounded value and no
/// explicit dimension, so it yields to whatever contains it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct RouteBudgets {
    request: RequestBudget,
    upload: TransferBudget,
    download: TransferBudget,
    explicit: DimensionSet,
}

impl Default for RouteBudgets {
    fn default() -> Self {
        Self::new(
            RequestBudget::unbounded(),
            TransferBudget::unbounded(),
            TransferBudget::unbounded(),
            DimensionSet::EMPTY,
        )
    }
}

impl RouteBudgets {
    pub(super) const fn new(
        request: RequestBudget,
        upload: TransferBudget,
        download: TransferBudget,
        explicit: DimensionSet,
    ) -> Self {
        Self {
            request,
            upload,
            download,
            explicit,
        }
    }

    pub(super) fn with_request(self, request: RequestBudget) -> Self {
        Self {
            request,
            explicit: self.explicit.union(DimensionSet::REQUEST),
            ..self
        }
    }

    pub(super) fn with_upload(self, upload: TransferBudget) -> Self {
        Self {
            upload,
            explicit: self.explicit.union(DimensionSet::UPLOAD),
            ..self
        }
    }

    pub(super) fn with_download(self, download: TransferBudget) -> Self {
        Self {
            download,
            explicit: self.explicit.union(DimensionSet::DOWNLOAD),
            ..self
        }
    }

    /// These budgets at `layer`: each explicit dimension names that layer, and
    /// an unconfigured one names no layer because it supplies nothing.
    pub(super) fn located(self, layer: PolicyLayer) -> Located<Self> {
        Located::new(
            self,
            DimensionSources::collect(Self::dimensions(), |dimension| {
                match self.explicit.contains(dimension) {
                    true => PolicyLayers::one(layer, false),
                    false => PolicyLayers::EMPTY,
                }
            }),
        )
    }

    /// The listed dimensions set to unbounded, so the other side of an edge
    /// supplies them.
    fn without(self, dimensions: DimensionSet) -> Self {
        let request = match dimensions.contains(PolicyDimension::RequestBodyIdle) {
            true => self.request.without_body_idle(),
            false => self.request,
        };
        let request = match dimensions.contains(PolicyDimension::RequestTotal) {
            true => request.without_total(),
            false => request,
        };
        Self {
            request,
            upload: transfer_without(self.upload, dimensions, PolicyDimension::UPLOAD),
            download: transfer_without(self.download, dimensions, PolicyDimension::DOWNLOAD),
            ..self
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

    /// These budgets with no upload payload maximum.
    ///
    /// For a consumer whose payload bytes route-aware body admission counts.
    fn upload_deadlines_only(self) -> Self {
        Self {
            upload: self.upload.without_max_bytes(),
            ..self
        }
    }
}

impl Located<RouteBudgets> {
    /// The upload envelope when body admission owns payload-byte accounting.
    pub(super) fn upload_deadlines_only(self) -> Self {
        Self::new(self.values().upload_deadlines_only(), self.all_sources())
            .without_source(PolicyDimension::UploadMaxBytes)
    }
}

/// `budget` with each of its three listed dimensions set to unbounded.
fn transfer_without(
    budget: TransferBudget,
    dimensions: DimensionSet,
    [max_bytes, idle, total]: [PolicyDimension; 3],
) -> TransferBudget {
    let budget = match dimensions.contains(max_bytes) {
        true => budget.without_max_bytes(),
        false => budget,
    };
    let budget = match dimensions.contains(idle) {
        true => budget.without_idle(),
        false => budget,
    };
    match dimensions.contains(total) {
        true => budget.without_total(),
        false => budget,
    }
}

impl PolicyValues for RouteBudgets {
    fn dimensions() -> impl Iterator<Item = PolicyDimension> {
        PolicyDimension::ROUTE.into_iter()
    }

    fn explicit(&self) -> DimensionSet {
        self.explicit
    }

    fn value(&self, dimension: PolicyDimension) -> Option<PolicyValue> {
        match dimension {
            PolicyDimension::RequestBodyIdle => {
                Some(PolicyValue::Duration(self.request.body_idle()))
            }
            PolicyDimension::RequestTotal => Some(PolicyValue::Duration(self.request.total())),
            PolicyDimension::UploadMaxBytes => Some(PolicyValue::Bytes(self.upload.max_bytes())),
            PolicyDimension::UploadIdle => Some(PolicyValue::Duration(self.upload.idle())),
            PolicyDimension::UploadTotal => Some(PolicyValue::Duration(self.upload.total())),
            PolicyDimension::DownloadMaxBytes => {
                Some(PolicyValue::Bytes(self.download.max_bytes()))
            }
            PolicyDimension::DownloadIdle => Some(PolicyValue::Duration(self.download.idle())),
            PolicyDimension::DownloadTotal => Some(PolicyValue::Duration(self.download.total())),
            PolicyDimension::HeaderTimeout
            | PolicyDimension::ShutdownTimeout
            | PolicyDimension::ConnectionLimit
            | PolicyDimension::HealthRoute
            | PolicyDimension::ProfilingResponseLimit
            | PolicyDimension::RequestBodyHardCap
            | PolicyDimension::RequestBodyLimit => None,
        }
    }

    fn contained_by(self, outer: Self) -> Self {
        let (inner_yields, outer_yields) = yielded(&self, &outer);
        let inner = self.without(inner_yields);
        let outer = outer.without(outer_yields);
        Self {
            request: inner.request.narrowed_by(outer.request),
            upload: inner.upload.narrowed_by(outer.upload),
            download: inner.download.narrowed_by(outer.download),
            explicit: inner.explicit.union(outer.explicit),
        }
    }
}
