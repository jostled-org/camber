use super::handle::ConnCtx;
use super::router::Handler;
use super::{Request, Response};
use crate::resource::HealthState;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};

/// Which internal route was matched. Used to avoid per-request Box<dyn Fn> allocation.
pub(super) enum InternalRoute {
    Metrics(metrics_exporter_prometheus::PrometheusHandle),
    Health(HealthState),
    #[cfg(feature = "profiling")]
    Profiling(u64),
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
pub(super) fn invoke_internal_route(route: &InternalRoute) -> Response {
    match route {
        InternalRoute::Metrics(handle) => {
            let body = handle.render();
            Response::bytes_raw(200, body)
                .with_content_type("text/plain; version=0.0.4; charset=utf-8")
        }
        InternalRoute::Health(hs) => build_health_response(hs),
        #[cfg(feature = "profiling")]
        InternalRoute::Profiling(seconds) => invoke_profiling(*seconds),
    }
}

/// Run CPU profiling for the given duration and return a flamegraph SVG.
#[cfg(feature = "profiling")]
fn invoke_profiling(seconds: u64) -> Response {
    let guard = match start_profiling() {
        Ok(g) => g,
        Err(resp) => return resp,
    };
    std::thread::sleep(std::time::Duration::from_secs(seconds));
    render_flamegraph(guard)
}

/// Build a boxed handler for an internal route. Only used when middleware must wrap it.
pub(super) fn build_internal_handler(route: InternalRoute) -> Handler {
    Box::new(move |_: &Request| {
        let resp = invoke_internal_route(&route);
        Box::pin(async move { resp }) as Pin<Box<dyn Future<Output = Response> + Send>>
    })
}

/// Build a JSON health response from the health state array.
/// Returns 200 if all resources are healthy, 503 if any are unhealthy.
fn build_health_response(health_state: &[(Box<str>, AtomicBool)]) -> Response {
    let mut all_healthy = true;
    let mut resources = serde_json::Map::new();

    for (name, healthy) in health_state.iter() {
        let is_healthy: bool = healthy.load(Ordering::Acquire);
        let status = match is_healthy {
            true => "ok",
            false => {
                all_healthy = false;
                "error"
            }
        };
        resources.insert(name.to_string(), serde_json::Value::String(status.into()));
    }

    let status_label = match all_healthy {
        true => "healthy",
        false => "unhealthy",
    };

    let status_code = match all_healthy {
        true => 200,
        false => 503,
    };

    match Response::json(
        status_code,
        &serde_json::json!({
            "status": status_label,
            "resources": resources,
        }),
    ) {
        Ok(resp) => resp,
        Err(e) => Response::text_raw(500, &e.to_string()),
    }
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
fn start_profiling() -> Result<pprof::ProfilerGuard<'static>, Response> {
    pprof::ProfilerGuardBuilder::default()
        .frequency(1000)
        .build()
        .map_err(|e| Response::text_raw(500, &format!("profiler start failed: {e}")))
}

#[cfg(feature = "profiling")]
fn render_flamegraph(guard: pprof::ProfilerGuard<'_>) -> Response {
    let report = match guard.report().build() {
        Ok(r) => r,
        Err(e) => return Response::text_raw(500, &format!("profiler report failed: {e}")),
    };

    let mut svg = Vec::new();
    match report.flamegraph(&mut svg) {
        Ok(()) => Response::bytes_raw(200, svg).with_content_type("image/svg+xml"),
        Err(e) => Response::text_raw(500, &format!("flamegraph generation failed: {e}")),
    }
}
