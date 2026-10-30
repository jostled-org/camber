//! Typed inspection of the same resolved budgets dispatch enforces.
//!
//! Each wrapper keeps one typed budget beside the sources of each of its
//! dimensions, so a caller reads the enforced value and why it applies together.

use super::policy_dimension::PolicyDimension;
use super::policy_resolution::Located;
use super::policy_sources::{PolicyBound, PolicyLayers};
use super::route_budgets::RouteBudgets;
use super::{RequestBudget, TransferBudget};
use std::time::Duration;

/// Effective request deadlines and their limiting layers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectiveRequestBudget {
    value: RequestBudget,
    body_idle: PolicyLayers,
    total: PolicyLayers,
}

impl EffectiveRequestBudget {
    pub(super) fn of(budgets: &Located<RouteBudgets>) -> Self {
        Self {
            value: budgets.values().request(),
            body_idle: budgets.sources(PolicyDimension::RequestBodyIdle),
            total: budgets.sources(PolicyDimension::RequestTotal),
        }
    }
    /// The maximum quiet interval while reading the request body.
    pub const fn body_idle(self) -> PolicyBound<Option<Duration>> {
        PolicyBound::new(self.value.body_idle(), self.body_idle)
    }
    /// The maximum time before the response head commits.
    pub const fn total(self) -> PolicyBound<Option<Duration>> {
        PolicyBound::new(self.value.total(), self.total)
    }
    /// The validated budget enforced by the request owner.
    pub const fn value(self) -> RequestBudget {
        self.value
    }
}

/// Effective transfer limits and their limiting layers.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct EffectiveTransferBudget {
    value: TransferBudget,
    max_bytes: PolicyLayers,
    idle: PolicyLayers,
    total: PolicyLayers,
}

impl EffectiveTransferBudget {
    pub(super) fn upload(budgets: &Located<RouteBudgets>) -> Self {
        Self::of(budgets.values().upload(), budgets, PolicyDimension::UPLOAD)
    }

    pub(super) fn download(budgets: &Located<RouteBudgets>) -> Self {
        Self::of(
            budgets.values().download(),
            budgets,
            PolicyDimension::DOWNLOAD,
        )
    }

    fn of(
        value: TransferBudget,
        budgets: &Located<RouteBudgets>,
        [max_bytes, idle, total]: [PolicyDimension; 3],
    ) -> Self {
        Self {
            value,
            max_bytes: budgets.sources(max_bytes),
            idle: budgets.sources(idle),
            total: budgets.sources(total),
        }
    }
    /// The maximum payload bytes.
    pub const fn max_bytes(self) -> PolicyBound<Option<usize>> {
        PolicyBound::new(self.value.max_bytes(), self.max_bytes)
    }
    /// The maximum quiet interval between frames.
    pub const fn idle(self) -> PolicyBound<Option<Duration>> {
        PolicyBound::new(self.value.idle(), self.idle)
    }
    /// The maximum transfer lifetime.
    pub const fn total(self) -> PolicyBound<Option<Duration>> {
        PolicyBound::new(self.value.total(), self.total)
    }
    /// The validated budget enforced by the transfer owner.
    pub const fn value(self) -> TransferBudget {
        self.value
    }
}
