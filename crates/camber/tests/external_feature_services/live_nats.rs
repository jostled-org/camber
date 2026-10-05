#![cfg(feature = "nats")]
//! 5.T3: the Core NATS contract against a real nats-server.
//!
//! Selected through `.github/scripts/check-local-integrations.sh nats`, which
//! owns the server container and passes its address in `NATS_URL`. Each row
//! runs in its own Camber runtime through the public async API. A loopback
//! relay the row owns stands between the SDK and the server where a row needs
//! to cut the transport and let it recover.
//!
//! The retained predicates of the old per-feature cases live here: a published
//! payload arrives intact, a queue group delivers each message exactly once,
//! and concurrent handlers connect, subscribe, and publish without blocking
//! their workers.
//!
//! `nats_local_jetstream_publish_matrix` proves acknowledged publishing against
//! the lane's JetStream server. A test-only [`StreamOwner`] creates each stream
//! before Camber connects and reads stored messages back through its own
//! client. Core subscriptions stay owned by `nats_local_contract_matrix`.

use crate::common;
use crate::integration_rows::{
    ROW_BOUND, Row, assert_verdicts, clean_run, expect, expect_eq, expect_ok, expect_refused,
    integration_aggregate, next_payload, refusal, rejected, row_bounded, settled, unavailable,
};
use crate::jetstream_streams::{Stored, StreamOwner};
use crate::resources::{ExternalResourceError, ExternalRun, lane_variable, selected_run};
use crate::tcp_relay::Relay;

use camber::http::{Request, Response, Router};
use camber::mq::nats::{self, Connection};
use camber::{IntegrationFailure, IntegrationKind, IntegrationOperation, RuntimeError, runtime};
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::Duration;

/// Concurrent handler requests in the worker-progress row.
const ASYNC_REQUEST_COUNT: usize = 5;

/// Messages the queue-group row publishes.
const QUEUE_MESSAGES: usize = 10;

/// How long a row waits to be sure nothing more arrives, after the one
/// message it expects.
const QUIET: Duration = Duration::from_millis(500);

/// The slow-consumer row's subscription capacity.
const SLOW_CAPACITY: usize = 2;

/// Messages the slow-consumer row floods a capacity-bounded subscription with.
const FLOOD: usize = 256;

/// The operation and failure an integration error carries.
fn failure_of(error: &RuntimeError) -> Option<(IntegrationOperation, IntegrationFailure)> {
    refusal(error).map(|(operation, failure, _)| (operation, failure))
}

/// Poll the connection's committed readiness until `reached` holds.
fn wait_ready(
    connection: &Connection,
    what: &str,
    reached: fn(&Result<(), RuntimeError>) -> bool,
) -> Row {
    row_bounded(what, async {
        while !reached(&connection.ready()) {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
}

/// Every NATS failure in a lifecycle aggregate, by operation and failure.
fn nats_aggregate(
    error: &RuntimeError,
) -> Result<Box<[(IntegrationOperation, IntegrationFailure)]>, String> {
    Ok(integration_aggregate(error, IntegrationKind::Nats)?
        .into_iter()
        .map(|(_, (operation, failure, _))| (operation, failure))
        .collect())
}

#[test]
#[ignore = "external lane nats; owner: Camber runtime integrations; run: .github/scripts/check-local-integrations.sh nats"]
fn nats_local_contract_matrix() {
    let (run, witness) = selected_run().expect("a valid external run ID and witness path");
    let url = lane_variable("NATS_URL").expect("the lane's NATS server URL");
    let subjects = Subjects::new(&run);

    let rows: [(&str, Row); 6] = [
        (
            "publish and subscribe",
            publish_and_subscribe(&url, &subjects),
        ),
        ("queue group", queue_group(&url, &subjects)),
        (
            "single worker progress",
            single_worker_progress(&url, &subjects),
        ),
        ("concurrent handlers", concurrent_handlers(&url, &subjects)),
        (
            "disconnect and reconnect",
            disconnect_and_reconnect(&url, &subjects),
        ),
        (
            "slow consumer retention",
            slow_consumer_retention(&url, &subjects),
        ),
    ];
    assert_verdicts("the NATS contract matrix", rows);

    witness
        .emit(&run, &subjects.all())
        .expect("emit cleanup witness after every NATS connection closed");
}

/// The run-scoped subjects and group every row uses.
struct Subjects {
    publish: Box<str>,
    queue: Box<str>,
    group: Box<str>,
    progress: Box<str>,
    handlers: Arc<str>,
    reconnect: Box<str>,
    slow: Box<str>,
}

impl Subjects {
    fn new(run: &ExternalRun) -> Self {
        Self {
            publish: run.nats_subject("publish-and-subscribe"),
            queue: run.nats_subject("queue-group"),
            group: run.nats_queue_group("queue-group"),
            progress: run.nats_subject("single-worker"),
            handlers: run.nats_subject("async-worker").into(),
            reconnect: run.nats_subject("reconnect"),
            slow: run.nats_subject("slow-consumer"),
        }
    }

    fn all(&self) -> [&str; 7] {
        [
            &self.publish,
            &self.queue,
            &self.group,
            &self.progress,
            &self.handlers,
            &self.reconnect,
            &self.slow,
        ]
    }
}

/// A published payload arrives intact, and close settles the connection.
fn publish_and_subscribe(url: &str, subjects: &Subjects) -> Row {
    let url = url.to_owned();
    let subject = subjects.publish.clone();
    clean_run(runtime::builder().run(move || -> Row {
        let connection = settled("connect", nats::connect(&url))?;
        let mut subscription = expect_ok(
            "subscribe",
            row_bounded("subscribe", connection.subscribe(&subject))?,
        )?;
        expect_ok(
            "publish",
            row_bounded("publish", connection.publish(&subject, b"hello nats"))?,
        )?;
        expect_eq(
            "payload",
            next_payload(&mut subscription)?,
            b"hello nats"[..].into(),
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)?;
        expect_eq(
            "subscription after close",
            row_bounded("next", subscription.next())?
                .map(|message| message.is_some())
                .map_err(|e| failure_of(&e)),
            Ok(false),
        )
    }))
}

/// Two members of one queue group receive every message exactly once
/// between them.
fn queue_group(url: &str, subjects: &Subjects) -> Row {
    let url = url.to_owned();
    let subject = subjects.queue.clone();
    let group = subjects.group.clone();
    clean_run(runtime::builder().run(move || -> Row {
        let connection = settled("connect", nats::connect(&url))?;
        let mut members = [
            expect_ok(
                "member a",
                row_bounded("subscribe", connection.queue_subscribe(&subject, &group))?,
            )?,
            expect_ok(
                "member b",
                row_bounded("subscribe", connection.queue_subscribe(&subject, &group))?,
            )?,
        ];
        for index in 0..QUEUE_MESSAGES {
            expect_ok(
                "publish",
                row_bounded(
                    "publish",
                    connection.publish(&subject, format!("msg-{index}").as_bytes()),
                )?,
            )?;
        }
        let mut received = Vec::new();
        row_bounded("every queued message", async {
            while received.len() < QUEUE_MESSAGES {
                for member in &mut members {
                    let buffered = member
                        .try_next()
                        .map_err(|error| format!("receive: {error:?}"))?;
                    received.extend(buffered.map(|message| message.payload().to_vec()));
                }
                tokio::task::yield_now().await;
            }
            Ok::<(), String>(())
        })??;
        let extra = row_bounded("the quiet period", async {
            let mut extra = 0_usize;
            for member in &mut members {
                match tokio::time::timeout(QUIET, member.next()).await {
                    Ok(Ok(Some(_))) => extra += 1,
                    Ok(Ok(None)) | Err(_) => {}
                    Ok(Err(error)) => {
                        return Err(format!("receive in the quiet period: {error:?}"));
                    }
                }
            }
            Ok(extra)
        })??;
        expect_eq("messages received", received.len(), QUEUE_MESSAGES)?;
        received.sort();
        received.dedup();
        expect_eq("distinct messages", received.len(), QUEUE_MESSAGES)?;
        expect_eq("messages delivered twice", extra, 0)?;
        expect_ok("close", row_bounded("close", connection.close())?)
    }))
}

/// Every operation completes on a runtime with one worker.
fn single_worker_progress(url: &str, subjects: &Subjects) -> Row {
    let url = url.to_owned();
    let subject = subjects.progress.clone();
    clean_run(runtime::builder().worker_threads(1).run(move || -> Row {
        let connection = settled("connect", nats::connect(&url))?;
        let mut subscription = expect_ok(
            "subscribe",
            row_bounded("subscribe", connection.subscribe(&subject))?,
        )?;
        expect_ok(
            "publish",
            row_bounded("publish", connection.publish(&subject, b"async msg"))?,
        )?;
        expect_eq(
            "payload",
            next_payload(&mut subscription)?,
            b"async msg"[..].into(),
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)
    }))
}

/// Concurrent handlers each connect, subscribe, and publish, then all meet
/// at one barrier: none of them blocks the worker it runs on.
fn concurrent_handlers(url: &str, subjects: &Subjects) -> Row {
    let url: Arc<str> = url.into();
    let subject = Arc::clone(&subjects.handlers);
    clean_run(common::test_runtime().run(move || -> Row {
        let (entered_sender, entered_receiver) = mpsc::channel();
        let barrier = Arc::new(tokio::sync::Barrier::new(ASYNC_REQUEST_COUNT));
        let router = handler_router(url, subject, entered_sender, barrier);
        let addr = common::spawn_server(router);
        let requests: Box<[JoinHandle<Box<str>>]> = (0..ASYNC_REQUEST_COUNT)
            .map(|_| {
                std::thread::spawn(move || {
                    crate::http::raw_request(addr, "GET", "/nats-async", &[])
                })
            })
            .collect();
        for _ in 0..ASYNC_REQUEST_COUNT {
            entered_receiver
                .recv_timeout(ROW_BOUND)
                .map_err(|_| "a handler never entered the overlap barrier".to_owned())?;
        }
        for request in requests {
            let response = request
                .join()
                .map_err(|_| "a request thread panicked".to_owned())?;
            expect_eq(
                &format!("handler status for {response}"),
                crate::http::status_from_raw(&response),
                200,
            )?;
        }
        runtime::request_shutdown();
        Ok(())
    }))
}

fn handler_router(
    url: Arc<str>,
    subject: Arc<str>,
    entered: mpsc::Sender<()>,
    barrier: Arc<tokio::sync::Barrier>,
) -> Router {
    let mut router = Router::new();
    router.get("/nats-async", move |_req: &Request| {
        let url = Arc::clone(&url);
        let subject = Arc::clone(&subject);
        let entered = entered.clone();
        let barrier = Arc::clone(&barrier);
        async move {
            let connection = match nats::connect(&url).await {
                Ok(connection) => connection,
                Err(error) => return Response::text(500, &format!("connect: {error}")),
            };
            let subscription = match connection.subscribe(&subject).await {
                Ok(subscription) => subscription,
                Err(error) => return Response::text(500, &format!("subscribe: {error}")),
            };
            if let Err(error) = connection.publish(&subject, b"ping").await {
                return Response::text(500, &format!("publish: {error}"));
            }
            if entered.send(()).is_err() {
                return Response::text(500, "evidence channel closed");
            }
            if tokio::time::timeout(ROW_BOUND, barrier.wait())
                .await
                .is_err()
            {
                return Response::text(500, "overlap barrier timed out");
            }
            drop(subscription);
            match connection.close().await {
                Ok(()) => Response::text(200, "ok"),
                Err(error) => Response::text(500, &format!("close: {error}")),
            }
        }
    });
    router
}

/// With the transport cut, publish is `Unavailable` and nothing queues; once
/// the SDK reconnects, the subscription carries on and the refused message
/// never arrives.
fn disconnect_and_reconnect(url: &str, subjects: &Subjects) -> Row {
    let upstream = upstream_address(url)?;
    let relay = Relay::start(upstream, ROW_BOUND)?;
    let relay_url = format!("nats://{}", relay.address());
    let cut = relay.control();
    let subject = subjects.reconnect.clone();
    let verdict = clean_run(runtime::builder().run(move || -> Row {
        let connection = settled("connect", nats::connect(&relay_url))?;
        let mut subscription = expect_ok(
            "subscribe",
            row_bounded("subscribe", connection.subscribe(&subject))?,
        )?;
        cut.set_refusing(true);
        wait_ready(&connection, "the SDK disconnect", |ready| {
            matches!(
                ready.as_ref().map_err(failure_of),
                Err(Some((_, IntegrationFailure::Unavailable)))
            )
        })?;
        expect_eq(
            "publish while disconnected",
            row_bounded("publish", connection.publish(&subject, b"offline"))?
                .map_err(|error| failure_of(&error)),
            Err(Some((
                IntegrationOperation::Publish,
                IntegrationFailure::Unavailable,
            ))),
        )?;
        cut.set_refusing(false);
        wait_ready(&connection, "the SDK reconnect", Result::is_ok)?;
        expect_ok(
            "publish after reconnect",
            row_bounded("publish", connection.publish(&subject, b"online"))?,
        )?;
        expect_eq(
            "the first message after reconnect",
            next_payload(&mut subscription)?,
            b"online"[..].into(),
        )?;
        expect_ok("close", row_bounded("close", connection.close())?)
    }));
    verdict.and(relay.finish())
}

/// A subscription bounded to a small capacity never retains more than it;
/// a slow-consumer event the SDK delivers closes the connection and is
/// reported once. The SDK may drop that event, so both outcomes are allowed.
fn slow_consumer_retention(url: &str, subjects: &Subjects) -> Row {
    let url = url.to_owned();
    let subject = subjects.slow.clone();
    let outcome = runtime::builder().run(move || -> Row {
        let slow = expect_ok(
            "connect",
            row_bounded(
                "connect",
                nats::builder(&url)
                    .subscription_capacity(SLOW_CAPACITY)
                    .connect(),
            )?,
        )?;
        let mut subscription = expect_ok(
            "subscribe",
            row_bounded("subscribe", slow.subscribe(&subject))?,
        )?;
        let publisher = settled("connect", nats::connect(&url))?;
        for index in 0..FLOOD {
            expect_ok(
                "flood publish",
                row_bounded(
                    "publish",
                    publisher.publish(&subject, format!("{index}").as_bytes()),
                )?,
            )?;
        }
        expect_ok("publisher close", row_bounded("close", publisher.close())?)?;
        let mut retained = 0_usize;
        while let Ok(Ok(Some(_))) =
            runtime::block_on(tokio::time::timeout(QUIET, subscription.next()))
        {
            retained += 1;
        }
        expect(
            &format!("{retained} of {FLOOD} messages reached the application"),
            retained < FLOOD,
        )?;
        match row_bounded("close", slow.close())? {
            Ok(()) => Ok(()),
            Err(error) => expect_eq(
                "a slow-consumer close",
                failure_of(&error),
                Some((
                    IntegrationOperation::Receive,
                    IntegrationFailure::LimitExceeded,
                )),
            ),
        }
    });
    match outcome {
        Ok(verdict) => verdict,
        Err(error) => expect_eq(
            "aggregate NATS failures",
            nats_aggregate(&error)?,
            Box::from([(
                IntegrationOperation::Receive,
                IntegrationFailure::LimitExceeded,
            )]),
        ),
    }
}

/// The bytes the stored-publish row publishes; not valid UTF-8, so readback
/// proves bytes, not text.
const STORED_PAYLOAD: &[u8] = b"stored \x00\xff bytes";

#[test]
#[ignore = "external lane nats; owner: Camber runtime integrations; run: .github/scripts/check-local-integrations.sh nats external_nats::nats_local_jetstream_publish_matrix"]
fn nats_local_jetstream_publish_matrix() {
    let (run, witness) = selected_run().expect("a valid external run ID and witness path");
    let url = lane_variable("NATS_URL").expect("the lane's NATS server URL");
    let streams = Streams::new(&run).expect("valid run-scoped JetStream names");
    let mut owner = StreamOwner::connect(&url).expect("connect the test-only stream owner");

    let rows = [
        ("stored publish", stored_publish(&url, &streams, &mut owner)),
        ("missing stream", missing_stream(&url, &streams, &owner)),
        (
            "mismatched stream",
            mismatched_stream(&url, &streams, &mut owner),
        ),
        ("stream and client cleanup", owner.finish()),
    ];
    assert_verdicts("the NATS JetStream publish matrix", rows);

    witness
        .emit(&run, &streams.all())
        .expect("emit cleanup witness after every stream was deleted and every client closed");
}

/// The run-scoped streams and subjects the JetStream rows use.
struct Streams {
    stored: Box<str>,
    stored_subject: Box<str>,
    missing: Box<str>,
    missing_subject: Box<str>,
    captured: Box<str>,
    captured_subject: Box<str>,
    expected_elsewhere: Box<str>,
}

impl Streams {
    fn new(run: &ExternalRun) -> Result<Self, ExternalResourceError> {
        Ok(Self {
            stored: run.nats_stream("stored")?,
            stored_subject: run.nats_subject("jetstream-stored"),
            missing: run.nats_stream("missing")?,
            missing_subject: run.nats_subject("jetstream-missing"),
            captured: run.nats_stream("captured")?,
            captured_subject: run.nats_subject("jetstream-captured"),
            expected_elsewhere: run.nats_stream("expected-elsewhere")?,
        })
    }

    fn all(&self) -> [&str; 7] {
        [
            &self.stored,
            &self.stored_subject,
            &self.missing,
            &self.missing_subject,
            &self.captured,
            &self.captured_subject,
            &self.expected_elsewhere,
        ]
    }
}

/// Publish once through an acknowledged connection expecting `stream`, then
/// close it. The row's verdict carries the publish outcome.
fn acknowledged_publish(
    url: &str,
    stream: &str,
    subject: &str,
    check: fn(Result<(), RuntimeError>) -> Row,
) -> Row {
    let url = url.to_owned();
    let stream = stream.to_owned();
    let subject = subject.to_owned();
    clean_run(runtime::builder().run(move || -> Row {
        let connection = expect_ok(
            "connect",
            row_bounded(
                "connect",
                nats::builder(&url)
                    .acknowledged_publishing(&stream)
                    .connect(),
            )?,
        )?;
        let published = check(row_bounded(
            "publish",
            connection.publish(&subject, STORED_PAYLOAD),
        )?);
        let closed = expect_ok("close", row_bounded("close", connection.close())?);
        published.and(closed)
    }))
}

/// An acknowledged publish succeeds, and the stream holds exactly that
/// subject and those bytes.
fn stored_publish(url: &str, streams: &Streams, owner: &mut StreamOwner) -> Row {
    owner.create(&streams.stored, &streams.stored_subject)?;
    acknowledged_publish(url, &streams.stored, &streams.stored_subject, |published| {
        expect_ok("acknowledged publish", published)
    })?;
    let expected: Stored = (streams.stored_subject.clone(), STORED_PAYLOAD.into());
    expect_eq(
        "stored messages",
        owner.stored(&streams.stored)?,
        Box::from([expected]),
    )
}

/// With no stream capturing the subject, publish fails as no responders and
/// never falls back to Core; nothing creates the stream.
fn missing_stream(url: &str, streams: &Streams, owner: &StreamOwner) -> Row {
    acknowledged_publish(
        url,
        &streams.missing,
        &streams.missing_subject,
        |published| {
            expect_refused(
                "publish without a stream",
                published,
                unavailable(IntegrationOperation::Publish),
            )
        },
    )?;
    owner.absent(&streams.missing)
}

/// A stream captures the subject, but the connection expects another: the
/// server rejects the publish and the capturing stream stores nothing.
fn mismatched_stream(url: &str, streams: &Streams, owner: &mut StreamOwner) -> Row {
    owner.create(&streams.captured, &streams.captured_subject)?;
    acknowledged_publish(
        url,
        &streams.expected_elsewhere,
        &streams.captured_subject,
        |published| {
            expect_refused(
                "publish expecting another stream",
                published,
                rejected(IntegrationOperation::Publish),
            )
        },
    )?;
    expect_eq(
        "messages stored after the rejection",
        owner.stored(&streams.captured)?,
        Box::default(),
    )
}

/// The server address behind `url`.
fn upstream_address(url: &str) -> Result<SocketAddr, String> {
    let authority = url.trim_start_matches("nats://");
    std::net::ToSocketAddrs::to_socket_addrs(authority)
        .map_err(|error| format!("resolve {authority}: {error}"))?
        .next()
        .ok_or_else(|| format!("{authority} resolved to nothing"))
}
