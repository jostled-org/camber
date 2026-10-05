//! NATS operation terminal events through public transport paths.

use crate::event_rows::{
    EXHAUSTED, SMALL_MAX, expect_timeout_duration, expect_unreached, failed_as, run_terminal_rows,
    timed,
};
use crate::integration_events::{
    Observation, ROW_BOUND, bounded, expect_aggregate_failures, failed, outside_camber, refused,
    settled, success,
};
use crate::integration_rows::{
    LIVE_LIMIT, NamedRow, Refusal, Row, all, busy, cancelled, clean_run, closed, expect, expect_eq,
    expect_no_runtime, expect_ok, expect_pending, expect_polled_pending, expect_refused,
    expect_scope_closed, expired, hold_live_slots, invalid_config, limit_exceeded,
    observed_verdict, permission_denied, refusal, run_observing, timed_out, unavailable,
};
use crate::nats_ack_events as ack;
use crate::nats_ack_rows::ready_polls_until;
use crate::nats_peer::{
    NatsPeer, PeerControl as NatsControl, Script as NatsScript, expect_stalled_publish,
    hold_transport,
};
use camber::mq::nats::{self, Connection, Subscription};
use camber::runtime_test_support::{NatsEventProbe, NatsPublishProbe};
use camber::{
    IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability, RuntimeError, runtime,
};
use std::future::Future;
use std::time::{Duration, Instant};

/// A receive bound a row lets pass with nothing delivered.
const QUIET: Duration = Duration::from_millis(50);

/// The per-subscription buffer a slow-consumer row floods.
const FLOODED_CAPACITY: usize = 2;

/// A payload larger than any loopback socket buffers while the peer is not
/// reading, so its flush cannot complete.
const STALLED_PAYLOAD: usize = 16 * 1024 * 1024;

/// A NATS password the connect URL carries; no event may repeat it.
const NATS_PASSWORD: &str = "nats-password-7f3a";

/// A NATS subject; it may name an event, never a label.
const SUBJECT: &str = "orders.subject-c41b";

/// A message payload no event may repeat, as text for the redaction scan.
const PAYLOAD_TEXT: &str = "payload-body-9e2d";

/// [`PAYLOAD_TEXT`] as the bytes a row publishes.
const PAYLOAD: &[u8] = PAYLOAD_TEXT.as_bytes();

#[test]
fn nats_terminals_match_events_counters_and_durations() {
    run_terminal_rows(
        "nats_events::nats_terminals_match_events_counters_and_durations",
        "nats-terminal-events",
        "NATS_TERMINAL_EVENTS_COMPLETE",
        "M9 NATS terminal event or metric contract is missing",
        TERMINAL_ROWS,
    );
}

const TERMINAL_ROWS: &[NamedRow<'static>] = &[
    (
        "nats cancelled connect settles once",
        nats_cancelled_connect_settles_once,
    ),
    (
        "nats cancelled next settles once",
        nats_cancelled_next_settles_once,
    ),
    (
        "nats cancelled next timeout settles once",
        nats_cancelled_next_timeout_settles_once,
    ),
    (
        "nats unsubmitted publish cancellation settles once",
        nats_unsubmitted_publish_cancellation_settles_once,
    ),
    (
        "nats operations settle once each",
        nats_operations_settle_once_each,
    ),
    (
        "nats configuration refusal redacts its source",
        nats_configuration_refusal_redacts_its_source,
    ),
    (
        "nats pre admission refusal reaches no later operation",
        nats_pre_admission_refusal_reaches_no_later_operation,
    ),
    (
        "nats service rejection is one terminal",
        nats_service_rejection_is_one_terminal,
    ),
    (
        "nats timeout and unknown outcome are bounded once",
        nats_timeout_and_unknown_outcome_are_bounded_once,
    ),
    (
        "nats abandoned result is one terminal",
        nats_abandoned_result_is_one_terminal,
    ),
    (
        "nats runtime stop closes under shutdown",
        nats_runtime_stop_closes_under_shutdown,
    ),
    (
        "nats missing runtime refusal is one terminal",
        nats_missing_runtime_refusal_is_one_terminal,
    ),
    (
        "nats closed scope refusal is one terminal",
        nats_closed_scope_refusal_is_one_terminal,
    ),
    (
        "nats unavailable peer fails connect once",
        nats_unavailable_peer_fails_connect_once,
    ),
    (
        "nats connect timeout is bounded once",
        nats_connect_timeout_is_bounded_once,
    ),
    (
        "nats refusals on an admitted connection have no duration",
        nats_refusals_on_an_admitted_connection_have_no_duration,
    ),
    (
        "nats disconnected refusals are unavailable once each",
        nats_disconnected_refusals_are_unavailable_once_each,
    ),
    (
        "nats subscription admission and close settle once",
        nats_subscription_admission_and_close_settle_once,
    ),
    (
        "nats receive outcomes settle once per call",
        nats_receive_outcomes_settle_once_per_call,
    ),
    (
        "nats slow consumer closure is one terminal",
        nats_slow_consumer_closure_is_one_terminal,
    ),
    (
        "nats concurrent close settles once",
        nats_concurrent_close_settles_once,
    ),
    (
        "nats last handle drop closes once",
        nats_last_handle_drop_closes_once,
    ),
    (
        "nats publish held at runtime stop settles under shutdown",
        nats_publish_held_at_runtime_stop_settles_under_shutdown,
    ),
    (
        "nats_acknowledged_terminals_commit_once",
        ack::nats_acknowledged_terminals_commit_once,
    ),
    (
        "nats acknowledged safe expiry settles once",
        ack::nats_acknowledged_safe_expiry_settles_once,
    ),
    (
        "nats acknowledged safe cancellation settles once",
        ack::nats_acknowledged_safe_cancellation_settles_once,
    ),
    (
        "nats acknowledged submitted expiry settles once",
        ack::nats_acknowledged_submitted_expiry_settles_once,
    ),
    (
        "nats acknowledged submitted cancellation settles once",
        ack::nats_acknowledged_submitted_cancellation_settles_once,
    ),
    (
        "nats_acknowledged_late_reply_emits_no_terminal",
        ack::nats_acknowledged_late_reply_emits_no_terminal,
    ),
    (
        "nats_acknowledged_pre_admission_refusal_has_no_duration",
        ack::nats_acknowledged_pre_admission_refusal_has_no_duration,
    ),
    (
        "nats acknowledged graceful stop settles once",
        ack::nats_acknowledged_graceful_stop_settles_once,
    ),
    (
        "nats acknowledged forced stop settles under shutdown",
        ack::nats_acknowledged_forced_stop_settles_under_shutdown,
    ),
    (
        "nats_acknowledged_redacts_dynamic_values",
        ack::nats_acknowledged_redacts_dynamic_values,
    ),
];

/// Poll the public waiter into its admitted pending state, then abandon it.
fn abandon_pending(future: impl Future) -> Row {
    let mut pending = Box::pin(future);
    expect_pending("the operation before caller drop", pending.as_mut())?;
    drop(pending);
    Ok(())
}

fn nats_cancelled_connect_settles_once() -> Row {
    let peer = NatsPeer::start();
    peer.control().script(NatsScript::Silent);
    let builder = nats::builder(&peer.url()).connect_timeout(ROW_BOUND);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || abandon_pending(builder.connect()));
    let observed = observation.finish();
    let nats = IntegrationKind::Nats;
    peer.finished(
        ROW_BOUND,
        all([
            clean_run(outcome),
            observed.expect_terminals(&[failed(
                nats,
                IntegrationOperation::Connect,
                IntegrationFailure::Cancelled,
                Retryability::Safe,
            )]),
            observed.expect_one_instance(nats),
        ]),
    )
}

fn nats_cancelled_next_settles_once() -> Row {
    cancelled_receive(false)
}

fn nats_cancelled_next_timeout_settles_once() -> Row {
    cancelled_receive(true)
}

/// Cancellation leaves the subscription usable and does not consume a message.
fn cancelled_receive(timed: bool) -> Row {
    use IntegrationOperation::{Close, Connect, Receive, Subscribe};
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let control = peer.control();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut subscription = settled("subscribe", connection.subscribe(SUBJECT))?;
        let sid = control.sid(SUBJECT, ROW_BOUND)?;
        match timed {
            true => abandon_pending(subscription.next_timeout(ROW_BOUND))?,
            false => abandon_pending(subscription.next())?,
        }
        control.deliver(&sid, SUBJECT, &[PAYLOAD])?;
        let message = expect_ok(
            "receive after cancellation",
            bounded("next", subscription.next())?,
        )?
        .ok_or_else(|| "cancellation ended the subscription".to_owned())?;
        expect_eq(
            "the next receive keeps the payload",
            message.payload(),
            PAYLOAD,
        )?;
        settled("subscription close", subscription.close())?;
        settled("close", connection.close())
    });
    let observed = observation.finish();
    let nats = IntegrationKind::Nats;
    peer.finished(
        ROW_BOUND,
        all([
            clean_run(outcome),
            observed.expect_terminals(&[
                success(nats, Connect),
                success(nats, Subscribe),
                success(nats, Receive),
                failed(
                    nats,
                    Receive,
                    IntegrationFailure::Cancelled,
                    Retryability::Safe,
                ),
                success(nats, Close).times(2),
            ]),
            observed.expect_one_instance(nats),
        ]),
    )
}

fn nats_unsubmitted_publish_cancellation_settles_once() -> Row {
    use IntegrationOperation::{Close, Connect, Publish};
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let control = peer.control();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let mut connection = settled("connect", builder.connect())?;
        let mut probe = NatsPublishProbe::hold(&mut connection)
            .ok_or_else(|| "the unshared connection must accept a publish checkpoint".to_owned())?;
        runtime::block_on(async {
            let mut publish = Box::pin(connection.publish(SUBJECT, PAYLOAD));
            expect_polled_pending("the publish", &futures_util::poll!(publish.as_mut()))?;
            let reached = tokio::time::timeout(ROW_BOUND, probe.reached())
                .await
                .map_err(|_| "publish never reached the pre-submission checkpoint".to_owned())?;
            expect("publish reached the pre-submission checkpoint", reached)?;
            drop(publish);
            Ok::<(), String>(())
        })?;
        expect_ok(
            "close joins cancelled work",
            bounded("close", connection.close())?,
        )?;
        probe.release();
        expect(
            "an unsubmitted publish must not reach the wire",
            control.log().published.is_empty(),
        )
    });
    let observed = observation.finish();
    let nats = IntegrationKind::Nats;
    peer.finished(
        ROW_BOUND,
        all([
            clean_run(outcome),
            observed.expect_terminals(&[
                success(nats, Connect),
                failed(
                    nats,
                    Publish,
                    IntegrationFailure::Cancelled,
                    Retryability::Safe,
                ),
                success(nats, Close),
            ]),
            observed.expect_one_instance(nats),
        ]),
    )
}

/// `url` with a user and [`NATS_PASSWORD`] in its userinfo.
fn credentialed(url: &str) -> String {
    url.replacen("nats://", &format!("nats://camber:{NATS_PASSWORD}@"), 1)
}

/// Connect, readiness, publish, subscribe, receive, and both closes each
/// settle into one successful terminal of the same instance. Neither the
/// password nor the payload reaches an event.
fn nats_operations_settle_once_each() -> Row {
    use IntegrationOperation::{Close, Connect, Publish, Ready, Receive, Subscribe};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let url = credentialed(&peer.url());
    let control = peer.control();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", nats::builder(&url).connect())?;
        expect_ok("ready", connection.ready())?;
        settled("publish", connection.publish(SUBJECT, PAYLOAD))?;
        let mut subscription = settled("subscribe", connection.subscribe(SUBJECT))?;
        control.deliver(&control.sid(SUBJECT, ROW_BOUND)?, SUBJECT, &[PAYLOAD])?;
        let delivered = settled("next", subscription.next())?;
        expect("the delivered message", delivered.is_some())?;
        expect_ok(
            "subscription close",
            bounded("close", subscription.close())?,
        )?;
        settled("close", connection.close())
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(nats, Connect),
            success(nats, Ready),
            success(nats, Publish),
            success(nats, Subscribe),
            success(nats, Receive),
            success(nats, Close).times(2),
        ]),
        observed.expect_one_instance(nats),
        observed.expect_redacted(&[NATS_PASSWORD, PAYLOAD_TEXT]),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// An unparsable URL carrying a password is one configuration terminal with
/// no instance, and its source never puts the password in the event.
fn nats_configuration_refusal_redacts_its_source() -> Row {
    let invalid = invalid_config(IntegrationOperation::Connect);
    let url = format!("nats://camber:{NATS_PASSWORD}@127.0.0.1:not-a-port");
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        expect_refused(
            "connect with an unparsable URL",
            bounded("connect", nats::connect(&url))?,
            invalid,
        )
    });
    let observed = observation.finish();
    all([
        clean_run(outcome),
        observed.expect_terminals(&[failed_as(IntegrationKind::Nats, invalid).before_admission()]),
        observed.expect_no_instance(IntegrationKind::Nats),
        observed.expect_redacted(&[NATS_PASSWORD]),
    ])
}

/// A connect refused at the live-integration limit is one `Busy` terminal
/// with no instance; no later operation is reached or counted.
fn nats_pre_admission_refusal_reaches_no_later_operation() -> Row {
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let url = peer.url();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let held = hold_live_slots(LIVE_LIMIT, IntegrationKind::Sqs)?;
        let refused = bounded("connect", nats::connect(&url))?;
        drop(held);
        expect_refused(
            "connect at the live limit",
            refused,
            busy(IntegrationOperation::Connect),
        )
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            failed_as(nats, busy(IntegrationOperation::Connect)).before_admission()
        ]),
        observed.expect_no_instance(nats),
        expect_unreached(
            &observed,
            nats,
            &[
                IntegrationOperation::Ready,
                IntegrationOperation::Publish,
                IntegrationOperation::Close,
            ],
        ),
        peer.control().expect_no_connection(),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A server authorization violation is one `PermissionDenied` connect
/// terminal.
fn nats_service_rejection_is_one_terminal() -> Row {
    let peer = NatsPeer::start();
    peer.control().script(NatsScript::DenyAuthorization);
    let url = credentialed(&peer.url());
    let denied = permission_denied(IntegrationOperation::Connect);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        expect_refused(
            "connect with denied authorization",
            bounded("connect", nats::connect(&url))?,
            denied,
        )
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[failed_as(IntegrationKind::Nats, denied)]),
        observed.expect_redacted(&[NATS_PASSWORD]),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Behind a stalled transport the held publish times out with an unknown
/// outcome, the next is `Busy`, and the close that cannot see the SDK's
/// closed event times out. Each is one terminal, the close is not repeated
/// when the runtime aggregate takes it, and the held publish's duration lies
/// between its deadline and the caller's own wait.
fn nats_timeout_and_unknown_outcome_are_bounded_once() -> Row {
    use IntegrationOperation::{Close, Connect, Publish};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url())
        .max_in_flight(1)
        .max_message_bytes(STALLED_PAYLOAD)
        .operation_timeout(EXHAUSTED)
        .shutdown_timeout(EXHAUSTED);
    let control = peer.control();
    let observation = Observation::start();
    let (waited, _) = run_observing(runtime::builder(), move || {
        let connection = settled("connect", builder.connect())?;
        exhaust_publish(&connection, &control)
    });
    let observed = observation.finish();
    let verdict = all([
        observed_verdict(waited)
            .and_then(|waited| expect_timeout_duration(&observed, nats, Publish, waited)),
        observed.expect_terminals(&[
            success(nats, Connect),
            failed_as(nats, busy(Publish)).before_admission(),
            failed(
                nats,
                Publish,
                IntegrationFailure::Timeout,
                Retryability::OutcomeUnknown,
            ),
            failed_as(nats, timed_out(Close)),
        ]),
        observed.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Observe submission before exhausting the publish and close deadlines.
fn exhaust_publish(connection: &Connection, control: &NatsControl) -> Result<Duration, String> {
    use IntegrationOperation::{Close, Publish};
    let payload = vec![0_u8; STALLED_PAYLOAD];
    let started = Instant::now();
    let held = hold_transport(connection, control, &payload)?;
    runtime::block_on(async {
        expect_refused(
            "publish past the operation limit",
            connection.publish("small", b"x").await,
            busy(Publish),
        )?;
        let answer = tokio::time::timeout(ROW_BOUND, held)
            .await
            .map_err(|_| "the held publish outlived its deadline".to_owned())?;
        let waited = started.elapsed();
        expect_refused(
            "the held publish",
            answer,
            (
                Publish,
                IntegrationFailure::Timeout,
                Retryability::OutcomeUnknown,
            ),
        )?;
        expect_refused(
            "close with no closed event",
            tokio::time::timeout(ROW_BOUND, connection.close())
                .await
                .map_err(|_| "close outlived its bound".to_owned())?,
            timed_out(Close),
        )?;
        Ok(waited)
    })
}

/// A publish whose waiter is dropped after submission settles once as
/// `Cancelled` with an unknown outcome; the aggregate that keeps its account
/// adds no second terminal.
fn nats_abandoned_result_is_one_terminal() -> Row {
    use IntegrationOperation::{Close, Connect, Publish};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_message_bytes(STALLED_PAYLOAD);
    let control = peer.control();
    let observation = Observation::start();
    let (verdict, _) = run_observing(runtime::builder(), move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let payload = vec![0_u8; STALLED_PAYLOAD];
        let big = hold_transport(&connection, &control, &payload)?;
        runtime::block_on(async {
            let mut dropped = Box::pin(connection.publish("dropped", b"x"));
            expect_polled_pending(
                "the publish behind the stall",
                &futures_util::poll!(dropped.as_mut()),
            )?;
            drop(dropped);
            control.script(NatsScript::Serve);
            expect_ok(
                "the stalled publish",
                tokio::time::timeout(ROW_BOUND, big)
                    .await
                    .map_err(|_| "the stalled publish never finished".to_owned())?,
            )
        })?;
        control.wait_for("the dropped publish on the wire", ROW_BOUND, |log| {
            log.published
                .iter()
                .any(|(subject, _)| &**subject == "dropped")
        })?;
        settled("close", connection.close())
    });
    let observed = observation.finish();
    let verdict = all([
        observed_verdict(verdict),
        observed.expect_terminals(&[
            success(nats, Connect),
            success(nats, Publish),
            failed_as(nats, cancelled(Publish)),
            success(nats, Close),
        ]),
        observed.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A connection still open when the runtime stops is closed by the stop: one
/// close terminal under shutdown, and none again when the escaped handle
/// drops afterwards.
fn nats_runtime_stop_closes_under_shutdown() -> Row {
    use IntegrationOperation::{Close, Connect};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || settled("connect", builder.connect()));
    let escaped = clean_run(outcome).map(drop);
    let observed = observation.finish();
    let verdict = all([
        escaped,
        observed.expect_terminals(&[
            success(nats, Connect),
            success(nats, Close).under_shutdown(),
        ]),
        observed.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Outside a Camber runtime the connect is one closed refusal terminal with
/// no instance and no duration, and the peer sees nothing.
fn nats_missing_runtime_refusal_is_one_terminal() -> Row {
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let url = peer.url();
    let observation = Observation::start();
    let answered = outside_camber(nats::connect(&url));
    let observed = observation.finish();
    let verdict = all([
        answered.and_then(|answer| expect_no_runtime("connect outside Camber", answer)),
        observed.expect_terminals(&[refused(nats, IntegrationOperation::Connect)]),
        observed.expect_no_instance(nats),
        peer.control().expect_no_connection(),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A connect after root admission closed is one closed refusal terminal with
/// no instance and no duration, and the peer sees nothing.
fn nats_closed_scope_refusal_is_one_terminal() -> Row {
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let url = peer.url();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        runtime::request_shutdown();
        expect_scope_closed(
            "connect after closure",
            bounded("connect", nats::connect(&url))?,
        )
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[refused(nats, IntegrationOperation::Connect)]),
        observed.expect_no_instance(nats),
        peer.control().expect_no_connection(),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A transport that closes every connection before the handshake fails the
/// admitted connect once as `Unavailable`, with one duration and no close.
fn nats_unavailable_peer_fails_connect_once() -> Row {
    let nats = IntegrationKind::Nats;
    let unavailable = unavailable(IntegrationOperation::Connect);
    let peer = NatsPeer::start();
    let control = peer.control();
    control.script(NatsScript::Refuse);
    let url = peer.url();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        expect_refused(
            "connect to a refusing transport",
            bounded("connect", nats::connect(&url))?,
            unavailable,
        )
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[failed_as(nats, unavailable)]),
        observed.expect_one_instance(nats),
        expect(
            "the peer saw no connection attempt",
            control.log().accepted >= 1,
        ),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A server that never answers the handshake holds the connect to its bound:
/// one `Timeout` terminal whose duration lies between that bound and the
/// caller's own wait.
fn nats_connect_timeout_is_bounded_once() -> Row {
    let nats = IntegrationKind::Nats;
    let timed_out = expired(IntegrationOperation::Connect);
    let peer = NatsPeer::start();
    peer.control().script(NatsScript::Silent);
    let builder = nats::builder(&peer.url()).connect_timeout(EXHAUSTED);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Result<Duration, String> {
        let (answer, waited) = timed("connect", builder.connect())?;
        expect_refused("connect to a silent server", answer, timed_out)?;
        Ok(waited)
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome).and_then(|waited| {
            expect_timeout_duration(&observed, nats, IntegrationOperation::Connect, waited)
        }),
        observed.expect_terminals(&[failed_as(nats, timed_out)]),
        observed.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// On an admitted connection, a payload over the maximum and every operation
/// after close are refused before admission: one terminal and one counter
/// each, no duration. The payload at the maximum publishes, and a repeated
/// close read adds no terminal.
fn nats_refusals_on_an_admitted_connection_have_no_duration() -> Row {
    use IntegrationOperation::{Close, Connect, Publish, Ready, Subscribe};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_message_bytes(SMALL_MAX);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        expect_refused(
            "a payload over the maximum",
            bounded("publish", connection.publish(SUBJECT, b"12345"))?,
            limit_exceeded(Publish),
        )?;
        expect_ok(
            "a payload at the maximum",
            bounded("publish", connection.publish(SUBJECT, b"1234"))?,
        )?;
        settled("close", connection.close())?;
        expect_ok("repeated close", bounded("close", connection.close())?)?;
        expect_refused("ready after close", connection.ready(), closed(Ready))?;
        expect_refused(
            "publish after close",
            bounded("publish", connection.publish(SUBJECT, b"x"))?,
            closed(Publish),
        )?;
        expect_refused(
            "subscribe after close",
            bounded("subscribe", connection.subscribe(SUBJECT))?,
            closed(Subscribe),
        )
    });
    let observed = observation.finish();
    let published = peer
        .control()
        .wait_for("the publish at the maximum", ROW_BOUND, |log| {
            !log.published.is_empty()
        })
        .map(|log| log.published);
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(nats, Connect),
            failed_as(nats, limit_exceeded(Publish)).before_admission(),
            success(nats, Publish),
            success(nats, Close),
            failed_as(nats, closed(Ready)).before_admission(),
            failed_as(nats, closed(Publish)).before_admission(),
            failed_as(nats, closed(Subscribe)).before_admission(),
        ]),
        observed.expect_one_instance(nats),
        published.and_then(|published| {
            expect_eq(
                "publishes the peer read",
                published,
                vec![(Box::from(SUBJECT), SMALL_MAX)],
            )
        }),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// While the SDK reports the transport gone, readiness, publish, and
/// subscribe are each one `Unavailable` refusal with no duration. The close
/// that cannot reach a refusing peer is one terminal under either committed
/// result.
fn nats_disconnected_refusals_are_unavailable_once_each() -> Row {
    use IntegrationFailure::Timeout;
    use IntegrationOperation::{Close, Connect, Publish, Ready, Subscribe};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).shutdown_timeout(EXHAUSTED);
    let control = peer.control();
    let observation = Observation::start();
    let (seen, _) = run_observing(runtime::builder(), move || -> Result<usize, String> {
        let connection = settled("connect", builder.connect())?;
        control.script(NatsScript::Refuse);
        let connected_polls =
            ready_polls_until("the SDK disconnect", &connection, unavailable(Ready))?;
        expect_refused(
            "publish while disconnected",
            bounded("publish", connection.publish(SUBJECT, PAYLOAD))?,
            unavailable(Publish),
        )?;
        expect_refused(
            "subscribe while disconnected",
            bounded("subscribe", connection.subscribe(SUBJECT))?,
            unavailable(Subscribe),
        )?;
        expect_close_or_timeout(bounded("close", connection.close())?)?;
        Ok(connected_polls)
    });
    let observed = observation.finish();
    let seen = observed_verdict(seen);
    let connected_polls = seen.as_ref().map_or(0, |polls| *polls);
    let log = peer.control().log();
    let verdict = all([
        seen.map(drop),
        observed.expect_terminals(&[
            success(nats, Connect),
            success(nats, Ready).times(connected_polls),
            failed_as(nats, unavailable(Ready)).before_admission(),
            failed_as(nats, unavailable(Publish)).before_admission(),
            failed_as(nats, unavailable(Subscribe)).before_admission(),
            success(nats, Close).or(Timeout),
        ]),
        observed.expect_one_instance(nats),
        expect_eq("publishes the peer read", log.published.len(), 0),
        expect_eq("subscriptions the peer read", log.subscribed.len(), 0),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A close that cannot reach its peer either commits success or reports its
/// bound passed; both are valid unordered settlements.
fn expect_close_or_timeout(closed: Result<(), RuntimeError>) -> Row {
    match closed {
        Ok(()) => Ok(()),
        Err(error) => expect_eq(
            "close of an unreachable peer",
            refusal(&error),
            Some(timed_out(IntegrationOperation::Close)),
        ),
    }
}

/// The subscription limit refuses once as `Busy` with no duration. Closing
/// the subscription and then the connection are one terminal each, and a
/// repeated subscription close read adds none.
fn nats_subscription_admission_and_close_settle_once() -> Row {
    use IntegrationOperation::{Close, Connect, Subscribe};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_subscriptions(1);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut first = settled("subscribe", connection.subscribe(SUBJECT))?;
        expect_refused(
            "subscribe past the limit",
            bounded("subscribe", connection.queue_subscribe("b", "workers"))?,
            busy(Subscribe),
        )?;
        expect_ok("subscription close", bounded("close", first.close())?)?;
        expect_ok(
            "repeated subscription close",
            bounded("close", first.close())?,
        )?;
        settled("close", connection.close())
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(nats, Connect),
            success(nats, Subscribe),
            failed_as(nats, busy(Subscribe)).before_admission(),
            success(nats, Close).times(2),
        ]),
        observed.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Whether the next receive delivered an item, or the typed refusal.
fn next_delivered(
    subscription: &mut Subscription,
) -> Result<Result<bool, Option<Refusal>>, String> {
    Ok(bounded("next", subscription.next())?
        .map(|message| message.is_some())
        .map_err(|error| refusal(&error)))
}

/// Every receive call is one terminal: an empty poll, a delivered item, a
/// passed receive bound, an oversized delivery, a completed stream, and a
/// poll of a closed subscription. The closed poll is a refusal with no
/// duration.
fn nats_receive_outcomes_settle_once_per_call() -> Row {
    use IntegrationOperation::{Close, Connect, Receive, Subscribe};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_message_bytes(SMALL_MAX);
    let control = peer.control();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut subscription = settled("subscribe", connection.subscribe(SUBJECT))?;
        let sid = control.sid(SUBJECT, ROW_BOUND)?;
        expect_eq(
            "an empty poll",
            subscription
                .try_next()
                .map(|message| message.is_some())
                .map_err(|error| refusal(&error)),
            Ok(false),
        )?;
        control.deliver(&sid, SUBJECT, &[b"one"])?;
        expect_eq(
            "the delivered item",
            next_delivered(&mut subscription)?,
            Ok(true),
        )?;
        expect_refused(
            "a receive past its bound",
            bounded("next_timeout", subscription.next_timeout(QUIET))?,
            expired(Receive),
        )?;
        control.deliver(&sid, SUBJECT, &[b"12345"])?;
        expect_refused(
            "an inbound payload over the maximum",
            bounded("next", subscription.next())?,
            limit_exceeded(Receive),
        )?;
        expect_ok(
            "subscription close",
            bounded("close", subscription.close())?,
        )?;
        expect_eq(
            "the completed stream",
            next_delivered(&mut subscription)?,
            Ok(false),
        )?;
        expect_refused(
            "a poll of the closed subscription",
            subscription.try_next(),
            closed(Receive),
        )?;
        settled("close", connection.close())
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(nats, Connect),
            success(nats, Subscribe),
            success(nats, Receive).times(3),
            failed_as(nats, expired(Receive)),
            failed_as(nats, limit_exceeded(Receive)),
            failed_as(nats, closed(Receive)).before_admission(),
            success(nats, Close).times(2),
        ]),
        observed.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Take messages until `subscription` completes, counting every receive
/// call, the completing one included.
fn receives_until_complete(subscription: &mut Subscription) -> Result<usize, String> {
    let mut calls = 0_usize;
    loop {
        calls += 1;
        match next_delivered(subscription)? {
            Ok(true) => {}
            Ok(false) => return Ok(calls),
            Err(refused) => return Err(format!("the subscription failed: {refused:?}")),
        }
    }
}

/// A delivered slow-consumer event settles the connection once: one
/// `LimitExceeded` receive terminal, no close terminal for either close
/// read, and none again when the runtime aggregate takes the account.
fn nats_slow_consumer_closure_is_one_terminal() -> Row {
    use IntegrationOperation::{Close, Connect, Receive, Subscribe};
    let nats = IntegrationKind::Nats;
    let slow_consumer = limit_exceeded(Receive);
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).subscription_capacity(FLOODED_CAPACITY);
    let control = peer.control();
    let observation = Observation::start();
    let (seen, teardown) = run_observing(runtime::builder(), move || -> Result<usize, String> {
        let connection = settled("connect", builder.connect())?;
        let mut flooded = settled("subscribe", connection.subscribe(SUBJECT))?;
        let flood: [&[u8]; 6] = [b"1", b"2", b"3", b"4", b"5", b"6"];
        control.deliver(&control.sid(SUBJECT, ROW_BOUND)?, SUBJECT, &flood)?;
        NatsEventProbe::deliver_slow_consumer(&connection);
        let receives = receives_until_complete(&mut flooded)?;
        expect_refused(
            "close after the event",
            bounded("close", connection.close())?,
            slow_consumer,
        )?;
        expect_refused(
            "a second close",
            bounded("close", connection.close())?,
            slow_consumer,
        )?;
        Ok(receives)
    });
    let observed = observation.finish();
    let seen = observed_verdict(seen);
    let receives = seen.as_ref().map_or(0, |receives| *receives);
    let verdict = all([
        seen.map(drop),
        expect_aggregate_failures(teardown, nats, &[slow_consumer]),
        observed.expect_terminals(&[
            success(nats, Connect),
            success(nats, Subscribe),
            success(nats, Receive).times(receives),
            failed_as(nats, slow_consumer),
        ]),
        observed.expect_absent(nats, Close),
        observed.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Two concurrent closes and a repeated close read settle one close
/// terminal, and an escaped clone's publish after it is one `Closed` refusal
/// with no duration.
fn nats_concurrent_close_settles_once() -> Row {
    use IntegrationOperation::{Close, Connect, Publish};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let escaped = connection.clone();
        let (first, second) = bounded("concurrent close", async {
            tokio::join!(connection.close(), escaped.close())
        })?;
        expect_ok("first close", first)?;
        expect_ok("concurrent close", second)?;
        expect_ok("repeated close", bounded("close", connection.close())?)?;
        expect_refused(
            "publish on the escaped clone",
            bounded("publish", escaped.publish(SUBJECT, PAYLOAD))?,
            closed(Publish),
        )
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(nats, Connect),
            success(nats, Close),
            failed_as(nats, closed(Publish)).before_admission(),
        ]),
        observed.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Dropping the last handle while the runtime runs is one close terminal,
/// not under shutdown, and the escaped subscription's completing receive is
/// one more.
fn nats_last_handle_drop_closes_once() -> Row {
    use IntegrationOperation::{Close, Connect, Receive, Subscribe};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let control = peer.control();
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut subscription = settled("subscribe", connection.subscribe(SUBJECT))?;
        let sid = control.sid(SUBJECT, ROW_BOUND)?;
        drop(connection);
        control.wait_for("the client's close", ROW_BOUND, |log| {
            log.closed >= 1 && log.unsubscribed.contains(&sid)
        })?;
        expect_eq(
            "the subscription after the last handle dropped",
            next_delivered(&mut subscription)?,
            Ok(false),
        )
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(nats, Connect),
            success(nats, Subscribe),
            success(nats, Receive),
            success(nats, Close),
        ]),
        observed.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A publish the SDK holds unflushed when the runtime stops settles once
/// under shutdown with an unknown outcome, and the stop's close is one
/// terminal under shutdown; neither repeats when the aggregate takes them.
fn nats_publish_held_at_runtime_stop_settles_under_shutdown() -> Row {
    use IntegrationFailure::Timeout;
    use IntegrationOperation::{Close, Connect, Publish};
    let nats = IntegrationKind::Nats;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_message_bytes(STALLED_PAYLOAD);
    let control = peer.control();
    let observation = Observation::start();
    let (seen, _) = run_observing(runtime::builder().shutdown_timeout(EXHAUSTED), move || {
        let connection = settled("connect", builder.connect())?;
        let payload = vec![0_u8; STALLED_PAYLOAD];
        control.script(NatsScript::Stall);
        let mut held = Box::pin(async move { connection.publish("big", &payload).await });
        expect_stalled_publish(&control, held.as_mut())?;
        runtime::request_shutdown();
        Ok(held)
    });
    let seen = observed_verdict(seen)
        .and_then(outside_camber)
        .and_then(expect_held_publish_stopped);
    let observed = observation.finish();
    let verdict = all([
        seen,
        observed.expect_terminals(&[
            success(nats, Connect),
            failed_as(nats, cancelled(Publish))
                .or(Timeout)
                .under_shutdown(),
            success(nats, Close).or(Timeout).under_shutdown(),
        ]),
        observed.expect_one_instance(nats),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A submitted publish the stop settles is cancelled or timed out, and its
/// outcome is unknown either way: the server may have the message.
fn expect_held_publish_stopped(answer: Result<(), RuntimeError>) -> Row {
    let stopped = match answer {
        Ok(()) => return Err("the held publish succeeded behind a stalled peer".to_owned()),
        Err(error) => refusal(&error),
    };
    expect(
        &format!("the held publish settled as {stopped:?}"),
        matches!(
            stopped,
            Some((
                IntegrationOperation::Publish,
                IntegrationFailure::Cancelled | IntegrationFailure::Timeout,
                Retryability::OutcomeUnknown,
            ))
        ),
    )
}
