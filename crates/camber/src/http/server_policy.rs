//! The complete operating envelope one HTTP server serves under.

use super::health_route::HealthRoute;
use super::policy_dimension::{DimensionSet, DimensionSources, PolicyDimension};
use super::policy_report::PolicyValue;
use super::policy_resolution::{Located, PolicyValues};
use super::policy_sources::{PolicyLayer, PolicyLayers};
use super::policy_value::positive_limit;
use super::policy_value::{MAX_POLICY_DEADLINE, finite_duration, limit_within, narrow};
use super::request_budget::RequestBudget;
use super::route_budgets::RouteBudgets;
use super::server::MAX_CONNECTION_LIMIT;
use super::transfer_budget::TransferBudget;
use crate::RuntimeError;
use std::time::Duration;

/// The default pre-head wait for a complete request head.
pub(crate) const DEFAULT_HEADER_TIMEOUT: Duration = Duration::from_secs(60);
/// The default deadline every graceful shutdown participant shares.
pub(crate) const DEFAULT_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(30);
/// The default body-idle and request-total deadlines.
const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// The default ceiling that contains every configured request-body limit.
const DEFAULT_REQUEST_BODY_HARD_CAP: usize = 256 * 1024 * 1024;
/// The default maximum a rendered profiling response is retained under: eight
/// MiB, the same ceiling Camber's ordinary buffered collections use.
#[cfg(feature = "profiling")]
pub const DEFAULT_PROFILING_RESPONSE_LIMIT: usize = 8 * 1024 * 1024;

/// Every bound one server applies to the work it admits.
///
/// A server policy is the sole authority under bare Tokio. Inside a Camber
/// runtime the runtime's own policy contains it, per dimension:
///
/// - When both set a dimension explicitly, the narrower value applies.
/// - When only one sets it explicitly, that value applies. A default never
///   narrows an explicit setting.
/// - When neither sets it, the narrower default applies.
///
/// The runtime shutdown deadline is the one exception. The runtime enforces it
/// on every participant, so a server drain never outlives it, even when it is
/// the runtime's default.
///
/// | Dimension | Default |
/// | --- | --- |
/// | `header_timeout` | 60 seconds |
/// | request `body_idle` | 30 seconds |
/// | request `total` | 30 seconds |
/// | `upload_budget` | [`TransferBudget::unbounded`] |
/// | `download_budget` | [`TransferBudget::unbounded`] |
/// | `request_body_hard_cap` | 256 MiB |
/// | `shutdown_timeout` | 30 seconds |
/// | `profiling_response_limit` | eight MiB |
/// | `connection_limit` | none — unbounded |
/// | `health_route` | [`HealthRoute::Default`] — `/health` |
///
/// With the `profiling` feature, the rendered-profile maximum is finite by default.
/// Only `unbounded_profiling_response` removes it; omission keeps the finite default.
///
/// The two streaming defaults are unbounded on purpose: a long-lived stream
/// that its registration did not bound stays open. Their channels remain
/// memory-bounded by the configured buffer depth. A production service that
/// serves untrusted peers should select finite streaming budgets and a finite
/// [`connection_limit`](Self::connection_limit); omitting the connection limit
/// is intended for development, tests, or a service behind an admission
/// boundary that already enforces one.
///
/// ```rust
/// use camber::http::{RequestBudget, ServerPolicy};
/// use std::time::Duration;
///
/// # fn main() -> Result<(), camber::RuntimeError> {
/// let policy = ServerPolicy::default()
///     .header_timeout(Duration::from_secs(10))?
///     .request_budget(RequestBudget::bounded(
///         Duration::from_secs(5),
///         Duration::from_secs(15),
///     )?)
///     .connection_limit(1024)?;
/// assert_ne!(policy, ServerPolicy::default());
/// # Ok(())
/// # }
/// ```
///
/// Equality compares configuration, which includes intent: a setter that
/// restates a documented default still records an explicit choice. Compare
/// [`EffectiveServerPolicy::same_bounds`](super::EffectiveServerPolicy::same_bounds)
/// when only the enforced values matter.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ServerPolicy {
    header_timeout: Duration,
    request: RequestBudget,
    upload: TransferBudget,
    download: TransferBudget,
    shutdown_timeout: Duration,
    /// The maximum a rendered profile is retained under, or `None` for the
    /// explicit opt-out.
    #[cfg(feature = "profiling")]
    profiling_response: Option<usize>,
    connection_limit: Option<usize>,
    health_route: HealthRoute,
    request_body_hard_cap: usize,
    /// The dimensions a setter chose rather than left at their defaults.
    explicit: DimensionSet,
}

/// The pre-head wait a test runtime serves under.
pub(crate) const TEST_HEADER_TIMEOUT: Duration = Duration::from_millis(100);
/// The aggregate shutdown deadline a test runtime serves under.
pub(crate) const TEST_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(1);

// The two test bounds are literals this crate chooses, not caller input, so
// `finite_duration`'s two rules are stated to the compiler here rather than to
// a `Result` arm no value could reach. A retune that put either bound outside
// the range a policy can carry fails the build instead of handing every
// `#[camber::test]` the production envelope under a refusal nobody reads.
const _: () = assert!(!TEST_HEADER_TIMEOUT.is_zero());
const _: () = assert!(TEST_HEADER_TIMEOUT.as_secs() <= MAX_POLICY_DEADLINE.as_secs());
const _: () = assert!(!TEST_SHUTDOWN_TIMEOUT.is_zero());
const _: () = assert!(TEST_SHUTDOWN_TIMEOUT.as_secs() <= MAX_POLICY_DEADLINE.as_secs());

impl ServerPolicy {
    /// One envelope over the two deadlines a caller may choose between.
    ///
    /// `Default::default` is not a `const fn`, so the constant test envelope
    /// cannot reach the defaults through the trait. Both spellings read this
    /// one literal, so the dimensions neither of them sets cannot drift apart.
    const fn of(header_timeout: Duration, shutdown_timeout: Duration) -> Self {
        Self {
            header_timeout,
            request: RequestBudget::from_constants(
                DEFAULT_REQUEST_TIMEOUT,
                DEFAULT_REQUEST_TIMEOUT,
            ),
            upload: TransferBudget::unbounded(),
            download: TransferBudget::unbounded(),
            shutdown_timeout,
            #[cfg(feature = "profiling")]
            profiling_response: Some(DEFAULT_PROFILING_RESPONSE_LIMIT),
            connection_limit: None,
            health_route: HealthRoute::Default,
            request_body_hard_cap: DEFAULT_REQUEST_BODY_HARD_CAP,
            explicit: DimensionSet::EMPTY,
        }
    }

    /// The envelope `runtime::test` and `#[camber::test]` serve under.
    ///
    /// A constant rather than a builder chain, because both bounds are fixed
    /// and validated above: routing them through the refusing setters would
    /// leave every test entry point carrying a refusal arm no value can reach,
    /// and the only ways to spend it are to propagate a `Result` that never
    /// arrives or to substitute the production envelope in silence.
    pub(crate) const fn of_test_bounds() -> Self {
        Self::of(TEST_HEADER_TIMEOUT, TEST_SHUTDOWN_TIMEOUT)
    }
}

impl Default for ServerPolicy {
    fn default() -> Self {
        Self::of(DEFAULT_HEADER_TIMEOUT, DEFAULT_SHUTDOWN_TIMEOUT)
    }
}

impl ServerPolicy {
    /// Set how long Hyper may wait for a complete HTTP/1 request head.
    ///
    /// This is a pre-head bound and nothing else. It is independent of request
    /// body idle time, request total time, connection admission, and shutdown:
    /// a peer that never finishes its head has no request, no request ID, and
    /// no mapped rejection — its transport is closed. Hyper exposes no
    /// equivalent HTTP/2 per-stream partial-HEADERS timer; HTTP/2 request
    /// budgets begin after Hyper delivers a complete head.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero or
    /// longer than the thirty-year ceiling every Camber deadline shares.
    pub fn header_timeout(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            header_timeout: finite_duration(timeout, "header_timeout")?,
            explicit: self.explicit.with(PolicyDimension::HeaderTimeout),
            ..self
        })
    }

    /// Set the default deadlines for every request this server admits.
    #[must_use]
    pub fn request_budget(self, budget: RequestBudget) -> Self {
        Self {
            request: budget,
            explicit: self.explicit.union(DimensionSet::REQUEST),
            ..self
        }
    }

    /// Set the finite ceiling containing every router's request-body limit.
    ///
    /// Defaults to 256 MiB. The cap always contains router and host limits,
    /// even at its default, so a limit above 256 MiB needs this setting too.
    /// Raising this ceiling does not raise an unconfigured router's 8 MiB
    /// request limit. An explicit runtime cap may narrow this value.
    ///
    /// # Errors
    /// Returns [`RuntimeError::InvalidArgument`] when `bytes` is zero.
    /// A router may still select a zero-byte request limit.
    pub fn request_body_hard_cap(self, bytes: usize) -> Result<Self, RuntimeError> {
        Ok(Self {
            request_body_hard_cap: positive_limit(bytes, "request_body_hard_cap")?,
            explicit: self.explicit.with(PolicyDimension::RequestBodyHardCap),
            ..self
        })
    }

    /// Set the default budget for streaming uploads.
    #[must_use]
    pub fn upload_budget(self, budget: TransferBudget) -> Self {
        Self {
            upload: budget,
            explicit: self.explicit.union(DimensionSet::UPLOAD),
            ..self
        }
    }

    /// Set the default budget for streaming downloads.
    #[must_use]
    pub fn download_budget(self, budget: TransferBudget) -> Self {
        Self {
            download: budget,
            explicit: self.explicit.union(DimensionSet::DOWNLOAD),
            ..self
        }
    }

    /// Set the one deadline every graceful shutdown participant shares.
    ///
    /// Inside a Camber runtime this is the server's own drain limit. The
    /// runtime's shutdown deadline still bounds it, because the runtime stops
    /// every participant when its deadline expires. An unset server drain
    /// takes an explicit runtime deadline.
    ///
    /// Cooperative work may finish until it expires. It bounds Camber's own
    /// waiting and escalation; it cannot stop an async task that never yields
    /// or an application callback running on a blocking thread.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when the duration is zero or
    /// longer than the thirty-year ceiling every Camber deadline shares. The
    /// first graceful transition adds this value to the clock, which panics on
    /// a deadline it cannot represent rather than saturating.
    pub fn shutdown_timeout(self, timeout: Duration) -> Result<Self, RuntimeError> {
        Ok(Self {
            shutdown_timeout: finite_duration(timeout, "shutdown_timeout")?,
            explicit: self.explicit.with(PolicyDimension::ShutdownTimeout),
            ..self
        })
    }

    /// Limit how many connections this listener admits at once.
    ///
    /// The permit is taken before TLS or HTTP work and released once, when the
    /// last owner of the transport — HTTP connection task, SSE response, gRPC
    /// stream, or direct/proxied WebSocket bridge — lets it go.
    ///
    /// Omitting the limit is unbounded. Production services should set one.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when `limit` is zero, which
    /// would admit nothing at all, or when it exceeds the largest limit the
    /// admission semaphore can hold. The refusal names that ceiling, so it is
    /// read from the semaphore rather than restated here. A limit that large
    /// bounds nothing a listener can reach; unbounded admission is spelled by
    /// omitting the limit.
    pub fn connection_limit(self, limit: usize) -> Result<Self, RuntimeError> {
        Ok(Self {
            connection_limit: Some(limit_within(
                limit,
                MAX_CONNECTION_LIMIT,
                "connection_limit",
            )?),
            explicit: self.explicit.with(PolicyDimension::ConnectionLimit),
            ..self
        })
    }

    /// Cap the rendered profiling response this server retains.
    ///
    /// Sampling and rendering run on a blocking thread, and the renderer's
    /// output is accounted before it is retained: the write that would carry the
    /// answer past this maximum is dropped, and the request is refused rather
    /// than answered with a partial profile.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when `max_bytes` is zero, which
    /// would retain nothing at all. Unbounded output is spelled by
    /// [`unbounded_profiling_response`](Self::unbounded_profiling_response).
    #[cfg(feature = "profiling")]
    pub fn profiling_response_limit(self, max_bytes: usize) -> Result<Self, RuntimeError> {
        Ok(Self {
            profiling_response: Some(positive_limit(max_bytes, "profiling_response_limit")?),
            explicit: self.explicit.with(PolicyDimension::ProfilingResponseLimit),
            ..self
        })
    }

    /// Retain a rendered profiling response with no size ceiling.
    ///
    /// **Warning:** a flamegraph grows with the number of distinct stacks the
    /// sampler found, so a process with deep or wide concurrency renders an
    /// answer this server then holds entirely in memory. Use it when a profile
    /// this service must produce is being truncated by the default, and prefer
    /// naming a larger [`profiling_response_limit`](Self::profiling_response_limit)
    /// where a number is known. This is the only spelling that removes the
    /// maximum.
    ///
    /// Inside a Camber runtime, an explicit runtime maximum still contains
    /// this choice. The runtime's default maximum does not.
    #[must_use]
    #[cfg(feature = "profiling")]
    pub const fn unbounded_profiling_response(self) -> Self {
        Self {
            profiling_response: None,
            explicit: self.explicit.with(PolicyDimension::ProfilingResponseLimit),
            ..self
        }
    }

    /// Choose whether this server answers Camber's resource health report, and
    /// at which path.
    ///
    /// See [`HealthRoute`] for how the choice narrows under a runtime's policy
    /// and which router routes it refuses.
    ///
    /// # Errors
    ///
    /// Returns [`RuntimeError::InvalidArgument`] when [`HealthRoute::Path`]
    /// names a path the report cannot be served at.
    pub fn health_route(self, route: HealthRoute) -> Result<Self, RuntimeError> {
        Ok(Self {
            health_route: route.validated()?,
            explicit: self.explicit.with(PolicyDimension::HealthRoute),
            ..self
        })
    }

    // The readers below carry a `_value` suffix because each dimension's
    // natural name is already the setter this immutable value is built with.
    // They are crate-internal: a caller configures a policy and compares
    // policies, and the fields stay private so a validated value cannot be
    // edited past its constructor.

    /// The configured pre-head wait for a complete request head.
    pub(crate) const fn header_timeout_value(&self) -> Duration {
        self.header_timeout
    }

    pub(crate) const fn request_body_hard_cap_value(&self) -> usize {
        self.request_body_hard_cap
    }

    /// The configured aggregate shutdown deadline.
    pub(crate) const fn shutdown_timeout_value(&self) -> Duration {
        self.shutdown_timeout
    }

    /// The configured connection limit, or `None` for unbounded admission.
    pub(crate) const fn connection_limit_value(&self) -> Option<usize> {
        self.connection_limit
    }

    /// The configured health route.
    pub(crate) const fn health_route_value(&self) -> HealthRoute {
        self.health_route
    }

    /// The request and transfer budgets this policy supplies to every route.
    pub(super) fn route_budgets(self) -> RouteBudgets {
        RouteBudgets::new(
            self.request,
            self.upload,
            self.download,
            self.explicit.intersection(DimensionSet::ROUTE),
        )
    }

    /// This policy at `layer`: every dimension names that layer, defaulted or not.
    pub(super) fn located(self, layer: PolicyLayer) -> Located<Self> {
        Located::new(
            self,
            DimensionSources::collect(Self::dimensions(), |dimension| {
                PolicyLayers::one(layer, !self.explicit.contains(dimension))
            }),
        )
    }
}

impl PolicyValues for ServerPolicy {
    fn dimensions() -> impl Iterator<Item = PolicyDimension> {
        PolicyDimension::server()
    }

    fn explicit(&self) -> DimensionSet {
        self.explicit
    }

    fn value(&self, dimension: PolicyDimension) -> Option<PolicyValue> {
        match dimension {
            PolicyDimension::RequestBodyIdle
            | PolicyDimension::RequestTotal
            | PolicyDimension::UploadMaxBytes
            | PolicyDimension::UploadIdle
            | PolicyDimension::UploadTotal
            | PolicyDimension::DownloadMaxBytes
            | PolicyDimension::DownloadIdle
            | PolicyDimension::DownloadTotal => self.route_budgets().value(dimension),
            PolicyDimension::HeaderTimeout => {
                Some(PolicyValue::Duration(Some(self.header_timeout)))
            }
            PolicyDimension::ShutdownTimeout => {
                Some(PolicyValue::Duration(Some(self.shutdown_timeout)))
            }
            PolicyDimension::ConnectionLimit => Some(PolicyValue::Bytes(self.connection_limit)),
            PolicyDimension::HealthRoute => Some(PolicyValue::Health(self.health_route)),
            PolicyDimension::ProfilingResponseLimit => self.profiling_value(),
            PolicyDimension::RequestBodyHardCap => {
                Some(PolicyValue::Bytes(Some(self.request_body_hard_cap)))
            }
            PolicyDimension::RequestBodyLimit => None,
        }
    }

    /// Every dimension resolves independently through its containment rule.
    ///
    /// The health route keeps its own exposure rule, which already lets a
    /// server that named nothing take the runtime's choice.
    fn contained_by(self, outer: Self) -> Self {
        let budgets = self.route_budgets().contained_by(outer.route_budgets());
        let supplier = |dimension| self.supplier(&outer, dimension);
        Self {
            header_timeout: supplier(PolicyDimension::HeaderTimeout).choose(
                self.header_timeout,
                outer.header_timeout,
                Ord::min,
            ),
            request: budgets.request(),
            upload: budgets.upload(),
            download: budgets.download(),
            shutdown_timeout: supplier(PolicyDimension::ShutdownTimeout).choose(
                self.shutdown_timeout,
                outer.shutdown_timeout,
                Ord::min,
            ),
            #[cfg(feature = "profiling")]
            profiling_response: supplier(PolicyDimension::ProfilingResponseLimit).choose(
                self.profiling_response,
                outer.profiling_response,
                narrow,
            ),
            connection_limit: supplier(PolicyDimension::ConnectionLimit).choose(
                self.connection_limit,
                outer.connection_limit,
                narrow,
            ),
            health_route: self.health_route.narrowed_by(outer.health_route),
            request_body_hard_cap: supplier(PolicyDimension::RequestBodyHardCap).choose(
                self.request_body_hard_cap,
                outer.request_body_hard_cap,
                Ord::min,
            ),
            explicit: self.explicit.union(outer.explicit),
        }
    }
}

impl ServerPolicy {
    #[cfg(feature = "profiling")]
    const fn profiling_value(&self) -> Option<PolicyValue> {
        Some(PolicyValue::Bytes(self.profiling_response))
    }

    /// No profiler is compiled in, so no server resolves this dimension.
    #[cfg(not(feature = "profiling"))]
    const fn profiling_value(&self) -> Option<PolicyValue> {
        None
    }
}

/// The rendered-profile maximum one served profiling route freezes from `policy`.
///
/// The single reader of that dimension: the built-in profiling route freezes this
/// value at match time, the render it performs measures against it, and the
/// focused contract reads it here rather than restating the documented default as
/// a number of its own. `None` is the named opt-out, never a zero.
///
/// Pure: one field out, no allocation and no state.
#[cfg(feature = "profiling")]
#[doc(hidden)]
pub const fn frozen_profiling_response_limit(policy: &ServerPolicy) -> Option<usize> {
    policy.profiling_response
}
