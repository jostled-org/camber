//! 2.T2–2.T3: acknowledged NATS publishing completes only on a correlated
//! server receipt.
//!
//! Every row enters through the public `camber::mq::nats` API against the
//! scripted wire peer, which records each complete publication and answers
//! one only when the row says so. A publish is required pending after the
//! peer read it and after a delivery barrier proved the SDK processed every
//! earlier frame, so socket progress is never mistaken for a receipt. Exact
//! outcomes follow a peer record, an owner-committed result, or a scheduling
//! checkpoint; no sleep sets precedence.
//!
//! Each row owns its runtime and its peer, finishes the peer on success and
//! failure alike, and returns its own verdict.
#![cfg(feature = "nats")]

use crate::integration_rows::{
    EXHAUSTED, INVALID_NATS_SUBJECT, NamedRow, ROW_BOUND, Refusal, Row, busy, clean_run, closed,
    expect, expect_aggregate, expect_eq, expect_failed_run, expect_ok, expect_polled_pending,
    expect_refused, expect_scope_closed, invalid_config, limit_exceeded, on_tokio, refused,
    rejected, row_bounded, run_observing, settled, timed_out, unavailable, unknown,
};
use crate::nats_ack_rows::{
    STREAM, SUBJECT, Teardown, ack, acknowledge, acknowledged, answered, correlated_answers,
    cut_transport, expect_retired, expect_withheld, finished_within, jetstream_error,
    on_connection, publication, reply_to, slot_reused, wait_ready,
};
use crate::nats_peer::wire::{ADVERTISED_MAX_PAYLOAD, Reply};
use crate::nats_peer::{NatsPeer, PeerControl, PeerLog, Script, wait_unavailable};
use camber::mq::nats::{self, Connection};
use camber::runtime_test_support::{NatsAckProbe, NatsQueueProbe};
use camber::{IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability, runtime};
use std::pin::pin;
use std::time::Duration;
use tokio::time::Instant;

/// The documented default for every NATS count bound.
const DEFAULT_COUNT: usize = 64;

/// The SDK queue the reply-queue rows configure for every subscription.
const REPLY_QUEUE: usize = 2;

/// The operation expiry of the one-deadline row: wide enough that its
/// release and its late receipt each land in their own part of it.
const ONE_DEADLINE: Duration = Duration::from_secs(2);

const PUBLISH: IntegrationOperation = IntegrationOperation::Publish;

// ── 2.T2 configuration and wire contract ─────────────────────────────

#[test]
fn acknowledged_configuration_and_wire_contract() {
    crate::integration_rows::run_rows(CONFIGURATION_AND_WIRE);
}

/// Every configuration and wire row, by the claim it proves.
const CONFIGURATION_AND_WIRE: &[NamedRow<'static>] = &[
    (
        "invalid stream names refuse before effects",
        invalid_stream_names_refuse_before_effects,
    ),
    (
        "valid stream names connect and keep their case",
        valid_stream_names_connect_and_keep_their_case,
    ),
    (
        "a repeated setter replaces the earlier stream",
        repeated_setter_replaces_the_earlier_stream,
    ),
    (
        "configuration refuses before runtime and admission",
        configuration_refuses_before_runtime_and_admission,
    ),
    (
        "connect installs one private wildcard subscription",
        connect_installs_one_private_wildcard_subscription,
    ),
    ("core remains the default", core_remains_the_default),
    (
        "subscriptions keep their core semantics",
        subscriptions_keep_their_core_semantics,
    ),
    (
        "the private subscription takes no subscription slot",
        private_subscription_takes_no_subscription_slot,
    ),
    (
        "the reply queue holds exactly the subscription capacity",
        reply_queue_holds_the_subscription_capacity,
    ),
    (
        "a reply past the subscription capacity overflows the connection",
        reply_past_the_subscription_capacity_overflows,
    ),
];

/// Names the setter refuses: empty, too long, and every byte outside ASCII
/// letters, digits, `_`, and `-`.
fn invalid_stream_names() -> Box<[Box<str>]> {
    [
        "", "é", "a b", "a.b", "a*", "a>", "a/b", "a\r\nb", "a:b", "a\0b", "\t", "a\u{7f}",
    ]
    .into_iter()
    .map(Box::from)
    .chain([Box::from("A".repeat(256))])
    .collect()
}

/// Every invalid name is `Connect/InvalidConfig/Never` and reaches no peer.
fn invalid_stream_names_refuse_before_effects() -> Row {
    let peer = NatsPeer::start();
    let url = peer.url();
    let outcome = runtime::builder().run(move || -> Row {
        for name in &invalid_stream_names() {
            expect_refused(
                &format!("stream name {name:?}"),
                row_bounded(
                    "connect",
                    nats::builder(&url).acknowledged_publishing(name).connect(),
                )?,
                invalid_config(IntegrationOperation::Connect),
            )?;
        }
        Ok(())
    });
    let verdict = clean_run(outcome).and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// One and 255 bytes, mixed case, digits, `_`, and `-` connect; the header
/// carries the configured name exactly.
fn valid_stream_names_connect_and_keep_their_case() -> Row {
    let peer = NatsPeer::start();
    let url = peer.url();
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let longest = "S".repeat(255);
        for (index, name) in ["A", longest.as_str(), "Orders_2-x"].iter().enumerate() {
            let connection = settled(
                "connect",
                nats::builder(&url).acknowledged_publishing(name).connect(),
            )?;
            runtime::block_on(publishes_expecting(&connection, &control, index, name))?;
            expect_ok("close", row_bounded("close", connection.close())?)?;
        }
        Ok(())
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// The last setter call wins: a valid name replaces an invalid one, an
/// invalid name replaces a valid one, and a second valid name replaces the
/// first on the wire.
fn repeated_setter_replaces_the_earlier_stream() -> Row {
    let peer = NatsPeer::start();
    let url = peer.url();
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        expect_refused(
            "an invalid name after a valid one",
            row_bounded(
                "connect",
                nats::builder(&url)
                    .acknowledged_publishing(STREAM)
                    .acknowledged_publishing("bad name")
                    .connect(),
            )?,
            invalid_config(IntegrationOperation::Connect),
        )?;
        control.expect_no_connection()?;
        let connection = settled(
            "connect",
            nats::builder(&url)
                .acknowledged_publishing("bad name")
                .acknowledged_publishing("FIRST")
                .acknowledged_publishing(STREAM)
                .connect(),
        )?;
        runtime::block_on(publishes_expecting(&connection, &control, 0, STREAM))?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// Publish once, require the peer to read it as publication `index` naming
/// `stream` as its expected stream, and settle it on that stream's receipt.
async fn publishes_expecting(
    connection: &Connection,
    control: &PeerControl,
    index: usize,
    stream: &str,
) -> Row {
    let mut publish = pin!(connection.publish(SUBJECT, b"payload"));
    expect_polled_pending("the publish", &futures_util::poll!(publish.as_mut()))?;
    let read = publication(control, index)?;
    expect_eq(
        &format!("the expected stream for {stream}"),
        read.header("Nats-Expected-Stream"),
        Some(stream),
    )?;
    control.reply(reply_to(&read)?, &Reply::Message(ack(stream, 1).as_bytes()))?;
    expect_ok(
        "the publish",
        finished_within("the publish", publish).await?,
    )
}

/// An invalid name is a configuration refusal outside any runtime and after
/// root admission closed, with no connection.
fn configuration_refuses_before_runtime_and_admission() -> Row {
    let peer = NatsPeer::start();
    let url = peer.url();
    let outside =
        on_tokio(nats::builder(&url).acknowledged_publishing("").connect()).and_then(|outcome| {
            expect_refused(
                "an invalid name outside a runtime",
                outcome,
                invalid_config(IntegrationOperation::Connect),
            )
        });
    let closed_url = url.clone();
    let outcome = runtime::builder().run(move || -> Row {
        runtime::request_shutdown();
        expect_refused(
            "an invalid name after admission closed",
            row_bounded(
                "connect",
                nats::builder(&closed_url)
                    .acknowledged_publishing("a.b")
                    .connect(),
            )?,
            invalid_config(IntegrationOperation::Connect),
        )
    });
    let verdict = outside
        .and_then(|()| clean_run(outcome))
        .and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// Connect subscribes exactly one private wildcard inbox and publishes
/// nothing: no stream lookup, creation, or other administration.
fn connect_installs_one_private_wildcard_subscription() -> Row {
    on_connection(
        |builder| builder,
        Teardown::Clean,
        |connection, control| {
            expect_ok("ready after connect", connection.ready())?;
            control.delivery_barrier(ROW_BOUND)?;
            let log = control.log();
            expect_eq("subscriptions after connect", log.subscribed.len(), 1)?;
            let (subject, queue, _) = &log.subscribed[0];
            expect(
                "the private subscription is a wildcard inbox",
                subject.starts_with("_INBOX.") && subject.ends_with(".*"),
            )?;
            expect_eq("the private subscription's queue group", queue, &None)?;
            expect_eq("publications at connect", log.publications.len(), 0)?;
            expect_retired(connection)?;
            Ok(())
        },
    )
}

/// A builder without the setter and the convenience connect publish Core:
/// no private subscription, no reply, no header, and success after the
/// local flush with no acknowledgement.
fn core_remains_the_default() -> Row {
    let peer = NatsPeer::start();
    let url = peer.url();
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let built = settled("builder connect", nats::builder(&url).connect())?;
        let convenient = settled("convenience connect", nats::connect(&url))?;
        expect(
            "a Core connection exposes acknowledgement state",
            NatsAckProbe::observe(&built).is_none(),
        )?;
        for connection in [&built, &convenient] {
            expect_ok(
                "Core publish",
                row_bounded("publish", connection.publish(SUBJECT, b"core"))?,
            )?;
        }
        let log = control.wait_for("both Core publications", ROW_BOUND, |log| {
            log.publications.len() >= 2
        })?;
        expect_eq("subscriptions", log.subscribed.len(), 0)?;
        for read in &log.publications {
            expect_eq("a Core reply subject", &read.reply, &None)?;
            expect_eq("Core headers", &read.headers, &None)?;
        }
        expect_ok("close", row_bounded("close", built.close())?)?;
        expect_ok("close", row_bounded("close", convenient.close())?)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// Ordinary and queue subscriptions deliver as in Core.
fn subscriptions_keep_their_core_semantics() -> Row {
    on_connection(
        |builder| builder,
        Teardown::Clean,
        |connection, control| {
            let mut ordinary = settled("subscribe", connection.subscribe("orders"))?;
            let mut grouped = settled(
                "queue subscribe",
                connection.queue_subscribe("orders.queued", "workers"),
            )?;
            let ordinary_sid = control.sid("orders", ROW_BOUND)?;
            let grouped_sid = control.sid("orders.queued", ROW_BOUND)?;
            control.deliver(&ordinary_sid, "orders", &[b"plain"])?;
            control.deliver(&grouped_sid, "orders.queued", &[b"grouped"])?;
            for (what, subscription, expected) in [
                ("the ordinary message", &mut ordinary, &b"plain"[..]),
                ("the queue message", &mut grouped, &b"grouped"[..]),
            ] {
                let message = settled(what, subscription.next())?
                    .ok_or_else(|| format!("{what} completed the subscription"))?;
                expect_eq(what, message.payload(), expected)?;
            }
            Ok(())
        },
    )
}

/// With one application subscription allowed, the private inbox leaves it
/// free: one subscribe succeeds, the next is `Busy`.
fn private_subscription_takes_no_subscription_slot() -> Row {
    on_connection(
        |builder| builder.max_subscriptions(1),
        Teardown::Clean,
        |connection, control| {
            let subscription = settled("the one allowed subscribe", connection.subscribe("one"))?;
            expect_refused(
                "a subscribe past the allowance",
                row_bounded("subscribe", connection.subscribe("two"))?,
                busy(IntegrationOperation::Subscribe),
            )?;
            drop(subscription);
            control.delivery_barrier(ROW_BOUND)?;
            expect_eq(
                "subscriptions the peer read",
                control.log().subscribed.len(),
                2,
            )?;
            Ok(())
        },
    )
}

/// The SDK subscription ID of the private inbox, and a reply subject under
/// it that names no publish.
fn private_inbox(control: &PeerControl) -> Result<(Box<str>, String), String> {
    control.delivery_barrier(ROW_BOUND)?;
    let log = control.log();
    let (prefix, sid) = private_prefix(&log)?;
    Ok((Box::from(sid), format!("{prefix}999999")))
}

/// The reply prefix of the private wildcard inbox the peer read first, and
/// its SDK subscription ID.
fn private_prefix(log: &PeerLog) -> Result<(&str, &str), String> {
    let (inbox, _, sid) = log.subscribed.first().ok_or("no private subscription")?;
    let prefix = inbox
        .strip_suffix('*')
        .ok_or_else(|| format!("{inbox} is not a wildcard"))?;
    Ok((prefix, sid))
}

/// Write `count` replies to the private inbox in one write.
///
/// The rows run on one worker, so the SDK reads the burst in one poll and
/// the reply receiver cannot drain between its messages.
fn reply_burst(control: &PeerControl, count: usize) -> Row {
    let (sid, subject) = private_inbox(control)?;
    let body = ack(STREAM, 1);
    control.deliver(&sid, &subject, &vec![body.as_bytes(); count])
}

/// A burst of exactly the configured `subscription_capacity` on the private
/// inbox fits the SDK queue: no slow-consumer event charges the connection,
/// an acknowledged publish still completes, and close settles cleanly.
fn reply_queue_holds_the_subscription_capacity() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url()).subscription_capacity(REPLY_QUEUE);
    let control = peer.control();
    let outcome = runtime::builder().worker_threads(1).run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        reply_burst(&control, REPLY_QUEUE)?;
        control.delivery_barrier(ROW_BOUND)?;
        let verdict: Row = runtime::block_on(async {
            let outcome = answered(
                &connection,
                &control,
                0,
                Reply::Message(ack(STREAM, 2).as_bytes()),
            )
            .await?;
            expect_ok("the publish after a full reply queue", outcome)
        });
        verdict?;
        // The SDK queues any slow-consumer event ahead of its closed event,
        // so a clean close proves the burst overflowed nothing.
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// One reply past the configured `subscription_capacity` overflows the
/// private inbox's SDK queue: the slow-consumer event closes the whole
/// connection, and the runtime keeps that one charge.
fn reply_past_the_subscription_capacity_overflows() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url()).subscription_capacity(REPLY_QUEUE);
    let control = peer.control();
    let overflow = limit_exceeded(IntegrationOperation::Receive);
    let outcome = run_observing(runtime::builder().worker_threads(1), move || -> Row {
        let connection = settled("connect", builder.connect())?;
        reply_burst(&control, REPLY_QUEUE + 1)?;
        row_bounded("the SDK slow-consumer event", async {
            while connection.ready().is_ok() {
                tokio::task::yield_now().await;
            }
        })?;
        expect_refused(
            "a publish after the overflow",
            row_bounded("publish", connection.publish(SUBJECT, b"late"))?,
            closed(PUBLISH),
        )?;
        expect_refused(
            "close after the overflow",
            row_bounded("close", connection.close())?,
            overflow,
        )
    });
    let verdict = expect_failed_run(outcome, "the reply overflow left no aggregate", |error| {
        expect_aggregate(error, IntegrationKind::Nats, &[overflow])
    })
    .and_then(|()| expect_eq("publications", peer.control().log().publications.len(), 0));
    peer.finished(ROW_BOUND, verdict)
}

// ── 2.T2 acknowledgement ─────────────────────────────────────────────

#[test]
fn acknowledged_publish_requires_matching_receipt() {
    crate::integration_rows::run_rows(MATCHING_RECEIPT);
}

/// Every receipt row, by the claim it proves.
const MATCHING_RECEIPT: &[NamedRow<'static>] = &[(
    "publication carries the expected stream and a private reply",
    publication_carries_the_expected_stream_and_a_private_reply,
)];

/// The one `HPUB` carries the subject, the payload, the expected-stream
/// header, and a reply under the private wildcard inbox.
fn publication_carries_the_expected_stream_and_a_private_reply() -> Row {
    on_connection(
        |builder| builder,
        Teardown::Clean,
        |connection, control| {
            let verdict: Row = runtime::block_on(async {
                let outcome = answered(
                    connection,
                    control,
                    0,
                    Reply::Message(ack(STREAM, 1).as_bytes()),
                )
                .await?;
                expect_ok("the acknowledged publish", outcome)
            });
            verdict?;
            let log = control.log();
            let read = log.publications.first().ok_or("no publication")?;
            expect_eq("the subject", &*read.subject, SUBJECT)?;
            expect_eq("the payload", &*read.payload, &b"payload"[..])?;
            expect_eq(
                "the expected stream",
                read.header("Nats-Expected-Stream"),
                Some(STREAM),
            )?;
            let (prefix, _) = private_prefix(&log)?;
            let token = reply_to(read)?
                .strip_prefix(prefix)
                .ok_or("the reply is outside the private inbox")?;
            expect(
                "the reply token is one decimal identity",
                !token.is_empty() && token.bytes().all(|byte| byte.is_ascii_digit()),
            )?;
            expect_eq("publications", log.publications.len(), 1)?;
            Ok(())
        },
    )
}

// ── 2.T3 classification ──────────────────────────────────────────────

#[test]
fn acknowledged_replies_classify_every_failure() {
    crate::integration_rows::run_rows(CLASSIFICATION);
}

/// Every classification row, by the claim it proves.
const CLASSIFICATION: &[NamedRow<'static>] = &[
    (
        "refusals before submission send nothing",
        refusals_before_submission_send_nothing,
    ),
    (
        "the SDK maximum includes the expected-stream header",
        sdk_maximum_includes_the_header,
    ),
    (
        "a disconnected publish is unavailable",
        disconnected_publish_is_unavailable,
    ),
    (
        "every correlated reply has its class",
        every_correlated_reply_has_its_class,
    ),
    (
        "a connection-wide permission error rejects no publish",
        connection_wide_permission_rejects_no_publish,
    ),
    (
        "closed access refuses before publication",
        closed_access_refuses_before_publication,
    ),
];

/// An invalid subject is `Rejected/Never` and a payload past the maximum is
/// `LimitExceeded/Never`, both before any publication.
fn refusals_before_submission_send_nothing() -> Row {
    on_connection(
        |builder| builder.max_message_bytes(4),
        Teardown::Clean,
        |connection, control| {
            expect_refused(
                "an invalid subject",
                row_bounded("publish", connection.publish(INVALID_NATS_SUBJECT, b"x"))?,
                rejected(PUBLISH),
            )?;
            expect_refused(
                "a payload past the maximum",
                row_bounded("publish", connection.publish(SUBJECT, b"12345"))?,
                limit_exceeded(PUBLISH),
            )?;
            control.delivery_barrier(ROW_BOUND)?;
            expect_eq("publications", control.log().publications.len(), 0)?;
            expect_retired(connection)?;
            Ok(())
        },
    )
}

/// A payload exactly at the peer's advertised maximum passes Camber's own
/// bound, but with the expected-stream header it exceeds the SDK's: the
/// SDK refuses it before submission.
fn sdk_maximum_includes_the_header() -> Row {
    on_connection(
        |builder| builder.max_message_bytes(ADVERTISED_MAX_PAYLOAD),
        Teardown::Clean,
        |connection, control| {
            let payload = vec![0_u8; ADVERTISED_MAX_PAYLOAD];
            expect_refused(
                "a payload at the SDK maximum with its header",
                row_bounded("publish", connection.publish(SUBJECT, &payload))?,
                limit_exceeded(PUBLISH),
            )?;
            control.delivery_barrier(ROW_BOUND)?;
            expect_eq("publications", control.log().publications.len(), 0)?;
            expect_retired(connection)?;
            Ok(())
        },
    )
}

/// A connection the SDK reports disconnected refuses an acknowledged publish
/// as `Unavailable/Safe` and queues nothing.
fn disconnected_publish_is_unavailable() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url()).shutdown_timeout(EXHAUSTED);
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        control.script(Script::Refuse);
        wait_unavailable(&connection, ROW_BOUND)?;
        expect_refused(
            "an acknowledged publish while disconnected",
            row_bounded("publish", connection.publish(SUBJECT, b"x"))?,
            unavailable(PUBLISH),
        )?;
        expect_retired(&connection)?;
        drop(connection);
        Ok(())
    });
    let verdict = match outcome {
        Ok(verdict) => verdict,
        // The dropped connection's close cannot reach a refusing peer.
        Err(error) => expect_aggregate(
            &error,
            IntegrationKind::Nats,
            &[timed_out(IntegrationOperation::Close)],
        ),
    };
    let verdict = verdict
        .and_then(|()| expect_eq("publications", peer.control().log().publications.len(), 0));
    peer.finished(ROW_BOUND, verdict)
}

/// Each correlated reply settles its own publish with its class, on one
/// connection whose correlation state retires after each.
fn every_correlated_reply_has_its_class() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url()).max_in_flight(1);
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let answers = correlated_answers();
        let verdict: Row = runtime::block_on(async {
            for (index, (what, answer, expected)) in answers.iter().enumerate() {
                let outcome = answered(&connection, &control, index, answer.reply()).await?;
                expect_eq(what, refused(outcome), *expected)?;
                expect_retired(&connection)?;
            }
            Ok(())
        });
        verdict?;
        expect_eq(
            "publications, one per reply",
            control.log().publications.len(),
            answers.len(),
        )?;
        slot_reused(&connection, &control, answers.len())?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// A connection-wide permissions error is no correlated rejection: the
/// submitted publish stays pending after the SDK processed it, and a later
/// disconnect leaves it unknown, never `PermissionDenied`.
fn connection_wide_permission_rejects_no_publish() -> Row {
    on_connection(
        |builder| builder,
        Teardown::Clean,
        |connection, control| {
            let verdict: Row = runtime::block_on(async {
                let mut publish = pin!(connection.publish(SUBJECT, b"payload"));
                expect_withheld("the publish", control, 0, publish.as_mut()).await?;
                control.server_error("Permissions Violation for Publish to \"events.created\"")?;
                control.delivery_barrier(ROW_BOUND)?;
                expect_polled_pending(
                    "the publish after a connection-wide permissions error",
                    &futures_util::poll!(publish.as_mut()),
                )?;
                cut_transport(control)?;
                expect_refused(
                    "the publish after the permissions error",
                    finished_within("the publish", publish).await?,
                    unknown(PUBLISH),
                )
            });
            verdict?;
            wait_ready(connection)?;
            Ok(())
        },
    )
}

/// An acknowledged connection refuses a publish as `Closed` after its own
/// close and after a runtime stop, and an acknowledged connect after the
/// stop is `ScopeClosed`. None publishes, and the refused connect never
/// reaches the peer.
fn closed_access_refuses_before_publication() -> Row {
    let peer = NatsPeer::start();
    let builder = acknowledged(&peer.url());
    let outcome = runtime::builder().run(move || -> Row {
        let closed_first = settled("connect", builder.clone().connect())?;
        expect_ok("close", row_bounded("close", closed_first.close())?)?;
        expect_refused(
            "a publish after close",
            row_bounded("publish", closed_first.publish(SUBJECT, b"late"))?,
            closed(PUBLISH),
        )?;
        let stopped = settled("connect", builder.clone().connect())?;
        runtime::request_shutdown();
        expect_refused(
            "a publish after the stop",
            row_bounded("publish", stopped.publish(SUBJECT, b"late"))?,
            closed(PUBLISH),
        )?;
        expect_scope_closed(
            "an acknowledged connect after the stop",
            row_bounded("connect", builder.connect())?,
        )?;
        expect_ok(
            "close after the stop",
            row_bounded("close", stopped.close())?,
        )
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let log = peer.control().log();
        expect_eq("publications", log.publications.len(), 0)?;
        expect_eq("connections the peer accepted", log.accepted, 2)
    });
    peer.finished(ROW_BOUND, verdict)
}

// ── 2.T3 correlation ─────────────────────────────────────────────────

#[test]
fn acknowledged_publish_routes_out_of_order_replies() {
    crate::integration_rows::run_rows(&[(
        "out-of-order replies settle their own publishes",
        out_of_order_replies_settle_their_own_publishes,
    )]);
}

/// Three concurrent publishes answered in reverse order each read their own
/// reply. A duplicate reply after settlement changes nothing, and the next
/// publish succeeds on its own receipt.
fn out_of_order_replies_settle_their_own_publishes() -> Row {
    on_connection(
        |builder| builder,
        Teardown::Clean,
        |connection, control| {
            let verdict: Row = runtime::block_on(async {
                let payloads: [&[u8]; 3] = [b"one", b"two", b"three"];
                let mut publishes: Vec<_> = payloads
                    .iter()
                    .map(|payload| Box::pin(connection.publish(SUBJECT, payload)))
                    .collect();
                for publish in &mut publishes {
                    expect_polled_pending(
                        "a concurrent publish",
                        &futures_util::poll!(publish.as_mut()),
                    )?;
                }
                let read = control.publications(3, ROW_BOUND)?;
                let reply_of = |payload: &[u8]| {
                    read.iter()
                        .find(|publication| &*publication.payload == payload)
                        .ok_or("a publication is missing")
                };
                let third = reply_of(b"three")?;
                acknowledge(control, third, 3)?;
                acknowledge(control, reply_of(b"one")?, 1)?;
                control.reply(
                    reply_to(reply_of(b"two")?)?,
                    &Reply::Message(jetstream_error(400, 10060).as_bytes()),
                )?;
                let outcomes =
                    tokio::time::timeout(ROW_BOUND, futures_util::future::join_all(publishes))
                        .await
                        .map_err(|_| "the concurrent publishes never finished".to_owned())?;
                let classes: Vec<Option<Refusal>> = outcomes.into_iter().map(refused).collect();
                expect_eq(
                    "each publish's own result",
                    classes,
                    vec![None, Some(rejected(PUBLISH)), None],
                )?;
                control.reply(
                    reply_to(third)?,
                    &Reply::Message(jetstream_error(400, 10060).as_bytes()),
                )?;
                let next = answered(
                    connection,
                    control,
                    3,
                    Reply::Message(ack(STREAM, 4).as_bytes()),
                )
                .await?;
                expect_ok("the publish after a duplicate reply", next)
            });
            verdict?;
            expect_retired(connection)?;
            Ok(())
        },
    )
}

// ── 2.T3 capacity and deadline ───────────────────────────────────────

#[test]
fn acknowledged_capacity_and_deadline_are_shared() {
    crate::integration_rows::run_rows(CAPACITY_AND_DEADLINE);
}

/// Every capacity and deadline row, by the claim it proves.
const CAPACITY_AND_DEADLINE: &[NamedRow<'static>] = &[
    (
        "publish and subscribe share one overridden slot",
        publish_and_subscribe_share_one_slot,
    ),
    (
        "the default capacity bounds withheld publishes",
        default_capacity_bounds_withheld_publishes,
    ),
    (
        "the receipt wait keeps the original expiry",
        receipt_wait_keeps_the_original_expiry,
    ),
];

/// A withheld publish holds the one slot: another publish and a subscribe
/// are `Busy` with no effect, and the slot works again once answered.
fn publish_and_subscribe_share_one_slot() -> Row {
    on_connection(
        |builder| builder.max_in_flight(1),
        Teardown::Clean,
        |connection, control| {
            let verdict: Row = runtime::block_on(async {
                let mut held = pin!(connection.publish(SUBJECT, b"held"));
                let read = expect_withheld("the held publish", control, 0, held.as_mut()).await?;
                expect_refused(
                    "a publish past the slot",
                    connection.publish(SUBJECT, b"refused").await,
                    busy(PUBLISH),
                )?;
                expect_refused(
                    "a subscribe past the slot",
                    connection.subscribe("refused").await.map(drop),
                    busy(IntegrationOperation::Subscribe),
                )?;
                acknowledge(control, &read, 1)?;
                expect_ok(
                    "the held publish",
                    finished_within("the held publish", held).await?,
                )?;
                let subscription = connection
                    .subscribe("after")
                    .await
                    .map_err(|error| format!("subscribe after the slot freed: {error:?}"))?;
                drop(subscription);
                let next = answered(
                    connection,
                    control,
                    1,
                    Reply::Message(ack(STREAM, 2).as_bytes()),
                )
                .await?;
                expect_ok("the publish after the slot freed", next)
            });
            verdict?;
            let log = control.log();
            expect_eq("publications", log.publications.len(), 2)?;
            expect(
                "the refused subscribe reached the peer",
                log.subscribed
                    .iter()
                    .all(|(subject, _, _)| &**subject != "refused"),
            )?;
            Ok(())
        },
    )
}

/// The default 64 withheld publishes fill the operation limit: the next is
/// `Busy` with no publication, and every held one completes on its receipt.
fn default_capacity_bounds_withheld_publishes() -> Row {
    on_connection(
        |builder| builder,
        Teardown::Clean,
        |connection, control| {
            let verdict: Row = runtime::block_on(async {
                let mut held: Vec<_> = (0..DEFAULT_COUNT)
                    .map(|_| Box::pin(connection.publish(SUBJECT, b"held")))
                    .collect();
                for publish in &mut held {
                    expect_polled_pending(
                        "a held publish",
                        &futures_util::poll!(publish.as_mut()),
                    )?;
                }
                let read = control.publications(DEFAULT_COUNT, ROW_BOUND)?;
                expect_refused(
                    "the publish past the default limit",
                    connection.publish(SUBJECT, b"refused").await,
                    busy(PUBLISH),
                )?;
                for (sequence, publication) in (1..).zip(read.iter()) {
                    acknowledge(control, publication, sequence)?;
                }
                let outcomes =
                    tokio::time::timeout(ROW_BOUND, futures_util::future::join_all(held))
                        .await
                        .map_err(|_| "the held publishes never finished".to_owned())?;
                let classes: Box<[Option<Refusal>]> = outcomes.into_iter().map(refused).collect();
                expect_eq(
                    "each held publish's own result",
                    classes,
                    vec![None; DEFAULT_COUNT].into_boxed_slice(),
                )
            });
            verdict?;
            control.delivery_barrier(ROW_BOUND)?;
            expect_eq(
                "publications",
                control.log().publications.len(),
                DEFAULT_COUNT,
            )?;
            expect_retired(connection)?;
            Ok(())
        },
    )
}

/// A publish released from its post-submission hold before its expiry
/// waits for its receipt only until that same expiry. A matching receipt
/// sent after it, while a deadline restarted at the release would still
/// run, cannot turn the `Timeout/OutcomeUnknown` into success.
///
/// The Camber runtime runs on several workers, so its clock cannot pause.
/// Observed edges order the row instead: the SDK admission and the peer's
/// receipt come before the release, and the receipt goes out only after the
/// publish settled or once the original expiry has certainly passed. The
/// real-time waits only place the release and the receipt in the windows a
/// restarted deadline would expose; the asserted class never depends on
/// them, and the measured instants fail the row if a window was missed.
fn receipt_wait_keeps_the_original_expiry() -> Row {
    on_connection(
        |builder| builder.operation_timeout(ONE_DEADLINE),
        Teardown::Clean,
        |connection, control| {
            let mut probe =
                NatsQueueProbe::hold(connection).ok_or("the queue probe did not attach")?;
            let verdict: Row = runtime::block_on(async {
                // The publish starts its one expiry no earlier than this.
                let started = Instant::now();
                let mut publish = pin!(connection.publish(SUBJECT, b"held"));
                expect_polled_pending("the held publish", &futures_util::poll!(publish.as_mut()))?;
                let admitted = finished_within("the SDK admission", probe.polled(1)).await?;
                expect_eq("SDK admissions", admitted, Some(1))?;
                let read = publication(control, 0)?;
                // The expiry began before the admission this observes.
                let expiry_by = Instant::now() + ONE_DEADLINE;
                tokio::time::sleep_until(started + ONE_DEADLINE / 2).await;
                probe.release();
                let released = Instant::now();
                expect(
                    "the hold released before the original expiry",
                    released < started + ONE_DEADLINE,
                )?;
                let settled_first = tokio::select! {
                    biased;
                    outcome = publish.as_mut() => Some(outcome),
                    () = tokio::time::sleep_until(expiry_by + ONE_DEADLINE / 4) => None,
                };
                acknowledge(control, &read, 1)?;
                expect(
                    "the receipt went out before a restarted expiry",
                    Instant::now() < released + ONE_DEADLINE,
                )?;
                let outcome = match settled_first {
                    Some(outcome) => outcome,
                    None => finished_within("the released publish", publish).await?,
                };
                expect_refused(
                    "the publish whose receipt came after its expiry",
                    outcome,
                    (
                        PUBLISH,
                        IntegrationFailure::Timeout,
                        Retryability::OutcomeUnknown,
                    ),
                )
            });
            verdict?;
            control.delivery_barrier(ROW_BOUND)?;
            expect_eq("publications", control.log().publications.len(), 1)?;
            expect_retired(connection)?;
            Ok(())
        },
    )
}
