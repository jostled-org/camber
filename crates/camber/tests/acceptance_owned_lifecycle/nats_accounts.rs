//! 5.T4: NATS report accounts across abandoned results and failed-close churn.
//!
//! Public NATS connections run inside one real runtime against scripted peers.
//! Controls first prove that successes and delivered errors retire their
//! accounts. Then sequential abandoned failures and instances whose close
//! cannot complete fill the runtime's 256 reserved-plus-retained accounts. At
//! saturation the next operation and the next connect are `Busy` with no peer
//! effect, a live instance still closes on its reserved account, and the
//! returned aggregate names every retained account under its own instance,
//! once.
//!
//! Every exact outcome follows an owner-committed fact: a delivered result, a
//! peer record, or an admission refusal. An abandoned failure is dropped unread
//! only after the operation limit admits the next operation, which the entry
//! commits once the failure is fixed.
#![cfg(feature = "nats")]

use crate::integration_rows::{
    CONTROL_ROUNDS, INVALID_NATS_SUBJECT, REPORT_BUDGET, Row, busy, expect_eq,
    expect_own_instances, expect_refused, integration_admitted_after, integration_aggregate,
    nats_instance, rejected, run_observing, timed_out,
};
use crate::nats_peer::{NatsPeer, PeerControl, Script, wait_unavailable};
use camber::mq::nats::{self, Connection};
use camber::{IntegrationKind, IntegrationOperation, RuntimeError, runtime};
use std::future::Future;
use std::task::Poll;
use std::time::Duration;

/// Instances churned through an incomplete close. The last one's admission
/// takes the final account.
const FAILED_CLOSES: usize = 5;

/// Abandoned failures: every account the live instance's close reservation
/// and the churned instances leave.
const ABANDONED: usize = REPORT_BUDGET - 1 - FAILED_CLOSES;

/// Attempts to catch a rejected publish before its waiter reads it.
const ABANDON_ATTEMPTS: usize = 100;

/// The hang guard every bounded wait runs under; never a timing assertion.
const ROW_BOUND: Duration = Duration::from_secs(20);

/// The close bound a churned instance exhausts on purpose.
const EXHAUSTED: Duration = Duration::from_millis(100);

fn bounded<F: Future>(what: &str, future: F) -> Result<F::Output, String> {
    crate::integration_rows::bounded(what, ROW_BOUND, future)
}

#[test]
fn nats_abandoned_results_and_failed_close_churn_exhaust_report_budget() {
    crate::integration_rows::run_rows(&[(
        "abandoned results and failed-close churn fill the report budget once",
        abandoned_results_and_churn_exhaust_report_budget,
    )]);
}

/// Controls retire, abandoned failures and failed-close churn fill every
/// account, and the one aggregate names each under its own instance.
fn abandoned_results_and_churn_exhaust_report_budget() -> Row {
    let steady = NatsPeer::start();
    let churn = NatsPeer::start();
    let steady_control = steady.control();
    let churn_control = churn.control();
    let steady_url = steady.url();
    let churn_url = churn.url();
    let (driven, outcome) = run_observing(runtime::builder(), || {
        drive_to_saturation(&steady_url, &steady_control, &churn_url, &churn_control)
    });

    let verdict = match (outcome, driven) {
        (_, None) => Err("the closure never reported".to_owned()),
        (_, Some(Err(reason))) => Err(reason),
        (Ok(()), Some(Ok(_))) => Err("a saturated budget left no aggregate".to_owned()),
        (Err(error @ RuntimeError::Lifecycle(_)), Some(Ok(expected))) => {
            expect_retained_accounts(&error, &expected)
        }
        (Err(other), Some(Ok(_))) => Err(format!("the runtime returned {other:?}")),
    };
    let verdict = steady.finished(ROW_BOUND, verdict);
    churn.finished(ROW_BOUND, verdict)
}

/// Retire the controls, fill the budget, and check saturation; the live
/// instance then closes on its reserved account.
fn drive_to_saturation(
    steady_url: &str,
    steady: &PeerControl,
    churn_url: &str,
    churn: &PeerControl,
) -> Result<Expected, String> {
    let connection = bounded(
        "connect",
        nats::builder(steady_url).max_in_flight(1).connect(),
    )?
    .map_err(|error| format!("connect: {error:?}"))?;
    let steady_id = nats_instance(&connection)?;
    controls_retire_their_accounts(&connection)?;
    for _ in 0..ABANDONED {
        abandon_one_failure(&connection)?;
    }
    let churned = churn_failed_closes(churn_url, churn)?;
    saturation_refuses_without_effects(&connection, steady)?;
    bounded("close at saturation", connection.close())?
        .map_err(|error| format!("close at saturation: {error:?}"))?;
    Ok(Expected {
        steady_id,
        abandoned: ABANDONED,
        churned,
    })
}

/// What the closure expects the aggregate to hold.
struct Expected {
    steady_id: u64,
    abandoned: usize,
    churned: Box<[u64]>,
}

/// More successes and more delivered errors than the whole budget, one at a
/// time: each account retires, so admission never saturates.
fn controls_retire_their_accounts(connection: &Connection) -> Row {
    for round in 0..CONTROL_ROUNDS {
        bounded("a control publish", connection.publish("control", b"ok"))?
            .map_err(|error| format!("control success {round}: {error:?}"))?;
        expect_refused(
            &format!("control delivered error {round}"),
            bounded(
                "a control publish",
                connection.publish(INVALID_NATS_SUBJECT, b"x"),
            )?,
            rejected(IntegrationOperation::Publish),
        )?;
    }
    Ok(())
}

/// Submit a publish the SDK rejects, wait until the entry commits its result,
/// then drop its waiter unread: an abandoned failure.
///
/// The connection admits one operation at a time, and an operation frees that
/// slot only once its result is fixed. The probe publish that follows is
/// therefore admitted only after the rejected operation's failure is fixed, so
/// the drop that follows abandons it rather than cancelling running work.
fn abandon_one_failure(connection: &Connection) -> Row {
    for _ in 0..ABANDON_ATTEMPTS {
        if abandoned(connection)? {
            return Ok(());
        }
    }
    Err(format!(
        "every one of {ABANDON_ATTEMPTS} rejected publishes was read before it could be abandoned"
    ))
}

/// One attempt: `false` when the waiter's first poll already read the
/// failure, which delivered it instead.
fn abandoned(connection: &Connection) -> Result<bool, String> {
    runtime::block_on(async {
        let mut publish = Box::pin(connection.publish(INVALID_NATS_SUBJECT, b"x"));
        match futures_util::poll!(publish.as_mut()) {
            Poll::Pending => {}
            Poll::Ready(outcome) => {
                expect_refused(
                    "a rejected publish",
                    outcome,
                    rejected(IntegrationOperation::Publish),
                )?;
                return Ok(false);
            }
        }
        integration_admitted_after(IntegrationOperation::Publish, ROW_BOUND, || {
            connection.publish("probe", b"p")
        })
        .await?;
        // Unread: the failure was published, never delivered.
        drop(publish);
        Ok(true)
    })
}

/// Connect instances whose close cannot complete: each one's failed-close
/// account outlives the instance.
fn churn_failed_closes(url: &str, control: &PeerControl) -> Result<Box<[u64]>, String> {
    let mut churned = Vec::with_capacity(FAILED_CLOSES);
    for _ in 0..FAILED_CLOSES {
        control.script(Script::Serve);
        let connection = bounded(
            "connect",
            nats::builder(url).shutdown_timeout(EXHAUSTED).connect(),
        )?
        .map_err(|error| format!("churn connect: {error:?}"))?;
        churned.push(nats_instance(&connection)?);
        control.script(Script::Refuse);
        wait_unavailable(&connection, ROW_BOUND)?;
        expect_refused(
            "a close the SDK can never acknowledge",
            bounded("close", connection.close())?,
            timed_out(IntegrationOperation::Close),
        )?;
    }
    Ok(churned.into_boxed_slice())
}

/// At saturation the next operation and the next connect are `Busy`, and
/// no peer sees anything.
///
/// The refused connect targets a fresh peer: a churned instance's SDK may
/// still be reconnecting to the churn peer after its close timed out, so
/// that peer's connection count is not Camber's to hold still.
fn saturation_refuses_without_effects(connection: &Connection, steady: &PeerControl) -> Row {
    let fresh = NatsPeer::start();
    let fresh_url = fresh.url();
    let verdict = saturated_operations_refuse(connection, steady, &fresh_url)
        .and_then(|()| fresh.control().expect_no_connection());
    fresh.finished(ROW_BOUND, verdict)
}

/// The publish, subscribe, and connect a saturated budget refuses, with no
/// effect on `steady`.
fn saturated_operations_refuse(connection: &Connection, steady: &PeerControl, url: &str) -> Row {
    let published = steady.log().published.len();
    expect_refused(
        "publish at saturation",
        bounded("publish", connection.publish("saturated", b"x"))?,
        busy(IntegrationOperation::Publish),
    )?;
    expect_refused(
        "subscribe at saturation",
        bounded("subscribe", connection.subscribe("saturated"))?,
        busy(IntegrationOperation::Subscribe),
    )?;
    expect_refused(
        "connect at saturation",
        bounded("connect", nats::connect(url))?,
        busy(IntegrationOperation::Connect),
    )?;
    expect_eq(
        "publishes the peer read",
        steady.log().published.len(),
        published,
    )?;
    expect_eq(
        "subscriptions the peer read",
        steady.log().subscribed.len(),
        0,
    )
}

/// Fail unless the aggregate holds exactly the expected retained accounts,
/// each under its own instance, once.
fn expect_retained_accounts(error: &RuntimeError, expected: &Expected) -> Row {
    expect_own_instances(error, IntegrationKind::Nats)?;
    let accounts = integration_aggregate(error, IntegrationKind::Nats)?;
    let abandoned_account = (expected.steady_id, rejected(IntegrationOperation::Publish));
    let abandoned = accounts
        .iter()
        .filter(|account| **account == abandoned_account)
        .count();
    expect_eq("abandoned accounts", abandoned, expected.abandoned)?;
    for id in &expected.churned {
        expect_eq(
            &format!("failed-close accounts of instance {id}"),
            accounts
                .iter()
                .filter(|(account, _)| account == id)
                .copied()
                .collect::<Vec<_>>(),
            vec![(*id, timed_out(IntegrationOperation::Close))],
        )?;
    }
    expect_eq(
        "every retained account",
        accounts.len(),
        expected.abandoned + expected.churned.len(),
    )
}
