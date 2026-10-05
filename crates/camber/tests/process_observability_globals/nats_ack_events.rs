//! 4.T1–4.T2: acknowledged NATS publishing settles through the existing
//! terminal owner.
//!
//! The rows join `nats_events::nats_terminals_match_events_counters_and_durations`
//! and run in its private child under its one recorder. Each row owns one
//! runtime and one scripted peer, and finishes the peer on every exit. The
//! peer answers a publication only when a row hands it the bytes, so every
//! settled class comes from Camber's own decoder and operation owner.
//!
//! Each admitted operation must yield exactly one event, one counter
//! increment, and one duration sample. A refusal before admission yields one
//! event and one counter, with no duration. A late, duplicate, or stranger's
//! reply yields nothing. The reply receiver never emits a terminal.

use crate::event_rows::{EXHAUSTED, expect_timeout_duration, failed_as, timed};
use crate::integration_events::{
    Observation, Observed, Terminal, failed, outside_camber, refused, success,
};
use crate::integration_rows::{
    INVALID_NATS_SUBJECT, ROW_BOUND, Refusal, Row, all, busy, cancelled, clean_run, closed, expect,
    expect_aggregate, expect_eq, expect_failed_run, expect_ok, expect_polled_pending,
    expect_refused, expect_scope_closed, expired, invalid_config, limit_exceeded, observed_verdict,
    refusal, refused as refusal_of, rejected, row_bounded, run_observing, settled, unavailable,
    unknown,
};
use crate::nats_ack_rows::{
    CorrelatedAnswer, FORCED_GRACE, HeldPublish, STREAM, SUBJECT, ack, ack_probe, acknowledge,
    acknowledged, answered, answered_twice, correlated_answers, disconnect_after_submission,
    expect_forced_accounts, expect_reached_hold, expect_retired, expect_withheld, finished_within,
    late_reply_is_discarded, publication, ready_polls_until, reply_to, stranger_reply,
    wait_resubscribed, wait_snapshot,
};
use crate::nats_peer::wire::Reply;
use crate::nats_peer::{NatsPeer, PeerControl, Script};
use camber::mq::nats::{self, Connection};
use camber::runtime_test_support::{
    NatsAckProbe, NatsAckReceiver, NatsPublishProbe, NatsQueueProbe,
};
use camber::{
    IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability, RuntimeError, runtime,
};
use std::error::Error;
use std::pin::pin;
use std::time::Duration;

const NATS: IntegrationKind = IntegrationKind::Nats;

const PUBLISH: IntegrationOperation = IntegrationOperation::Publish;

/// The one terminal per distinct class among `outcomes`, counted: `None` is
/// a successful publish.
fn publish_terminals(outcomes: &[Option<Refusal>]) -> Box<[Terminal]> {
    let mut counted: Vec<(Option<Refusal>, usize)> = Vec::new();
    for outcome in outcomes {
        match counted.iter_mut().find(|(seen, _)| seen == outcome) {
            Some((_, count)) => *count += 1,
            None => counted.push((*outcome, 1)),
        }
    }
    counted
        .into_iter()
        .map(|(outcome, count)| {
            let terminal = match outcome {
                None => success(NATS, PUBLISH),
                Some(refused) => failed_as(NATS, refused),
            };
            terminal.times(count)
        })
        .collect()
}

/// Connect and close each settled once, with `publishes` between them.
fn around_publishes(publishes: impl IntoIterator<Item = Terminal>) -> Box<[Terminal]> {
    [
        success(NATS, IntegrationOperation::Connect),
        success(NATS, IntegrationOperation::Close),
    ]
    .into_iter()
    .chain(publishes)
    .collect()
}

/// Every terminal is the one listed, all of one admitted instance.
fn expect_one_connection(observed: &Observed, expected: &[Terminal]) -> Row {
    all([
        observed.expect_terminals(expected),
        observed.expect_one_instance(NATS),
    ])
}

// ── 4.T1 terminals commit once ───────────────────────────────────────

/// One example per terminal class, a send refusal after registration, and a
/// disconnect after submission each settle their publish once, with one
/// counter and one duration; the receiver adds nothing.
pub(crate) fn nats_acknowledged_terminals_commit_once() -> Row {
    let answers = terminal_answers();
    expect_eq("distinct reply terminal classes", answers.len(), 6)?;
    let reply_count = answers.len();
    let expected: Box<[Option<Refusal>]> = answers
        .iter()
        .map(|(_, _, expected)| *expected)
        .chain([Some(rejected(PUBLISH)), Some(unknown(PUBLISH))])
        .collect();
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let control = peer.control();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Result<Box<[Option<Refusal>]>, String> {
        let connection = settled("connect", builder.connect())?;
        let mut read = runtime::block_on(every_reply_settles(&connection, &control, &answers))?;
        read.push(refusal_of(row_bounded(
            "publish",
            connection.publish(INVALID_NATS_SUBJECT, b"x"),
        )?));
        let (_, cut) = runtime::block_on(disconnect_after_submission(
            &connection,
            &control,
            reply_count,
        ))?;
        read.push(refusal_of(cut));
        wait_resubscribed(&control)?;
        expect_retired(&connection)?;
        settled("close", connection.close())?;
        Ok(read.into_boxed_slice())
    });
    let observed = observation.finish();
    let verdict = clean_run(outcome).and_then(|read| {
        all([
            expect_eq("the classes the publishes read", &read, &expected),
            expect_one_connection(&observed, &around_publishes(publish_terminals(&expected))),
            expect_eq(
                "publications, one per submitted publish",
                peer.control().log().publications.len(),
                reply_count + 1,
            ),
        ])
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Decoder edge cases belong to the component matrix; telemetry needs each class once.
fn terminal_answers() -> Box<[CorrelatedAnswer]> {
    let mut distinct: Vec<CorrelatedAnswer> = Vec::new();
    for answer in correlated_answers() {
        if !distinct.iter().any(|seen| seen.2 == answer.2) {
            distinct.push(answer);
        }
    }
    distinct.into_boxed_slice()
}

/// Answer one publish with each correlated reply in turn; what each read.
async fn every_reply_settles(
    connection: &Connection,
    control: &PeerControl,
    answers: &[CorrelatedAnswer],
) -> Result<Vec<Option<Refusal>>, String> {
    let mut read = Vec::new();
    for (index, (_, answer, _)) in answers.iter().enumerate() {
        read.push(refusal_of(
            answered(connection, control, index, answer.reply()).await?,
        ));
    }
    Ok(read)
}

/// A publish held before the SDK queue until its expiry is one
/// `Timeout/Safe` terminal whose duration lies between that expiry and the
/// caller's wait; nothing reaches the peer.
pub(crate) fn nats_acknowledged_safe_expiry_settles_once() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url()).operation_timeout(EXHAUSTED);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Result<Duration, String> {
        let mut connection = settled("connect", builder.connect())?;
        let hold =
            NatsPublishProbe::hold(&mut connection).ok_or("the publish probe did not attach")?;
        let held = timed("the held publish", connection.publish(SUBJECT, b"held"));
        hold.release();
        let (answer, waited) = held?;
        expect_refused(
            "the publish held before submission",
            answer,
            expired(PUBLISH),
        )?;
        expect_retired(&connection)?;
        settled("close", connection.close())?;
        Ok(waited)
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome)
            .and_then(|waited| expect_timeout_duration(&observed, NATS, PUBLISH, waited)),
        expect_one_connection(
            &observed,
            &around_publishes([failed_as(NATS, expired(PUBLISH))]),
        ),
        expect_eq("publications", peer.control().log().publications.len(), 0),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A publish dropped while held before the SDK queue is one
/// `Cancelled/Safe` terminal, and its account is released.
pub(crate) fn nats_acknowledged_safe_cancellation_settles_once() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let mut connection = settled("connect", builder.connect())?;
        let mut hold =
            NatsPublishProbe::hold(&mut connection).ok_or("the publish probe did not attach")?;
        let dropped: Row = runtime::block_on(async {
            let mut publish = Box::pin(connection.publish(SUBJECT, b"dropped"));
            expect_polled_pending("the held publish", &futures_util::poll!(publish.as_mut()))?;
            expect_reached_hold(&mut hold).await?;
            drop(publish);
            Ok(())
        });
        // Close waits for the abandoned work, so the release reaches nothing.
        let closed = dropped.and_then(|()| settled("close", connection.close()));
        hold.release();
        closed
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        expect_one_connection(
            &observed,
            &around_publishes([failed(
                NATS,
                PUBLISH,
                IntegrationFailure::Cancelled,
                Retryability::Safe,
            )]),
        ),
        expect_eq("publications", peer.control().log().publications.len(), 0),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A publish the SDK admitted and that is held after its submission mark
/// past its expiry is one `Timeout/OutcomeUnknown` terminal, bounded by
/// that expiry and the caller's wait.
pub(crate) fn nats_acknowledged_submitted_expiry_settles_once() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url()).operation_timeout(EXHAUSTED);
    let control = peer.control();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Result<Duration, String> {
        let mut connection = settled("connect", builder.connect())?;
        let mut queue =
            NatsQueueProbe::hold(&mut connection).ok_or("the queue probe did not attach")?;
        let held = timed("the held publish", connection.publish(SUBJECT, b"held"));
        let admitted = row_bounded("the SDK admission", queue.polled(1));
        queue.release();
        let (answer, waited) = held?;
        expect_eq("SDK admissions", admitted?, Some(1))?;
        expect_refused(
            "the publish held after submission",
            answer,
            (
                PUBLISH,
                IntegrationFailure::Timeout,
                Retryability::OutcomeUnknown,
            ),
        )?;
        let read = publication(&control, 0)?;
        late_reply_is_discarded(&connection, &control, &read)?;
        settled("close", connection.close())?;
        Ok(waited)
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome)
            .and_then(|waited| expect_timeout_duration(&observed, NATS, PUBLISH, waited)),
        expect_eq("publications", peer.control().log().publications.len(), 1),
        expect_one_connection(
            &observed,
            &around_publishes([failed(
                NATS,
                PUBLISH,
                IntegrationFailure::Timeout,
                Retryability::OutcomeUnknown,
            )]),
        ),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A publish dropped after the SDK admitted it and the peer read it is one
/// `Cancelled/OutcomeUnknown` terminal; the aggregate that keeps its account
/// adds no second one.
pub(crate) fn nats_acknowledged_submitted_cancellation_settles_once() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let control = peer.control();
    let observation = Observation::start();
    let outcome = run_observing(runtime::builder(), move || -> Row {
        let mut connection = settled("connect", builder.connect())?;
        let mut queue =
            NatsQueueProbe::hold(&mut connection).ok_or("the queue probe did not attach")?;
        let dropped: Row = runtime::block_on(async {
            let mut publish = Box::pin(connection.publish(SUBJECT, b"dropped"));
            expect_polled_pending("the held publish", &futures_util::poll!(publish.as_mut()))?;
            let admitted = finished_within("the SDK admission", queue.polled(1)).await?;
            expect_eq("SDK admissions", admitted, Some(1))?;
            publication(&control, 0)?;
            drop(publish);
            Ok(())
        });
        let closed = dropped.and_then(|()| settled("close", connection.close()));
        queue.release();
        closed
    });
    let observed = observation.finish();
    let verdict = all([
        expect_failed_run(
            outcome,
            "the cancelled submitted publish left no aggregate",
            |error| expect_aggregate(error, NATS, &[cancelled(PUBLISH)]),
        ),
        expect_one_connection(
            &observed,
            &around_publishes([failed_as(NATS, cancelled(PUBLISH))]),
        ),
        expect_eq("publications", peer.control().log().publications.len(), 1),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

// ── 4.T1 late replies ────────────────────────────────────────────────

/// A duplicate refusal after a receipt, a receipt after the caller dropped
/// its publish, and a receipt for a token no publish holds emit no terminal:
/// each admitted publish settles exactly once.
pub(crate) fn nats_acknowledged_late_reply_emits_no_terminal() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let control = peer.control();
    let observation = Observation::start();
    let outcome = run_observing(runtime::builder(), move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let probe = ack_probe(&connection)?;
        runtime::block_on(answered_twice(&connection, &control))?;
        let read = runtime::block_on(async {
            let mut publish = Box::pin(connection.publish(SUBJECT, b"dropped"));
            let read = expect_withheld("the publish", &control, 1, publish.as_mut()).await?;
            drop(publish);
            Ok::<_, String>(read)
        })?;
        wait_snapshot("the dropped entry", &probe, |snapshot| {
            snapshot.pending == 0
        })?;
        late_reply_is_discarded(&connection, &control, &read)?;
        control.reply(
            &stranger_reply(&read)?,
            &Reply::Message(ack(STREAM, 3).as_bytes()),
        )?;
        control.delivery_barrier(ROW_BOUND)?;
        expect_retired(&connection)?;
        expect_ok(
            "the publish after every late reply",
            runtime::block_on(answered(
                &connection,
                &control,
                2,
                Reply::Message(ack(STREAM, 4).as_bytes()),
            ))?,
        )?;
        settled("close", connection.close())
    });
    let observed = observation.finish();
    let verdict = all([
        expect_failed_run(outcome, "the dropped publish left no aggregate", |error| {
            expect_aggregate(error, NATS, &[cancelled(PUBLISH)])
        }),
        expect_one_connection(
            &observed,
            &around_publishes([
                success(NATS, PUBLISH).times(2),
                failed_as(NATS, cancelled(PUBLISH)),
            ]),
        ),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

// ── 4.T1 refusals before admission ───────────────────────────────────

/// Refusals before admission are one terminal and one counter each, with no
/// duration: an invalid stream and a closed scope before any instance, then
/// a payload over the maximum, the operation limit, a disconnected
/// transport, and closed access on an admitted connection.
pub(crate) fn nats_acknowledged_pre_admission_refusal_has_no_duration() -> Row {
    all([
        unadmitted_connects_have_no_instance(),
        admitted_connection_refusals(),
    ])
}

/// An invalid stream refuses before admission; so does an acknowledged
/// connect after the runtime's stop. Neither names an instance or reaches
/// the peer.
fn unadmitted_connects_have_no_instance() -> Row {
    use IntegrationOperation::Connect;
    let peer = NatsPeer::start();
    let url = peer.url();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        expect_refused(
            "an invalid stream",
            row_bounded(
                "connect",
                nats::builder(&url)
                    .acknowledged_publishing("bad.stream")
                    .connect(),
            )?,
            invalid_config(Connect),
        )?;
        runtime::request_shutdown();
        expect_scope_closed(
            "an acknowledged connect after the stop",
            row_bounded("connect", acknowledged(&url).connect())?,
        )
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            failed_as(NATS, invalid_config(Connect)).before_admission(),
            refused(NATS, Connect),
        ]),
        observed.expect_no_instance(NATS),
        peer.control().expect_no_connection(),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// On an admitted connection: a payload over the maximum, a publish at the
/// operation limit, a publish while disconnected, and a publish after close
/// each refuse before admission. The publish that held the limit settles
/// once on its receipt, and every readiness poll is one terminal.
fn admitted_connection_refusals() -> Row {
    use IntegrationOperation::Ready;
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url())
        .max_in_flight(1)
        .max_message_bytes(4);
    let control = peer.control();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Result<usize, String> {
        let connection = settled("connect", builder.connect())?;
        expect_refused(
            "a payload past the maximum",
            row_bounded("publish", connection.publish(SUBJECT, b"12345"))?,
            limit_exceeded(PUBLISH),
        )?;
        runtime::block_on(limit_refuses_while_held(&connection, &control))?;
        control.script(Script::Refuse);
        let ready_polls = ready_polls_until("the SDK disconnect", &connection, unavailable(Ready))?;
        expect_refused(
            "a publish while disconnected",
            row_bounded("publish", connection.publish(SUBJECT, b"x"))?,
            unavailable(PUBLISH),
        )?;
        control.script(Script::Serve);
        wait_resubscribed(&control)?;
        settled("close", connection.close())?;
        expect_refused(
            "a publish after close",
            row_bounded("publish", connection.publish(SUBJECT, b"x"))?,
            closed(PUBLISH),
        )?;
        Ok(ready_polls)
    });
    let observed = observation.finish();
    let seen = clean_run(outcome);
    let ready_polls = seen.as_ref().map_or(0, |polls| *polls);
    let verdict = all([
        seen.map(drop),
        expect_one_connection(
            &observed,
            &around_publishes([
                success(NATS, Ready).times(ready_polls),
                failed_as(NATS, unavailable(Ready)).before_admission(),
                success(NATS, PUBLISH),
                failed_as(NATS, limit_exceeded(PUBLISH)).before_admission(),
                failed_as(NATS, busy(PUBLISH)).before_admission(),
                failed_as(NATS, unavailable(PUBLISH)).before_admission(),
                failed_as(NATS, closed(PUBLISH)).before_admission(),
            ]),
        ),
        expect_eq("publications", peer.control().log().publications.len(), 1),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// While one publish awaits its receipt in the only slot, the next is
/// `Busy`; the receipt then settles the first.
async fn limit_refuses_while_held(connection: &Connection, control: &PeerControl) -> Row {
    let mut publish = pin!(connection.publish(SUBJECT, b"held"));
    let read = expect_withheld("the publish", control, 0, publish.as_mut()).await?;
    expect_refused(
        "a publish past the operation limit",
        connection.publish(SUBJECT, b"x").await,
        busy(PUBLISH),
    )?;
    acknowledge(control, &read, 1)?;
    expect_ok(
        "the held publish",
        finished_within("the publish", publish).await?,
    )
}

// ── 4.T1 shutdown ────────────────────────────────────────────────────

/// A graceful stop with a publish awaiting its receipt refuses new work at
/// once, but the receiver routes until the admitted publish settles on its
/// receipt: one success, and one close under shutdown.
pub(crate) fn nats_acknowledged_graceful_stop_settles_once() -> Row {
    use IntegrationOperation::{Close, Connect, Ready};
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let control = peer.control();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Result<usize, String> {
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
        let ready_polls =
            ready_polls_until("the stop's closing commit", &connection, closed(Ready))?;
        expect_refused(
            "a publish after the stop",
            row_bounded("publish", connection.publish(SUBJECT, b"refused"))?,
            closed(PUBLISH),
        )?;
        acknowledge(&control, &read, 1)?;
        expect_ok(
            "the publish the stop drained",
            row_bounded("the draining publish", publish)?,
        )?;
        wait_snapshot("the receiver's end", &probe, |snapshot| {
            snapshot.receiver == NatsAckReceiver::Ended
        })?;
        Ok(ready_polls)
    });
    let observed = observation.finish();
    let seen = clean_run(outcome);
    let ready_polls = seen.as_ref().map_or(0, |polls| *polls);
    let verdict = all([
        seen.map(drop),
        observed.expect_terminals(&[
            success(NATS, Connect),
            success(NATS, Ready).times(ready_polls),
            failed_as(NATS, closed(Ready)).before_admission(),
            failed_as(NATS, closed(PUBLISH)).before_admission(),
            success(NATS, PUBLISH),
            success(NATS, Close).under_shutdown(),
        ]),
        observed.expect_one_instance(NATS),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A publish the peer read, still awaiting its receipt when the root
/// returns, outlives the stop's grace: the forced stop drops both the
/// operation and the close owner that runs the receiver.
///
/// The drops have no order Camber promises. The operation's own drop
/// settles `Cancelled`; a receiver dropped first leaves the polled
/// operation `OutcomeUnknown`. Both keep the retry unknown and settle under
/// the stop, once.
pub(crate) fn nats_acknowledged_forced_stop_settles_under_shutdown() -> Row {
    use IntegrationOperation::{Close, Connect};
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url()).operation_timeout(ROW_BOUND);
    let control = peer.control();
    let observation = Observation::start();
    let (seen, teardown) = run_observing(
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
            Ok((publish, probe))
        },
    );
    let read = observed_verdict(seen).and_then(|(publish, probe)| {
        let read = refusal_of(outside_camber(publish)?);
        expect(
            &format!("the forced publish read {read:?}"),
            [Some(cancelled(PUBLISH)), Some(unknown(PUBLISH))].contains(&read),
        )?;
        let snapshot = probe.snapshot();
        expect_eq(
            "the reply receiver",
            snapshot.receiver,
            NatsAckReceiver::Ended,
        )?;
        expect_eq("pending correlation entries", snapshot.pending, 0)?;
        expect_forced_accounts(&teardown, read)
    });
    let observed = observation.finish();
    let verdict = all([
        read,
        observed.expect_terminals(&[
            success(NATS, Connect),
            failed(
                NATS,
                PUBLISH,
                IntegrationFailure::Cancelled,
                Retryability::OutcomeUnknown,
            )
            .or(IntegrationFailure::OutcomeUnknown)
            .under_shutdown(),
            success(NATS, Close)
                .or(IntegrationFailure::Timeout)
                .under_shutdown(),
        ]),
        observed.expect_one_instance(NATS),
        expect_eq("publications", peer.control().log().publications.len(), 1),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

// ── 4.T2 redaction ───────────────────────────────────────────────────

/// A stream name no event, sample, or rendering may repeat.
const STREAM_MARKER: &str = "STREAM_MARK_51C3";

/// A subject no event, sample, or rendering may repeat.
const SUBJECT_MARKER: &str = "orders.subject-mark-7a2e";

/// A payload no event, sample, or rendering may repeat.
const PAYLOAD_MARKER: &str = "payload-mark-0b9d";

/// A connect password no event, sample, or rendering may repeat.
const PASSWORD_MARKER: &str = "nats-password-mark-6f1e";

/// A JetStream error description the typed source keeps and no rendering
/// repeats.
const DESCRIPTION_MARKER: &str = "description-mark-d47a";

/// A stream mismatch whose description carries [`DESCRIPTION_MARKER`].
fn marked_refusal() -> String {
    format!(
        "{{\"error\":{{\"code\":400,\"err_code\":10060,\"description\":\"{DESCRIPTION_MARKER}\"}}}}"
    )
}

/// No stream, subject, inbox, payload, credential, or reply description
/// reaches an event, a metric sample, or an error rendering, and the
/// builder's rendering keeps no credential. The rejected publish keeps its
/// typed JetStream source.
pub(crate) fn nats_acknowledged_redacts_dynamic_values() -> Row {
    let peer = NatsPeer::start();
    let url = peer
        .url()
        .replacen("nats://", &format!("nats://camber:{PASSWORD_MARKER}@"), 1);
    let builder = nats::builder(&url).acknowledged_publishing(STREAM_MARKER);
    let rendered_builder = format!("{builder:?}");
    let control = peer.control();
    let observation = Observation::start();
    let outcome =
        runtime::builder().run(move || -> Result<(Box<str>, Box<[RuntimeError]>), String> {
            let connection = settled("connect", builder.connect())?;
            let (inbox, errors) = runtime::block_on(marked_publishes(&connection, &control))?;
            settled("close", connection.close())?;
            Ok((inbox, errors))
        });
    let observed = observation.finish();
    let verdict = clean_run(outcome).and_then(|(inbox, errors)| {
        let markers = [
            STREAM_MARKER,
            SUBJECT_MARKER,
            PAYLOAD_MARKER,
            PASSWORD_MARKER,
            DESCRIPTION_MARKER,
            &inbox,
        ];
        all([
            expect_one_connection(
                &observed,
                &around_publishes([
                    success(NATS, PUBLISH),
                    failed_as(NATS, rejected(PUBLISH)),
                    failed_as(NATS, unknown(PUBLISH)),
                ]),
            ),
            observed.expect_redacted(&markers),
            expect_samples_redacted(&observed, &markers),
            expect_unrendered("the builder", &rendered_builder, &[PASSWORD_MARKER]),
            expect_server_list_redacted(),
            all(errors
                .iter()
                .map(|error| expect_error_redacted(error, &markers))),
            expect_typed_source(&errors),
        ])
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Publish the marked payload to the marked subject three times: a matching
/// receipt, a marked refusal, and another stream's receipt. The private
/// inbox the publications named, and the two errors.
async fn marked_publishes(
    connection: &Connection,
    control: &PeerControl,
) -> Result<(Box<str>, Box<[RuntimeError]>), String> {
    let marked_ack = ack(STREAM_MARKER, 1);
    let other_stream = ack("OTHER", 1);
    let refusal_body = marked_refusal();
    let answers = [
        marked_ack.as_bytes(),
        refusal_body.as_bytes(),
        other_stream.as_bytes(),
    ];
    let mut errors = Vec::new();
    let mut inbox = None;
    for (index, answer) in answers.into_iter().enumerate() {
        let mut publish = pin!(connection.publish(SUBJECT_MARKER, PAYLOAD_MARKER.as_bytes()));
        let read = expect_withheld("the marked publish", control, index, publish.as_mut()).await?;
        let reply = reply_to(&read)?;
        let (prefix, _) = reply.rsplit_once('.').ok_or("no reply token")?;
        inbox.get_or_insert_with(|| Box::from(prefix));
        control.reply(reply, &Reply::Message(answer))?;
        if let Err(error) = finished_within("the marked publish", publish).await? {
            errors.push(error);
        }
    }
    let inbox = inbox.ok_or("no publication named an inbox")?;
    expect_eq("errors the marked publishes read", errors.len(), 2)?;
    Ok((inbox, errors.into_boxed_slice()))
}

/// Fail the row if `rendered` repeats any of `markers`.
fn expect_unrendered(what: &str, rendered: &str, markers: &[&str]) -> Row {
    let leaked: Vec<&str> = markers
        .iter()
        .copied()
        .filter(|marker| rendered.contains(marker))
        .collect();
    expect_eq(
        &format!("markers {what} rendered: {rendered}"),
        leaked,
        Vec::<&str>::new(),
    )
}

/// A builder over several servers renders every host but no credential,
/// with or without a scheme or a path.
fn expect_server_list_redacted() -> Row {
    let url = format!(
        "tls://camber:{PASSWORD_MARKER}@nats-a:4222,nats-b:4222,camber:{PASSWORD_MARKER}@nats-c:4222/x"
    );
    let rendered = format!("{:?}", nats::builder(&url));
    all([
        expect_unrendered("a server list", &rendered, &[PASSWORD_MARKER]),
        expect(
            &format!("a server list rendered every host: {rendered}"),
            [
                "tls://<redacted>@nats-a:4222",
                ",nats-b:4222,",
                "<redacted>@nats-c:4222/x",
            ]
            .iter()
            .all(|host| rendered.contains(host)),
        ),
    ])
}

/// No rendered integration sample names any of `markers`.
fn expect_samples_redacted(observed: &Observed, markers: &[&str]) -> Row {
    let (_, after) = observed.scrapes()?;
    all(after
        .iter()
        .map(|sample| expect_unrendered("a metric sample", &format!("{sample:?}"), markers)))
}

/// Neither `Display` nor either `Debug` of `error` repeats a marker.
fn expect_error_redacted(error: &RuntimeError, markers: &[&str]) -> Row {
    all([
        expect_unrendered("an error's Display", &error.to_string(), markers),
        expect_unrendered("an error's Debug", &format!("{error:?}"), markers),
        expect_unrendered(
            "an error's alternate Debug",
            &format!("{error:#?}"),
            markers,
        ),
    ])
}

/// The rejected publish's error keeps the decoded JetStream error as its
/// source, description and all, for a caller who inspects it.
fn expect_typed_source(errors: &[RuntimeError]) -> Row {
    let rejected_error = errors
        .iter()
        .find_map(|error| match error {
            RuntimeError::Integration(failure) if refusal(error) == Some(rejected(PUBLISH)) => {
                Some(failure)
            }
            _ => None,
        })
        .ok_or("no rejected publish error")?;
    let source = rejected_error
        .source()
        .ok_or("the rejected publish dropped its JetStream source")?;
    expect(
        &format!("the JetStream source {source} keeps its description"),
        source.to_string().contains(DESCRIPTION_MARKER),
    )
}
