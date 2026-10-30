//! The one rule every same-dimension containment edge resolves by.
//!
//! An explicit value on both sides resolves to the narrower one. A defaulted
//! value yields to an explicit value on the other side. Two defaults resolve to
//! the narrower one. A [`Containment::Hard`] dimension keeps its outer bound
//! even when that bound is a default.
//!
//! [`Containment::Hard`]: super::policy_dimension::Containment::Hard

use super::policy_dimension::{DimensionSet, DimensionSources, PolicyDimension, Supplier};
use super::policy_report::{PolicyDiagnostic, PolicyValue};
use super::policy_sources::{PolicyLayer, PolicyLayers, supplied_sources};

/// One layer's values, read per dimension.
pub(super) trait PolicyValues: Copy {
    /// Every dimension these values carry.
    fn dimensions() -> impl Iterator<Item = PolicyDimension>;
    /// The dimensions a caller set explicitly, at this layer or one it resolved.
    fn explicit(&self) -> DimensionSet;
    /// The value of `dimension`, or `None` when these values carry no such dimension.
    fn value(&self, dimension: PolicyDimension) -> Option<PolicyValue>;
    /// These values inside `outer`, resolved per dimension.
    fn contained_by(self, outer: Self) -> Self;

    /// Which side supplies `dimension` when these values sit inside `outer`.
    fn supplier(&self, outer: &Self, dimension: PolicyDimension) -> Supplier {
        dimension.containment().supplier(
            self.explicit().contains(dimension),
            outer.explicit().contains(dimension),
        )
    }
}

/// Values and the layers that supplied each of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Located<V> {
    values: V,
    sources: DimensionSources,
}

impl<V: PolicyValues> Located<V> {
    pub(super) const fn new(values: V, sources: DimensionSources) -> Self {
        Self { values, sources }
    }

    /// The resolved values production owners enforce.
    pub(super) const fn values(&self) -> V {
        self.values
    }

    /// The layers tied at the resolved value of `dimension`.
    pub(super) const fn sources(&self, dimension: PolicyDimension) -> PolicyLayers {
        self.sources.get(dimension)
    }

    /// The sources of every dimension.
    pub(super) const fn all_sources(&self) -> DimensionSources {
        self.sources
    }

    /// These values inside `outer`, with every resolved value's sources.
    pub(super) fn contained_by(self, outer: Self) -> Self {
        let values = self.values.contained_by(outer.values);
        let sources = DimensionSources::collect(V::dimensions(), |dimension| {
            supplied_sources(
                self.values.supplier(&outer.values, dimension),
                (self.values.value(dimension), self.sources(dimension)),
                (outer.values.value(dimension), outer.sources(dimension)),
                values.value(dimension),
            )
        });
        Self { values, sources }
    }

    /// One diagnostic for each explicit `local` value this resolution changed.
    pub(super) fn diagnostics(
        &self,
        layer: PolicyLayer,
        local: &V,
    ) -> impl Iterator<Item = PolicyDiagnostic> {
        let local = *local;
        let effective = *self;
        V::dimensions()
            .filter(move |dimension| local.explicit().contains(*dimension))
            .filter_map(move |dimension| {
                PolicyDiagnostic::changed(
                    layer,
                    dimension,
                    local.value(dimension)?,
                    effective.values.value(dimension)?,
                    effective.sources(dimension),
                )
            })
    }

    /// These values with `dimension`'s sources cleared.
    pub(super) const fn without_source(self, dimension: PolicyDimension) -> Self {
        Self {
            sources: self.sources.with(dimension, PolicyLayers::EMPTY),
            ..self
        }
    }
}

/// The dimensions each side of one edge gives up to the other.
///
/// The first set is where the inner side yields to an explicit outer value,
/// and the second is where the outer side yields to an explicit inner value.
pub(super) fn yielded<V: PolicyValues>(inner: &V, outer: &V) -> (DimensionSet, DimensionSet) {
    V::dimensions().fold(
        (DimensionSet::EMPTY, DimensionSet::EMPTY),
        |(inner_yields, outer_yields), dimension| match inner.supplier(outer, dimension) {
            Supplier::Outer => (inner_yields.with(dimension), outer_yields),
            Supplier::Inner => (inner_yields, outer_yields.with(dimension)),
            Supplier::Narrower => (inner_yields, outer_yields),
        },
    )
}
