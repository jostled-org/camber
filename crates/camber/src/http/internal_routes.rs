use super::handle::ConnCtx;
#[cfg(feature = "profiling")]
use super::profiling::ProfilingRequest;
use super::response::HandlerOutcome;
use super::router::Handler;
#[cfg(feature = "profiling")]
use super::server_policy::frozen_profiling_response_limit;
use super::{Request, Response};
use crate::RuntimeError;
use crate::resource::entry::ResourceEntry;
use crate::resource::registry::ResourceRegistry;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

/// The fixed identity `/metrics` is named by.
static METRICS_ROUTE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("/metrics"));

/// The fixed identity `/health` is named by.
static HEALTH_ROUTE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("/health"));

/// The fixed identity `/debug/pprof/cpu` is named by.
#[cfg(feature = "profiling")]
static PROFILING_ROUTE: LazyLock<Arc<str>> = LazyLock::new(|| Arc::from("/debug/pprof/cpu"));

/// Which internal route was matched. Used to avoid per-request Box<dyn Fn> allocation.
pub(super) enum InternalRoute {
    Metrics(metrics_exporter_prometheus::PrometheusHandle),
    Health(ResourceRegistry),
    #[cfg(feature = "profiling")]
    Profiling(ProfilingRequest),
}

impl InternalRoute {
    /// The fixed pattern this route is named by.
    ///
    /// Internal routes are matched from the path rather than registered in the
    /// trie, so they have no frozen pattern to carry. One shared identity per
    /// route, minted once, keeps a refusal here nameable without allocating on
    /// the request path.
    pub(super) fn route(&self) -> Arc<str> {
        match self {
            Self::Metrics(_) => Arc::clone(&METRICS_ROUTE),
            Self::Health(_) => Arc::clone(&HEALTH_ROUTE),
            #[cfg(feature = "profiling")]
            Self::Profiling(_) => Arc::clone(&PROFILING_ROUTE),
        }
    }
}

/// Identify an internal route from path and query alone (no Request needed).
///
/// Used before body collection to bypass buffering for internal routes.
pub(super) fn match_internal_route_from_path(path: &str, ctx: &ConnCtx) -> Option<InternalRoute> {
    match path {
        "/metrics" => ctx.metrics_handle.clone().map(InternalRoute::Metrics),
        "/health" => ctx
            .resources
            .as_ref()
            .map(|hs| InternalRoute::Health(hs.clone())),
        _ => None,
    }
}

/// Check if a path matches the profiling internal route.
///
/// The sampling window and the output maximum are both frozen here: the window
/// is the peer's, clamped, and the maximum is the serving policy's at the instant
/// this request was matched.
#[cfg(feature = "profiling")]
pub(super) fn match_profiling_route(
    path: &str,
    query: Option<&str>,
    ctx: &ConnCtx,
) -> Option<InternalRoute> {
    match path {
        "/debug/pprof/cpu" if ctx.profiling_enabled => {
            Some(InternalRoute::Profiling(ProfilingRequest::new(
                parse_profiling_seconds_from_query(query),
                frozen_profiling_response_limit(&ctx.policy),
            )))
        }
        _ => None,
    }
}

/// Execute an internal route directly, bypassing handler boxing.
///
/// Fallible for the same reason a handler is: a route Camber could not build a
/// response for is a refusal the rejection boundary classifies and redacts,
/// rather than a status this file invents alongside its cause.
///
/// Async because one of these routes waits: the profiler samples for as long as
/// the peer asked for, and a route that answered that wait from the caller's
/// thread would hold whatever the caller was running on.
pub(super) async fn invoke_internal_route(route: &InternalRoute) -> Result<Response, RuntimeError> {
    match route {
        InternalRoute::Metrics(handle) => Ok(Response::bytes_raw(200, handle.render())
            .with_content_type("text/plain; version=0.0.4; charset=utf-8")),
        InternalRoute::Health(hs) => build_health_response(hs),
        // The whole answer belongs to `profiling`: it samples for as long as the
        // peer asked for and retains what it renders under the maximum this route
        // froze, both on a thread that may block.
        #[cfg(feature = "profiling")]
        InternalRoute::Profiling(request) => request.answer().await,
    }
}

/// Build a boxed handler for an internal route. Only used when middleware must wrap it.
///
/// The route is shared rather than captured by value: the handler is a `Fn`, so
/// every call needs its own handle to await through, and computing the answer
/// once at build time would have run the profiler's wait before the chain the
/// handler was built for had entered.
pub(super) fn build_internal_handler(route: InternalRoute) -> Handler {
    let route = Arc::new(route);
    Box::new(move |_: &Request| {
        let route = Arc::clone(&route);
        Box::pin(async move { invoke_internal_route(&route).await })
            as Pin<Box<dyn Future<Output = HandlerOutcome> + Send>>
    })
}

/// Build the JSON health response from every registered resource's published
/// state. Returns 200 when all of them are well, 503 when any is not.
///
/// One read per resource, into a snapshot both answers are then derived from.
/// Reading each entry a second time to summarize it would let the status line
/// describe a health state the resource map does not — a resource that recovers
/// between the two passes reports `error` beside an overall `healthy`.
fn build_health_response(resources: &[Arc<ResourceEntry>]) -> Result<Response, RuntimeError> {
    let snapshot: Box<[(&str, ResourceStatus)]> = resources
        .iter()
        .map(|entry| {
            (
                entry.name().as_ref(),
                ResourceStatus::published(entry.published_failure()),
            )
        })
        .collect();

    let all_healthy = snapshot
        .iter()
        .all(|(_, status)| matches!(status, ResourceStatus::Ok));

    let (status, code) = match all_healthy {
        true => ("healthy", 200),
        false => ("unhealthy", 503),
    };

    Response::json(
        code,
        &HealthReport {
            status,
            resources: ResourceMap(&snapshot),
        },
    )
}

/// What `/health` says about one resource.
///
/// The failure is the closed name its phase published, never the cause behind
/// it: a peer reading this body learns which of the five outcomes happened, and
/// the arbitrary text lives in the operator event the coordinator emitted.
#[derive(serde::Serialize)]
#[serde(tag = "status", rename_all = "lowercase")]
enum ResourceStatus {
    Ok,
    Error { failure: &'static str },
}

impl ResourceStatus {
    /// Name what one resource's published state is reported as.
    const fn published(failure: Option<&'static str>) -> Self {
        match failure {
            None => Self::Ok,
            Some(failure) => Self::Error { failure },
        }
    }
}

/// The whole `/health` body.
#[derive(serde::Serialize)]
struct HealthReport<'a> {
    status: &'static str,
    resources: ResourceMap<'a>,
}

/// The resource map, serialized in registration order.
///
/// Written by hand because `serde_json`'s own map orders its keys, and the
/// order an operator reads has to be the order the resources were registered
/// in — the same order their callbacks are invoked in.
struct ResourceMap<'a>(&'a [(&'a str, ResourceStatus)]);

impl serde::Serialize for ResourceMap<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.0.len()))?;
        for (name, status) in self.0 {
            map.serialize_entry(name, status)?;
        }
        map.end()
    }
}

/// How long the peer asked to be profiled for, inside the allowed window.
///
/// A missing, unparseable, or over-long window becomes a value this process is
/// willing to spend a blocking thread on rather than a refusal, because the
/// endpoint is an operator's and the answer is the same shape either way.
#[cfg(feature = "profiling")]
fn parse_profiling_seconds_from_query(query: Option<&str>) -> u64 {
    query
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("seconds=")))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(DEFAULT_PROFILING_SECONDS)
        .min(MAX_PROFILING_SECONDS)
}

/// How long a request that names no window is profiled for.
#[cfg(feature = "profiling")]
const DEFAULT_PROFILING_SECONDS: u64 = 5;

/// The longest window any request can ask to be profiled for.
#[cfg(feature = "profiling")]
const MAX_PROFILING_SECONDS: u64 = 60;
