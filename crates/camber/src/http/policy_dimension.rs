//! The named bounds a service policy resolves, and sets of them.

use super::policy_sources::PolicyLayers;
use std::fmt;

/// One bound that a policy report or diagnostic names.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum PolicyDimension {
    /// The longest quiet interval while a request body is read.
    RequestBodyIdle,
    /// The longest time before the response head commits.
    RequestTotal,
    /// The streaming-upload payload maximum.
    UploadMaxBytes,
    /// The streaming-upload quiet interval.
    UploadIdle,
    /// The streaming-upload lifetime.
    UploadTotal,
    /// The streaming-download payload maximum.
    DownloadMaxBytes,
    /// The streaming-download quiet interval.
    DownloadIdle,
    /// The streaming-download lifetime.
    DownloadTotal,
    /// The pre-head wait for a complete request head.
    HeaderTimeout,
    /// The graceful drain deadline.
    ShutdownTimeout,
    /// The concurrent connection maximum.
    ConnectionLimit,
    /// Where the resource health report is served.
    HealthRoute,
    /// The rendered profile maximum. Only the `profiling` feature resolves it.
    ProfilingResponseLimit,
    /// The finite ceiling that contains every request-body limit.
    RequestBodyHardCap,
    /// A host, router, or route request-body limit.
    RequestBodyLimit,
}

impl PolicyDimension {
    /// How many dimensions exist.
    pub(super) const COUNT: usize = Self::ALL.len();

    const ALL: [Self; 15] = [
        Self::RequestBodyIdle,
        Self::RequestTotal,
        Self::UploadMaxBytes,
        Self::UploadIdle,
        Self::UploadTotal,
        Self::DownloadMaxBytes,
        Self::DownloadIdle,
        Self::DownloadTotal,
        Self::HeaderTimeout,
        Self::ShutdownTimeout,
        Self::ConnectionLimit,
        Self::HealthRoute,
        Self::ProfilingResponseLimit,
        Self::RequestBodyHardCap,
        Self::RequestBodyLimit,
    ];

    /// The two request deadlines.
    pub(super) const REQUEST: [Self; 2] = [Self::RequestBodyIdle, Self::RequestTotal];
    /// The three streaming-upload bounds.
    pub(super) const UPLOAD: [Self; 3] =
        [Self::UploadMaxBytes, Self::UploadIdle, Self::UploadTotal];
    /// The three streaming-download bounds.
    pub(super) const DOWNLOAD: [Self; 3] = [
        Self::DownloadMaxBytes,
        Self::DownloadIdle,
        Self::DownloadTotal,
    ];
    /// Every bound a router, host, or route registration can configure.
    pub(super) const ROUTE: [Self; 8] = [
        Self::RequestBodyIdle,
        Self::RequestTotal,
        Self::UploadMaxBytes,
        Self::UploadIdle,
        Self::UploadTotal,
        Self::DownloadMaxBytes,
        Self::DownloadIdle,
        Self::DownloadTotal,
    ];
    /// The server-only bounds that every build resolves.
    const SERVER_ONLY: [Self; 5] = [
        Self::HeaderTimeout,
        Self::ShutdownTimeout,
        Self::ConnectionLimit,
        Self::HealthRoute,
        Self::RequestBodyHardCap,
    ];

    /// Every bound a server policy resolves in this build.
    pub(super) fn server() -> impl Iterator<Item = Self> {
        #[cfg(feature = "profiling")]
        let profiling = Some(Self::ProfilingResponseLimit);
        // No profiler is compiled in, so no server resolves its maximum.
        #[cfg(not(feature = "profiling"))]
        let profiling = None;
        Self::ROUTE
            .into_iter()
            .chain(Self::SERVER_ONLY)
            .chain(profiling)
    }

    /// The stable name that logs and documentation use.
    pub const fn name(self) -> &'static str {
        match self {
            Self::RequestBodyIdle => "request.body_idle",
            Self::RequestTotal => "request.total",
            Self::UploadMaxBytes => "upload.max_bytes",
            Self::UploadIdle => "upload.idle",
            Self::UploadTotal => "upload.total",
            Self::DownloadMaxBytes => "download.max_bytes",
            Self::DownloadIdle => "download.idle",
            Self::DownloadTotal => "download.total",
            Self::HeaderTimeout => "header_timeout",
            Self::ShutdownTimeout => "shutdown_timeout",
            Self::ConnectionLimit => "connection_limit",
            Self::HealthRoute => "health_route",
            Self::ProfilingResponseLimit => "profiling_response_limit",
            Self::RequestBodyHardCap => "request_body_hard_cap",
            Self::RequestBodyLimit => "request_body_limit",
        }
    }

    /// How an outer value of this dimension contains an inner one.
    ///
    /// The runtime enforces its own shutdown deadline on every participant, so
    /// a contained server drain can never outlive it, even at its default.
    pub(super) const fn containment(self) -> Containment {
        match self {
            Self::ShutdownTimeout => Containment::Hard,
            Self::RequestBodyIdle
            | Self::RequestTotal
            | Self::UploadMaxBytes
            | Self::UploadIdle
            | Self::UploadTotal
            | Self::DownloadMaxBytes
            | Self::DownloadIdle
            | Self::DownloadTotal
            | Self::HeaderTimeout
            | Self::ConnectionLimit
            | Self::HealthRoute
            | Self::ProfilingResponseLimit
            | Self::RequestBodyHardCap
            | Self::RequestBodyLimit => Containment::Intent,
        }
    }

    /// The position of this dimension in [`Self::ALL`].
    const fn index(self) -> usize {
        self as usize
    }

    const fn bit(self) -> u16 {
        1 << self.index()
    }
}

impl fmt::Display for PolicyDimension {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How an outer bound contains an inner bound of the same dimension.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Containment {
    /// A defaulted value yields to an explicit one on either side.
    Intent,
    /// The outer value always bounds the inner one. An inner default still
    /// yields to an explicit outer value.
    Hard,
}

/// Which side of one containment edge supplies a resolved value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Supplier {
    /// The inner layer's value stands.
    Inner,
    /// The outer layer's value stands.
    Outer,
    /// The narrower of the two stands.
    Narrower,
}

impl Supplier {
    /// The value this side choice resolves to.
    pub(super) fn choose<T>(self, inner: T, outer: T, narrower: impl FnOnce(T, T) -> T) -> T {
        match self {
            Self::Inner => inner,
            Self::Outer => outer,
            Self::Narrower => narrower(inner, outer),
        }
    }
}

impl Containment {
    /// The side that supplies a value, from the intent of each side.
    pub(super) const fn supplier(self, inner_explicit: bool, outer_explicit: bool) -> Supplier {
        match (self, inner_explicit, outer_explicit) {
            (_, false, true) => Supplier::Outer,
            (Self::Intent, true, false) => Supplier::Inner,
            (Self::Hard, true, false) | (_, true, true) | (_, false, false) => Supplier::Narrower,
        }
    }
}

/// A set of dimensions, such as the ones a caller set explicitly.
#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub(super) struct DimensionSet(u16);

impl DimensionSet {
    /// No dimension.
    pub(super) const EMPTY: Self = Self(0);
    /// Both request deadlines.
    pub(super) const REQUEST: Self = Self::of(&PolicyDimension::REQUEST);
    /// The three streaming-upload bounds.
    pub(super) const UPLOAD: Self = Self::of(&PolicyDimension::UPLOAD);
    /// The three streaming-download bounds.
    pub(super) const DOWNLOAD: Self = Self::of(&PolicyDimension::DOWNLOAD);
    /// Every route-level bound.
    pub(super) const ROUTE: Self = Self::of(&PolicyDimension::ROUTE);

    /// The set of exactly `dimensions`.
    pub(super) const fn of(dimensions: &[PolicyDimension]) -> Self {
        let mut bits = 0;
        let mut next = 0;
        while next < dimensions.len() {
            bits |= dimensions[next].bit();
            next += 1;
        }
        Self(bits)
    }

    /// This set with `dimension` added.
    pub(super) const fn with(self, dimension: PolicyDimension) -> Self {
        Self(self.0 | dimension.bit())
    }

    /// Every dimension in either set.
    pub(super) const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The dimensions in both sets.
    pub(super) const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Whether `dimension` is in this set.
    pub(super) const fn contains(self, dimension: PolicyDimension) -> bool {
        self.0 & dimension.bit() != 0
    }

    fn iter(self) -> impl Iterator<Item = PolicyDimension> {
        PolicyDimension::ALL
            .into_iter()
            .filter(move |dimension| self.contains(*dimension))
    }
}

impl fmt::Debug for DimensionSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_set()
            .entries(self.iter().map(PolicyDimension::name))
            .finish()
    }
}

/// The layers that supplied each resolved dimension.
#[derive(Clone, Copy, Default, Eq, Hash, PartialEq)]
pub(super) struct DimensionSources([PolicyLayers; PolicyDimension::COUNT]);

impl DimensionSources {
    /// The layers that supplied `dimension`.
    pub(super) const fn get(&self, dimension: PolicyDimension) -> PolicyLayers {
        self.0[dimension.index()]
    }

    /// These sources with `dimension` supplied by `layers`.
    pub(super) const fn with(mut self, dimension: PolicyDimension, layers: PolicyLayers) -> Self {
        self.0[dimension.index()] = layers;
        self
    }

    /// Sources for every dimension `dimensions` yields, computed by `source`.
    pub(super) fn collect(
        dimensions: impl Iterator<Item = PolicyDimension>,
        source: impl Fn(PolicyDimension) -> PolicyLayers,
    ) -> Self {
        dimensions.fold(Self::default(), |sources, dimension| {
            sources.with(dimension, source(dimension))
        })
    }
}

impl fmt::Debug for DimensionSources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_map()
            .entries(
                PolicyDimension::ALL
                    .into_iter()
                    .filter(|dimension| !self.get(*dimension).is_empty())
                    .map(|dimension| (dimension.name(), self.get(dimension))),
            )
            .finish()
    }
}
