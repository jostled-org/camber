use super::boundary::CrossedBound;
use super::completion::{CompletionTerminal, Telemetry};
use super::handle::ConnCtx;
use super::rejection::{RejectionKind, RejectionScope};

fn build_status_text_table() -> Box<[Box<str>]> {
    (100u16..600)
        .map(|code| Box::from(code.to_string()))
        .collect()
}

/// One finished request, named by what the peer was actually given.
///
/// Grouped rather than passed one value at a time, because every field is read
/// by both the event and the counters: a caller that supplied the status of one
/// response and the terminal of another would be describing a request that
/// never happened.
///
/// The identity is the scope every exit already names the request by, borrowed
/// rather than unpacked: the request identifier and the raw path are the two
/// values this record carries into an event and deliberately never into a
/// label, and reading them off one owner is what keeps that split in one place.
pub(super) struct Completed<'a> {
    pub(super) scope: &'a RejectionScope,
    pub(super) status: u16,
    pub(super) terminal: CompletionTerminal,
    pub(super) boundary: CrossedBound,
}

/// Record a completed request as a tracing event and Prometheus metrics.
///
/// The elapsed time is handed in rather than measured here: the clock belongs
/// to the request, and the owner that watched it end is the only one that can
/// say how long the whole head-to-terminal span was.
pub(super) fn record_request(
    telemetry: Telemetry,
    completed: &Completed<'_>,
    elapsed: std::time::Duration,
) {
    let &Completed {
        scope,
        status,
        terminal,
        boundary,
    } = completed;

    if telemetry.events() {
        tracing::info!(
            request_id = scope.request_id().as_str(),
            method = scope.method_label(),
            path = scope.path(),
            status,
            protocol = scope.protocol_label(),
            terminal = terminal.label(),
            boundary = boundary.label(),
            latency_ms = elapsed.as_millis(),
            "request completed"
        );
    }

    if telemetry.metrics() {
        // Built once and read by both instruments. Two spellings of one label
        // set are two things that can disagree, and a counter and a histogram
        // that disagree about how a request is labelled cannot be read together.
        let labels = [
            metrics::Label::from_static_parts("method", scope.method_label()),
            metrics::Label::from_static_parts("status", status_to_label(status)),
            metrics::Label::from_static_parts("protocol", scope.protocol_label()),
            metrics::Label::from_static_parts("terminal", terminal.label()),
            metrics::Label::from_static_parts("boundary", boundary.label()),
        ];
        metrics::counter!("http_requests_total", labels.iter()).increment(1);
        metrics::histogram!("http_request_duration_seconds", labels.iter())
            .record(elapsed.as_secs_f64());
    }
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
