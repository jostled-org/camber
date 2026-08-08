use super::handle::ConnCtx;
use super::rejection::{RejectionKind, RejectionScope, RequestId};

fn build_status_text_table() -> Box<[Box<str>]> {
    (100u16..600)
        .map(|code| Box::from(code.to_string()))
        .collect()
}

/// One finished request, named by what the peer was actually given.
///
/// Grouped rather than passed one value at a time, because every field is read
/// by both the event and the counters: a caller that supplied the status of one
/// response and the path of another would be describing a request that never
/// happened.
pub(super) struct Completed<'a> {
    pub(super) request_id: RequestId,
    pub(super) method: &'static str,
    pub(super) path: &'a str,
    pub(super) status: u16,
}

/// Record a completed request as a tracing event and Prometheus metrics.
pub(super) fn record_request(ctx: &ConnCtx, completed: Completed<'_>, start: std::time::Instant) {
    let elapsed = start.elapsed();
    let Completed {
        request_id,
        method,
        path,
        status,
    } = completed;

    if ctx.tracing_enabled {
        tracing::info!(
            request_id = request_id.as_str(),
            method,
            path,
            status,
            latency_ms = elapsed.as_millis(),
            "request completed"
        );
    }

    if ctx.metrics_handle.is_some() {
        let status_label = status_to_label(status);
        metrics::counter!(
            "http_requests_total",
            "method" => method,
            "status" => status_label,
        )
        .increment(1);
        metrics::histogram!(
            "http_request_duration_seconds",
            "method" => method,
            "status" => status_label,
        )
        .record(elapsed.as_secs_f64());
    }
}

/// Record one request answered through its rejection scope.
///
/// The scope is what names a request at the boundary every answer leaves
/// through, so the identity it recorded is read off the scope rather than
/// rebuilt beside it. Stated once because the buffered exit and the streaming
/// proxy exit both answer that way, and two copies are two things that can
/// disagree about what names a request.
pub(super) fn record_scoped(
    ctx: &ConnCtx,
    scope: &RejectionScope,
    status: u16,
    start: std::time::Instant,
) {
    record_request(
        ctx,
        Completed {
            request_id: scope.request_id(),
            method: scope.method_label(),
            path: scope.path(),
            status,
        },
        start,
    );
}

/// Count one refusal under its category and the status the peer was given.
///
/// Two labels, both from closed vocabularies: the category is one of the
/// taxonomy's static names and the status is one of 500 fixed strings. A
/// request identifier, a path, a route, a peer address, or an error string
/// would make this counter unbounded, so none of them is offered here to be
/// added by mistake.
pub(super) fn count_rejection(ctx: &ConnCtx, kind: RejectionKind, status: u16) {
    if ctx.metrics_handle.is_some() {
        metrics::counter!(
            "http_rejections_total",
            "kind" => kind.label(),
            "status" => status_to_label(status),
        )
        .increment(1);
    }
}

/// Return a static string label for an HTTP status code.
///
/// Common codes get `&'static str` with zero allocation. Rare codes
/// are cached in a fixed-size table initialized on first use — no memory leak.
fn status_to_label(status: u16) -> &'static str {
    match status {
        200 => "200",
        201 => "201",
        204 => "204",
        301 => "301",
        302 => "302",
        304 => "304",
        400 => "400",
        401 => "401",
        403 => "403",
        404 => "404",
        405 => "405",
        413 => "413",
        500 => "500",
        502 => "502",
        503 => "503",
        100..600 => {
            static TABLE: std::sync::OnceLock<Box<[Box<str>]>> = std::sync::OnceLock::new();
            let table = TABLE.get_or_init(build_status_text_table);
            &table[(status - 100) as usize]
        }
        _ => "unknown",
    }
}
