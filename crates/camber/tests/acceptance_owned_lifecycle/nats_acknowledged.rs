//! 3.T1–3.T4: acknowledged correlation state and its reply receiver follow
//! the connection's existing owner. 4.T3: abandoned acknowledged results
//! fill the existing report budget.
//!
//! Every row enters through the public `camber::mq::nats` API inside a real
//! runtime against the scripted peer. `NatsAckProbe` reads the real
//! correlation map: its live entries, its allocation, and whether the reply
//! receiver still routes. It inserts, answers, and settles nothing.
//! `NatsQueueProbe` and `NatsPublishProbe` hold scheduling only; production
//! sets the submission mark and derives every result.
//!
//! An exact outcome follows a peer record, an owner-committed result, or a
//! scheduling checkpoint. Where two owners race without an order Camber
//! promises, a row accepts each contract-valid outcome and names the causal
//! path that yields it. No sleep sets precedence.
//!
//! Each row owns its runtime and its peer and finishes the peer on every exit.
#![cfg(feature = "nats")]

use crate::integration_rows::{
    CONTROL_ROUNDS, EXHAUSTED, INVALID_NATS_SUBJECT, NamedRow, REPORT_BUDGET, ROW_BOUND, Refusal,
    Row, busy, cancelled, clean_run, closed, expect, expect_eq, expect_ok, expect_own_instances,
    expect_polled_pending, expect_refused, integration_admitted_after, integration_aggregate,
    nats_instance, observed_verdict, on_tokio, permission_denied, refused, rejected, row_bounded,
    run_observing, settled, timed_out, unknown,
};
use crate::nats_ack_rows::{
    FORCED_GRACE, HeldPublish, STREAM, SUBJECT, Teardown, ack, ack_probe, acknowledge,
    acknowledged, answered, answered_twice, cut_transport, disconnect_after_submission,
    expect_ended, expect_forced_accounts, expect_reached_hold, expect_retired, expect_routing,
    expect_withheld, finished_within, jetstream_error, late_reply_is_discarded, on_connection,
    private_subscriptions, publication, reply_to, slot_reused, stranger_reply, wait_ready,
    wait_resubscribed, wait_snapshot,
};
use crate::nats_peer::wire::Reply;
use crate::nats_peer::{NatsPeer, PeerControl, PeerLog, Script, wait_unavailable};
use camber::mq::nats::{Connection, NatsBuilder};
use camber::runtime_test_support::{
    NatsAckProbe, NatsAckReceiver, NatsPublishProbe, NatsQueueProbe,
};
use camber::{
    IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability, RuntimeError, runtime,
};
use std::future::Future;
use std::pin::{Pin, pin};
use std::time::{Duration, Instant};

const PUBLISH: IntegrationOperation = IntegrationOperation::Publish;

/// A publish that reached the SDK and then outlived its expiry.
const SUBMITTED_TIMEOUT: Refusal = (
    PUBLISH,
    IntegrationFailure::Timeout,
    Retryability::OutcomeUnknown,
);

/// Sequential submitted timeouts: more than the runtime's report accounts,
/// so an entry or an account each one kept would show.
const SEQUENTIAL_TIMEOUTS: usize = REPORT_BUDGET + 1;

/// The operation expiry of the sequential timeout row: each held publish
/// costs all of it, so it stays short.
const SHORT_EXPIRY: Duration = Duration::from_millis(50);

/// The one outer guard over every sequential timeout.
const SEQUENTIAL_GUARD: Duration = Duration::from_secs(120);

/// Publishes the sequential row may need before one beats [`SHORT_EXPIRY`]
/// to its receipt.
const SUCCESS_ATTEMPTS: usize = 10;

/// The expiry of a row that times one publish out and then reuses the slot:
/// wide enough that the answered publish after it never races it.
const TIMEOUT_ROW_EXPIRY: Duration = Duration::from_secs(1);

/// Wait until the operation's own task retired every entry.
fn wait_retired(what: &str, probe: &NatsAckProbe) -> Row {
    wait_snapshot(what, probe, |snapshot| snapshot.pending == 0).map(drop)
}

/// Wait until the connection refuses readiness as `Closed`: its entry
/// committed closing.
fn wait_closing(connection: &Connection) -> Row {
    row_bounded("the entry's closing commit", async {
        while refused(connection.ready()) != Some(closed(IntegrationOperation::Ready)) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
}

// ── 3.T1 delivered timeouts ──────────────────────────────────────────

#[test]
fn acknowledged_delivered_timeouts_retire_reply_state() {
    crate::integration_rows::run_rows(&[(
        "more submitted timeouts than report accounts retain no reply state",
        delivered_timeouts_retain_no_reply_state,
    )]);
}

/// More than 256 sequential publishes, each admitted by the SDK, held after
/// its submission mark until its short expiry passes, and delivered to its
/// caller as `Timeout/OutcomeUnknown`.
///
/// The queue probe's admission count proves each was submitted. It stays
/// readable after the expiry, so neither the peer nor the row must beat it.
/// The peer then reads each publication under the normal guard before the
/// next starts. After each settlement the real map holds no entry and its
/// allocation never grows past the first one's. Released, the connection
/// still completes an acknowledged publish.
fn delivered_timeouts_retain_no_reply_state() -> Row {
    on_connection(
        |builder| builder.operation_timeout(SHORT_EXPIRY),
        Teardown::Clean,
        |connection, control| {
            let probe = ack_probe(connection)?;
            let mut queue =
                NatsQueueProbe::hold(connection).ok_or("the queue probe did not attach")?;
            let timed_out = submitted_timeouts(connection, control, &probe, &mut queue);
            queue.release();
            timed_out?;
            acknowledged_within_short_expiry(connection, control, SEQUENTIAL_TIMEOUTS)
        },
    )
}

/// Every sequential submitted timeout, each settled and retired before the
/// next starts, all inside one outer guard.
fn submitted_timeouts(
    connection: &Connection,
    control: &PeerControl,
    probe: &NatsAckProbe,
    queue: &mut NatsQueueProbe,
) -> Row {
    let guard = Instant::now() + SEQUENTIAL_GUARD;
    let mut first_allocation = None;
    for round in 0..SEQUENTIAL_TIMEOUTS {
        expect(
            &format!("timeout {round} began after the {SEQUENTIAL_GUARD:?} guard"),
            Instant::now() < guard,
        )?;
        expect_refused(
            &format!("held publish {round}"),
            row_bounded("a held publish", connection.publish(SUBJECT, b"held"))?,
            SUBMITTED_TIMEOUT,
        )?;
        let admitted = row_bounded("the SDK admission", queue.polled(round + 1))?;
        expect_eq(
            &format!("SDK admissions after timeout {round}"),
            admitted,
            Some(round + 1),
        )?;
        let snapshot = probe.snapshot();
        expect_routing(snapshot, 0)?;
        let first = *first_allocation.get_or_insert(snapshot.capacity);
        expect(
            &format!(
                "the map's allocation grew to {} after timeout {round}, past {first}",
                snapshot.capacity
            ),
            snapshot.capacity <= first,
        )?;
        publication(control, round)?;
    }
    Ok(())
}

/// Publish from publication `first` on until one succeeds on its receipt.
///
/// An attempt whose receipt misses the short expiry is the contract's own
/// `Timeout/OutcomeUnknown`, never success, so the next attempt takes a
/// fresh token. Any other answer fails the row.
fn acknowledged_within_short_expiry(
    connection: &Connection,
    control: &PeerControl,
    first: usize,
) -> Row {
    for index in first..first + SUCCESS_ATTEMPTS {
        let outcome = runtime::block_on(async {
            let mut publish = pin!(connection.publish(SUBJECT, b"after"));
            expect_polled_pending(
                "the publish after the release",
                &futures_util::poll!(publish.as_mut()),
            )?;
            acknowledge(control, &publication(control, index)?, 1)?;
            finished_within("the publish after the release", publish).await
        })?;
        match refused(outcome) {
            None => return expect_retired(connection),
            Some(SUBMITTED_TIMEOUT) => {}
            Some(other) => return Err(format!("the publish after the release: {other:?}")),
        }
    }
    Err(format!(
        "none of {SUCCESS_ATTEMPTS} publishes met its receipt inside {SHORT_EXPIRY:?}"
    ))
}

// ── 3.T2 cancellation and late replies ───────────────────────────────

#[test]
fn acknowledged_cancellation_and_late_replies_retire_state() {
    crate::integration_rows::run_rows(CANCELLATION_AND_LATE_REPLIES);
}

/// Every retirement row, by the exit it proves.
const CANCELLATION_AND_LATE_REPLIES: &[NamedRow<'static>] = &[
    (
        "a cancellation before submission retires its entry",
        cancellation_before_submission_retires,
    ),
    (
        "a cancellation after recorded receipt retires its entry",
        cancellation_after_receipt_retires,
    ),
    (
        "a caller dropped while awaiting its receipt retires its entry",
        caller_drop_while_awaiting_receipt_retires,
    ),
    ("a send refusal retires its entry", send_refusal_retires),
    (
        "a timeout retires its entry and discards its late reply",
        timeout_retires_and_discards_its_late_reply,
    ),
    (
        "a disconnect retires its entry and discards its late reply",
        disconnect_retires_and_discards_its_late_reply,
    ),
    (
        "an unknown token leaves the live entry alone",
        unknown_token_leaves_the_live_entry,
    ),
    (
        "a duplicate reply settles nothing twice",
        duplicate_reply_settles_once,
    ),
];

/// One operation slot: a reused slot proves the retired operation freed it.
fn one_slot(builder: NatsBuilder) -> NatsBuilder {
    builder.max_in_flight(1)
}

/// A publish dropped while held before the SDK queue retires its entry,
/// sends nothing, and releases its account and slot.
fn cancellation_before_submission_retires() -> Row {
    on_connection(one_slot, Teardown::Clean, |connection, control| {
        let probe = ack_probe(connection)?;
        let mut hold =
            NatsPublishProbe::hold(connection).ok_or("the publish probe did not attach")?;
        let dropped: Row = runtime::block_on(async {
            let mut publish = Box::pin(connection.publish(SUBJECT, b"dropped"));
            expect_polled_pending("the held publish", &futures_util::poll!(publish.as_mut()))?;
            expect_reached_hold(&mut hold).await?;
            expect_routing(probe.snapshot(), 1)?;
            drop(publish);
            Ok(())
        });
        // The work is gone once its entry is: the release reaches no publish.
        let retired = dropped.and_then(|()| wait_retired("the dropped entry", &probe));
        hold.release();
        retired?;
        control.delivery_barrier(ROW_BOUND)?;
        expect_eq("publications", control.log().publications.len(), 0)?;
        slot_reused(connection, control, 0)
    })
}

/// A publish dropped while held after the SDK admitted it and the peer read
/// it retires its entry; its late receipt is discarded, and the runtime
/// keeps its unknown outcome.
fn cancellation_after_receipt_retires() -> Row {
    on_connection(one_slot, Teardown::Cancelled, |connection, control| {
        let probe = ack_probe(connection)?;
        let mut queue = NatsQueueProbe::hold(connection).ok_or("the queue probe did not attach")?;
        let dropped = runtime::block_on(async {
            let mut publish = Box::pin(connection.publish(SUBJECT, b"dropped"));
            expect_polled_pending("the held publish", &futures_util::poll!(publish.as_mut()))?;
            let admitted = finished_within("the SDK admission", queue.polled(1)).await?;
            expect_eq("SDK admissions", admitted, Some(1))?;
            let read = publication(control, 0)?;
            expect_routing(probe.snapshot(), 1)?;
            drop(publish);
            Ok::<_, String>(read)
        });
        let retired = dropped.and_then(|read| {
            wait_retired("the cancelled entry", &probe)?;
            Ok(read)
        });
        queue.release();
        let read = retired?;
        late_reply_is_discarded(connection, control, &read)?;
        slot_reused(connection, control, 1)
    })
}

/// A publish whose caller drops it while it awaits its withheld receipt
/// retires its entry; its late receipt is discarded, and the runtime keeps
/// its unknown outcome.
fn caller_drop_while_awaiting_receipt_retires() -> Row {
    on_connection(one_slot, Teardown::Cancelled, |connection, control| {
        let probe = ack_probe(connection)?;
        let read = runtime::block_on(async {
            let mut publish = Box::pin(connection.publish(SUBJECT, b"dropped"));
            let read = expect_withheld("the publish", control, 0, publish.as_mut()).await?;
            expect_routing(probe.snapshot(), 1)?;
            drop(publish);
            Ok::<_, String>(read)
        })?;
        wait_retired("the dropped entry", &probe)?;
        late_reply_is_discarded(connection, control, &read)?;
        slot_reused(connection, control, 1)
    })
}

/// A publish the SDK refuses after registration settles as `Rejected`,
/// sends nothing, and retires its entry.
fn send_refusal_retires() -> Row {
    on_connection(one_slot, Teardown::Clean, |connection, control| {
        expect_refused(
            "a subject the SDK refuses",
            row_bounded("publish", connection.publish(INVALID_NATS_SUBJECT, b"x"))?,
            rejected(PUBLISH),
        )?;
        expect_retired(connection)?;
        control.delivery_barrier(ROW_BOUND)?;
        expect_eq("publications", control.log().publications.len(), 0)?;
        slot_reused(connection, control, 0)
    })
}

/// A withheld receipt expires as `Timeout/OutcomeUnknown` and retires the
/// entry; the receipt that comes after is discarded.
fn timeout_retires_and_discards_its_late_reply() -> Row {
    on_connection(
        |builder| one_slot(builder).operation_timeout(TIMEOUT_ROW_EXPIRY),
        Teardown::Clean,
        |connection, control| {
            expect_refused(
                "the unanswered publish",
                row_bounded("publish", connection.publish(SUBJECT, b"late"))?,
                SUBMITTED_TIMEOUT,
            )?;
            let read = publication(control, 0)?;
            expect_retired(connection)?;
            late_reply_is_discarded(connection, control, &read)?;
            slot_reused(connection, control, 1)
        },
    )
}

/// A transport lost after the peer read the publication settles it as
/// unknown and retires its entry; its receipt on the next transport is
/// discarded.
fn disconnect_retires_and_discards_its_late_reply() -> Row {
    on_connection(one_slot, Teardown::Clean, |connection, control| {
        let (read, outcome) =
            runtime::block_on(disconnect_after_submission(connection, control, 0))?;
        expect_refused(
            "the publish across the disconnect",
            outcome,
            unknown(PUBLISH),
        )?;
        expect_retired(connection)?;
        wait_ready(connection)?;
        wait_resubscribed(control)?;
        late_reply_is_discarded(connection, control, &read)?;
        slot_reused(connection, control, 1)
    })
}

/// A well-formed receipt for a token no publish holds leaves the one live
/// entry in place; the live publish settles on its own receipt.
fn unknown_token_leaves_the_live_entry() -> Row {
    on_connection(one_slot, Teardown::Clean, |connection, control| {
        let probe = ack_probe(connection)?;
        let verdict: Row = runtime::block_on(async {
            let mut publish = pin!(connection.publish(SUBJECT, b"live"));
            let read = expect_withheld("the publish", control, 0, publish.as_mut()).await?;
            control.reply(
                &stranger_reply(&read)?,
                &Reply::Message(ack(STREAM, 1).as_bytes()),
            )?;
            control.delivery_barrier(ROW_BOUND)?;
            expect_routing(probe.snapshot(), 1)?;
            expect_polled_pending(
                "the publish after a stranger's receipt",
                &futures_util::poll!(publish.as_mut()),
            )?;
            acknowledge(control, &read, 2)?;
            expect_ok(
                "the live publish",
                finished_within("the publish", publish).await?,
            )
        });
        verdict?;
        expect_retired(connection)?;
        slot_reused(connection, control, 1)
    })
}

/// A receipt followed by a refusal for the same token settles the publish
/// once, as success; the refusal finds no entry.
fn duplicate_reply_settles_once() -> Row {
    on_connection(one_slot, Teardown::Clean, |connection, control| {
        runtime::block_on(answered_twice(connection, control))?;
        expect_retired(connection)?;
        slot_reused(connection, control, 1)
    })
}

// ── 3.T3 receiver and close owner ────────────────────────────────────

#[test]
fn acknowledged_receiver_ends_with_close_owner() {
    crate::integration_rows::run_rows(RECEIVER_AND_CLOSE_OWNER);
}

/// Every close-owner row, by the path that ends the receiver.
const RECEIVER_AND_CLOSE_OWNER: &[NamedRow<'static>] = &[
    (
        "an explicit close ends the receiver",
        explicit_close_ends_the_receiver,
    ),
    (
        "concurrent closers wait while the receiver drains admitted work",
        concurrent_close_drains_admitted_work,
    ),
    (
        "the last handle's drop ends the receiver",
        last_handle_drop_ends_the_receiver,
    ),
    (
        "an escaped clone reads the ended receiver",
        escaped_clone_reads_the_ended_receiver,
    ),
    (
        "a graceful runtime stop drains admitted work",
        graceful_stop_drains_admitted_work,
    ),
    (
        "the aggregate deadline leaves a submitted publish unknown",
        aggregate_deadline_leaves_a_submitted_publish_unknown,
    ),
];

/// Close settles only after the receiver ended and released its state;
/// every later publish is `Closed`, and the client ends its transport.
fn explicit_close_ends_the_receiver() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let probe = ack_probe(&connection)?;
        let published = runtime::block_on(answered(
            &connection,
            &control,
            0,
            Reply::Message(ack(STREAM, 1).as_bytes()),
        ))?;
        expect_ok("the publish before close", published)?;
        expect_routing(probe.snapshot(), 0)?;
        expect_ok("close", row_bounded("close", connection.close())?)?;
        expect_ended(&probe)?;
        expect_refused(
            "a publish after close",
            row_bounded("publish", connection.publish(SUBJECT, b"late"))?,
            closed(PUBLISH),
        )?;
        control.wait_for("the client's close", ROW_BOUND, |log| log.closed >= 1)?;
        expect_eq("publications", control.log().publications.len(), 1)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// Close requested by two clones while a publish awaits its receipt
/// commits at once: new work is `Closed`, yet the receiver still routes. The
/// receipt settles the admitted publish, and only then do both closers read
/// the one result, with the receiver ended.
///
/// A receiver that stopped before admitted work drained would leave the
/// publish unknown.
fn concurrent_close_drains_admitted_work() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let other = connection.clone();
        let probe = ack_probe(&connection)?;
        let verdict: Row = runtime::block_on(async {
            let mut publish = pin!(connection.publish(SUBJECT, b"draining"));
            let read = expect_withheld("the publish", &control, 0, publish.as_mut()).await?;
            let mut closing = pin!(connection.close());
            let mut concurrent = pin!(other.close());
            expect_polled_pending("close", &futures_util::poll!(closing.as_mut()))?;
            expect_polled_pending(
                "the concurrent close",
                &futures_util::poll!(concurrent.as_mut()),
            )?;
            expect_refused(
                "readiness while closing",
                connection.ready(),
                closed(IntegrationOperation::Ready),
            )?;
            expect_refused(
                "a publish while closing",
                connection.publish(SUBJECT, b"refused").await,
                closed(PUBLISH),
            )?;
            expect_routing(probe.snapshot(), 1)?;
            acknowledge(&control, &read, 1)?;
            expect_ok(
                "the draining publish",
                finished_within("the draining publish", publish).await?,
            )?;
            expect_ok("close", finished_within("close", closing).await?)?;
            expect_ok(
                "the concurrent close",
                finished_within("the concurrent close", concurrent).await?,
            )
        });
        verdict?;
        expect_ended(&probe)?;
        expect_eq("publications", control.log().publications.len(), 1)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// Dropping one of two handles leaves the receiver routing; dropping the
/// last ends it while the runtime runs. A receiver that held access would
/// keep the connection open, and this wait would never end.
fn last_handle_drop_ends_the_receiver() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let probe = ack_probe(&connection)?;
        let last = connection.clone();
        drop(connection);
        expect_routing(probe.snapshot(), 0)?;
        drop(last);
        wait_snapshot("the receiver's end", &probe, |snapshot| {
            snapshot.receiver == NatsAckReceiver::Ended
        })?;
        expect_ended(&probe)?;
        control.wait_for("the client's close", ROW_BOUND, |log| log.closed >= 1)?;
        expect("the runtime was stopping", !runtime::is_shutting_down())
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// A clone that escapes the runtime's closure keeps no receiver alive:
/// the root's return closes the connection, the receiver ends, and the
/// escaped clone's publish is `Closed` with nothing sent.
fn escaped_clone_reads_the_ended_receiver() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let outcome = runtime::builder().run(move || -> Result<(Connection, NatsAckProbe), String> {
        let connection = settled("connect", builder.connect())?;
        let probe = ack_probe(&connection)?;
        expect_routing(probe.snapshot(), 0)?;
        Ok((connection.clone(), probe))
    });
    let verdict = clean_run(outcome).and_then(|(escaped, probe)| {
        expect_ended(&probe)?;
        expect_refused(
            "a publish through the escaped clone",
            on_tokio(escaped.publish(SUBJECT, b"late"))?,
            closed(PUBLISH),
        )?;
        drop(escaped);
        let log = peer
            .control()
            .wait_for("the client's close", ROW_BOUND, |log| log.closed >= 1)?;
        expect_eq("publications", log.publications.len(), 0)
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A graceful stop with a publish awaiting its receipt commits closing at
/// once, but the receiver routes until that admitted publish settles on its
/// receipt. Only then does the receiver end, before the runtime returns.
fn graceful_stop_drains_admitted_work() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let probe = ack_probe(&connection)?;
        let mut publish = Box::pin(connection.publish(SUBJECT, b"draining"));
        let read = runtime::block_on(expect_withheld(
            "the publish",
            &control,
            0,
            publish.as_mut(),
        ))?;
        runtime::request_shutdown();
        wait_closing(&connection)?;
        expect_refused(
            "a publish after the stop",
            row_bounded("publish", connection.publish(SUBJECT, b"refused"))?,
            closed(PUBLISH),
        )?;
        expect_routing(probe.snapshot(), 1)?;
        acknowledge(&control, &read, 1)?;
        expect_ok(
            "the publish the stop drained",
            row_bounded("the draining publish", publish)?,
        )?;
        wait_snapshot("the receiver's end", &probe, |snapshot| {
            snapshot.receiver == NatsAckReceiver::Ended
        })?;
        expect_ended(&probe)?;
        expect_eq("publications", control.log().publications.len(), 1)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// A publish the peer read, still awaiting its receipt when the root returns,
/// outlives the stop's one aggregate grace: the forced stop drops both the
/// operation and the close owner that runs the receiver.
///
/// The two drops have no order Camber promises. When the operation's task
/// goes first, its waiter reads the stop's `Cancelled/OutcomeUnknown`. When
/// the close owner goes first, dropping the receiver ends routing, and an
/// operation polled before its own drop commits `OutcomeUnknown`. Both keep
/// the retry unknown; neither is success or a safe retry. The waiter reads
/// one of them, and the runtime retains at most that one publish account.
fn aggregate_deadline_leaves_a_submitted_publish_unknown() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url()).operation_timeout(ROW_BOUND);
    let control = peer.control();
    let outcome = run_observing(
        runtime::builder().shutdown_timeout(FORCED_GRACE),
        move || -> Result<(HeldPublish, NatsAckProbe), String> {
            let connection = settled("connect", builder.connect())?;
            let probe = ack_probe(&connection)?;
            let mut publish: HeldPublish =
                Box::pin(async move { connection.publish(SUBJECT, b"held").await });
            runtime::block_on(expect_withheld(
                "the publish",
                &control,
                0,
                publish.as_mut(),
            ))?;
            expect_routing(probe.snapshot(), 1)?;
            Ok((publish, probe))
        },
    );
    let (observed, teardown) = outcome;
    let verdict = observed_verdict(observed).and_then(|(publish, probe)| {
        let answer = on_tokio(async { tokio::time::timeout(ROW_BOUND, publish).await })?
            .map_err(|_| "the forced publish never settled".to_owned())?;
        let read = refused(answer);
        let permitted = [Some(cancelled(PUBLISH)), Some(unknown(PUBLISH))];
        expect(
            &format!("the forced publish read {read:?}"),
            permitted.contains(&read),
        )?;
        expect_ended(&probe)?;
        expect_eq("publications", peer.control().log().publications.len(), 1)?;
        expect_forced_accounts(&teardown, read)
    });
    peer.finished(ROW_BOUND, verdict)
}

// ── 3.T3 failed setup ────────────────────────────────────────────────

#[test]
fn acknowledged_failed_setup_releases_owned_state() {
    crate::integration_rows::run_rows(FAILED_SETUP);
}

/// Every failed-setup row, by where setup stopped.
///
/// The pinned SDK refuses the private subscription only once its command
/// queue closed, and its readiness flush is a local socket flush, so no
/// public input fails setup between the subscription and readiness. A stop
/// reaches that window instead: the subscription is installed, and the
/// closed scope refuses the close owner.
const FAILED_SETUP: &[NamedRow<'static>] = &[
    (
        "a connect cancelled during the handshake releases its transport",
        connect_cancelled_during_the_handshake,
    ),
    (
        "a refused handshake settles before any subscription",
        refused_handshake_settles_before_any_subscription,
    ),
    (
        "a stop before the close owner attaches releases the installed subscription",
        stop_before_the_close_owner_releases_the_subscription,
    ),
];

/// Drive `connecting` until the peer's log reaches `reached`, under the hang
/// guard; the connect settling first fails the row.
async fn drive_until<F>(
    control: &PeerControl,
    what: &str,
    connecting: Pin<&mut F>,
    reached: impl Fn(&PeerLog) -> bool,
) -> Row
where
    F: Future<Output = Result<Connection, RuntimeError>> + ?Sized,
{
    let observed = async {
        while !reached(&control.log()) {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    };
    finished_within(what, async {
        tokio::select! {
            biased;
            () = observed => Ok(()),
            answer = connecting => Err(format!(
                "the connect settled before {what}: {:?}",
                answer.map(drop)
            )),
        }
    })
    .await?
}

/// Fail the row unless the client ended its transport after the peer read
/// `subscriptions` private subscriptions and no publication.
fn expect_setup_released(control: &PeerControl, subscriptions: usize) -> Row {
    let log = control.wait_for("the client's close", ROW_BOUND, |log| log.closed >= 1)?;
    expect_eq(
        "private subscriptions",
        private_subscriptions(&log),
        subscriptions,
    )?;
    expect_eq("subscriptions", log.subscribed.len(), subscriptions)?;
    expect_eq("publications", log.publications.len(), 0)
}

/// A connect dropped while the peer withholds its handshake drops the SDK
/// transport with it: once the peer answers, it reads the client's end,
/// never a `CONNECT`. The runtime keeps nothing.
fn connect_cancelled_during_the_handshake() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url()).connect_timeout(ROW_BOUND);
    let control = peer.control();
    control.script(Script::Silent);
    let outcome = runtime::builder().run(move || -> Row {
        let cancelled: Row = runtime::block_on(async {
            let mut connecting = Box::pin(builder.connect());
            drive_until(
                &control,
                "the held connection",
                connecting.as_mut(),
                |log| log.accepted >= 1,
            )
            .await?;
            drop(connecting);
            Ok(())
        });
        control.script(Script::Serve);
        cancelled?;
        expect_setup_released(&control, 0)?;
        expect_eq("handshakes", control.log().connects, 0)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// A handshake the server denies settles the connect as `PermissionDenied`
/// before the private subscription is installed.
fn refused_handshake_settles_before_any_subscription() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let control = peer.control();
    control.script(Script::DenyAuthorization);
    let outcome = runtime::builder().run(move || -> Row {
        expect_refused(
            "a denied acknowledged connect",
            row_bounded("connect", builder.connect())?,
            permission_denied(IntegrationOperation::Connect),
        )?;
        let log = control.log();
        expect_eq("handshakes", log.connects, 1)?;
        expect_eq("subscriptions", log.subscribed.len(), 0)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// A stop requested while the handshake waits closes the root scope. The
/// handshake then completes and installs the private subscription, but the
/// connect is refused before its close owner attaches: as `Closed` when the
/// entry's closing commit came first, or as the closed scope's own refusal
/// of the owner. Either way the receiver and the client drop with the
/// refusal, and the peer reads the client's end.
fn stop_before_the_close_owner_releases_the_subscription() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url()).connect_timeout(ROW_BOUND);
    let control = peer.control();
    control.script(Script::Silent);
    let outcome = runtime::builder().run(move || -> Row {
        let refused_connect: Row = runtime::block_on(async {
            let mut connecting = Box::pin(builder.connect());
            drive_until(
                &control,
                "the held connection",
                connecting.as_mut(),
                |log| log.accepted >= 1,
            )
            .await?;
            runtime::request_shutdown();
            control.script(Script::Serve);
            match finished_within("the connect", connecting).await? {
                Err(RuntimeError::ScopeClosed) => Ok(()),
                other => expect_refused(
                    "a connect after the stop",
                    other.map(drop),
                    closed(IntegrationOperation::Connect),
                ),
            }
        });
        control.script(Script::Serve);
        refused_connect?;
        expect_setup_released(&control, 1)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

// ── 3.T4 reconnect and committed results ─────────────────────────────

#[test]
fn acknowledged_reconnect_never_reissues() {
    crate::integration_rows::run_rows(&[(
        "a receipt on the next transport answers no submitted publish",
        receipt_on_the_next_transport_answers_nothing,
    )]);
}

/// A publish the peer read loses its transport. The SDK reconnects and
/// resubscribes the private inbox, and the peer's receipt for the old token
/// arrives on the new transport. The publish is `OutcomeUnknown` either
/// way: the disconnect it watches settles it, or the transport recheck
/// refuses a receipt read on another generation. The peer reads it once;
/// nothing republishes it.
fn receipt_on_the_next_transport_answers_nothing() -> Row {
    on_connection(
        |builder| builder,
        Teardown::Clean,
        |connection, control| {
            let verdict: Row = runtime::block_on(async {
                let mut publish = pin!(connection.publish(SUBJECT, b"once"));
                let read = expect_withheld("the publish", control, 0, publish.as_mut()).await?;
                cut_transport(control)?;
                wait_resubscribed(control)?;
                acknowledge(control, &read, 1)?;
                expect_refused(
                    "the publish across the reconnect",
                    finished_within("the publish", publish).await?,
                    unknown(PUBLISH),
                )
            });
            verdict?;
            control.delivery_barrier(ROW_BOUND)?;
            expect_retired(connection)?;
            expect_eq(
                "publications after the reconnect",
                control.log().publications.len(),
                1,
            )?;
            wait_ready(connection)?;
            slot_reused(connection, control, 1)?;
            expect_eq("publications", control.log().publications.len(), 2)
        },
    )
}

#[test]
fn acknowledged_committed_success_is_stable() {
    crate::integration_rows::run_rows(COMMITTED_SUCCESS);
}

/// Every commitment row, by what follows the commit.
const COMMITTED_SUCCESS: &[NamedRow<'static>] = &[
    (
        "a committed success survives a reconnect",
        committed_success_survives_a_reconnect,
    ),
    (
        "a committed success survives a runtime stop",
        committed_success_survives_a_stop,
    ),
    (
        "a receipt and a cancellation released together settle once",
        receipt_and_cancellation_settle_once,
    ),
];

/// Answer the publish the peer read first, then wait until its result is
/// fixed, unread: the connection's one slot frees only then, so the
/// subscribe that follows is admitted only after the commit.
async fn commit_unread(
    connection: &Connection,
    control: &PeerControl,
    mut publish: Pin<&mut impl Future<Output = Result<(), RuntimeError>>>,
) -> Row {
    let read = expect_withheld("the publish", control, 0, publish.as_mut()).await?;
    acknowledge(control, &read, 1)?;
    integration_admitted_after(IntegrationOperation::Subscribe, ROW_BOUND, || {
        connection.subscribe("after-commit")
    })
    .await
}

/// A success committed before the transport is lost stays a success when
/// its caller reads it after the reconnect.
fn committed_success_survives_a_reconnect() -> Row {
    on_connection(one_slot, Teardown::Clean, |connection, control| {
        let mut publish = Box::pin(connection.publish(SUBJECT, b"committed"));
        runtime::block_on(commit_unread(connection, control, publish.as_mut()))?;
        cut_transport(control)?;
        wait_ready(connection)?;
        expect_ok(
            "the success read after the reconnect",
            row_bounded("the committed publish", publish)?,
        )?;
        expect_retired(connection)?;
        expect_eq("publications", control.log().publications.len(), 1)
    })
}

/// A success committed before a runtime stop stays a success when its
/// caller reads it after the stop.
fn committed_success_survives_a_stop() -> Row {
    on_connection(one_slot, Teardown::Clean, |connection, control| {
        let mut publish = Box::pin(connection.publish(SUBJECT, b"committed"));
        runtime::block_on(commit_unread(connection, control, publish.as_mut()))?;
        runtime::request_shutdown();
        wait_closing(connection)?;
        expect_ok(
            "the success read after the stop",
            row_bounded("the committed publish", publish)?,
        )?;
        expect_eq("publications", control.log().publications.len(), 1)
    })
}

/// A receipt written and the waiter dropped back to back race without an
/// order Camber promises. The receipt may commit success first, which the
/// drop then leaves unread and releases; or the drop may cancel the
/// submitted publish first, which the runtime keeps as
/// `Cancelled/OutcomeUnknown`. Either way the entry retires once, the peer
/// reads one publication, and the slot works again.
fn receipt_and_cancellation_settle_once() -> Row {
    on_connection(
        one_slot,
        Teardown::CleanOrCancelled,
        |connection, control| {
            let probe = ack_probe(connection)?;
            let mut publish = Box::pin(connection.publish(SUBJECT, b"raced"));
            let read =
                runtime::block_on(expect_withheld("the publish", control, 0, publish.as_mut()))?;
            acknowledge(control, &read, 1)?;
            drop(publish);
            wait_retired("the raced entry", &probe)?;
            expect_routing(probe.snapshot(), 0)?;
            slot_reused(connection, control, 1)?;
            expect_eq("publications", control.log().publications.len(), 2)
        },
    )
}

// ── 4.T3 abandoned results and the report budget ─────────────────────

/// Abandoned refusals: every account the live instance's close reservation
/// and the churned instance leave. Each abandon needs one more account free
/// to prove its result fixed, so the churned instance takes the last.
const ABANDONED: usize = REPORT_BUDGET - 2;

#[test]
fn acknowledged_abandoned_results_exhaust_report_budget() {
    crate::integration_rows::run_rows(&[(
        "abandoned acknowledged refusals fill the report budget once",
        abandoned_refusals_exhaust_report_budget,
    )]);
}

/// Controls first prove that receipts and delivered refusals retire their
/// accounts. Then refusals the receipt path settles, each dropped unread,
/// and one acknowledged instance whose close cannot complete fill every
/// account the live instance's close reservation leaves. At saturation the
/// next publish, subscribe, and connect are `Busy` with no SDK effect; the
/// live instance still closes on its reserved account; and the one returned
/// aggregate names every retained account under the instance that admitted
/// it.
fn abandoned_refusals_exhaust_report_budget() -> Row {
    let steady = NatsPeer::start();
    let churn = NatsPeer::start();
    let builder = one_slot(acknowledged(&steady.url()));
    let churn_url = churn.url();
    let control = steady.control();
    let churn_control = churn.control();
    let (driven, teardown) = run_observing(runtime::builder(), move || {
        let connection = settled("connect", builder.connect())?;
        let steady_id = nats_instance(&connection)?;
        controls_retire_their_accounts(&connection, &control)?;
        for round in 0..ABANDONED {
            abandon_one_refusal(&connection, &control, 2 * CONTROL_ROUNDS + round)?;
        }
        let churned_id = churn_failed_close(&churn_url, &churn_control)?;
        saturation_refuses_without_effects(&connection, &control, 2 * CONTROL_ROUNDS + ABANDONED)?;
        expect_ok(
            "close at saturation",
            row_bounded("close", connection.close())?,
        )?;
        Ok::<_, String>((steady_id, churned_id))
    });
    let verdict = observed_verdict(driven).and_then(|ids| expect_retained_accounts(&teardown, ids));
    let verdict = steady.finished(ROW_BOUND, verdict);
    churn.finished(ROW_BOUND, verdict)
}

/// Connect one acknowledged instance whose close cannot complete: its
/// receiver ends, then its failed-close account outlives the instance and
/// takes the budget's last account.
fn churn_failed_close(url: &str, control: &PeerControl) -> Result<u64, String> {
    let connection = settled(
        "connect",
        acknowledged(url).shutdown_timeout(EXHAUSTED).connect(),
    )?;
    let probe = ack_probe(&connection)?;
    let id = nats_instance(&connection)?;
    control.script(Script::Refuse);
    wait_unavailable(&connection, ROW_BOUND)?;
    expect_refused(
        "a close the SDK can never acknowledge",
        row_bounded("close", connection.close())?,
        timed_out(IntegrationOperation::Close),
    )?;
    expect_ended(&probe)?;
    Ok(id)
}

/// More receipts and more delivered refusals than the whole budget, one at
/// a time: each account retires, so admission never saturates.
fn controls_retire_their_accounts(connection: &Connection, control: &PeerControl) -> Row {
    let receipt = ack(STREAM, 1);
    let refusal_body = jetstream_error(400, 10060);
    runtime::block_on(async {
        for round in 0..CONTROL_ROUNDS {
            expect_ok(
                &format!("control receipt {round}"),
                answered(
                    connection,
                    control,
                    2 * round,
                    Reply::Message(receipt.as_bytes()),
                )
                .await?,
            )?;
            expect_refused(
                &format!("control delivered refusal {round}"),
                answered(
                    connection,
                    control,
                    2 * round + 1,
                    Reply::Message(refusal_body.as_bytes()),
                )
                .await?,
                rejected(PUBLISH),
            )?;
        }
        Ok(())
    })
}

/// Submit one publish, answer publication `index` with a refusal, wait
/// until the operation fixed its result, then drop its waiter unread: an
/// abandoned failure.
///
/// The connection admits one operation at a time, and an operation frees
/// that slot only once its result is fixed. The probe admitted next proves
/// the refusal is fixed, so the drop abandons it rather than cancelling
/// running work.
fn abandon_one_refusal(connection: &Connection, control: &PeerControl, index: usize) -> Row {
    let refusal_body = jetstream_error(400, 10060);
    runtime::block_on(async {
        let mut publish = Box::pin(connection.publish(SUBJECT, b"abandoned"));
        let read = expect_withheld("the publish", control, index, publish.as_mut()).await?;
        control.reply(reply_to(&read)?, &Reply::Message(refusal_body.as_bytes()))?;
        slot_freed(connection)
            .await
            .map_err(|error| format!("abandon {index}: {error}"))?;
        // Unread: the refusal was published, never delivered.
        drop(publish);
        Ok(())
    })
}

/// Retry a publish the SDK refuses after admission until the one slot
/// admits it: the operation that held the slot has fixed its result.
async fn slot_freed(connection: &Connection) -> Row {
    finished_within("the held slot", async {
        loop {
            match refused(connection.publish(INVALID_NATS_SUBJECT, b"p").await) {
                Some(answer) if answer == busy(PUBLISH) => tokio::task::yield_now().await,
                Some(answer) if answer == rejected(PUBLISH) => return Ok(()),
                other => return Err(format!("the probe after the held slot read {other:?}")),
            }
        }
    })
    .await?
}

/// At saturation the next publish, subscribe, and connect are `Busy`: the
/// publish registers nothing, the peer reads no publication beyond the
/// `published` before it and no subscription beyond the private inbox, and
/// a fresh peer sees no connection.
fn saturation_refuses_without_effects(
    connection: &Connection,
    control: &PeerControl,
    published: usize,
) -> Row {
    expect_refused(
        "publish at saturation",
        row_bounded("publish", connection.publish(SUBJECT, b"x"))?,
        busy(PUBLISH),
    )?;
    expect_retired(connection)?;
    expect_refused(
        "subscribe at saturation",
        row_bounded("subscribe", connection.subscribe("saturated"))?,
        busy(IntegrationOperation::Subscribe),
    )?;
    let fresh = NatsPeer::start();
    let refused_connect = row_bounded("connect", acknowledged(&fresh.url()).connect())
        .and_then(|answer| {
            expect_refused(
                "connect at saturation",
                answer,
                busy(IntegrationOperation::Connect),
            )
        })
        .and_then(|()| fresh.control().expect_no_connection());
    fresh.finished(ROW_BOUND, refused_connect)?;
    control.delivery_barrier(ROW_BOUND)?;
    let log = control.log();
    expect_eq("publications", log.publications.len(), published)?;
    expect_eq(
        "subscriptions",
        private_subscriptions(&log),
        log.subscribed.len(),
    )?;
    expect_eq("private inbox subscriptions", log.subscribed.len(), 1)
}

/// Fail unless the runtime returned one aggregate holding exactly the
/// abandoned refusals of the steady instance and the churned instance's
/// failed close, each under its own instance.
fn expect_retained_accounts(teardown: &Result<(), RuntimeError>, ids: (u64, u64)) -> Row {
    let (steady_id, churned_id) = ids;
    let Err(error) = teardown else {
        return Err("a saturated budget left no aggregate".to_owned());
    };
    expect_own_instances(error, IntegrationKind::Nats)?;
    let accounts = integration_aggregate(error, IntegrationKind::Nats)?;
    let abandoned = (steady_id, rejected(PUBLISH));
    let failed_close = (churned_id, timed_out(IntegrationOperation::Close));
    let others: Vec<&(u64, Refusal)> = accounts
        .iter()
        .filter(|account| **account != abandoned && **account != failed_close)
        .collect();
    expect_eq("accounts no instance retained", others, Vec::new())?;
    expect_eq(
        "abandoned accounts",
        accounts
            .iter()
            .filter(|account| **account == abandoned)
            .count(),
        ABANDONED,
    )?;
    expect_eq(
        "failed-close accounts",
        accounts
            .iter()
            .filter(|account| **account == failed_close)
            .count(),
        1,
    )
}
