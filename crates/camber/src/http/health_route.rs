//! Where one server exposes Camber's resource health report, if it does.

use super::internal_routes::{DEFAULT_HEALTH_PATH, METRICS_PATH, PROFILING_PATH};
use crate::RuntimeError;

/// Whether a server answers Camber's resource health report, and at which path.
///
/// The runtime owns the registered resources and runs every health check
/// whatever this says. This only decides whether a listener answers the report:
/// a service that keeps its own health contract disables it, or moves it, and
/// still registers resources for their lifecycle.
///
/// A server serves the report only when its runtime has at least one resource.
/// While the report is served, a router route at the same exact path is refused
/// when the server starts, because Camber's route would shadow it. A parameter
/// or wildcard route that also matches the path is not refused, and still
/// yields to the report.
///
/// Inside a runtime, the server's policy narrows the runtime's: `Disabled` on
/// either side disables the report, a server's `Path` wins over the runtime's
/// choice, and a server's `Default` keeps whatever the runtime chose.
///
/// ```rust
/// use camber::http::{HealthRoute, ServerPolicy};
///
/// # fn main() -> Result<(), camber::RuntimeError> {
/// let admin = ServerPolicy::default().health_route(HealthRoute::Path("/ops/health"))?;
/// let public = ServerPolicy::default().health_route(HealthRoute::Disabled)?;
/// assert_ne!(admin, public);
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HealthRoute {
    /// Serve the report at `/health`, or at the path the runtime chose.
    #[default]
    Default,
    /// Serve the report at this exact path.
    ///
    /// The path must start with `/`, name only static segments, carry no empty
    /// segment or trailing `/`, carry no query or fragment, and not be one of
    /// Camber's other internal routes (`/metrics`, `/debug/pprof/cpu`).
    Path(&'static str),
    /// Serve no report on this server.
    Disabled,
}

impl HealthRoute {
    /// This route, or the refusal that names why its path cannot be served.
    pub(super) fn validated(self) -> Result<Self, RuntimeError> {
        match self {
            Self::Path(path) => path_defect(path).map_or(Ok(self), |defect| {
                Err(RuntimeError::InvalidArgument(
                    format!("health_route path {path:?} {defect}").into_boxed_str(),
                ))
            }),
            Self::Default | Self::Disabled => Ok(self),
        }
    }

    /// This server's choice applied under the runtime's.
    ///
    /// Exposure only narrows: either side disabling the report disables it. A
    /// server that named a path keeps it, and one that named nothing keeps the
    /// runtime's choice.
    pub(super) const fn narrowed_by(self, outer: Self) -> Self {
        match (self, outer) {
            (Self::Disabled, _) | (_, Self::Disabled) => Self::Disabled,
            (Self::Path(_), _) => self,
            (Self::Default, outer) => outer,
        }
    }

    /// The path the report is served at, or `None` when it is not served.
    pub(super) const fn path(self) -> Option<&'static str> {
        match self {
            Self::Default => Some(DEFAULT_HEALTH_PATH),
            Self::Path(path) => Some(path),
            Self::Disabled => None,
        }
    }
}

/// Why a health path cannot be served, or `None` when it can.
///
/// The internal route matches the raw request path exactly, while the router
/// matches normalized segments. A path in any other shape would answer requests
/// that no router registration could be checked against.
fn path_defect(path: &str) -> Option<&'static str> {
    let segments = path.strip_prefix('/').map(|rest| rest.split('/'));
    match segments {
        None => Some("must start with '/'"),
        Some(_) if path.contains(['?', '#']) => Some("must not carry a query or fragment"),
        Some(_) if path == METRICS_PATH || path == PROFILING_PATH => {
            Some("is another internal route")
        }
        Some(_) if path == "/" => None,
        Some(mut segments) => segments.find_map(segment_defect),
    }
}

/// Why one path segment cannot be served, or `None` when it can.
fn segment_defect(segment: &str) -> Option<&'static str> {
    match segment.chars().next() {
        None => Some("must not have an empty segment or a trailing '/'"),
        Some(':' | '*') => Some("must name only static segments"),
        Some(_) => None,
    }
}
