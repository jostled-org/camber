//! Row helpers the NATS, SQS, and DNS terminal-event rows share.

use crate::common::run_in_child;
use crate::integration_events::{
    CHILD_BOUND, Observed, Terminal, bounded, failed, install_recorder,
};
pub use crate::integration_rows::EXHAUSTED;
use crate::integration_rows::{NamedRow, Refusal, Row, all, clean_run, run_rows_under};
use camber::{IntegrationFailure, IntegrationKind, IntegrationOperation, RuntimeError};
use std::future::Future;
use std::time::{Duration, Instant};

/// A payload or body maximum small enough to cross with a literal.
pub const SMALL_MAX: usize = 4;

/// Run `rows` in the private child `mode` under its own metrics recorder,
/// failing once under `diagnostic`; the parent waits for `marker`.
///
/// Every kind's terminal matrix owns the process-global recorder, so each
/// runs alone in a child that installs it first.
pub fn run_terminal_rows(
    test_name: &str,
    mode: &str,
    marker: &str,
    diagnostic: &str,
    rows: &[NamedRow<'_>],
) {
    run_in_child(test_name, mode, marker, CHILD_BOUND, || {
        install_recorder();
        run_rows_under(diagnostic, rows);
    });
}

/// Run `future` under the hang guard, with the caller's wait around it.
pub fn timed<F: Future>(what: &str, future: F) -> Result<(F::Output, Duration), String> {
    let started = Instant::now();
    let answer = bounded(what, future)?;
    Ok((answer, started.elapsed()))
}

/// The one `operation` timeout of `kind` recorded a duration between
/// [`EXHAUSTED`] and the caller's causally enclosing wait.
pub fn expect_timeout_duration(
    observed: &Observed,
    kind: IntegrationKind,
    operation: IntegrationOperation,
    waited: Duration,
) -> Row {
    let (kind, operation) = (kind.to_string(), operation.to_string());
    let timeout = IntegrationFailure::Timeout.to_string();
    observed.expect_duration_between((&kind, &operation, &timeout), EXHAUSTED, waited)
}

/// No terminal of `kind` under any of `operations` was recorded or counted.
pub fn expect_unreached(
    observed: &Observed,
    kind: IntegrationKind,
    operations: &[IntegrationOperation],
) -> Row {
    all(operations
        .iter()
        .map(|operation| observed.expect_absent(kind, *operation)))
}

/// The one failed terminal of `kind` that `refusal` names.
pub fn failed_as(kind: IntegrationKind, refusal: Refusal) -> Terminal {
    let (operation, failure, retryability) = refusal;
    failed(kind, operation, failure, retryability)
}

/// The runtime tore down clean.
pub fn expect_clean_teardown(teardown: Result<(), RuntimeError>) -> Row {
    clean_run(teardown.map(Ok))
}
