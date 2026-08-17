use super::handle::ConnCtx;
#[cfg(feature = "profiling")]
use super::profiling::ProfilingRequest;
use super::response::HandlerOutcome;
use super::router::Handler;
use super::{Request, Response};
use crate::RuntimeError;
use crate::resource::HealthState;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
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
    Health(HealthState),
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
            .health_state
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
                ctx.policy.profiling_response_limit_value(),
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

/// Build a JSON health response from the health state array.
/// Returns 200 if all resources are healthy, 503 if any are unhealthy.
///
/// One load per resource, into a snapshot both answers are then derived from.
/// Reading the flags a second time to summarize them would double the atomic
/// loads and let the status line describe a health state the resource map does
/// not — a resource that recovers between the two passes reports `error` beside
/// an overall `healthy`.
fn build_health_response(
    health_state: &[(Box<str>, AtomicBool)],
) -> Result<Response, RuntimeError> {
    let snapshot: Box<[(&str, bool)]> = health_state
        .iter()
        .map(|(name, healthy)| (name.as_ref(), healthy.load(Ordering::Acquire)))
        .collect();

    let all_healthy = snapshot.iter().all(|(_, healthy)| *healthy);

    let resources: serde_json::Map<String, serde_json::Value> = snapshot
        .iter()
        .map(|(name, healthy)| {
            let status = match *healthy {
                true => "ok",
                false => "error",
            };
            (
                (*name).to_owned(),
                serde_json::Value::String(status.to_owned()),
            )
        })
        .collect();

    let status_label = match all_healthy {
        true => "healthy",
        false => "unhealthy",
    };

    let status_code = match all_healthy {
        true => 200,
        false => 503,
    };

    Response::json(
        status_code,
        &serde_json::json!({
            "status": status_label,
            "resources": resources,
        }),
    )
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
