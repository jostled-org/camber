//! Integration terminal event and metric assertions.

use crate::common::{
    Sample, TraceCapture, capture_events, field_occurrences, field_value, field_value_starts,
    opens_a_field, parse_sample, total_where,
};
use crate::integration_rows::{Refusal, Row, all, expect, expect_aggregate, expect_eq, on_tokio};
use crate::integration_vocabulary::{FAILURES, KINDS, OPERATIONS, RETRYABILITIES};
use camber::{
    IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability, RuntimeError,
};
use metrics_exporter_prometheus::PrometheusHandle;
use std::future::Future;
use std::sync::OnceLock;
use std::time::Duration;

pub const CHILD_BOUND: Duration = Duration::from_secs(240);

/// The hang guard every bounded wait runs under; never a timing assertion.
pub const ROW_BOUND: Duration = Duration::from_secs(30);

/// The fixed sentence every terminal event carries as its whole message.
const MESSAGE: &str = "integration operation finished";

/// The outcome label a successful terminal carries.
pub const SUCCESS: &str = "success";

/// The counter every terminal increments once.
pub const OPERATIONS_TOTAL: &str = "camber_integration_operations_total";

/// The admission-to-settlement duration each admitted operation records once.
const DURATION: &str = "camber_integration_operation_duration_seconds";

/// Every metric name an integration family may render under.
const FAMILY_PREFIX: &str = "camber_integration_";

/// The only labels a terminal sample may carry.
const LABELS: [&str; 3] = ["kind", "operation", "outcome"];
static RECORDER: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the child's one Prometheus recorder.
pub fn install_recorder() {
    let recorder = metrics_exporter_prometheus::PrometheusBuilder::new().build_recorder();
    let handle = recorder.handle();
    assert!(
        metrics::set_global_recorder(recorder).is_ok(),
        "the isolated child already had a metrics recorder"
    );
    assert!(
        RECORDER.set(handle).is_ok(),
        "the recorder handle was already stored"
    );
}
// ── the closed vocabulary ─────────────────────────────────────────────

/// Whether `value` is the label one of `closed` renders as.
fn is_label_of<T: std::fmt::Display>(closed: &[T], value: &str) -> bool {
    closed.iter().any(|label| label.to_string() == value)
}

pub fn is_kind(value: &str) -> bool {
    is_label_of(&KINDS, value)
}

pub fn is_operation(value: &str) -> bool {
    is_label_of(&OPERATIONS, value)
}

pub fn is_failure(value: &str) -> bool {
    is_label_of(&FAILURES, value)
}

pub fn is_outcome(value: &str) -> bool {
    value == SUCCESS || is_failure(value)
}

pub fn is_retryability(value: &str) -> bool {
    is_label_of(&RETRYABILITIES, value)
}

// ── expected terminals ────────────────────────────────────────────────

/// The terminals one row's operations must settle into.
#[derive(Clone, Debug)]
pub struct Terminal {
    kind: Box<str>,
    operation: Box<str>,
    outcome: Box<str>,
    /// Further outcomes an unordered settlement may commit instead.
    alternatives: Vec<Box<str>>,
    /// The retryability a failure must carry; `None` leaves it to the closed
    /// set alone.
    retryability: Option<Box<str>>,
    shutdown: bool,
    count: usize,
    admitted: bool,
}

impl Terminal {
    pub fn before_admission(mut self) -> Self {
        self.admitted = false;
        self
    }

    pub fn times(mut self, count: usize) -> Self {
        self.count = count;
        self
    }

    pub fn under_shutdown(mut self) -> Self {
        self.shutdown = true;
        self
    }

    /// Also permit `failure`: the settlement is unordered, so either
    /// committed outcome is valid.
    pub fn or(mut self, failure: IntegrationFailure) -> Self {
        self.alternatives.push(failure.to_string().into_boxed_str());
        self
    }

    pub fn pair(&self) -> (&str, &str) {
        (&self.kind, &self.operation)
    }

    pub fn key(&self) -> (&str, &str, &str) {
        (&self.kind, &self.operation, &self.outcome)
    }

    /// Every outcome this terminal permits, the named one first.
    pub fn outcomes(&self) -> impl Iterator<Item = &str> {
        std::iter::once(&*self.outcome).chain(self.alternatives.iter().map(Box::as_ref))
    }

    /// Whether `operation` settling as `outcome` is one this terminal permits.
    pub fn permits(&self, operation: &str, outcome: &str) -> bool {
        &*self.operation == operation && self.outcomes().any(|permitted| permitted == outcome)
    }

    /// The operations of this terminal that record a duration sample.
    pub fn admitted_count(&self) -> usize {
        match self.admitted {
            true => self.count,
            false => 0,
        }
    }
}

/// One successful terminal of `operation`.
pub fn success(kind: IntegrationKind, operation: IntegrationOperation) -> Terminal {
    terminal(kind, operation, SUCCESS.into())
}

/// One failed terminal of `operation` with `failure` and `retryability`.
pub fn failed(
    kind: IntegrationKind,
    operation: IntegrationOperation,
    failure: IntegrationFailure,
    retryability: Retryability,
) -> Terminal {
    Terminal {
        retryability: Some(retryability.to_string().into_boxed_str()),
        ..failed_unchecked(kind, operation, failure)
    }
}

/// One failed terminal whose retryability only the closed set constrains.
pub fn failed_unchecked(
    kind: IntegrationKind,
    operation: IntegrationOperation,
    failure: IntegrationFailure,
) -> Terminal {
    terminal(kind, operation, failure.to_string().into_boxed_str())
}

/// One admitted terminal of `operation` settling as `outcome`, outside shutdown.
fn terminal(kind: IntegrationKind, operation: IntegrationOperation, outcome: Box<str>) -> Terminal {
    Terminal {
        kind: kind.to_string().into_boxed_str(),
        operation: operation.to_string().into_boxed_str(),
        outcome,
        alternatives: Vec::new(),
        retryability: None,
        shutdown: false,
        count: 1,
        admitted: true,
    }
}

/// A runtime refusal before admission is `Closed/Never`, with no duration.
pub fn refused(kind: IntegrationKind, operation: IntegrationOperation) -> Terminal {
    failed(
        kind,
        operation,
        IntegrationFailure::Closed,
        Retryability::Never,
    )
    .before_admission()
}

// ── one row's observation ─────────────────────────────────────────────

/// One row's capture and its scrape before the row ran.
pub struct Observation {
    capture: TraceCapture,
    before: Result<Box<[Sample]>, String>,
}

impl Observation {
    pub fn start() -> Self {
        Self {
            capture: capture_events(MESSAGE),
            before: scrape(),
        }
    }

    pub fn finish(self) -> Observed {
        let events = match self.capture.truncated() {
            true => Err("the terminal transcript exceeded its capture budget".to_owned()),
            false => Ok(self.capture.events()),
        };
        Observed {
            events,
            before: self.before,
            after: scrape(),
        }
    }
}

/// What one row's operations recorded.
pub struct Observed {
    events: Result<Box<[Box<str>]>, String>,
    before: Result<Box<[Sample]>, String>,
    after: Result<Box<[Sample]>, String>,
}

impl Observed {
    pub fn events(&self) -> Result<&[Box<str>], String> {
        self.events.as_deref().map_err(Clone::clone)
    }

    pub fn scrapes(&self) -> Result<(&[Sample], &[Sample]), String> {
        match (&self.before, &self.after) {
            (Ok(before), Ok(after)) => Ok((before, after)),
            (Err(error), _) | (_, Err(error)) => Err(error.clone()),
        }
    }

    /// The captured terminals of `kind`.
    pub fn of_kind(&self, kind: &str) -> Result<Vec<&str>, String> {
        Ok(self
            .events()?
            .iter()
            .map(Box::as_ref)
            .filter(|event| field_value(event, "kind") == Some(kind))
            .collect())
    }

    /// The captured terminals of `kind` and `operation`.
    pub fn of(&self, kind: &str, operation: &str) -> Result<Vec<&str>, String> {
        Ok(self
            .of_kind(kind)?
            .into_iter()
            .filter(|event| field_value(event, "operation") == Some(operation))
            .collect())
    }

    /// The captured terminals of `kind`, `operation`, and `outcome`.
    pub fn with_outcome(&self, key: (&str, &str, &str)) -> Result<Vec<&str>, String> {
        let (kind, operation, outcome) = key;
        Ok(self
            .of(kind, operation)?
            .into_iter()
            .filter(|event| field_value(event, "outcome") == Some(outcome))
            .collect())
    }

    /// Every captured event and every rendered sample stays inside the
    /// closed vocabulary, and the row's operations settled into exactly
    /// `expected`: for each operation pair named, no terminal beyond those
    /// listed, one counter per terminal, and one duration per admitted operation.
    /// For each kind named, the complete multiset matches: no event, counter,
    /// or duration sample of that kind beyond the listed terminals.
    pub fn expect_terminals(&self, expected: &[Terminal]) -> Row {
        let mut checks = vec![self.expect_well_formed(), self.expect_closed_samples()];
        let mut kinds: Vec<&str> = expected.iter().map(|terminal| &*terminal.kind).collect();
        kinds.sort_unstable();
        kinds.dedup();
        checks.extend(kinds.into_iter().map(|kind| {
            let listed: Vec<&Terminal> = expected
                .iter()
                .filter(|terminal| &*terminal.kind == kind)
                .collect();
            self.expect_kind_multiset(kind, &listed)
        }));
        let mut pairs: Vec<(&str, &str)> = expected.iter().map(Terminal::pair).collect();
        pairs.sort_unstable();
        pairs.dedup();
        for (kind, operation) in pairs {
            let listed: usize = expected
                .iter()
                .filter(|terminal| terminal.pair() == (kind, operation))
                .map(|terminal| terminal.count)
                .sum();
            checks.push(self.of(kind, operation).and_then(|events| {
                expect_eq(
                    &format!("{kind} {operation} terminal events"),
                    events.len(),
                    listed,
                )
            }));
        }
        checks.extend(
            expected
                .iter()
                .map(|terminal| self.expect_terminal(terminal)),
        );
        all(checks)
    }

    /// The complete terminal multiset of `kind` is `listed`: every event of
    /// the kind is permitted by one listed terminal, and the kind's events,
    /// counter increments, and duration samples total exactly what is listed.
    pub fn expect_kind_multiset(&self, kind: &str, listed: &[&Terminal]) -> Row {
        let events = self.of_kind(kind)?;
        let unexpected: Vec<&str> = events
            .iter()
            .copied()
            .filter(|event| {
                let operation = field_value(event, "operation").unwrap_or_default();
                !listed
                    .iter()
                    .any(|terminal| terminal.permits(operation, outcome_of(event)))
            })
            .collect();
        let terminals: usize = listed.iter().map(|terminal| terminal.count).sum();
        let admitted: usize = listed
            .iter()
            .map(|terminal| terminal.admitted_count())
            .sum();
        let (before, after) = self.scrapes()?;
        let of_kind = |name: &str| {
            delta_where(before, after, |sample| {
                sample.name() == name && sample.label("kind") == Some(kind)
            })
        };
        all([
            expect_eq(
                &format!("{kind} terminals no listed operation permits"),
                unexpected,
                Vec::<&str>::new(),
            ),
            expect_eq(
                &format!("{kind} terminal events in all"),
                events.len(),
                terminals,
            ),
            expect_eq(
                &format!("{kind} counter delta in all"),
                of_kind(OPERATIONS_TOTAL),
                count_value(terminals),
            ),
            expect_eq(
                &format!("{kind} duration sample delta in all"),
                of_kind(&format!("{DURATION}_count")),
                count_value(admitted),
            ),
        ])
    }

    /// One expected terminal: its events, their fields, and its samples.
    pub fn expect_terminal(&self, terminal: &Terminal) -> Row {
        let outcomes: Vec<&str> = terminal.outcomes().collect();
        let what = format!(
            "{} {} {}",
            terminal.kind,
            terminal.operation,
            outcomes.join("|")
        );
        let events: Vec<&str> = self
            .of(&terminal.kind, &terminal.operation)?
            .into_iter()
            .filter(|event| terminal.permits(&terminal.operation, outcome_of(event)))
            .collect();
        let mut checks = vec![expect_eq(
            &format!("{what} terminal events"),
            events.len(),
            terminal.count,
        )];
        checks.extend(
            events
                .iter()
                .map(|event| expect_terminal_fields(&what, terminal, event)),
        );
        checks.push(self.scrapes().and_then(|(before, after)| {
            let moved = |name: &str| -> f64 {
                outcomes
                    .iter()
                    .map(|outcome| {
                        delta(
                            before,
                            after,
                            name,
                            (&terminal.kind, &terminal.operation, *outcome),
                        )
                    })
                    .sum()
            };
            all([
                expect_eq(
                    &format!("{what} counter delta"),
                    moved(OPERATIONS_TOTAL),
                    count_value(terminal.count),
                ),
                expect_eq(
                    &format!("{what} duration sample delta"),
                    moved(&format!("{DURATION}_count")),
                    count_value(terminal.admitted_count()),
                ),
            ])
        }));
        all(checks)
    }

    /// No terminal of `kind` and `operation` was recorded or counted, under
    /// any outcome.
    pub fn expect_absent(&self, kind: IntegrationKind, operation: IntegrationOperation) -> Row {
        let (kind, operation) = (kind.to_string(), operation.to_string());
        let events = self.of(&kind, &operation)?;
        let (before, after) = self.scrapes()?;
        let counted = delta_where(before, after, |sample| {
            sample.name() == OPERATIONS_TOTAL
                && sample.label("kind") == Some(kind.as_str())
                && sample.label("operation") == Some(operation.as_str())
        });
        all([
            expect_eq(
                &format!("{kind} {operation} events of an operation never reached"),
                events.len(),
                0,
            ),
            expect_eq(
                &format!("{kind} {operation} counter delta of an operation never reached"),
                counted,
                0.0,
            ),
        ])
    }

    /// Every event of `kind` names one admitted instance, the same one.
    pub fn expect_one_instance(&self, kind: IntegrationKind) -> Row {
        let kind = kind.to_string();
        let events = self.of_kind(&kind)?;
        let mut ids: Vec<Option<&str>> = events
            .iter()
            .map(|event| field_value(event, "instance_id"))
            .collect();
        ids.sort_unstable();
        ids.dedup();
        all([
            expect(
                &format!("no {kind} terminal was recorded"),
                !events.is_empty(),
            ),
            expect(
                &format!("a {kind} terminal names no instance"),
                ids.iter().all(Option::is_some),
            ),
            expect_eq(&format!("{kind} instances named"), ids.len(), 1),
        ])
    }

    /// No event of `kind` names an instance: admission refused it first.
    pub fn expect_no_instance(&self, kind: IntegrationKind) -> Row {
        let kind = kind.to_string();
        let named = self
            .of_kind(&kind)?
            .into_iter()
            .filter(|event| field_occurrences(event, "instance_id") > 0)
            .count();
        expect_eq(
            &format!("{kind} terminals naming an instance before admission"),
            named,
            0,
        )
    }

    /// No captured event repeats any of `secrets`.
    pub fn expect_redacted(&self, secrets: &[&str]) -> Row {
        let leaked: Vec<&str> = secrets
            .iter()
            .copied()
            .filter(|secret| {
                self.events
                    .iter()
                    .flatten()
                    .any(|event| event.contains(secret))
            })
            .collect();
        all([
            self.events().map(drop),
            expect_eq("secrets an event repeated", leaked, Vec::<&str>::new()),
        ])
    }

    /// Every event is one terminal: a fixed message and exactly one closed
    /// value per field.
    pub fn expect_well_formed(&self) -> Row {
        let malformed: Vec<&str> = self
            .events()?
            .iter()
            .map(Box::as_ref)
            .filter(|event| !well_formed(event))
            .collect();
        expect_eq("malformed terminal events", malformed, Vec::<&str>::new())
    }

    /// Every rendered integration sample carries only closed names and labels.
    pub fn expect_closed_samples(&self) -> Row {
        let (_, after) = self.scrapes()?;
        let open: Vec<String> = after
            .iter()
            .filter(|sample| !closed_sample(sample))
            .map(|sample| format!("{sample:?}"))
            .collect();
        expect_eq("open integration samples", open, Vec::<String>::new())
    }

    /// The seconds the duration samples of `key` moved by.
    pub fn duration_sum(&self, key: (&str, &str, &str)) -> Result<f64, String> {
        let (before, after) = self.scrapes()?;
        Ok(delta(before, after, &format!("{DURATION}_sum"), key))
    }

    /// The one duration `key` recorded lies between `floor`, the deadline
    /// that settled it, and `waited`, the caller's causally enclosing wait.
    pub fn expect_duration_between(
        &self,
        key: (&str, &str, &str),
        floor: Duration,
        waited: Duration,
    ) -> Row {
        let (kind, operation, outcome) = key;
        let recorded = self.duration_sum(key)?;
        all([
            expect(
                &format!(
                    "the {kind} {operation} {outcome} terminal recorded {recorded}s, under its {floor:?} deadline"
                ),
                recorded >= floor.as_secs_f64(),
            ),
            expect(
                &format!(
                    "the {kind} {operation} {outcome} terminal recorded {recorded}s, past the caller's {waited:?} wait"
                ),
                recorded <= waited.as_secs_f64(),
            ),
        ])
    }
}

/// The outcome `event` settled with; empty when it names none.
pub fn outcome_of(event: &str) -> &str {
    field_value(event, "outcome").unwrap_or_default()
}

/// The fields one permitted event of `terminal` must carry.
pub fn expect_terminal_fields(what: &str, terminal: &Terminal, event: &str) -> Row {
    let shutdown = match terminal.shutdown {
        true => "true",
        false => "false",
    };
    let mut checks = vec![expect_eq(
        &format!("{what} shutdown field"),
        field_value(event, "shutdown"),
        Some(shutdown),
    )];
    match outcome_of(event) {
        SUCCESS => {}
        outcome => checks.push(expect_eq(
            &format!("{what} failure field"),
            field_value(event, "failure"),
            Some(outcome),
        )),
    }
    if let Some(retryability) = &terminal.retryability {
        checks.push(expect_eq(
            &format!("{what} retryability field"),
            field_value(event, "retryability"),
            Some(&**retryability),
        ));
    }
    all(checks)
}

/// Whether `event` is one terminal: a fixed message, exactly one of each
/// closed field, closed values in each, and INFO metadata from the subscriber.
pub fn well_formed(event: &str) -> bool {
    let fixed = fixed_message(event);
    let single = ["kind", "operation", "outcome", "shutdown"]
        .iter()
        .all(|field| field_occurrences(event, field) == 1);
    let closed = field_value(event, "kind").is_some_and(is_kind)
        && field_value(event, "operation").is_some_and(is_operation)
        && field_value(event, "outcome").is_some_and(is_outcome)
        && matches!(field_value(event, "shutdown"), Some("true" | "false"))
        && field_value(event, "failure").is_none_or(is_failure)
        && field_value(event, "retryability").is_none_or(is_retryability)
        && field_occurrences(event, "failure") <= 1
        && field_occurrences(event, "retryability") <= 1;
    let failure_matches = match field_value(event, "outcome") {
        Some(SUCCESS) => field_occurrences(event, "failure") == 0,
        outcome => field_value(event, "failure") == outcome,
    };
    let info = field_value(event, "metadata_level") == Some("INFO");
    fixed && single && closed && failure_matches && info
}

/// Whether the terminal message is the whole fixed sentence.
pub fn fixed_message(event: &str) -> bool {
    field_value_starts(event, "message").any(|at| {
        event[at..]
            .strip_prefix(MESSAGE)
            .is_some_and(|tail| tail.is_empty() || opens_a_field(tail))
    })
}

// ── the scrape ────────────────────────────────────────────────────────

/// Whether `sample` carries the terminal labels `key`: kind, operation, and
/// outcome.
fn matches(sample: &Sample, key: (&str, &str, &str)) -> bool {
    let (kind, operation, outcome) = key;
    sample.label("kind") == Some(kind)
        && sample.label("operation") == Some(operation)
        && sample.label("outcome") == Some(outcome)
}

/// Every integration sample the recorder renders now.
pub fn scrape() -> Result<Box<[Sample]>, String> {
    let handle = RECORDER
        .get()
        .ok_or("the child installed no metrics recorder")?;
    handle
        .render()
        .lines()
        .filter(|line| line.starts_with(FAMILY_PREFIX))
        .map(|line| parse_sample(line).ok_or_else(|| format!("an unreadable sample: {line:?}")))
        .collect()
}

/// Whether one rendered sample uses only the two families' names and their
/// closed labels.
pub fn closed_sample(sample: &Sample) -> bool {
    let name = sample.name();
    let extra = match (name, name.strip_prefix(DURATION)) {
        (OPERATIONS_TOTAL, _) | (_, Some("_sum" | "_count")) => None,
        (_, Some("")) => Some("quantile"),
        (_, Some("_bucket")) => Some("le"),
        _ => return false,
    };
    let names_closed = sample
        .labels()
        .iter()
        .all(|(label, _)| LABELS.contains(&label.as_ref()) || Some(label.as_ref()) == extra);
    let names_complete = LABELS.iter().all(|label| {
        sample
            .labels()
            .iter()
            .filter(|(candidate, _)| candidate.as_ref() == *label)
            .count()
            == 1
    });
    names_closed
        && names_complete
        && sample.label("kind").is_some_and(is_kind)
        && sample.label("operation").is_some_and(is_operation)
        && sample.label("outcome").is_some_and(is_outcome)
}

/// How far the samples named `name` under `key` moved between two scrapes.
pub fn delta(before: &[Sample], after: &[Sample], name: &str, key: (&str, &str, &str)) -> f64 {
    delta_where(before, after, |sample| {
        sample.name() == name && matches(sample, key)
    })
}

/// How far the samples `selected` picks moved between two scrapes.
pub fn delta_where(before: &[Sample], after: &[Sample], selected: impl Fn(&Sample) -> bool) -> f64 {
    total_where(after, &selected) - total_where(before, &selected)
}

/// A terminal count as the sample value it must move by.
pub fn count_value(count: usize) -> f64 {
    u32::try_from(count).map_or(f64::INFINITY, f64::from)
}

pub fn bounded<F: Future>(what: &str, future: F) -> Result<F::Output, String> {
    crate::integration_rows::bounded(what, ROW_BOUND, future)
}

/// Run `future` under the hang guard and require it to succeed.
pub fn settled<T>(
    what: &str,
    future: impl Future<Output = Result<T, RuntimeError>>,
) -> Result<T, String> {
    crate::integration_rows::settled_within(what, ROW_BOUND, future)
}
/// Drive `future` on a bare Tokio runtime with no Camber owner, under the
/// hang guard.
pub fn outside_camber<F: Future>(future: F) -> Result<F::Output, String> {
    on_tokio(async { tokio::time::timeout(ROW_BOUND, future).await })?
        .map_err(|_| format!("the call outside Camber did not finish within {ROW_BOUND:?}"))
}

/// Fail the row unless the runtime aggregate holds exactly `expected`
/// failures of `kind`, in order.
pub fn expect_aggregate_failures(
    teardown: Result<(), RuntimeError>,
    kind: IntegrationKind,
    expected: &[Refusal],
) -> Row {
    match teardown {
        Ok(()) => Err("the runtime left no aggregate".to_owned()),
        Err(error) => expect_aggregate(&error, kind, expected),
    }
}
