//! 5.T1–5.T2: Core NATS admission, acknowledgement, limits, and close.
//!
//! Every row enters through the public `camber::mq::nats` API against a
//! scripted wire peer that records each command it reads. Exact outcomes are
//! asserted only after a peer record or an owner-committed fact; a row that
//! waits for a state polls that committed state under a hang guard.
//!
//! Each row owns its runtime and its peer, finishes the peer on success and
//! failure alike, and returns its own verdict, so one broken claim cannot hide
//! another.
#![cfg(feature = "nats")]

use crate::integration_rows::{
    EXHAUSTED, LIVE_LIMIT, NamedRow, ROW_BOUND, Row, busy, cancelled, clean_run, closed, expect,
    expect_aggregate, expect_eq, expect_failed_run, expect_no_runtime, expect_ok, expect_pending,
    expect_polled_pending, expect_refused, expect_scope_closed, expired, hold_live_slots,
    limit_exceeded, next_payload, on_tokio, permission_denied, refusal, rejected, row_bounded,
    run_observing, run_rows, settled, timed_out, unavailable, unknown,
};
use crate::nats_peer::{NatsPeer, PeerLog, Script, hold_transport, wait_unavailable};
use camber::mq::nats::{self, NatsBuilder, Subscription};
use camber::runtime_test_support::{NatsEventProbe, NatsQueueProbe};
use camber::{
    IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability, RuntimeError, runtime,
};
use std::future::Future;
use std::task::Poll;
use std::time::Duration;

/// A payload larger than any loopback socket buffers while the peer is not
/// reading, so its flush cannot complete.
const STALLED_PAYLOAD: usize = 16 * 1024 * 1024;

/// The documented default for every NATS count bound.
const DEFAULT_COUNT: usize = 64;

/// The documented default payload maximum.
const DEFAULT_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

/// Fail the row unless the next receive on `subscription` completes it.
fn expect_completed(what: &str, subscription: &mut Subscription) -> Row {
    expect_eq(
        what,
        row_bounded("next", subscription.next())?
            .map(|message| message.is_some())
            .map_err(|error| refusal(&error)),
        Ok(false),
    )
}

/// Fail the row unless the peer accepted a connection but read no handshake.
fn expect_no_handshake(peer: &NatsPeer) -> Row {
    let log = peer.control().log();
    expect("the peer saw no connection attempt", log.accepted >= 1)?;
    expect_eq("handshakes the peer read", log.connects, 0)
}

/// The `PUB` commands the peer read for `subject`.
fn published_to(log: &PeerLog, subject: &str) -> usize {
    log.published
        .iter()
        .filter(|(recorded, _)| &**recorded == subject)
        .count()
}

// ── 5.T1 ──────────────────────────────────────────────────────────────

#[test]
fn nats_admission_acknowledgement_and_close_are_bounded() {
    run_rows(ADMISSION_ACKNOWLEDGEMENT_AND_CLOSE);
}

/// Every 5.T1 row, by the claim it proves.
const ADMISSION_ACKNOWLEDGEMENT_AND_CLOSE: &[NamedRow<'static>] = &[
    (
        "every builder bound refuses before I/O",
        builder_bounds_refuse_before_io,
    ),
    (
        "no runtime refuses before I/O",
        no_runtime_refuses_before_io,
    ),
    (
        "closed admission refuses before I/O",
        closed_admission_refuses_before_io,
    ),
    (
        "live integration limit refuses before I/O",
        live_limit_refuses_before_io,
    ),
    (
        "readiness and publish name their acknowledgements",
        readiness_and_publish_name_their_acknowledgements,
    ),
    (
        "payload maximum refuses before copy",
        payload_maximum_refuses_before_copy,
    ),
    (
        "operation limit and lost acknowledgement",
        operation_limit_and_lost_acknowledgement,
    ),
    (
        "disconnected publish is unavailable",
        disconnected_publish_is_unavailable,
    ),
    (
        "authorization violation is permission denied",
        authorization_violation_is_permission_denied,
    ),
    ("subscription limit", subscription_limit),
    (
        "try_next is immediate and delivery is bounded",
        try_next_is_immediate_and_delivery_is_bounded,
    ),
    (
        "timeout is distinct from completion",
        timeout_is_distinct_from_completion,
    ),
    (
        "close is idempotent and refuses every clone",
        close_is_idempotent_and_refuses_every_clone,
    ),
    (
        "close completes every subscription",
        close_completes_every_subscription,
    ),
    (
        "single worker runtime makes progress",
        single_worker_runtime_makes_progress,
    ),
    (
        "connect refused by the transport is unavailable",
        connect_refused_by_the_transport_is_unavailable,
    ),
    (
        "connect with no server answer times out",
        connect_with_no_server_answer_times_out,
    ),
    (
        "default payload maximum refuses before copy",
        default_payload_maximum_refuses_before_copy,
    ),
    ("default subscription limit", default_subscription_limit),
    (
        "default operation limit refuses before effect",
        default_operation_limit_refuses_before_effect,
    ),
    (
        "invalid subscription is rejected before submission",
        invalid_subscription_is_rejected_before_submission,
    ),
    (
        "dropped subscribe waiter withdraws its registration",
        dropped_subscribe_waiter_withdraws_its_registration,
    ),
    (
        "dropped publish waiter after submission keeps an unknown outcome",
        dropped_publish_waiter_after_submission_keeps_an_unknown_outcome,
    ),
    (
        "reconnect after submission is an unknown outcome",
        reconnect_after_submission_is_an_unknown_outcome,
    ),
    (
        "last handle drop closes before runtime stop",
        last_handle_drop_closes_before_runtime_stop,
    ),
];

/// Each bound below, at, and above its range: out-of-range bounds refuse as
/// `InvalidConfig` with no connection; the boundary values connect.
fn builder_bounds_refuse_before_io() -> Row {
    let peer = NatsPeer::start();
    let day = Duration::from_secs(24 * 60 * 60);
    let base = nats::builder(&peer.url());
    let invalid = out_of_range_builders(&base, day + Duration::from_nanos(1));
    let boundary = base
        .operation_timeout(day)
        .connect_timeout(day)
        .shutdown_timeout(day)
        .max_in_flight(1)
        .max_message_bytes(1)
        .subscription_capacity(1)
        .client_capacity(1)
        .max_subscriptions(1);
    let outcome = runtime::builder().run(move || -> Row {
        for (what, builder) in invalid {
            expect_refused(
                what,
                row_bounded(what, builder.connect())?,
                (
                    IntegrationOperation::Connect,
                    IntegrationFailure::InvalidConfig,
                    Retryability::Never,
                ),
            )?;
        }
        let connection = settled("connect", boundary.connect())?;
        expect_ok("boundary close", row_bounded("close", connection.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        expect_eq(
            "connections the peer accepted",
            peer.control().log().accepted,
            1,
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Every builder bound set just outside its range from `base`, where `over`
/// is the first duration past the timeout maximum, plus an unparsable URL.
fn out_of_range_builders(base: &NatsBuilder, over: Duration) -> Box<[(&'static str, NatsBuilder)]> {
    Box::new([
        (
            "zero operation timeout",
            base.clone().operation_timeout(Duration::ZERO),
        ),
        (
            "operation timeout over a day",
            base.clone().operation_timeout(over),
        ),
        (
            "zero connect timeout",
            base.clone().connect_timeout(Duration::ZERO),
        ),
        (
            "connect timeout over a day",
            base.clone().connect_timeout(over),
        ),
        (
            "zero shutdown timeout",
            base.clone().shutdown_timeout(Duration::ZERO),
        ),
        (
            "shutdown timeout over a day",
            base.clone().shutdown_timeout(over),
        ),
        ("zero max_in_flight", base.clone().max_in_flight(0)),
        (
            "overflowing max_in_flight",
            base.clone().max_in_flight(usize::MAX),
        ),
        ("zero max_message_bytes", base.clone().max_message_bytes(0)),
        (
            "overflowing max_message_bytes",
            base.clone().max_message_bytes(usize::MAX),
        ),
        (
            "zero subscription_capacity",
            base.clone().subscription_capacity(0),
        ),
        (
            "overflowing subscription_capacity",
            base.clone().subscription_capacity(usize::MAX),
        ),
        ("zero client_capacity", base.clone().client_capacity(0)),
        (
            "overflowing client_capacity",
            base.clone().client_capacity(usize::MAX),
        ),
        ("zero max_subscriptions", base.clone().max_subscriptions(0)),
        (
            "overflowing max_subscriptions",
            base.clone().max_subscriptions(usize::MAX),
        ),
        ("unparsable url", nats::builder("nats://[::1")),
    ])
}

/// Outside a Camber runtime there is no owner to capture, even inside a Tokio
/// runtime: `NoRuntime`, and the peer sees nothing.
fn no_runtime_refuses_before_io() -> Row {
    let peer = NatsPeer::start();
    let url = peer.url();
    let verdict = on_tokio(nats::connect(&url))
        .and_then(|answer| expect_no_runtime("connect outside Camber", answer))
        .and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// A connect after root admission closed is refused as `ScopeClosed`.
fn closed_admission_refuses_before_io() -> Row {
    let peer = NatsPeer::start();
    let url = peer.url();
    let outcome = runtime::builder().run(move || -> Row {
        runtime::request_shutdown();
        expect_scope_closed(
            "connect after closure",
            row_bounded("connect", nats::connect(&url))?,
        )
    });
    let verdict = clean_run(outcome).and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// With every live integration slot taken, a connect is `Busy` and the peer
/// sees nothing.
fn live_limit_refuses_before_io() -> Row {
    let peer = NatsPeer::start();
    let url = peer.url();
    let outcome = runtime::builder().run(move || -> Row {
        let held = hold_live_slots(LIVE_LIMIT, IntegrationKind::Sqs)?;
        let refused = row_bounded("connect", nats::connect(&url))?;
        drop(held);
        expect_refused(
            "connect at the live limit",
            refused,
            busy(IntegrationOperation::Connect),
        )
    });
    let verdict = clean_run(outcome).and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// Readiness follows the server's handshake answer; publish success follows
/// the flush of a command the peer then reads.
fn readiness_and_publish_name_their_acknowledgements() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let log = peer.control().log();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        expect_ok("ready after connect", connection.ready())?;
        expect_ok(
            "publish",
            row_bounded("publish", connection.publish("orders.created", b"hello"))?,
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        expect_eq("the log before connecting", log.connects, 0)?;
        let log = peer
            .control()
            .wait_for("the published command", ROW_BOUND, |log| {
                !log.published.is_empty()
            })?;
        expect_eq("handshakes", log.connects, 1)?;
        expect_eq(
            "published commands",
            log.published,
            vec![(Box::from("orders.created"), 5)],
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A payload at the maximum publishes; one byte over is refused before it is
/// copied or sent.
fn payload_maximum_refuses_before_copy() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_message_bytes(4);
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        expect_ok(
            "publish at the maximum",
            row_bounded("publish", connection.publish("s", b"1234"))?,
        )?;
        expect_refused(
            "publish over the maximum",
            row_bounded("publish", connection.publish("s", b"12345"))?,
            limit_exceeded(IntegrationOperation::Publish),
        )?;
        expect_ok(
            "publish after the refusal",
            row_bounded("publish", connection.publish("s", b"4321"))?,
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let log = peer
            .control()
            .wait_for("both admitted publishes", ROW_BOUND, |log| {
                log.published.len() >= 2
            })?;
        expect_eq(
            "published commands",
            log.published,
            vec![(Box::from("s"), 4), (Box::from("s"), 4)],
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// With the peer no longer reading, a submitted publish holds the one
/// operation slot: the next is `Busy` before any effect, and the held one
/// reaches its deadline with an unknown outcome. The close that follows cannot
/// see the SDK's closed event and reports an incomplete close, which the
/// runtime aggregate keeps under the instance.
fn operation_limit_and_lost_acknowledgement() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url())
        .max_in_flight(1)
        .max_message_bytes(STALLED_PAYLOAD)
        .operation_timeout(EXHAUSTED)
        .shutdown_timeout(EXHAUSTED);
    let control = peer.control();
    let outcome = run_observing(runtime::builder(), move || -> Row {
        let connection = settled("connect", builder.connect())?;
        control.script(Script::Stall);
        let payload = vec![0_u8; STALLED_PAYLOAD];
        let verdict: Row = runtime::block_on(async {
            let mut held = std::pin::pin!(connection.publish("big", &payload));
            expect_polled_pending("the held publish", &futures_util::poll!(held.as_mut()))?;
            expect_refused(
                "publish past the operation limit",
                connection.publish("small", b"x").await,
                busy(IntegrationOperation::Publish),
            )?;
            expect_refused(
                "the held publish",
                tokio::time::timeout(ROW_BOUND, held)
                    .await
                    .map_err(|_| "the held publish outlived its deadline".to_owned())?,
                (
                    IntegrationOperation::Publish,
                    IntegrationFailure::Timeout,
                    Retryability::OutcomeUnknown,
                ),
            )?;
            expect_refused(
                "close with no closed event",
                tokio::time::timeout(ROW_BOUND, connection.close())
                    .await
                    .map_err(|_| "close outlived its bound".to_owned())?,
                timed_out(IntegrationOperation::Close),
            )
        });
        verdict
    });
    let verdict = expect_failed_run(
        outcome,
        "an incomplete close left no aggregate",
        expect_one_failed_close,
    )
    .and_then(|()| {
        expect_eq(
            "small publishes the peer read",
            published_to(&peer.control().log(), "small"),
            0,
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Fail the row unless the aggregate holds exactly one NATS failure: an
/// incomplete close.
fn expect_one_failed_close(error: &RuntimeError) -> Row {
    expect_aggregate(
        error,
        IntegrationKind::Nats,
        &[timed_out(IntegrationOperation::Close)],
    )
}

/// Fail the row unless the aggregate holds exactly one NATS failure: a
/// slow-consumer overflow charged to receive.
fn expect_one_slow_consumer(error: &RuntimeError) -> Row {
    expect_aggregate(
        error,
        IntegrationKind::Nats,
        &[limit_exceeded(IntegrationOperation::Receive)],
    )
}

/// A connection the SDK reports disconnected refuses a publish as
/// `Unavailable` and queues nothing for later.
fn disconnected_publish_is_unavailable() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).shutdown_timeout(EXHAUSTED);
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        control.script(Script::Refuse);
        wait_unavailable(&connection, ROW_BOUND)?;
        expect_refused(
            "publish while disconnected",
            row_bounded("publish", connection.publish("offline", b"x"))?,
            unavailable(IntegrationOperation::Publish),
        )?;
        expect_refused(
            "subscribe while disconnected",
            row_bounded("subscribe", connection.subscribe("offline"))?,
            unavailable(IntegrationOperation::Subscribe),
        )?;
        drop(connection);
        Ok(())
    });
    let verdict = match outcome {
        Ok(verdict) => verdict,
        // The dropped connection's close cannot reach a refusing peer, so
        // an incomplete close is the one permitted aggregate entry.
        Err(error) => expect_one_failed_close(&error),
    };
    let verdict = verdict.and_then(|()| {
        let log = peer.control().log();
        expect_eq("publishes the peer read", log.published.len(), 0)?;
        expect_eq("subscriptions the peer read", log.subscribed.len(), 0)
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A server authorization violation is `PermissionDenied`, never retryable.
fn authorization_violation_is_permission_denied() -> Row {
    let peer = NatsPeer::start();
    peer.control().script(Script::DenyAuthorization);
    let url = peer.url();
    let outcome = runtime::builder().run(move || -> Row {
        expect_refused(
            "connect with denied authorization",
            row_bounded("connect", nats::connect(&url))?,
            permission_denied(IntegrationOperation::Connect),
        )
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// The subscription limit refuses as `Busy` before any command, and closing a
/// subscription returns its slot.
fn subscription_limit() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_subscriptions(1);
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut first = expect_ok(
            "first subscribe",
            row_bounded("subscribe", connection.subscribe("a"))?,
        )?;
        expect_refused(
            "subscribe past the limit",
            row_bounded("subscribe", connection.queue_subscribe("b", "workers"))?,
            busy(IntegrationOperation::Subscribe),
        )?;
        expect_ok("close the first", row_bounded("close", first.close())?)?;
        let second = expect_ok(
            "subscribe after close",
            row_bounded("subscribe", connection.queue_subscribe("c", "workers"))?,
        )?;
        drop(second);
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let log = peer
            .control()
            .wait_for("both subscriptions", ROW_BOUND, |log| {
                log.subscribed.len() >= 2
            })?;
        expect_eq(
            "subscriptions the peer read",
            log.subscribed
                .iter()
                .map(|(subject, queue, _)| (&**subject, queue.as_deref()))
                .collect::<Vec<_>>(),
            vec![("a", None), ("c", Some("workers"))],
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// `try_next` answers at once: nothing buffered is `None`, a buffered message
/// is delivered. An inbound payload over the maximum is refused undelivered
/// and the subscription stays open.
fn try_next_is_immediate_and_delivery_is_bounded() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_message_bytes(4);
    let control = peer.control();
    let outcome = runtime::builder().run({
        move || -> Row {
            let connection = settled("connect", builder.connect())?;
            let mut subscription = expect_ok(
                "subscribe",
                row_bounded("subscribe", connection.subscribe("events"))?,
            )?;
            let sid = control.sid("events", ROW_BOUND)?;
            expect_eq(
                "try_next with nothing buffered",
                subscription
                    .try_next()
                    .map(|message| message.map(|m| m.payload().to_vec()))
                    .map_err(|error| refusal(&error)),
                Ok(None),
            )?;
            control.deliver(&sid, "events", &[b"one", b"two"])?;
            expect_eq(
                "the first message",
                next_payload(&mut subscription)?,
                b"one"[..].into(),
            )?;
            let second = row_bounded("the buffered message", async {
                loop {
                    match subscription.try_next() {
                        Ok(Some(message)) => return Ok(message.payload().to_vec()),
                        Ok(None) => tokio::task::yield_now().await,
                        Err(error) => return Err(format!("try_next failed: {error:?}")),
                    }
                }
            })??;
            expect_eq("the buffered message", second, b"two".to_vec())?;
            control.deliver(&sid, "events", &[b"12345"])?;
            expect_refused(
                "an inbound payload over the maximum",
                row_bounded("the oversized message", subscription.next())?,
                limit_exceeded(IntegrationOperation::Receive),
            )?;
            control.deliver(&sid, "events", &[b"123"])?;
            expect_eq(
                "the message after the refusal",
                next_payload(&mut subscription)?,
                b"123"[..].into(),
            )?;
            expect_ok("close", row_bounded("close", connection.close())?)
        }
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// A receive bound that passes is a typed `Timeout`; a closed subscription
/// completes with `None`, and `try_next` then reports it closed.
fn timeout_is_distinct_from_completion() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut subscription = expect_ok(
            "subscribe",
            row_bounded("subscribe", connection.subscribe("quiet"))?,
        )?;
        expect_refused(
            "a receive past its bound",
            row_bounded(
                "next_timeout",
                subscription.next_timeout(Duration::from_millis(50)),
            )?,
            expired(IntegrationOperation::Receive),
        )?;
        expect_ok("close", row_bounded("close", subscription.close())?)?;
        expect_ok("close again", row_bounded("close", subscription.close())?)?;
        expect_completed("next after close", &mut subscription)?;
        expect_refused(
            "try_next after close",
            subscription.try_next(),
            closed(IntegrationOperation::Receive),
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// Concurrent and repeated closes read one result, and every clone refuses
/// new work once close is committed.
fn close_is_idempotent_and_refuses_every_clone() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let escaped = connection.clone();
        let (first, second) = row_bounded("concurrent close", async {
            tokio::join!(connection.close(), escaped.close())
        })?;
        expect_ok("first close", first)?;
        expect_ok("concurrent close", second)?;
        expect_ok("repeated close", row_bounded("close", connection.close())?)?;
        expect_refused(
            "ready after close",
            escaped.ready(),
            closed(IntegrationOperation::Ready),
        )?;
        expect_refused(
            "publish after close",
            row_bounded("publish", escaped.publish("late", b"x"))?,
            closed(IntegrationOperation::Publish),
        )?;
        expect_refused(
            "subscribe after close",
            row_bounded("subscribe", escaped.subscribe("late"))?,
            closed(IntegrationOperation::Subscribe),
        )
    });
    let verdict = clean_run(outcome).and_then(|()| {
        expect_eq(
            "publishes the peer read",
            peer.control().log().published.len(),
            0,
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A close drains every subscription: each completes, and the peer reads its
/// unsubscribe.
fn close_completes_every_subscription() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut plain = expect_ok(
            "subscribe",
            row_bounded("subscribe", connection.subscribe("a"))?,
        )?;
        let mut grouped = expect_ok(
            "queue subscribe",
            row_bounded("subscribe", connection.queue_subscribe("b", "workers"))?,
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)?;
        for (what, subscription) in [("plain", &mut plain), ("grouped", &mut grouped)] {
            expect_completed(&format!("{what} subscription after close"), subscription)?;
        }
        Ok(())
    });
    let verdict = clean_run(outcome).and_then(|()| {
        peer.control()
            .wait_for("both unsubscribes", ROW_BOUND, |log: &PeerLog| {
                log.unsubscribed.len() >= 2
            })
            .map(drop)
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Every operation makes progress on a single-worker runtime: none of them
/// blocks the worker it runs on.
fn single_worker_runtime_makes_progress() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let control = peer.control();
    let outcome = runtime::builder().worker_threads(1).run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut subscription = expect_ok(
            "subscribe",
            row_bounded("subscribe", connection.subscribe("progress"))?,
        )?;
        expect_ok(
            "publish",
            row_bounded("publish", connection.publish("progress", b"p"))?,
        )?;
        control.deliver(&control.sid("progress", ROW_BOUND)?, "progress", &[b"p"])?;
        expect_eq(
            "delivered",
            next_payload(&mut subscription)?,
            b"p"[..].into(),
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// A transport that closes every connection before the handshake refuses
/// the connect as `Unavailable`, safe to repeat: nothing was written.
fn connect_refused_by_the_transport_is_unavailable() -> Row {
    let peer = NatsPeer::start();
    peer.control().script(Script::Refuse);
    let url = peer.url();
    let outcome = runtime::builder().run(move || -> Row {
        expect_refused(
            "connect to a refusing transport",
            row_bounded("connect", nats::connect(&url))?,
            unavailable(IntegrationOperation::Connect),
        )
    });
    let verdict = clean_run(outcome).and_then(|()| expect_no_handshake(&peer));
    peer.finished(ROW_BOUND, verdict)
}

/// A server that accepts but never sends `INFO` holds the connect until the
/// connect bound: `Timeout`, safe to repeat.
fn connect_with_no_server_answer_times_out() -> Row {
    let peer = NatsPeer::start();
    peer.control().script(Script::Silent);
    let builder = nats::builder(&peer.url()).connect_timeout(EXHAUSTED);
    let outcome = runtime::builder().run(move || -> Row {
        expect_refused(
            "connect to a silent server",
            row_bounded("connect", builder.connect())?,
            expired(IntegrationOperation::Connect),
        )
    });
    let verdict = clean_run(outcome).and_then(|()| expect_no_handshake(&peer));
    peer.finished(ROW_BOUND, verdict)
}

/// On the default builder a 1 MiB payload publishes and one byte more is
/// refused before it is copied or sent.
fn default_payload_maximum_refuses_before_copy() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let over = vec![0_u8; DEFAULT_MAX_MESSAGE_BYTES + 1];
        expect_ok(
            "publish at the default maximum",
            row_bounded(
                "publish",
                connection.publish("max", &over[..DEFAULT_MAX_MESSAGE_BYTES]),
            )?,
        )?;
        expect_refused(
            "publish over the default maximum",
            row_bounded("publish", connection.publish("over", &over))?,
            limit_exceeded(IntegrationOperation::Publish),
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        expect_eq(
            "published commands",
            peer.control().log().published,
            vec![(Box::from("max"), DEFAULT_MAX_MESSAGE_BYTES)],
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// On the default builder 64 subscriptions register and the next is `Busy`
/// before any command.
fn default_subscription_limit() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut open = Vec::with_capacity(DEFAULT_COUNT);
        for index in 0..DEFAULT_COUNT {
            open.push(expect_ok(
                &format!("subscription {index}"),
                row_bounded("subscribe", connection.subscribe(&format!("s.{index}")))?,
            )?);
        }
        expect_refused(
            "subscribe past the default limit",
            row_bounded("subscribe", connection.subscribe("refused"))?,
            busy(IntegrationOperation::Subscribe),
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let log = peer.control().log();
        expect_eq(
            "subscriptions the peer read",
            log.subscribed.len(),
            DEFAULT_COUNT,
        )?;
        expect(
            "the refused subscription reached the peer",
            log.subscribed
                .iter()
                .all(|(subject, _, _)| &**subject != "refused"),
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Fail the row unless the publish [`hold_transport`] stalled completes once
/// the peer reads again.
async fn released(big: impl Future<Output = Result<(), RuntimeError>>) -> Row {
    expect_ok(
        "the stalled publish",
        tokio::time::timeout(ROW_BOUND, big)
            .await
            .map_err(|_| "the stalled publish never finished".to_owned())?,
    )
}

/// On the default builder 64 submitted operations hold their slots behind a
/// stalled transport and the next is `Busy` before any effect. Once the peer
/// reads again, every held one completes.
fn default_operation_limit_refuses_before_effect() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_message_bytes(STALLED_PAYLOAD);
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let payload = vec![0_u8; STALLED_PAYLOAD];
        let big = hold_transport(&connection, &control, &payload)?;
        let verdict: Row = runtime::block_on(async {
            let mut held = Vec::with_capacity(DEFAULT_COUNT);
            for index in 1..DEFAULT_COUNT {
                let mut next = Box::pin(connection.publish("held", b"x"));
                match futures_util::poll!(next.as_mut()) {
                    Poll::Pending => held.push(next),
                    Poll::Ready(outcome) => {
                        return Err(format!(
                            "operation {index} behind the stall finished: {outcome:?}"
                        ));
                    }
                }
            }
            let mut refused = std::pin::pin!(connection.publish("refused", b"x"));
            let Poll::Ready(refused) = futures_util::poll!(refused.as_mut()) else {
                return Err("the operation past the default limit was admitted".to_owned());
            };
            expect_refused(
                "the operation past the default limit",
                refused,
                busy(IntegrationOperation::Publish),
            )?;
            control.script(Script::Serve);
            released(big).await?;
            for (index, publish) in held.into_iter().enumerate() {
                expect_ok(
                    &format!("held publish {index}"),
                    tokio::time::timeout(ROW_BOUND, publish)
                        .await
                        .map_err(|_| format!("held publish {index} never finished"))?,
                )?;
            }
            Ok(())
        });
        verdict?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let log = peer.control().log();
        expect_eq(
            "held publishes the peer read",
            published_to(&log, "held"),
            DEFAULT_COUNT - 1,
        )?;
        expect_eq(
            "refused publishes the peer read",
            published_to(&log, "refused"),
            0,
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// An invalid subject or queue group is `Rejected` before the SDK queues a
/// command, never retryable.
fn invalid_subscription_is_rejected_before_submission() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let rejected = rejected(IntegrationOperation::Subscribe);
        expect_refused(
            "an invalid subject",
            row_bounded("subscribe", connection.subscribe("bad subject"))?,
            rejected,
        )?;
        expect_refused(
            "an invalid queue group",
            row_bounded("subscribe", connection.queue_subscribe("fine", "bad group"))?,
            rejected,
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        expect_eq(
            "subscriptions the peer read",
            peer.control().log().subscribed.len(),
            0,
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Dropping a subscribe waiter while its flush is held returns its
/// subscription slot at once, and a registration the SDK already queued is
/// withdrawn rather than left live.
fn dropped_subscribe_waiter_withdraws_its_registration() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url())
        .max_message_bytes(STALLED_PAYLOAD)
        .max_subscriptions(1);
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let payload = vec![0_u8; STALLED_PAYLOAD];
        let big = hold_transport(&connection, &control, &payload)?;
        let verdict: Row = runtime::block_on(async {
            let mut dropped = Box::pin(connection.subscribe("dropped"));
            expect_polled_pending("the held subscribe", &futures_util::poll!(dropped.as_mut()))?;
            drop(dropped);
            let mut next = Box::pin(connection.subscribe("next"));
            expect_polled_pending(
                "the subscribe after the drop",
                &futures_util::poll!(next.as_mut()),
            )?;
            control.script(Script::Serve);
            released(big).await?;
            let subscription = expect_ok(
                "the subscribe after the drop",
                tokio::time::timeout(ROW_BOUND, next)
                    .await
                    .map_err(|_| "the subscribe after the drop never finished".to_owned())?,
            )?;
            drop(subscription);
            Ok(())
        });
        verdict?;
        // The peer has read every command written before `next`'s `SUB`,
        // including the dropped one's, so the check below reads a settled log.
        control.sid("next", ROW_BOUND)?;
        control.wait_for("the dropped registration withdrawn", ROW_BOUND, |log| {
            let mut dropped = log
                .subscribed
                .iter()
                .filter(|(subject, _, _)| &**subject == "dropped")
                .peekable();
            dropped.peek().is_some() && dropped.all(|(_, _, sid)| log.unsubscribed.contains(sid))
        })?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// Dropping a publish waiter after the SDK queued its command does not take
/// the command back. The publish waits behind a stalled transport, so its
/// flush cannot finish before the drop. Once the peer reads again it receives
/// that `PUB`, and the runtime aggregate keeps the abandoned publish as an
/// unknown outcome, never a released success.
///
/// The SDK's flush is a local socket flush, not a peer reply, so no peer
/// record can come between a written `PUB` and its flush. The stall is the
/// only deterministic window between submission and acknowledgement.
fn dropped_publish_waiter_after_submission_keeps_an_unknown_outcome() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_message_bytes(STALLED_PAYLOAD);
    let control = peer.control();
    let outcome = run_observing(runtime::builder(), move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let payload = vec![0_u8; STALLED_PAYLOAD];
        let big = hold_transport(&connection, &control, &payload)?;
        let verdict: Row = runtime::block_on(async {
            let mut dropped = Box::pin(connection.publish("dropped", b"x"));
            expect_polled_pending(
                "the publish behind the stall",
                &futures_util::poll!(dropped.as_mut()),
            )?;
            drop(dropped);
            control.script(Script::Serve);
            released(big).await?;
            Ok(())
        });
        verdict?;
        control.wait_for("the dropped publish on the wire", ROW_BOUND, |log| {
            published_to(log, "dropped") > 0
        })?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    let verdict = expect_failed_run(
        outcome,
        "the abandoned publish left no aggregate",
        |error| {
            expect_aggregate(
                error,
                IntegrationKind::Nats,
                &[cancelled(IntegrationOperation::Publish)],
            )
        },
    )
    .and_then(|()| {
        expect_eq(
            "dropped publishes the peer read",
            published_to(&peer.control().log(), "dropped"),
            1,
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A publish and a subscribe submitted behind a stalled transport lose their
/// unwritten commands when the SDK replaces the connection. The new
/// connection's flush proves nothing about them: both report an unknown
/// outcome, never success.
fn reconnect_after_submission_is_an_unknown_outcome() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).max_message_bytes(STALLED_PAYLOAD);
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let payload = vec![0_u8; STALLED_PAYLOAD];
        let big = hold_transport(&connection, &control, &payload)?;
        let verdict: Row = runtime::block_on(async {
            let mut subscribe = Box::pin(connection.subscribe("behind"));
            expect_polled_pending(
                "the held subscribe",
                &futures_util::poll!(subscribe.as_mut()),
            )?;
            let accepted = control.log().accepted;
            control.script(Script::Refuse);
            control.wait_for("a refused reconnect", ROW_BOUND, |log| {
                log.accepted > accepted
            })?;
            control.script(Script::Serve);
            expect_refused(
                "the publish across the reconnect",
                tokio::time::timeout(ROW_BOUND, big)
                    .await
                    .map_err(|_| "the held publish never finished".to_owned())?,
                unknown(IntegrationOperation::Publish),
            )?;
            expect_refused(
                "the subscribe across the reconnect",
                tokio::time::timeout(ROW_BOUND, subscribe)
                    .await
                    .map_err(|_| "the held subscribe never finished".to_owned())?,
                unknown(IntegrationOperation::Subscribe),
            )
        });
        verdict?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// Dropping the last handle requests close while the runtime still runs:
/// the peer reads the drain's unsubscribe and the client's end of stream, and
/// the escaped subscription completes.
fn last_handle_drop_closes_before_runtime_stop() -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url());
    let control = peer.control();
    let outcome = runtime::builder().run(move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut subscription = expect_ok(
            "subscribe",
            row_bounded("subscribe", connection.subscribe("live"))?,
        )?;
        let sid = control.sid("live", ROW_BOUND)?;
        drop(connection);
        control.wait_for("the client's close", ROW_BOUND, |log| {
            log.closed >= 1 && log.unsubscribed.contains(&sid)
        })?;
        expect_completed(
            "the subscription after the last handle dropped",
            &mut subscription,
        )
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

// ── 5.T2 ──────────────────────────────────────────────────────────────

#[test]
fn default_inbound_payload_limit_is_one_mebibyte() {
    run_rows(&[(
        "default received payload boundary",
        default_inbound_payload_boundary,
    )]);
}

fn default_inbound_payload_boundary() -> Row {
    let peer = NatsPeer::start();
    let outcome = runtime::builder().run(|| -> Row {
        let connection = settled("connect", nats::builder(&peer.url()).connect())?;
        let mut subscription = expect_ok(
            "subscribe",
            row_bounded("subscribe", connection.subscribe("size"))?,
        )?;
        let control = peer.control();
        let sid = control.sid("size", ROW_BOUND)?;
        let payload = vec![b'x'; DEFAULT_MAX_MESSAGE_BYTES];
        control.deliver(&sid, "size", &[&payload])?;
        expect_eq(
            "default boundary payload",
            next_payload(&mut subscription)?,
            payload.into_boxed_slice(),
        )?;
        control.deliver(&sid, "size", &[&vec![b'x'; DEFAULT_MAX_MESSAGE_BYTES + 1]])?;
        expect_refused(
            "default overflow",
            row_bounded("oversized receive", subscription.next())?,
            limit_exceeded(IntegrationOperation::Receive),
        )?;
        control.deliver(&sid, "size", &[b"after"])?;
        expect_eq(
            "message after overflow",
            next_payload(&mut subscription)?,
            b"after"[..].into(),
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

#[test]
fn default_subscription_queue_holds_exactly_sixty_four_messages() {
    let peer = NatsPeer::start();
    let outcome = run_observing(runtime::builder(), || -> Row {
        let connection = settled("connect", nats::builder(&peer.url()).connect())?;
        let mut subscription = expect_ok(
            "subscribe",
            row_bounded("subscribe", connection.subscribe("queue"))?,
        )?;
        let control = peer.control();
        let sid = control.sid("queue", ROW_BOUND)?;
        control.deliver(&sid, "queue", &[b"x".as_slice(); DEFAULT_COUNT])?;
        control.delivery_barrier(ROW_BOUND)?;
        expect_ok("ready with a full default queue", connection.ready())?;
        control.deliver(&sid, "queue", &[b"overflow"])?;
        row_bounded("SDK slow-consumer event", async {
            while connection.ready().is_ok() {
                tokio::task::yield_now().await;
            }
        })?;
        expect_eq(
            "messages buffered at the default capacity",
            drained(&mut subscription)?,
            DEFAULT_COUNT,
        )?;
        expect_refused(
            "close after actual overflow",
            row_bounded("close", connection.close())?,
            limit_exceeded(IntegrationOperation::Receive),
        )
    });
    let verdict = expect_failed_run(
        outcome,
        "overflow left no aggregate",
        expect_one_slow_consumer,
    );
    peer.finished(ROW_BOUND, verdict)
        .expect("default subscription boundary");
}

#[test]
fn client_queue_default_and_override_bound_actual_sdk_admission() {
    for (capacity, override_capacity) in [(DEFAULT_COUNT, None), (2, Some(2))] {
        client_queue_boundary(capacity, override_capacity).expect("SDK client queue boundary");
    }
}

fn client_queue_boundary(capacity: usize, override_capacity: Option<usize>) -> Row {
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url())
        .max_message_bytes(STALLED_PAYLOAD)
        .max_in_flight(128);
    let builder = match override_capacity {
        Some(capacity) => builder.client_capacity(capacity),
        None => builder,
    };
    let outcome = runtime::builder().run(|| -> Row {
        let mut connection = settled("connect", builder.connect())?;
        let mut probe = NatsQueueProbe::hold(&mut connection).ok_or("connection already shared")?;
        let control = peer.control();
        let payload = vec![0; STALLED_PAYLOAD];
        let big = hold_transport(&connection, &control, &payload)?;
        let mut queued = Vec::new();
        for _ in 0..=capacity {
            let mut publish = Box::pin(connection.publish("queued", b"x"));
            expect_pending("queued publish", publish.as_mut())?;
            queued.push(publish);
        }
        let admitted = row_bounded("all SDK admission polls", probe.polled(capacity + 2))?;
        let boundary = expect_eq(
            "SDK admissions including stalled write",
            admitted,
            Some(capacity + 1),
        );
        probe.release();
        control.script(Script::Serve);
        row_bounded("stalled publish completion", released(big))??;
        for publish in queued {
            expect_ok(
                "queued publish completion",
                row_bounded("queued publish", publish)?,
            )?;
        }
        expect_ok("close", row_bounded("close", connection.close())?)?;
        boundary
    });
    peer.finished(ROW_BOUND, clean_run(outcome))
}

/// A delivered slow-consumer event closes the whole connection: close
/// commits at once, every subscription completes after at most its buffered
/// messages, every closer reads the charged failure, and the runtime
/// aggregate keeps exactly that one account.
#[test]
fn delivered_slow_consumer_event_closes_every_subscription() {
    const CAPACITY: usize = 2;
    let peer = NatsPeer::start();
    let builder = nats::builder(&peer.url()).subscription_capacity(CAPACITY);
    let control = peer.control();
    let outcome = run_observing(runtime::builder(), move || -> Row {
        let connection = settled("connect", builder.connect())?;
        let mut flooded = expect_ok(
            "subscribe",
            row_bounded("subscribe", connection.subscribe("flood"))?,
        )?;
        let mut grouped = expect_ok(
            "queue subscribe",
            row_bounded("subscribe", connection.queue_subscribe("calm", "workers"))?,
        )?;
        let flood: [&[u8]; CAPACITY] = [b"1", b"2"];
        control.deliver(&control.sid("flood", ROW_BOUND)?, "flood", &flood)?;
        control.delivery_barrier(ROW_BOUND)?;

        NatsEventProbe::deliver_slow_consumer(&connection);

        let slow_consumer = limit_exceeded(IntegrationOperation::Receive);
        expect_refused(
            "ready after the event",
            connection.ready(),
            closed(IntegrationOperation::Ready),
        )?;
        let retained = drained(&mut flooded)?;
        expect_eq("messages buffered before the event", retained, CAPACITY)?;
        expect_completed("the grouped subscription", &mut grouped)?;
        expect_refused(
            "close after the event",
            row_bounded("close", connection.close())?,
            slow_consumer,
        )?;
        expect_refused(
            "a second close",
            row_bounded("close", connection.close())?,
            slow_consumer,
        )
    });
    let verdict = expect_failed_run(
        outcome,
        "the slow-consumer close left no aggregate",
        expect_one_slow_consumer,
    );
    if let Err(reason) = peer.finished(ROW_BOUND, verdict) {
        panic!("{reason}");
    }
}

/// Take messages until `subscription` completes, within the hang guard, and
/// count them.
fn drained(subscription: &mut Subscription) -> Result<usize, String> {
    let mut count = 0_usize;
    loop {
        match row_bounded("the draining subscription", subscription.next())? {
            Ok(Some(_)) => count += 1,
            Ok(None) => return Ok(count),
            Err(error) => return Err(format!("the subscription failed: {error:?}")),
        }
    }
}
