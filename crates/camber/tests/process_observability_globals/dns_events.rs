//! DNS order, record, cache, renewal, and close terminal observations.
//!
//! Every row enters through public `camber::dns01` provisioning, renewal, or
//! `RuntimeBuilder` startup. The controlled ACME and Cloudflare-shaped peers
//! record each request before they answer, and the renewal clock elapses only
//! when a row lets it. Each row derives its expected terminal multiset from the
//! logical operations it invoked and the stages the peers or the owner
//! acknowledged, before it reads any telemetry. A terminal is counted per
//! managed operation: one `ZoneLookup` per preparation, however many zone
//! queries it sends, and one `CacheRead` per logical generation read, however
//! many files it opens.

use crate::common::{field_value, poll_until};
use crate::dns_cleanup_peers::{
    BUNDLE, CfLog, DeleteAnswer, Fixture, Named, Script, Scripts, Stage, TWO_ZONES,
    await_renewal_waits, dns_accounts, elapse_renewal, expect_exact_deletes, expect_retained,
    expected_unresolved, generation_expiring, hold_budget, integration, leaf_of, named,
    prior_generation, release_budget, retire_renewal, run_observing, served_leaf, served_store,
};
use crate::event_rows::{
    EXHAUSTED, expect_clean_teardown, expect_timeout_duration, expect_unreached, failed_as,
    run_terminal_rows, timed,
};
use crate::integration_events::{
    OPERATIONS_TOTAL, Observation, Observed, ROW_BOUND, SUCCESS, Terminal, bounded, delta, failed,
    failed_unchecked, outside_camber, refused, success,
};
use crate::integration_rows::{
    LIVE_LIMIT, NamedRow, Row, all, await_signal, busy, clean_run, expect, expect_eq,
    expect_no_runtime, expect_ok, expect_refused, expect_scope_closed, expired, hold_live_slots,
    integration_admitted_after, invalid_config, observed_verdict, permission_denied, rejected,
    unavailable,
};
use camber::dns01::{AcmeDns01, CloudflareProvider, DnsProvider, RecordId};
use camber::runtime_test_support::{
    IntegrationLifecycleProbe, IntegrationProbeHandle, RuntimeCheckpoint, RuntimeController,
    runtime_schedule,
};
use camber::{
    IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability, RuntimeBuilder,
    RuntimeError, runtime,
};
use rustls::sign::CertifiedKey;
use serde_json::json;
use std::future::Future;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A Cloudflare API token no event may repeat.
const DNS_TOKEN: &str = "cloudflare-token-3b8e";

/// The kind every row's terminals carry.
const DNS: IntegrationKind = IntegrationKind::Dns01;

/// One domain: each order raises one record.
const ONE: [&str; 1] = ["app.example.com"];

/// The zone authority each of [`TWO_ZONES`] resolves to.
const AUTHORITIES: [&str; 2] = ["example.com", "example.org"];

/// Days left on a cached leaf that is due for renewal.
const DUE_DAYS: u32 = 29;

/// Days left on a cached leaf that is not due for renewal.
const FRESH_DAYS: u32 = 60;

/// The runtime's aggregate shutdown deadline in the forced-stop row.
const FORCED_SHUTDOWN: Duration = Duration::from_secs(1);

/// A cleanup bound far past [`FORCED_SHUTDOWN`]: only the aggregate expiry
/// can end the forced row's cleanup.
const LONG_CLEANUP: Duration = Duration::from_secs(60);

#[test]
fn dns_terminals_match_nested_operations_and_cleanup() {
    run_terminal_rows(
        "dns_events::dns_terminals_match_nested_operations_and_cleanup",
        "dns-terminal-events",
        "DNS_TERMINAL_EVENTS_COMPLETE",
        "M9 DNS terminal event or metric contract is missing",
        TERMINAL_ROWS,
    );
}

const TERMINAL_ROWS: &[NamedRow<'static>] = &[
    (
        "dns provision settles every nested operation once",
        dns_provision_settles_every_nested_operation_once,
    ),
    (
        "dns nested operations share the order's report account",
        dns_nested_operations_share_the_order_account,
    ),
    (
        "dns cleanup failure names its record outside labels",
        dns_cleanup_failure_names_its_record_outside_labels,
    ),
    (
        "dns renewal settles one order",
        dns_renewal_settles_one_order,
    ),
    (
        "dns renewal cancelled during its order is one cancelled terminal",
        dns_renewal_cancelled_during_its_order_is_one_cancelled_terminal,
    ),
    (
        "dns runtime renewal settles one order",
        dns_runtime_renewal_settles_one_order,
    ),
    (
        "dns renewal refused by the report budget has no duration",
        dns_renewal_refused_by_the_report_budget_has_no_duration,
    ),
    (
        "dns failed publication keeps the prior generation",
        dns_failed_publication_keeps_the_prior_generation,
    ),
    (
        "dns unreadable cache is one failed read per check",
        dns_unreadable_cache_is_one_failed_read_per_check,
    ),
    (
        "dns denied preparation stops before any record",
        dns_denied_preparation_stops_before_any_record,
    ),
    (
        "dns preparation timeout is bounded once",
        dns_preparation_timeout_is_bounded_once,
    ),
    (
        "dns waiter drop cancels preparation once",
        dns_waiter_drop_cancels_preparation_once,
    ),
    (
        "dns rejected create settles once",
        dns_rejected_create_settles_once,
    ),
    (
        "dns lost create answer is one unknown outcome",
        dns_lost_create_answer_is_one_unknown_outcome,
    ),
    (
        "dns forced stop cancels the outstanding delete once",
        dns_forced_stop_cancels_the_outstanding_delete_once,
    ),
    (
        "dns unwound order names its unresolved record",
        dns_unwound_order_names_its_unresolved_record,
    ),
    (
        "dns order failure cleans up every record",
        dns_order_failure_cleans_up_every_record,
    ),
    (
        "dns missing runtime refusal is one terminal",
        dns_missing_runtime_refusal_is_one_terminal,
    ),
    (
        "dns closed scope refusal is one terminal",
        dns_closed_scope_refusal_is_one_terminal,
    ),
    (
        "dns live limit refusal reaches no later operation",
        dns_live_limit_refusal_reaches_no_later_operation,
    ),
    (
        "dns invalid configuration refusal has no duration",
        dns_invalid_configuration_refusal_has_no_duration,
    ),
    (
        "dns startup with no cache provisions once",
        dns_startup_with_no_cache_provisions_once,
    ),
    (
        "dns startup with a valid cache reads once",
        dns_startup_with_a_valid_cache_reads_once,
    ),
    (
        "dns startup with an invalid cache orders once",
        dns_startup_with_an_invalid_cache_orders_once,
    ),
    (
        "dns startup legacy import is one read and one write",
        dns_startup_legacy_import_is_one_read_and_one_write,
    ),
    (
        "dns close requested inside the stop reports under shutdown",
        dns_close_requested_inside_the_stop_reports_under_shutdown,
    ),
];

// ── shared fixture helpers ────────────────────────────────────────────

/// A real Cloudflare provider pointed at `base` under [`DNS_TOKEN`].
fn provider_at(base: &str) -> Result<CloudflareProvider, String> {
    CloudflareProvider::with_base_url(DNS_TOKEN.into(), base.into())
        .map_err(|error| format!("descriptor: {error:?}"))
}

/// A real Cloudflare provider pointed at the fixture's peer under
/// [`DNS_TOKEN`].
fn token_provider(fixture: &Fixture) -> Result<CloudflareProvider, String> {
    fixture.peers.cloudflare.provider(DNS_TOKEN)
}

/// The token and every challenge value the peer received: no event may
/// repeat any of them.
fn dns_secrets(log: &CfLog) -> Box<[&str]> {
    std::iter::once(DNS_TOKEN)
        .chain(log.creates.iter().map(|create| &*create.content))
        .collect()
}

/// No event repeats the token or a challenge value the peer received.
fn expect_dns_redacted(observed: &Observed, log: &CfLog) -> Row {
    observed.expect_redacted(&dns_secrets(log))
}

/// The terminals one order that prepared once and raised and deleted
/// `records` records settles into, beside its own `Provision` or `Renew`.
fn cleaned_records(records: usize) -> [Terminal; 3] {
    use IntegrationOperation::{CreateTxt, DeleteTxt, ZoneLookup};
    [
        success(DNS, ZoneLookup),
        success(DNS, CreateTxt).times(records),
        success(DNS, DeleteTxt).times(records),
    ]
}

/// Every terminal in `groups`, in order.
fn terminals<const N: usize>(groups: [&[Terminal]; N]) -> Box<[Terminal]> {
    groups.into_iter().flatten().cloned().collect()
}

/// The peer committed and acknowledged `records` creates and deleted each
/// of them by its exact ID, and nothing unrelated.
fn expect_cleaned(log: &CfLog, records: usize) -> Row {
    all([
        expect_eq("creates the peer received", log.creates.len(), records),
        expect_eq(
            "creates the peer acknowledged",
            log.acknowledged().len(),
            records,
        ),
        expect_eq(
            "records the peer deleted",
            log.deletes
                .iter()
                .filter(|delete| delete.answer == DeleteAnswer::Deleted)
                .count(),
            records,
        ),
        expect_eq("deletes the peer received", log.deletes.len(), records),
        expect_exact_deletes(log, &[]),
    ])
}

/// Every name the peer was asked to resolve as a zone, in arrival order.
fn zone_queries(log: &CfLog) -> Box<[&str]> {
    log.requests
        .iter()
        .filter_map(|request| request.strip_prefix("GET /zones?"))
        .filter_map(|query| query.split('&').find_map(|pair| pair.strip_prefix("name=")))
        .collect()
}

/// The one preparation queried every configured domain's authority.
fn expect_every_authority(log: &CfLog) -> Row {
    let queried = zone_queries(log);
    all(AUTHORITIES.iter().map(|authority| {
        expect(
            &format!("no zone query reached the authority {authority}: {queried:?}"),
            queried.contains(authority),
        )
    }))
}

/// The failure a caller's integration error carries.
fn caller_failure<T>(
    what: &str,
    answer: &Result<T, RuntimeError>,
) -> Result<IntegrationFailure, String> {
    match answer {
        Ok(_) => Err(format!(
            "{what}: succeeded, expected an integration failure"
        )),
        Err(RuntimeError::Integration(error)) => Ok(error.failure()),
        Err(other) => Err(format!(
            "{what}: {other:?}, expected an integration failure"
        )),
    }
}

/// Fail the row unless `answer` is an integration error with `failure`.
fn expect_refused_failure<T>(
    what: &str,
    answer: &Result<T, RuntimeError>,
    failure: IntegrationFailure,
) -> Row {
    caller_failure(what, answer)
        .and_then(|actual| expect_eq(&format!("{what}'s failure"), actual, failure))
}

/// The captured DNS terminals of `operation` that settled with `failure`.
fn dns_failures(
    observed: &Observed,
    operation: IntegrationOperation,
    failure: IntegrationFailure,
) -> Result<Vec<&str>, String> {
    observed.with_outcome((
        &DNS.to_string(),
        &operation.to_string(),
        &failure.to_string(),
    ))
}

/// The identity a cleanup account names a record by: its exact ID, or its
/// domain when no acknowledgement named one.
fn identities(named: &Named) -> impl Iterator<Item = &str> {
    named
        .iter()
        .map(|(domain, id)| id.as_deref().unwrap_or(domain))
}

/// The caller's typed cleanup account names exactly `expected`.
fn expect_caller_account<T>(answer: &Result<T, RuntimeError>, expected: &Named) -> Row {
    let error = match answer {
        Ok(_) => return Err("the order succeeded with unresolved records".to_owned()),
        Err(error) => error,
    };
    match integration(error) {
        Some(account) => all([
            expect_eq(
                "the caller's failure",
                account.failure(),
                IntegrationFailure::CleanupIncomplete,
            ),
            expect_eq("the caller's unresolved records", &named(account), expected),
        ]),
        None => Err(format!("the caller received {error:?}")),
    }
}

/// Every unresolved identity appears in the incomplete order's one
/// terminal, an event field and never a label.
fn expect_identities_in(
    observed: &Observed,
    operation: IntegrationOperation,
    expected: &Named,
) -> Row {
    let incomplete = dns_failures(observed, operation, IntegrationFailure::CleanupIncomplete)?;
    all([
        expect_eq(
            &format!("incomplete {operation} terminals"),
            incomplete.len(),
            1,
        ),
        all(identities(expected).map(|identity| {
            expect(
                &format!("the incomplete {operation} terminal does not name {identity}"),
                incomplete.iter().all(|event| event.contains(identity)),
            )
        })),
    ])
}

/// The runtime retained exactly one cleanup account, naming `expected`.
fn expect_one_retained(teardown: &Result<(), RuntimeError>, expected: &Named) -> Row {
    expect_retained(&dns_accounts(teardown)?, expected)
}

/// A runtime builder whose renewal clock `controller` drives.
fn scheduled(controller: &RuntimeController) -> RuntimeBuilder {
    runtime::builder().with_test_schedule(controller)
}

/// A runtime whose startup admits a DNS owner of [`TWO_ZONES`] against the
/// fixture's peers, on `controller`'s renewal clock.
fn startup(fixture: &Fixture, controller: &RuntimeController) -> Result<RuntimeBuilder, String> {
    Ok(scheduled(controller)
        .tls_auto_dns01(fixture.configuration(&TWO_ZONES)?, DNS_TOKEN.into())
        .with_test_dns_transport(&fixture.peers.cloudflare.uri()))
}

/// Elapse renewal interval `check`, then wait for the owner's next wait.
fn run_check(controller: &RuntimeController, check: usize) -> Row {
    elapse_renewal(controller, check, ROW_BOUND)?;
    await_renewal_waits(controller, check + 1, ROW_BOUND)
}

/// What one provision on a plain runtime answered and recorded.
type Provisioned = (
    Option<Result<Result<CertifiedKey, RuntimeError>, String>>,
    Result<(), RuntimeError>,
    Observed,
);

/// Provision `acme` through `provider` on a plain runtime under observation.
fn observe_provision<P: DnsProvider + 'static>(acme: &AcmeDns01, provider: P) -> Provisioned {
    let observation = Observation::start();
    let (caller, teardown) = run_observing(runtime::builder(), || {
        bounded("provision_cert", acme.provision_cert(provider))
    });
    (caller, teardown, observation.finish())
}

/// The startup owner served its first certificate and began its first
/// renewal wait; the closure then returns and the runtime stops it.
fn serve_until_first_wait(controller: &RuntimeController) -> Row {
    await_renewal_waits(controller, 1, ROW_BOUND)
}

/// What one runtime startup served until its first renewal wait, and
/// recorded.
type Served = (Option<Row>, Result<(), RuntimeError>, Observed);

/// Start a runtime that admits a DNS owner of [`TWO_ZONES`] against the
/// fixture's peers, serve until its first renewal wait, and stop it, under
/// observation.
fn observe_startup(fixture: &Fixture) -> Result<Served, String> {
    let controller = runtime_schedule();
    let builder = startup(fixture, &controller)?;
    let observation = Observation::start();
    let (served, teardown) = run_observing(builder, || serve_until_first_wait(&controller));
    Ok((served, teardown, observation.finish()))
}

/// The served leaf in `bundle`, or a named failure.
fn leaf(bundle: Option<Vec<u8>>, what: &str) -> Result<Vec<u8>, String> {
    bundle
        .and_then(|bundle| leaf_of(&bundle))
        .ok_or_else(|| format!("{what} holds no leaf"))
}

/// One two-domain provision settled every nested operation once, published
/// once, closed once, and repeated no secret.
fn expect_one_provision(observed: &Observed, log: &CfLog) -> Row {
    use IntegrationOperation::{CacheRead, CacheWrite, Close, Provision, Renew};
    let expected = terminals([
        &cleaned_records(TWO_ZONES.len()),
        &[
            success(DNS, Provision),
            success(DNS, CacheWrite),
            success(DNS, Close),
        ],
    ]);
    all([
        expect_cleaned(log, TWO_ZONES.len()),
        expect_every_authority(log),
        observed.expect_terminals(&expected),
        expect_unreached(observed, DNS, &[CacheRead, Renew]),
        observed.expect_one_instance(DNS),
        expect_dns_redacted(observed, log),
    ])
}

// ── preserved rows ────────────────────────────────────────────────────

/// One successful two-domain provision settles once, with exactly one zone
/// lookup that still queried both authorities, one terminal per TXT create and
/// delete, one cache publication, and one close. No event repeats the token or
/// a challenge value.
fn dns_provision_settles_every_nested_operation_once() -> Row {
    let fixture = Fixture::start(Scripts::default())?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = token_provider(&fixture)?;
    let (caller, teardown, observed) = observe_provision(&acme, provider);
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        observed_verdict(caller).and_then(|answer| expect_ok("provision", answer).map(drop)),
        expect_clean_teardown(teardown),
        expect_one_provision(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// Two free report accounts cover instance close and one two-domain order.
/// Its zone lookup, creates, deletes, and publication share the order's account.
/// No nested operation reserves another account or reports `Busy`. The answer
/// does not acknowledge the owner's settlement, so observation spans teardown.
fn dns_nested_operations_share_the_order_account() -> Row {
    let fixture = Fixture::start(Scripts::default())?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = token_provider(&fixture)?;
    let (caller, teardown) = run_observing(runtime::builder(), || -> Result<_, String> {
        let budget = hold_budget(2)?;
        let observation = Observation::start();
        let answer = bounded("provision_cert", acme.provision_cert(provider));
        release_budget(budget)?;
        Ok((answer?, observation))
    });
    let (answer, observation) = observed_verdict(caller)?;
    let observed = observation.finish();
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        expect_ok("provision with two free report accounts", answer).map(drop),
        expect_clean_teardown(teardown),
        expect_one_provision(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// A refused delete leaves one `CleanupIncomplete` provision terminal, one
/// failed delete terminal naming the record's exact ID as an event field, and
/// one incomplete close. The caller's account and the one retained account
/// name the same record; nothing is published, and no ID reaches a label.
fn dns_cleanup_failure_names_its_record_outside_labels() -> Row {
    use IntegrationFailure::{CleanupIncomplete, PermissionDenied};
    use IntegrationOperation::{CacheWrite, Close, CreateTxt, DeleteTxt, Provision, ZoneLookup};
    let fixture = Fixture::start(Scripts {
        delete: Script::answer().nth(2, Stage::Refuse),
        ..Scripts::default()
    })?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = token_provider(&fixture)?;
    let (caller, teardown, observed) = observe_provision(&acme, provider);
    let log = fixture.peers.cloudflare.log();
    let expected_records = expected_unresolved(&log, &[]);
    let refused = log.deletes.get(1).map(|delete| delete.id.clone());
    let named_in_events = refused.as_deref().map_or_else(
        || Err("the second delete never reached the peer".to_owned()),
        |id| expect_record_named(&observed, id),
    );
    let checks = all([
        observed_verdict(caller).and_then(|answer| {
            all([
                expect_caller_account(&answer, &expected_records),
                expect_refused_failure("the provision", &answer, CleanupIncomplete),
            ])
        }),
        expect_eq("records left unresolved", expected_records.len(), 1),
        expect_eq("creates the peer acknowledged", log.acknowledged().len(), 2),
        observed.expect_terminals(&[
            success(DNS, ZoneLookup),
            success(DNS, CreateTxt).times(2),
            success(DNS, DeleteTxt),
            failed_unchecked(DNS, DeleteTxt, PermissionDenied),
            failed_unchecked(DNS, Provision, CleanupIncomplete),
            failed_unchecked(DNS, Close, CleanupIncomplete),
        ]),
        observed.expect_absent(DNS, CacheWrite),
        observed.expect_one_instance(DNS),
        named_in_events,
        expect_identities_in(&observed, Provision, &expected_records),
        expect_one_retained(&teardown, &expected_records),
        expect("an incomplete order published", fixture.bundle().is_none()),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// The refused record's exact ID is the failed delete's `record_id` field
/// and appears in the provision's terminal: an event field, never a label.
fn expect_record_named(observed: &Observed, id: &str) -> Row {
    let deletes = dns_failures(
        observed,
        IntegrationOperation::DeleteTxt,
        IntegrationFailure::PermissionDenied,
    )?;
    let provisions = dns_failures(
        observed,
        IntegrationOperation::Provision,
        IntegrationFailure::CleanupIncomplete,
    )?;
    all([
        expect_eq(
            "the failed delete's record_id",
            deletes
                .iter()
                .map(|event| field_value(event, "record_id"))
                .collect::<Vec<_>>(),
            vec![Some(id)],
        ),
        expect(
            "the incomplete provision's terminal does not name the unresolved record",
            !provisions.is_empty() && provisions.iter().all(|event| event.contains(id)),
        ),
    ])
}

/// A due public renewal runs one order: one cache read at the check, one
/// successful renewal terminal, one cache publication, and one close. Stopping
/// the owner after its next interval wait began adds no renewal terminal.
fn dns_renewal_settles_one_order() -> Row {
    use IntegrationOperation::{CacheRead, CacheWrite, Close, Provision, Renew};
    let fixture = Fixture::start(Scripts::default())?;
    fixture.seed(&generation_expiring(&TWO_ZONES, DUE_DAYS)?)?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let store = served_store(&acme)?;
    let provider = token_provider(&fixture)?;
    let controller = runtime_schedule();
    let observation = Observation::start();
    let (renewed, teardown) = run_observing(scheduled(&controller), || -> Row {
        let handle = acme.spawn_renewal(provider, store);
        run_check(&controller, 1)?;
        retire_renewal(handle, ROW_BOUND)
    });
    let observed = observation.finish();
    let log = fixture.peers.cloudflare.log();
    let expected = terminals([
        &cleaned_records(TWO_ZONES.len()),
        &[
            success(DNS, CacheRead),
            success(DNS, Renew),
            success(DNS, CacheWrite),
            success(DNS, Close),
        ],
    ]);
    let checks = all([
        observed_verdict(renewed),
        expect_clean_teardown(teardown),
        expect_cleaned(&log, TWO_ZONES.len()),
        observed.expect_terminals(&expected),
        observed.scrapes().and_then(|(before, after)| {
            expect_eq(
                "successful renewal counter delta",
                delta(before, after, OPERATIONS_TOTAL, ("dns01", "renew", SUCCESS)),
                1.0,
            )
        }),
        expect_unreached(&observed, DNS, &[Provision]),
        observed.expect_one_instance(DNS),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

// ── renewal ───────────────────────────────────────────────────────────

/// Cancelling a public renewal while its admitted order holds at the ACME
/// challenge is one cancelled renewal terminal. The record it raised is
/// deleted, nothing is published, and the owner closes once.
fn dns_renewal_cancelled_during_its_order_is_one_cancelled_terminal() -> Row {
    use IntegrationFailure::Cancelled;
    use IntegrationOperation::{CacheRead, CacheWrite, Close, Renew};
    let fixture = Fixture::start(Scripts {
        challenge: Script::answer().nth(1, Stage::Hold),
        ..Scripts::default()
    })?;
    let due = generation_expiring(&TWO_ZONES, DUE_DAYS)?;
    fixture.seed(&due)?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let store = served_store(&acme)?;
    let provider = token_provider(&fixture)?;
    let directory = fixture.peers.acme.clone();
    let controller = runtime_schedule();
    let observation = Observation::start();
    let (cancelled, teardown) = run_observing(scheduled(&controller), || -> Row {
        let handle = acme.spawn_renewal(provider, store);
        elapse_renewal(&controller, 1, ROW_BOUND)?;
        bounded(
            "the renewal's first challenge to be held",
            directory.until(|log| log.held >= 1),
        )?;
        retire_renewal(handle, ROW_BOUND)
    });
    let observed = observation.finish();
    let log = fixture.peers.cloudflare.log();
    let expected = terminals([
        &cleaned_records(1),
        &[
            success(DNS, CacheRead),
            failed_unchecked(DNS, Renew, Cancelled),
            success(DNS, Close),
        ],
    ]);
    let checks = all([
        observed_verdict(cancelled),
        expect_clean_teardown(teardown),
        expect_cleaned(&log, 1),
        observed.expect_terminals(&expected),
        observed.expect_absent(DNS, CacheWrite),
        observed.expect_one_instance(DNS),
        expect_eq(
            "the cached generation after the cancelled order",
            fixture.bundle(),
            Some(due.into_bytes()),
        ),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// The runtime-managed owner reads a fresh cache once at startup without an
/// order. Once the cache falls due, its next check reads once more and runs
/// one renewal order. The runtime's stop closes the owner once, under
/// shutdown.
fn dns_runtime_renewal_settles_one_order() -> Row {
    use IntegrationOperation::{CacheRead, CacheWrite, Close, Provision, Renew};
    let fixture = Fixture::start(Scripts::default())?;
    fixture.seed(&generation_expiring(&TWO_ZONES, FRESH_DAYS)?)?;
    let controller = runtime_schedule();
    let builder = startup(&fixture, &controller)?;
    let observation = Observation::start();
    let (renewed, teardown) = run_observing(builder, || -> Row {
        serve_until_first_wait(&controller)?;
        fixture.seed(&generation_expiring(&TWO_ZONES, DUE_DAYS)?)?;
        run_check(&controller, 1)
    });
    let observed = observation.finish();
    let log = fixture.peers.cloudflare.log();
    let expected = terminals([
        &cleaned_records(TWO_ZONES.len()),
        &[
            success(DNS, CacheRead).times(2),
            success(DNS, Renew),
            success(DNS, CacheWrite),
            success(DNS, Close).under_shutdown(),
        ],
    ]);
    let checks = all([
        observed_verdict(renewed),
        expect_clean_teardown(teardown),
        expect_cleaned(&log, TWO_ZONES.len()),
        expect_every_authority(&log),
        observed.expect_terminals(&expected),
        observed.expect_absent(DNS, Provision),
        observed.expect_one_instance(DNS),
        expect_eq(
            "the cached leaf after the renewal",
            fixture.bundle().and_then(|bundle| leaf_of(&bundle)),
            fixture.peers.acme.log().issued,
        ),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// A due check against a full report budget reads the cache once, then its
/// renewal is one `Busy` terminal with no duration. It sends no provider or
/// directory request, and the owner still closes once.
fn dns_renewal_refused_by_the_report_budget_has_no_duration() -> Row {
    use IntegrationFailure::Busy;
    use IntegrationOperation::{
        CacheRead, CacheWrite, Close, CreateTxt, DeleteTxt, Provision, Renew, ZoneLookup,
    };
    let fixture = Fixture::start(Scripts::default())?;
    fixture.seed(&generation_expiring(&TWO_ZONES, DUE_DAYS)?)?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let store = served_store(&acme)?;
    let provider = token_provider(&fixture)?;
    let controller = runtime_schedule();
    let (refused, teardown) = run_observing(scheduled(&controller), || {
        let handle = acme.spawn_renewal(provider, store);
        await_renewal_waits(&controller, 1, ROW_BOUND)?;
        let budget = hold_budget(0)?;
        let observation = Observation::start();
        run_check(&controller, 1)?;
        release_budget(budget)?;
        retire_renewal(handle, ROW_BOUND)?;
        Ok(observation)
    });
    let observed = observed_verdict(refused)?.finish();
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        expect_clean_teardown(teardown),
        observed.expect_terminals(&[
            success(DNS, CacheRead),
            failed(DNS, Renew, Busy, Retryability::Safe).before_admission(),
            success(DNS, Close),
        ]),
        expect_unreached(
            &observed,
            DNS,
            &[ZoneLookup, CreateTxt, DeleteTxt, CacheWrite, Provision],
        ),
        observed.expect_one_instance(DNS),
        expect_eq(
            "provider requests of the refused renewal",
            log.requests.len(),
            0,
        ),
        expect_eq(
            "challenges of the refused renewal",
            fixture.peers.acme.log().challenges,
            0,
        ),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// A cache directory that denies writes until dropped.
struct ReadOnly(PathBuf);

impl ReadOnly {
    /// Deny writes to `dir`, and prove the denial holds for this user.
    fn deny(dir: &Path) -> Result<Self, String> {
        set_mode(dir, 0o555)?;
        let guard = Self(dir.to_owned());
        match std::fs::write(dir.join(".write-witness"), b"") {
            Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied => Ok(guard),
            Err(error) => Err(format!("the write witness failed otherwise: {error}")),
            Ok(()) => Err(
                "the cache accepted a write under mode 0555: this user bypasses file modes"
                    .to_owned(),
            ),
        }
    }
}

impl Drop for ReadOnly {
    fn drop(&mut self) {
        drop(set_mode(&self.0, 0o755));
    }
}

fn set_mode(dir: &Path, mode: u32) -> Row {
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(mode))
        .map_err(|error| format!("set {} to {mode:o}: {error}", dir.display()))
}

/// A renewal whose publication is denied is one failed renewal and one
/// failed cache write after a successful first renewal. Its records are
/// deleted, the prior generation stays on disk, and the store keeps serving
/// the first renewal's leaf.
fn dns_failed_publication_keeps_the_prior_generation() -> Row {
    use IntegrationFailure::PermissionDenied;
    use IntegrationOperation::{
        CacheRead, CacheWrite, Close, CreateTxt, DeleteTxt, Renew, ZoneLookup,
    };
    let fixture = Fixture::start(Scripts::default())?;
    fixture.seed(&generation_expiring(&TWO_ZONES, DUE_DAYS)?)?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let store = served_store(&acme)?;
    let serving = store.clone();
    let provider = token_provider(&fixture)?;
    let controller = runtime_schedule();
    let reseeded = generation_expiring(&TWO_ZONES, DUE_DAYS)?;
    let mut first_leaf = None;
    let observation = Observation::start();
    let (renewed, teardown) = run_observing(scheduled(&controller), || -> Row {
        let handle = acme.spawn_renewal(provider, store);
        run_check(&controller, 1)?;
        first_leaf = Some(leaf(fixture.bundle(), "the first renewal's bundle")?);
        fixture.seed(&reseeded)?;
        let denied = ReadOnly::deny(fixture.cache())?;
        run_check(&controller, 2)?;
        drop(denied);
        retire_renewal(handle, ROW_BOUND)
    });
    let observed = observation.finish();
    let log = fixture.peers.cloudflare.log();
    let expected = terminals([
        &[
            success(DNS, ZoneLookup).times(2),
            success(DNS, CreateTxt).times(2 * TWO_ZONES.len()),
            success(DNS, DeleteTxt).times(2 * TWO_ZONES.len()),
        ],
        &[
            success(DNS, CacheRead).times(2),
            success(DNS, Renew),
            failed_unchecked(DNS, Renew, PermissionDenied),
            success(DNS, CacheWrite),
            failed(DNS, CacheWrite, PermissionDenied, Retryability::Never),
            success(DNS, Close),
        ],
    ]);
    let checks = all([
        observed_verdict(renewed),
        expect_clean_teardown(teardown),
        expect_cleaned(&log, 2 * TWO_ZONES.len()),
        observed.expect_terminals(&expected),
        observed.expect_one_instance(DNS),
        expect_eq(
            "the generation on disk after the denied publication",
            fixture.bundle(),
            Some(reseeded.into_bytes()),
        ),
        expect_eq(
            "the leaf served after the denied publication",
            served_leaf(&serving),
            first_leaf,
        ),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// Put a directory where the cache's bundle file stood.
fn obstruct_bundle(fixture: &Fixture) -> Row {
    let bundle = fixture.cache().join(BUNDLE);
    std::fs::remove_file(&bundle)
        .and_then(|()| std::fs::create_dir(&bundle))
        .and_then(|()| std::fs::write(bundle.join("occupant"), b"occupied"))
        .map_err(|error| format!("obstruct the bundle: {error}"))
}

/// A cache the check cannot read is one failed cache read; the renewal it
/// starts raises and deletes its records, then cannot publish over the
/// obstruction: one failed cache write and one failed renewal. The store keeps
/// serving its prior leaf.
fn dns_unreadable_cache_is_one_failed_read_per_check() -> Row {
    use IntegrationFailure::Unavailable;
    use IntegrationOperation::{CacheRead, CacheWrite, Close, Renew};
    let fixture = Fixture::start(Scripts::default())?;
    fixture.seed(&generation_expiring(&TWO_ZONES, DUE_DAYS)?)?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let store = served_store(&acme)?;
    let serving = store.clone();
    let prior = served_leaf(&store);
    obstruct_bundle(&fixture)?;
    let provider = token_provider(&fixture)?;
    let controller = runtime_schedule();
    let observation = Observation::start();
    let (renewed, teardown) = run_observing(scheduled(&controller), || -> Row {
        let handle = acme.spawn_renewal(provider, store);
        run_check(&controller, 1)?;
        retire_renewal(handle, ROW_BOUND)
    });
    let observed = observation.finish();
    let log = fixture.peers.cloudflare.log();
    let expected = terminals([
        &cleaned_records(TWO_ZONES.len()),
        &[
            failed_as(DNS, unavailable(CacheRead)),
            failed_unchecked(DNS, Renew, Unavailable),
            failed_as(DNS, unavailable(CacheWrite)),
            success(DNS, Close),
        ],
    ]);
    let checks = all([
        observed_verdict(renewed),
        expect_clean_teardown(teardown),
        expect_cleaned(&log, TWO_ZONES.len()),
        observed.expect_terminals(&expected),
        observed.expect_one_instance(DNS),
        expect_eq(
            "the leaf served after the failed publication",
            served_leaf(&serving),
            prior,
        ),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

// ── preparation ───────────────────────────────────────────────────────

/// A Cloudflare-shaped peer that denies every zone query.
fn denying_zone_peer() -> Result<MockServer, String> {
    outside_camber(async {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/zones"))
            .respond_with(ResponseTemplate::new(403).set_body_json(json!({
                "success": false, "result": null, "errors": [{"code": 9109}]
            })))
            .mount(&server)
            .await;
        server
    })
}

/// Every write request `server` received.
fn writes_to(server: &MockServer) -> Result<usize, String> {
    let received = outside_camber(server.received_requests())?
        .ok_or("the denying peer does not record requests")?;
    Ok(received
        .iter()
        .filter(|request| request.method.as_str() != "GET")
        .count())
}

/// A denied preparation is one denied zone lookup and one failed provision.
/// No record is raised or deleted, and the owner closes once.
fn dns_denied_preparation_stops_before_any_record() -> Row {
    use IntegrationFailure::PermissionDenied;
    use IntegrationOperation::{CacheWrite, Close, CreateTxt, DeleteTxt, Provision, ZoneLookup};
    let fixture = Fixture::start(Scripts::default())?;
    let denying = denying_zone_peer()?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = provider_at(&denying.uri())?;
    let (caller, teardown, observed) = observe_provision(&acme, provider);
    let checks = all([
        observed_verdict(caller).and_then(|answer| {
            expect_refused(
                "the denied preparation",
                answer,
                permission_denied(ZoneLookup),
            )
        }),
        expect_clean_teardown(teardown),
        observed.expect_terminals(&[
            failed(DNS, ZoneLookup, PermissionDenied, Retryability::Never),
            failed_unchecked(DNS, Provision, PermissionDenied),
            success(DNS, Close),
        ]),
        expect_unreached(&observed, DNS, &[CreateTxt, DeleteTxt, CacheWrite]),
        observed.expect_one_instance(DNS),
        writes_to(&denying).and_then(|writes| expect_eq("writes the provider sent", writes, 0)),
        observed.expect_redacted(&[DNS_TOKEN]),
    ]);
    all([checks, fixture.finish()])
}

/// A provider whose preparation never finishes by itself, signalling when it
/// begins and when its owner drops it.
#[derive(Default)]
struct Stalled {
    entered: Option<oneshot::Sender<()>>,
    dropped: Option<oneshot::Sender<()>>,
}

impl DnsProvider for Stalled {
    fn prepare(
        &mut self,
        _domains: &[Arc<str>],
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        let entered = self.entered.take();
        async move {
            if let Some(entered) = entered {
                entered.send(()).unwrap_or_default();
            }
            std::future::pending::<()>().await;
            Ok(())
        }
    }

    fn create_txt_record(
        &self,
        _fqdn: &str,
        _value: &str,
    ) -> impl Future<Output = Result<RecordId, RuntimeError>> + Send {
        std::future::ready(Err(RuntimeError::Config(
            "a stalled preparation reached a record create".into(),
        )))
    }

    fn delete_txt_record(
        &self,
        _record_id: &str,
    ) -> impl Future<Output = Result<(), RuntimeError>> + Send {
        std::future::ready(Err(RuntimeError::Config(
            "a stalled preparation reached a record delete".into(),
        )))
    }
}

impl Drop for Stalled {
    fn drop(&mut self) {
        if let Some(dropped) = self.dropped.take() {
            dropped.send(()).unwrap_or_default();
        }
    }
}

/// A preparation that outlasts the order bound is one timed-out zone lookup
/// and one timed-out provision, whose duration lies between that bound and
/// the caller's own wait. The owner closes once.
fn dns_preparation_timeout_is_bounded_once() -> Row {
    use IntegrationFailure::Timeout;
    use IntegrationOperation::{CacheWrite, Close, CreateTxt, DeleteTxt, Provision, ZoneLookup};
    let fixture = Fixture::start(Scripts::default())?;
    let acme = fixture
        .configuration(&TWO_ZONES)?
        .operation_timeout(EXHAUSTED);
    let observation = Observation::start();
    let (caller, teardown) = run_observing(runtime::builder(), || -> Result<Duration, String> {
        let (answer, waited) = timed("provision_cert", acme.provision_cert(Stalled::default()))?;
        expect_refused("the stalled preparation", answer, expired(ZoneLookup))?;
        Ok(waited)
    });
    let observed = observation.finish();
    let checks = all([
        observed_verdict(caller)
            .and_then(|waited| expect_timeout_duration(&observed, DNS, Provision, waited)),
        expect_clean_teardown(teardown),
        observed.expect_terminals(&[
            failed_as(DNS, expired(ZoneLookup)),
            failed_unchecked(DNS, Provision, Timeout),
            success(DNS, Close),
        ]),
        expect_unreached(&observed, DNS, &[CreateTxt, DeleteTxt, CacheWrite]),
        observed.expect_one_instance(DNS),
        expect_eq(
            "provider requests of a stalled preparation",
            fixture.peers.cloudflare.log().requests.len(),
            0,
        ),
    ]);
    all([checks, fixture.finish()])
}

/// Dropping the provision waiter while preparation is held is one cancelled
/// zone lookup and one cancelled provision. The owner closes once, on its
/// own and not under the runtime's stop.
fn dns_waiter_drop_cancels_preparation_once() -> Row {
    use IntegrationFailure::Cancelled;
    use IntegrationOperation::{CacheWrite, Close, CreateTxt, DeleteTxt, Provision, ZoneLookup};
    let fixture = Fixture::start(Scripts::default())?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let (entered, entered_rx) = oneshot::channel();
    let (dropped, dropped_rx) = oneshot::channel();
    let provider = Stalled {
        entered: Some(entered),
        dropped: Some(dropped),
    };
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        // Every live slot but one, so the DNS owner's slot is the last.
        let others = hold_live_slots(LIVE_LIMIT - 1, IntegrationKind::Nats)?;
        let waiter = camber::spawn_async(async move { acme.provision_cert(provider).await });
        await_signal("preparation to begin", ROW_BOUND, entered_rx)?;
        waiter.cancel();
        drop(waiter);
        await_signal("the owner to drop the provider", ROW_BOUND, dropped_rx)?;
        let settled = runtime::block_on(integration_admitted_after(
            IntegrationOperation::Connect,
            ROW_BOUND,
            || std::future::ready(IntegrationLifecycleProbe::admit(IntegrationKind::Nats)),
        ));
        drop(others);
        settled
    });
    let observed = observation.finish();
    let checks = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            failed(DNS, ZoneLookup, Cancelled, Retryability::Safe),
            failed_unchecked(DNS, Provision, Cancelled),
            success(DNS, Close),
        ]),
        expect_unreached(&observed, DNS, &[CreateTxt, DeleteTxt, CacheWrite]),
        observed.expect_one_instance(DNS),
    ]);
    all([checks, fixture.finish()])
}

// ── records ───────────────────────────────────────────────────────────

/// A create the peer refuses is one rejected create and one failed
/// provision. Nothing was committed, so nothing is deleted, and the owner
/// closes once.
fn dns_rejected_create_settles_once() -> Row {
    use IntegrationFailure::Rejected;
    use IntegrationOperation::{CacheWrite, Close, CreateTxt, DeleteTxt, Provision, ZoneLookup};
    let fixture = Fixture::start(Scripts {
        create: Script::every(Stage::Refuse),
        ..Scripts::default()
    })?;
    let acme = fixture.configuration(&ONE)?;
    let provider = token_provider(&fixture)?;
    let (caller, teardown, observed) = observe_provision(&acme, provider);
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        observed_verdict(caller)
            .and_then(|answer| expect_refused("the refused create", answer, rejected(CreateTxt))),
        expect_clean_teardown(teardown),
        expect_eq("creates the peer received", log.creates.len(), 1),
        expect_eq("records the peer committed", log.acknowledged().len(), 0),
        observed.expect_terminals(&[
            success(DNS, ZoneLookup),
            failed(DNS, CreateTxt, Rejected, Retryability::Never),
            failed_unchecked(DNS, Provision, Rejected),
            success(DNS, Close),
        ]),
        expect_unreached(&observed, DNS, &[DeleteTxt, CacheWrite]),
        observed.expect_one_instance(DNS),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// A create whose answer is lost after the peer committed it is one
/// unknown-outcome create. Cleanup cannot name an ID it never learned, so no
/// delete runs: the provision and the close are incomplete, and the caller's
/// account and the one retained account name the record by its domain.
fn dns_lost_create_answer_is_one_unknown_outcome() -> Row {
    use IntegrationFailure::{CleanupIncomplete, OutcomeUnknown};
    use IntegrationOperation::{CacheWrite, Close, CreateTxt, DeleteTxt, Provision, ZoneLookup};
    let fixture = Fixture::start(Scripts {
        create: Script::every(Stage::Lose),
        ..Scripts::default()
    })?;
    let acme = fixture.configuration(&ONE)?;
    let provider = token_provider(&fixture)?;
    let (caller, teardown, observed) = observe_provision(&acme, provider);
    let log = fixture.peers.cloudflare.log();
    let expected_records = expected_unresolved(&log, &[]);
    let checks = all([
        observed_verdict(caller)
            .and_then(|answer| expect_caller_account(&answer, &expected_records)),
        expect_eq(
            "records left unresolved",
            &expected_records,
            &ONE.iter()
                .map(|domain| ((*domain).to_owned(), None))
                .collect::<Named>(),
        ),
        expect_eq(
            "records the zone holds: the unrelated one and the lost create's",
            log.records.len(),
            2,
        ),
        observed.expect_terminals(&[
            success(DNS, ZoneLookup),
            failed(DNS, CreateTxt, OutcomeUnknown, Retryability::OutcomeUnknown),
            failed_unchecked(DNS, Provision, CleanupIncomplete),
            failed_unchecked(DNS, Close, CleanupIncomplete),
        ]),
        expect_unreached(&observed, DNS, &[DeleteTxt, CacheWrite]),
        expect_eq("deletes the peer received", log.deletes.len(), 0),
        observed.expect_one_instance(DNS),
        expect_identities_in(&observed, Provision, &expected_records),
        expect_one_retained(&teardown, &expected_records),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// A runtime stop while the order holds at its first challenge sends that
/// record's delete; the peer never answers, and the aggregate deadline cuts it
/// short. The outstanding delete is one cancelled terminal, and the provision
/// and close are incomplete, all under shutdown. The one retained account
/// names the record by its exact ID.
fn dns_forced_stop_cancels_the_outstanding_delete_once() -> Row {
    use IntegrationFailure::{Cancelled, CleanupIncomplete};
    use IntegrationOperation::{CacheWrite, Close, CreateTxt, DeleteTxt, Provision, ZoneLookup};
    let fixture = Fixture::start(Scripts {
        challenge: Script::answer().nth(1, Stage::Hold),
        delete: Script::every(Stage::Hold),
        ..Scripts::default()
    })?;
    let acme = fixture
        .configuration(&TWO_ZONES)?
        .cleanup_timeout(LONG_CLEANUP);
    let provider = token_provider(&fixture)?;
    let directory = fixture.peers.acme.clone();
    let builder = runtime::builder().shutdown_timeout(FORCED_SHUTDOWN);
    let observation = Observation::start();
    let (stopped, teardown) = run_observing(builder, move || {
        let waiter = camber::spawn_async(async move { acme.provision_cert(provider).await });
        let held = bounded(
            "the first challenge to be held",
            directory.until(|log| log.held >= 1),
        );
        runtime::request_shutdown();
        held.map(|()| waiter)
    });
    let observed = observation.finish();
    drop(stopped);
    let log = fixture.peers.cloudflare.log();
    let expected_records = expected_unresolved(&log, &[]);
    let checks = all([
        expect_eq("creates the peer acknowledged", log.acknowledged().len(), 1),
        expect_eq("records left unresolved", expected_records.len(), 1),
        expect(
            "the outstanding record was deleted",
            log.acknowledged()
                .iter()
                .filter_map(|create| create.id.as_deref())
                .all(|id| log.records.contains_key(id)),
        ),
        observed.expect_terminals(&[
            success(DNS, ZoneLookup),
            success(DNS, CreateTxt),
            failed_unchecked(DNS, DeleteTxt, Cancelled).under_shutdown(),
            failed_unchecked(DNS, Provision, CleanupIncomplete).under_shutdown(),
            failed_unchecked(DNS, Close, CleanupIncomplete).under_shutdown(),
        ]),
        observed.expect_absent(DNS, CacheWrite),
        observed.expect_one_instance(DNS),
        expect_identities_in(&observed, Provision, &expected_records),
        expect_one_retained(&teardown, &expected_records),
        expect("a stopped order published", fixture.bundle().is_none()),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// The domain of the record the unwound order's provider acknowledged.
const UNWOUND_DOMAIN: &str = "app.example.com";

/// The exact ID the provider acknowledged for the unwound order's record.
const UNWOUND_RECORD: &str = "unwound-record-7c1d";

/// Retained order work that unwinds after the provider acknowledged a record
/// settles as one incomplete provision, as a forced drop does: its
/// `unresolved` event field names the record's exact ID. The caller reads the
/// unwind, and the runtime retains one account naming the same record.
fn dns_unwound_order_names_its_unresolved_record() -> Row {
    use IntegrationFailure::CleanupIncomplete;
    use IntegrationOperation::Provision;
    let expected_records: Named =
        vec![(UNWOUND_DOMAIN.to_owned(), Some(UNWOUND_RECORD.to_owned()))];
    let observation = Observation::start();
    let (caller, teardown) = run_observing(runtime::builder(), || -> Result<_, String> {
        let probe = IntegrationLifecycleProbe::admit(DNS)
            .map_err(|error| format!("the order's instance was refused: {error:?}"))?;
        let waiter = probe
            .run_retained(|cleanup| async move {
                match cleanup.acknowledged(UNWOUND_DOMAIN, UNWOUND_RECORD) {
                    true => panic!("the order unwinds after its record was acknowledged"),
                    false => Ok(()),
                }
            })
            .map_err(|error| format!("the order was refused: {error:?}"))?;
        bounded("the unwound order", waiter.wait())
    });
    let observed = observation.finish();
    let unresolved = dns_failures(&observed, Provision, CleanupIncomplete).map(|events| {
        events
            .iter()
            .map(|event| field_value(event, "unresolved"))
            .collect::<Vec<_>>()
    });
    all([
        observed_verdict(caller).and_then(|answer| match answer {
            Err(RuntimeError::TaskPanicked(_)) => Ok(()),
            other => Err(format!("the caller read {other:?}, not the unwind")),
        }),
        observed.expect_terminals(&[failed_unchecked(DNS, Provision, CleanupIncomplete)]),
        unresolved.and_then(|fields| {
            expect_eq(
                "the incomplete provision's unresolved field",
                fields,
                vec![Some(UNWOUND_RECORD)],
            )
        }),
        expect_one_retained(&teardown, &expected_records),
    ])
}

/// An order the directory refuses at finalize is one failed provision
/// carrying the caller's failure. Both records it raised are deleted, nothing
/// is published, and the owner closes once.
fn dns_order_failure_cleans_up_every_record() -> Row {
    use IntegrationOperation::{CacheWrite, Close, Provision};
    let fixture = Fixture::start(Scripts {
        finalize: Stage::Refuse,
        ..Scripts::default()
    })?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = token_provider(&fixture)?;
    let (caller, teardown, observed) = observe_provision(&acme, provider);
    let log = fixture.peers.cloudflare.log();
    let failure = observed_verdict(caller)
        .and_then(|answer| caller_failure("the refused finalize", &answer))?;
    let expected = terminals([
        &cleaned_records(TWO_ZONES.len()),
        &[
            failed_unchecked(DNS, Provision, failure),
            success(DNS, Close),
        ],
    ]);
    let checks = all([
        expect(
            &format!("the refused finalize answered {failure:?}"),
            failure != IntegrationFailure::CleanupIncomplete,
        ),
        expect_clean_teardown(teardown),
        expect_cleaned(&log, TWO_ZONES.len()),
        observed.expect_terminals(&expected),
        observed.expect_absent(DNS, CacheWrite),
        observed.expect_one_instance(DNS),
        expect("a failed order published", fixture.bundle().is_none()),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

// ── refusals before admission ─────────────────────────────────────────

/// Every operation an admitted owner reaches.
const OWNER_OPERATIONS: [IntegrationOperation; 7] = [
    IntegrationOperation::ZoneLookup,
    IntegrationOperation::CreateTxt,
    IntegrationOperation::DeleteTxt,
    IntegrationOperation::CacheRead,
    IntegrationOperation::CacheWrite,
    IntegrationOperation::Renew,
    IntegrationOperation::Close,
];

/// A provision refused before admission is its one terminal: no instance, no
/// duration, no later operation, and no request.
fn expect_refused_alone(observed: &Observed, expected: Terminal, fixture: &Fixture) -> Row {
    all([
        observed.expect_terminals(&[expected]),
        expect_unreached(observed, DNS, &OWNER_OPERATIONS),
        observed.expect_no_instance(DNS),
        observed.expect_redacted(&[DNS_TOKEN]),
        expect_eq(
            "provider requests of a refused provision",
            fixture.peers.cloudflare.log().requests.len(),
            0,
        ),
    ])
}

/// Outside a Camber runtime the provision is one closed refusal terminal.
fn dns_missing_runtime_refusal_is_one_terminal() -> Row {
    let fixture = Fixture::start(Scripts::default())?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = token_provider(&fixture)?;
    let observation = Observation::start();
    let answered = outside_camber(acme.provision_cert(provider));
    let observed = observation.finish();
    let checks = all([
        answered.and_then(|answer| expect_no_runtime("provision outside Camber", answer)),
        expect_refused_alone(
            &observed,
            refused(DNS, IntegrationOperation::Provision),
            &fixture,
        ),
    ]);
    all([checks, fixture.finish()])
}

/// A provision after root admission closed is one closed refusal terminal.
fn dns_closed_scope_refusal_is_one_terminal() -> Row {
    let fixture = Fixture::start(Scripts::default())?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = token_provider(&fixture)?;
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        runtime::request_shutdown();
        expect_scope_closed(
            "provision after closure",
            bounded("provision_cert", acme.provision_cert(provider))?,
        )
    });
    let observed = observation.finish();
    let checks = all([
        clean_run(outcome),
        expect_refused_alone(
            &observed,
            refused(DNS, IntegrationOperation::Provision),
            &fixture,
        ),
    ]);
    all([checks, fixture.finish()])
}

/// A provision refused at the live-integration limit is one `Busy` terminal.
fn dns_live_limit_refusal_reaches_no_later_operation() -> Row {
    use IntegrationOperation::Provision;
    let fixture = Fixture::start(Scripts::default())?;
    let acme = fixture.configuration(&TWO_ZONES)?;
    let provider = token_provider(&fixture)?;
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let held = hold_live_slots(LIVE_LIMIT, IntegrationKind::Nats)?;
        let answer = bounded("provision_cert", acme.provision_cert(provider))?;
        drop(held);
        expect_refused("provision at the live limit", answer, busy(Provision))
    });
    let observed = observation.finish();
    let checks = all([
        clean_run(outcome),
        expect_refused_alone(
            &observed,
            failed_as(DNS, busy(Provision)).before_admission(),
            &fixture,
        ),
    ]);
    all([checks, fixture.finish()])
}

/// A provision whose order bound is zero is one `InvalidConfig` terminal.
fn dns_invalid_configuration_refusal_has_no_duration() -> Row {
    use IntegrationOperation::Provision;
    let fixture = Fixture::start(Scripts::default())?;
    let acme = fixture
        .configuration(&TWO_ZONES)?
        .operation_timeout(Duration::ZERO);
    let provider = token_provider(&fixture)?;
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        expect_refused(
            "provision with a zero order bound",
            bounded("provision_cert", acme.provision_cert(provider))?,
            invalid_config(Provision),
        )
    });
    let observed = observation.finish();
    let checks = all([
        clean_run(outcome),
        expect_refused_alone(
            &observed,
            failed_as(DNS, invalid_config(Provision)).before_admission(),
            &fixture,
        ),
    ]);
    all([checks, fixture.finish()])
}

// ── startup ───────────────────────────────────────────────────────────

/// Startup over an empty cache reads it once, finding nothing, and runs one
/// provision order that queries both authorities and publishes once. The
/// runtime's stop closes the owner once, under shutdown, with no renewal.
fn dns_startup_with_no_cache_provisions_once() -> Row {
    use IntegrationOperation::{CacheRead, CacheWrite, Close, Provision, Renew};
    let fixture = Fixture::start(Scripts::default())?;
    let (served, teardown, observed) = observe_startup(&fixture)?;
    let log = fixture.peers.cloudflare.log();
    let expected = terminals([
        &cleaned_records(TWO_ZONES.len()),
        &[
            success(DNS, CacheRead),
            success(DNS, Provision),
            success(DNS, CacheWrite),
            success(DNS, Close).under_shutdown(),
        ],
    ]);
    let checks = all([
        observed_verdict(served),
        expect_clean_teardown(teardown),
        expect_cleaned(&log, TWO_ZONES.len()),
        expect_every_authority(&log),
        observed.expect_terminals(&expected),
        observed.expect_absent(DNS, Renew),
        observed.expect_one_instance(DNS),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// Startup over a valid cache that is not due reads it once and serves it
/// with no order, no request, and no renewal. The runtime's stop closes the
/// owner once, under shutdown.
fn dns_startup_with_a_valid_cache_reads_once() -> Row {
    use IntegrationOperation::{
        CacheRead, CacheWrite, Close, CreateTxt, DeleteTxt, Provision, Renew, ZoneLookup,
    };
    let fixture = Fixture::start(Scripts::default())?;
    fixture.seed(&prior_generation(&TWO_ZONES)?)?;
    let (served, teardown, observed) = observe_startup(&fixture)?;
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        observed_verdict(served),
        expect_clean_teardown(teardown),
        observed.expect_terminals(&[
            success(DNS, CacheRead),
            success(DNS, Close).under_shutdown(),
        ]),
        expect_unreached(
            &observed,
            DNS,
            &[
                ZoneLookup, CreateTxt, DeleteTxt, Provision, CacheWrite, Renew,
            ],
        ),
        observed.expect_one_instance(DNS),
        expect_eq(
            "provider requests of a cached startup",
            log.requests.len(),
            0,
        ),
        observed.expect_redacted(&[DNS_TOKEN]),
    ]);
    all([checks, fixture.finish()])
}

/// Startup over a cached leaf that does not cover the configured domains is
/// one invalid-certificate read, then one provision order that publishes once.
/// The runtime's stop closes the owner once, under shutdown.
fn dns_startup_with_an_invalid_cache_orders_once() -> Row {
    use IntegrationFailure::InvalidCertificate;
    use IntegrationOperation::{CacheRead, CacheWrite, Close, Provision, Renew};
    let fixture = Fixture::start(Scripts::default())?;
    fixture.seed(&generation_expiring(&["other.example.net"], FRESH_DAYS)?)?;
    let (served, teardown, observed) = observe_startup(&fixture)?;
    let log = fixture.peers.cloudflare.log();
    let expected = terminals([
        &cleaned_records(TWO_ZONES.len()),
        &[
            failed(DNS, CacheRead, InvalidCertificate, Retryability::Never),
            success(DNS, Provision),
            success(DNS, CacheWrite),
            success(DNS, Close).under_shutdown(),
        ],
    ]);
    let checks = all([
        observed_verdict(served),
        expect_clean_teardown(teardown),
        expect_cleaned(&log, TWO_ZONES.len()),
        observed.expect_terminals(&expected),
        observed.expect_absent(DNS, Renew),
        observed.expect_one_instance(DNS),
        expect_eq(
            "the cached leaf after the fresh order",
            fixture.bundle().and_then(|bundle| leaf_of(&bundle)),
            fixture.peers.acme.log().issued,
        ),
        expect_dns_redacted(&observed, &log),
    ]);
    all([checks, fixture.finish()])
}

/// Write `bundle` to the cache as a legacy `cert.pem` and `key.pem` pair.
fn seed_legacy(fixture: &Fixture, bundle: &str) -> Row {
    const END: &str = "-----END CERTIFICATE-----";
    let (cert, key) = bundle
        .split_once(END)
        .ok_or("the generation holds no certificate")?;
    let cache = fixture.cache();
    std::fs::create_dir_all(cache)
        .and_then(|()| std::fs::write(cache.join("cert.pem"), format!("{cert}{END}\n")))
        .and_then(|()| std::fs::write(cache.join("key.pem"), key.trim_start()))
        .map_err(|error| format!("seed the legacy pair: {error}"))
}

/// Startup over a valid legacy pair reads it as one logical cache read and
/// migrates it as one cache write, with no order and no request. The runtime's
/// stop closes the owner once, under shutdown.
fn dns_startup_legacy_import_is_one_read_and_one_write() -> Row {
    use IntegrationOperation::{
        CacheRead, CacheWrite, Close, CreateTxt, DeleteTxt, Provision, Renew, ZoneLookup,
    };
    let fixture = Fixture::start(Scripts::default())?;
    seed_legacy(&fixture, &prior_generation(&TWO_ZONES)?)?;
    let (served, teardown, observed) = observe_startup(&fixture)?;
    let log = fixture.peers.cloudflare.log();
    let checks = all([
        observed_verdict(served),
        expect_clean_teardown(teardown),
        observed.expect_terminals(&[
            success(DNS, CacheRead),
            success(DNS, CacheWrite),
            success(DNS, Close).under_shutdown(),
        ]),
        expect_unreached(
            &observed,
            DNS,
            &[ZoneLookup, CreateTxt, DeleteTxt, Provision, Renew],
        ),
        observed.expect_one_instance(DNS),
        expect(
            "the legacy pair was not migrated",
            fixture.bundle().is_some(),
        ),
        expect_eq(
            "provider requests of a legacy startup",
            log.requests.len(),
            0,
        ),
        observed.expect_redacted(&[DNS_TOKEN]),
    ]);
    all([checks, fixture.finish()])
}

// ── stop window ───────────────────────────────────────────────────────

/// What the run's closure hands the row: one DNS instance that reports its
/// close, as an owner does, or the refusal that kept it from being one.
type Admitted = std::sync::mpsc::Receiver<Result<IntegrationProbeHandle, RuntimeError>>;

/// Drop the last handle of the instance `admitted` delivers while the stop
/// is held between root closure and the registry's own stop.
fn drop_inside_stop(controller: &RuntimeController, admitted: &Admitted) -> Row {
    let checkpoint = RuntimeCheckpoint::RegistryStopTransition;
    let probe = admitted
        .recv_timeout(ROW_BOUND)
        .map_err(|_| "the run never delivered its instance".to_owned())?
        .map_err(|error| format!("the instance was refused: {error:?}"))?;
    expect(
        "the stop never reached the registry",
        poll_until(ROW_BOUND, || controller.is_paused(checkpoint)),
    )?;
    drop(probe);
    controller
        .release(checkpoint)
        .map_err(|error| format!("releasing the registry stop: {error:?}"))
}

/// An owner instance whose last handle drops after root closure fired
/// `ScopeClosing` but before the registry's stop reached it is closed by
/// that stop: one close terminal, under shutdown. Work that wakes on
/// `ScopeClosing` and drops its owner is this case.
fn dns_close_requested_inside_the_stop_reports_under_shutdown() -> Row {
    use IntegrationOperation::Close;
    let controller = runtime_schedule();
    controller
        .pause_once(RuntimeCheckpoint::RegistryStopTransition)
        .map_err(|error| format!("arming the registry stop: {error:?}"))?;
    let (deliver, admitted) = std::sync::mpsc::sync_channel(1);
    let observation = Observation::start();
    let (dropped, teardown) = std::thread::scope(|scope| {
        let run = scope.spawn(|| {
            scheduled(&controller).run(move || {
                let owner = IntegrationLifecycleProbe::admit(DNS).inspect(|probe| {
                    probe.reports_close();
                });
                deliver.send(owner).unwrap_or_default();
            })
        });
        let dropped = drop_inside_stop(&controller, &admitted);
        controller.disarm();
        (dropped, run.join())
    });
    let observed = observation.finish();
    all([
        dropped,
        teardown
            .map_err(|_| "the runtime thread unwound".to_owned())
            .and_then(expect_clean_teardown),
        observed.expect_terminals(&[success(DNS, Close).under_shutdown()]),
        observed.expect_one_instance(DNS),
    ])
}
