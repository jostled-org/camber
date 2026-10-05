//! 6.T1: SQS admission, submission classification, and close.
//!
//! Every row enters through the public `camber::mq::sqs` API. The real AWS SDK
//! sends each request over loopback HTTP to a scripted peer that records the
//! request in full, so a row counts the attempts that reached the wire. Exact
//! outcomes are asserted only after a peer record or an owner-committed fact;
//! a row that waits for a state waits on that committed state under a hang
//! guard.
//!
//! Each row owns its runtime and its peer, finishes the peer on success and
//! failure alike, and returns its own verdict, so one broken claim cannot hide
//! another.
#![cfg(feature = "sqs")]

use crate::integration_rows::{
    EXHAUSTED, LIVE_LIMIT, NamedRow, ROW_BOUND, Refusal, Row, all, busy, cancelled, clean_run,
    closed, expect, expect_aggregate, expect_eq, expect_failed_run, expect_no_runtime, expect_ok,
    expect_polled_pending, expect_refused, expect_scope_closed, expired, hold_live_slots,
    integration_admitted_after, invalid_config, limit_exceeded, on_tokio, permission_denied,
    rejected, row_bounded, run_observing, run_rows, settled, unavailable, unknown,
};
use crate::process::ChildGuard;
use crate::sqs_peer::{ACCESS_KEY, PeerControl, REGION, Reply, SECRET_KEY, SqsPeer};
use crate::temp_support::TempRoot;
use camber::mq::sqs::{self, Client, SqsBuilder};
use camber::runtime_test_support::{SqsBounds, SqsBoundsProbe, SqsCredentialProbe};
use camber::{
    IntegrationFailure, IntegrationKind, IntegrationOperation, Retryability, RuntimeError, runtime,
};
use std::future::Future;
use std::net::TcpListener;
use std::pin::pin;
use std::process::{Command, Stdio};
use std::time::Duration;

/// The documented default operation limit.
const DEFAULT_IN_FLIGHT: usize = 64;

/// The documented default payload maximum.
const DEFAULT_MAX_MESSAGE_BYTES: usize = 1024 * 1024;

const QUEUE: &str = "orders";

/// The child the default-chain row re-enters this binary at.
const CHAIN_CHILD: &str = "sqs_operations::sqs_default_credential_chain_child";

/// The scripted peer endpoint the default-chain child connects to.
const CHAIN_ENDPOINT_ENV: &str = "CAMBER_SQS_CHAIN_CHILD_ENDPOINT";

/// The line the default-chain child prints once its claim held.
const CHAIN_MARKER: &str = "camber-sqs-default-chain-bounded";

// ── 6.T1 ──────────────────────────────────────────────────────────────

#[test]
fn sqs_operations_classify_submission_and_refuse_without_effects() {
    run_rows(SUBMISSION_AND_REFUSAL);
}

/// Every 6.T1 row, by the claim it proves.
const SUBMISSION_AND_REFUSAL: &[NamedRow<'static>] = &[
    (
        "the builder carries the documented defaults",
        builder_defaults,
    ),
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
        "operations after a runtime stop refuse before I/O",
        operations_after_runtime_stop_refuse_before_io,
    ),
    (
        "credential loading is bounded by the connect deadline",
        credential_loading_is_bounded_by_connect,
    ),
    (
        "the default credential chain is bounded by the connect deadline",
        default_credential_chain_is_bounded_by_connect,
    ),
    (
        "endpoint, region, and credentials belong to one instance",
        configuration_is_per_instance,
    ),
    (
        "readiness names one queue query",
        readiness_names_one_queue_query,
    ),
    (
        "readiness proves no operation permission",
        readiness_proves_no_operation_permission,
    ),
    (
        "receive bounds refuse before submission",
        receive_bounds_refuse_before_submission,
    ),
    (
        "a send without a message ID is an unknown outcome",
        missing_message_id_is_an_unknown_outcome,
    ),
    (
        "an overlarge batch fails without deletion",
        overlarge_batch_fails_without_deletion,
    ),
    (
        "the default inbound maximum fails the batch",
        default_inbound_maximum_fails_the_batch,
    ),
    (
        "payload maximum refuses before copy",
        payload_maximum_refuses_before_copy,
    ),
    (
        "default payload maximum refuses before copy",
        default_payload_maximum_refuses_before_copy,
    ),
    (
        "operation limit refuses before copy and submission",
        operation_limit_refuses_before_submission,
    ),
    (
        "default operation limit refuses before submission",
        default_operation_limit_refuses_before_submission,
    ),
    (
        "a held send reaches its deadline with an unknown outcome",
        held_send_reaches_its_deadline,
    ),
    (
        "loss before submission is safe",
        loss_before_submission_is_safe,
    ),
    (
        "an unsubmitted side effect is safe",
        unsubmitted_side_effect_is_safe,
    ),
    (
        "loss after submission is an unknown outcome",
        loss_after_submission_is_unknown,
    ),
    (
        "service answers are classified by status",
        service_answers_are_classified_by_status,
    ),
    (
        "service answers are classified by code before status",
        service_answers_are_classified_by_code,
    ),
    (
        "close is fixed and refuses every clone",
        close_is_fixed_and_refuses_every_clone,
    ),
    (
        "close cuts a running operation at its bound",
        close_cuts_a_running_operation,
    ),
    (
        "a dropped waiter releases its operation",
        dropped_waiter_releases_its_operation,
    ),
    (
        "a waiter dropped before submission leaves no account",
        unsubmitted_dropped_waiter_leaves_no_account,
    ),
    (
        "an escaped handle is inert after its runtime",
        escaped_handle_is_inert_after_its_runtime,
    ),
    (
        "a single-worker runtime makes progress",
        single_worker_runtime_makes_progress,
    ),
];

/// A builder no setter touched hands connect the documented bounds: connect
/// 10 s, operation 30 s, close 5 s, 64 operations, and 1 MiB bodies.
fn builder_defaults() -> Row {
    expect_eq(
        "the default bounds",
        SqsBoundsProbe::read(&sqs::builder()),
        SqsBounds {
            connect_timeout: Duration::from_secs(10),
            operation_timeout: Duration::from_secs(30),
            shutdown_timeout: Duration::from_secs(5),
            max_in_flight: DEFAULT_IN_FLIGHT,
            max_message_bytes: DEFAULT_MAX_MESSAGE_BYTES,
        },
    )
}

/// Each bound below, at, and above its range: out-of-range bounds and
/// malformed endpoint, region, and credentials refuse as `InvalidConfig`
/// with no request; the boundary values connect and answer readiness.
fn builder_bounds_refuse_before_io() -> Row {
    let peer = SqsPeer::start();
    let day = Duration::from_secs(24 * 60 * 60);
    let base = peer.builder();
    let invalid = out_of_range_builders(&base, day + Duration::from_nanos(1));
    let boundary = base
        .operation_timeout(day)
        .connect_timeout(day)
        .shutdown_timeout(day)
        .max_in_flight(1)
        .max_message_bytes(1);
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        for (what, builder) in invalid {
            expect_refused(
                what,
                row_bounded(what, builder.connect())?,
                invalid_config(IntegrationOperation::Connect),
            )?;
        }
        let client = settled("connect", boundary.connect())?;
        expect_ok("ready", row_bounded("ready", client.ready(&queue))?)?;
        expect_ok("boundary close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        peer.control()
            .log()
            .expect_requests(&["GetQueueAttributes"])
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Every configuration value set just outside its range from `base`, where
/// `over` is the first duration past the timeout maximum.
fn out_of_range_builders(base: &SqsBuilder, over: Duration) -> Box<[(&'static str, SqsBuilder)]> {
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
        ("empty region", base.clone().region("")),
        ("region with a space", base.clone().region("us east")),
        ("relative endpoint", base.clone().endpoint("127.0.0.1:9324")),
        (
            "non-HTTP endpoint",
            base.clone().endpoint("ftp://127.0.0.1"),
        ),
        (
            "endpoint with userinfo",
            base.clone().endpoint("http://user:pass@127.0.0.1:9324"),
        ),
        (
            "endpoint with a query",
            base.clone().endpoint("http://127.0.0.1:9324/?token=x"),
        ),
        (
            "empty access key",
            base.clone().credentials("", SECRET_KEY, None),
        ),
        (
            "empty secret key",
            base.clone().credentials(ACCESS_KEY, "", None),
        ),
        (
            "empty session token",
            base.clone().credentials(ACCESS_KEY, SECRET_KEY, Some("")),
        ),
    ])
}

/// Outside a Camber runtime there is no owner to capture, even inside a
/// multi-thread Tokio runtime: `NoRuntime`, and the peer sees nothing. The
/// current-thread executor is `message_queue_validation`'s row.
fn no_runtime_refuses_before_io() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let verdict = (|| -> Row {
        let tokio = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("build a bare Tokio runtime: {error}"))?;
        expect_no_runtime("connect outside Camber", tokio.block_on(builder.connect()))?;
        peer.control().expect_no_connection()
    })();
    peer.finished(ROW_BOUND, verdict)
}

/// A connect after root admission closed is refused as `ScopeClosed`.
fn closed_admission_refuses_before_io() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let outcome = runtime::builder().run(move || -> Row {
        runtime::request_shutdown();
        expect_scope_closed(
            "connect after closure",
            row_bounded("connect", builder.connect())?,
        )
    });
    let verdict = clean_run(outcome).and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// With every live integration slot taken, a connect is `Busy` and the peer
/// sees nothing.
fn live_limit_refuses_before_io() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let outcome = runtime::builder().run(move || -> Row {
        let held = hold_live_slots(LIVE_LIMIT, IntegrationKind::Nats)?;
        let refused = row_bounded("connect", builder.connect())?;
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

/// A runtime stop commits every live client's close: each operation on a
/// client connected before it is `Closed` with nothing sent, the next connect
/// is `ScopeClosed`, and the client's close settles cleanly.
fn operations_after_runtime_stop_refuse_before_io() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.clone().connect())?;
        runtime::request_shutdown();
        let refusals = [
            expect_refused(
                "ready after the stop",
                row_bounded("ready", client.ready(&queue))?,
                closed(IntegrationOperation::Ready),
            ),
            expect_refused(
                "send after the stop",
                row_bounded("send", client.send_message(&queue, "late"))?,
                closed(IntegrationOperation::Publish),
            ),
            expect_refused(
                "receive after the stop",
                row_bounded(
                    "receive",
                    client.receive_messages(&queue, 1, Duration::ZERO),
                )?,
                closed(IntegrationOperation::Receive),
            ),
            expect_refused(
                "delete after the stop",
                row_bounded("delete", client.delete_message(&queue, "receipt"))?,
                closed(IntegrationOperation::Delete),
            ),
            expect_scope_closed(
                "connect after the stop",
                row_bounded("connect", builder.connect())?,
            ),
            expect_ok(
                "close after the stop",
                row_bounded("close", client.close())?,
            ),
        ];
        all(refusals)
    });
    let verdict = clean_run(outcome).and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// Credential loading is part of connect: a source that never answers is cut
/// by the connect deadline as `Timeout`, and a source with no credentials is
/// `InvalidConfig`. Neither sends a request.
fn credential_loading_is_bounded_by_connect() -> Row {
    let peer = SqsPeer::start();
    let endpoint = peer.endpoint();
    let outcome = runtime::builder().run(move || -> Row {
        let base = || sqs::builder().endpoint(&endpoint).region(REGION);
        expect_refused(
            "credentials that never load",
            row_bounded(
                "connect",
                SqsCredentialProbe::unresolved(base().connect_timeout(EXHAUSTED)).connect(),
            )?,
            expired(IntegrationOperation::Connect),
        )?;
        expect_refused(
            "no credentials to load",
            row_bounded("connect", SqsCredentialProbe::absent(base()).connect())?,
            invalid_config(IntegrationOperation::Connect),
        )?;
        let client = settled(
            "connect",
            base()
                .credentials(ACCESS_KEY, SECRET_KEY, Some("session"))
                .connect(),
        )?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// With no explicit credentials, connect loads the SDK's default chain under
/// the connect deadline.
///
/// The chain reads the process environment, so the claim runs in a child of
/// this binary whose environment the row owns: no keys, no profile files,
/// and an instance-metadata endpoint that is a loopback listener which never
/// answers. The chain then stalls in its metadata request, whose own read
/// timeout is a second, and the child requires the 300 ms connect deadline
/// to cut it as `Timeout` with nothing sent to the queue peer.
fn default_credential_chain_is_bounded_by_connect() -> Row {
    let peer = SqsPeer::start();
    let verdict = (|| -> Row {
        let metadata = TcpListener::bind("127.0.0.1:0")
            .map_err(|error| format!("bind the silent metadata listener: {error}"))?;
        let metadata_address = metadata
            .local_addr()
            .map_err(|error| format!("read the metadata listener address: {error}"))?;
        let home = TempRoot::new().map_err(|error| format!("create the child home: {error}"))?;
        let mut command = Command::new(
            std::env::current_exe().map_err(|error| format!("find this test binary: {error}"))?,
        );
        command
            .args(["--exact", CHAIN_CHILD, "--ignored", "--nocapture"])
            .env_clear()
            .env("HOME", home.path())
            .env("AWS_CONFIG_FILE", home.path().join("config"))
            .env(
                "AWS_SHARED_CREDENTIALS_FILE",
                home.path().join("credentials"),
            )
            .env(
                "AWS_EC2_METADATA_SERVICE_ENDPOINT",
                format!("http://{metadata_address}"),
            )
            .env(CHAIN_ENDPOINT_ENV, peer.endpoint())
            .stdin(Stdio::null());
        let mut child = ChildGuard::spawn(command, ROW_BOUND)
            .map_err(|error| format!("start the default-chain child: {error}"))?;
        let exited = child
            .wait_for_readiness(CHAIN_MARKER, ROW_BOUND)
            .and_then(|()| child.wait_bounded(ROW_BOUND));
        let checked = match exited {
            Ok(status) => expect(
                &format!(
                    "the default-chain child exited with {status}: {}",
                    String::from_utf8_lossy(child.stderr())
                ),
                status.success(),
            ),
            Err(error) => {
                drop(child.shutdown());
                Err(format!(
                    "the default-chain child: {error}\n{}",
                    String::from_utf8_lossy(child.stderr())
                ))
            }
        };
        drop(metadata);
        let removed = home
            .close()
            .map_err(|error| format!("remove the child home: {error}"));
        all([checked, removed])
    })();
    let verdict = verdict.and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// The default-chain row's child: a connect with no explicit credentials
/// must end as the connect deadline's `Timeout`.
///
/// # Panics
///
/// When it is run outside the row that owns its environment, or the connect
/// answered anything else.
#[test]
#[ignore = "child of default_credential_chain_is_bounded_by_connect; it needs the environment that row sets"]
fn sqs_default_credential_chain_child() {
    let endpoint = std::env::var(CHAIN_ENDPOINT_ENV)
        .expect("the default-chain child runs only under its parent row");
    let outcome = runtime::builder().run(move || {
        row_bounded(
            "connect",
            sqs::builder()
                .endpoint(&endpoint)
                .region(REGION)
                .connect_timeout(EXHAUSTED)
                .connect(),
        )
        .and_then(|connected| {
            expect_refused(
                "connect through the default chain",
                connected,
                expired(IntegrationOperation::Connect),
            )
        })
    });
    if let Err(reason) = clean_run(outcome) {
        panic!("{reason}");
    }
    println!("{CHAIN_MARKER}");
}

/// Two instances in one runtime each sign with their own credentials and
/// region and reach only their own endpoint.
fn configuration_is_per_instance() -> Row {
    let first = SqsPeer::start();
    let second = SqsPeer::start();
    let first_builder = first.builder();
    let second_builder = sqs::builder()
        .endpoint(&second.endpoint())
        .region("eu-west-1")
        .credentials("camber-second", "camber-second-secret", None);
    let first_queue = first.queue_url(QUEUE);
    let second_queue = second.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let one = settled("connect", first_builder.connect())?;
        let two = settled("connect", second_builder.connect())?;
        expect_ok(
            "first ready",
            row_bounded("ready", one.ready(&first_queue))?,
        )?;
        expect_ok(
            "second ready",
            row_bounded("ready", two.ready(&second_queue))?,
        )?;
        expect_ok("first close", row_bounded("close", one.close())?)?;
        expect_ok("second close", row_bounded("close", two.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let signed = |peer: &SqsPeer| {
            peer.control()
                .log()
                .requests
                .iter()
                .map(|request| (request.access_key.clone(), request.region.clone()))
                .collect::<Vec<_>>()
        };
        expect_eq(
            "the first peer's signatures",
            signed(&first),
            vec![(Box::from(ACCESS_KEY), Box::from(REGION))],
        )?;
        expect_eq(
            "the second peer's signatures",
            signed(&second),
            vec![(Box::from("camber-second"), Box::from("eu-west-1"))],
        )
    });
    first
        .finished(ROW_BOUND, verdict)
        .and(second.finish(ROW_BOUND))
}

/// Readiness succeeds on a queue query the peer answered; a refusal is typed
/// by its status, and a lost or unanswered query is safe because the query is
/// read-only. Each is one attempt.
///
/// The unanswered query's deadline is the claim, so it keeps a real time
/// bound. An answered query first opens the pooled connection, so the held
/// query is one write on that open socket; the held count and the single
/// accepted connection prove the peer read it there.
fn readiness_names_one_queue_query() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let bounded_client = settled(
            "connect",
            builder.clone().operation_timeout(EXHAUSTED).connect(),
        )?;
        expect_ok(
            "the warming query",
            row_bounded("ready", bounded_client.ready(&queue))?,
        )?;
        control.script([Reply::Hold]);
        expect_refused(
            "an unanswered query",
            row_bounded("ready", bounded_client.ready(&queue))?,
            expired(IntegrationOperation::Ready),
        )?;
        let log = control.wait_held(1, ROW_BOUND)?;
        expect_eq("connections the unanswered query used", log.accepted, 1)?;
        expect_ok("close", row_bounded("close", bounded_client.close())?)?;
        let client = settled("connect", builder.connect())?;
        expect_ok("ready", row_bounded("ready", client.ready(&queue))?)?;
        let rows = [
            (
                "a missing queue",
                Reply::error(400, "QueueDoesNotExist"),
                rejected(IntegrationOperation::Ready),
            ),
            (
                "a denied query",
                Reply::error(403, "AccessDenied"),
                (
                    IntegrationOperation::Ready,
                    IntegrationFailure::PermissionDenied,
                    Retryability::Never,
                ),
            ),
            (
                "a lost answer",
                Reply::Drop,
                unavailable(IntegrationOperation::Ready),
            ),
        ];
        for (what, reply, expected) in rows {
            control.script([reply]);
            expect_refused(what, row_bounded("ready", client.ready(&queue))?, expected)?;
        }
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        expect_eq(
            "queue queries",
            peer.control().log().count("GetQueueAttributes"),
            6,
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A queue that answered readiness can still deny every operation: each
/// denial reaches the caller as `PermissionDenied`, so readiness stands in for
/// no permission to send, receive, or delete.
fn readiness_proves_no_operation_permission() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        expect_ok("ready", row_bounded("ready", client.ready(&queue))?)?;
        control.script([
            Reply::error(403, "AccessDenied"),
            Reply::error(403, "AccessDenied"),
            Reply::error(403, "AccessDenied"),
        ]);
        let denials = [
            expect_refused(
                "send on a ready queue",
                row_bounded("send", client.send_message(&queue, "body"))?,
                permission_denied(IntegrationOperation::Publish),
            ),
            expect_refused(
                "receive on a ready queue",
                row_bounded(
                    "receive",
                    client.receive_messages(&queue, 1, Duration::ZERO),
                )?,
                permission_denied(IntegrationOperation::Receive),
            ),
            expect_refused(
                "delete on a ready queue",
                row_bounded("delete", client.delete_message(&queue, "receipt"))?,
                permission_denied(IntegrationOperation::Delete),
            ),
        ];
        all(denials)?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        peer.control().log().expect_requests(&[
            "GetQueueAttributes",
            "SendMessage",
            "ReceiveMessage",
            "DeleteMessage",
        ])
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Batch sizes outside 1–10.
pub(crate) const INVALID_BATCH_SIZES: &[(&str, i32, Duration)] = &[
    ("zero messages", 0, Duration::from_secs(1)),
    ("negative messages", -1, Duration::from_secs(1)),
    ("eleven messages", 11, Duration::from_secs(1)),
];

/// Waits over 20 seconds.
pub(crate) const INVALID_WAITS: &[(&str, i32, Duration)] = &[
    ("a wait over twenty seconds", 1, Duration::from_secs(21)),
    ("an unbounded wait", 1, Duration::MAX),
];

/// Batch sizes outside 1–10 and waits over 20 seconds are refused before
/// submission; the boundaries reach the peer exactly as given.
fn receive_bounds_refuse_before_submission() -> Row {
    all([
        receive_refused_before_submission(INVALID_BATCH_SIZES),
        receive_refused_before_submission(INVALID_WAITS),
    ])
}

/// Each `invalid` receive, named with its batch size and wait, is `Rejected`
/// before submission, while the boundaries 1 with no wait and 10 with a
/// 20-second wait reach the peer exactly as given.
pub(crate) fn receive_refused_before_submission(
    invalid: &'static [(&'static str, i32, Duration)],
) -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        for &(what, max_messages, wait) in invalid {
            expect_refused(
                what,
                row_bounded(
                    "receive",
                    client.receive_messages(&queue, max_messages, wait),
                )?,
                rejected(IntegrationOperation::Receive),
            )?;
        }
        for (max_messages, wait) in [(1, Duration::ZERO), (10, Duration::from_secs(20))] {
            let received = expect_ok(
                "receive at a boundary",
                row_bounded(
                    "receive",
                    client.receive_messages(&queue, max_messages, wait),
                )?,
            )?;
            expect_eq("messages from an empty queue", received.len(), 0)?;
        }
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let log = peer.control().log();
        expect_eq(
            "receive requests, by size and wait",
            log.requests
                .iter()
                .map(|request| {
                    (
                        request.number("MaxNumberOfMessages"),
                        request.number("WaitTimeSeconds"),
                    )
                })
                .collect::<Vec<_>>(),
            vec![(Some(1), Some(0)), (Some(10), Some(20))],
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A send answered without a message ID names no acknowledgement: the queue
/// may hold the message, so the outcome is unknown. One attempt each.
pub(crate) fn missing_message_id_is_an_unknown_outcome() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let replies = [
            ("absent", Reply::Json(200, "{}".into())),
            ("empty", Reply::Json(200, r#"{"MessageId":""}"#.into())),
        ];
        for (what, reply) in replies {
            control.script([reply]);
            expect_refused(
                &format!("a send with an {what} message ID"),
                row_bounded("send", client.send_message(&queue, "body"))?,
                unknown(IntegrationOperation::Publish),
            )?;
        }
        let id = expect_ok(
            "send",
            row_bounded("send", client.send_message(&queue, "body"))?,
        )?;
        expect_eq("the acknowledged message ID", &*id, "peer-sent-1")?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        expect_eq(
            "send requests",
            peer.control().log().count("SendMessage"),
            3,
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A batch larger than requested, or holding a body over the maximum, fails
/// whole: no message reaches the caller and nothing is deleted. A body at the
/// maximum is delivered with its fields.
fn overlarge_batch_fails_without_deletion() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder().max_message_bytes(4);
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let over_limit = limit_exceeded(IntegrationOperation::Receive);
        control.script([Reply::messages(&["a", "b", "c"])]);
        expect_refused(
            "more messages than requested",
            row_bounded(
                "receive",
                client.receive_messages(&queue, 2, Duration::ZERO),
            )?,
            over_limit,
        )?;
        control.script([Reply::messages(&["1234", "12345"])]);
        expect_refused(
            "a body over the maximum",
            row_bounded(
                "receive",
                client.receive_messages(&queue, 2, Duration::ZERO),
            )?,
            over_limit,
        )?;
        control.script([Reply::messages(&["1234"])]);
        let received = expect_ok(
            "a body at the maximum",
            row_bounded(
                "receive",
                client.receive_messages(&queue, 2, Duration::ZERO),
            )?,
        )?;
        let fields: Vec<_> = received
            .iter()
            .map(|message| {
                (
                    message.body(),
                    message.receipt_handle(),
                    message.message_id(),
                )
            })
            .collect();
        expect_eq(
            "the delivered message",
            fields,
            vec![(Some("1234"), Some("peer-receipt-0"), Some("peer-message-0"))],
        )?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let log = peer.control().log();
        expect_eq("receive requests", log.count("ReceiveMessage"), 3)?;
        expect_eq("delete requests", log.count("DeleteMessage"), 0)
    });
    peer.finished(ROW_BOUND, verdict)
}

/// On the default builder a delivered body of 1 MiB reaches the caller, and a
/// batch holding one byte more fails whole with nothing deleted.
fn default_inbound_maximum_fails_the_batch() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let at = "x".repeat(DEFAULT_MAX_MESSAGE_BYTES);
        let over = "x".repeat(DEFAULT_MAX_MESSAGE_BYTES + 1);
        control.script([Reply::messages(&[&at]), Reply::messages(&["small", &over])]);
        let delivered = expect_ok(
            "a body at the default maximum",
            row_bounded(
                "receive",
                client.receive_messages(&queue, 2, Duration::ZERO),
            )?,
        )?;
        let checks = [
            expect_eq(
                "delivered body sizes",
                delivered
                    .iter()
                    .map(|message| message.body().map(str::len))
                    .collect::<Vec<_>>(),
                vec![Some(DEFAULT_MAX_MESSAGE_BYTES)],
            ),
            expect_refused(
                "a body over the default maximum",
                row_bounded(
                    "receive",
                    client.receive_messages(&queue, 2, Duration::ZERO),
                )?,
                limit_exceeded(IntegrationOperation::Receive),
            ),
        ];
        all(checks)?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        peer.control()
            .log()
            .expect_requests(&["ReceiveMessage", "ReceiveMessage"])
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A body at the maximum sends; one byte over is refused before it is copied
/// or sent.
fn payload_maximum_refuses_before_copy() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder().max_message_bytes(4);
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        expect_ok(
            "send at the maximum",
            row_bounded("send", client.send_message(&queue, "1234"))?,
        )?;
        expect_refused(
            "send over the maximum",
            row_bounded("send", client.send_message(&queue, "12345"))?,
            limit_exceeded(IntegrationOperation::Publish),
        )?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let log = peer.control().log();
        expect_eq(
            "bodies the peer read",
            log.requests
                .iter()
                .map(|request| request.field("MessageBody").map(str::len))
                .collect::<Vec<_>>(),
            vec![Some(4)],
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// The default maximum is 1 MiB: a body of that size sends, one byte more is
/// refused before copy.
fn default_payload_maximum_refuses_before_copy() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let at = "x".repeat(DEFAULT_MAX_MESSAGE_BYTES);
        let over = "x".repeat(DEFAULT_MAX_MESSAGE_BYTES + 1);
        expect_ok(
            "send at the default maximum",
            row_bounded("send", client.send_message(&queue, &at))?,
        )?;
        expect_refused(
            "send over the default maximum",
            row_bounded("send", client.send_message(&queue, &over))?,
            limit_exceeded(IntegrationOperation::Publish),
        )?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        expect_eq(
            "send requests",
            peer.control().log().count("SendMessage"),
            1,
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// With the one operation slot held by a send the peer never answers, the
/// next send and receive are `Busy` before any effect. The hold ends only by
/// the row's close, which cuts the held send with an unknown outcome: it was
/// submitted.
fn operation_limit_refuses_before_submission() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.held_until_close().max_in_flight(1);
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([Reply::Hold]);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        runtime::block_on(async {
            let mut held = pin!(client.send_message(&queue, "held"));
            expect_polled_pending("the held send", &futures_util::poll!(held.as_mut()))?;
            control.wait_held(1, ROW_BOUND)?;
            let refusals = [
                expect_refused(
                    "send past the operation limit",
                    client.send_message(&queue, "refused").await,
                    busy(IntegrationOperation::Publish),
                ),
                expect_refused(
                    "receive past the operation limit",
                    client.receive_messages(&queue, 1, Duration::ZERO).await,
                    busy(IntegrationOperation::Receive),
                ),
            ];
            let released = released_by_close(&client, [held]).await;
            all(refusals.into_iter().chain([released]))
        })
    });
    let verdict =
        clean_run(outcome).and_then(|()| peer.control().log().expect_requests(&["SendMessage"]));
    peer.finished(ROW_BOUND, verdict)
}

/// The default limit is 64 operations: with 64 sends held, the next is
/// `Busy` and never reaches the peer. The row's close releases the holds.
fn default_operation_limit_refuses_before_submission() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.held_until_close();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script(std::iter::repeat_n(Reply::Hold, DEFAULT_IN_FLIGHT));
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        runtime::block_on(async {
            let mut held = (0..DEFAULT_IN_FLIGHT)
                .map(|_| Box::pin(client.send_message(&queue, "held")))
                .collect::<Box<[_]>>();
            for send in &mut held {
                expect_polled_pending("a held send", &futures_util::poll!(send.as_mut()))?;
            }
            control.wait_held(DEFAULT_IN_FLIGHT, ROW_BOUND)?;
            let refused = expect_refused(
                "send past the default limit",
                client.send_message(&queue, "refused").await,
                busy(IntegrationOperation::Publish),
            );
            let released = released_by_close(&client, held).await;
            all([refused, released])
        })
    });
    let verdict = clean_run(outcome).and_then(|()| {
        expect_eq(
            "send requests",
            peer.control().log().count("SendMessage"),
            DEFAULT_IN_FLIGHT,
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Close `client` beside its `held` sends: the close succeeds, and each send
/// the peer held reads the cut with an unknown outcome.
async fn released_by_close<F>(client: &Client, held: impl IntoIterator<Item = F>) -> Row
where
    F: Future<Output = Result<Box<str>, RuntimeError>>,
{
    let (sent, closed) = tokio::time::timeout(ROW_BOUND, async {
        tokio::join!(futures_util::future::join_all(held), client.close())
    })
    .await
    .map_err(|_| "the close did not release the held sends".to_owned())?;
    all(sent
        .into_iter()
        .map(|sent| {
            expect_refused(
                "a held send",
                sent,
                cancelled(IntegrationOperation::Publish),
            )
        })
        .chain([expect_ok("close", closed)]))
}

/// A send the peer holds past its operation deadline ends as `Timeout` with
/// an unknown outcome: it was submitted.
///
/// The deadline is the claim, so the row keeps a real time bound. An answered
/// query first opens the one pooled connection and caches the credentials,
/// which leaves the send only its signing and one write on that open socket
/// before the deadline. The peer's single accepted connection proves the send
/// used it.
fn held_send_reaches_its_deadline() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder().operation_timeout(EXHAUSTED);
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        expect_ok(
            "the warming query",
            row_bounded("ready", client.ready(&queue))?,
        )?;
        control.script([Reply::Hold]);
        expect_refused(
            "the held send",
            row_bounded("send", client.send_message(&queue, "held"))?,
            (
                IntegrationOperation::Publish,
                IntegrationFailure::Timeout,
                Retryability::OutcomeUnknown,
            ),
        )?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let log = peer.control().log();
        log.expect_requests(&["GetQueueAttributes", "SendMessage"])?;
        expect_eq("requests the peer held", log.held, 1)?;
        expect_eq("connections the peer accepted", log.accepted, 1)
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A transport that refuses the connection proves nothing was written:
/// every operation is `Unavailable` and safe to repeat.
fn loss_before_submission_is_safe() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let queue = peer.queue_url(QUEUE);
    let stopped = peer.finish(ROW_BOUND);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        expect_refused(
            "ready",
            row_bounded("ready", client.ready(&queue))?,
            unavailable(IntegrationOperation::Ready),
        )?;
        expect_refused(
            "send",
            row_bounded("send", client.send_message(&queue, "body"))?,
            unavailable(IntegrationOperation::Publish),
        )?;
        expect_refused(
            "receive",
            row_bounded(
                "receive",
                client.receive_messages(&queue, 1, Duration::ZERO),
            )?,
            unavailable(IntegrationOperation::Receive),
        )?;
        expect_refused(
            "delete",
            row_bounded("delete", client.delete_message(&queue, "receipt"))?,
            unavailable(IntegrationOperation::Delete),
        )?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    stopped.and(clean_run(outcome))
}

/// A side effect that never reached the transport is safe to repeat. Each
/// operation here stalls loading credentials before it can sign, so nothing is
/// submitted: its deadline is `Timeout` and a close cut is `Cancelled`, both
/// `Safe`, and the peer accepts no connection.
fn unsubmitted_side_effect_is_safe() -> Row {
    let peer = SqsPeer::start();
    let (deadline, _) =
        SqsCredentialProbe::answers_once(peer.builder().operation_timeout(EXHAUSTED));
    let (cut, mut loads) = SqsCredentialProbe::answers_once(peer.held_until_close());
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", deadline.connect())?;
        let deadlines = [
            expect_refused(
                "a send past its deadline",
                row_bounded("send", client.send_message(&queue, "body"))?,
                expired(IntegrationOperation::Publish),
            ),
            expect_refused(
                "a receive past its deadline",
                row_bounded(
                    "receive",
                    client.receive_messages(&queue, 1, Duration::ZERO),
                )?,
                expired(IntegrationOperation::Receive),
            ),
            expect_refused(
                "a delete past its deadline",
                row_bounded("delete", client.delete_message(&queue, "receipt"))?,
                expired(IntegrationOperation::Delete),
            ),
            expect_ok("close", row_bounded("close", client.close())?),
        ];
        let client = settled("connect", cut.connect())?;
        let cut = runtime::block_on(async {
            let mut stalled = pin!(client.send_message(&queue, "stalled"));
            expect_polled_pending("the stalled send", &futures_util::poll!(stalled.as_mut()))?;
            tokio::time::timeout(ROW_BOUND, loads.reached(2))
                .await
                .map_err(|_| "the send never asked for credentials".to_owned())?;
            let (sent, closed) =
                tokio::time::timeout(ROW_BOUND, async { tokio::join!(stalled, client.close()) })
                    .await
                    .map_err(|_| "the close did not cut the stalled send".to_owned())?;
            all([
                expect_refused(
                    "a send cut before submission",
                    sent,
                    (
                        IntegrationOperation::Publish,
                        IntegrationFailure::Cancelled,
                        Retryability::Safe,
                    ),
                ),
                expect_ok("close", closed),
            ])
        });
        all(deadlines.into_iter().chain([cut]))
    });
    let verdict = clean_run(outcome).and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// A connection dropped after the peer read a side-effecting request leaves
/// its effect unknown, and Camber sends it once: no retry.
fn loss_after_submission_is_unknown() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([Reply::Drop, Reply::Drop, Reply::Drop]);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        expect_refused(
            "send",
            row_bounded("send", client.send_message(&queue, "body"))?,
            unknown(IntegrationOperation::Publish),
        )?;
        expect_refused(
            "receive",
            row_bounded(
                "receive",
                client.receive_messages(&queue, 1, Duration::ZERO),
            )?,
            unknown(IntegrationOperation::Receive),
        )?;
        expect_refused(
            "delete",
            row_bounded("delete", client.delete_message(&queue, "receipt"))?,
            unknown(IntegrationOperation::Delete),
        )?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        peer.control()
            .log()
            .expect_requests(&["SendMessage", "ReceiveMessage", "DeleteMessage"])
    });
    peer.finished(ROW_BOUND, verdict)
}

/// A scripted service answer, and the refusal the operation it names must
/// read.
type Answer = (&'static str, Reply, Refusal);

fn answer(what: &'static str, status: u16, code: &str, expected: Refusal) -> Answer {
    (what, Reply::error(status, code), expected)
}

/// Script each answer in turn and run its operation once against it.
fn classified(client: &Client, control: &PeerControl, queue: &str, answers: Box<[Answer]>) -> Row {
    all(answers.into_iter().map(|(what, reply, expected)| {
        control.script([reply]);
        let outcome = match expected.0 {
            IntegrationOperation::Ready => row_bounded(what, client.ready(queue))?,
            IntegrationOperation::Publish => {
                row_bounded(what, client.send_message(queue, "body"))?.map(drop)
            }
            IntegrationOperation::Receive => {
                row_bounded(what, client.receive_messages(queue, 1, Duration::ZERO))?.map(drop)
            }
            IntegrationOperation::Delete => {
                row_bounded(what, client.delete_message(queue, "receipt"))?
            }
            other => return Err(format!("{what}: {other:?} is not an SQS operation")),
        };
        expect_refused(what, outcome, expected)
    }))
}

/// Every side-effecting request the answers name, in order.
fn answered_requests(answers: &[Answer]) -> Box<[&'static str]> {
    answers
        .iter()
        .map(|(_, _, (operation, _, _))| match operation {
            IntegrationOperation::Ready => "GetQueueAttributes",
            IntegrationOperation::Publish => "SendMessage",
            IntegrationOperation::Receive => "ReceiveMessage",
            _ => "DeleteMessage",
        })
        .collect()
}

/// Run `answers` on one client, then require one request per answer: no
/// answer is retried.
fn classification_row(answers: fn() -> Box<[Answer]>) -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let checked = classified(&client, &control, &queue, answers());
        all([
            checked,
            expect_ok("close", row_bounded("close", client.close())?),
        ])
    });
    let verdict = clean_run(outcome).and_then(|()| {
        peer.control()
            .log()
            .expect_requests(&answered_requests(&answers()))
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Without a code Camber recognizes, a service answer is typed by its status:
/// 4xx rejects, 401 and 403 deny, and a server failure leaves a side effect
/// unknown. One attempt each, for send, receive, and delete.
fn service_answers_are_classified_by_status() -> Row {
    all([
        classification_row(status_answers),
        acknowledged_delete_names_its_receipt(),
    ])
}

fn status_answers() -> Box<[Answer]> {
    use IntegrationOperation::{Delete, Publish, Receive};
    let failed = |operation| {
        (
            operation,
            IntegrationFailure::Unavailable,
            Retryability::OutcomeUnknown,
        )
    };
    Box::new([
        answer(
            "a rejected send",
            400,
            "InvalidMessageContents",
            rejected(Publish),
        ),
        answer(
            "a denied send",
            403,
            "AccessDenied",
            permission_denied(Publish),
        ),
        answer(
            "an unauthenticated send",
            401,
            "Unauthenticated",
            permission_denied(Publish),
        ),
        answer("a failed send", 500, "InternalError", failed(Publish)),
        answer(
            "a rejected receive",
            400,
            "QueueDoesNotExist",
            rejected(Receive),
        ),
        answer(
            "a denied receive",
            403,
            "AccessDenied",
            permission_denied(Receive),
        ),
        answer("a failed receive", 503, "Unscripted", failed(Receive)),
        answer(
            "a rejected delete",
            400,
            "ReceiptHandleIsInvalid",
            rejected(Delete),
        ),
        answer(
            "a denied delete",
            403,
            "AccessDenied",
            permission_denied(Delete),
        ),
        answer("a failed delete", 500, "InternalError", failed(Delete)),
    ])
}

/// An acknowledged delete names the receipt it was given.
fn acknowledged_delete_names_its_receipt() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        expect_ok(
            "delete",
            row_bounded("delete", client.delete_message(&queue, "receipt-7"))?,
        )?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| {
        expect_eq(
            "receipts the deletes named",
            peer.control()
                .log()
                .requests
                .iter()
                .filter_map(|request| request.field("ReceiptHandle"))
                .collect::<Vec<_>>(),
            vec!["receipt-7"],
        )
    });
    peer.finished(ROW_BOUND, verdict)
}

/// The service's error code names the failure before its status does. A
/// throttle refused the request before it took effect, so it is `Unavailable`
/// and safe to repeat whatever its status; an access denial is
/// `PermissionDenied` whatever its status; and a server failure the code names
/// leaves a side effect unknown even under a 4xx status.
fn service_answers_are_classified_by_code() -> Row {
    classification_row(code_answers)
}

fn code_answers() -> Box<[Answer]> {
    use IntegrationOperation::{Delete, Publish, Ready, Receive};
    let failed = |operation, retry| (operation, IntegrationFailure::Unavailable, retry);
    Box::new([
        answer(
            "a throttled send",
            400,
            "RequestThrottled",
            unavailable(Publish),
        ),
        answer(
            "a send over the throttling limit",
            400,
            "ThrottlingException",
            unavailable(Publish),
        ),
        answer(
            "a send over a service limit",
            403,
            "OverLimit",
            unavailable(Publish),
        ),
        answer(
            "a receive throttled by KMS",
            400,
            "KmsThrottled",
            unavailable(Receive),
        ),
        answer(
            "a throttled delete",
            400,
            "RequestThrottled",
            unavailable(Delete),
        ),
        answer(
            "a throttled queue query",
            400,
            "RequestThrottled",
            unavailable(Ready),
        ),
        answer(
            "a send denied under 400",
            400,
            "AccessDeniedException",
            permission_denied(Publish),
        ),
        answer(
            "a receive denied by KMS",
            400,
            "KmsAccessDenied",
            permission_denied(Receive),
        ),
        answer(
            "a delete denied under 500",
            500,
            "AccessDenied",
            permission_denied(Delete),
        ),
        answer(
            "a send failed under 400",
            400,
            "InternalFailure",
            failed(Publish, Retryability::OutcomeUnknown),
        ),
        answer(
            "a queue query failed under 400",
            400,
            "ServiceUnavailable",
            unavailable(Ready),
        ),
    ])
}

/// Concurrent and repeated closes read one result, and every clone refuses
/// new work once close is committed, with nothing sent.
fn close_is_fixed_and_refuses_every_clone() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        let escaped = client.clone();
        let (first, second) = row_bounded("concurrent close", async {
            tokio::join!(client.close(), escaped.close())
        })?;
        expect_ok("first close", first)?;
        expect_ok("concurrent close", second)?;
        expect_ok("repeated close", row_bounded("close", client.close())?)?;
        expect_refused(
            "ready after close",
            row_bounded("ready", escaped.ready(&queue))?,
            closed(IntegrationOperation::Ready),
        )?;
        expect_refused(
            "send after close",
            row_bounded("send", escaped.send_message(&queue, "late"))?,
            closed(IntegrationOperation::Publish),
        )?;
        expect_refused(
            "receive after close",
            row_bounded(
                "receive",
                escaped.receive_messages(&queue, 1, Duration::ZERO),
            )?,
            closed(IntegrationOperation::Receive),
        )?;
        expect_refused(
            "delete after close",
            row_bounded("delete", escaped.delete_message(&queue, "receipt"))?,
            closed(IntegrationOperation::Delete),
        )
    });
    let verdict = clean_run(outcome).and_then(|()| peer.control().log().expect_requests(&[]));
    peer.finished(ROW_BOUND, verdict)
}

/// A close waits for running work only within its own bound: a send the peer
/// holds past it is cut, with an unknown outcome because it was submitted,
/// and the close itself succeeds.
fn close_cuts_a_running_operation() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.held_until_close();
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([Reply::Hold]);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        runtime::block_on(async {
            let mut held = pin!(client.send_message(&queue, "held"));
            expect_polled_pending("the held send", &futures_util::poll!(held.as_mut()))?;
            control.wait_held(1, ROW_BOUND)?;
            released_by_close(&client, [held]).await
        })
    });
    let verdict =
        clean_run(outcome).and_then(|()| peer.control().log().expect_requests(&["SendMessage"]));
    peer.finished(ROW_BOUND, verdict)
}

/// Dropping the waiter of a submitted send cancels its work and frees its
/// operation slot: with one slot, the next operation is admitted and the
/// close settles. The send's outcome is unknown and nobody read it, so the
/// runtime aggregate keeps it.
fn dropped_waiter_releases_its_operation() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder().max_in_flight(1);
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([Reply::Hold]);
    let outcome = run_observing(runtime::builder(), move || -> Row {
        let client = settled("connect", builder.connect())?;
        runtime::block_on(async {
            let mut held = Box::pin(client.send_message(&queue, "abandoned"));
            expect_polled_pending("the held send", &futures_util::poll!(held.as_mut()))?;
            control.wait_held(1, ROW_BOUND)?;
            drop(held);
            integration_admitted_after(IntegrationOperation::Ready, ROW_BOUND, || {
                client.ready(&queue)
            })
            .await
        })?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = expect_failed_run(outcome, "the abandoned send left no aggregate", |error| {
        expect_aggregate(
            error,
            IntegrationKind::Sqs,
            &[cancelled(IntegrationOperation::Publish)],
        )
    });
    let verdict = verdict.and_then(|()| {
        let log = peer.control().log();
        expect_eq("send requests", log.count("SendMessage"), 1)?;
        expect_eq("queue queries", log.count("GetQueueAttributes"), 1)
    });
    peer.finished(ROW_BOUND, verdict)
}

/// Dropping the waiter of a send that never reached the transport leaves
/// nothing to account for: the send stalls loading credentials before it can
/// sign, its drop releases the account, the close settles, and the runtime
/// returns no aggregate.
fn unsubmitted_dropped_waiter_leaves_no_account() -> Row {
    let peer = SqsPeer::start();
    let (builder, mut loads) = SqsCredentialProbe::answers_once(peer.held_until_close());
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        runtime::block_on(async {
            let mut stalled = Box::pin(client.send_message(&queue, "abandoned"));
            expect_polled_pending("the stalled send", &futures_util::poll!(stalled.as_mut()))?;
            tokio::time::timeout(ROW_BOUND, loads.reached(2))
                .await
                .map_err(|_| "the send never asked for credentials".to_owned())?;
            drop(stalled);
            Ok::<(), String>(())
        })?;
        expect_ok("close", row_bounded("close", client.close())?)
    });
    let verdict = clean_run(outcome).and_then(|()| peer.control().expect_no_connection());
    peer.finished(ROW_BOUND, verdict)
}

/// A clone that outlives its runtime keeps only inert state: every operation
/// is `Closed` and nothing reaches the peer.
fn escaped_handle_is_inert_after_its_runtime() -> Row {
    let peer = SqsPeer::start();
    let builder = peer.builder();
    let queue = peer.queue_url(QUEUE);
    let outcome = runtime::builder().run(move || -> Result<Client, String> {
        let client = settled("connect", builder.connect())?;
        let escaped = client.clone();
        expect_ok("close", row_bounded("close", client.close())?)?;
        Ok(escaped)
    });
    let verdict = match outcome {
        Ok(Ok(escaped)) => on_tokio(async {
            expect_refused(
                "send on an escaped handle",
                escaped.send_message(&queue, "late").await,
                closed(IntegrationOperation::Publish),
            )?;
            expect_refused(
                "receive on an escaped handle",
                escaped.receive_messages(&queue, 1, Duration::ZERO).await,
                closed(IntegrationOperation::Receive),
            )?;
            expect_ok("close on an escaped handle", escaped.close().await)
        })
        .and_then(|verdict| verdict),
        Ok(Err(reason)) => Err(reason),
        Err(error) => Err(format!("the runtime tore down with {error:?}")),
    };
    let verdict = verdict.and_then(|()| peer.control().log().expect_requests(&[]));
    peer.finished(ROW_BOUND, verdict)
}

/// On one worker, a receive the peer holds does not stop a readiness query:
/// no operation blocks the worker it runs on. The close then cuts the held
/// receive, whose caller reads the cut.
fn single_worker_runtime_makes_progress() -> Row {
    let peer = SqsPeer::start();
    let builder = peer
        .builder()
        .operation_timeout(ROW_BOUND)
        .shutdown_timeout(EXHAUSTED);
    let control = peer.control();
    let queue = peer.queue_url(QUEUE);
    control.script([Reply::Hold]);
    let outcome = runtime::builder().worker_threads(1).run(move || -> Row {
        let client = settled("connect", builder.connect())?;
        runtime::block_on(async {
            let mut held = pin!(client.receive_messages(&queue, 1, Duration::from_secs(20)));
            expect_polled_pending("the held receive", &futures_util::poll!(held.as_mut()))?;
            control.wait_held(1, ROW_BOUND)?;
            let ready = tokio::time::timeout(ROW_BOUND, async {
                tokio::select! {
                    biased;
                    ready = client.ready(&queue) => Ok(ready),
                    _ = held.as_mut() => Err("the held receive finished first".to_owned()),
                }
            })
            .await
            .map_err(|_| "readiness made no progress".to_owned())??;
            expect_ok("ready beside a held receive", ready)?;
            let (received, closed) =
                tokio::time::timeout(ROW_BOUND, async { tokio::join!(held, client.close()) })
                    .await
                    .map_err(|_| "close did not cut the held receive".to_owned())?;
            expect_refused(
                "the cut receive",
                received,
                cancelled(IntegrationOperation::Receive),
            )?;
            expect_ok("close", closed)
        })
    });
    let verdict = clean_run(outcome).and_then(|()| {
        let log = peer.control().log();
        expect_eq("queue queries", log.count("GetQueueAttributes"), 1)
    });
    peer.finished(ROW_BOUND, verdict)
}
