//! SQS operation terminal events through the SDK transport.
//!
//! Every row enters through the public `camber::mq::sqs` API. The real AWS SDK
//! sends each request over loopback HTTP to the scripted peer, which records
//! every request it reads in full. A row expects an unknown outcome only after
//! the peer's record proves the request was submitted.

use crate::event_rows::{
    EXHAUSTED, SMALL_MAX, expect_clean_teardown, expect_timeout_duration, expect_unreached,
    failed_as, run_terminal_rows, timed,
};
use crate::integration_events::{
    Observation, ROW_BOUND, bounded, expect_aggregate_failures, outside_camber, refused, settled,
    success,
};
use crate::integration_rows::{
    LIVE_LIMIT, NamedRow, Row, all, busy, cancelled, clean_run, closed, expect, expect_eq,
    expect_no_runtime, expect_ok, expect_pending, expect_refused, expect_scope_closed, expired,
    hold_live_slots, invalid_config, limit_exceeded, observed_verdict, permission_denied, rejected,
    run_observing, unknown,
};
use crate::sqs_peer::{PeerControl, Reply, SECRET_KEY, SqsPeer};
use camber::runtime_test_support::{SqsCredentialLoads, SqsCredentialProbe};
use camber::{IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability, runtime};
use std::time::Duration;

/// The SQS queue every SQS row addresses.
const QUEUE: &str = "terminal-events";

/// An SQS message body no event may repeat.
const SQS_BODY: &str = "sqs-body-5a7c";

/// The receipt handle the scripted peer hands the first delivered message.
const RECEIPT: &str = "peer-receipt-0";

/// A secret a refused configuration carries; no event may repeat it.
const CONFIG_SECRET: &str = "sqs-config-secret-31d8";

/// What no event may repeat: the signing secret, a body, and the queue.
const REDACTED: [&str; 3] = [SECRET_KEY, SQS_BODY, QUEUE];

#[test]
fn sqs_terminals_match_events_counters_and_durations() {
    run_terminal_rows(
        "sqs_events::sqs_terminals_match_events_counters_and_durations",
        "sqs-terminal-events",
        "SQS_TERMINAL_EVENTS_COMPLETE",
        "M9 SQS terminal event or metric contract is missing",
        TERMINAL_ROWS,
    );
}

const TERMINAL_ROWS: &[NamedRow<'static>] = &[
    (
        "sqs operations settle once each",
        sqs_operations_settle_once_each,
    ),
    (
        "sqs rejection unknown outcome and pre submission refusal",
        sqs_rejection_unknown_outcome_and_pre_submission_refusal,
    ),
    (
        "sqs configuration refusals redact their source",
        sqs_configuration_refusals_redact_their_source,
    ),
    (
        "sqs missing runtime refusal is one terminal",
        sqs_missing_runtime_refusal_is_one_terminal,
    ),
    (
        "sqs closed scope refusal is one terminal",
        sqs_closed_scope_refusal_is_one_terminal,
    ),
    (
        "sqs live limit refusal reaches no later operation",
        sqs_live_limit_refusal_reaches_no_later_operation,
    ),
    (
        "sqs connect timeout is bounded once",
        sqs_connect_timeout_is_bounded_once,
    ),
    (
        "sqs readiness denial and timeout settle once each",
        sqs_readiness_denial_and_timeout_settle_once_each,
    ),
    (
        "sqs input refusals have no duration",
        sqs_input_refusals_have_no_duration,
    ),
    (
        "sqs delete outcomes settle once each",
        sqs_delete_outcomes_settle_once_each,
    ),
    (
        "sqs operation limit refusals have no duration",
        sqs_operation_limit_refusals_have_no_duration,
    ),
    (
        "sqs submitted timeouts are bounded once",
        sqs_submitted_timeouts_are_bounded_once,
    ),
    (
        "sqs receive outcomes settle once per call",
        sqs_receive_outcomes_settle_once_per_call,
    ),
    (
        "sqs abandoned results are one terminal each",
        sqs_abandoned_results_are_one_terminal_each,
    ),
    (
        "sqs unsubmitted work settles safe under a runtime stop",
        sqs_unsubmitted_work_settles_safe_under_a_runtime_stop,
    ),
    (
        "sqs concurrent close settles once",
        sqs_concurrent_close_settles_once,
    ),
    (
        "sqs runtime stop closes under shutdown",
        sqs_runtime_stop_closes_under_shutdown,
    ),
    (
        "sqs last handle drop closes once",
        sqs_last_handle_drop_closes_once,
    ),
];

/// Wait until the credential source was asked for `count` loads: an
/// operation past connect's load stalls there, before it can sign. A source
/// dropped first ends the wait short of `count`, which fails the row.
fn wait_loads(loads: &mut SqsCredentialLoads, count: usize) -> Row {
    bounded("the credential loads", loads.reached(count))?;
    expect(
        &format!("the credential source was never asked for {count} loads"),
        loads.asked() >= count,
    )
}

/// Fail the row unless neither the peer nor the credential source saw an
/// effect.
fn expect_untouched(control: &PeerControl, loads: &SqsCredentialLoads) -> Row {
    all([
        control.expect_no_connection(),
        expect_eq("credential loads", loads.asked(), 0),
    ])
}

/// Connect, readiness, send, receive, delete, and close each settle into one
/// successful terminal of the same instance. Neither the secret key, the
/// body, nor the receipt handle reaches an event.
fn sqs_operations_settle_once_each() -> Row {
    use IntegrationOperation::{Close, Connect, Delete, Publish, Ready, Receive};
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        settled("ready", client.ready(&queue))?;
        settled("send", client.send_message(&queue, SQS_BODY))?;
        control.script([Reply::messages(&[SQS_BODY])]);
        let received = settled(
            "receive",
            client.receive_messages(&queue, 1, Duration::ZERO),
        )?;
        let receipt = received
            .first()
            .and_then(|message| message.receipt_handle())
            .ok_or("the received message carried no receipt")?;
        expect_eq("the delivered receipt", receipt, RECEIPT)?;
        settled("delete", client.delete_message(&queue, receipt))?;
        settled("close", client.close())
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(sqs, Connect),
            success(sqs, Ready),
            success(sqs, Publish),
            success(sqs, Receive),
            success(sqs, Delete),
            success(sqs, Close),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        observed.expect_redacted(&[RECEIPT]),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// An explicit denial, a lost answer, and a receive refused before
/// submission are one terminal each; the refused receive sends nothing and
/// records no duration.
fn sqs_rejection_unknown_outcome_and_pre_submission_refusal() -> Row {
    use IntegrationOperation::{Close, Connect, Publish, Receive};
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([Reply::error(403, "AccessDenied"), Reply::Drop]);
    let observation = Observation::start();
    let (verdict, teardown) = run_observing(runtime::builder(), move || -> Row {
        let client = settled("connect", builder.connect())?;
        expect_refused(
            "a denied send",
            bounded("send", client.send_message(&queue, SQS_BODY))?,
            permission_denied(Publish),
        )?;
        let lost = bounded("send", client.send_message(&queue, SQS_BODY))?;
        control.wait_for("the lost send read in full", ROW_BOUND, |log| {
            log.count("SendMessage") == 2
        })?;
        expect_refused("a send whose answer was lost", lost, unknown(Publish))?;
        expect_refused(
            "a receive over the batch bound",
            bounded(
                "receive",
                client.receive_messages(&queue, 11, Duration::ZERO),
            )?,
            rejected(Receive),
        )?;
        settled("close", client.close())
    });
    let observed = observation.finish();
    let log = peer.control().log();
    let verdict = all([
        observed_verdict(verdict),
        expect_clean_teardown(teardown),
        observed.expect_terminals(&[
            success(sqs, Connect),
            failed_as(sqs, permission_denied(Publish)),
            failed_as(sqs, unknown(Publish)),
            failed_as(sqs, rejected(Receive)).before_admission(),
            success(sqs, Close),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        expect_eq("send requests", log.count("SendMessage"), 2),
        expect_eq("receive requests", log.count("ReceiveMessage"), 0),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Each invalid configuration is one `InvalidConfig` connect terminal with
/// no instance and no duration, and its source never puts the secret it
/// carried in the event.
fn sqs_configuration_refusals_redact_their_source() -> Row {
    let peer = SqsPeer::start();
    let control = peer.control();
    let invalid = [
        peer.builder()
            .endpoint(&format!("http://camber:{CONFIG_SECRET}@127.0.0.1:1")),
        peer.builder().credentials("", CONFIG_SECRET, None),
        peer.builder().max_in_flight(0),
    ];
    let attempts = invalid.len();
    let refusal = invalid_config(IntegrationOperation::Connect);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        all(invalid.into_iter().map(|builder| {
            expect_refused(
                "connect with an invalid configuration",
                bounded("connect", builder.connect())?,
                refusal,
            )
        }))
    });
    let observed = observation.finish();
    let sqs = IntegrationKind::Sqs;
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[failed_as(sqs, refusal).times(attempts).before_admission()]),
        observed.expect_no_instance(sqs),
        observed.expect_redacted(&[CONFIG_SECRET]),
        control.expect_no_connection(),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Outside a Camber runtime the connect is one closed refusal terminal with
/// no instance and no duration; neither the peer nor the credential source
/// sees anything.
fn sqs_missing_runtime_refusal_is_one_terminal() -> Row {
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let (builder, loads) = SqsCredentialProbe::answers_once(peer.builder());
    let observation = Observation::start();
    let answered = outside_camber(builder.connect());
    let observed = observation.finish();
    let verdict = all([
        answered.and_then(|answer| expect_no_runtime("connect outside Camber", answer)),
        observed.expect_terminals(&[refused(sqs, IntegrationOperation::Connect)]),
        observed.expect_no_instance(sqs),
        observed.expect_redacted(&REDACTED),
        expect_untouched(&peer.control(), &loads),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A connect after root admission closed is one closed refusal terminal with
/// no instance and no duration; neither the peer nor the credential source
/// sees anything.
fn sqs_closed_scope_refusal_is_one_terminal() -> Row {
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let (builder, loads) = SqsCredentialProbe::answers_once(peer.builder());
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        runtime::request_shutdown();
        expect_scope_closed(
            "connect after closure",
            bounded("connect", builder.connect())?,
        )
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[refused(sqs, IntegrationOperation::Connect)]),
        observed.expect_no_instance(sqs),
        observed.expect_redacted(&REDACTED),
        expect_untouched(&peer.control(), &loads),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A connect refused at the live-integration limit is one `Busy` terminal
/// with no instance and no duration; no later operation is reached or
/// counted, and neither the peer nor the credential source sees anything.
fn sqs_live_limit_refusal_reaches_no_later_operation() -> Row {
    use IntegrationOperation::{Close, Connect, Publish, Ready};
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let (builder, loads) = SqsCredentialProbe::answers_once(peer.builder());
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let held = hold_live_slots(LIVE_LIMIT, IntegrationKind::Nats)?;
        let refused = bounded("connect", builder.connect())?;
        drop(held);
        expect_refused("connect at the live limit", refused, busy(Connect))
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[failed_as(sqs, busy(Connect)).before_admission()]),
        observed.expect_no_instance(sqs),
        expect_unreached(&observed, sqs, &[Ready, Publish, Close]),
        observed.expect_redacted(&REDACTED),
        expect_untouched(&peer.control(), &loads),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A credential source that never answers holds the admitted connect to its
/// bound: one `Timeout` terminal with an instance, whose duration lies
/// between that bound and the caller's own wait, and no close terminal.
fn sqs_connect_timeout_is_bounded_once() -> Row {
    use IntegrationOperation::{Close, Connect};
    let sqs = IntegrationKind::Sqs;
    let timed_out = expired(Connect);
    let peer = SqsPeer::start();
    let builder = SqsCredentialProbe::unresolved(peer.builder().connect_timeout(EXHAUSTED));
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Result<Duration, String> {
        let (answer, waited) = timed("connect", builder.connect())?;
        expect_refused("connect with unresolved credentials", answer, timed_out)?;
        Ok(waited)
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome)
            .and_then(|waited| expect_timeout_duration(&observed, sqs, Connect, waited)),
        observed.expect_terminals(&[failed_as(sqs, timed_out)]),
        observed.expect_absent(sqs, Close),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        peer.control().expect_no_connection(),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A denied queue query and one the peer holds past the operation deadline
/// are one readiness terminal each. The held query is read-only, so its
/// timeout is safe, and its duration lies between the deadline and the
/// caller's own wait.
fn sqs_readiness_denial_and_timeout_settle_once_each() -> Row {
    use IntegrationOperation::{Close, Connect, Ready};
    let sqs = IntegrationKind::Sqs;
    let held = expired(Ready);
    let peer = SqsPeer::start();
    let builder = peer.builder().operation_timeout(EXHAUSTED);
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([Reply::error(403, "AccessDenied"), Reply::Hold]);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Result<Duration, String> {
        let client = settled("connect", builder.connect())?;
        expect_refused(
            "a denied queue query",
            bounded("ready", client.ready(&queue))?,
            permission_denied(Ready),
        )?;
        let (answer, waited) = timed("ready", client.ready(&queue))?;
        control.wait_held(1, ROW_BOUND)?;
        expect_refused("a held queue query", answer, held)?;
        settled("close", client.close())?;
        Ok(waited)
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome)
            .and_then(|waited| expect_timeout_duration(&observed, sqs, Ready, waited)),
        observed.expect_terminals(&[
            success(sqs, Connect),
            failed_as(sqs, permission_denied(Ready)),
            failed_as(sqs, held),
            success(sqs, Close),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A body over the maximum and receive parameters out of range are refused
/// before admission: one terminal each, no duration, and nothing sent. The
/// body at the maximum is sent.
fn sqs_input_refusals_have_no_duration() -> Row {
    use IntegrationOperation::{Close, Connect, Publish, Receive};
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let builder = peer.builder().max_message_bytes(SMALL_MAX);
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    let out_of_range = [
        (0, Duration::ZERO),
        (11, Duration::ZERO),
        (1, Duration::from_secs(21)),
    ];
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        expect_refused(
            "a body over the maximum",
            bounded("send", client.send_message(&queue, "12345"))?,
            limit_exceeded(Publish),
        )?;
        expect_ok(
            "a body at the maximum",
            bounded("send", client.send_message(&queue, "1234"))?,
        )?;
        for (max_messages, wait) in out_of_range {
            expect_refused(
                &format!("a receive of {max_messages} waiting {wait:?}"),
                bounded(
                    "receive",
                    client.receive_messages(&queue, max_messages, wait),
                )?,
                rejected(Receive),
            )?;
        }
        settled("close", client.close())
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(sqs, Connect),
            failed_as(sqs, limit_exceeded(Publish)).before_admission(),
            success(sqs, Publish),
            failed_as(sqs, rejected(Receive))
                .times(out_of_range.len())
                .before_admission(),
            success(sqs, Close),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        control.log().expect_requests(&["SendMessage"]),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// An acknowledged delete, a denied one, a stale receipt, and a delete whose
/// answer was lost after the peer read it are one admitted terminal each.
fn sqs_delete_outcomes_settle_once_each() -> Row {
    use IntegrationOperation::{Close, Connect, Delete};
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([
        Reply::Serve,
        Reply::error(403, "AccessDenied"),
        Reply::error(400, "ReceiptHandleIsInvalid"),
        Reply::Drop,
    ]);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let delete = || bounded("delete", client.delete_message(&queue, RECEIPT));
        expect_ok("an acknowledged delete", delete()?)?;
        expect_refused("a denied delete", delete()?, permission_denied(Delete))?;
        expect_refused("a stale receipt", delete()?, rejected(Delete))?;
        let lost = delete()?;
        control.wait_for("the lost delete read in full", ROW_BOUND, |log| {
            log.count("DeleteMessage") == 4
        })?;
        expect_refused("a delete whose answer was lost", lost, unknown(Delete))?;
        settled("close", client.close())
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(sqs, Connect),
            success(sqs, Delete),
            failed_as(sqs, permission_denied(Delete)),
            failed_as(sqs, rejected(Delete)),
            failed_as(sqs, unknown(Delete)),
            success(sqs, Close),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        observed.expect_redacted(&[RECEIPT]),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// With one operation slot held by a submitted send, a send, a receive, and
/// a delete are each one `Busy` refusal with no duration and nothing sent.
/// The close then cuts the held send, which was submitted, so its outcome is
/// unknown.
fn sqs_operation_limit_refusals_have_no_duration() -> Row {
    use IntegrationOperation::{Close, Connect, Delete, Publish, Receive};
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let builder = peer.held_until_close().max_in_flight(1);
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([Reply::Hold]);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let mut held = Box::pin(client.send_message(&queue, SQS_BODY));
        expect_pending("the held send", held.as_mut())?;
        control.wait_held(1, ROW_BOUND)?;
        expect_refused(
            "send past the operation limit",
            bounded("send", client.send_message(&queue, SQS_BODY))?,
            busy(Publish),
        )?;
        expect_refused(
            "receive past the operation limit",
            bounded(
                "receive",
                client.receive_messages(&queue, 1, Duration::ZERO),
            )?,
            busy(Receive),
        )?;
        expect_refused(
            "delete past the operation limit",
            bounded("delete", client.delete_message(&queue, RECEIPT))?,
            busy(Delete),
        )?;
        let (sent, closed) = bounded("the close cut", async {
            tokio::join!(held, client.close())
        })?;
        expect_refused("the cut send", sent, cancelled(Publish))?;
        expect_ok("close", closed)
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(sqs, Connect),
            failed_as(sqs, busy(Publish)).before_admission(),
            failed_as(sqs, busy(Receive)).before_admission(),
            failed_as(sqs, busy(Delete)).before_admission(),
            failed_as(sqs, cancelled(Publish)),
            success(sqs, Close),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        peer.control().log().expect_requests(&["SendMessage"]),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A send and a receive the peer holds past the operation deadline were
/// read in full, so each times out once with an unknown outcome. An answered
/// query first opens the one pooled connection, so each held request needs
/// only one write before its deadline.
fn sqs_submitted_timeouts_are_bounded_once() -> Row {
    use IntegrationOperation::{Close, Connect, Publish, Ready, Receive};
    let sqs = IntegrationKind::Sqs;
    let submitted = |operation| {
        (
            operation,
            IntegrationFailure::Timeout,
            Retryability::OutcomeUnknown,
        )
    };
    let peer = SqsPeer::start();
    let builder = peer.builder().operation_timeout(EXHAUSTED);
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Result<[Duration; 2], String> {
        let client = settled("connect", builder.connect())?;
        expect_ok("the warming query", bounded("ready", client.ready(&queue))?)?;
        control.script([Reply::Hold]);
        let (sent, send_waited) = timed("send", client.send_message(&queue, SQS_BODY))?;
        control.wait_held(1, ROW_BOUND)?;
        expect_refused("the held send", sent, submitted(Publish))?;
        control.script([Reply::Hold]);
        let (received, receive_waited) = timed(
            "receive",
            client.receive_messages(&queue, 1, Duration::ZERO),
        )?;
        control.wait_held(2, ROW_BOUND)?;
        expect_refused("the held receive", received, submitted(Receive))?;
        settled("close", client.close())?;
        Ok([send_waited, receive_waited])
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome).and_then(|[send_waited, receive_waited]| {
            all([
                expect_timeout_duration(&observed, sqs, Publish, send_waited),
                expect_timeout_duration(&observed, sqs, Receive, receive_waited),
            ])
        }),
        observed.expect_terminals(&[
            success(sqs, Connect),
            success(sqs, Ready),
            failed_as(sqs, submitted(Publish)),
            failed_as(sqs, submitted(Receive)),
            success(sqs, Close),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Every receive call is one terminal, never one per message: a batch of
/// three, an empty batch, a batch larger than asked for, a body over the
/// maximum, and an unreadable answer, which the service may have acted on.
/// The receive happened each time, so the failures are admitted and record a
/// duration; nothing is deleted.
fn sqs_receive_outcomes_settle_once_per_call() -> Row {
    use IntegrationOperation::{Close, Connect, Receive};
    let sqs = IntegrationKind::Sqs;
    let unreadable = (
        Receive,
        IntegrationFailure::Unavailable,
        Retryability::OutcomeUnknown,
    );
    let peer = SqsPeer::start();
    let builder = peer.builder().max_message_bytes(SMALL_MAX);
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([
        Reply::messages(&["a", "b", "c"]),
        Reply::Serve,
        Reply::messages(&["a", "b", "c"]),
        Reply::messages(&["12345"]),
        Reply::Json(200, "{\"Messages\":".into()),
    ]);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let receive = |max_messages| {
            bounded(
                "receive",
                client.receive_messages(&queue, max_messages, Duration::ZERO),
            )
        };
        let batch = expect_ok("a batch of three", receive(3)?)?;
        expect_eq("messages in the batch", batch.len(), 3)?;
        let empty = expect_ok("an empty batch", receive(3)?)?;
        expect_eq("messages in the empty batch", empty.len(), 0)?;
        expect_refused(
            "a batch larger than asked for",
            receive(2)?,
            limit_exceeded(Receive),
        )?;
        expect_refused(
            "a body over the maximum",
            receive(1)?,
            limit_exceeded(Receive),
        )?;
        expect_refused("an unreadable answer", receive(1)?, unreadable)?;
        settled("close", client.close())
    });
    let observed = observation.finish();
    let log = peer.control().log();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(sqs, Connect),
            success(sqs, Receive).times(2),
            failed_as(sqs, limit_exceeded(Receive)).times(2),
            failed_as(sqs, unreadable),
            success(sqs, Close),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        expect_eq("receive requests", log.count("ReceiveMessage"), 5),
        expect_eq("delete requests", log.count("DeleteMessage"), 0),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A send, a delete, and a receive whose waiters drop after the peer read
/// them settle once each as `Cancelled` with an unknown outcome. The runtime
/// aggregate that keeps those accounts adds no second terminal. A receive the
/// peer holds when the close cuts it settles the same way, returns its answer
/// to its own caller, and leaves no account. Nothing is deleted after the
/// receives.
fn sqs_abandoned_results_are_one_terminal_each() -> Row {
    use IntegrationOperation::{Close, Connect, Delete, Publish, Receive};
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let builder = peer.held_until_close();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([Reply::Hold, Reply::Hold, Reply::Hold, Reply::Hold]);
    let observation = Observation::start();
    let (verdict, teardown) = run_observing(runtime::builder(), move || -> Row {
        let client = settled("connect", builder.connect())?;
        let mut send = Box::pin(client.send_message(&queue, SQS_BODY));
        expect_pending("the held send", send.as_mut())?;
        control.wait_held(1, ROW_BOUND)?;
        drop(send);
        let mut delete = Box::pin(client.delete_message(&queue, RECEIPT));
        expect_pending("the held delete", delete.as_mut())?;
        control.wait_held(2, ROW_BOUND)?;
        drop(delete);
        let mut dropped = Box::pin(client.receive_messages(&queue, 1, Duration::ZERO));
        expect_pending("the held receive", dropped.as_mut())?;
        control.wait_held(3, ROW_BOUND)?;
        drop(dropped);
        let mut cut = Box::pin(client.receive_messages(&queue, 1, Duration::ZERO));
        expect_pending("the held receive", cut.as_mut())?;
        control.wait_held(4, ROW_BOUND)?;
        let (received, closed) =
            bounded("the close cut", async { tokio::join!(cut, client.close()) })?;
        expect_refused("the cut receive", received, cancelled(Receive))?;
        expect_ok("close", closed)
    });
    let observed = observation.finish();
    let verdict = all([
        observed_verdict(verdict),
        expect_aggregate_failures(
            teardown,
            sqs,
            &[cancelled(Publish), cancelled(Delete), cancelled(Receive)],
        ),
        observed.expect_terminals(&[
            success(sqs, Connect),
            failed_as(sqs, cancelled(Publish)),
            failed_as(sqs, cancelled(Delete)),
            failed_as(sqs, cancelled(Receive)).times(2),
            success(sqs, Close),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        peer.control().log().expect_requests(&[
            "SendMessage",
            "DeleteMessage",
            "ReceiveMessage",
            "ReceiveMessage",
        ]),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Work stalled before it could sign was never submitted. A waiter dropped
/// then, and a send, a delete, and a receive the runtime stop's close cuts,
/// each settle once as a safe `Cancelled`. Operations after the stop are
/// `Closed` refusals with no duration, the stop's close is one terminal under
/// shutdown, and the runtime keeps no account.
fn sqs_unsubmitted_work_settles_safe_under_a_runtime_stop() -> Row {
    use IntegrationOperation::{Close, Connect, Delete, Publish, Receive};
    let sqs = IntegrationKind::Sqs;
    let unsubmitted = |operation| (operation, IntegrationFailure::Cancelled, Retryability::Safe);
    let peer = SqsPeer::start();
    let (builder, mut loads) = SqsCredentialProbe::answers_once(peer.held_until_close());
    let queue = peer.queue_url(QUEUE);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let mut dropped = Box::pin(client.send_message(&queue, SQS_BODY));
        expect_pending("the stalled send", dropped.as_mut())?;
        wait_loads(&mut loads, 2)?;
        drop(dropped);
        // Each first poll admits and submits its work. The SDK shares one
        // pending credential load among concurrent operations, so one load
        // stalls all three ahead of signing.
        let mut sent = Box::pin(client.send_message(&queue, SQS_BODY));
        expect_pending("the stalled send", sent.as_mut())?;
        let mut deleted = Box::pin(client.delete_message(&queue, RECEIPT));
        expect_pending("the stalled delete", deleted.as_mut())?;
        let mut received = Box::pin(client.receive_messages(&queue, 1, Duration::ZERO));
        expect_pending("the stalled receive", received.as_mut())?;
        wait_loads(&mut loads, 3)?;
        runtime::request_shutdown();
        expect_refused(
            "a send the stop's close cut",
            bounded("send", sent)?,
            unsubmitted(Publish),
        )?;
        expect_refused(
            "a delete the stop's close cut",
            bounded("delete", deleted)?,
            unsubmitted(Delete),
        )?;
        expect_refused(
            "a receive the stop's close cut",
            bounded("receive", received)?,
            unsubmitted(Receive),
        )?;
        expect_refused(
            "send after the stop",
            bounded("send", client.send_message(&queue, SQS_BODY))?,
            closed(Publish),
        )?;
        expect_refused(
            "delete after the stop",
            bounded("delete", client.delete_message(&queue, RECEIPT))?,
            closed(Delete),
        )?;
        expect_ok("close after the stop", bounded("close", client.close())?)
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(sqs, Connect),
            failed_as(sqs, unsubmitted(Publish)).times(2),
            failed_as(sqs, unsubmitted(Delete)),
            failed_as(sqs, unsubmitted(Receive)),
            failed_as(sqs, closed(Publish)).before_admission(),
            failed_as(sqs, closed(Delete)).before_admission(),
            success(sqs, Close).under_shutdown(),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        peer.control().expect_no_connection(),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Two concurrent closes and a repeated close read settle one close
/// terminal, and an escaped clone's operations after it are `Closed`
/// refusals with no duration.
fn sqs_concurrent_close_settles_once() -> Row {
    use IntegrationOperation::{Close, Connect, Publish, Ready};
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let queue = peer.queue_url(QUEUE);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let escaped = client.clone();
        let (first, second) = bounded("concurrent close", async {
            tokio::join!(client.close(), escaped.close())
        })?;
        expect_ok("first close", first)?;
        expect_ok("concurrent close", second)?;
        expect_ok("repeated close", bounded("close", client.close())?)?;
        expect_refused(
            "ready on the escaped clone",
            bounded("ready", escaped.ready(&queue))?,
            closed(Ready),
        )?;
        expect_refused(
            "send on the escaped clone",
            bounded("send", escaped.send_message(&queue, SQS_BODY))?,
            closed(Publish),
        )
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(sqs, Connect),
            success(sqs, Close),
            failed_as(sqs, closed(Ready)).before_admission(),
            failed_as(sqs, closed(Publish)).before_admission(),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        peer.control().log().expect_requests(&[]),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// A client still open when the runtime stops is closed by the stop: one
/// close terminal under shutdown. The escaped handle's send afterwards is
/// one `Closed` refusal, and its close read adds no terminal.
fn sqs_runtime_stop_closes_under_shutdown() -> Row {
    use IntegrationOperation::{Close, Connect, Publish};
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let queue = peer.queue_url(QUEUE);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || settled("connect", builder.connect()));
    let escaped = clean_run(outcome).and_then(|escaped| {
        outside_camber(async {
            expect_refused(
                "send on the escaped handle",
                escaped.send_message(&queue, SQS_BODY).await,
                closed(Publish),
            )?;
            expect_ok("close on the escaped handle", escaped.close().await)
        })?
    });
    let observed = observation.finish();
    let verdict = all([
        escaped,
        observed.expect_terminals(&[
            success(sqs, Connect),
            success(sqs, Close).under_shutdown(),
            failed_as(sqs, closed(Publish)).before_admission(),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
        peer.control().log().expect_requests(&[]),
    ]);
    peer.finished(ROW_BOUND, verdict)
}

/// Dropping the last handle while the runtime runs is one close terminal,
/// not under shutdown.
fn sqs_last_handle_drop_closes_once() -> Row {
    use IntegrationOperation::{Close, Connect, Ready};
    let sqs = IntegrationKind::Sqs;
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let queue = peer.queue_url(QUEUE);
    let observation = Observation::start();
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        settled("ready", client.ready(&queue))?;
        drop(client);
        Ok(())
    });
    let observed = observation.finish();
    let verdict = all([
        clean_run(outcome),
        observed.expect_terminals(&[
            success(sqs, Connect),
            success(sqs, Ready),
            success(sqs, Close),
        ]),
        observed.expect_one_instance(sqs),
        observed.expect_redacted(&REDACTED),
    ]);
    peer.finished(ROW_BOUND, verdict)
}
