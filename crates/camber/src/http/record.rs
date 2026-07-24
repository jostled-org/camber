use super::handle::ConnCtx;

fn build_status_text_table() -> Box<[Box<str>]> {
    (100u16..600)
        .map(|code| Box::from(code.to_string()))
        .collect()
}

/// Record a completed request as a tracing event and Prometheus metrics.
pub(super) fn record_request(
    ctx: &ConnCtx,
    method: &'static str,
    path: &str,
    status: u16,
    start: std::time::Instant,
) {
    let elapsed = start.elapsed();

    if ctx.tracing_enabled {
        tracing::info!(
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
