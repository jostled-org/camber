use super::handle::ConnCtx;
use super::router::Handler;
use super::trie::HandlerOutcome;
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
    Profiling(u64),
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
#[cfg(feature = "profiling")]
pub(super) fn match_profiling_route(
    path: &str,
    query: Option<&str>,
    ctx: &ConnCtx,
) -> Option<InternalRoute> {
    match path {
        "/debug/pprof/cpu" if ctx.profiling_enabled => Some(InternalRoute::Profiling(
            parse_profiling_seconds_from_query(query),
        )),
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
        #[cfg(feature = "profiling")]
        InternalRoute::Profiling(seconds) => invoke_profiling(*seconds).await,
    }
}

/// Run CPU profiling for the given duration and return a flamegraph SVG.
///
/// The sampling window is a real thread sleep of up to a minute, so it runs on
/// a blocking thread rather than on the worker that accepted the request: one
/// `/debug/pprof/cpu?seconds=60` parked a worker for that whole minute. The
/// guard is taken and dropped inside the blocking closure, so the profiler's
/// registration never crosses an await and never outlives the thread it sampled.
#[cfg(feature = "profiling")]
async fn invoke_profiling(seconds: u64) -> Result<Response, RuntimeError> {
    let sampled = tokio::task::spawn_blocking(move || {
        let guard = start_profiling()?;
        std::thread::sleep(std::time::Duration::from_secs(seconds));
        render_flamegraph(guard)
    })
    .await;
    match sampled {
        Ok(rendered) => rendered,
        // Neither outcome is a transport failure, so neither is reported as
        // one. A panic inside the profiler carries a payload `panic_to_error`
        // can name, and flattening it into `Http` text lost both the name and
        // the category; anything else the join reports is the task going away
        // under it.
        Err(error) if error.is_panic() => Err(crate::task::panic_to_error(error.into_panic())),
        Err(_) => Err(RuntimeError::Cancelled),
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

#[cfg(feature = "profiling")]
fn parse_profiling_seconds_from_query(query: Option<&str>) -> u64 {
    query
        .and_then(|q| q.split('&').find_map(|pair| pair.strip_prefix("seconds=")))
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(5)
        .min(60)
}

#[cfg(feature = "profiling")]
fn start_profiling() -> Result<pprof::ProfilerGuard<'static>, RuntimeError> {
    pprof::ProfilerGuardBuilder::default()
        .frequency(1000)
        .build()
        .map_err(|e| RuntimeError::Http(format!("profiler start failed: {e}").into()))
}

#[cfg(feature = "profiling")]
fn render_flamegraph(guard: pprof::ProfilerGuard<'_>) -> Result<Response, RuntimeError> {
    let report = guard
        .report()
        .build()
        .map_err(|e| RuntimeError::Http(format!("profiler report failed: {e}").into()))?;

    let mut svg = Vec::new();
    report
        .flamegraph(&mut svg)
        .map_err(|e| RuntimeError::Http(format!("flamegraph generation failed: {e}").into()))?;
    Ok(Response::bytes_raw(200, svg).with_content_type("image/svg+xml"))
}
