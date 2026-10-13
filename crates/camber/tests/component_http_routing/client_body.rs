//! Streaming 4.T1 and 4.T2: the body owner of one outbound stream, driven
//! through the real `ClientBody` over a scripted source.
//!
//! This is component proof of the body engine only. No row sends a request,
//! resolves a name, or runs under a Camber runtime. Real framing, transport
//! confinement, and captured-runtime ownership belong to the streaming rows
//! of Step 5.
//!
//! # The driver these rows use
//!
//! The driver is doc-hidden and semver-unsupported, under `camber::__private`.
//! It supplies source frames and reads counts. It cannot choose a terminal
//! result.
//!
//! - `ClientBodyBounds { limit, counted, idle, total, attempt }`: the byte
//!   maximum, the running total the count starts at, the demand idle interval,
//!   the operation total, and the optional attempt bound.
//! - `ClientBodyDriver::begin(ClientBodyBounds) -> ClientBodyDriver`: fix the
//!   operation deadline and the attempt expiry now, as an operation's first
//!   poll and its final send do.
//! - `hand_off(&self) -> Option<ClientBody>`: start one bare-Tokio task that
//!   runs the production engine over the scripted source, and return the real
//!   handle. `None` after the first call.
//! - `feed(&self, ClientBodyFrame)`: queue the next source frame. A source
//!   polled with nothing queued stays pending. A released source drops every
//!   frame still queued, and every frame fed afterwards.
//! - `abort_owner(&self)`: abort the owner task. It names no result.
//! - `observed(&self) -> ClientBodyObservation`: a copy of the counts.
//!
//! `ClientBodyFrame` is `Data(Bytes)`, `Trailers`, `Failure`, or `End`.
//! `Failure` is a failed source read. The engine alone names its category.
//!
//! `ClientBodyObservation` fields:
//!
//! | Field | Meaning |
//! |---|---|
//! | `source_polls` | Every poll of the source, pending ones included |
//! | `source_frames` | Frames the source returned, a failure and the end included |
//! | `admitted_chunks`, `admitted_bytes` | Nonempty chunks that passed the byte check |
//! | `retained_chunks`, `retained_bytes` | Admitted chunks the caller has not taken |
//! | `peak_retained_bytes` | The most `retained_bytes` has been |
//! | `pending_demand` | The owner accepted a demand and armed its idle expiry. False again once the demand is answered |
//! | `awaiting_demand` | The owner is parked with no demand, under its carried expiries |
//! | `source_released` | The source is dropped, by any path |
//! | `owner_ended` | The owner task's future is dropped |
//!
//! # Clocks
//!
//! Both tests run on a paused current-thread runtime. A row never parks: every
//! wait is a yield loop under the real-time fixture watchdog, so the clock
//! moves only where a row advances it. A row advances only after the owner
//! acknowledged the wait the step is measured from.
//!
//! # Clause → row → the mutation that turns the row red
//!
//! | Clause | Row | Mutation |
//! |---|---|---|
//! | The counts are true | `assert_observations_are_calibrated` | falsify an observation count |
//! | No poll before demand, one chunk per demand | `assert_one_chunk_per_demand` | prefetch a frame |
//! | Storage is passed, not copied | `assert_one_chunk_per_demand` | copy the payload |
//! | One undelivered chunk, no poll behind it | `assert_undelivered_chunk_stops_the_source` | poll while a chunk waits |
//! | Empty frames and trailers deliver nothing | `assert_framing_delivers_no_bytes` | return an empty chunk |
//! | The exact limit ends cleanly | `assert_exact_limit_ends_cleanly` | refuse at the limit |
//! | The crossing chunk is withheld whole | `assert_crossing_chunk_is_withheld` | count after delivery |
//! | An overflowing total is refused | `assert_overflow_is_refused` | wrap the arithmetic |
//! | A failed read is not the end | `assert_read_failure_is_not_the_end` | return EOF on error |
//! | Retained payload is flat | `assert_retention_is_flat` | queue read-ahead chunks |
//! | A quiet demand expires | `assert_quiet_demand_expires` | reset idle on an empty frame |
//! | A caller pause is not idle | `assert_caller_pause_is_not_idle` | time idle from the last chunk |
//! | Carried expiries need no demand | `assert_carried_expiry_needs_no_demand` | omit the no-demand timer, or reset the handoff clock |
//! | Close acknowledges release | `assert_close_acknowledges_release` | answer the close on the committed fact, with no join |
//! | An abandoned demand is cancellation | `assert_abandoned_demand_is_cancellation` | drop the cancellation guard |
//! | An abandoned chunk is not handed on | `assert_abandonment_answers_the_next_demand` | keep the waiting chunk at the abandonment |
//! | A terminal returns after release | `assert_abandonment_answers_the_next_demand` | answer a demand on the committed fact |
//! | Drop releases without a caller poll | `assert_drop_releases_unpolled` | detach the owner task |
//! | Owner loss is a transport failure | `assert_owner_loss_is_transport` | report EOF for a lost owner |
//! | A committed cause is kept | `assert_committed_cause_survives_owner_loss`, `assert_sticky` | overwrite the cause at the settlement |
//! | The owner keeps the caller's cause | `assert_abandonment_survives_owner_expiry` | overwrite the cause at the owner's commit |
//!
//! # What these rows do not prove
//!
//! The owner drops its source and then acknowledges, in one turn of its own
//! task. Both tests run on one thread, so no row can stand between the two
//! drops, and an owner that acknowledged first would pass them all. That
//! order is a race guard. Its proof is the multithread rows of Step 5's
//! `streaming_response_drop_and_close_settle_on_bare_tokio` (5.T4), whose
//! mutations name it.
//!
//! The close row turns red only when the settled gate and the owner join are
//! both removed. Each alone holds the close until release.

use crate::retry_upstream::{
    StreamNext, TIMER_ROUNDING, advance_to, counts_reached, delivered_chunk, must_settle,
    poll_once, settle_wakeups,
};

use bytes::Bytes;
use camber::__private::ClientBodyBounds;
use camber::__private::ClientBodyDriver;
use camber::__private::ClientBodyFrame;
use camber::__private::ClientBodyObservation;
use camber::RuntimeError;
use camber::http::ClientBody;
use camber::http::ClientError;
use camber::http::ClientTimeout;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::Poll;
use std::time::Duration;
use tokio::time::Instant;

/// The byte maximum of every row that does not place its own.
const LIMIT: usize = 64 * 1024;

/// The demand idle interval of every row.
const IDLE: Duration = Duration::from_secs(2);

/// The operation total of every row that does not expire it.
const TOTAL: Duration = Duration::from_secs(60);

/// The bounds of a row that crosses none of them.
fn bounds() -> ClientBodyBounds {
    ClientBodyBounds {
        limit: LIMIT,
        counted: 0,
        idle: IDLE,
        total: TOTAL,
        attempt: None,
    }
}

/// The default bounds under another byte maximum.
fn limited(limit: usize) -> ClientBodyBounds {
    ClientBodyBounds { limit, ..bounds() }
}

/// How a demand or a close must end.
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// A clean end of the body.
    End,
    /// This client category.
    Client(ClientError),
    /// The caller's own cancellation.
    Cancelled,
}

/// The crossing of `limit`.
const fn too_large(limit: usize) -> Ending {
    Ending::Client(ClientError::ResponseTooLarge { limit })
}

const TRANSPORT: Ending = Ending::Client(ClientError::Transport);
const RESPONSE_IDLE: Ending = Ending::Client(ClientError::Timeout(ClientTimeout::ResponseIdle));
const ATTEMPT: Ending = Ending::Client(ClientError::Timeout(ClientTimeout::Attempt));
const OPERATION: Ending = Ending::Client(ClientError::Timeout(ClientTimeout::Operation));

impl Ending {
    /// Whether `error` is this failure.
    fn failed(self, error: &RuntimeError) -> bool {
        match (self, error) {
            (Self::Cancelled, RuntimeError::Cancelled) => true,
            (Self::Client(expected), RuntimeError::Client(found)) => expected == *found,
            _ => false,
        }
    }

    /// Whether a demand ended this way.
    fn ended(self, result: &StreamNext) -> bool {
        match (self, result) {
            (Self::End, Ok(None)) => true,
            (_, Err(error)) => self.failed(error),
            _ => false,
        }
    }
}

/// Chunk storage that counts its own release.
struct Witnessed {
    payload: Vec<u8>,
    released: Arc<AtomicUsize>,
}

impl AsRef<[u8]> for Witnessed {
    fn as_ref(&self) -> &[u8] {
        &self.payload
    }
}

impl Drop for Witnessed {
    fn drop(&mut self) {
        self.released.fetch_add(1, Ordering::SeqCst);
    }
}

/// A chunk of `len` bytes, and the count of times its storage was released.
///
/// The row keeps no clone, so the count reaches one when the last holder of
/// the chunk drops it.
fn witnessed(len: usize, fill: u8) -> (Bytes, Arc<AtomicUsize>) {
    let released = Arc::new(AtomicUsize::new(0));
    let storage = Witnessed {
        payload: vec![fill; len],
        released: Arc::clone(&released),
    };
    (Bytes::from_owner(storage), released)
}

/// A chunk of `len` bytes.
fn plain(len: usize, fill: u8) -> Bytes {
    Bytes::from(vec![fill; len])
}

/// How many times a witnessed chunk's storage was released.
fn releases(witness: &Arc<AtomicUsize>) -> usize {
    witness.load(Ordering::SeqCst)
}

/// Poll `future` to its output, yielding between polls.
///
/// The loop is always runnable, so the paused clock never advances itself.
async fn turned<F: Future>(mut future: Pin<&mut F>) -> F::Output {
    loop {
        if let Poll::Ready(output) = poll_once(future.as_mut()).await {
            return output;
        }
        tokio::task::yield_now().await;
    }
}

/// The output of `future`, or a panic once the fixture watchdog expires.
async fn resolved<F: Future>(future: F, context: &str, what: &str) -> F::Output {
    let mut future = std::pin::pin!(future);
    must_settle(context, what, turned(future.as_mut())).await
}

/// Yield until the owner's counts satisfy `ready`, and return them.
///
/// This is the acknowledgement a row waits for before it moves the clock. It
/// observes. It moves no clock and commits nothing.
async fn reached(
    driver: &ClientBodyDriver,
    context: &str,
    what: &str,
    ready: impl Fn(&ClientBodyObservation) -> bool,
) -> ClientBodyObservation {
    counts_reached(|| driver.observed(), ready)
        .await
        .unwrap_or_else(|seen| panic!("{context}: never observed that {what}: {seen:?}"))
}

/// Yield until the owner task has ended, and return its counts.
async fn owner_ended(driver: &ClientBodyDriver, context: &str) -> ClientBodyObservation {
    reached(driver, context, "the owner task ended", |seen| {
        seen.owner_ended
    })
    .await
}

/// Hand the source to the engine, and wait until the owner is parked with no
/// demand.
async fn handed_off(driver: &ClientBodyDriver, context: &str) -> ClientBody {
    let body: ClientBody = driver
        .hand_off()
        .unwrap_or_else(|| panic!("{context}: the source was already handed off"));
    reached(driver, context, "the owner parked with no demand", |seen| {
        seen.awaiting_demand
    })
    .await;
    body
}

/// Fix the bounds now and hand the source off at once.
async fn started(bounds: ClientBodyBounds, context: &str) -> (ClientBodyDriver, ClientBody) {
    let driver: ClientBodyDriver = ClientBodyDriver::begin(bounds);
    let body: ClientBody = handed_off(&driver, context).await;
    (driver, body)
}

/// Queue `chunks`, in order.
fn feed_data(driver: &ClientBodyDriver, chunks: impl IntoIterator<Item = Bytes>) {
    for chunk in chunks {
        driver.feed(ClientBodyFrame::Data(chunk));
    }
}

/// One demand, awaited to its result.
async fn demand(body: &mut ClientBody, context: &str) -> StreamNext {
    resolved(body.next(), context, "the demand").await
}

/// One demand that must deliver a chunk.
async fn delivered(body: &mut ClientBody, context: &str) -> Bytes {
    delivered_chunk(context, demand(body, context).await)
}

/// Poll the demand `next` once, and require it still pending.
///
/// The caller pins the demand, so it can await or drop it afterwards.
async fn park<F>(next: Pin<&mut F>, context: &str)
where
    F: Future<Output = StreamNext>,
{
    let first: Poll<StreamNext> = poll_once(next).await;
    assert!(
        first.is_pending(),
        "{context}: a demand was answered before its owner accepted it: {first:?}"
    );
}

/// Yield until the owner accepted the parked demand, and return its counts.
async fn demand_accepted(driver: &ClientBodyDriver, context: &str) -> ClientBodyObservation {
    reached(driver, context, "the demand was accepted", |seen| {
        seen.pending_demand
    })
    .await
}

/// Yield until one admitted chunk waits for its caller, and return the counts.
async fn chunk_waiting(driver: &ClientBodyDriver, context: &str) -> ClientBodyObservation {
    reached(driver, context, "a chunk waited for its caller", |seen| {
        seen.retained_chunks == 1
    })
    .await
}

/// Require a demand that ended as `ending`, with the source already released.
fn assert_terminal(seen: &ClientBodyObservation, next: &StreamNext, ending: Ending, context: &str) {
    assert!(
        ending.ended(next),
        "{context}: returned {next:?}, not {ending:?}"
    );
    assert!(
        seen.source_released,
        "{context}: the terminal returned before the source was released: {seen:?}"
    );
}

/// Require a demand to end as `ending`, with the source already released.
///
/// An awaited end or failure acknowledges release, so the count is read at
/// the return and not after another turn of the owner.
async fn assert_ends(
    driver: &ClientBodyDriver,
    body: &mut ClientBody,
    ending: Ending,
    context: &str,
) {
    let next: StreamNext = demand(body, context).await;
    let seen: ClientBodyObservation = driver.observed();
    assert_terminal(&seen, &next, ending, context);
    assert_eq!(
        seen.retained_chunks, 0,
        "{context}: a chunk outlived the terminal"
    );
}

/// Require `ending` on every later demand, with no further source poll.
///
/// A frame is fed first. A released source drops it, so a late reply has
/// nothing to overwrite the committed fact with.
async fn assert_sticky(
    driver: &ClientBodyDriver,
    body: &mut ClientBody,
    ending: Ending,
    context: &str,
) {
    let before: ClientBodyObservation = driver.observed();
    let (late, late_released) = witnessed(3, b'z');
    driver.feed(ClientBodyFrame::Data(late));
    for repeat in 0..2 {
        let again: StreamNext = demand(body, context).await;
        assert!(
            ending.ended(&again),
            "{context}: repeat {repeat} returned {again:?}, not {ending:?}"
        );
    }
    settle_wakeups().await;
    let after: ClientBodyObservation = driver.observed();
    assert_eq!(
        after.source_polls, before.source_polls,
        "{context}: the source was polled after its terminal"
    );
    assert_eq!(
        after.admitted_chunks, before.admitted_chunks,
        "{context}: a chunk was admitted after the terminal"
    );
    assert_eq!(
        releases(&late_released),
        1,
        "{context}: a frame fed after release was kept"
    );
}

/// Close the body, join the owner, and require the close to end as `failure`
/// or, for `None`, to succeed.
///
/// The counts are read at the return, because a completed close acknowledges
/// release.
async fn finish(
    driver: &ClientBodyDriver,
    body: ClientBody,
    failure: Option<Ending>,
    context: &str,
) {
    let closed: Result<(), RuntimeError> = resolved(body.close(), context, "the close").await;
    let seen: ClientBodyObservation = driver.observed();
    let joined = owner_ended(driver, context).await;
    match (failure, &closed) {
        (None, Ok(())) => {}
        (Some(expected), Err(error)) if expected.failed(error) => {}
        _ => panic!("{context}: the close returned {closed:?}, not {failure:?}"),
    }
    assert!(
        seen.source_released,
        "{context}: the close returned before the source was released: {seen:?}"
    );
    assert_eq!(
        joined.retained_chunks, 0,
        "{context}: a chunk outlived its owner"
    );
}

/// Drive one known frame and one release, and require every count to follow.
///
/// Every later row reads these counts, so a count that never moved would
/// pass them all.
async fn assert_observations_are_calibrated() {
    let context = "calibration";
    let (driver, mut body) = started(bounds(), context).await;
    let parked: ClientBodyObservation = driver.observed();
    let (chunk, chunk_released) = witnessed(7, b'c');
    let (queued, queued_released) = witnessed(5, b'q');
    driver.feed(ClientBodyFrame::Data(chunk));
    driver.feed(ClientBodyFrame::Data(queued));

    let taken: Bytes = delivered(&mut body, context).await;
    let after_delivery: ClientBodyObservation = driver.observed();
    let held = releases(&chunk_released);
    drop(taken);
    let dropped = releases(&chunk_released);
    finish(&driver, body, None, context).await;

    assert_eq!(
        (parked.source_polls, parked.source_frames),
        (0, 0),
        "{context}: a parked owner polled its source: {parked:?}"
    );
    assert!(
        !parked.source_released && !parked.owner_ended && !parked.pending_demand,
        "{context}: a parked owner is neither released nor waiting on a frame: {parked:?}"
    );
    assert_eq!(
        (after_delivery.source_frames, after_delivery.admitted_chunks),
        (1, 1),
        "{context}: one frame was read and admitted: {after_delivery:?}"
    );
    assert_eq!(
        (
            after_delivery.admitted_bytes,
            after_delivery.peak_retained_bytes
        ),
        (7, 7),
        "{context}: the frame's seven bytes were counted: {after_delivery:?}"
    );
    assert!(
        after_delivery.source_polls >= 1 && after_delivery.retained_chunks == 0,
        "{context}: the taken chunk is no longer retained: {after_delivery:?}"
    );
    assert_eq!(
        (held, dropped),
        (0, 1),
        "{context}: the caller alone held the chunk it was given"
    );
    assert_eq!(
        releases(&queued_released),
        1,
        "{context}: the released source kept a queued frame"
    );
}

/// The frames of the demand rows: three chunks, then the end.
const DEMAND_SIZES: [usize; 3] = [3, 11, 1];

/// Nothing is polled before a demand. Each demand reads one frame and
/// delivers the storage the source yielded. The end is sticky.
async fn assert_one_chunk_per_demand() {
    let context = "one chunk per demand";
    let driver: ClientBodyDriver = ClientBodyDriver::begin(bounds());
    let sent: Box<[Bytes]> = DEMAND_SIZES.iter().map(|len| plain(*len, b'd')).collect();
    feed_data(&driver, sent.iter().cloned());
    driver.feed(ClientBodyFrame::End);
    let mut body: ClientBody = handed_off(&driver, context).await;
    settle_wakeups().await;
    let unasked: ClientBodyObservation = driver.observed();
    assert_eq!(
        (unasked.source_polls, unasked.admitted_chunks),
        (0, 0),
        "{context}: the source was polled before any demand: {unasked:?}"
    );

    for (index, expected) in sent.iter().enumerate() {
        let chunk: Bytes = delivered(&mut body, context).await;
        settle_wakeups().await;
        let seen: ClientBodyObservation = driver.observed();
        assert_eq!(
            chunk.as_ptr(),
            expected.as_ptr(),
            "{context}: chunk {index} is a copy of the source's storage"
        );
        assert_eq!(&chunk, expected, "{context}: chunk {index} bytes");
        assert_eq!(
            (
                seen.source_frames,
                seen.admitted_chunks,
                seen.retained_chunks
            ),
            (index + 1, index + 1, 0),
            "{context}: demand {index} read more than its own frame: {seen:?}"
        );
    }
    assert_ends(&driver, &mut body, Ending::End, context).await;
    assert_sticky(&driver, &mut body, Ending::End, context).await;
    finish(&driver, body, None, context).await;
}

/// Park a demand, let the owner admit its chunk, and return the counts read
/// while the caller has not taken it, and again after the caller did.
async fn undelivered_then_taken(
    driver: &ClientBodyDriver,
    body: &mut ClientBody,
    context: &str,
) -> (ClientBodyObservation, StreamNext, ClientBodyObservation) {
    let mut next = std::pin::pin!(body.next());
    park(next.as_mut(), context).await;
    chunk_waiting(driver, context).await;
    settle_wakeups().await;
    let waiting: ClientBodyObservation = driver.observed();
    let taken: StreamNext = resolved(next, context, "the waiting chunk").await;
    settle_wakeups().await;
    (waiting, taken, driver.observed())
}

/// One admitted chunk waits for its caller, and the owner reads nothing
/// behind it.
async fn assert_undelivered_chunk_stops_the_source() {
    let context = "undelivered chunk";
    let (driver, mut body) = started(bounds(), context).await;
    let (first, first_released) = witnessed(9, b'u');
    driver.feed(ClientBodyFrame::Data(first));
    feed_data(&driver, [plain(4, b'v'), plain(4, b'w')]);

    let (waiting, taken, after) = undelivered_then_taken(&driver, &mut body, context).await;
    finish(&driver, body, None, context).await;

    assert_eq!(
        (waiting.retained_chunks, waiting.retained_bytes),
        (1, 9),
        "{context}: one chunk of nine bytes waited: {waiting:?}"
    );
    assert_eq!(
        waiting.source_frames, 1,
        "{context}: the owner read behind an undelivered chunk: {waiting:?}"
    );
    assert!(
        matches!(&taken, Ok(Some(chunk)) if chunk.len() == 9),
        "{context}: the waiting chunk was not delivered: {taken:?}"
    );
    assert_eq!(
        (after.retained_chunks, after.source_frames),
        (0, 1),
        "{context}: the owner read on with no new demand: {after:?}"
    );
    drop(taken);
    assert_eq!(
        releases(&first_released),
        1,
        "{context}: the engine kept the chunk it delivered"
    );
}

/// Empty data frames and trailers are framing. One demand reads past them to
/// one nonempty chunk, and none of them is counted as payload.
async fn assert_framing_delivers_no_bytes() {
    let context = "empty frames and trailers";
    let (driver, mut body) = started(bounds(), context).await;
    let payload = plain(6, b'f');
    feed_data(&driver, [Bytes::new(), Bytes::new(), payload.clone()]);
    driver.feed(ClientBodyFrame::Trailers);
    driver.feed(ClientBodyFrame::End);

    let chunk: Bytes = delivered(&mut body, context).await;
    let after_chunk: ClientBodyObservation = driver.observed();
    assert_ends(&driver, &mut body, Ending::End, context).await;
    let after_end: ClientBodyObservation = driver.observed();
    finish(&driver, body, None, context).await;

    assert_eq!(chunk, payload, "{context}: the nonempty chunk");
    assert_eq!(
        (after_chunk.source_frames, after_chunk.admitted_chunks),
        (3, 1),
        "{context}: one demand read two empty frames and one chunk: {after_chunk:?}"
    );
    assert_eq!(
        (
            after_end.source_frames,
            after_end.admitted_chunks,
            after_end.admitted_bytes
        ),
        (5, 1, 6),
        "{context}: trailers and the end were counted as payload: {after_end:?}"
    );
}

/// Content that reaches the limit exactly is all delivered, and succeeds only
/// at the clean end behind it.
async fn assert_exact_limit_ends_cleanly() {
    let context = "exact limit";
    let (driver, mut body) = started(limited(8), context).await;
    feed_data(&driver, [plain(5, b'e'), plain(3, b'e')]);
    driver.feed(ClientBodyFrame::End);

    let lengths = [
        delivered(&mut body, context).await.len(),
        delivered(&mut body, context).await.len(),
    ];
    let at_limit: ClientBodyObservation = driver.observed();
    assert_ends(&driver, &mut body, Ending::End, context).await;
    finish(&driver, body, None, context).await;

    assert_eq!(lengths, [5, 3], "{context}: both chunks were delivered");
    assert_eq!(
        (at_limit.admitted_bytes, at_limit.source_released),
        (8, false),
        "{context}: the limit itself ended the body: {at_limit:?}"
    );
}

/// The chunk that crosses the limit is never delivered, in whole or in part,
/// and the source ends with it.
async fn assert_crossing_chunk_is_withheld() {
    let context = "crossing chunk";
    let ending = too_large(8);
    let (driver, mut body) = started(limited(8), context).await;
    let (crossing, crossing_released) = witnessed(4, b'x');
    let (later, later_released) = witnessed(1, b'l');
    driver.feed(ClientBodyFrame::Data(plain(5, b'p')));
    driver.feed(ClientBodyFrame::Data(crossing));
    driver.feed(ClientBodyFrame::Data(later));

    let prefix: Bytes = delivered(&mut body, context).await;
    assert_ends(&driver, &mut body, ending, context).await;
    let refused: ClientBodyObservation = driver.observed();
    let dropped = (releases(&crossing_released), releases(&later_released));
    assert_sticky(&driver, &mut body, ending, context).await;
    finish(&driver, body, Some(ending), context).await;

    assert_eq!(prefix.len(), 5, "{context}: the delivered prefix");
    assert_eq!(
        (refused.admitted_chunks, refused.admitted_bytes),
        (1, 5),
        "{context}: the crossing chunk was admitted: {refused:?}"
    );
    assert_eq!(
        dropped,
        (1, 1),
        "{context}: the crossing chunk and the frame behind it were released"
    );
}

/// A running total that would overflow is refused as a crossing, and the
/// total that reaches the largest limit exactly is not.
async fn assert_overflow_is_refused() {
    let context = "overflowing total";
    let ending = too_large(usize::MAX);
    let near = ClientBodyBounds {
        limit: usize::MAX,
        counted: usize::MAX - 2,
        ..bounds()
    };
    let (driver, mut body) = started(near, context).await;
    let (overflowing, overflowing_released) = witnessed(1, b'o');
    driver.feed(ClientBodyFrame::Data(plain(2, b'n')));
    driver.feed(ClientBodyFrame::Data(overflowing));

    let exact: Bytes = delivered(&mut body, context).await;
    assert_ends(&driver, &mut body, ending, context).await;
    let dropped = releases(&overflowing_released);
    assert_sticky(&driver, &mut body, ending, context).await;
    finish(&driver, body, Some(ending), context).await;

    assert_eq!(
        exact.len(),
        2,
        "{context}: the chunk that reached the limit"
    );
    assert_eq!(dropped, 1, "{context}: the overflowing chunk was released");
}

/// A failed read is a transport failure, before any data and after a
/// delivered prefix. It is never a clean end.
///
/// A scripted failure stands for a failed read and for a truncated body: the
/// transport reports both as a failed read. Real truncation at a declared
/// length is proved against a real peer in Step 5.
async fn assert_read_failure_is_not_the_end(prefix: &[usize], context: &str) {
    let (driver, mut body) = started(bounds(), context).await;
    feed_data(&driver, prefix.iter().map(|len| plain(*len, b't')));
    driver.feed(ClientBodyFrame::Failure);
    driver.feed(ClientBodyFrame::End);

    let mut lengths = Vec::with_capacity(prefix.len());
    for _ in prefix {
        lengths.push(delivered(&mut body, context).await.len());
    }
    assert_ends(&driver, &mut body, TRANSPORT, context).await;
    let failed: ClientBodyObservation = driver.observed();
    assert_sticky(&driver, &mut body, TRANSPORT, context).await;
    finish(&driver, body, Some(TRANSPORT), context).await;

    assert_eq!(lengths, prefix, "{context}: the delivered prefix");
    assert_eq!(
        failed.source_frames,
        prefix.len() + 1,
        "{context}: the source was read past its failure: {failed:?}"
    );
}

/// The fixed chunk size of the retention rows.
pub(crate) const FIXED_CHUNK: usize = 1024;

/// The chunk counts of the short and the long retention rows.
pub(crate) const SHORT_BODY: usize = 4;
pub(crate) const LONG_BODY: usize = 64;

/// Read a body of `chunks` fixed chunks, every frame queued before the first
/// demand. Return the peak retained payload, and the frames the source had
/// yielded after each delivery.
async fn retention(chunks: usize, context: &str) -> (usize, Vec<usize>) {
    let driver: ClientBodyDriver = ClientBodyDriver::begin(limited(FIXED_CHUNK * LONG_BODY));
    for _ in 0..chunks {
        driver.feed(ClientBodyFrame::Data(plain(FIXED_CHUNK, b'r')));
    }
    driver.feed(ClientBodyFrame::End);
    let mut body: ClientBody = handed_off(&driver, context).await;

    let mut progression = Vec::with_capacity(chunks);
    for _ in 0..chunks {
        let chunk: Bytes = delivered(&mut body, context).await;
        assert_eq!(chunk.len(), FIXED_CHUNK, "{context}: a fixed chunk");
        settle_wakeups().await;
        progression.push(driver.observed().source_frames);
    }
    assert_ends(&driver, &mut body, Ending::End, context).await;
    let peak = driver.observed().peak_retained_bytes;
    finish(&driver, body, None, context).await;
    (peak, progression)
}

/// At one chunk size and one limit, a long body retains no more than a short
/// one, and each is read one frame per delivery.
async fn assert_retention_is_flat() {
    let (short_peak, short_frames) = retention(SHORT_BODY, "short body").await;
    let (long_peak, long_frames) = retention(LONG_BODY, "long body").await;

    assert_eq!(
        (short_peak, long_peak),
        (FIXED_CHUNK, FIXED_CHUNK),
        "retained payload grew with the response length"
    );
    assert_eq!(
        short_frames,
        (1..=SHORT_BODY).collect::<Vec<usize>>(),
        "the short body was read ahead of its demands"
    );
    assert_eq!(
        long_frames,
        (1..=LONG_BODY).collect::<Vec<usize>>(),
        "the long body was read ahead of its demands"
    );
}

/// 4.T1
#[tokio::test(start_paused = true)]
async fn client_body_demand_and_bytes_are_bounded() {
    assert_observations_are_calibrated().await;
    assert_one_chunk_per_demand().await;
    assert_undelivered_chunk_stops_the_source().await;
    assert_framing_delivers_no_bytes().await;
    assert_exact_limit_ends_cleanly().await;
    assert_crossing_chunk_is_withheld().await;
    assert_overflow_is_refused().await;
    assert_read_failure_is_not_the_end(&[], "failed read before any data").await;
    assert_read_failure_is_not_the_end(&[5, 2], "failed read after a prefix").await;
    assert_retention_is_flat().await;
}

/// Park one demand on a quiet source and step to its idle expiry.
///
/// The steps are measured from the instant the owner acknowledged the demand.
/// `quiet` is fed half-way and must be read before the clock moves again, so
/// a frame that reset the interval would leave the demand pending at the
/// expiry. Returns whether the demand was still pending one tick short, and
/// what it returned at the expiry.
///
/// A demand answered one tick short is returned as it was. It is not polled
/// again.
async fn expired_demand(
    driver: &ClientBodyDriver,
    body: &mut ClientBody,
    quiet: Vec<ClientBodyFrame>,
    context: &str,
) -> (bool, StreamNext) {
    let mut next = std::pin::pin!(body.next());
    park(next.as_mut(), context).await;
    let accepted_counts = demand_accepted(driver, context).await;
    let accepted = Instant::now();
    let read_through = accepted_counts.source_frames + quiet.len();

    advance_to(accepted + IDLE / 2).await;
    for frame in quiet {
        driver.feed(frame);
    }
    reached(driver, context, "the quiet frames were read", |seen| {
        seen.source_frames == read_through
    })
    .await;
    advance_to(accepted + IDLE - TIMER_ROUNDING).await;
    settle_wakeups().await;
    let one_tick_short: Poll<StreamNext> = poll_once(next.as_mut()).await;
    match one_tick_short {
        Poll::Ready(early) => (false, early),
        Poll::Pending => {
            let pending_before = !driver.observed().source_released;
            advance_to(accepted + IDLE).await;
            tokio::time::advance(TIMER_ROUNDING).await;
            let result: StreamNext = resolved(next, context, "the expired demand").await;
            (pending_before, result)
        }
    }
}

/// Require an expired demand to have ended at its idle expiry and not before.
fn assert_idle_expiry(driver: &ClientBodyDriver, outcome: &(bool, StreamNext), context: &str) {
    let (pending_before, result) = outcome;
    let seen: ClientBodyObservation = driver.observed();
    assert!(
        *pending_before,
        "{context}: the demand ended before its idle interval, and returned {result:?}"
    );
    assert_terminal(&seen, result, RESPONSE_IDLE, context);
}

/// A demand the source never answers ends at its idle interval, whether the
/// source is silent or sends only framing.
async fn assert_quiet_demand_expires(quiet: Vec<ClientBodyFrame>, context: &str) {
    let (driver, mut body) = started(bounds(), context).await;
    let outcome = expired_demand(&driver, &mut body, quiet, context).await;
    assert_idle_expiry(&driver, &outcome, context);
    assert_sticky(&driver, &mut body, RESPONSE_IDLE, context).await;
    finish(&driver, body, Some(RESPONSE_IDLE), context).await;
}

/// How long the caller holds what it was given before its next demand: five
/// idle intervals, inside the operation total.
pub(crate) const CALLER_PAUSE: Duration = Duration::from_secs(10);

/// A caller that pauses longer than the idle interval expires nothing. The
/// interval starts at its next demand.
async fn assert_caller_pause_is_not_idle() {
    let context = "caller pause";
    let (driver, mut body) = started(bounds(), context).await;
    driver.feed(ClientBodyFrame::Data(plain(4, b'h')));
    let chunk: Bytes = delivered(&mut body, context).await;

    tokio::time::advance(CALLER_PAUSE).await;
    settle_wakeups().await;
    let paused: ClientBodyObservation = driver.observed();
    let outcome = expired_demand(&driver, &mut body, Vec::new(), context).await;

    assert_eq!(chunk.len(), 4, "{context}: the chunk before the pause");
    assert!(
        !paused.source_released && paused.source_frames == 1,
        "{context}: the pause ended or polled the source: {paused:?}"
    );
    assert_idle_expiry(&driver, &outcome, context);
    finish(&driver, body, Some(RESPONSE_IDLE), context).await;
}

/// When a carried expiry sits, measured from the instant the bounds were
/// fixed. An operation fixes them at its first poll.
pub(crate) const CARRIED_BOUND: Duration = Duration::from_secs(5);

/// How long the source waits between that instant and its handoff. A real
/// response is handed off when its head arrives.
pub(crate) const HANDOFF_DELAY: Duration = Duration::from_secs(2);

/// One expiry fixed before the handoff.
struct Carried {
    label: &'static str,
    total: Duration,
    attempt: Option<Duration>,
    ending: Ending,
}

const CARRIED: [Carried; 2] = [
    Carried {
        label: "attempt expiry with no demand",
        total: TOTAL,
        attempt: Some(CARRIED_BOUND),
        ending: ATTEMPT,
    },
    Carried {
        label: "operation expiry with no demand",
        total: CARRIED_BOUND,
        attempt: None,
        ending: OPERATION,
    },
];

/// An expiry fixed before the handoff ends an unread body at its original
/// instant, with no demand and no caller poll.
///
/// The handoff happens two seconds after the bounds were fixed. An engine
/// that restarted either clock at the handoff would still be pending at the
/// original instant.
async fn assert_carried_expiry_needs_no_demand(row: &Carried) {
    let context = row.label;
    let fixed = Instant::now();
    let carried = ClientBodyBounds {
        total: row.total,
        attempt: row.attempt,
        ..bounds()
    };
    let driver: ClientBodyDriver = ClientBodyDriver::begin(carried);
    tokio::time::advance(HANDOFF_DELAY).await;
    let mut body: ClientBody = handed_off(&driver, context).await;

    advance_to(fixed + CARRIED_BOUND - TIMER_ROUNDING).await;
    settle_wakeups().await;
    let before: ClientBodyObservation = driver.observed();
    advance_to(fixed + CARRIED_BOUND).await;
    tokio::time::advance(TIMER_ROUNDING).await;
    let after = reached(&driver, context, "the expiry released the source", |seen| {
        seen.source_released
    })
    .await;

    assert!(
        !before.source_released,
        "{context}: the body ended before its original expiry: {before:?}"
    );
    assert_eq!(
        after.source_polls, 0,
        "{context}: an unread body was polled: {after:?}"
    );
    assert_ends(&driver, &mut body, row.ending, context).await;
    assert_sticky(&driver, &mut body, row.ending, context).await;
    finish(&driver, body, Some(row.ending), context).await;
}

/// Closing a live unread body succeeds, and returns only after the source and
/// its queued frame are released.
async fn assert_close_acknowledges_release() {
    let context = "close of an unread body";
    let (driver, body) = started(bounds(), context).await;
    let (queued, queued_released) = witnessed(8, b'k');
    driver.feed(ClientBodyFrame::Data(queued));

    let closed: Result<(), RuntimeError> = resolved(body.close(), context, "the close").await;
    let seen: ClientBodyObservation = driver.observed();
    let dropped = releases(&queued_released);
    owner_ended(&driver, context).await;

    assert!(
        matches!(closed, Ok(())),
        "{context}: an intentional close returned {closed:?}"
    );
    assert!(
        seen.source_released && seen.source_polls == 0,
        "{context}: the close returned before release, or polled the source: {seen:?}"
    );
    assert_eq!(
        dropped, 1,
        "{context}: the close returned before the queued frame was released"
    );
}

/// Where a pending demand is when its caller drops it.
#[derive(Clone, Copy, Debug)]
enum Abandoned {
    /// The source has not answered.
    QuietSource,
    /// The owner admitted the chunk and the caller has not taken it.
    ChunkWaiting,
}

/// Poll one demand, bring it to `stage`, and drop it unfinished.
async fn abandon_demand(
    driver: &ClientBodyDriver,
    body: &mut ClientBody,
    stage: Abandoned,
    context: &str,
) {
    let mut next = std::pin::pin!(body.next());
    park(next.as_mut(), context).await;
    match stage {
        Abandoned::QuietSource => demand_accepted(driver, context).await,
        Abandoned::ChunkWaiting => chunk_waiting(driver, context).await,
    };
}

/// Start a body, bring one demand to `stage`, and drop it unfinished.
///
/// Returns the release count of the chunk that waited. A quiet source was
/// fed no chunk, so it has no count: `None`.
async fn abandoned(
    stage: Abandoned,
    context: &str,
) -> (ClientBodyDriver, ClientBody, Option<Arc<AtomicUsize>>) {
    let (driver, mut body) = started(bounds(), context).await;
    let chunk_released = match stage {
        Abandoned::ChunkWaiting => Some(feed_witnessed(&driver)),
        Abandoned::QuietSource => None,
    };
    abandon_demand(&driver, &mut body, stage, context).await;
    (driver, body, chunk_released)
}

/// Feed one witnessed chunk, and return its release count.
fn feed_witnessed(driver: &ClientBodyDriver) -> Arc<AtomicUsize> {
    let (chunk, released) = witnessed(6, b'a');
    driver.feed(ClientBodyFrame::Data(chunk));
    released
}

/// Require the storage of a chunk that waited to be released exactly once, or
/// panic with `fault`.
///
/// A row that fed no chunk has no count, and this proves nothing for it.
fn assert_released_once(dropped: Option<usize>, context: &str, fault: &str) {
    if let Some(count) = dropped {
        assert_eq!(count, 1, "{context}: {fault}");
    }
}

/// A dropped pending demand commits abandonment. A chunk that waited is
/// released, not handed to a later demand, and every later demand reports
/// cancellation.
async fn assert_abandoned_demand_is_cancellation(stage: Abandoned, context: &str) {
    let (driver, mut body, chunk_released) = abandoned(stage, context).await;
    let released = reached(
        &driver,
        context,
        "the abandonment released the source",
        |seen| seen.source_released,
    )
    .await;
    let dropped: Option<usize> = chunk_released.as_ref().map(releases);
    assert_ends(&driver, &mut body, Ending::Cancelled, context).await;
    assert_sticky(&driver, &mut body, Ending::Cancelled, context).await;
    finish(&driver, body, None, context).await;

    assert_eq!(
        released.retained_chunks, 0,
        "{context}: the abandoned chunk was kept: {released:?}"
    );
    assert_released_once(dropped, context, "the abandoned chunk's storage was kept");
}

/// The demand placed straight after an abandonment reports cancellation, and
/// returns only after the source is released.
///
/// The row gives the owner no turn between the drop and the next demand. That
/// demand therefore meets the abandonment while the owner still holds its
/// source: it must not take the chunk that waited, and it must not return on
/// a fact the owner has not acknowledged.
async fn assert_abandonment_answers_the_next_demand(stage: Abandoned, context: &str) {
    let (driver, mut body, chunk_released) = abandoned(stage, context).await;
    let unserved: ClientBodyObservation = driver.observed();
    assert_ends(&driver, &mut body, Ending::Cancelled, context).await;
    let dropped: Option<usize> = chunk_released.as_ref().map(releases);
    assert_sticky(&driver, &mut body, Ending::Cancelled, context).await;
    finish(&driver, body, None, context).await;

    assert!(
        !unserved.source_released,
        "{context}: the owner released its source before the next demand: {unserved:?}"
    );
    assert_released_once(
        dropped,
        context,
        "the abandoned chunk was not released once at the return",
    );
}

/// An abandonment the caller commits beside the owner's ready expiry stays
/// the fact, and its close succeeds.
///
/// One clock step crosses the operation deadline. The step wakes the owner
/// and returns to the row before the owner runs, so the close commits the
/// abandonment while the expiry is ready and unread. The owner then reads its
/// deadline ahead of the caller's fact, and its own commit must keep that
/// fact.
async fn assert_abandonment_survives_owner_expiry() {
    let context = "close beside a ready expiry";
    let fixed = Instant::now();
    let expiring = ClientBodyBounds {
        total: CARRIED_BOUND,
        ..bounds()
    };
    let (driver, body) = started(expiring, context).await;

    advance_to(fixed + CARRIED_BOUND + TIMER_ROUNDING).await;
    let staged: ClientBodyObservation = driver.observed();
    assert!(
        !staged.source_released,
        "{context}: the owner ended before the close, so the row staged nothing: {staged:?}"
    );
    finish(&driver, body, None, context).await;
}

/// Dropping an unread body releases its source and ends its owner while the
/// caller polls nothing.
async fn assert_drop_releases_unpolled() {
    let context = "dropped unread body";
    let (driver, body) = started(bounds(), context).await;
    let (queued, queued_released) = witnessed(8, b'g');
    driver.feed(ClientBodyFrame::Data(queued));

    drop(body);
    let ended = reached(&driver, context, "the drop ended the owner", |seen| {
        seen.source_released && seen.owner_ended
    })
    .await;

    assert_eq!(
        (ended.source_polls, ended.retained_chunks),
        (0, 0),
        "{context}: a dropped body was polled or kept a chunk: {ended:?}"
    );
    assert_eq!(
        releases(&queued_released),
        1,
        "{context}: the dropped source kept its queued frame"
    );
}

/// Abort the owner under a pending demand, and return what the demand got.
async fn demand_across_owner_loss(
    driver: &ClientBodyDriver,
    body: &mut ClientBody,
    context: &str,
) -> StreamNext {
    let mut next = std::pin::pin!(body.next());
    park(next.as_mut(), context).await;
    demand_accepted(driver, context).await;
    driver.abort_owner();
    owner_ended(driver, context).await;
    resolved(next, context, "the demand of a lost owner").await
}

/// An owner lost with no cause committed is a transport failure. The pending
/// demand is woken with it, and it is never a clean end.
async fn assert_owner_loss_is_transport() {
    let context = "owner loss";
    let (driver, mut body) = started(bounds(), context).await;
    let lost: StreamNext = demand_across_owner_loss(&driver, &mut body, context).await;
    let seen: ClientBodyObservation = driver.observed();

    assert_terminal(&seen, &lost, TRANSPORT, context);
    assert_sticky(&driver, &mut body, TRANSPORT, context).await;
    finish(&driver, body, Some(TRANSPORT), context).await;
}

/// A cause committed before the owner is lost stays the cause.
async fn assert_committed_cause_survives_owner_loss() {
    let context = "owner loss after a committed cause";
    let (driver, mut body) = started(bounds(), context).await;
    let outcome = expired_demand(&driver, &mut body, Vec::new(), context).await;
    assert_idle_expiry(&driver, &outcome, context);

    driver.abort_owner();
    owner_ended(&driver, context).await;
    assert_sticky(&driver, &mut body, RESPONSE_IDLE, context).await;
    finish(&driver, body, Some(RESPONSE_IDLE), context).await;
}

/// 4.T2
#[tokio::test(start_paused = true)]
async fn client_body_time_and_cancellation_settle_without_demand() {
    assert_quiet_demand_expires(Vec::new(), "silent demand").await;
    let framing = vec![
        ClientBodyFrame::Data(Bytes::new()),
        ClientBodyFrame::Trailers,
    ];
    assert_quiet_demand_expires(framing, "demand answered with framing only").await;
    assert_caller_pause_is_not_idle().await;
    for row in &CARRIED {
        assert_carried_expiry_needs_no_demand(row).await;
    }
    assert_close_acknowledges_release().await;
    assert_abandoned_demand_is_cancellation(Abandoned::QuietSource, "abandoned quiet demand").await;
    assert_abandoned_demand_is_cancellation(Abandoned::ChunkWaiting, "abandoned waiting chunk")
        .await;
    assert_abandonment_answers_the_next_demand(Abandoned::QuietSource, "demand behind a quiet one")
        .await;
    assert_abandonment_answers_the_next_demand(
        Abandoned::ChunkWaiting,
        "demand behind a waiting chunk",
    )
    .await;
    assert_abandonment_survives_owner_expiry().await;
    assert_drop_releases_unpolled().await;
    assert_owner_loss_is_transport().await;
    assert_committed_cause_survives_owner_loss().await;
}
