use super::completion::{CompletionSnapshot, Telemetry};
use super::handle::ConnCtx;
use super::rejection::{RejectionKind, RequestIdentity};

fn build_status_text_table() -> Box<[Box<str>]> {
    (100u16..600)
        .map(|code| Box::from(code.to_string()))
        .collect()
}

/// Record a completed request as a tracing event and Prometheus metrics.
///
/// The identity and the facts arrive as two borrowed values rather than as
/// loose fields, because each is one thing: a caller that supplied the name of
/// one request and the dimensions of another would be describing a request that
/// never happened.
///
/// The identity is what every exit already names the request by. The request
/// identifier and the raw path are the two values this record carries into an
/// event and deliberately never into a label, and reading them off one owner is
/// what keeps that split in one place.
///
/// The elapsed time is handed in rather than measured here: the clock belongs to
/// the request, and the owner that watched it end is the only one that can say
/// how long the whole head-to-terminal span was.
pub(super) fn record_request(
    telemetry: Telemetry,
    identity: &RequestIdentity,
    facts: &CompletionSnapshot,
    elapsed: std::time::Duration,
) {
    let status = status_to_label(facts.status);

    if telemetry.events() {
        tracing::info!(
            request_id = identity.request_id().as_str(),
            method = identity.method_label(),
            path = identity.path(),
            protocol = identity.protocol_label(),
            status,
            origin = facts.origin_label(),
            rejection = facts.rejection_label(),
            delivery = facts.delivery_label(),
            connection_end = facts.connection_end_label(),
            boundary = facts.boundary_label(),
            shutdown = facts.shutdown_label(),
            latency_ms = elapsed.as_millis(),
            "request completed"
        );
    }

    if telemetry.metrics() {
        // Built once and read by both instruments. Two spellings of one label
        // set are two things that can disagree, and a counter and a histogram
        // that disagree about how a request is labelled cannot be read together.
        let labels = [
            metrics::Label::from_static_parts("method", identity.method_label()),
            metrics::Label::from_static_parts("status", status),
            metrics::Label::from_static_parts("protocol", identity.protocol_label()),
            metrics::Label::from_static_parts("origin", facts.origin_label()),
            metrics::Label::from_static_parts("rejection", facts.rejection_label()),
            metrics::Label::from_static_parts("delivery", facts.delivery_label()),
            metrics::Label::from_static_parts("connection_end", facts.connection_end_label()),
            metrics::Label::from_static_parts("boundary", facts.boundary_label()),
            metrics::Label::from_static_parts("shutdown", facts.shutdown_label()),
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
            "status" => status_to_label(Some(status)),
        )
        .increment(1);
    }
}

/// Return a static string label for an HTTP status code.
///
/// Absence is a name of its own: an operation that ended before any head
/// committed gave its peer no status, and a counter whose status label is
/// sometimes missing splits one time series into two.
///
/// Common codes get `&'static str` with zero allocation. Rare codes
/// are cached in a fixed-size table initialized on first use — no memory leak.
fn status_to_label(status: Option<u16>) -> &'static str {
    match status {
        None => super::completion::ABSENT_LABEL,
        Some(200) => "200",
        Some(201) => "201",
        Some(204) => "204",
        Some(301) => "301",
        Some(302) => "302",
        Some(304) => "304",
        Some(400) => "400",
        Some(401) => "401",
        Some(403) => "403",
        Some(404) => "404",
        Some(405) => "405",
        Some(408) => "408",
        Some(413) => "413",
        Some(500) => "500",
        Some(502) => "502",
        Some(503) => "503",
        Some(code @ 100..600) => {
            static TABLE: std::sync::OnceLock<Box<[Box<str>]>> = std::sync::OnceLock::new();
            let table = TABLE.get_or_init(build_status_text_table);
            &table[(code - 100) as usize]
        }
        Some(_) => "unknown",
    }
}
