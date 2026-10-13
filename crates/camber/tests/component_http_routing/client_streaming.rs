//! Streaming 5.T1 and 5.T2: a streamed response read through the public
//! client, against real loopback peers.
//!
//! Every row sends a public request with `send_streaming`. A paced peer writes
//! the head and then each part of the body when the row lets it, so a row
//! reads what the client did while the rest of the body was not on the wire.
//!
//! # The observation these rows read
//!
//! `mock::transfer_owner(addr)` already watches the calls one client makes to
//! the peer at `addr`. These rows read one more view of it:
//!
//! - `ScopedTransferOwner::streamed() -> ClientBodyObservation`: what the
//!   streaming body owner of the calls to this peer did. The fields are the
//!   ones the body driver of Step 4 reports, and the production owner of a
//!   real response writes them. It is doc-hidden and read-only. It polls no
//!   frame and commits no result.
//!
//! Each row binds a peer of its own and streams one response from it, so the
//! counts are that one stream's. A real transport splits frames where it
//! reads them, so no row compares a frame count with a count of peer writes.
//! A row compares admitted chunks with delivered chunks, which the owner
//! decides.
//!
//! # Clocks
//!
//! Most rows run on real time: every wait is an acknowledgement from the peer
//! or the owner, under the fixture watchdog. A timer row pauses the clock,
//! starts a runnable driver, and advances only after the owner acknowledged
//! the wait the step is measured from.
//!
//! # Clause → row → the mutation that turns the row red
//!
//! | Clause | Row | Mutation |
//! |---|---|---|
//! | The counts are true | `assert_observations_are_calibrated` | falsify an observation count |
//! | The head returns while the body is withheld | `assert_head_precedes_withheld_body` | collect the body before the head returns |
//! | Repeated and non-text headers arrive as bytes | `assert_head_precedes_withheld_body` | keep one value, or decode a value as text |
//! | No body poll before demand | `assert_head_precedes_withheld_body`, `assert_retention_is_flat` | prefetch a frame at the handoff |
//! | A declared crossing is refused at the head | `DECLARED` | skip the declared-length check |
//! | The effective length is read, not the raw line | `BODYLESS` | read `Content-Length` of a HEAD answer |
//! | The exact limit ends cleanly | `assert_chunked_exact_limit_ends_cleanly` | refuse at the limit |
//! | A crossing after a delivered prefix is withheld whole | `assert_crossing_after_prefix_is_withheld` | count after delivery |
//! | Truncation is a transport failure | `TRUNCATED` | skip the real frame check, or return EOF on error |
//! | Trailers are framing | `assert_trailers_are_framing` | deliver or count a trailer |
//! | Encoded bytes are delivered | `CODINGS` | enable any codec |
//! | An error status is a head | `assert_error_status_is_a_head` | fail the send on a status |
//! | Retained payload is flat | `assert_retention_is_flat` | call buffered collection, or queue read-ahead |
//! | Retries and redirects end at the head | `assert_retry_and_redirect_end_at_the_head` | return the first head |
//! | Nothing is sent after the final head | `AFTER_HEAD` | wire a second retry loop |
//! | Both terminals read one head | `assert_buffered_and_streamed_agree` | build a second header rule |
//! | TLS streams | `assert_tls_stream_is_read` | stream only over plain TCP |
//! | The head carries its expiries | `CARRIED` | reset the carried deadline at the handoff |
//! | Stream admission needs finite bounds | `BUDGETS` | validate only the buffered send |
//! | A refused budget admits no owner | `assert_refused_budget_admits_no_owner` | read the stream's bounds after the task is admitted |
//! | The profiles share no pool, and a write discards both | `TRANSPORT_WRITES`, `assert_warm_profiles_are_reused` | reuse the buffered pool, or ignore a setter |
//! | The next request reads the written value | `assert_written_maximum_is_read`, `assert_written_policy_is_read`, `IDLE_CELLS` | ignore a setter |
//! | Stream idle is the demand's, buffered idle is the transport's | `IDLE_CELLS`, `BUFFERED_IDLE_CELLS` | reuse the buffered read timer |
//! | A restricted stream uses the checked snapshot | `assert_restricted_stream_*` | build a second policy path for streams |
//! | A mock stream passes the same admission | `assert_mock_streams_under_the_same_admission` | bypass mock admission |
//!
//! # What these rows do not prove
//!
//! A mock row resolves nothing and dials nothing. It proves the order of
//! admission and the shape of the answer. It is not transport proof.
//!
//! The owner's demand idle and a transport read timer of the same length
//! would expire at one instant. The idle cells require `ResponseIdle`, and
//! `assert_warm_profiles_are_reused` proves that the two profiles hold
//! separate pools. Neither can show which timer a tie would read first.

use super::bounded_client::{DEFAULT_CEILING, QUIET_IDLE};
use super::client_body::{
    CALLER_PAUSE, CARRIED_BOUND, FIXED_CHUNK, HANDOFF_DELAY, LONG_BODY, SHORT_BODY,
};
use super::client_egress::{
    CROSSED_BACKOFF, FETCH, FETCH_AT_V4, FETCH_HOST, FLAKY, Family, LANDED, LITERAL_V4, NAME,
    ONE_HOP, Peers, SECURE_BODY, SECURE_FETCH, START, V4, V6, assert_completed_handshake,
    assert_refused_handshake, assert_undialed, at, closing_answer, excepting, named_request,
    restricted, secure_policy, target,
};
use crate::retry_upstream::{
    Answer, CHUNKED, HeaderEntry, Pace, Paced, PacedUpstream, PeerEvent, Recorded, Recording,
    RecordingUpstream, RunnableDriver, ScriptedUpstream, StreamHead, StreamNext, TIMER_ROUNDING,
    advance_to, answer, answered, assert_released, close, delivered_chunk, demand, failed_as,
    header_entries, must_settle, owner_must_reach, paced_chunk, paced_end, paced_head, parked,
    poll_once, read_out, required, settle_wakeups, settled, spawn_stream_get, stream_get,
    values_of, watch_streams, within_watchdog,
};
use crate::tls_support::generate_cert_with_san;
use crate::untrusted_tls::named_tls_peer;

use bytes::Bytes;
use camber::__private::ClientBodyObservation;
use camber::__private::ClientDnsEdge;
use camber::__private::client_test_root;
use camber::__private::client_transport_is_built;
use camber::RuntimeError;
use camber::http;
use camber::http::Client;
use camber::http::ClientBody;
use camber::http::ClientEgress;
use camber::http::ClientError;
use camber::http::ClientInput;
use camber::http::ClientResponse;
use camber::http::ClientStreamResponse;
use camber::http::ClientTimeout;
use camber::http::EgressPolicy;
use camber::http::RedirectPolicy;
use camber::http::Response;
use camber::http::TransferBudget;
use camber::http::mock::{self, ScopedTransferOwner};
use camber::runtime;
use camber::runtime_test_support::runtime_schedule;
use std::fmt::Debug;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// The one path every row requests of its own peer.
const PATH: &str = "/stream";

/// A client bound no row waits for.
const UNREACHED: Duration = Duration::from_secs(60);

const TRANSPORT: ClientError = ClientError::Transport;
const RESPONSE_IDLE: ClientError = ClientError::Timeout(ClientTimeout::ResponseIdle);
const STREAM_BUDGET: ClientError = ClientError::InvalidRequest(ClientInput::StreamBudget);

/// Send one request through the streaming terminal under its own maximum.
async fn stream_limited(client: &Client, method: &str, url: &str, limit: usize) -> StreamHead {
    client
        .request(method, url)?
        .response_limit(limit)?
        .send_streaming()
        .await
}

/// One GET through the streaming terminal, awaited to its head.
async fn sent(row: &str, client: &Client, url: &str) -> StreamHead {
    must_settle(row, "the send", stream_get(client, url)).await
}

/// One request through the streaming terminal under its own maximum, awaited
/// to its head.
async fn sent_limited(
    row: &str,
    client: &Client,
    method: &str,
    url: &str,
    limit: usize,
) -> StreamHead {
    must_settle(row, "the send", stream_limited(client, method, url, limit)).await
}

/// Send one GET through the buffered terminal.
async fn buffered(client: &Client, url: &str) -> Result<ClientResponse, RuntimeError> {
    client.request("GET", url)?.send().await
}

/// One GET through the buffered terminal, awaited to its result.
async fn sent_buffered(
    row: &str,
    client: &Client,
    url: &str,
) -> Result<ClientResponse, RuntimeError> {
    must_settle(row, "the buffered send", buffered(client, url)).await
}

/// Require a send refused as exactly `category`.
fn assert_refused(row: &str, head: StreamHead, category: ClientError) {
    assert_failed(row, "the streaming send", &head, category);
}

/// Require `what`, a demand or a close, to have failed as exactly `category`.
///
/// A close fails this way when it kept the committed failure.
fn assert_failed<T: Debug>(
    row: &str,
    what: &str,
    result: &Result<T, RuntimeError>,
    category: ClientError,
) {
    assert!(
        failed_as(result, category),
        "{row}: {what} returned {result:?} instead of {category:?}"
    );
}

/// Wait for the peer's counts, or panic naming what it never did.
async fn peer_reached(
    row: &str,
    peer: &mut PacedUpstream,
    what: &str,
    ready: impl Fn(&Paced) -> bool,
) -> Paced {
    match peer.reached(what, ready).await {
        Ok(seen) => seen,
        Err(unreached) => panic!("{row}: {unreached}"),
    }
}

/// Wait until the owner accepted the demand placed on it.
async fn accepted(row: &str, watched: &ScopedTransferOwner) -> ClientBodyObservation {
    owner_must_reach(row, watched, "accepted the demand", |seen| {
        seen.pending_demand
    })
    .await
}

/// One demand that must deliver a chunk.
async fn chunk(row: &str, body: &mut ClientBody) -> Bytes {
    delivered_chunk(row, demand(row, body).await)
}

/// Require a body that ended with no failure and a close that reported none.
fn assert_clean_end(row: &str, end: &StreamNext, closed: &Result<(), RuntimeError>) {
    assert!(matches!(end, Ok(None)), "{row}: the body ended as {end:?}");
    assert!(
        matches!(closed, Ok(())),
        "{row}: the close returned {closed:?}"
    );
}

/// Require a buffered call that failed as exactly `category`.
fn assert_buffered_failed(
    row: &str,
    collected: Result<ClientResponse, RuntimeError>,
    category: ClientError,
) {
    assert!(
        failed_as(&collected, category),
        "{row}: the buffered call returned {:?}",
        collected.map(|answer| answer.status())
    );
}

/// A streamed answer whose body was read to the first demand that delivered
/// no chunk, and is not closed yet.
struct Unclosed {
    status: u16,
    headers: Box<[HeaderEntry]>,
    payload: Vec<u8>,
    end: StreamNext,
    body: ClientBody,
}

/// What one streamed answer returned and delivered, to its close.
struct Drained {
    status: u16,
    /// Every header, in the order `headers` yields them.
    headers: Box<[HeaderEntry]>,
    payload: Vec<u8>,
    /// The first demand that delivered no chunk.
    end: StreamNext,
    closed: Result<(), RuntimeError>,
}

/// Read the head of `response`, then its body to the first demand that
/// delivers no chunk.
async fn read_to_end(row: &str, response: ClientStreamResponse) -> Unclosed {
    let status: u16 = response.status();
    let headers = header_entries(response.headers());
    let mut body: ClientBody = response.into_body();
    let (payload, end) = read_out(row, &mut body).await;
    Unclosed {
        status,
        headers,
        payload,
        end,
        body,
    }
}

impl Unclosed {
    /// Close the body, and keep what the close reported.
    async fn closed(self, row: &str) -> Drained {
        let Self {
            status,
            headers,
            payload,
            end,
            body,
        } = self;
        let closed = close(row, body).await;
        Drained {
            status,
            headers,
            payload,
            end,
            closed,
        }
    }
}

/// Require an answered head, read its body out, and close it.
async fn drained(row: &str, head: StreamHead) -> Drained {
    read_to_end(row, answered(row, head))
        .await
        .closed(row)
        .await
}

/// Open an answered head, read its body to a clean end, and close it.
///
/// Returns the payload. The end and the close must both report no failure.
async fn read_cleanly(row: &str, head: StreamHead) -> Vec<u8> {
    let read = drained(row, head).await;
    assert_clean_end(row, &read.end, &read.closed);
    read.payload
}

/// The payload of an admitted send, read cleanly. A send that is not
/// admitted must be refused as exactly `refused_as`, and delivers nothing.
async fn delivered_or_refused(
    row: &str,
    admitted: bool,
    head: StreamHead,
    refused_as: ClientError,
) -> Option<Vec<u8>> {
    if admitted {
        return Some(read_cleanly(row, head).await);
    }
    assert_refused(row, head, refused_as);
    None
}

/// Place one demand on a body whose next part the peer withholds, wait until
/// the owner accepted it, open one gate, and await the result.
///
/// Returns whether the demand was pending while the part was withheld.
async fn gated_demand(
    row: &str,
    body: &mut ClientBody,
    watched: &ScopedTransferOwner,
    peer: &PacedUpstream,
) -> (bool, StreamNext) {
    let mut next = std::pin::pin!(body.next());
    let pending = placed(row, next.as_mut(), watched).await;
    peer.open_gate();
    let result: StreamNext = must_settle(row, "the gated demand", next).await;
    (pending, result)
}

/// Poll one demand once, and wait until the owner accepted it.
///
/// Returns whether that first poll left the demand pending.
async fn placed(
    row: &str,
    next: Pin<&mut impl Future<Output = StreamNext>>,
    watched: &ScopedTransferOwner,
) -> bool {
    let first: Poll<StreamNext> = poll_once(next).await;
    accepted(row, watched).await;
    first.is_pending()
}

/// Drive one known frame and one release, and require every count to follow.
///
/// Every later row reads these counts, so a count that never moved would
/// pass them all.
async fn assert_observations_are_calibrated() {
    let row = "calibration";
    let mut peer = PacedUpstream::bind_steps(vec![
        Pace::Send(paced_head("200 OK", &[CHUNKED])),
        Pace::Gate,
        Pace::Send(paced_chunk(b"seven77")),
        Pace::UntilClosed,
    ])
    .await;
    let watched = watch_streams(peer.addr());
    let client: Client = http::client();

    let head: StreamHead = sent(row, &client, &peer.url(PATH)).await;
    let response: ClientStreamResponse = answered(row, head);
    peer_reached(row, &mut peer, "parked at its gate", |seen| seen.gates == 1).await;
    let parked = parked(row, &watched).await;
    let mut body: ClientBody = response.into_body();
    let (pending, taken) = gated_demand(row, &mut body, &watched, &peer).await;
    let after_delivery: ClientBodyObservation = watched.streamed();
    let closed = close(row, body).await;
    let after_close: ClientBodyObservation = watched.streamed();
    let left = peer.must_see_client_left(row).await;
    let ended = owner_must_reach(row, &watched, "ended its task", |seen| seen.owner_ended).await;
    let recording = peer.finish(row).await;

    assert_eq!(
        (parked.source_polls, parked.source_frames),
        (0, 0),
        "{row}: a parked owner polled its source: {parked:?}"
    );
    assert!(
        !parked.source_released && !parked.owner_ended && !parked.pending_demand,
        "{row}: a parked owner is neither released nor waiting on a frame: {parked:?}"
    );
    assert!(pending, "{row}: a withheld chunk was delivered");
    assert!(
        matches!(&taken, Ok(Some(chunk)) if &chunk[..] == b"seven77"),
        "{row}: the demand returned {taken:?}"
    );
    assert_eq!(
        (
            after_delivery.admitted_chunks,
            after_delivery.admitted_bytes,
            after_delivery.peak_retained_bytes,
            after_delivery.retained_chunks
        ),
        (1, 7, 7, 0),
        "{row}: one chunk of seven bytes was admitted and taken: {after_delivery:?}"
    );
    assert!(
        after_delivery.source_polls >= 1 && !after_delivery.source_released,
        "{row}: the source was read and is still held: {after_delivery:?}"
    );
    assert!(
        matches!(closed, Ok(())),
        "{row}: an intentional close returned {closed:?}"
    );
    assert!(
        after_close.source_released,
        "{row}: the close returned before the source was released: {after_close:?}"
    );
    assert_eq!(left.client_closes, 1, "{row}: the peer's held connection");
    assert_eq!(
        ended.retained_chunks, 0,
        "{row}: a chunk outlived its owner"
    );
    assert_eq!(recording.requests.len(), 1, "{row}: requests the peer read");
}

/// A head with two values of one header and one value that is not text.
const RAW_HEAD: &[u8] = b"HTTP/1.1 200 OK\r\nSet-Cookie: a=1\r\nSet-Cookie: b=2\r\nX-Raw: \xff\xfe\r\nTransfer-Encoding: chunked\r\n\r\n";

/// What a row read from the head [`RAW_HEAD`] before any demand.
struct RawHead {
    status: u16,
    first_cookie: Option<Box<[u8]>>,
    raw: Option<Box<[u8]>>,
    headers: Box<[HeaderEntry]>,
    rendering: Box<str>,
}

impl RawHead {
    fn of(response: &ClientStreamResponse) -> Self {
        Self {
            status: response.status(),
            first_cookie: response.header("set-cookie").map(Box::from),
            raw: response.header("X-Raw").map(Box::from),
            headers: header_entries(response.headers()),
            rendering: format!("{response:?} {response:#?}").into_boxed_str(),
        }
    }

    /// Require the status, both values of the repeated header in order, the
    /// value that is not text, and a Debug rendering that shows no value.
    fn assert_read(&self, row: &str) {
        assert_eq!(self.status, 200, "{row}: final status");
        assert_eq!(
            self.first_cookie.as_deref(),
            Some(&b"a=1"[..]),
            "{row}: the first value of a repeated header"
        );
        assert_eq!(
            *values_of(&self.headers, "set-cookie"),
            [&b"a=1"[..], &b"b=2"[..]],
            "{row}: every value of a repeated header, in order"
        );
        assert_eq!(
            self.raw.as_deref(),
            Some(&[0xff, 0xfe][..]),
            "{row}: a header value that is not text"
        );
        assert!(
            !self.rendering.contains("a=1") && !self.rendering.contains("b=2"),
            "{row}: Debug shows a header value: {}",
            self.rendering
        );
    }
}

/// The head returns while the peer withholds the whole body. The owner polls
/// nothing until a demand, each demand delivers one chunk, and the end is
/// sticky.
async fn assert_head_precedes_withheld_body() {
    let row = "head before a withheld body";
    let mut peer = PacedUpstream::bind_steps(vec![
        Pace::Send(RAW_HEAD.to_vec()),
        Pace::Gate,
        Pace::Send(paced_chunk(b"first")),
        Pace::Gate,
        Pace::Send(paced_end(&[])),
    ])
    .await;
    let watched = watch_streams(peer.addr());
    let client: Client = http::client();

    let head: StreamHead = sent(row, &client, &peer.url(PATH)).await;
    let response: ClientStreamResponse = answered(row, head);
    let withheld = peer_reached(row, &mut peer, "parked at its first gate", |seen| {
        seen.gates == 1
    })
    .await;
    let unasked: ClientBodyObservation = parked(row, &watched).await;
    let raw_head = RawHead::of(&response);

    let mut body: ClientBody = response.into_body();
    let (first_pending, first) = gated_demand(row, &mut body, &watched, &peer).await;
    let (end_pending, end) = gated_demand(row, &mut body, &watched, &peer).await;
    let at_end: ClientBodyObservation = watched.streamed();
    let again: StreamNext = demand(row, &mut body).await;
    let after_again: ClientBodyObservation = watched.streamed();
    let closed = close(row, body).await;
    let recording = peer.finish(row).await;

    raw_head.assert_read(row);
    assert_eq!(
        (withheld.sends, withheld.gates),
        (1, 1),
        "{row}: the head returned after the peer wrote body bytes"
    );
    assert_eq!(
        (unasked.source_polls, unasked.admitted_chunks),
        (0, 0),
        "{row}: the body was polled before any demand: {unasked:?}"
    );
    assert!(
        first_pending && end_pending,
        "{row}: a withheld part answered its demand"
    );
    assert!(
        matches!(&first, Ok(Some(chunk)) if &chunk[..] == b"first"),
        "{row}: the first demand returned {first:?}"
    );
    assert!(matches!(end, Ok(None)), "{row}: the end returned {end:?}");
    assert!(
        at_end.source_released,
        "{row}: the end returned before the source was released: {at_end:?}"
    );
    assert!(
        matches!(again, Ok(None)),
        "{row}: the end is not sticky: {again:?}"
    );
    assert_eq!(
        after_again.source_polls, at_end.source_polls,
        "{row}: the source was polled after its end"
    );
    assert!(
        matches!(closed, Ok(())),
        "{row}: a close after the end returned {closed:?}"
    );
    assert_eq!(recording.requests.len(), 1, "{row}: requests the peer read");
}

/// What one GET through the streaming terminal returned and delivered.
struct Read {
    answer: Drained,
    /// The owner's counts at the end the body returned.
    seen: ClientBodyObservation,
    recording: Recording,
}

/// Send `method` at a peer that answers with `steps`, read the body to its
/// end, close it, and join the peer.
async fn read_through(row: &str, method: &str, limit: usize, steps: Vec<Pace>) -> Read {
    let peer = PacedUpstream::bind_steps(steps).await;
    let watched = watch_streams(peer.addr());
    let client: Client = http::client();

    let head: StreamHead = sent_limited(row, &client, method, &peer.url(PATH), limit).await;
    let unclosed = read_to_end(row, answered(row, head)).await;
    let seen: ClientBodyObservation = watched.streamed();
    let answer = unclosed.closed(row).await;
    let recording = peer.finish(row).await;
    Read {
        answer,
        seen,
        recording,
    }
}

impl Read {
    /// Require this status, this payload, a clean end, and one request.
    fn assert_delivered(&self, row: &str, status: u16, payload: &[u8]) {
        let answer = &self.answer;
        assert_eq!(answer.status, status, "{row}: final status");
        assert_eq!(answer.payload, payload, "{row}: the delivered payload");
        assert_clean_end(row, &answer.end, &answer.closed);
        assert!(
            self.seen.source_released,
            "{row}: the end returned before the source was released: {:?}",
            self.seen
        );
        assert_eq!(
            self.seen.admitted_bytes,
            payload.len(),
            "{row}: the bytes the owner admitted"
        );
        assert_eq!(
            self.recording.requests.len(),
            1,
            "{row}: requests the peer read"
        );
    }
}

/// The request maximum of the declared-length rows.
const DECLARED_LIMIT: usize = 16;

/// One answer that declares its length.
struct Declared {
    row: &'static str,
    declared: usize,
    /// Whether the head is refused before it is returned.
    crossed: bool,
}

const DECLARED: [Declared; 3] = [
    Declared {
        row: "declared length under the limit",
        declared: DECLARED_LIMIT - 7,
        crossed: false,
    },
    Declared {
        row: "declared length at the limit",
        declared: DECLARED_LIMIT,
        crossed: false,
    },
    Declared {
        row: "declared length over the limit",
        declared: DECLARED_LIMIT + 1,
        crossed: true,
    },
];

/// A declared length within the limit is delivered whole. One past it is
/// refused before the head is returned, with no body poll.
async fn assert_declared_length(case: &Declared) {
    let row = case.row;
    let length = case.declared.to_string();
    let head_bytes = paced_head("200 OK", &[("Content-Length", &length)]);
    if !case.crossed {
        let payload = vec![b'd'; case.declared];
        let steps = vec![Pace::Send(head_bytes), Pace::Send(payload.clone())];
        let read = read_through(row, "GET", DECLARED_LIMIT, steps).await;
        read.assert_delivered(row, 200, &payload);
        return;
    }

    let mut peer = PacedUpstream::bind_steps(vec![Pace::Send(head_bytes), Pace::UntilClosed]).await;
    let watched = watch_streams(peer.addr());
    let client: Client = http::client();
    let head: StreamHead = sent_limited(row, &client, "GET", &peer.url(PATH), DECLARED_LIMIT).await;
    let seen: ClientBodyObservation = watched.streamed();
    let left = peer.must_see_client_left(row).await;
    let recording = peer.finish(row).await;

    assert_refused(
        row,
        head,
        ClientError::ResponseTooLarge {
            limit: DECLARED_LIMIT,
        },
    );
    assert_eq!(
        (seen.source_polls, seen.admitted_chunks),
        (0, 0),
        "{row}: a refused declaration was polled: {seen:?}"
    );
    assert_eq!(left.client_closes, 1, "{row}: the refused transport");
    assert_eq!(recording.requests.len(), 1, "{row}: requests the peer read");
}

/// The raw `Content-Length` of the bodyless rows: far over their limit.
const UNSENT_LENGTH: &str = "4096";

/// One answer whose effective body is empty whatever its head declares.
struct Bodyless {
    row: &'static str,
    method: &'static str,
    status_line: &'static str,
    status: u16,
    declared: Option<&'static str>,
}

const BODYLESS: [Bodyless; 3] = [
    Bodyless {
        row: "HEAD answer that declares a representation",
        method: "HEAD",
        status_line: "200 OK",
        status: 200,
        declared: Some(UNSENT_LENGTH),
    },
    Bodyless {
        row: "304 answer that declares a representation",
        method: "GET",
        status_line: "304 Not Modified",
        status: 304,
        declared: Some(UNSENT_LENGTH),
    },
    Bodyless {
        row: "204 answer",
        method: "GET",
        status_line: "204 No Content",
        status: 204,
        declared: None,
    },
];

/// A head that describes a representation that will not arrive is not a
/// crossing. The body ends cleanly with no payload.
async fn assert_effective_length_is_read(case: &Bodyless) {
    let row = case.row;
    let declared = case.declared.map(|length| ("Content-Length", length));
    let steps = vec![Pace::Send(paced_head(
        case.status_line,
        declared.as_slice(),
    ))];

    let read = read_through(row, case.method, DECLARED_LIMIT, steps).await;

    read.assert_delivered(row, case.status, b"");
    let raw: Box<[&[u8]]> = values_of(&read.answer.headers, "content-length");
    let expected: Box<[&[u8]]> = case
        .declared
        .iter()
        .map(|length| length.as_bytes())
        .collect();
    assert_eq!(
        raw, expected,
        "{row}: the raw length line is still a header"
    );
}

/// Chunked content that reaches the limit exactly is all delivered. The limit
/// itself ends nothing: the body succeeds at the clean end behind it.
async fn assert_chunked_exact_limit_ends_cleanly() {
    let row = "chunked body at the limit";
    let peer = PacedUpstream::bind_steps(vec![
        Pace::Send(paced_head("200 OK", &[CHUNKED])),
        Pace::Send(paced_chunk(b"01234")),
        Pace::Gate,
        Pace::Send(paced_chunk(b"567")),
        Pace::Gate,
        Pace::Send(paced_end(&[])),
    ])
    .await;
    let watched = watch_streams(peer.addr());
    let client: Client = http::client();

    let head: StreamHead = sent_limited(row, &client, "GET", &peer.url(PATH), 8).await;
    let mut body: ClientBody = answered(row, head).into_body();
    let prefix: Bytes = chunk(row, &mut body).await;
    let (_, rest) = gated_demand(row, &mut body, &watched, &peer).await;
    let at_limit: ClientBodyObservation = parked(row, &watched).await;
    let (end_pending, end) = gated_demand(row, &mut body, &watched, &peer).await;
    let closed = close(row, body).await;
    let recording = peer.finish(row).await;

    assert_eq!(&prefix[..], b"01234", "{row}: the first chunk");
    assert!(
        matches!(&rest, Ok(Some(chunk)) if &chunk[..] == b"567"),
        "{row}: the chunk that reaches the limit returned {rest:?}"
    );
    assert_eq!(
        (at_limit.admitted_bytes, at_limit.source_released),
        (8, false),
        "{row}: the limit itself ended the body: {at_limit:?}"
    );
    assert!(end_pending, "{row}: the body ended before its framing did");
    assert_clean_end(row, &end, &closed);
    assert_eq!(recording.requests.len(), 1, "{row}: requests the peer read");
}

/// A chunk that crosses the limit after a delivered prefix is never
/// delivered, in whole or in part, and the source ends with it.
async fn assert_crossing_after_prefix_is_withheld() {
    let row = "crossing chunk after a delivered prefix";
    let crossing = ClientError::ResponseTooLarge { limit: 8 };
    let mut peer = PacedUpstream::bind_steps(vec![
        Pace::Send(paced_head("200 OK", &[CHUNKED])),
        Pace::Send(paced_chunk(b"ppppp")),
        Pace::Gate,
        Pace::Send(paced_chunk(b"xxxx")),
        Pace::Send(paced_chunk(b"l")),
        Pace::UntilClosed,
    ])
    .await;
    let watched = watch_streams(peer.addr());
    let client: Client = http::client();

    let head: StreamHead = sent_limited(row, &client, "GET", &peer.url(PATH), 8).await;
    let mut body: ClientBody = answered(row, head).into_body();
    let prefix: Bytes = chunk(row, &mut body).await;
    let (_, crossed) = gated_demand(row, &mut body, &watched, &peer).await;
    let refused: ClientBodyObservation = watched.streamed();
    let again: [StreamNext; 2] = [demand(row, &mut body).await, demand(row, &mut body).await];
    let after_again: ClientBodyObservation = watched.streamed();
    let closed = close(row, body).await;
    let left = peer.must_see_client_left(row).await;
    let recording = peer.finish(row).await;

    assert_eq!(&prefix[..], b"ppppp", "{row}: the delivered prefix");
    assert_failed(row, "the demand", &crossed, crossing);
    assert!(
        refused.source_released,
        "{row}: the crossing returned before the source was released: {refused:?}"
    );
    assert_eq!(
        (
            refused.admitted_chunks,
            refused.admitted_bytes,
            refused.retained_chunks
        ),
        (1, 5, 0),
        "{row}: the crossing chunk was admitted or kept: {refused:?}"
    );
    for repeat in &again {
        assert_failed(row, "the demand", repeat, crossing);
    }
    assert_eq!(
        after_again.source_polls, refused.source_polls,
        "{row}: the source was polled after its terminal"
    );
    assert_failed(row, "the close", &closed, crossing);
    assert_eq!(left.client_closes, 1, "{row}: the crossed transport");
    assert_eq!(recording.requests.len(), 1, "{row}: requests the peer read");
}

/// The payload a truncated answer sends before its peer closes.
const PARTIAL: &[u8] = b"partial";

/// One answer whose framing never completes.
struct Truncated {
    row: &'static str,
    /// The framing line of the head. The declared length is within the limit.
    framing: (&'static str, &'static str),
    /// Whether the payload is written as one chunked frame.
    chunked: bool,
}

const TRUNCATED: [Truncated; 2] = [
    Truncated {
        row: "declared length cut short",
        framing: ("Content-Length", "64"),
        chunked: false,
    },
    Truncated {
        row: "chunked body cut short",
        framing: CHUNKED,
        chunked: true,
    },
];

/// A body whose peer closes before its framing ends fails as a transport
/// failure after the delivered prefix. It is never a clean end.
async fn assert_truncation_is_transport(case: &Truncated) {
    let row = case.row;
    let sent = match case.chunked {
        true => paced_chunk(PARTIAL),
        false => PARTIAL.to_vec(),
    };
    let steps = vec![
        Pace::Send(paced_head("200 OK", &[case.framing])),
        Pace::Send(sent),
        Pace::Close,
    ];

    let read = read_through(row, "GET", DEFAULT_CEILING, steps).await;

    assert_eq!(read.answer.status, 200, "{row}: final status");
    assert_eq!(read.answer.payload, PARTIAL, "{row}: the delivered prefix");
    assert_failed(row, "the demand", &read.answer.end, TRANSPORT);
    assert!(
        read.seen.source_released,
        "{row}: the failure returned before the source was released: {:?}",
        read.seen
    );
    assert_failed(row, "the close", &read.answer.closed, TRANSPORT);
    assert_eq!(
        read.recording.requests.len(),
        1,
        "{row}: requests the peer read"
    );
}

/// Trailers end the framing. They are not payload and are not counted.
async fn assert_trailers_are_framing() {
    let row = "chunked body with trailers";
    let steps = vec![
        Pace::Send(paced_head("200 OK", &[CHUNKED, ("Trailer", "X-Sum")])),
        Pace::Send(paced_chunk(b"payload")),
        Pace::Send(paced_end(&[("X-Sum", "trailer-value")])),
    ];

    let read = read_through(row, "GET", 7, steps).await;

    read.assert_delivered(row, 200, b"payload");
}

/// The empty gzip stream. A decoder would deliver no byte of it.
const EMPTY_GZIP: &[u8] = &[
    0x1f, 0x8b, 0x08, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x03, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

/// Each content coding a transport could decode, and the bytes the peer
/// sends under it.
///
/// A row requires the sent bytes and the coding's header back. It needs no
/// valid stream for that: a decoder that ran would change the bytes or fail
/// the read.
const CODINGS: [(&str, &[u8]); 4] = [
    ("gzip", EMPTY_GZIP),
    ("deflate", &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]),
    ("br", b"\x8b\x03\x80encoded\x03"),
    ("zstd", b"\x28\xb5\x2f\xfd\x04\x00\x39\x00\x00encoded"),
];

/// An encoded body is delivered as the bytes the peer sent, under a limit
/// that counts those bytes. The request asks for no coding.
async fn assert_encoded_bytes_are_delivered(coding: &str, encoded: &[u8]) {
    let row = format!("{coding}-coded body");
    let row = row.as_str();
    let length = encoded.len().to_string();
    let steps = vec![
        Pace::Send(paced_head(
            "200 OK",
            &[("Content-Encoding", coding), ("Content-Length", &length)],
        )),
        Pace::Send(encoded.to_vec()),
    ];

    let read = read_through(row, "GET", encoded.len(), steps).await;

    read.assert_delivered(row, 200, encoded);
    assert_eq!(
        *values_of(&read.answer.headers, "content-encoding"),
        [coding.as_bytes()],
        "{row}: the coding the peer named"
    );
    assert_eq!(
        *values_of(&read.answer.headers, "content-length"),
        [length.as_bytes()],
        "{row}: the encoded length the peer declared"
    );
    let asked: Box<[&str]> = read.recording.requests[0].header_values("accept-encoding");
    assert!(asked.is_empty(), "{row}: the request asked for {asked:?}");
}

/// An error status is a final head. Its body is read like any other.
async fn assert_error_status_is_a_head() {
    let row = "error status";
    let steps = vec![
        Pace::Send(paced_head("404 Not Found", &[("Content-Length", "7")])),
        Pace::Send(b"missing".to_vec()),
    ];

    let read = read_through(row, "GET", DEFAULT_CEILING, steps).await;

    read.assert_delivered(row, 404, b"missing");
}

/// What one retention row measured.
struct Retention {
    /// The owner's counts before the first demand, with body on the wire.
    unasked: ClientBodyObservation,
    /// The most payload the owner held at once.
    peak: usize,
    /// The payload bytes delivered.
    delivered: usize,
    /// The first delivery after which the owner had admitted a chunk the
    /// caller had not asked for, if there was one.
    read_ahead: Option<usize>,
}

/// Read a body of `chunks` fixed chunks the peer writes without waiting.
///
/// The peer is not the backpressure: it writes as fast as the socket takes.
/// After each delivery the owner must have admitted exactly the chunks the
/// caller took.
async fn retention(row: &str, chunks: usize) -> Retention {
    let mut steps = vec![Pace::Send(paced_head("200 OK", &[CHUNKED]))];
    steps.extend((0..chunks).map(|_| Pace::Send(paced_chunk(&[b'r'; FIXED_CHUNK]))));
    steps.push(Pace::Send(paced_end(&[])));
    let mut peer = PacedUpstream::bind_steps(steps).await;
    let watched = watch_streams(peer.addr());
    let client: Client = http::client();

    let head: StreamHead = sent_limited(
        row,
        &client,
        "GET",
        &peer.url(PATH),
        FIXED_CHUNK * LONG_BODY,
    )
    .await;
    let mut body: ClientBody = answered(row, head).into_body();
    peer_reached(row, &mut peer, "wrote body bytes", |seen| seen.sends >= 2).await;
    let unasked: ClientBodyObservation = parked(row, &watched).await;

    let (mut delivered, mut deliveries, mut read_ahead) = (0, 0, None);
    let end: StreamNext = loop {
        let taken: Bytes = match demand(row, &mut body).await {
            Ok(Some(taken)) => taken,
            end => break end,
        };
        delivered += taken.len();
        deliveries += 1;
        let seen: ClientBodyObservation = parked(row, &watched).await;
        if seen.admitted_chunks != deliveries || seen.retained_chunks != 0 {
            read_ahead.get_or_insert(deliveries);
        }
    };
    let peak = watched.streamed().peak_retained_bytes;
    let closed = close(row, body).await;
    peer.finish(row).await;

    assert_clean_end(row, &end, &closed);
    Retention {
        unasked,
        peak,
        delivered,
        read_ahead,
    }
}

/// At one chunk size and one limit, a long body retains no more than a short
/// one, is not read before a demand, and is not read ahead of one.
async fn assert_retention_is_flat() {
    for (row, chunks) in [("short body", SHORT_BODY), ("long body", LONG_BODY)] {
        let measured = retention(row, chunks).await;
        assert_eq!(
            (
                measured.unasked.source_polls,
                measured.unasked.admitted_chunks
            ),
            (0, 0),
            "{row}: body on the wire was read before any demand: {:?}",
            measured.unasked
        );
        assert_eq!(
            measured.delivered,
            chunks * FIXED_CHUNK,
            "{row}: the payload delivered"
        );
        assert_eq!(
            measured.read_ahead, None,
            "{row}: the owner admitted a chunk no demand asked for"
        );
        assert!(
            (1..=FIXED_CHUNK).contains(&measured.peak),
            "{row}: the owner held {} bytes at once, over one chunk of {FIXED_CHUNK}",
            measured.peak
        );
    }
}

/// Answer one request of the journey by its target.
///
/// `START` redirects to `FLAKY` at the same origin. The first `FLAKY` request
/// is a transient answer that closes its connection. The second is the final
/// answer, whose body the peer writes one chunk at a time.
fn journey(flaky_requests: &AtomicUsize, request: &Recorded) -> std::sync::Arc<[Pace]> {
    let steps = match request.target() {
        START => vec![Pace::Send(answer("302 Found", &[("Location", FLAKY)], b""))],
        FLAKY if flaky_requests.fetch_add(1, Ordering::SeqCst) == 0 => vec![
            Pace::Send(answer(
                "503 Service Unavailable",
                &[("Connection", "close")],
                b"",
            )),
            Pace::Close,
        ],
        FLAKY => vec![
            Pace::Send(paced_head("200 OK", &[CHUNKED])),
            Pace::Send(paced_chunk(LANDED)),
            Pace::Send(paced_end(&[])),
        ],
        _ => vec![Pace::Send(answer(
            "500 Internal Server Error",
            &[],
            b"unscripted target",
        ))],
    };
    steps.into()
}

/// A redirect and a retry are followed before the head is returned, and the
/// head returned is the final one.
async fn assert_retry_and_redirect_end_at_the_head() {
    let row = "redirect and retry before the head";
    let flaky_requests = AtomicUsize::new(0);
    let peer = PacedUpstream::bind(move |request| journey(&flaky_requests, request)).await;
    let client: Client = http::client()
        .retries(1)
        .backoff(CROSSED_BACKOFF)
        .redirect_policy(RedirectPolicy::SameOrigin { max_hops: ONE_HOP });

    let head: StreamHead = sent(row, &client, &peer.url(START)).await;
    let read = drained(row, head).await;
    let recording = peer.finish(row).await;

    assert_eq!(
        read.status, 200,
        "{row}: the head returned is the final one"
    );
    assert_eq!(read.payload, LANDED, "{row}: the final body");
    assert_clean_end(row, &read.end, &read.closed);
    let targets: Box<[&str]> = recording.targets();
    assert_eq!(
        *targets,
        [START, FLAKY, FLAKY],
        "{row}: the requests the peer read"
    );
}

/// What ends an operation whose final head has arrived.
#[derive(Clone, Copy, Debug)]
enum AfterHead {
    /// The head declares a crossing, so the send itself fails.
    DeclaredCrossing,
    /// The caller converts an error status before any demand.
    StatusConversion,
    /// The first demand meets a truncated body.
    BodyFailure,
    /// The caller drops the unread response.
    UnreadDrop,
}

const AFTER_HEAD: [AfterHead; 4] = [
    AfterHead::DeclaredCrossing,
    AfterHead::StatusConversion,
    AfterHead::BodyFailure,
    AfterHead::UnreadDrop,
];

impl AfterHead {
    fn steps(self) -> Vec<Pace> {
        match self {
            Self::DeclaredCrossing => vec![
                Pace::Send(paced_head("200 OK", &[("Content-Length", "17")])),
                Pace::UntilClosed,
            ],
            Self::StatusConversion => vec![
                Pace::Send(paced_head(
                    "500 Internal Server Error",
                    &[("Content-Length", "64")],
                )),
                Pace::Send(PARTIAL.to_vec()),
                Pace::UntilClosed,
            ],
            Self::BodyFailure => vec![
                Pace::Send(paced_head("200 OK", &[("Content-Length", "64")])),
                Pace::Send(PARTIAL.to_vec()),
                Pace::Close,
            ],
            Self::UnreadDrop => vec![
                Pace::Send(paced_head("200 OK", &[CHUNKED])),
                Pace::UntilClosed,
            ],
        }
    }

    /// The request maximum. Only the crossing row's head declares more.
    fn limit(self) -> usize {
        match self {
            Self::DeclaredCrossing => DECLARED_LIMIT,
            Self::StatusConversion | Self::BodyFailure | Self::UnreadDrop => DEFAULT_CEILING,
        }
    }
}

/// Once the final head has arrived, no failure and no conversion sends again,
/// even with retries to spare and before the first demand.
///
/// The row ends only after the owner task has ended, so a send the operation
/// still owed would already have reached the peer.
async fn assert_nothing_is_resent_after_the_head(case: AfterHead) {
    let row = format!("no send after the head: {case:?}");
    let row = row.as_str();
    let mut peer = PacedUpstream::bind_steps(case.steps()).await;
    let watched = watch_streams(peer.addr());
    let client: Client = http::client().retries(2).backoff(CROSSED_BACKOFF);

    let head: StreamHead = sent_limited(row, &client, "GET", &peer.url(PATH), case.limit()).await;
    match case {
        AfterHead::DeclaredCrossing => {
            assert_refused(
                row,
                head,
                ClientError::ResponseTooLarge {
                    limit: DECLARED_LIMIT,
                },
            );
            // An awaited send failure acknowledges release, and the peer's
            // closed transport is its evidence.
            peer.must_see_client_left(row).await;
        }
        AfterHead::StatusConversion => {
            let converted: Result<u16, RuntimeError> = answered(row, head)
                .error_for_status()
                .map(|kept: ClientStreamResponse| kept.status());
            assert!(
                failed_as(&converted, ClientError::Status(500)),
                "{row}: the conversion returned {converted:?}"
            );
        }
        AfterHead::BodyFailure => {
            let mut body: ClientBody = answered(row, head).into_body();
            let (_, end) = read_out(row, &mut body).await;
            assert_failed(row, "the demand", &end, TRANSPORT);
            let closed = close(row, body).await;
            assert_failed(row, "the close", &closed, TRANSPORT);
        }
        AfterHead::UnreadDrop => drop(answered(row, head)),
    }
    if !matches!(case, AfterHead::DeclaredCrossing) {
        owner_must_reach(row, &watched, "ended its task", |seen| {
            seen.source_released && seen.owner_ended
        })
        .await;
    }
    let recording = peer.finish(row).await;

    assert_eq!(
        (recording.connections, recording.requests.len()),
        (1, 1),
        "{row}: the peer read {:?}",
        recording.requests
    );
}

/// The buffered and the streaming terminal read one answer the same way: the
/// same status, the same header bytes in the same order, and the same body.
async fn assert_buffered_and_streamed_agree() {
    let row = "buffered and streamed answers";
    let reply = answer(
        "203 Non-Authoritative Information",
        &[("X-One", "1"), ("X-Two", "first"), ("X-Two", "second")],
        b"same-bytes",
    );
    let peer = RecordingUpstream::bind(reply).await;
    let client: Client = http::client();

    let collected = sent_buffered(row, &client, &peer.url(PATH)).await;
    let head: StreamHead = sent(row, &client, &peer.url(PATH)).await;
    let read = drained(row, head).await;
    let recording = peer.finish(row).await;

    let collected = answered(row, collected);
    let collected_headers = header_entries(collected.headers());
    assert_eq!(read.status, collected.status(), "{row}: final status");
    assert_eq!(read.headers, collected_headers, "{row}: the header list");
    assert_eq!(
        read.payload,
        collected.body_bytes(),
        "{row}: the body bytes"
    );
    assert_clean_end(row, &read.end, &read.closed);
    assert_eq!(recording.requests.len(), 2, "{row}: requests the peer read");
}

/// A streamed response is read over TLS as it is over plain TCP.
async fn assert_tls_stream_is_read() {
    let row = "stream over TLS";
    let (cert_pem, key_pem) = generate_cert_with_san("127.0.0.1");
    let mut peer = named_tls_peer(&cert_pem, &key_pem, closing_answer(SECURE_BODY)).await;
    let port = peer.addr().port();
    let client: Client = client_test_root(http::client(), &cert_pem)
        .unwrap_or_else(|error| panic!("{row}: the test root was refused: {error:?}"));

    let head: StreamHead = sent(row, &client, &format!("https://127.0.0.1:{port}{PATH}")).await;
    let read = drained(row, head).await;
    let handshake = within_watchdog(peer.recv()).await;
    peer.finish(row, within_watchdog).await;

    assert_eq!(read.status, 200, "{row}: final status");
    assert_eq!(read.payload, SECURE_BODY, "{row}: the delivered payload");
    assert_clean_end(row, &read.end, &read.closed);
    assert_completed_handshake(row, &handshake);
}

/// One expiry fixed before the head arrived.
struct Carried {
    row: &'static str,
    total: Duration,
    attempt: Duration,
    expired: ClientError,
}

const CARRIED: [Carried; 2] = [
    Carried {
        row: "attempt expiry carried through the head",
        total: UNREACHED,
        attempt: CARRIED_BOUND,
        expired: ClientError::Timeout(ClientTimeout::Attempt),
    },
    Carried {
        row: "operation expiry carried through the head",
        total: CARRIED_BOUND,
        attempt: UNREACHED,
        expired: ClientError::Timeout(ClientTimeout::Operation),
    },
];

/// An expiry fixed at the first poll ends a retained unread response at its
/// original instant, with no demand and no caller poll.
///
/// The head arrives two seconds after the first poll. An operation that
/// restarted either clock at the head would still be live at the original
/// instant.
async fn assert_head_carries_its_expiry(case: &Carried) {
    let row = case.row;
    tokio::time::pause();
    let driver = RunnableDriver::start();
    let mut upstream = ScriptedUpstream::bind(&[Answer::StallBodyAfter(HANDOFF_DELAY)]).await;
    let watched = watch_streams(upstream.addr());
    let client: Client = http::client()
        .total_timeout(case.total)
        .request_timeout(case.attempt)
        .connect_timeout(UNREACHED)
        .response_idle_timeout(UNREACHED);
    let url = upstream.url(PATH);
    let entry = Instant::now();
    let call: JoinHandle<StreamHead> = spawn_stream_get(client, url);

    upstream.expect(PeerEvent::Started(0), row).await;
    advance_to(entry + HANDOFF_DELAY).await;
    tokio::time::advance(TIMER_ROUNDING).await;
    upstream.expect(PeerEvent::Stalled(0), row).await;
    let response: ClientStreamResponse = answered(row, required(row, settled(call).await));
    parked(row, &watched).await;
    advance_to(entry + CARRIED_BOUND - TIMER_ROUNDING).await;
    settle_wakeups().await;
    let before: ClientBodyObservation = watched.streamed();
    advance_to(entry + CARRIED_BOUND).await;
    tokio::time::advance(TIMER_ROUNDING).await;
    let after = owner_must_reach(row, &watched, "released the source at the expiry", |seen| {
        seen.source_released
    })
    .await;
    let released = upstream.saw(PeerEvent::Released(0)).await;
    let mut body: ClientBody = response.into_body();
    let next: StreamNext = demand(row, &mut body).await;
    let closed = close(row, body).await;
    let starts = upstream.finish(driver, row).await;
    tokio::time::resume();

    assert!(
        !before.source_released,
        "{row}: the response ended before its original expiry: {before:?}"
    );
    assert_eq!(
        after.source_polls, 0,
        "{row}: an unread body was polled: {after:?}"
    );
    assert_released(released, row);
    assert_failed(row, "the demand", &next, case.expired);
    assert_failed(row, "the close", &closed, case.expired);
    assert_eq!(starts, 1, "{row}: the call started {starts} requests");
}

/// 5.T1
#[tokio::test]
async fn streamed_response_headers_precede_bounded_body() {
    assert_observations_are_calibrated().await;
    assert_head_precedes_withheld_body().await;
    for case in &DECLARED {
        assert_declared_length(case).await;
    }
    for case in &BODYLESS {
        assert_effective_length_is_read(case).await;
    }
    assert_chunked_exact_limit_ends_cleanly().await;
    assert_crossing_after_prefix_is_withheld().await;
    for case in &TRUNCATED {
        assert_truncation_is_transport(case).await;
    }
    assert_trailers_are_framing().await;
    for (coding, encoded) in CODINGS {
        assert_encoded_bytes_are_delivered(coding, encoded).await;
    }
    assert_error_status_is_a_head().await;
    assert_retention_is_flat().await;
    assert_retry_and_redirect_end_at_the_head().await;
    for case in AFTER_HEAD {
        assert_nothing_is_resent_after_the_head(case).await;
    }
    assert_buffered_and_streamed_agree().await;
    assert_tls_stream_is_read().await;
    // Last, because these are the rows that hold the clock still.
    for case in &CARRIED {
        assert_head_carries_its_expiry(case).await;
    }
}

/// The finite maximum, idle interval, and attempt lifetime a budget row
/// writes.
const WRITTEN_MAX: usize = 4096;
const WRITTEN_IDLE: Duration = Duration::from_secs(7);
const WRITTEN_TOTAL: Duration = Duration::from_secs(8);

/// A response policy with these three finite bounds.
fn budget(max: usize, idle: Duration, total: Duration) -> TransferBudget {
    TransferBudget::bounded(max, idle, total)
        .unwrap_or_else(|error| panic!("a finite response budget was refused: {error:?}"))
}

/// A response policy with every dimension finite.
fn finite_budget() -> TransferBudget {
    budget(WRITTEN_MAX, WRITTEN_IDLE, WRITTEN_TOTAL)
}

/// One client policy, sent through both terminals.
struct Budget {
    row: &'static str,
    /// The policy write under test.
    write: fn(Client) -> Client,
    /// The maximum the streaming request sets for itself, if it sets one.
    request_limit: Option<usize>,
    /// Whether the streaming terminal admits the call.
    admitted: bool,
}

const BUDGETS: [Budget; 9] = [
    Budget {
        row: "streaming and buffered, cold client, default policy",
        write: |client| client,
        request_limit: None,
        admitted: true,
    },
    Budget {
        row: "streaming and buffered, cold client, finite budget",
        write: |client| client.response_budget(finite_budget()),
        request_limit: None,
        admitted: true,
    },
    Budget {
        row: "streaming and buffered, cold client, budget with no attempt lifetime",
        write: |client| client.response_budget(finite_budget().without_total()),
        request_limit: None,
        admitted: true,
    },
    Budget {
        row: "streaming and buffered, cold client, unbounded_response",
        write: Client::unbounded_response,
        request_limit: None,
        admitted: false,
    },
    Budget {
        row: "streaming and buffered, cold client, budget with no maximum",
        write: |client| client.response_budget(finite_budget().without_max_bytes()),
        request_limit: None,
        admitted: false,
    },
    Budget {
        row: "streaming and buffered, cold client, budget with no idle interval",
        write: |client| client.response_budget(finite_budget().without_idle()),
        request_limit: None,
        admitted: false,
    },
    Budget {
        row: "streaming and buffered, cold client, unbounded budget",
        write: |client| client.response_budget(TransferBudget::unbounded()),
        request_limit: None,
        admitted: false,
    },
    Budget {
        row: "streaming and buffered, cold client, request limit over unbounded_response",
        write: Client::unbounded_response,
        request_limit: Some(WRITTEN_MAX),
        admitted: true,
    },
    Budget {
        row: "streaming and buffered, cold client, request limit over no idle interval",
        write: |client| client.response_budget(finite_budget().without_idle()),
        request_limit: Some(WRITTEN_MAX),
        admitted: false,
    },
];

/// A stream needs a finite maximum and a finite idle interval. A policy that
/// removed either is refused before the name is looked up or the peer is
/// dialed, and the buffered terminal of the same client still answers.
///
/// The client is restricted to the peer through a controlled lookup, so a
/// lookup the refused call started would be counted.
async fn assert_stream_budget_admission(case: &Budget) {
    let row = case.row;
    let peer = RecordingUpstream::bind(answer("200 OK", &[], LANDED)).await;
    let port = peer.addr().port();
    let edge = ClientDnsEdge::new();
    edge.answer(&[V4]);
    edge.answer(&[V4]);
    let client: Client = (case.write)(restricted(excepting(row, FETCH_AT_V4, port), &edge));
    let url = target(FETCH, port);

    let head: StreamHead = match case.request_limit {
        Some(limit) => sent_limited(row, &client, "GET", &url, limit).await,
        None => sent(row, &client, &url).await,
    };
    let lookups_after_stream: usize = edge.lookups();
    let delivered = delivered_or_refused(row, case.admitted, head, STREAM_BUDGET).await;
    buffer_landed(row, &client, &url).await;
    let recording = peer.finish(row).await;

    let streams = usize::from(case.admitted);
    assert_eq!(
        lookups_after_stream, streams,
        "{row}: lookups the streaming call started"
    );
    assert_eq!(
        delivered.as_deref(),
        case.admitted.then_some(LANDED),
        "{row}: the streamed payload"
    );
    assert_eq!(
        recording.requests.len(),
        streams + 1,
        "{row}: requests the peer read"
    );
}

/// What the root scope of one Camber runtime answered about the child a row
/// named, after each of two streaming sends.
struct OwnerAdmissions {
    /// The send of a client with no response bounds.
    refused: Result<u16, RuntimeError>,
    /// Whether the scope had admitted the named child after that send.
    after_refusal: Result<bool, RuntimeError>,
    /// The same reading after a send of the default client.
    after_admission: Result<bool, RuntimeError>,
    /// The payload of the default client's send.
    payload: Vec<u8>,
}

/// Poll two streaming sends under one Camber runtime, and read the root
/// scope's next admission after each.
///
/// The child is named before the first send, so the name binds to the first
/// child either send admits.
fn owner_admissions(row: &str, url: &str) -> Result<OwnerAdmissions, RuntimeError> {
    let controller = runtime_schedule();
    runtime::builder().with_test_schedule(&controller).run(|| {
        let owner = controller.scope_settlement().name_next_admission();
        let unbounded = http::client().unbounded_response();
        let refused: StreamHead = runtime::block_on(stream_get(&unbounded, url));
        let after_refusal = owner.admitted();
        let head: StreamHead = runtime::block_on(stream_get(&http::client(), url));
        let after_admission = owner.admitted();
        let payload = runtime::block_on(read_cleanly(row, head));
        OwnerAdmissions {
            refused: refused.map(|response: ClientStreamResponse| response.status()),
            after_refusal,
            after_admission,
            payload,
        }
    })
}

/// A stream whose budget is refused admits no owner task.
///
/// A Camber runtime of the row's own polls both sends. It runs on a plain
/// thread, because a Tokio thread refuses a nested runtime, and the peer
/// keeps running here. The lookup count of the
/// `BUDGETS` rows cannot see an admission: a task admitted and then dropped
/// starts no lookup. The root scope's own record of the child it admitted
/// can. The second send is the calibration. Its owner takes the name, so the
/// reading that must be false after a refusal is one that can be true.
async fn assert_refused_budget_admits_no_owner() {
    let row = "streaming, cold client, unbounded_response, polled by a Camber runtime";
    let peer = RecordingUpstream::bind(answer("200 OK", &[], LANDED)).await;
    let url = peer.url(PATH);

    let (report, reported) = tokio::sync::oneshot::channel();
    let polling = std::thread::spawn(move || {
        // A closed receiver means the row has already failed.
        let _ = report.send(owner_admissions(row, &url));
    });
    let polled = must_settle(row, "the Camber runtime", reported).await;
    let joined = polling.join();
    let seen = match (polled, joined) {
        (Ok(Ok(seen)), Ok(())) => seen,
        (Ok(Err(error)), _) => panic!("{row}: the runtime did not return cleanly: {error:?}"),
        (_, Err(payload)) => std::panic::resume_unwind(payload),
        (Err(lost), Ok(())) => panic!("{row}: the runtime's thread reported nothing: {lost}"),
    };
    let recording = peer.finish(row).await;

    assert_failed(row, "the streaming send", &seen.refused, STREAM_BUDGET);
    assert!(
        matches!(seen.after_refusal, Ok(false)),
        "{row}: the scope admitted a child for a refused budget: {:?}",
        seen.after_refusal
    );
    assert!(
        matches!(seen.after_admission, Ok(true)),
        "{row}: the scope admitted no owner for an accepted stream: {:?}",
        seen.after_admission
    );
    assert_eq!(seen.payload, LANDED, "{row}: the streamed payload");
    assert_eq!(recording.requests.len(), 1, "{row}: requests the peer read");
}

/// Read one streamed answer of the policy peer to its end and close it.
async fn stream_landed(row: &str, client: &Client, url: &str) {
    let head: StreamHead = sent(row, client, url).await;
    let payload = read_cleanly(row, head).await;
    assert_eq!(payload, LANDED, "{row}: the streamed payload");
}

/// Read one buffered answer of the policy peer.
async fn buffer_landed(row: &str, client: &Client, url: &str) {
    let collected = answered(row, sent_buffered(row, client, url).await);
    assert_eq!(collected.body_bytes(), LANDED, "{row}: the buffered body");
}

/// Send one buffered and then one streamed call, so both profiles of the
/// client are built and each holds an idle connection.
async fn warm_both(row: &str, client: &Client, url: &str) {
    buffer_landed(row, client, url).await;
    stream_landed(row, client, url).await;
}

/// A warm client reuses each profile's own connection, and neither profile
/// takes the other's.
///
/// The buffered call ends before the streamed one starts, so its connection
/// is idle. A shared pool would hand it to the stream. This is also the
/// control of the write rows: without it, a peer that never kept a
/// connection alive would pass them.
async fn assert_warm_profiles_are_reused() {
    let row = "buffered then streaming, warm client, no write";
    let peer = RecordingUpstream::bind(answer("200 OK", &[], LANDED)).await;
    let url = peer.url(PATH);
    let client: Client = http::client();

    warm_both(row, &client, &url).await;
    let built: bool = client_transport_is_built(&client);
    warm_both(row, &client, &url).await;
    let recording = peer.finish(row).await;

    assert!(built, "{row}: two answered calls built no transport");
    assert_eq!(
        *recording.connections(),
        [0, 1, 0, 1],
        "{row}: each profile keeps a pool of its own and reuses it"
    );
}

/// One write to a warm client that must discard both built profiles.
struct TransportWrite {
    row: &'static str,
    write: fn(Client) -> Client,
}

const TRANSPORT_WRITES: [TransportWrite; 4] = [
    TransportWrite {
        row: "buffered then streaming, warm client, connect_timeout written",
        write: |client| client.connect_timeout(WRITTEN_IDLE),
    },
    TransportWrite {
        row: "buffered then streaming, warm client, response_idle_timeout written",
        write: |client| client.response_idle_timeout(WRITTEN_IDLE),
    },
    TransportWrite {
        row: "buffered then streaming, warm client, response_budget written",
        write: |client| client.response_budget(finite_budget()),
    },
    TransportWrite {
        row: "buffered then streaming, warm client, egress_policy written",
        write: |client| client.egress_policy(EgressPolicy::unrestricted()),
    },
];

/// A transport-affecting write discards both built profiles. The next call
/// through each terminal dials a connection of its own.
async fn assert_transport_write_discards_both_profiles(case: &TransportWrite) {
    let row = case.row;
    let peer = RecordingUpstream::bind(answer("200 OK", &[], LANDED)).await;
    let url = peer.url(PATH);
    let warm: Client = http::client();

    warm_both(row, &warm, &url).await;
    let was_built: bool = client_transport_is_built(&warm);
    let written: Client = (case.write)(warm);
    let kept: bool = client_transport_is_built(&written);
    warm_both(row, &written, &url).await;
    let recording = peer.finish(row).await;

    assert!(was_built, "{row}: two answered calls built no transport");
    assert!(!kept, "{row}: the write kept a built profile");
    assert_eq!(
        *recording.connections(),
        [0, 1, 2, 3],
        "{row}: a call after the write was served by a discarded pool"
    );
}

/// A maximum written to a warm client is the one the next call of each
/// terminal is refused by.
async fn assert_written_maximum_is_read() {
    let row = "buffered and streaming, warm client, smaller maximum written";
    let crossing = ClientError::ResponseTooLarge { limit: 4 };
    let peer = RecordingUpstream::bind(answer("200 OK", &[], LANDED)).await;
    let url = peer.url(PATH);
    let warm: Client = http::client();
    let smaller = budget(4, WRITTEN_IDLE, WRITTEN_TOTAL);

    warm_both(row, &warm, &url).await;
    let written: Client = warm.response_budget(smaller);
    let head: StreamHead = sent(row, &written, &url).await;
    let collected = sent_buffered(row, &written, &url).await;
    peer.finish(row).await;

    assert_refused(row, head, crossing);
    assert_buffered_failed(row, collected, crossing);
}

/// A policy written to a warm client refuses the next call of each terminal,
/// and neither reaches the peer its warm pools still hold.
async fn assert_written_policy_is_read() {
    let row = "buffered and streaming, warm client, egress_policy written";
    let address = ClientError::Egress(ClientEgress::Address);
    let peer = RecordingUpstream::bind(answer("200 OK", &[], LANDED)).await;
    let url = peer.url(PATH);
    let warm: Client = http::client();

    warm_both(row, &warm, &url).await;
    let closed: Client = warm.egress_policy(EgressPolicy::public_internet());
    let head: StreamHead = sent(row, &closed, &url).await;
    let collected = sent_buffered(row, &closed, &url).await;
    let recording = peer.finish(row).await;

    assert_refused(row, head, address);
    assert_buffered_failed(row, collected, address);
    assert_eq!(
        recording.requests.len(),
        2,
        "{row}: a refused call reached the peer"
    );
}

/// The demand idle interval of every stream idle cell.
const DEMAND_IDLE: Duration = Duration::from_secs(2);

/// The idle interval a cell writes first and then replaces.
const REPLACED_IDLE: Duration = Duration::from_secs(9);

/// A budget whose idle interval is `idle` and whose other bounds no cell
/// reaches.
fn idle_budget(idle: Duration) -> TransferBudget {
    budget(WRITTEN_MAX, idle, UNREACHED)
}

/// One streaming client whose last idle write is [`DEMAND_IDLE`].
struct IdleCell {
    row: &'static str,
    /// Whether both profiles are built before the writes.
    warm: bool,
    write: fn(Client) -> Client,
}

const IDLE_CELLS: [IdleCell; 4] = [
    IdleCell {
        row: "streaming, cold client, response_idle_timeout written",
        warm: false,
        write: |client| client.response_idle_timeout(DEMAND_IDLE),
    },
    IdleCell {
        row: "streaming, warm client, response_idle_timeout written",
        warm: true,
        write: |client| client.response_idle_timeout(DEMAND_IDLE),
    },
    IdleCell {
        row: "streaming, warm client, response_budget written after response_idle_timeout",
        warm: true,
        write: |client| {
            client
                .response_idle_timeout(REPLACED_IDLE)
                .response_budget(idle_budget(DEMAND_IDLE))
        },
    },
    IdleCell {
        row: "streaming, warm client, response_idle_timeout written after response_budget",
        warm: true,
        write: |client| {
            client
                .response_budget(idle_budget(REPLACED_IDLE))
                .response_idle_timeout(DEMAND_IDLE)
        },
    },
];

/// The client of one idle cell: warmed first when the cell is warm, then
/// written by the cell. No other bound of it is reached.
async fn idle_cell_client(row: &str, case: &IdleCell) -> Client {
    let base: Client = http::client()
        .total_timeout(UNREACHED)
        .connect_timeout(UNREACHED);
    if case.warm {
        let warming = RecordingUpstream::bind(answer("200 OK", &[], LANDED)).await;
        warm_both(row, &base, &warming.url(PATH)).await;
        warming.finish(row).await;
    }
    (case.write)(base).request_timeout(UNREACHED)
}

/// A streamed body's idle interval is the last one written, and it measures a
/// pending demand only.
///
/// The peer sends the head and then nothing. The caller holds the unread
/// response for five idle intervals, which ends nothing and polls nothing.
/// Its first demand then expires one interval after the owner accepted it,
/// as `ResponseIdle`.
async fn assert_stream_idle_is_the_demand_idle(case: &IdleCell) {
    let row = case.row;
    let client: Client = idle_cell_client(row, case).await;

    tokio::time::pause();
    let driver = RunnableDriver::start();
    let mut peer = PacedUpstream::bind_steps(vec![
        Pace::Send(paced_head("200 OK", &[CHUNKED])),
        Pace::UntilClosed,
    ])
    .await;
    let watched = watch_streams(peer.addr());
    let head: StreamHead = sent(row, &client, &peer.url(PATH)).await;
    let mut body: ClientBody = answered(row, head).into_body();
    parked(row, &watched).await;
    tokio::time::advance(CALLER_PAUSE).await;
    settle_wakeups().await;
    let paused: ClientBodyObservation = watched.streamed();

    let (pending_before, expired) = {
        let mut next = std::pin::pin!(body.next());
        let first_pending = placed(row, next.as_mut(), &watched).await;
        let accepted_at = Instant::now();
        advance_to(accepted_at + DEMAND_IDLE - TIMER_ROUNDING).await;
        settle_wakeups().await;
        // A demand that already ended is not polled again: its result is
        // the early one.
        match poll_once(next.as_mut()).await {
            Poll::Ready(early) => (false, early),
            Poll::Pending => {
                let live = first_pending && !watched.streamed().source_released;
                advance_to(accepted_at + DEMAND_IDLE).await;
                tokio::time::advance(TIMER_ROUNDING).await;
                let expired: StreamNext = must_settle(row, "the expired demand", next).await;
                (live, expired)
            }
        }
    };
    let at_expiry: ClientBodyObservation = watched.streamed();
    let closed = close(row, body).await;
    let left = peer.must_see_client_left(row).await;
    tokio::time::resume();
    driver.stop().await;
    let recording = peer.finish(row).await;

    assert!(
        !paused.source_released && paused.source_polls == 0,
        "{row}: the caller's pause ended or polled the body: {paused:?}"
    );
    assert!(
        pending_before,
        "{row}: the demand ended before its idle interval, and returned {expired:?}"
    );
    assert_failed(row, "the demand", &expired, RESPONSE_IDLE);
    assert!(
        at_expiry.source_released,
        "{row}: the expiry returned before the source was released: {at_expiry:?}"
    );
    assert_failed(row, "the close", &closed, RESPONSE_IDLE);
    assert_eq!(left.client_closes, 1, "{row}: the idle transport");
    assert_eq!(recording.requests.len(), 1, "{row}: requests the peer read");
}

/// One buffered client whose last idle write is [`QUIET_IDLE`].
const BUFFERED_IDLE_CELLS: [IdleCell; 2] = [
    IdleCell {
        row: "buffered, cold client, response_idle_timeout written",
        warm: false,
        write: |client| client.response_idle_timeout(QUIET_IDLE),
    },
    IdleCell {
        row: "buffered, warm client, response_idle_timeout written after response_budget",
        warm: true,
        write: |client| {
            client
                .response_budget(idle_budget(REPLACED_IDLE))
                .response_idle_timeout(QUIET_IDLE)
        },
    },
];

/// A buffered body's quiet interval is still the transport's read timer, and
/// its expiry is still `ClientTimeout::Transport`.
async fn assert_buffered_idle_is_the_transport_timer(case: &IdleCell) {
    let row = case.row;
    let client: Client = idle_cell_client(row, case).await;
    let mut upstream = ScriptedUpstream::bind(&[Answer::StallBody]).await;

    let collected = sent_buffered(row, &client, &upstream.url(PATH)).await;
    let reached = upstream
        .saw_each_unordered(&[
            PeerEvent::Started(0),
            PeerEvent::Stalled(0),
            PeerEvent::Released(0),
        ])
        .await;
    let starts = upstream.finish(RunnableDriver::start(), row).await;

    assert_eq!(reached, Ok(()), "{row}: the peer's phases");
    assert_buffered_failed(
        row,
        collected,
        ClientError::Timeout(ClientTimeout::Transport),
    );
    assert_eq!(starts, 1, "{row}: the call started {starts} requests");
}

/// A restricted stream is refused by the same checked decision a buffered
/// call is: a private literal and a mixed answer are refused before either
/// peer is dialed.
async fn assert_restricted_stream_is_refused(
    row: &str,
    restriction: Restriction,
    answers: Option<&[std::net::IpAddr]>,
    origin: &str,
    reason: ClientEgress,
) {
    let peers = Peers::bind();
    let port = peers.port;
    let edge = ClientDnsEdge::new();
    if let Some(addresses) = answers {
        edge.answer(addresses);
    }
    let client: Client = restricted(restriction.policy(row, port), &edge);

    let head: StreamHead = sent(row, &client, &target(origin, port)).await;
    let lookups: usize = edge.lookups();
    let seen = peers.finish(row).await;

    assert_refused(row, head, ClientError::Egress(reason));
    assert_eq!(
        lookups,
        usize::from(answers.is_some()),
        "{row}: lookups the operation started"
    );
    assert_undialed(row, &seen);
}

/// A restricted stream to an excepted name is pinned to the address its one
/// lookup returned, keeps the name as its `Host`, and leaves no transport in
/// the reusable cache.
async fn assert_restricted_stream_is_pinned() {
    let row = "streaming, cold restricted client, excepted name";
    let peers = Peers::bind();
    let port = peers.port;
    let edge = ClientDnsEdge::new();
    edge.answer(&[V4]);
    let client: Client = restricted(excepting(row, FETCH_AT_V4, port), &edge);

    let head: StreamHead = sent(row, &client, &target(FETCH, port)).await;
    let read = drained(row, head).await;
    let lookups: usize = edge.lookups();
    let cached: bool = client_transport_is_built(&client);
    let seen = peers.finish(row).await;

    assert_eq!(read.status, 200, "{row}: final status");
    assert_eq!(read.payload, Family::V4.body(), "{row}: the answering peer");
    assert_clean_end(row, &read.end, &read.closed);
    assert_eq!(lookups, 1, "{row}: lookups the operation started");
    assert!(
        !cached,
        "{row}: a restricted transport was kept in the reusable cache"
    );
    assert_eq!(
        (seen.v4.connections, seen.v6.connections),
        (1, 0),
        "{row}: connections each peer accepted"
    );
    let hosts: Box<[&str]> = seen
        .v4
        .requests
        .iter()
        .flat_map(|request| request.header_values("host"))
        .collect();
    assert_eq!(
        *hosts,
        [at(FETCH_HOST, port).as_str()],
        "{row}: the Host the one request sent"
    );
}

/// A restricted HTTPS stream asks for the requested name and authenticates
/// it, whichever address it is pinned to.
async fn assert_restricted_stream_authenticates_the_name(row: &str, presented: &str) {
    let trusted = presented == NAME;
    let (cert_pem, key_pem) = generate_cert_with_san(presented);
    let mut peer = named_tls_peer(&cert_pem, &key_pem, closing_answer(SECURE_BODY)).await;
    let port = peer.addr().port();
    let edge = ClientDnsEdge::new();
    edge.answer(&[V4]);
    let client: Client = client_test_root(restricted(secure_policy(row, port), &edge), &cert_pem)
        .unwrap_or_else(|error| panic!("{row}: the test root was refused: {error:?}"));

    let head: StreamHead = sent(row, &client, &format!("{}{PATH}", at(SECURE_FETCH, port))).await;
    let delivered = delivered_or_refused(row, trusted, head, ClientError::Tls).await;
    let handshake = within_watchdog(peer.recv()).await;
    peer.finish(row, within_watchdog).await;

    assert_eq!(
        delivered.as_deref(),
        trusted.then_some(SECURE_BODY),
        "{row}: the streamed payload"
    );
    match trusted {
        true => {
            named_request(row, &handshake);
        }
        false => assert_refused_handshake(row, handshake),
    }
}

/// The restricted policies the refusal cells build for the peers' port.
#[derive(Clone, Copy)]
enum Restriction {
    /// The public internet, with no exception.
    PublicOnly,
    /// The public internet, and the fetch name at its IPv4 peer.
    NameAtV4,
    /// As [`Restriction::NameAtV4`], over HTTPS only.
    HttpsOnly,
}

impl Restriction {
    /// The policy of this cell for the peers' port.
    fn policy(self, row: &str, port: u16) -> EgressPolicy {
        match self {
            Self::PublicOnly => EgressPolicy::public_internet(),
            Self::NameAtV4 => excepting(row, FETCH_AT_V4, port),
            Self::HttpsOnly => excepting(row, FETCH_AT_V4, port).https_only(),
        }
    }
}

/// The HTTP target only the mock cells name, and what its mock answers.
const MOCKED: &str = "http://mocked.streaming.test/answer";
const MOCKED_BODY: &str = "mocked-body";
const MOCK_HEADER: (&str, &str) = ("x-mock", "kept");

/// A mocked call through the streaming terminal returns a head and a body
/// under the same admission a real one passes: the stream budget, the
/// declared size, and the scheme are all read before the mock answers.
///
/// A mock resolves nothing and dials nothing. No cell here is transport
/// proof.
async fn assert_mock_streams_under_the_same_admission() {
    let row = "streaming, cold client, mocked answer";
    let canned = Response::text(200, MOCKED_BODY)
        .unwrap_or_else(|error| panic!("{row}: the mock answer was refused: {error:?}"))
        .with_header(MOCK_HEADER.0, MOCK_HEADER.1);
    let registered = mock::http(MOCKED).returns(canned);
    let edge = ClientDnsEdge::new();
    let open: Client = restricted(EgressPolicy::public_internet(), &edge);
    let secure: Client = restricted(EgressPolicy::public_internet().https_only(), &edge);
    let unbounded: Client = restricted(EgressPolicy::public_internet(), &edge).unbounded_response();

    let scheme: StreamHead =
        must_settle(row, "the https-only send", stream_get(&secure, MOCKED)).await;
    let budget: StreamHead =
        must_settle(row, "the unbounded send", stream_get(&unbounded, MOCKED)).await;
    let head: StreamHead = sent(row, &open, MOCKED).await;
    let response: ClientStreamResponse = answered(row, head);
    let header: Option<Box<[u8]>> = response.header(MOCK_HEADER.0).map(Box::from);
    let read = read_to_end(row, response).await.closed(row).await;
    registered.assert_called_once();
    let crossed: StreamHead = must_settle(
        row,
        "the limited send",
        stream_limited(&open, "GET", MOCKED, MOCKED_BODY.len() - 1),
    )
    .await;
    let lookups: usize = edge.lookups();
    drop(registered);

    assert_refused(
        "streaming, https-only client, mocked HTTP target",
        scheme,
        ClientError::Egress(ClientEgress::Scheme),
    );
    assert_refused(
        "streaming, unbounded client, mocked answer",
        budget,
        STREAM_BUDGET,
    );
    assert_eq!(read.status, 200, "{row}: final status");
    assert_eq!(
        header.as_deref(),
        Some(MOCK_HEADER.1.as_bytes()),
        "{row}: the mock's header"
    );
    assert_eq!(
        read.payload,
        MOCKED_BODY.as_bytes(),
        "{row}: the mock's body"
    );
    assert_clean_end(row, &read.end, &read.closed);
    assert_refused(
        "streaming, cold client, mocked answer over the request limit",
        crossed,
        ClientError::ResponseTooLarge {
            limit: MOCKED_BODY.len() - 1,
        },
    );
    assert_eq!(lookups, 0, "{row}: a mocked call was resolved");
}

/// 5.T2
#[tokio::test]
async fn streaming_transport_profiles_do_not_share_policy() {
    for case in &BUDGETS {
        assert_stream_budget_admission(case).await;
    }
    assert_refused_budget_admits_no_owner().await;
    assert_warm_profiles_are_reused().await;
    for case in &TRANSPORT_WRITES {
        assert_transport_write_discards_both_profiles(case).await;
    }
    assert_written_maximum_is_read().await;
    assert_written_policy_is_read().await;
    for case in &BUFFERED_IDLE_CELLS {
        assert_buffered_idle_is_the_transport_timer(case).await;
    }
    assert_restricted_stream_is_refused(
        "streaming, cold restricted client, private literal",
        Restriction::PublicOnly,
        None,
        LITERAL_V4,
        ClientEgress::Address,
    )
    .await;
    assert_restricted_stream_is_refused(
        "streaming, cold restricted client, mixed answer",
        Restriction::NameAtV4,
        Some(&[V4, V6]),
        FETCH,
        ClientEgress::Address,
    )
    .await;
    assert_restricted_stream_is_refused(
        "streaming, cold https-only client, HTTP target",
        Restriction::HttpsOnly,
        None,
        FETCH,
        ClientEgress::Scheme,
    )
    .await;
    assert_restricted_stream_is_pinned().await;
    assert_restricted_stream_authenticates_the_name(
        "streaming, cold restricted client, trusted certificate for the requested name",
        NAME,
    )
    .await;
    assert_restricted_stream_authenticates_the_name(
        "streaming, cold restricted client, trusted certificate for the pinned address",
        "127.0.0.1",
    )
    .await;
    assert_mock_streams_under_the_same_admission().await;
    // Last, because these are the cells that hold the clock still.
    for case in &IDLE_CELLS {
        assert_stream_idle_is_the_demand_idle(case).await;
    }
}
