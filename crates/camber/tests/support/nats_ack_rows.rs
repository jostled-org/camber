//! Row helpers for acknowledged NATS publishing against the scripted peer.
//!
//! The component and acceptance roots share them. A helper reads a peer
//! record, an owner-committed result, or the read-only acknowledgement probe;
//! none supplies an outcome. The peer answers a publication only when a row
//! calls [`PeerControl::reply`] with bytes the row chose.

use crate::integration_rows::{
    ROW_BOUND, Refusal, Row, cancelled, expect, expect_eq, expect_ok, expect_polled_pending,
    integration_admitted_after, integration_aggregate, limit_exceeded, observed_verdict,
    permission_denied, refusal, rejected, row_bounded, run_observing, settled, unavailable,
    unknown,
};
use crate::nats_peer::wire::{Publication, Reply};
use crate::nats_peer::{NatsPeer, PeerControl, PeerLog, Script};
use camber::mq::nats::{self, Connection, NatsBuilder};
use camber::runtime_test_support::{
    NatsAckProbe, NatsAckReceiver, NatsAckSnapshot, NatsPublishProbe,
};
use camber::{IntegrationKind, IntegrationOperation, RuntimeError, runtime};
use std::future::Future;
use std::pin::{Pin, pin};
use std::time::Duration;

/// The stream every acknowledged row expects.
pub const STREAM: &str = "EVENTS";

/// The subject every acknowledged row publishes to.
pub const SUBJECT: &str = "events.created";

/// The largest acknowledgement Camber decodes, in bytes.
pub const MAX_ACK_BYTES: usize = 4096;

/// The aggregate grace of a forced-stop row: far inside the publish's own
/// expiry, so only the forced stop can end it.
pub const FORCED_GRACE: Duration = Duration::from_millis(500);

const PUBLISH: IntegrationOperation = IntegrationOperation::Publish;

/// What a row's runtime may tear down with.
#[derive(Clone, Copy, Debug)]
pub enum Teardown {
    /// No aggregate.
    Clean,
    /// Exactly one publish cancelled after submission.
    Cancelled,
    /// Either: a receipt and a cancellation raced without an order.
    CleanOrCancelled,
}

/// Fail the row unless the closure passed and the runtime tore down as
/// `expected`.
fn expect_teardown(outcome: (Option<Row>, Result<(), RuntimeError>), expected: Teardown) -> Row {
    observed_verdict(outcome.0)?;
    let held = held_refusals(&outcome.1)?;
    let permitted = match (expected, held.as_deref()) {
        (Teardown::Clean | Teardown::CleanOrCancelled, None) => true,
        (Teardown::Cancelled | Teardown::CleanOrCancelled, Some(held)) => {
            held == [cancelled(PUBLISH)]
        }
        (Teardown::Clean, Some(_)) | (Teardown::Cancelled, None) => false,
    };
    expect(
        &format!(
            "the runtime tore down with {:?}, not {expected:?}",
            outcome.1
        ),
        permitted,
    )
}

/// Run `body` on one acknowledged connection to a fresh peer, configured by
/// `configure`, then close it and require its receiver ended. The peer
/// finishes on every exit; the runtime must tear down as `expected`.
///
/// # Errors
///
/// When setup, the row, close, receiver cleanup, or runtime teardown fails.
pub fn on_connection(
    configure: impl FnOnce(NatsBuilder) -> NatsBuilder,
    expected: Teardown,
    body: impl FnOnce(&mut Connection, &PeerControl) -> Row,
) -> Row {
    let peer = NatsPeer::start();
    let builder = configure(acknowledged(&peer.url()));
    let control = peer.control();
    let outcome = run_observing(runtime::builder(), move || -> Row {
        let mut connection = settled("connect", builder.connect())?;
        let probe = ack_probe(&connection)?;
        body(&mut connection, &control)?;
        expect_ok("close", row_bounded("close", connection.close())?)?;
        expect_ended(&probe)
    });
    peer.finished(ROW_BOUND, expect_teardown(outcome, expected))
}

/// Fail the row unless the receiver ended and released every entry and
/// its allocation.
///
/// # Errors
///
/// When the receiver or correlation storage survives close.
pub fn expect_ended(probe: &NatsAckProbe) -> Row {
    let snapshot = probe.snapshot();
    expect_eq(
        "the reply receiver",
        snapshot.receiver,
        NatsAckReceiver::Ended,
    )?;
    expect_eq("pending correlation entries", snapshot.pending, 0)?;
    expect_eq("the correlation map's allocation", snapshot.capacity, 0)
}

/// A publish that escapes its runtime's closure unread.
pub type HeldPublish = Pin<Box<dyn Future<Output = Result<(), RuntimeError>> + Send>>;

/// What a row answers one publication with.
pub enum Answer {
    Body(Box<str>),
    Status(u16),
}

impl Answer {
    /// The frame the peer writes for this answer.
    #[must_use]
    pub fn reply(&self) -> Reply<'_> {
        match self {
            Self::Body(body) => Reply::Message(body.as_bytes()),
            Self::Status(code) => Reply::Status {
                code: *code,
                description: "status",
            },
        }
    }
}

/// One correlated reply: what it is, the answer, and the class it settles.
pub type CorrelatedAnswer = (&'static str, Answer, Option<Refusal>);

/// Every correlated reply and the class it settles: success only for a
/// matching stream and a positive sequence within the decode bound; explicit
/// typed refusals by code; everything inconclusive is an unknown outcome.
#[must_use]
pub fn correlated_answers() -> Box<[CorrelatedAnswer]> {
    let padded = |width: usize| format!("{:<width$}", ack(STREAM, 9));
    Box::new([
        (
            "a matching acknowledgement",
            Answer::Body(ack(STREAM, 1).into()),
            None,
        ),
        (
            "an acknowledgement of exactly 4096 bytes",
            Answer::Body(padded(MAX_ACK_BYTES).into()),
            None,
        ),
        (
            "an acknowledgement of 4097 bytes",
            Answer::Body(padded(MAX_ACK_BYTES + 1).into()),
            Some(unknown(PUBLISH)),
        ),
        (
            "no responders",
            Answer::Status(503),
            Some(unavailable(PUBLISH)),
        ),
        (
            "a typed stream mismatch",
            Answer::Body(jetstream_error(400, 10060).into()),
            Some(rejected(PUBLISH)),
        ),
        (
            "a typed message size refusal",
            Answer::Body(jetstream_error(400, 10054).into()),
            Some(limit_exceeded(PUBLISH)),
        ),
        (
            "a typed header size refusal",
            Answer::Body(jetstream_error(400, 10097).into()),
            Some(limit_exceeded(PUBLISH)),
        ),
        (
            "a correlated permission refusal",
            Answer::Body(jetstream_error(403, 0).into()),
            Some(permission_denied(PUBLISH)),
        ),
        (
            "another decoded JetStream error",
            Answer::Body(jetstream_error(500, 10077).into()),
            Some(rejected(PUBLISH)),
        ),
        (
            "malformed JSON",
            Answer::Body("{\"stream\":".into()),
            Some(unknown(PUBLISH)),
        ),
        (
            "another stream's acknowledgement",
            Answer::Body(ack("OTHER", 1).into()),
            Some(unknown(PUBLISH)),
        ),
        (
            "a stream name differing in case",
            Answer::Body(ack("events", 1).into()),
            Some(unknown(PUBLISH)),
        ),
        (
            "a zero sequence",
            Answer::Body(ack(STREAM, 0).into()),
            Some(unknown(PUBLISH)),
        ),
        (
            "an unexpected status",
            Answer::Status(408),
            Some(unknown(PUBLISH)),
        ),
    ])
}

/// A builder for the peer at `url` that expects [`STREAM`].
pub fn acknowledged(url: &str) -> NatsBuilder {
    nats::builder(url).acknowledged_publishing(STREAM)
}

/// A JetStream publish acknowledgement.
#[must_use]
pub fn ack(stream: &str, sequence: u64) -> String {
    format!("{{\"stream\":\"{stream}\",\"seq\":{sequence}}}")
}

/// A decoded JetStream error response.
#[must_use]
pub fn jetstream_error(code: u16, err_code: u64) -> String {
    format!("{{\"error\":{{\"code\":{code},\"err_code\":{err_code},\"description\":\"refused\"}}}}")
}

/// The reply subject publication `publication` named.
///
/// # Errors
///
/// When the publication named none.
pub fn reply_to(publication: &Publication) -> Result<&str, String> {
    publication
        .reply
        .as_deref()
        .ok_or_else(|| format!("{publication:?} named no reply subject"))
}

/// A well-formed reply subject under the private inbox `publication`
/// replied to, whose token no publish holds.
///
/// # Errors
///
/// When the publication named no reply, or its reply has no numeric token.
pub fn stranger_reply(publication: &Publication) -> Result<String, String> {
    let reply = reply_to(publication)?;
    let (inbox, token) = reply
        .rsplit_once('.')
        .ok_or_else(|| format!("the reply {reply} names no token"))?;
    let token: u64 = token
        .parse()
        .map_err(|error| format!("the reply {reply} names a non-numeric token: {error}"))?;
    Ok(format!("{inbox}.{}", token + 1000))
}

/// Publication number `index`, counting from zero, once the peer read it.
///
/// # Errors
///
/// When the peer read fewer within [`ROW_BOUND`].
pub fn publication(control: &PeerControl, index: usize) -> Result<Publication, String> {
    control
        .publications(index + 1, ROW_BOUND)?
        .get(index)
        .cloned()
        .ok_or_else(|| format!("publication {index} is missing"))
}

/// Answer publication `read` with a matching acknowledgement numbered
/// `sequence`.
///
/// # Errors
///
/// When the publication named no reply or the peer could not write it.
pub fn acknowledge(control: &PeerControl, read: &Publication, sequence: u64) -> Row {
    control
        .reply(
            reply_to(read)?,
            &Reply::Message(ack(STREAM, sequence).as_bytes()),
        )
        .map(drop)
}

/// Await `future` under the hang guard.
///
/// # Errors
///
/// When [`ROW_BOUND`] passes first.
pub async fn finished_within<T>(what: &str, future: impl Future<Output = T>) -> Result<T, String> {
    tokio::time::timeout(ROW_BOUND, future)
        .await
        .map_err(|_| format!("{what} never finished"))
}

/// Fail the row unless `publish`, named `what`, is pending at its first poll
/// and, read by the peer as publication `index`, still pending once the SDK
/// processed every earlier frame.
///
/// # Errors
///
/// When the publish finished at once, the peer never read it, or the publish
/// finished before its acknowledgement.
pub async fn expect_withheld<F>(
    what: &str,
    control: &PeerControl,
    index: usize,
    mut publish: Pin<&mut F>,
) -> Result<Publication, String>
where
    F: Future<Output = Result<(), RuntimeError>> + ?Sized,
{
    expect_polled_pending(what, &futures_util::poll!(publish.as_mut()))?;
    let read = publication(control, index)?;
    control.delivery_barrier(ROW_BOUND)?;
    expect_polled_pending(
        &format!("{what} with its acknowledgement withheld"),
        &futures_util::poll!(publish.as_mut()),
    )?;
    Ok(read)
}

/// Start one publish, require it withheld once the peer read it as
/// publication `index`, answer it with `reply`, and return its result.
///
/// # Errors
///
/// When the publish finished before its reply, or never after it.
pub async fn answered(
    connection: &Connection,
    control: &PeerControl,
    index: usize,
    reply: Reply<'_>,
) -> Result<Result<(), RuntimeError>, String> {
    let mut publish = pin!(connection.publish(SUBJECT, b"payload"));
    let read =
        expect_withheld("the acknowledged publish", control, index, publish.as_mut()).await?;
    control.reply(reply_to(&read)?, &reply)?;
    finished_within("the answered publish", publish).await
}

/// Prove a retired operation's slot accepts another acknowledged publish.
/// Correlation retirement can precede slot release; wait for admission first.
///
/// # Errors
///
/// When publication `index` cannot complete or leaves a correlation entry.
pub fn slot_reused(connection: &Connection, control: &PeerControl, index: usize) -> Row {
    camber::runtime::block_on(integration_admitted_after(
        IntegrationOperation::Subscribe,
        ROW_BOUND,
        || connection.subscribe("after-retirement"),
    ))?;
    let outcome = camber::runtime::block_on(answered(
        connection,
        control,
        index,
        Reply::Message(ack(STREAM, 99).as_bytes()),
    ))?;
    expect_ok("the publish in the freed slot", outcome)?;
    expect_retired(connection)
}

/// Deliver a late receipt and verify that it leaves no correlation entry.
///
/// # Errors
///
/// When delivery fails or the retired entry reappears.
pub fn late_reply_is_discarded(
    connection: &Connection,
    control: &PeerControl,
    read: &Publication,
) -> Row {
    acknowledge(control, read, 1)?;
    control.delivery_barrier(ROW_BOUND)?;
    expect_retired(connection)
}

/// Answer publication 0 with a receipt and then a refusal for the same
/// token: the publish settles once, as success.
///
/// # Errors
///
/// When the publish was not withheld, or did not read the receipt.
pub async fn answered_twice(connection: &Connection, control: &PeerControl) -> Row {
    let mut publish = pin!(connection.publish(SUBJECT, b"twice"));
    let read = expect_withheld("the publish", control, 0, publish.as_mut()).await?;
    acknowledge(control, &read, 1)?;
    control.reply(
        reply_to(&read)?,
        &Reply::Message(jetstream_error(400, 10060).as_bytes()),
    )?;
    expect_ok(
        "the publish answered twice",
        finished_within("the publish", publish).await?,
    )?;
    control.delivery_barrier(ROW_BOUND)
}

/// Lose the transport after the peer read publication `index`: the
/// publication and what the publish read.
///
/// # Errors
///
/// When the publish was not withheld, the peer saw no refused reconnect, or
/// the publish never finished.
pub async fn disconnect_after_submission(
    connection: &Connection,
    control: &PeerControl,
    index: usize,
) -> Result<(Publication, Result<(), RuntimeError>), String> {
    let mut publish = pin!(connection.publish(SUBJECT, b"cut"));
    let read = expect_withheld("the publish", control, index, publish.as_mut()).await?;
    cut_transport(control)?;
    Ok((read, finished_within("the publish", publish).await?))
}

/// Drop the transport, let the SDK see one refused reconnect, then serve
/// again.
///
/// # Errors
///
/// When the peer refused no reconnect within [`ROW_BOUND`].
pub fn cut_transport(control: &PeerControl) -> Row {
    let accepted = control.log().accepted;
    control.script(Script::Refuse);
    control.wait_for("a refused reconnect", ROW_BOUND, |log| {
        log.accepted > accepted
    })?;
    control.script(Script::Serve);
    Ok(())
}

/// The private inbox subscriptions the peer read, across connections.
#[must_use]
pub fn private_subscriptions(log: &PeerLog) -> usize {
    log.subscribed
        .iter()
        .filter(|(subject, _, _)| subject.starts_with("_INBOX."))
        .count()
}

/// Wait until the SDK reconnected and installed its private inbox again: a
/// peer record, so no readiness poll adds a terminal.
///
/// # Errors
///
/// When the peer read no second private subscription within [`ROW_BOUND`].
pub fn wait_resubscribed(control: &PeerControl) -> Row {
    control
        .wait_for("the private inbox resubscribed", ROW_BOUND, |log| {
            private_subscriptions(log) >= 2
        })
        .map(drop)
}

/// Fail the row unless publish work reaches `hold` within [`ROW_BOUND`].
///
/// # Errors
///
/// When the connection ended first, or the guard passed.
pub async fn expect_reached_hold(hold: &mut NatsPublishProbe) -> Row {
    let reached = finished_within("the publish's hold", hold.reached()).await?;
    expect("the publish reached the hold", reached)
}

/// Poll readiness until it is refused as `until`, under the hang guard
/// named `what`. Every poll is one terminal, so the row counts the polls
/// that still answered ready.
///
/// # Errors
///
/// Any other answer, or the guard passed.
pub fn ready_polls_until(
    what: &str,
    connection: &Connection,
    until: Refusal,
) -> Result<usize, String> {
    row_bounded(what, async {
        let mut ready = 0_usize;
        loop {
            match connection.ready().map_err(|error| refusal(&error)) {
                Ok(()) => ready += 1,
                Err(Some(answer)) if answer == until => return Ok(ready),
                Err(other) => return Err(format!("ready before {until:?} answered {other:?}")),
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })?
}

/// The NATS refusals a runtime's teardown retained: `None` for a clean run.
///
/// # Errors
///
/// When the teardown error is no lifecycle aggregate.
pub fn held_refusals(
    teardown: &Result<(), RuntimeError>,
) -> Result<Option<Box<[Refusal]>>, String> {
    match teardown {
        Ok(()) => Ok(None),
        Err(error) => Ok(Some(
            integration_aggregate(error, IntegrationKind::Nats)?
                .into_iter()
                .map(|(_, held)| held)
                .collect(),
        )),
    }
}

/// Fail the row unless a forced teardown retains at most one publish
/// account, and only the class its waiter read.
///
/// # Errors
///
/// The retained publish accounts, or a teardown error that is no aggregate.
pub fn expect_forced_accounts(teardown: &Result<(), RuntimeError>, read: Option<Refusal>) -> Row {
    let publishes: Box<[Refusal]> = held_refusals(teardown)?
        .unwrap_or_default()
        .into_iter()
        .filter(|(operation, _, _)| *operation == PUBLISH)
        .collect();
    expect(
        &format!("the forced stop retained {publishes:?} for a publish that read {read:?}"),
        publishes.is_empty() || publishes.iter().map(|held| Some(*held)).eq([read]),
    )
}

/// The read-only acknowledgement probe of an acknowledged connection.
///
/// # Errors
///
/// When `connection` publishes Core.
pub fn ack_probe(connection: &Connection) -> Result<NatsAckProbe, String> {
    NatsAckProbe::observe(connection)
        .ok_or_else(|| "an acknowledged connection exposed no acknowledgement state".to_owned())
}

/// Fail the row unless the connection retains no correlation entry and its
/// receiver still routes.
///
/// # Errors
///
/// The live entries or the ended receiver.
pub fn expect_retired(connection: &Connection) -> Row {
    expect_routing(ack_probe(connection)?.snapshot(), 0)
}

/// Fail the row unless `snapshot` holds exactly `pending` correlation
/// entries and its receiver still routes.
///
/// # Errors
///
/// The other entry count or the ended receiver.
pub fn expect_routing(snapshot: NatsAckSnapshot, pending: usize) -> Row {
    expect_eq("pending correlation entries", snapshot.pending, pending)?;
    expect_eq(
        "the reply receiver",
        snapshot.receiver,
        NatsAckReceiver::Routing,
    )
}

/// Wait until `probe` reads `reached`, under the hang guard.
///
/// For state an operation's own task retires after its waiter is gone: the
/// probe reads the owner's committed map, never a guess about scheduling.
///
/// # Errors
///
/// The last snapshot when [`ROW_BOUND`] passes first.
pub fn wait_snapshot(
    what: &str,
    probe: &NatsAckProbe,
    reached: impl Fn(&NatsAckSnapshot) -> bool,
) -> Result<NatsAckSnapshot, String> {
    row_bounded(what, async {
        loop {
            let snapshot = probe.snapshot();
            if reached(&snapshot) {
                return snapshot;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .map_err(|error| format!("{error}: last read {:?}", probe.snapshot()))
}

/// Poll committed readiness until the SDK reports the connection ready.
///
/// # Errors
///
/// When [`ROW_BOUND`] passes first.
pub fn wait_ready(connection: &Connection) -> Row {
    row_bounded("the SDK reconnect", async {
        while connection.ready().is_err() {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
}
