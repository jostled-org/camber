#![cfg(feature = "sqs")]
//! 6.T2: the SQS Standard-queue contract against a real ElasticMQ.
//!
//! Selected through `.github/scripts/check-local-integrations.sh sqs`, which
//! owns the ElasticMQ container and passes its loopback address in
//! `CAMBER_LOCAL_ELASTICMQ_QUERY`. Every Camber client names its endpoint,
//! region, and dummy credentials explicitly, so no row reads the process
//! environment or cloud metadata, and the real AWS SDK transport carries every
//! request.
//!
//! Fixture setup creates one unique Standard queue through the SDK directly;
//! that is not product behavior. Each row then runs in its own Camber runtime
//! through the public async API. Finish deletes the queue and proves it gone
//! before the cleanup witness names it.
//!
//! The runtime-stop row puts a loopback relay it owns between the SDK and
//! ElasticMQ. The relay records every byte the client sends, so the row
//! observes the receive on the wire before it stops the runtime, and reads
//! which access key signed it.

use crate::integration_rows::{
    Row, assert_verdicts, bounded, cancelled, clean_run, expect, expect_eq, expect_ok,
    expect_polled_pending, expect_refused, refusal, rejected, settled_within,
};
use crate::resources::{lane_address, selected_run};
use crate::tcp_relay::Relay;

use camber::mq::sqs::{self, Message, SqsBuilder};
use camber::{IntegrationOperation, runtime};
use std::future::Future;
use std::net::SocketAddr;
use std::pin::pin;
use std::time::Duration;

/// The hang guard every bounded wait runs under; never a timing assertion.
const ROW_BOUND: Duration = Duration::from_secs(30);

/// The variable the lane runner publishes ElasticMQ's loopback address in.
const ADDRESS_VARIABLE: &str = "CAMBER_LOCAL_ELASTICMQ_QUERY";

/// The dummy credentials every client names. They name no account, and they
/// differ from the `AWS_*` values the lane runner exports, so a request the
/// relay records signed with this key proves the explicit credentials were
/// used.
const ACCESS_KEY: &str = "camber-explicit";
const SECRET_KEY: &str = "camber-explicit-secret";
const REGION: &str = "us-east-1";

/// The queue's visibility timeout: a received message the caller does not
/// delete is delivered again after it.
const VISIBILITY_SECONDS: &str = "1";

/// The longest receive wait the service allows.
const LONG_POLL: Duration = Duration::from_secs(20);

fn endpoint_of(address: SocketAddr) -> String {
    format!("http://{address}")
}

/// How many times `needle` occurs in `haystack`.
fn occurrences(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

/// The message IDs of a received batch, in order.
fn message_ids(received: &[Message]) -> Box<[Option<&str>]> {
    received.iter().map(Message::message_id).collect()
}

/// The receipt handle `message` must carry for a delete.
fn receipt_of(message: &Message) -> Result<&str, String> {
    message
        .receipt_handle()
        .ok_or_else(|| "a received message carried no receipt".to_owned())
}

fn within<F: Future>(what: &str, future: F) -> Result<F::Output, String> {
    bounded(what, ROW_BOUND, future)
}

fn local(endpoint: &str) -> SqsBuilder {
    sqs::builder()
        .endpoint(endpoint)
        .region(REGION)
        .credentials(ACCESS_KEY, SECRET_KEY, None)
}

#[test]
#[ignore = "external lane sqs; owner: Camber runtime integrations; run: .github/scripts/check-local-integrations.sh sqs"]
fn sqs_local_standard_queue_contract_matrix() {
    let (run, witness) = selected_run().expect("a valid external run ID and witness path");
    let address = lane_address(ADDRESS_VARIABLE).expect("the lane's ElasticMQ address");
    let endpoint = endpoint_of(address);
    let name = run
        .sqs_queue("matrix")
        .expect("a valid run-scoped queue name");
    let queue = QueueFixture::create(&endpoint, &name).expect("create the run's Standard queue");

    let rows: [(&str, Row); 5] = [
        (
            "ready, send, receive, and delete",
            send_receive_delete(&endpoint, &queue.url),
        ),
        (
            "visibility timeout redelivers",
            visibility_redelivers(&endpoint, &queue.url),
        ),
        (
            "explicit refusals",
            explicit_refusals(&endpoint, &queue.url),
        ),
        (
            "single worker progress",
            single_worker_progress(&endpoint, &queue.url),
        ),
        // Last: the service may keep the cut receive's long poll open, and
        // no later row may send a message it could take.
        ("runtime stop", runtime_stop(address, &queue.url)),
    ];
    // The queue is deleted after every row, and its cleanup is one more verdict.
    assert_verdicts(
        "the SQS Standard-queue contract matrix",
        rows.into_iter().chain([("queue cleanup", queue.finish())]),
    );

    witness
        .emit(&run, &[&name])
        .expect("emit cleanup witness after the queue was deleted");
}

/// A queue query succeeds, a sent message comes back with its identity and
/// body, its receipt deletes it, and nothing is delivered after.
fn send_receive_delete(endpoint: &str, queue: &str) -> Row {
    let builder = local(endpoint);
    let queue = queue.to_owned();
    clean_run(runtime::builder().run(move || -> Row {
        let client = settled_within("connect", ROW_BOUND, builder.connect())?;
        expect_ok("ready", within("ready", client.ready(&queue))?)?;
        let id = expect_ok(
            "send",
            within("send", client.send_message(&queue, "hello sqs"))?,
        )?;
        let received = expect_ok(
            "receive",
            within("receive", client.receive_messages(&queue, 10, LONG_POLL))?,
        )?;
        expect_eq("messages received", received.len(), 1)?;
        let message = &received[0];
        expect_eq("the message ID", message.message_id(), Some(&*id))?;
        expect_eq("the body", message.body(), Some("hello sqs"))?;
        let receipt = receipt_of(message)?;
        expect_ok(
            "delete",
            within("delete", client.delete_message(&queue, receipt))?,
        )?;
        let after = expect_ok(
            "receive after delete",
            within(
                "receive",
                client.receive_messages(&queue, 10, Duration::from_secs(2)),
            )?,
        )?;
        expect_eq("messages after delete", after.len(), 0)?;
        expect_ok("close", within("close", client.close())?)
    }))
}

/// A message received and not deleted is delivered again once its
/// visibility timeout passes; the newer receipt deletes it.
fn visibility_redelivers(endpoint: &str, queue: &str) -> Row {
    let builder = local(endpoint);
    let queue = queue.to_owned();
    clean_run(runtime::builder().run(move || -> Row {
        let client = settled_within("connect", ROW_BOUND, builder.connect())?;
        let id = expect_ok(
            "send",
            within("send", client.send_message(&queue, "redelivered"))?,
        )?;
        let first = expect_ok(
            "first receive",
            within("receive", client.receive_messages(&queue, 1, LONG_POLL))?,
        )?;
        expect_eq(
            "first delivery",
            message_ids(&first),
            Box::from([Some(&*id)]),
        )?;
        let second = expect_ok(
            "second receive",
            within("receive", client.receive_messages(&queue, 1, LONG_POLL))?,
        )?;
        expect_eq("redelivery", message_ids(&second), Box::from([Some(&*id)]))?;
        expect(
            "the redelivery carries a new receipt",
            first[0].receipt_handle() != second[0].receipt_handle(),
        )?;
        let receipt = receipt_of(&second[0])?;
        expect_ok(
            "delete",
            within("delete", client.delete_message(&queue, receipt))?,
        )?;
        expect_ok("close", within("close", client.close())?)
    }))
}

/// The service refuses a missing queue and an invalid receipt; Camber refuses
/// invalid receive parameters before the service sees them.
fn explicit_refusals(endpoint: &str, queue: &str) -> Row {
    let builder = local(endpoint);
    let queue = queue.to_owned();
    let missing = format!("{endpoint}/000000000000/camber-missing-queue");
    clean_run(runtime::builder().run(move || -> Row {
        let client = settled_within("connect", ROW_BOUND, builder.connect())?;
        expect_refused(
            "ready on a missing queue",
            within("ready", client.ready(&missing))?,
            rejected(IntegrationOperation::Ready),
        )?;
        expect_refused(
            "delete with an invalid receipt",
            within("delete", client.delete_message(&queue, "not-a-receipt"))?,
            rejected(IntegrationOperation::Delete),
        )?;
        expect_refused(
            "receive of eleven messages",
            within(
                "receive",
                client.receive_messages(&queue, 11, Duration::ZERO),
            )?,
            rejected(IntegrationOperation::Receive),
        )?;
        expect_ok("close", within("close", client.close())?)
    }))
}

/// A runtime stop cuts a submitted long poll: once the relay has recorded the
/// receive on the wire, the stop resolves it as `Cancelled` with an unknown
/// outcome. An answer, empty or not, means the stop never cut the poll. The
/// runtime returns with nothing left to report, and the recorded request is
/// signed with the explicit key, not the lane's ambient one.
fn runtime_stop(address: SocketAddr, queue: &str) -> Row {
    expect(
        "the lane exports the explicit access key as its ambient one",
        std::env::var("AWS_ACCESS_KEY_ID").as_deref() != Ok(ACCESS_KEY),
    )?;
    let relay = Relay::start(address, ROW_BOUND)?;
    let builder = local(&endpoint_of(relay.address())).shutdown_timeout(Duration::from_millis(300));
    let mut sent = relay.control().sent();
    let queue = queue.to_owned();
    let verdict = clean_run(runtime::builder().run(move || -> Row {
        let client = settled_within("connect", ROW_BOUND, builder.connect())?;
        runtime::block_on(async {
            let mut polling = pin!(client.receive_messages(&queue, 1, LONG_POLL));
            let submission = async {
                tokio::select! {
                    biased;
                    seen = sent.wait_for(|bytes| occurrences(bytes, b"ReceiveMessage") > 0) => {
                        seen.map(drop).map_err(|_| "the relay stopped recording".to_owned())
                    }
                    outcome = polling.as_mut() => Err(format!(
                        "the long poll ended before its request reached the relay: {:?}",
                        outcome.map(|received| received.len()),
                    )),
                }
            };
            tokio::time::timeout(ROW_BOUND, submission)
                .await
                .map_err(|_| "the receive never reached the relay".to_owned())??;
            runtime::request_shutdown();
            let outcome = tokio::time::timeout(ROW_BOUND, polling)
                .await
                .map_err(|_| "the stop did not cut the long poll".to_owned())?;
            match outcome {
                Ok(received) => Err(format!(
                    "the long poll answered {} messages; the stop never cut it",
                    received.len()
                )),
                Err(error) => expect_eq(
                    "the cut long poll",
                    refusal(&error),
                    Some(cancelled(IntegrationOperation::Receive)),
                ),
            }
        })?;
        let recorded = sent.borrow();
        let signatures = occurrences(&recorded, b"Credential=");
        expect("the relay recorded a signed request", signatures > 0)?;
        expect_eq(
            "signatures by the explicit key, of all signatures",
            occurrences(&recorded, format!("Credential={ACCESS_KEY}/").as_bytes()),
            signatures,
        )
    }));
    verdict.and(relay.finish())
}

/// On one worker, a pending long poll does not stop a send, and the send's
/// message ends that same poll.
fn single_worker_progress(endpoint: &str, queue: &str) -> Row {
    let builder = local(endpoint);
    let queue = queue.to_owned();
    clean_run(runtime::builder().worker_threads(1).run(move || -> Row {
        let client = settled_within("connect", ROW_BOUND, builder.connect())?;
        let received = runtime::block_on(async {
            let mut polling = pin!(client.receive_messages(&queue, 1, LONG_POLL));
            expect_polled_pending("the long poll", &futures_util::poll!(polling.as_mut()))?;
            let id = tokio::time::timeout(ROW_BOUND, client.send_message(&queue, "progress"))
                .await
                .map_err(|_| "the send made no progress".to_owned())?
                .map_err(|error| format!("send: {error:?}"))?;
            let received = tokio::time::timeout(ROW_BOUND, polling)
                .await
                .map_err(|_| "the long poll never ended".to_owned())?
                .map_err(|error| format!("receive: {error:?}"))?;
            expect_eq(
                "the polled message",
                message_ids(&received),
                Box::from([Some(&*id)]),
            )?;
            Ok::<_, String>(received)
        })?;
        for message in &*received {
            let receipt = receipt_of(message)?;
            expect_ok(
                "delete",
                within("delete", client.delete_message(&queue, receipt))?,
            )?;
        }
        expect_ok("close", within("close", client.close())?)
    }))
}

/// The run's queue, created and deleted through the SDK directly.
///
/// The one owner of the queue. [`Self::finish`] deletes it and proves it
/// gone; `Drop` is the fallback for an unwinding row.
struct QueueFixture {
    admin: aws_sdk_sqs::Client,
    tokio: tokio::runtime::Runtime,
    name: Box<str>,
    url: Box<str>,
    deleted: bool,
}

impl QueueFixture {
    fn create(endpoint: &str, name: &str) -> Result<Self, String> {
        let tokio = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("build the fixture runtime: {error}"))?;
        let config = aws_sdk_sqs::Config::builder()
            .behavior_version(aws_sdk_sqs::config::BehaviorVersion::latest())
            .region(aws_sdk_sqs::config::Region::new(REGION))
            .endpoint_url(endpoint)
            .credentials_provider(aws_sdk_sqs::config::Credentials::new(
                ACCESS_KEY,
                SECRET_KEY,
                None,
                None,
                "camber-fixture",
            ))
            .build();
        let admin = aws_sdk_sqs::Client::from_conf(config);
        let created = tokio
            .block_on(async {
                tokio::time::timeout(
                    ROW_BOUND,
                    admin
                        .create_queue()
                        .queue_name(name)
                        .attributes(
                            aws_sdk_sqs::types::QueueAttributeName::VisibilityTimeout,
                            VISIBILITY_SECONDS,
                        )
                        .send(),
                )
                .await
            })
            .map_err(|_| "create the queue: no answer".to_owned())?
            .map_err(|error| format!("create the queue: {error:?}"))?;
        let url = created
            .queue_url()
            .ok_or_else(|| "the created queue has no URL".to_owned())?
            .into();
        Ok(Self {
            admin,
            tokio,
            name: name.into(),
            url,
            deleted: false,
        })
    }

    /// Delete the queue, then prove the service no longer knows its name.
    fn finish(mut self) -> Result<(), String> {
        self.delete()
    }

    fn delete(&mut self) -> Result<(), String> {
        self.deleted = true;
        let admin = &self.admin;
        let (url, name) = (&*self.url, &*self.name);
        self.tokio.block_on(async {
            tokio::time::timeout(ROW_BOUND, admin.delete_queue().queue_url(url).send())
                .await
                .map_err(|_| "delete the queue: no answer".to_owned())?
                .map_err(|error| format!("delete the queue: {error:?}"))?;
            let lookup =
                tokio::time::timeout(ROW_BOUND, admin.get_queue_url().queue_name(name).send())
                    .await
                    .map_err(|_| "look the deleted queue up: no answer".to_owned())?;
            match lookup {
                // ElasticMQ's error document parses as unmodeled, so the
                // fixture reads the protocol error code rather than the
                // typed variant.
                Err(error)
                    if aws_sdk_sqs::error::ProvideErrorMetadata::code(&error)
                        == Some("QueueDoesNotExist") =>
                {
                    Ok(())
                }
                other => Err(format!("the deleted queue still resolves: {other:?}")),
            }
        })
    }
}

impl Drop for QueueFixture {
    fn drop(&mut self) {
        if !self.deleted
            && let Err(error) = self.delete()
        {
            eprintln!("the SQS queue fixture left its queue: {error}");
        }
    }
}
