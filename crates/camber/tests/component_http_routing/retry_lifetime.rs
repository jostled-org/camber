//! 2.T1–2.T3, 4.T2 and 4.T4: one retry sequence, one absolute deadline, one
//! cancellation, and the delays between its attempts.
//!
//! Every row runs on a paused current-thread runtime against a scripted raw
//! upstream or a refused port. The clock moves only after the client future has
//! been polled and the peer or the production event has acknowledged the phase
//! the client is parked in, so each deadline a row crosses is one the row
//! placed, not one elapsed wall time happened to reach.

use crate::retry_upstream::{
    Answer, Call, PeerEvent, RunnableDriver, ScriptedUpstream, UNSAFE_METHODS, advance_to,
    assert_released, describe, send_method, settle_wakeups, settled, spawn_get, within_watchdog,
};
use crate::trace_capture::{TraceCapture, assert_field_value, capture_events};

use camber::http::{self, DeadlineBoundary, Response};
use camber::runtime_test_support::install_runtime_context;
use camber::{RuntimeError, runtime};
use std::time::Duration;
use tokio::time::Instant;

/// The one route every retry-lifetime row requests.
const RETRY_PATH: &str = "/retry-lifetime";

/// The configured bound on a whole retry sequence.
const RETRY_TIMEOUT: Duration = Duration::from_secs(5);

/// Every per-attempt bound, set longer than the sequence it sits inside so
/// only the sequence deadline can end a stalled attempt.
const ATTEMPT_LIMIT: Duration = Duration::from_secs(60);

/// The base backoff of rows that cross a delay on their way to a later phase.
///
/// The first delay is below twice this, and twice this is below the retry
/// deadline, so one step onto that bound finishes the delay and nothing else.
const CROSSED_BACKOFF: Duration = Duration::from_secs(1);

/// The base backoff of the row parked inside a delay: longer than the whole
/// sequence, so the deadline is what ends it.
const PARKED_BACKOFF: Duration = Duration::from_secs(60);

/// The documented retry-sequence bound a client starts with.
const DEFAULT_RETRY_TIMEOUT: Duration = Duration::from_secs(30);

/// The floor every infallible client deadline setter clamps to.
const CLAMP_FLOOR: Duration = Duration::from_millis(1);

/// The failure a row requires of a call that ended without an answer.
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// The retry sequence's own deadline ended it.
    RetryDeadline,
    /// Runtime shutdown ended it.
    Cancelled,
    /// One attempt's own lifetime ended it.
    AttemptTimeout,
}

impl Ending {
    fn ended(self, result: &Option<Result<Response, RuntimeError>>) -> bool {
        matches!(
            (self, result),
            (
                Self::RetryDeadline,
                Some(Err(RuntimeError::DeadlineExceeded(
                    DeadlineBoundary::ClientRetry
                )))
            ) | (Self::Cancelled, Some(Err(RuntimeError::Cancelled)))
                | (Self::AttemptTimeout, Some(Err(RuntimeError::Timeout)))
        )
    }
}

/// Where one row parks the retry sequence before crossing its deadline or
/// accepting its cancellation.
#[derive(Clone, Copy, Debug)]
enum Phase {
    /// Waiting for the first attempt's response head.
    InitialHead,
    /// Waiting for a later attempt's response head, after a transient answer.
    LaterHead,
    /// Collecting the final answer's body, after a transient answer.
    FinalBody,
    /// Inside the delay after a transient answer.
    Backoff,
}

const PHASES: [Phase; 4] = [
    Phase::InitialHead,
    Phase::LaterHead,
    Phase::FinalBody,
    Phase::Backoff,
];

impl Phase {
    fn script(self) -> &'static [Answer] {
        match self {
            Self::InitialHead => &[Answer::StallHead],
            Self::LaterHead => &[Answer::Transient, Answer::StallHead],
            Self::FinalBody => &[Answer::Transient, Answer::StallBody],
            Self::Backoff => &[Answer::Transient],
        }
    }

    fn backoff(self) -> Duration {
        match self {
            Self::Backoff => PARKED_BACKOFF,
            Self::InitialHead | Self::LaterHead | Self::FinalBody => CROSSED_BACKOFF,
        }
    }

    /// The attempt whose transport is still open while the sequence is parked,
    /// or `None` when the sequence is parked between attempts.
    fn in_flight(self) -> Option<usize> {
        match self {
            Self::InitialHead => Some(0),
            Self::LaterHead | Self::FinalBody => Some(1),
            Self::Backoff => None,
        }
    }

    /// Every request start the parked sequence has made, and all it may make.
    fn starts(self) -> u32 {
        match self {
            Self::InitialHead | Self::Backoff => 1,
            Self::LaterHead | Self::FinalBody => 2,
        }
    }

    /// A client with retries to spare at every phase, whose only bound short
    /// enough to matter is the retry sequence's own.
    fn client(self) -> http::ClientBuilder {
        sequence_client(3, self.backoff())
    }

    /// Drive the sequence into this phase and wait for the peer to confirm it.
    ///
    /// A crossed delay is stepped over only once the peer has seen the
    /// transient answer disposed, which is when the delay has begun.
    async fn reach(self, upstream: &mut ScriptedUpstream, context: &str) {
        upstream.expect(PeerEvent::Started(0), context).await;
        match self {
            Self::InitialHead => upstream.expect(PeerEvent::Stalled(0), context).await,
            Self::Backoff => upstream.expect(PeerEvent::Disposed(0), context).await,
            Self::LaterHead | Self::FinalBody => {
                upstream.expect(PeerEvent::Disposed(0), context).await;
                tokio::time::advance(2 * CROSSED_BACKOFF).await;
                upstream
                    .expect_each(&[PeerEvent::Started(1), PeerEvent::Stalled(1)], context)
                    .await;
            }
        }
    }
}

/// A client whose connect, request, and idle bounds all outlast every row.
fn long_attempt_client() -> http::ClientBuilder {
    http::client()
        .connect_timeout(ATTEMPT_LIMIT)
        .request_timeout(ATTEMPT_LIMIT)
        .response_idle_timeout(ATTEMPT_LIMIT)
}

/// A client whose only bound short enough to matter is the retry sequence's
/// own.
fn sequence_client(retries: u32, backoff: Duration) -> http::ClientBuilder {
    long_attempt_client()
        .retries(retries)
        .backoff(backoff)
        .retry_timeout(RETRY_TIMEOUT)
}

/// One GET in flight against a scripted upstream, with the instant it entered
/// the client and the driver that holds the paused clock still.
struct RetryRow {
    driver: RunnableDriver,
    upstream: ScriptedUpstream,
    entry: Instant,
    call: Call,
}

impl RetryRow {
    /// Bind an upstream that answers from `script`, then start one GET through
    /// `client` against it.
    async fn start(script: &[Answer], client: http::ClientBuilder) -> Self {
        let driver = RunnableDriver::start();
        let upstream = ScriptedUpstream::bind(script).await;
        let entry = Instant::now();
        let call = spawn_get(client, upstream.url(RETRY_PATH));
        Self {
            driver,
            upstream,
            entry,
            call,
        }
    }

    /// Start `phase`'s script through `client` and drive the call into that
    /// phase.
    async fn parked(phase: Phase, client: http::ClientBuilder, context: &str) -> Self {
        let mut row = Self::start(phase.script(), client).await;
        phase.reach(&mut row.upstream, context).await;
        row
    }

    /// A single-retry sequence with `backoff` whose first attempt answered
    /// transient with `Retry-After: value` and was disposed, so the call now
    /// waits on the delay before its one retry.
    async fn after_retry_after(value: &'static str, backoff: Duration, context: &str) -> Self {
        let mut row = Self::start(
            &[Answer::TransientRetryAfter(value), Answer::Complete],
            sequence_client(1, backoff),
        )
        .await;
        row.upstream
            .expect_each(&[PeerEvent::Started(0), PeerEvent::Disposed(0)], context)
            .await;
        row
    }
}

/// Whether the parked attempt's transport was closed by the client, or what
/// the upstream reported instead.
async fn released_in_flight(phase: Phase, upstream: &mut ScriptedUpstream) -> Result<(), String> {
    match phase.in_flight() {
        Some(attempt) => upstream.saw(PeerEvent::Released(attempt)).await,
        None => Ok(()),
    }
}

/// Assert what every row owes after its fixture has been released.
fn assert_cleanup(phase: Phase, released: Result<(), String>, starts: u32, context: &str) {
    assert_released(released, context);
    assert_eq!(
        starts,
        phase.starts(),
        "{context}: the call started {starts} requests"
    );
}

/// What ends one parked sequence, and the endings that ending admits.
#[derive(Clone, Copy, Debug)]
enum Trigger {
    /// Step onto the deadline fixed at call entry. The step lands on call
    /// entry plus the retry timeout, not on the parked attempt's start plus
    /// it, so a deadline any attempt or delay reset would not yet have expired.
    Deadline,
    /// Request runtime shutdown through the public control.
    Shutdown,
    /// Request shutdown, then step onto the deadline, with no happens-before
    /// edge between them the client could observe. Either fact may be the
    /// committed one; nothing else is admitted.
    ExpiryBesideShutdown,
}

impl Trigger {
    fn requests_shutdown(self) -> bool {
        matches!(self, Self::Shutdown | Self::ExpiryBesideShutdown)
    }

    fn crosses_deadline(self) -> bool {
        matches!(self, Self::Deadline | Self::ExpiryBesideShutdown)
    }

    fn admitted(self) -> &'static [Ending] {
        match self {
            Self::Deadline => &[Ending::RetryDeadline],
            Self::Shutdown => &[Ending::Cancelled],
            Self::ExpiryBesideShutdown => &[Ending::RetryDeadline, Ending::Cancelled],
        }
    }
}

/// One parked row: park the sequence, apply `trigger`, and require an ending
/// it admits, with every held peer released and nothing sent after it.
///
/// A row that requests shutdown installs the runtime context that request
/// reaches, and holds it until the fixture is released.
async fn assert_parked_ends(phase: Phase, trigger: Trigger) {
    let context = format!("{trigger:?} while parked at {phase:?}");
    let runtime_context = trigger.requests_shutdown().then(install_runtime_context);
    let RetryRow {
        driver,
        mut upstream,
        entry,
        call,
    } = RetryRow::parked(phase, phase.client(), &context).await;

    if trigger.requests_shutdown() {
        runtime::request_shutdown();
    }
    if trigger.crosses_deadline() {
        advance_to(entry + RETRY_TIMEOUT).await;
    }
    let result = settled(call).await;
    let released = released_in_flight(phase, &mut upstream).await;
    let starts = upstream.finish(driver, &context).await;
    drop(runtime_context);

    let admitted = trigger.admitted();
    assert!(
        admitted.iter().any(|ending| ending.ended(&result)),
        "{context}: returned {}, outside {admitted:?}",
        describe(&result)
    );
    assert_cleanup(phase, released, starts, &context);
}

/// 2.T1
#[tokio::test(start_paused = true)]
async fn retry_deadline_encloses_every_pending_phase() {
    for phase in PHASES {
        assert_parked_ends(phase, Trigger::Deadline).await;
    }
}

/// The tasks a caller-drop row's fixture keeps alive once every attempt it
/// served has ended: the runnable driver and the upstream's accept loop.
const FIXTURE_TASKS: usize = 2;

/// Every task alive on the row's runtime.
fn alive_tasks() -> usize {
    tokio::runtime::Handle::current()
        .metrics()
        .num_alive_tasks()
}

/// One caller-drop row: park the sequence, drop the call, and require the
/// transport and every owner the call made released with no attempt after the
/// drop.
///
/// Dropping the call is ordinary caller cancellation, not a runtime shutdown,
/// so no result is owed; the evidence is what the peer and the runtime observe.
/// The request owner is read off the runtime before the clock moves: an
/// owner the drop left behind — a detached sequence, a connection driver the
/// call spawned — is still parked on its deadline or its socket then, so it is
/// still an alive task beside the fixture's own. The clock is then stepped
/// past every delay and deadline the sequence held, so an owner that outlived
/// the drop would have had its chance to send.
async fn assert_caller_drop_releases(phase: Phase) {
    let context = format!("caller drop while parked at {phase:?}");
    let RetryRow {
        driver,
        mut upstream,
        call,
        ..
    } = RetryRow::parked(phase, phase.client(), &context).await;

    call.abort();
    let joined = within_watchdog(call).await;
    let released = released_in_flight(phase, &mut upstream).await;
    settle_wakeups().await;
    let alive_after_drop = alive_tasks();
    tokio::time::advance(RETRY_TIMEOUT + 2 * phase.backoff()).await;
    settle_wakeups().await;
    let starts = upstream.finish(driver, &context).await;

    assert!(
        matches!(&joined, Some(Err(error)) if error.is_cancelled()),
        "{context}: the dropped call was not cancelled"
    );
    assert_eq!(
        alive_after_drop, FIXTURE_TASKS,
        "{context}: tasks outlived the dropped call beside the fixture's own"
    );
    assert_cleanup(phase, released, starts, &context);
}

/// 2.T2
#[tokio::test(start_paused = true)]
async fn retry_shutdown_cancels_each_pending_phase() {
    for trigger in [Trigger::Shutdown, Trigger::ExpiryBesideShutdown] {
        for phase in PHASES {
            assert_parked_ends(phase, trigger).await;
        }
    }
    for phase in PHASES {
        assert_caller_drop_releases(phase).await;
    }
}

/// A client whose only bound short enough to matter is `retry_timeout`, set or
/// left at its default by the caller.
fn head_stall_client(retries: u32) -> http::ClientBuilder {
    long_attempt_client()
        .retries(retries)
        .backoff(CROSSED_BACKOFF)
}

/// Step to one tick before `deadline`, let every woken task run, and report
/// whether the call is still pending there.
///
/// The step is measured from call entry by the caller's `deadline`, so a row
/// proves where a bound sits rather than only that it is eventually crossed.
async fn pending_just_before(call: &Call, deadline: Instant) -> bool {
    advance_to(deadline - CLAMP_FLOOR).await;
    settle_wakeups().await;
    !call.is_finished()
}

/// Report whether the call is still pending one tick before `deadline`, then
/// step onto `deadline`.
async fn pending_until(call: &Call, deadline: Instant) -> bool {
    let pending = pending_just_before(call, deadline).await;
    advance_to(deadline).await;
    pending
}

/// Park the first attempt's head, step to one tick before `bound`, require the
/// call still pending, then step onto `bound` and require `expected`.
///
/// Both steps are measured from call entry, so the row proves where the bound
/// sits rather than only that it is eventually crossed.
async fn assert_head_stall_ends_at(
    client: http::ClientBuilder,
    bound: Duration,
    expected: Ending,
    context: &str,
) {
    const HEAD_STALL: Phase = Phase::InitialHead;

    let RetryRow {
        driver,
        mut upstream,
        entry,
        call,
    } = RetryRow::parked(HEAD_STALL, client, context).await;

    let pending_before_bound = pending_until(&call, entry + bound).await;
    let result = settled(call).await;
    let released = released_in_flight(HEAD_STALL, &mut upstream).await;
    let starts = upstream.finish(driver, context).await;

    assert!(
        pending_before_bound,
        "{context}: the call ended before {bound:?}"
    );
    assert!(
        expected.ended(&result),
        "{context}: returned {}",
        describe(&result)
    );
    assert_cleanup(HEAD_STALL, released, starts, context);
}

/// Require `client` to project `expected` as its retry timeout.
fn assert_retry_timeout_projected(client: &http::ClientBuilder, expected: Duration, label: &str) {
    let projected = format!("{client:?}");
    assert!(
        projected.contains(&format!("retry_timeout: {expected:?}")),
        "{label}: retry_timeout is not projected as {expected:?}: {projected}"
    );
}

/// 2.T3: the sequence exists only with configured retries, defaults to thirty
/// seconds, and clamps a zero or sub-millisecond bound to one millisecond.
#[tokio::test(start_paused = true)]
async fn retry_timeout_defaults_clamps_and_needs_configured_retries() {
    let defaulted = head_stall_client(3);
    assert_retry_timeout_projected(&defaulted, DEFAULT_RETRY_TIMEOUT, "default retry timeout");
    assert_head_stall_ends_at(
        defaulted,
        DEFAULT_RETRY_TIMEOUT,
        Ending::RetryDeadline,
        "default retry timeout",
    )
    .await;

    for (written, label) in [
        (Duration::ZERO, "zero retry timeout"),
        (Duration::from_micros(500), "sub-millisecond retry timeout"),
    ] {
        let clamped = head_stall_client(3).retry_timeout(written);
        assert_retry_timeout_projected(&clamped, CLAMP_FLOOR, label);
        assert_head_stall_ends_at(clamped, CLAMP_FLOOR, Ending::RetryDeadline, label).await;
    }

    // Zero configured retries is no sequence at all: the attempt's own
    // lifetime is the whole authority, however short the retry bound.
    let attempt_only = long_attempt_client()
        .retries(0)
        .request_timeout(RETRY_TIMEOUT)
        .retry_timeout(CLAMP_FLOOR);
    assert_head_stall_ends_at(
        attempt_only,
        RETRY_TIMEOUT,
        Ending::AttemptTimeout,
        "zero configured retries",
    )
    .await;
}

/// 2.T3: a per-attempt timeout that commits before the sequence deadline keeps
/// its own result, and that result is still eligible for another attempt.
#[tokio::test(start_paused = true)]
async fn attempt_timeout_inside_the_retry_deadline_stays_retryable() {
    const CONTEXT: &str = "attempt timeout inside the retry deadline";
    const ATTEMPT: Duration = Duration::from_secs(1);

    let client = sequence_client(2, CROSSED_BACKOFF).request_timeout(ATTEMPT);
    let RetryRow {
        driver,
        mut upstream,
        call,
        ..
    } = RetryRow::start(&[Answer::StallHead, Answer::Complete], client).await;
    upstream
        .expect_each(&[PeerEvent::Started(0), PeerEvent::Stalled(0)], CONTEXT)
        .await;

    tokio::time::advance(ATTEMPT).await;
    upstream.expect(PeerEvent::Released(0), CONTEXT).await;
    tokio::time::advance(2 * CROSSED_BACKOFF).await;
    upstream.finish_answered(1, call, driver, CONTEXT).await;
}

/// The last IMF-fixdate the grammar can name: no configured sequence lasts
/// until it.
const FAR_FUTURE_FIXDATE: &str = "Fri, 31 Dec 9999 23:59:59 GMT";

/// The largest delta-seconds a `u64` second count holds.
const LARGE_DELTA: &str = "18446744073709551615";

/// An IMF-fixdate already behind every wall clock this row runs under.
const PAST_FIXDATE: &str = "Sun, 06 Nov 1994 08:49:37 GMT";

/// A value in neither `Retry-After` form.
const INVALID_RETRY_AFTER: &str = "soon";

/// A valid wait inside the sequence and shorter than the parked backoff, as
/// delta-seconds and as the duration it names.
const STATED_DELTA: &str = "2";
const STATED_DELAY: Duration = Duration::from_secs(2);

/// One clipped row: a stated wait past the sequence deadline parks the
/// sequence until that deadline, which ends it with no second attempt.
///
/// The configured backoff is short, so a sequence that ignored the stated
/// wait would have started its second attempt long before the step one tick
/// short of the deadline.
async fn assert_retry_after_clipped(value: &'static str) {
    let context = format!("Retry-After {value:?} past the retry deadline");
    let RetryRow {
        driver,
        upstream,
        entry,
        call,
    } = RetryRow::after_retry_after(value, CROSSED_BACKOFF, &context).await;

    let pending_before_deadline = pending_until(&call, entry + RETRY_TIMEOUT).await;
    let result = settled(call).await;
    let starts = upstream.finish(driver, &context).await;

    assert!(
        pending_before_deadline,
        "{context}: the call ended before the retry deadline"
    );
    assert!(
        Ending::RetryDeadline.ended(&result),
        "{context}: returned {}",
        describe(&result)
    );
    assert_eq!(starts, 1, "{context}: the call started {starts} requests");
}

/// One invalid row: a value in neither form leaves the configured backoff in
/// force, so the second attempt waits for it and then answers.
async fn assert_invalid_retry_after_backs_off() {
    let context = format!("Retry-After {INVALID_RETRY_AFTER:?}");
    let RetryRow {
        driver,
        upstream,
        entry,
        call,
    } = RetryRow::after_retry_after(INVALID_RETRY_AFTER, CROSSED_BACKOFF, &context).await;

    let pending_inside_backoff = pending_just_before(&call, entry + CROSSED_BACKOFF).await;
    advance_to(entry + 2 * CROSSED_BACKOFF).await;
    upstream.finish_answered(1, call, driver, &context).await;

    assert!(
        pending_inside_backoff,
        "{context}: the call ended inside the configured backoff"
    );
}

/// One immediate row: a zero or past wait replaces a configured backoff
/// longer than the whole sequence, so the second attempt starts with the clock
/// held still.
async fn assert_retry_after_immediate(value: &'static str) {
    let context = format!("Retry-After {value:?} permitting an immediate retry");
    let RetryRow {
        driver,
        upstream,
        call,
        ..
    } = RetryRow::after_retry_after(value, PARKED_BACKOFF, &context).await;
    upstream.finish_answered(1, call, driver, &context).await;
}

/// One stated row: a valid wait inside the sequence replaces a configured
/// backoff longer than the sequence, and the second attempt starts exactly
/// when that wait ends.
async fn assert_retry_after_replaces_backoff() {
    let context = format!("Retry-After {STATED_DELTA:?} replacing the backoff");
    let RetryRow {
        driver,
        upstream,
        entry,
        call,
    } = RetryRow::after_retry_after(STATED_DELTA, PARKED_BACKOFF, &context).await;

    let pending_inside_wait = pending_until(&call, entry + STATED_DELAY).await;
    upstream.finish_answered(1, call, driver, &context).await;

    assert!(
        pending_inside_wait,
        "{context}: the call ended inside the stated wait"
    );
}

/// 4.T2
#[tokio::test(start_paused = true)]
async fn retry_after_is_clipped_and_invalid_values_use_backoff() {
    assert_retry_after_clipped(FAR_FUTURE_FIXDATE).await;
    assert_retry_after_clipped(LARGE_DELTA).await;
    assert_invalid_retry_after_backs_off().await;
    assert_retry_after_immediate("0").await;
    assert_retry_after_immediate(PAST_FIXDATE).await;
    assert_retry_after_replaces_backoff().await;
}

const CONNECT_RETRY_TEST: &str =
    "retry_lifetime::unsafe_connect_failure_retries_through_the_public_client";
const CONNECT_RETRY_MODE: &str = "unsafe-connect-retry-events";
const CONNECT_RETRY_MARKER: &str = "unsafe-connect-retry-events-observed";

/// The real-time bound on the whole private child. It bounds fixture failure
/// only.
const CONNECT_RETRY_CHILD_BOUND: Duration = Duration::from_secs(60);

/// The URL no service listens on: port zero is never a listener's address, so
/// every send fails at the connect stage without a reserved port.
const REFUSED_URL: &str = "http://127.0.0.1:0/";

/// The fixed sentence production records after each replayable failed send.
const RETRY_EVENT: &str = "retrying transient HTTP error";

const CONNECT_RETRIES: u32 = 2;
const CONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Start one unsafe method through the public client as its own task.
fn spawn_unsafe(client: http::ClientBuilder, method: &'static str, url: &'static str) -> Call {
    tokio::spawn(async move { send_method(&client, method, url).await })
}

/// A client whose retry, attempt, and sequence bounds all outlast the row, so
/// only the configured retry count and the backoff shape the attempts.
fn connect_retry_client(opt_in: bool) -> http::ClientBuilder {
    long_attempt_client()
        .retries(CONNECT_RETRIES)
        .retry_unsafe_methods(opt_in)
        .backoff(CONNECT_BACKOFF)
        .retry_timeout(ATTEMPT_LIMIT)
}

/// A step past every delay the backoff can choose after `attempt`: the
/// exponential term plus the jitter it adds, which stays below the base.
fn past_backoff(attempt: u32) -> Duration {
    CONNECT_BACKOFF * (1_u32 << attempt) + CONNECT_BACKOFF
}

/// What the client and the capture showed first.
#[derive(Debug)]
enum Progress {
    /// The capture holds the awaited count of retry events.
    Event,
    /// The call returned.
    Returned,
    /// Neither happened within the fixture watchdog.
    Stalled,
}

/// Poll the call and the capture together until `events` retry events are
/// recorded or the call returns.
///
/// Each turn hands the thread to the call first, so the capture is read only
/// once the call has parked on whatever it registered. The loop itself stays
/// runnable, so the paused clock never advances on its own while it waits.
async fn next_progress(call: &Call, capture: &TraceCapture, events: usize) -> Progress {
    let progress = within_watchdog(async {
        loop {
            tokio::task::yield_now().await;
            match (capture.len() >= events, call.is_finished()) {
                (true, _) => return Progress::Event,
                (false, true) => return Progress::Returned,
                (false, false) => {}
            }
        }
    })
    .await;
    progress.unwrap_or(Progress::Stalled)
}

/// Require the retry event for `attempt` of `method`, as production recorded
/// it.
fn assert_retry_event(capture: &TraceCapture, method: &str, attempt: u32, context: &str) {
    let events = capture.events();
    let event = &events[attempt as usize - 1];
    assert_field_value(event, "attempt", &attempt.to_string(), context);
    assert_field_value(event, "method", method, context);
    assert!(
        event.contains(RETRY_EVENT),
        "{context}: event {attempt} is not a retry event: {event}"
    );
}

/// Require the call to return before a retry event past `retries`, with the
/// transport failure itself rather than any deadline or cancellation, and
/// with exactly `retries` retry events recorded.
async fn assert_transport_failure_after(
    call: Call,
    capture: TraceCapture,
    retries: usize,
    context: &str,
) {
    let last = next_progress(&call, &capture, retries + 1).await;
    assert!(
        matches!(last, Progress::Returned),
        "{context}: after {retries} retry events the call showed {last:?}"
    );
    let result = settled(call).await;
    settle_wakeups().await;
    let recorded = capture.len();
    drop(capture);

    assert!(
        matches!(result, Some(Err(RuntimeError::Http(_)))),
        "{context}: returned {} instead of the transport failure",
        describe(&result)
    );
    assert_eq!(
        recorded, retries,
        "{context}: {recorded} retry events were recorded"
    );
}

/// One opted-in row: every refused send but the last is followed by one retry
/// event carrying its attempt number, and the clock crosses each backoff only
/// after that event.
async fn assert_connect_failure_retries(method: &'static str) {
    let context = format!("{method} to a refused port, opted in");
    let capture = capture_events(RETRY_EVENT);
    let call = spawn_unsafe(connect_retry_client(true), method, REFUSED_URL);

    for attempt in 1..=CONNECT_RETRIES {
        match next_progress(&call, &capture, attempt as usize).await {
            Progress::Event => {}
            Progress::Returned => panic!(
                "{context}: the call returned {} before retry event {attempt}",
                describe(&settled(call).await)
            ),
            Progress::Stalled => panic!("{context}: retry event {attempt} never arrived"),
        }
        assert_retry_event(&capture, method, attempt, &context);
        tokio::time::advance(past_backoff(attempt - 1)).await;
    }
    assert_transport_failure_after(call, capture, CONNECT_RETRIES as usize, &context).await;
}

/// One row without the opt-in: the first refused send is the answer, and no
/// retry event is ever recorded.
async fn assert_connect_failure_is_once(method: &'static str) {
    let context = format!("{method} to a refused port, not opted in");
    let capture = capture_events(RETRY_EVENT);
    let call = spawn_unsafe(connect_retry_client(false), method, REFUSED_URL);
    assert_transport_failure_after(call, capture, 0, &context).await;
}

/// 4.T4
///
/// Runs in a private child, because the capture it reads is the process's one
/// global subscriber.
#[test]
fn unsafe_connect_failure_retries_through_the_public_client() {
    crate::process::run_in_child(
        CONNECT_RETRY_TEST,
        CONNECT_RETRY_MODE,
        CONNECT_RETRY_MARKER,
        CONNECT_RETRY_CHILD_BOUND,
        || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .start_paused(true)
                .build()
                .unwrap();
            runtime.block_on(async {
                for &method in UNSAFE_METHODS {
                    assert_connect_failure_retries(method).await;
                    assert_connect_failure_is_once(method).await;
                }
            });
        },
    );
}
