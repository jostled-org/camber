//! What one admitted operation is finally recorded as.
//!
//! The completion counter, its duration family, and the completion event are
//! read here once. Every root row that claims "recorded once, under these
//! labels" reads them through this module, so two rows cannot disagree about
//! what a label set means.

/// The counter one completed operation is recorded under.
///
/// Owned by the rejection metrics, which roots without this module mount too.
pub use super::rejection_metrics::COMPLETION_METRIC;

/// The duration family's own count series, read beside the counter.
///
/// Both instruments carry one label set, so a row that moved one and not the
/// other would be describing two different requests.
const DURATION_COUNT_METRIC: &str = "http_request_duration_seconds_count";

/// The sentence one completed operation is recorded under.
///
/// Owned by the rejection support, which roots without this module mount too.
pub use super::rejection_support::COMPLETION_MESSAGE as COMPLETION_EVENT;

/// What one row must have been recorded as, once.
///
/// Seven dimensions and no eighth that ranks them. A row states each one it
/// expects, including the ones it expects to be absent, because "absent" is what
/// this record replaced a fold with: an application response interrupted by a
/// departing peer and a peer that left before any answer existed differ in
/// exactly the fields a strongest terminal used to collapse.
pub struct Expected<'a> {
    pub label: &'a str,
    pub method: &'a str,
    /// The status the peer was given, or `None` when no head committed.
    pub status: Option<u16>,
    pub protocol: &'a str,
    pub origin: &'a str,
    pub rejection: &'a str,
    pub delivery: &'a str,
    pub connection_end: &'a str,
    pub boundary: &'a str,
    pub shutdown: &'a str,
}

/// The name every absent completion dimension is published under.
pub const ABSENT: &str = "none";

impl Expected<'_> {
    /// One row's dimensions, with everything this row does not name absent.
    ///
    /// A row states what it is about and inherits the absences, so adding a
    /// dimension is one edit here rather than one per row — and a row that meant
    /// to name a dimension and did not says `none` rather than saying nothing.
    pub const fn of(label: &'static str, method: &'static str, protocol: &'static str) -> Self {
        Self {
            label,
            method,
            status: Some(200),
            protocol,
            origin: ABSENT,
            rejection: ABSENT,
            delivery: "produced",
            connection_end: ABSENT,
            boundary: ABSENT,
            shutdown: ABSENT,
        }
    }

    /// The same row, produced by a named origin.
    pub const fn from(self, origin: &'static str) -> Self {
        Self { origin, ..self }
    }

    /// The same row, answered with `status`.
    pub const fn answering(self, status: u16) -> Self {
        Self {
            status: Some(status),
            ..self
        }
    }

    /// The whole label set this row's record carries.
    ///
    /// Built as one value because it is one claim: a delta taken over a subset
    /// of the labels would count a record another class produced.
    fn labels<'a>(&'a self, status: &'a str) -> [(&'a str, &'a str); 9] {
        [
            ("method", self.method),
            ("status", status),
            ("protocol", self.protocol),
            ("origin", self.origin),
            ("rejection", self.rejection),
            ("delivery", self.delivery),
            ("connection_end", self.connection_end),
            ("boundary", self.boundary),
            ("shutdown", self.shutdown),
        ]
    }

    /// The status label this row's record carries, as production spells it.
    fn status_label(&self) -> Box<str> {
        match self.status {
            Some(status) => status.to_string().into(),
            None => ABSENT.into(),
        }
    }
}

/// What one scrape reports about completed operations.
///
/// The counter and the duration family are read out of one body, so the two are
/// one moment rather than two: a second send would itself be a completed
/// operation between them.
pub struct Recorded {
    pub completions: Box<[super::metrics_scrape::Sample]>,
    pub durations: Box<[super::metrics_scrape::Sample]>,
}

impl Recorded {
    pub fn scraped(addr: std::net::SocketAddr) -> Self {
        let scrape = super::http::send(addr, "GET", "/metrics", &[], b"");
        Self {
            completions: super::metrics_scrape::scraped_samples(&scrape, COMPLETION_METRIC),
            durations: super::metrics_scrape::scraped_samples(&scrape, DURATION_COUNT_METRIC),
        }
    }
}

/// How far both instruments moved for one row's whole label set.
///
/// Stated as one number because the two have to agree: a counter that moved
/// while the duration family did not is a completion recorded under a label set
/// no operator can read a latency for.
pub fn moved(before: &Recorded, after: &Recorded, expected: &Expected<'_>) -> u64 {
    let status = expected.status_label();
    let labels = expected.labels(&status);
    let counted = super::metrics_scrape::delta(&before.completions, &after.completions, &labels);
    let timed = super::metrics_scrape::delta(&before.durations, &after.durations, &labels);
    assert_eq!(
        counted, timed,
        "{}: the counter and the duration family disagree about {labels:?}",
        expected.label,
    );
    counted
}

/// Assert one row was recorded exactly once, under everything it declared.
pub fn assert_recorded_once(before: &Recorded, after: &Recorded, expected: &Expected<'_>) {
    assert_eq!(
        moved(before, after, expected),
        1,
        "{}: exactly one record was expected under {:?}, and the scrape holds {:?}",
        expected.label,
        expected.labels(&expected.status_label()),
        after.completions,
    );
}

/// Assert nothing was recorded for this row yet.
pub fn assert_not_recorded_yet(before: &Recorded, held: &Recorded, expected: &Expected<'_>) {
    assert_eq!(
        moved(before, held, expected),
        0,
        "{}: a record was written before this operation reached its terminal",
        expected.label,
    );
}

/// The one completion event this row's path produced.
pub fn only_completion(capture: &super::trace_capture::TraceCapture, label: &str) -> Box<str> {
    let events = capture.events();
    super::trace_capture::only_event(&events, COMPLETION_EVENT, label).into()
}

/// Assert one completion event states every dimension this row expects.
///
/// All seven, not a chosen few: the whole claim is that the dimensions are
/// orthogonal, and a check that read only the ones a row varies could not tell a
/// record that left the rest absent from one that folded them away.
///
/// Read off the same label set the scrape is matched against, less the method,
/// and field by field: `status=20` is a substring of `status=200`, so a
/// contains-check could pass a dimension the event never recorded.
pub fn assert_event_matches(event: &str, expected: &Expected<'_>) {
    let status = expected.status_label();
    for (name, value) in expected
        .labels(&status)
        .iter()
        .filter(|(name, _)| *name != "method")
    {
        super::trace_capture::assert_field_value(event, name, value, expected.label);
    }
}

/// How long one recorded operation reports having taken.
pub fn recorded_latency(event: &str, label: &str) -> u128 {
    super::trace_capture::field_value(event, "latency_ms")
        .unwrap_or_else(|| panic!("{label}: the completion event reports no latency"))
        .parse()
        .unwrap_or_else(|error| panic!("{label}: the recorded latency is unreadable: {error}"))
}
