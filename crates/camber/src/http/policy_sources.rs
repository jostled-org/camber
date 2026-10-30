//! Provenance for resolved service limits.

use super::policy_dimension::{Containment, Supplier};
use std::fmt;

/// A layer that supplied a service policy value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PolicyLayer {
    /// The enclosing Camber runtime.
    Runtime,
    /// The listener's server policy.
    Server,
    /// A host router containing child routers.
    Host,
    /// A child or standalone router.
    Router,
    /// A specialized route registration.
    Route,
}

impl PolicyLayer {
    const ALL: [Self; 5] = [
        Self::Runtime,
        Self::Server,
        Self::Host,
        Self::Router,
        Self::Route,
    ];

    /// The stable lowercase name that logs use.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Runtime => "runtime",
            Self::Server => "server",
            Self::Host => "host",
            Self::Router => "router",
            Self::Route => "route",
        }
    }

    /// The bit that records an explicit value from this layer.
    const fn explicit_bit(self) -> u16 {
        1 << (self as u16)
    }

    /// The bit that records a defaulted value from this layer.
    const fn default_bit(self) -> u16 {
        self.explicit_bit() << Self::ALL.len()
    }
}

impl fmt::Display for PolicyLayer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// One layer's contribution to a resolved value.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct LayerSource {
    layer: PolicyLayer,
    defaulted: bool,
}

impl LayerSource {
    /// The layer that supplied this bound.
    pub const fn layer(self) -> PolicyLayer {
        self.layer
    }
    /// Whether this layer used its documented default.
    pub const fn is_default(self) -> bool {
        self.defaulted
    }
}

impl fmt::Display for LayerSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.defaulted {
            true => write!(f, "{} (default)", self.layer),
            false => write!(f, "{}", self.layer),
        }
    }
}

/// Every layer tied at a resolved value, without allocation.
#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub struct PolicyLayers(u16);

impl PolicyLayers {
    pub(super) const EMPTY: Self = Self(0);

    /// One layer's value, explicit or defaulted.
    pub(super) const fn one(layer: PolicyLayer, defaulted: bool) -> Self {
        match defaulted {
            true => Self(layer.default_bit()),
            false => Self(layer.explicit_bit()),
        }
    }

    /// Every layer in either set.
    pub(super) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// Whether this layer supplied a limiting value.
    pub const fn contains(self, layer: PolicyLayer) -> bool {
        self.0 & (layer.explicit_bit() | layer.default_bit()) != 0
    }

    /// Whether no layer supplied a limiting value.
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Iterate tied sources in outer-to-inner order.
    pub fn iter(self) -> impl Iterator<Item = LayerSource> {
        PolicyLayer::ALL.into_iter().filter_map(move |layer| {
            self.contains(layer).then_some(LayerSource {
                layer,
                defaulted: self.0 & layer.default_bit() != 0,
            })
        })
    }
}

impl fmt::Debug for PolicyLayers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list()
            .entries(self.iter().map(|source| source.to_string()))
            .finish()
    }
}

/// Lists the layers as `runtime (default), server`, or `none`.
impl fmt::Display for PolicyLayers {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut sources = self.iter();
        match sources.next() {
            None => f.write_str("none"),
            Some(first) => {
                write!(f, "{first}")?;
                sources.try_for_each(|source| write!(f, ", {source}"))
            }
        }
    }
}

/// A resolved policy value and the layers that supplied it.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PolicyBound<T> {
    value: T,
    sources: PolicyLayers,
}

impl<T: Copy> PolicyBound<T> {
    pub(super) const fn new(value: T, sources: PolicyLayers) -> Self {
        Self { value, sources }
    }
    /// The value production owners enforce.
    pub const fn value(self) -> T {
        self.value
    }
    /// Every layer tied at the effective value.
    pub const fn sources(self) -> PolicyLayers {
        self.sources
    }
}

impl PolicyBound<usize> {
    /// The narrower of two bounds that both apply, with every tied source.
    pub(super) fn narrowed_by(self, outer: Self) -> Self {
        let value = self.value.min(outer.value);
        Self::new(
            value,
            supplied_sources(
                Supplier::Narrower,
                (self.value, self.sources),
                (outer.value, outer.sources),
                value,
            ),
        )
    }
}

/// One layer's value of one dimension, with the intent that resolution reads.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct Layered<T> {
    bound: PolicyBound<T>,
    explicit: bool,
}

impl<T: Copy + Eq> Layered<T> {
    /// A value `layer` set explicitly, or holds as its default.
    pub(super) const fn at(value: T, layer: PolicyLayer, explicit: bool) -> Self {
        Self {
            bound: PolicyBound::new(value, PolicyLayers::one(layer, !explicit)),
            explicit,
        }
    }

    /// The resolved value and its sources.
    pub(super) const fn bound(self) -> PolicyBound<T> {
        self.bound
    }

    /// This value inside `outer`, resolved by `containment`.
    pub(super) fn contained_by(
        self,
        outer: Self,
        containment: Containment,
        narrower: impl FnOnce(T, T) -> T,
    ) -> Self {
        let supplier = containment.supplier(self.explicit, outer.explicit);
        let value = supplier.choose(self.bound.value, outer.bound.value, narrower);
        Self {
            bound: PolicyBound::new(
                value,
                supplied_sources(
                    supplier,
                    (self.bound.value, self.bound.sources),
                    (outer.bound.value, outer.bound.sources),
                    value,
                ),
            ),
            explicit: self.explicit || outer.explicit,
        }
    }
}

/// The layers that supplied `effective`, given which side the edge chose.
///
/// A narrower choice names every side tied at the effective value.
pub(super) fn supplied_sources<T: Eq>(
    supplier: Supplier,
    (inner, inner_sources): (T, PolicyLayers),
    (outer, outer_sources): (T, PolicyLayers),
    effective: T,
) -> PolicyLayers {
    match (supplier, inner == effective, outer == effective) {
        (Supplier::Narrower, true, true) => inner_sources.union(outer_sources),
        (Supplier::Inner, _, _) | (Supplier::Narrower, true, false) => inner_sources,
        (Supplier::Outer, _, _) | (Supplier::Narrower, false, _) => outer_sources,
    }
}
