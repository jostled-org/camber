//! 6.T3: SQS report accounts across delivered, unread, abandoned, and cut
//! results.
//!
//! Public SQS clients run inside one real runtime against a scripted peer, with
//! the real SDK transport. Controls first prove that successes and received
//! errors retire their accounts: each family runs more attempts than the
//! budget holds. Then clients are churned: each reads one error to learn its
//! identity, leaves one failure unread, abandons one submitted send, and
//! closes. Their live slots retire, and only the unread and abandoned failures
//! stay charged. Two live clients then take the last accounts with their close
//! reservations. At 256 reserved-plus-retained accounts the next operation
//! and the next connect are `Busy` with no request and no connection, each
//! live client still closes on its reserved account, and the returned
//! aggregate names every retained account under its own instance, once.
//!
//! SQS close has no separate transport handshake. Forced shutdown can still
//! report an outstanding operation as a close failure; `sqs_close` proves that
//! path. Here the failure that survives churn belongs to an operation the
//! close cuts. The first
//! churned clients close with a submitted send still held; the close settles
//! only after the cut send has published its failure, and the waiter then
//! drops it unread.
//!
//! Every exact outcome follows an owner-committed fact: a delivered result, a
//! peer record, a settled close, or an admission refusal. An unread failure is
//! dropped only after the operation limit admits the next operation, which the
//! entry commits once that failure is fixed. Each claim returns its own
//! verdict; the row reports every failed claim together with the peer's
//! teardown.
#![cfg(feature = "sqs")]

use crate::integration_rows::{
    REPORT_BUDGET, Refusal, Row, all, busy, cancelled, expect_eq, expect_ok, expect_own_instances,
    expect_polled_pending, expect_refused, instance_of, integration_admitted_after,
    integration_aggregate, rejected, run_observing, settled_within,
};
use crate::sqs_peer::{PeerControl, Reply, SqsPeer};
use camber::mq::sqs::{Client, SqsBuilder};
use camber::{IntegrationKind, IntegrationOperation, RuntimeError, runtime};
use std::collections::BTreeSet;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

/// Live clients whose close reservations take the last accounts.
const LIVE: usize = 2;

/// Churned clients whose close cuts a held send, each leaving one more
/// retained failure.
const CUT_CLOSES: usize = 2;

/// Churned clients, each leaving one unread and one abandoned failure.
const CHURNED: usize = (REPORT_BUDGET - LIVE - CUT_CLOSES) / 2;

const _: () = assert!(2 * CHURNED + CUT_CLOSES + LIVE == REPORT_BUDGET);

/// Sequential attempts of each control family: more than the whole budget,
/// so a family that leaked would saturate it on its own.
const CONTROL_ROUNDS: usize = REPORT_BUDGET + 44;

/// The hang guard every bounded wait runs under; never a timing assertion.
const ROW_BOUND: Duration = Duration::from_secs(20);

/// The close bound a cutting client exhausts on purpose.
const CUT_AFTER: Duration = Duration::from_millis(300);

/// A cutting client's operation bound: longer than the row, so only the close
/// can end its held send.
const HELD_BOUND: Duration = Duration::from_secs(60);

const QUEUE: &str = "accounts";

/// One named claim and its verdict.
type Claim = (&'static str, Row);

fn bounded<F: Future>(what: &str, future: F) -> Result<F::Output, String> {
    crate::integration_rows::bounded(what, ROW_BOUND, future)
}

/// The service error the client reads as a rejected publish.
const REJECTION: &str = "InvalidMessageContents";

/// Script the peer to answer the next send with a rejection.
fn script_rejection(control: &PeerControl) {
    control.script([Reply::error(400, REJECTION)]);
}

/// What the churn expects the aggregate to hold.
#[derive(Default)]
struct Expected {
    /// Every retained account, by instance.
    retained: Vec<(u64, Refusal)>,
    /// Every churned instance, in admission order.
    churned: Vec<u64>,
}

/// What the closure observed: every claim it checked, and the aggregate the
/// runtime must return once the churn reached saturation.
struct Observed {
    claims: Vec<Claim>,
    expected: Option<Expected>,
}

#[test]
fn sqs_result_receipt_controls_history_after_instance_retirement() {
    let peer = SqsPeer::start();
    // One operation at a time, so an operation's result is fixed once the
    // next is admitted.
    let builder = peer.builder().max_in_flight(1);
    let queue = peer.queue_url(QUEUE);
    let control = peer.control();
    let (observed, outcome) =
        run_observing(runtime::builder(), || drive(&builder, &queue, &control));
    let mut claims = match observed {
        None => vec![("the churn", Err("the runtime never ran it".to_owned()))],
        Some(Observed {
            mut claims,
            expected: Some(expected),
        }) => {
            claims.extend(aggregate_claims(&outcome, &expected));
            claims
        }
        Some(Observed {
            claims,
            expected: None,
        }) => claims,
    };
    claims.push(("peer teardown", peer.finish(ROW_BOUND)));
    let named = claims
        .into_iter()
        .map(|(name, row)| row.map_err(|reason| format!("{name}: {reason}")));
    if let Err(reasons) = all(named) {
        panic!("{reasons}");
    }
}

/// Run each control family, then churn to saturation and check it. The
/// saturation claims and the aggregate need the whole churn, so a churn
/// failure is their one verdict.
fn drive(builder: &SqsBuilder, queue: &str, control: &PeerControl) -> Observed {
    let mut claims = vec![
        ("control successes", control_successes(builder, queue)),
        (
            "control received errors",
            control_errors(builder, queue, control),
        ),
    ];
    let expected = match churn(builder, queue, control) {
        Ok(expected) => {
            claims.extend(saturation(builder, queue, control));
            Some(expected)
        }
        Err(reason) => {
            claims.push(("churn", Err(reason)));
            None
        }
    };
    Observed { claims, expected }
}

fn closes(what: &str, client: &Client) -> Row {
    bounded(what, client.close()).and_then(|closed| expect_ok(what, closed))
}

/// Successes retire their accounts: more of them than the budget holds are
/// all admitted.
fn control_successes(builder: &SqsBuilder, queue: &str) -> Row {
    let client = settled_within("connect", ROW_BOUND, builder.clone().connect())?;
    for round in 0..CONTROL_ROUNDS {
        expect_ok(
            &format!("control success {round}"),
            bounded("send", client.send_message(queue, "control"))?,
        )?;
    }
    closes("close the success control client", &client)
}

/// Received errors retire their accounts: more of them than the budget holds
/// are all admitted.
fn control_errors(builder: &SqsBuilder, queue: &str, control: &PeerControl) -> Row {
    let client = settled_within("connect", ROW_BOUND, builder.clone().connect())?;
    for round in 0..CONTROL_ROUNDS {
        script_rejection(control);
        expect_refused(
            &format!("control received error {round}"),
            bounded("send", client.send_message(queue, "control"))?,
            rejected(IntegrationOperation::Publish),
        )?;
    }
    closes("close the error control client", &client)
}

/// Churn clients until only the live clients' close reservations are left.
/// The cutting clients go first, so their failures outlive every later
/// instance. Returns what the aggregate must then hold.
fn churn(builder: &SqsBuilder, queue: &str, control: &PeerControl) -> Result<Expected, String> {
    let cutting = builder
        .clone()
        .shutdown_timeout(CUT_AFTER)
        .operation_timeout(HELD_BOUND);
    let mut expected = Expected::default();
    for index in 0..CHURNED {
        let cut = index < CUT_CLOSES;
        let chosen = match cut {
            true => &cutting,
            false => builder,
        };
        let client = settled_within("connect", ROW_BOUND, chosen.clone().connect())?;
        let id = identity(&client, queue, control)?;
        leave_unread(&client, queue, control)?;
        abandon_submitted(&client, queue, control)?;
        expected.retained.extend([
            (id, rejected(IntegrationOperation::Publish)),
            (id, cancelled(IntegrationOperation::Publish)),
        ]);
        match cut {
            true => {
                close_cutting(&client, queue, control)?;
                expected
                    .retained
                    .push((id, cancelled(IntegrationOperation::Publish)));
            }
            false => closes("close a churned client", &client)?,
        }
        expected.churned.push(id);
    }
    Ok(expected)
}

/// The instance `client` was admitted as, read from an error it receives.
fn identity(client: &Client, queue: &str, control: &PeerControl) -> Result<u64, String> {
    script_rejection(control);
    instance_of(
        "the identifying send",
        bounded(
            "an identifying send",
            client.send_message(queue, "identify"),
        )?,
    )
}

/// Leave one fixed failure unread: its account moves into history.
///
/// The peer withholds the rejection until the send has been polled once, so
/// the operation task cannot fix it before the waiter is parked; the gate
/// opens only after that poll.
fn leave_unread(client: &Client, queue: &str, control: &PeerControl) -> Row {
    control.script([Reply::gated_error(400, REJECTION)]);
    runtime::block_on(async {
        let unread = still_pending("the unread send", client.send_message(queue, "unread")).await?;
        control.open_gate();
        admitted_after(client, queue).await?;
        drop(unread);
        Ok(())
    })
}

/// Drop the waiter of a send the peer read and holds: its outcome is unknown,
/// so its account moves into history.
fn abandon_submitted(client: &Client, queue: &str, control: &PeerControl) -> Row {
    runtime::block_on(async {
        let submitted = held(
            "the held send",
            control,
            client.send_message(queue, "abandoned"),
        )
        .await?;
        drop(submitted);
        admitted_after(client, queue).await
    })
}

/// Close with a submitted send still held. The close cuts it and settles
/// `Ok`; settlement waits for the cut send to end, so its failure is already
/// published when the waiter drops it unread.
fn close_cutting(client: &Client, queue: &str, control: &PeerControl) -> Row {
    runtime::block_on(async {
        let submitted = held("the cut send", control, client.send_message(queue, "cut")).await?;
        let closed = tokio::time::timeout(ROW_BOUND, client.close())
            .await
            .map_err(|_| "the cutting close never settled".to_owned())?;
        expect_ok("a close that cut a held send", closed)?;
        drop(submitted);
        Ok(())
    })
}

/// Poll `send` once and keep it: it must wait rather than finish at once.
async fn still_pending<F: Future>(what: &str, send: F) -> Result<Pin<Box<F>>, String> {
    let mut send = Box::pin(send);
    expect_polled_pending(what, &futures_util::poll!(send.as_mut()))?;
    Ok(send)
}

/// Submit `send` against a peer that holds it, and wait until the peer read
/// it.
async fn held<F: Future>(
    what: &str,
    control: &PeerControl,
    send: F,
) -> Result<Pin<Box<F>>, String> {
    control.script([Reply::Hold]);
    let held = control.log().held + 1;
    let send = still_pending(what, send).await?;
    control.wait_for(what, ROW_BOUND, |log| log.held >= held)?;
    Ok(send)
}

/// Wait until the one operation slot admits a readiness query: the operation
/// before it has ended.
async fn admitted_after(client: &Client, queue: &str) -> Row {
    integration_admitted_after(IntegrationOperation::Ready, ROW_BOUND, || {
        client.ready(queue)
    })
    .await
}

/// Two live clients take the last accounts. At 256 charged accounts the next
/// operation and the next connect are `Busy`, the peer reads no request and
/// accepts no connection, and each live client still closes.
fn saturation(builder: &SqsBuilder, queue: &str, control: &PeerControl) -> Vec<Claim> {
    let connect = || settled_within("connect", ROW_BOUND, builder.clone().connect());
    let (live, other) = match connect().and_then(|live| connect().map(|other| (live, other))) {
        Ok(clients) => clients,
        Err(reason) => return vec![("the live clients connect", Err(reason))],
    };
    let before = control.log();
    let mut claims = vec![
        (
            "send at saturation",
            bounded("send", live.send_message(queue, "refused")).and_then(|outcome| {
                expect_refused("send", outcome, busy(IntegrationOperation::Publish))
            }),
        ),
        (
            "receive at saturation",
            bounded("receive", live.receive_messages(queue, 1, Duration::ZERO)).and_then(
                |outcome| expect_refused("receive", outcome, busy(IntegrationOperation::Receive)),
            ),
        ),
        (
            "connect at saturation",
            bounded("connect", builder.clone().connect()).and_then(|outcome| {
                expect_refused("connect", outcome, busy(IntegrationOperation::Connect))
            }),
        ),
    ];
    let after = control.log();
    claims.extend([
        (
            "requests at saturation",
            expect_eq(
                "requests the peer read",
                after.requests.len(),
                before.requests.len(),
            ),
        ),
        (
            "connections at saturation",
            expect_eq(
                "connections the peer accepted",
                after.accepted,
                before.accepted,
            ),
        ),
        ("close at saturation", closes("close", &live)),
        ("close the other live client", closes("close", &other)),
    ]);
    claims
}

/// The claims the returned aggregate and the churned identities make.
fn aggregate_claims(outcome: &Result<(), RuntimeError>, expected: &Expected) -> Vec<Claim> {
    let distinct: BTreeSet<u64> = expected.churned.iter().copied().collect();
    let mut claims = vec![(
        "churned identities",
        expect_eq("distinct churned instances", distinct.len(), CHURNED),
    )];
    let error = match outcome {
        Ok(()) => {
            claims.push((
                "aggregate",
                Err("the retained failures left no aggregate".to_owned()),
            ));
            return claims;
        }
        Err(error) => error,
    };
    let mut actual = match integration_aggregate(error, IntegrationKind::Sqs) {
        Ok(actual) => actual,
        Err(reason) => {
            claims.push(("aggregate", Err(reason)));
            return claims;
        }
    };
    let mut wanted = expected.retained.clone();
    // The refusal enums carry no order, so their rendering is the tiebreak;
    // cached, it is rendered once per account rather than once per comparison.
    actual.sort_by_cached_key(|(id, refused)| (*id, format!("{refused:?}")));
    wanted.sort_by_cached_key(|(id, refused)| (*id, format!("{refused:?}")));
    claims.extend([
        (
            "aggregate size",
            expect_eq("retained SQS accounts", actual.len(), REPORT_BUDGET - LIVE),
        ),
        (
            "aggregate by instance",
            expect_eq("retained SQS accounts, by instance", actual, wanted),
        ),
        (
            "aggregate identities",
            expect_own_instances(error, IntegrationKind::Sqs),
        ),
    ]);
    claims
}
