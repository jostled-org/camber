//! Outbound 4.T1 and 5.T2 beside the retained 2.T1–2.T3, 4.T2 and 4.T4 rows:
//! one operation, one absolute deadline, one cancellation, and the counted
//! delays between its attempts.
//!
//! Every row runs on a paused current-thread runtime against a scripted raw
//! upstream or a refused port. The clock moves only after the client future has
//! been polled and the peer or the production event has acknowledged the phase
//! the client is parked in, so each deadline a row crosses is one the row
//! placed, not one elapsed wall time happened to reach.

use crate::retry_policy::{Draft, METHODS, PERMISSIONS, Permission, spawn_draft};
use crate::retry_upstream::{
    Answer, CLIENT_METHODS, Call, PeerEvent, RunnableDriver, ScriptedUpstream, TIMER_ROUNDING,
    advance_to, assert_released, describe, settle_wakeups, settled, spawn_get,
    wait_for_body_collection, within_watchdog,
};
use crate::trace_capture::{TraceCapture, assert_field_value, capture_events};

use camber::http;
use camber::http::Client;
use camber::http::ClientError;
use camber::http::ClientResponse;
use camber::http::ClientTimeout;
use camber::http::mock::{self, ScopedTransferOwner};
use camber::runtime_test_support::install_runtime_context;
use camber::{RuntimeError, runtime};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::time::Instant;

/// The one route every operation-lifetime row requests.
const RETRY_PATH: &str = "/retry-lifetime";

/// The configured bound on a whole operation.
const OPERATION_TOTAL: Duration = Duration::from_secs(5);

/// Every per-attempt bound, set longer than the operation it sits inside so
/// only the operation deadline can end a stalled attempt.
const ATTEMPT_LIMIT: Duration = Duration::from_secs(60);

/// The base backoff of rows that cross a delay on their way to a later phase.
///
/// The first delay is below twice this, and twice this is below the operation
/// deadline, so one step onto that bound finishes the delay and nothing else.
const CROSSED_BACKOFF: Duration = Duration::from_secs(1);

/// The base backoff of the row parked inside a delay: longer than the whole
/// operation, so the deadline is what ends it.
const PARKED_BACKOFF: Duration = Duration::from_secs(60);

/// The documented operation bound a client starts with.
const DEFAULT_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);

/// The floor every infallible client deadline setter clamps to.
const CLAMP_FLOOR: Duration = Duration::from_millis(1);

/// The retry count of every row that has retries to spare at its phase.
const SPARE_RETRIES: u32 = 3;

/// The failure a row requires of a call that ended without an answer.
#[derive(Clone, Copy, Debug)]
enum Ending {
    /// The operation's own deadline ended it.
    OperationDeadline,
    /// Runtime shutdown ended it.
    Cancelled,
    /// The attempt timer Camber owns ended it.
    AttemptTimeout,
    /// A timer the transport owns ended it.
    TransportTimeout,
}

impl Ending {
    fn ended(self, result: &Option<Result<ClientResponse, RuntimeError>>) -> bool {
        let timeout = match (self, result) {
            (Self::Cancelled, Some(Err(RuntimeError::Cancelled))) => return true,
            (_, Some(Err(RuntimeError::Client(ClientError::Timeout(timeout))))) => *timeout,
            _ => return false,
        };
        matches!(
            (self, timeout),
            (Self::OperationDeadline, ClientTimeout::Operation)
                | (Self::AttemptTimeout, ClientTimeout::Attempt)
                | (Self::TransportTimeout, ClientTimeout::Transport)
        )
    }

    /// The step past a bound this ending needs before its timer has fired.
    ///
    /// The transport arms a relative sleep, and Tokio rounds that up to the
    /// next millisecond. The timers Camber owns are stepped onto exactly.
    fn rounding(self) -> Duration {
        match self {
            Self::TransportTimeout => TIMER_ROUNDING,
            Self::OperationDeadline | Self::Cancelled | Self::AttemptTimeout => Duration::ZERO,
        }
    }
}

/// Where one row parks the operation before crossing its deadline or accepting
/// its cancellation.
#[derive(Clone, Copy, Debug)]
enum Phase {
    /// Waiting for the first attempt's response head.
    InitialHead,
    /// Collecting the first attempt's final body.
    InitialBody,
    /// Waiting for a later attempt's response head, after a transient answer.
    LaterHead,
    /// Collecting a later attempt's final body, after a transient answer.
    LaterBody,
    /// Inside the configured delay after a transient answer.
    Backoff,
    /// Inside the delay a transient answer's `Retry-After` stated.
    StatedDelay,
}

impl Phase {
    fn script(self) -> &'static [Answer] {
        match self {
            Self::InitialHead => &[Answer::StallHead],
            Self::InitialBody => &[Answer::StallBody],
            Self::LaterHead => &[Answer::Transient, Answer::StallHead],
            Self::LaterBody => &[Answer::Transient, Answer::StallBody],
            Self::Backoff => &[Answer::Transient],
            Self::StatedDelay => &[Answer::TransientRetryAfter(LARGE_DELTA)],
        }
    }

    /// The configured base backoff. Only [`Phase::Backoff`] is parked on it:
    /// the stated delay parks its own row whatever this is.
    fn backoff(self) -> Duration {
        match self {
            Self::Backoff => PARKED_BACKOFF,
            Self::InitialHead
            | Self::InitialBody
            | Self::LaterHead
            | Self::LaterBody
            | Self::StatedDelay => CROSSED_BACKOFF,
        }
    }

    /// The attempt whose transport is still open while the operation is
    /// parked, or `None` when the operation is parked between attempts.
    fn in_flight(self) -> Option<usize> {
        match self {
            Self::InitialHead | Self::InitialBody => Some(0),
            Self::LaterHead | Self::LaterBody => Some(1),
            Self::Backoff | Self::StatedDelay => None,
        }
    }

    /// Every request start the parked operation has made, and all it may make.
    fn starts(self) -> u32 {
        match self {
            Self::InitialHead | Self::InitialBody | Self::Backoff | Self::StatedDelay => 1,
            Self::LaterHead | Self::LaterBody => 2,
        }
    }

    /// Drive the operation into this phase and wait for the peer to confirm
    /// it.
    ///
    /// A crossed delay is stepped over only once the peer has seen the
    /// transient answer disposed, which is when the delay has begun.
    async fn reach(self, upstream: &mut ScriptedUpstream, context: &str) {
        upstream.expect(PeerEvent::Started(0), context).await;
        match self {
            Self::InitialHead | Self::InitialBody => {
                upstream.expect(PeerEvent::Stalled(0), context).await;
            }
            Self::Backoff | Self::StatedDelay => {
                upstream.expect(PeerEvent::Disposed(0), context).await;
            }
            Self::LaterHead | Self::LaterBody => {
                upstream.expect(PeerEvent::Disposed(0), context).await;
                tokio::time::advance(2 * CROSSED_BACKOFF).await;
                upstream
                    .expect_each(&[PeerEvent::Started(1), PeerEvent::Stalled(1)], context)
                    .await;
            }
        }
    }
}

/// One phase and the retry count the operation parked in it was given.
#[derive(Clone, Copy, Debug)]
struct Parked {
    phase: Phase,
    retries: u32,
}

impl Parked {
    const fn at(phase: Phase, retries: u32) -> Self {
        Self { phase, retries }
    }

    /// A client whose only bound short enough to matter is the operation's
    /// own.
    fn client(self) -> Client {
        operation_client(self.retries, self.phase.backoff())
    }
}

/// Every phase an operation can be parked in.
///
/// A first attempt is parked with no retries and with retries to spare. A
/// later attempt or a delay exists only with retries.
const PARKED: [Parked; 8] = [
    Parked::at(Phase::InitialHead, 0),
    Parked::at(Phase::InitialBody, 0),
    Parked::at(Phase::InitialHead, SPARE_RETRIES),
    Parked::at(Phase::InitialBody, SPARE_RETRIES),
    Parked::at(Phase::LaterHead, SPARE_RETRIES),
    Parked::at(Phase::LaterBody, SPARE_RETRIES),
    Parked::at(Phase::Backoff, SPARE_RETRIES),
    Parked::at(Phase::StatedDelay, SPARE_RETRIES),
];

/// Give `client` connect, attempt, and idle bounds that outlast every row.
fn long_attempts(client: Client) -> Client {
    client
        .connect_timeout(ATTEMPT_LIMIT)
        .request_timeout(ATTEMPT_LIMIT)
        .response_idle_timeout(ATTEMPT_LIMIT)
}

/// A client whose connect, attempt, and idle bounds all outlast every row, and
/// whose operation bound is the default.
fn long_attempt_client() -> Client {
    long_attempts(http::client())
}

/// A client whose only bound short enough to matter is the operation's own.
fn operation_client(retries: u32, backoff: Duration) -> Client {
    long_attempts(http::client().total_timeout(OPERATION_TOTAL))
        .retries(retries)
        .backoff(backoff)
}

/// One GET in flight against a scripted upstream, with the instant it entered
/// the client and the driver that holds the paused clock still.
struct OperationRow {
    driver: RunnableDriver,
    upstream: ScriptedUpstream,
    entry: Instant,
    call: Call,
    observed: ScopedTransferOwner,
}

impl OperationRow {
    /// Bind an upstream that answers from `script`, then start one GET through
    /// `client` against it.
    async fn start(script: &[Answer], client: Client) -> Self {
        Self::started(script, |url| spawn_get(client, url)).await
    }

    /// Bind an upstream that answers from `script`, then start `draft` through
    /// `client` against it.
    async fn start_draft(script: &[Answer], client: Client, draft: Draft) -> Self {
        Self::started(script, |url| spawn_draft(client, draft, url)).await
    }

    /// Bind an upstream that answers from `script`, then start the call `call`
    /// makes for its URL.
    async fn started(script: &[Answer], call: impl FnOnce(String) -> Call) -> Self {
        let driver = RunnableDriver::start();
        let upstream = ScriptedUpstream::bind(script).await;
        let observed = mock::transfer_owner(upstream.addr()).expect("one observer per upstream");
        let entry = Instant::now();
        let call = call(upstream.url(RETRY_PATH));
        Self {
            driver,
            upstream,
            entry,
            call,
            observed,
        }
    }

    /// Start `phase`'s script through `client` and drive the call into that
    /// phase.
    async fn parked(phase: Phase, client: Client, context: &str) -> Self {
        let mut row = Self::start(phase.script(), client).await;
        phase.reach(&mut row.upstream, context).await;
        if matches!(phase, Phase::InitialBody | Phase::LaterBody) {
            // The clock is held still. This current-thread call reaches its
            // next pending read before the observer can resume the test.
            wait_for_body_collection(&row.observed)
                .await
                .unwrap_or_else(|error| panic!("{context}: {error}"));
        }
        row
    }

    /// A single-retry operation with `backoff` whose first attempt answered
    /// transient with `Retry-After: value` and was disposed, so the call now
    /// waits on the delay before its one retry.
    ///
    /// The retry is the request's own: its client is configured with none.
    async fn after_retry_after(value: &'static str, backoff: Duration, context: &str) -> Self {
        let mut row = Self::start_draft(
            &[Answer::TransientRetryAfter(value), Answer::Complete],
            operation_client(0, backoff),
            Draft::new("GET", Permission::Defaulted).counted(1),
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

/// Step the clock past every delay and deadline an ended operation held.
///
/// An owner that outlived the ending would start its next attempt here. The
/// upstream counts that start and holds its report, so the row fails at its
/// cleanup.
async fn step_past_every_held_timer(phase: Phase) {
    tokio::time::advance(DEFAULT_TOTAL_TIMEOUT + 2 * phase.backoff()).await;
    settle_wakeups().await;
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

/// What ends one parked operation, and the endings that ending admits.
#[derive(Clone, Copy, Debug)]
enum Trigger {
    /// Step onto the deadline fixed at call entry. The step lands on call
    /// entry plus the total timeout, not on the parked attempt's start plus
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
            Self::Deadline => &[Ending::OperationDeadline],
            Self::Shutdown => &[Ending::Cancelled],
            Self::ExpiryBesideShutdown => &[Ending::OperationDeadline, Ending::Cancelled],
        }
    }
}

/// One parked row: park the operation, apply `trigger`, and require an ending
/// it admits, with every held peer released and nothing sent after it.
///
/// A row that requests shutdown installs the runtime context that request
/// reaches, and holds it until the fixture is released. A row ended by its
/// deadline alone then steps past every timer the operation held, so a send
/// after the committed expiry would reach the peer.
async fn assert_parked_ends(parked: Parked, trigger: Trigger) {
    let context = format!("{trigger:?} while parked at {parked:?}");
    let phase = parked.phase;
    let runtime_context = trigger.requests_shutdown().then(install_runtime_context);
    let OperationRow {
        driver,
        mut upstream,
        entry,
        call,
        ..
    } = OperationRow::parked(phase, parked.client(), &context).await;

    if trigger.requests_shutdown() {
        runtime::request_shutdown();
    }
    if trigger.crosses_deadline() {
        advance_to(entry + OPERATION_TOTAL).await;
    }
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    let released = released_in_flight(phase, &mut upstream).await;
    if !trigger.requests_shutdown() {
        step_past_every_held_timer(phase).await;
    }
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

/// 2.T1, revised for one operation owner: the deadline encloses every phase,
/// with no retries and with retries to spare.
#[tokio::test(start_paused = true)]
async fn retry_deadline_encloses_every_pending_phase() {
    for parked in PARKED {
        assert_parked_ends(parked, Trigger::Deadline).await;
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

/// One caller-drop row: park the operation, drop the call, and require the
/// transport and every owner the call made released with no attempt after the
/// drop.
///
/// Dropping the call is ordinary caller cancellation, not a runtime shutdown,
/// so no result is owed; the evidence is what the peer and the runtime observe.
/// The request owner is read off the runtime before the clock moves: an
/// owner the drop left behind — a detached operation, a connection driver the
/// call spawned — is still parked on its deadline or its socket then, so it is
/// still an alive task beside the fixture's own. The clock is then stepped
/// past every delay and deadline the operation held, so an owner that outlived
/// the drop would have had its chance to send.
async fn assert_caller_drop_releases(parked: Parked) {
    let context = format!("caller drop while parked at {parked:?}");
    let phase = parked.phase;
    let OperationRow {
        driver,
        mut upstream,
        call,
        ..
    } = OperationRow::parked(phase, parked.client(), &context).await;

    call.abort();
    let joined = within_watchdog(call).await;
    let released = released_in_flight(phase, &mut upstream).await;
    settle_wakeups().await;
    let alive_after_drop = alive_tasks();
    step_past_every_held_timer(phase).await;
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

/// 2.T2, revised for one operation owner: shutdown and caller drop end every
/// phase, with no retries and with retries to spare.
#[tokio::test(start_paused = true)]
async fn retry_shutdown_cancels_each_pending_phase() {
    for trigger in [Trigger::Shutdown, Trigger::ExpiryBesideShutdown] {
        for parked in PARKED {
            assert_parked_ends(parked, trigger).await;
        }
    }
    for parked in PARKED {
        assert_caller_drop_releases(parked).await;
    }
}

/// Step to `slack` before `deadline`, let every woken task run, and report
/// whether the call is still pending there.
///
/// The step is measured from call entry by the caller's `deadline`, so a row
/// proves where a bound sits rather than only that it is eventually crossed.
async fn pending_short_of(call: &Call, deadline: Instant, slack: Duration) -> bool {
    advance_to(deadline - slack).await;
    settle_wakeups().await;
    !call.is_finished()
}

/// Report whether the call is still pending one tick before `deadline`.
async fn pending_just_before(call: &Call, deadline: Instant) -> bool {
    pending_short_of(call, deadline, CLAMP_FLOOR).await
}

/// Report whether the call is still pending one tick before `deadline`, then
/// step onto `deadline`.
async fn pending_until(call: &Call, deadline: Instant) -> bool {
    let pending = pending_just_before(call, deadline).await;
    advance_to(deadline).await;
    pending
}

/// Park the first attempt at `phase`, step to one tick before `bound`, require
/// the call still pending, then step onto `bound` and require `expected`.
///
/// Both steps are measured from call entry, so the row proves where the bound
/// sits rather than only that it is eventually crossed. The clock is then
/// stepped past every timer the operation held, so a retry the ending left
/// scheduled would reach the peer.
async fn assert_first_attempt_ends_at(
    phase: Phase,
    client: Client,
    bound: Duration,
    expected: Ending,
    context: &str,
) {
    let OperationRow {
        driver,
        mut upstream,
        entry,
        call,
        ..
    } = OperationRow::parked(phase, client, context).await;

    let pending_before_bound = pending_until(&call, entry + bound).await;
    tokio::time::advance(expected.rounding()).await;
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    let released = released_in_flight(phase, &mut upstream).await;
    step_past_every_held_timer(phase).await;
    let starts = upstream.finish(driver, context).await;

    assert!(
        pending_before_bound,
        "{context}: the call ended before {bound:?}"
    );
    assert!(
        expected.ended(&result),
        "{context}: returned {}, not {expected:?}",
        describe(&result)
    );
    assert_cleanup(phase, released, starts, context);
}

/// The two phases a first attempt can be parked in.
const FIRST_ATTEMPT: [Phase; 2] = [Phase::InitialHead, Phase::InitialBody];

/// The operation total bounds a first attempt wherever it is parked: at the
/// default, at a written value, and at the floor a zero or sub-millisecond
/// write is clamped to.
///
/// Every attempt bound outlasts the row, so only the operation timer can end
/// it. Zero retries reaches the same owner as retries to spare.
async fn assert_total_bounds_the_first_attempt(retries: u32) {
    for phase in FIRST_ATTEMPT {
        let defaulted = long_attempt_client()
            .retries(retries)
            .backoff(CROSSED_BACKOFF);
        assert_first_attempt_ends_at(
            phase,
            defaulted,
            DEFAULT_TOTAL_TIMEOUT,
            Ending::OperationDeadline,
            &format!("default total at {phase:?} with {retries} retries"),
        )
        .await;

        assert_first_attempt_ends_at(
            phase,
            operation_client(retries, CROSSED_BACKOFF),
            OPERATION_TOTAL,
            Ending::OperationDeadline,
            &format!("written total at {phase:?} with {retries} retries"),
        )
        .await;
    }

    for (written, label) in [
        (Duration::ZERO, "zero total"),
        (Duration::from_micros(500), "sub-millisecond total"),
    ] {
        let clamped = long_attempts(http::client().total_timeout(written))
            .retries(retries)
            .backoff(CROSSED_BACKOFF);
        assert_first_attempt_ends_at(
            Phase::InitialHead,
            clamped,
            CLAMP_FLOOR,
            Ending::OperationDeadline,
            &format!("{label} with {retries} retries"),
        )
        .await;
    }
}

/// The bound each inner-timer row places, inside the operation total.
const INNER_BOUND: Duration = Duration::from_secs(1);

/// Each of the three timers names itself.
///
/// The operation total is the same in every row and outlasts the inner bound.
/// The attempt timer is Camber's and runs from the send through the end of the
/// body. The quiet interval is the transport's. A stalled body ends with its
/// own timer's name with retries to spare too: a final-body failure is never
/// sent again.
async fn assert_each_timer_names_itself() {
    for phase in FIRST_ATTEMPT {
        assert_first_attempt_ends_at(
            phase,
            operation_client(0, CROSSED_BACKOFF).request_timeout(INNER_BOUND),
            INNER_BOUND,
            Ending::AttemptTimeout,
            &format!("attempt timer at {phase:?} with no retries"),
        )
        .await;
        assert_first_attempt_ends_at(
            phase,
            operation_client(0, CROSSED_BACKOFF).response_idle_timeout(INNER_BOUND),
            INNER_BOUND,
            Ending::TransportTimeout,
            &format!("transport timer at {phase:?} with no retries"),
        )
        .await;
    }
    assert_first_attempt_ends_at(
        Phase::InitialBody,
        operation_client(SPARE_RETRIES, CROSSED_BACKOFF).request_timeout(INNER_BOUND),
        INNER_BOUND,
        Ending::AttemptTimeout,
        "attempt timer in the final body with retries to spare",
    )
    .await;
}

/// Send one GET with an absolute request deadline, under a client whose only
/// other bound short enough to matter is the operation total.
async fn get_until(
    url: String,
    retries: u32,
    deadline: std::time::Instant,
) -> Result<ClientResponse, RuntimeError> {
    operation_client(retries, CROSSED_BACKOFF)
        .request("GET", &url)?
        .deadline(deadline)
        .send()
        .await
}

/// A request deadline before the operation total, as a distance from now.
const EARLIER_DEADLINE: Duration = Duration::from_secs(3);

/// A request deadline past the operation total, as a distance from now.
const LATER_DEADLINE: Duration = Duration::from_secs(60);

/// How far before a request deadline a row still requires the call pending.
///
/// The deadline is a standard-clock instant, converted once when the call is
/// first polled. Real time passes between the row reading that clock and the
/// conversion, so the converted deadline sits slightly before the distance
/// the row wrote. The slack is far above that drift and far below the
/// distance.
const DEADLINE_SLACK: Duration = Duration::from_secs(1);

/// A request deadline already behind the standard clock.
fn expired_deadline() -> std::time::Instant {
    let now = std::time::Instant::now();
    now.checked_sub(Duration::from_secs(1)).unwrap_or(now)
}

/// A deadline already expired at entry ends the operation there, and nothing
/// is sent.
///
/// The clock is never stepped before the result: the operation ends at entry,
/// not at a timer. It is stepped afterwards, past every bound the client
/// holds, so a send the expiry failed to prevent would reach the peer.
async fn assert_expired_deadline_sends_nothing(retries: u32) {
    let context = format!("expired request deadline with {retries} retries");
    let OperationRow {
        driver,
        upstream,
        call,
        ..
    } = OperationRow::started(&[], |url| {
        tokio::spawn(get_until(url, retries, expired_deadline()))
    })
    .await;

    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    step_past_every_held_timer(Phase::InitialHead).await;
    let starts = upstream.finish(driver, &context).await;

    assert!(
        Ending::OperationDeadline.ended(&result),
        "{context}: returned {}",
        describe(&result)
    );
    assert_eq!(starts, 0, "{context}: the call started {starts} requests");
}

/// One request deadline, and the bound it leaves the operation under.
#[derive(Clone, Copy, Debug)]
struct RequestDeadline {
    label: &'static str,
    /// The deadline the request writes, as a distance from now.
    written: Duration,
    /// Where the operation ends, as a distance from call entry.
    bound: Duration,
    /// How far before `bound` the row still requires the call pending.
    slack: Duration,
}

/// A request deadline on either side of the operation total.
///
/// A deadline before the total is the operation's ceiling. The call is still
/// pending well inside the written distance and has ended once the clock
/// stands on it, two seconds before the total. A deadline past the total
/// cannot extend it, and the total is a Tokio instant the row steps onto
/// exactly.
const REQUEST_DEADLINES: [RequestDeadline; 2] = [
    RequestDeadline {
        label: "earlier request deadline",
        written: EARLIER_DEADLINE,
        bound: EARLIER_DEADLINE,
        slack: DEADLINE_SLACK,
    },
    RequestDeadline {
        label: "later request deadline",
        written: LATER_DEADLINE,
        bound: OPERATION_TOTAL,
        slack: CLAMP_FLOOR,
    },
];

/// One request-deadline row: the call is pending `slack` before its bound, and
/// ends with the operation's own name once the clock stands on that bound.
async fn assert_request_deadline_bounds(row: RequestDeadline, retries: u32) {
    const PHASE: Phase = Phase::InitialHead;

    let RequestDeadline {
        label,
        written,
        bound,
        slack,
    } = row;
    let context = format!("{label} with {retries} retries");
    let OperationRow {
        driver,
        mut upstream,
        entry,
        call,
        ..
    } = OperationRow::started(PHASE.script(), |url| {
        tokio::spawn(get_until(url, retries, std::time::Instant::now() + written))
    })
    .await;
    PHASE.reach(&mut upstream, &context).await;

    let pending_inside_bound = pending_short_of(&call, entry + bound, slack).await;
    advance_to(entry + bound).await;
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    let released = released_in_flight(PHASE, &mut upstream).await;
    let starts = upstream.finish(driver, &context).await;

    assert!(
        pending_inside_bound,
        "{context}: the call ended {slack:?} or more before its bound at {bound:?}"
    );
    assert!(
        Ending::OperationDeadline.ended(&result),
        "{context}: returned {}",
        describe(&result)
    );
    assert_cleanup(PHASE, released, starts, &context);
}

/// How a first-poll row makes its call before the clock moves.
#[derive(Clone, Copy, Debug)]
enum Made {
    /// A verb convenience, called and held unpolled.
    UnpolledConvenience,
    /// A request described through `Client::request`, with `send` not yet
    /// called. The description copies the client's operation settings here,
    /// two totals before the first poll.
    Described,
}

impl Made {
    /// Make the call, hold it until `polled` resolves, then run it.
    async fn held(
        self,
        client: Client,
        url: String,
        polled: oneshot::Receiver<()>,
    ) -> Result<ClientResponse, RuntimeError> {
        match self {
            Self::UnpolledConvenience => {
                let unpolled = client.get(&url);
                // A dropped sender polls the call too: that row has already
                // failed.
                let _ = polled.await;
                unpolled.await
            }
            Self::Described => {
                let described = client.request("GET", &url)?;
                let _ = polled.await;
                described.send().await
            }
        }
    }
}

/// The operation begins when the call is first polled, not when it is made.
///
/// The call is made and held unpolled while the clock moves two whole totals.
/// A deadline fixed when the call was made or described would have expired
/// there and sent nothing. Polled, the call sends, and its deadline sits one
/// total after that first poll.
async fn assert_operation_starts_at_first_poll(made: Made, retries: u32) {
    const PHASE: Phase = Phase::InitialHead;

    let context = format!("first poll after {made:?} with {retries} retries");
    let client: Client = operation_client(retries, CROSSED_BACKOFF);
    let (poll, polled) = oneshot::channel::<()>();
    // The row's own entry is the first poll, read once the clock has moved.
    let OperationRow {
        driver,
        mut upstream,
        call,
        ..
    } = OperationRow::started(PHASE.script(), |url| {
        tokio::spawn(made.held(client, url, polled))
    })
    .await;

    settle_wakeups().await;
    tokio::time::advance(2 * OPERATION_TOTAL).await;
    settle_wakeups().await;
    let ended_unpolled = call.is_finished();
    let entry = Instant::now();
    let released_to_poll = poll.send(()).is_ok();
    PHASE.reach(&mut upstream, &context).await;

    let pending_before_total = pending_until(&call, entry + OPERATION_TOTAL).await;
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    let released = released_in_flight(PHASE, &mut upstream).await;
    let starts = upstream.finish(driver, &context).await;

    assert!(!ended_unpolled, "{context}: the unpolled call ended");
    assert!(released_to_poll, "{context}: the call task was gone");
    assert!(
        pending_before_total,
        "{context}: the call ended before one total after its first poll"
    );
    assert!(
        Ending::OperationDeadline.ended(&result),
        "{context}: returned {}",
        describe(&result)
    );
    assert_cleanup(PHASE, released, starts, &context);
}

/// The canned body of the mock an expired deadline must not reach.
const MOCK_BODY: &str = "mock-answer";

/// A mocked call enters the operation, so an expired deadline ends it before
/// the mock answers.
///
/// The same request under a deadline still ahead is answered by the mock, so
/// the registration matches the refused request too. The mock is called once
/// in all: an expired call it answered would make that count two.
async fn assert_expired_deadline_precedes_a_mock(retries: u32) {
    let context = format!("mocked call past its deadline with {retries} retries");
    let url = format!("http://retry-lifetime.mock.test/expired-{retries}");
    let canned = http::Response::text(200, MOCK_BODY).expect("a valid mock status");
    let registered = mock::http(&url).returns(canned);

    let refused: Option<Result<ClientResponse, RuntimeError>> =
        Some(get_until(url.clone(), retries, expired_deadline()).await);
    let answered: Result<ClientResponse, RuntimeError> =
        get_until(url, retries, std::time::Instant::now() + EARLIER_DEADLINE).await;

    assert!(
        Ending::OperationDeadline.ended(&refused),
        "{context}: returned {}",
        describe(&refused)
    );
    let response: ClientResponse = answered.expect("the mock answers a call inside its deadline");
    assert_eq!(
        response.body_bytes(),
        MOCK_BODY.as_bytes(),
        "{context}: the mock's own answer must arrive"
    );
    registered.assert_called_once();
}

/// Outbound 4.T1: one operation deadline encloses a call with no retries
/// exactly as it encloses one with retries.
#[tokio::test(start_paused = true)]
async fn operation_deadline_encloses_zero_and_nonzero_retry_calls() {
    assert_each_timer_names_itself().await;
    for retries in [0, SPARE_RETRIES] {
        assert_total_bounds_the_first_attempt(retries).await;
        assert_expired_deadline_sends_nothing(retries).await;
        for row in REQUEST_DEADLINES {
            assert_request_deadline_bounds(row, retries).await;
        }
        assert_expired_deadline_precedes_a_mock(retries).await;
        for made in [Made::UnpolledConvenience, Made::Described] {
            assert_operation_starts_at_first_poll(made, retries).await;
        }
    }
}

/// 2.T3: a per-attempt timeout that commits before the operation deadline
/// keeps its own result, and that result is still eligible for another attempt.
#[tokio::test(start_paused = true)]
async fn attempt_timeout_inside_the_retry_deadline_stays_retryable() {
    const CONTEXT: &str = "attempt timeout inside the operation deadline";
    const ATTEMPT: Duration = Duration::from_secs(1);

    let client = operation_client(2, CROSSED_BACKOFF).request_timeout(ATTEMPT);
    let OperationRow {
        driver,
        mut upstream,
        call,
        ..
    } = OperationRow::start(&[Answer::StallHead, Answer::Complete], client).await;
    upstream
        .expect_each(&[PeerEvent::Started(0), PeerEvent::Stalled(0)], CONTEXT)
        .await;

    tokio::time::advance(ATTEMPT).await;
    upstream.expect(PeerEvent::Released(0), CONTEXT).await;
    tokio::time::advance(2 * CROSSED_BACKOFF).await;
    upstream.finish_answered(1, call, driver, CONTEXT).await;
}

/// The last IMF-fixdate the grammar can name: no configured operation lasts
/// until it.
const FAR_FUTURE_FIXDATE: &str = "Fri, 31 Dec 9999 23:59:59 GMT";

/// The largest delta-seconds a `u64` second count holds.
const LARGE_DELTA: &str = "18446744073709551615";

/// An IMF-fixdate already behind every wall clock this row runs under.
const PAST_FIXDATE: &str = "Sun, 06 Nov 1994 08:49:37 GMT";

/// A value in neither `Retry-After` form.
const INVALID_RETRY_AFTER: &str = "soon";

/// A valid wait inside the operation and shorter than the parked backoff, as
/// delta-seconds and as the duration it names.
const STATED_DELTA: &str = "2";
const STATED_DELAY: Duration = Duration::from_secs(2);

/// One clipped row: a stated wait past the operation deadline parks the
/// operation until that deadline, which ends it with no second attempt.
///
/// The configured backoff is short, so an operation that ignored the stated
/// wait would have started its second attempt long before the step one tick
/// short of the deadline.
async fn assert_retry_after_clipped(value: &'static str) {
    let context = format!("Retry-After {value:?} past the operation deadline");
    let OperationRow {
        driver,
        upstream,
        entry,
        call,
        ..
    } = OperationRow::after_retry_after(value, CROSSED_BACKOFF, &context).await;

    let pending_before_deadline = pending_until(&call, entry + OPERATION_TOTAL).await;
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    let starts = upstream.finish(driver, &context).await;

    assert!(
        pending_before_deadline,
        "{context}: the call ended before the operation deadline"
    );
    assert!(
        Ending::OperationDeadline.ended(&result),
        "{context}: returned {}",
        describe(&result)
    );
    assert_eq!(starts, 1, "{context}: the call started {starts} requests");
}

/// One invalid row: a value in neither form leaves the configured backoff in
/// force, so the second attempt waits for it and then answers.
async fn assert_invalid_retry_after_backs_off() {
    let context = format!("Retry-After {INVALID_RETRY_AFTER:?}");
    let OperationRow {
        driver,
        upstream,
        entry,
        call,
        ..
    } = OperationRow::after_retry_after(INVALID_RETRY_AFTER, CROSSED_BACKOFF, &context).await;

    let pending_inside_backoff = pending_just_before(&call, entry + CROSSED_BACKOFF).await;
    advance_to(entry + 2 * CROSSED_BACKOFF).await;
    upstream.finish_answered(1, call, driver, &context).await;

    assert!(
        pending_inside_backoff,
        "{context}: the call ended inside the configured backoff"
    );
}

/// One immediate row: a zero or past wait replaces a configured backoff
/// longer than the whole operation, so the second attempt starts with the
/// clock held still.
async fn assert_retry_after_immediate(value: &'static str) {
    let context = format!("Retry-After {value:?} permitting an immediate retry");
    let OperationRow {
        driver,
        upstream,
        call,
        ..
    } = OperationRow::after_retry_after(value, PARKED_BACKOFF, &context).await;
    upstream.finish_answered(1, call, driver, &context).await;
}

/// One stated row: a valid wait inside the operation replaces a configured
/// backoff longer than the operation, and the second attempt starts exactly
/// when that wait ends.
async fn assert_retry_after_replaces_backoff() {
    let context = format!("Retry-After {STATED_DELTA:?} replacing the backoff");
    let OperationRow {
        driver,
        upstream,
        entry,
        call,
        ..
    } = OperationRow::after_retry_after(STATED_DELTA, PARKED_BACKOFF, &context).await;

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

/// A complete transient answer that permits an immediate retry.
///
/// It declares no body, so a call with no send left collects it as its answer.
const FINAL_TRANSIENT: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nRetry-After: 0\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

/// One counted row: a client count, a request, and every send they allow.
#[derive(Clone, Copy, Debug)]
struct Allowance {
    client_count: u32,
    draft: Draft,
    /// The sends the row makes before its count or its permission is spent.
    sends: usize,
}

/// A request's own count replaces its client's in both directions, a zero
/// count sends once, and the largest count adds no send a permission refuses.
const ALLOWANCES: [Allowance; 6] = [
    Allowance {
        client_count: 0,
        draft: Draft::new("GET", Permission::Defaulted),
        sends: 1,
    },
    Allowance {
        client_count: 0,
        draft: Draft::new("GET", Permission::Defaulted).counted(1),
        sends: 2,
    },
    Allowance {
        client_count: SPARE_RETRIES,
        draft: Draft::new("GET", Permission::Defaulted).counted(0),
        sends: 1,
    },
    Allowance {
        client_count: 0,
        draft: Draft::new("POST", Permission::Replayable).counted(2),
        sends: 3,
    },
    Allowance {
        client_count: u32::MAX,
        draft: Draft::new("POST", Permission::Defaulted),
        sends: 1,
    },
    Allowance {
        client_count: SPARE_RETRIES,
        draft: Draft::new("GET", Permission::Never).counted(u32::MAX),
        sends: 1,
    },
];

/// One counted row: every answer is transient and permits an immediate retry,
/// so the call sends until its count or its permission is spent.
///
/// The configured backoff outlasts the operation and the clock is held still,
/// so each send the peer reports was allowed by the count, not by a delay that
/// ended. The answer past the last allowed send is a complete 200: a send too
/// many returns it and is counted.
async fn assert_allowance_is_exact(row: Allowance) {
    let context = format!("{row:?}");
    let repeats = row.sends - 1;
    let script: Box<[Answer]> = std::iter::repeat_n(Answer::TransientRetryAfter("0"), repeats)
        .chain([Answer::Exact(FINAL_TRANSIENT), Answer::Complete])
        .collect();
    let OperationRow {
        driver,
        mut upstream,
        call,
        ..
    } = OperationRow::start_draft(
        &script,
        operation_client(row.client_count, PARKED_BACKOFF),
        row.draft,
    )
    .await;

    for attempt in 0..repeats {
        upstream
            .expect_each(
                &[PeerEvent::Started(attempt), PeerEvent::Disposed(attempt)],
                &context,
            )
            .await;
    }
    upstream
        .expect_each(
            &[PeerEvent::Started(repeats), PeerEvent::Answered(repeats)],
            &context,
        )
        .await;
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    step_past_every_held_timer(Phase::Backoff).await;
    let starts = upstream.finish(driver, &context).await;

    assert!(
        matches!(&result, Some(Ok(response)) if response.status() == 503),
        "{context}: returned {}, not the last transient answer",
        describe(&result)
    );
    assert_eq!(
        usize::try_from(starts).unwrap(),
        row.sends,
        "{context}: the call started {starts} requests"
    );
}

/// How many transient answers a largest-count row is given before its answer.
const COUNTED_REPEATS: usize = 2;

/// The largest count is spent one send at a time and never wraps.
///
/// One initial send plus `u32::MAX` retries is not a `u32`. A total that
/// wrapped to zero would refuse the first retry, and one that overflowed would
/// end the call. The row needs two retries and then its answer.
async fn assert_largest_count_spends_one_send_each(client_count: u32, draft: Draft) {
    let context = format!("largest count, client={client_count} {draft:?}");
    let script: Box<[Answer]> =
        std::iter::repeat_n(Answer::TransientRetryAfter("0"), COUNTED_REPEATS)
            .chain([Answer::Complete])
            .collect();
    let OperationRow {
        driver,
        mut upstream,
        call,
        ..
    } = OperationRow::start_draft(
        &script,
        operation_client(client_count, PARKED_BACKOFF),
        draft,
    )
    .await;

    for attempt in 0..COUNTED_REPEATS {
        upstream
            .expect_each(
                &[PeerEvent::Started(attempt), PeerEvent::Disposed(attempt)],
                &context,
            )
            .await;
    }
    upstream
        .finish_answered(COUNTED_REPEATS, call, driver, &context)
        .await;
}

/// A replayable request's delay is held under the operation deadline, and
/// nothing is sent once that deadline has ended the call.
///
/// The peer sees the transient answer disposed before the clock moves, so the
/// delay holds no transport. The backoff outlasts the operation. The clock is
/// stepped onto the deadline and then past every timer the call held, so a
/// retry the expiry left scheduled would reach the peer.
async fn assert_replayable_delay_ends_at_the_deadline() {
    const PHASE: Phase = Phase::Backoff;

    let context = "replayable POST parked in its delay";
    let client = operation_client(0, PARKED_BACKOFF);
    let OperationRow {
        driver,
        mut upstream,
        entry,
        call,
        ..
    } = OperationRow::start_draft(
        PHASE.script(),
        client,
        Draft::new("POST", Permission::Replayable).counted(SPARE_RETRIES),
    )
    .await;
    PHASE.reach(&mut upstream, context).await;

    let pending_before_deadline = pending_until(&call, entry + OPERATION_TOTAL).await;
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    step_past_every_held_timer(PHASE).await;
    let starts = upstream.finish(driver, context).await;

    assert!(
        pending_before_deadline,
        "{context}: the call ended before the operation deadline"
    );
    assert!(
        Ending::OperationDeadline.ended(&result),
        "{context}: returned {}",
        describe(&result)
    );
    assert_cleanup(PHASE, Ok(()), starts, context);
}

/// Outbound 5.T2: a retry count is exact wherever it is written, and every
/// delay it buys stays inside the operation.
#[tokio::test(start_paused = true)]
async fn retry_allowances_and_delays_remain_bounded() {
    for row in ALLOWANCES {
        assert_allowance_is_exact(row).await;
    }
    assert_largest_count_spends_one_send_each(u32::MAX, Draft::new("GET", Permission::Defaulted))
        .await;
    assert_largest_count_spends_one_send_each(
        0,
        Draft::new("POST", Permission::Replayable).counted(u32::MAX),
    )
    .await;
    assert_replayable_delay_ends_at_the_deadline().await;
}

const CONNECT_RETRY_TEST: &str =
    "retry_lifetime::connect_failure_retries_under_the_request_permission";
const CONNECT_RETRY_MODE: &str = "connect-retry-events";
const CONNECT_RETRY_MARKER: &str = "connect-retry-events-observed";

/// The real-time bound on the whole private child. It bounds fixture failure
/// only.
const CONNECT_RETRY_CHILD_BOUND: Duration = Duration::from_secs(60);

/// The path segment and the query value the refused target carries.
const CONNECT_PATH_SECRET: &str = "connect-path-sentinel-3b7e";
const CONNECT_QUERY_SECRET: &str = "connect-query-sentinel-d90c";

/// The URL no service listens on: port zero is never a listener's address, so
/// every send fails at the connect stage without a reserved port.
const REFUSED_URL: &str =
    "http://127.0.0.1:0/connect-path-sentinel-3b7e?token=connect-query-sentinel-d90c";

/// The fixed sentence production records after each replayable failed send.
const RETRY_EVENT: &str = "retrying transient HTTP error";

const CONNECT_RETRIES: u32 = 2;
const CONNECT_BACKOFF: Duration = Duration::from_secs(1);

/// Start one described request at the refused port as its own task.
fn spawn_refused(draft: Draft) -> Call {
    spawn_draft(connect_retry_client(), draft, REFUSED_URL.to_owned())
}

/// A client whose retry, attempt, and operation bounds all outlast the row, so
/// only the configured retry count and the backoff shape the attempts.
///
/// The client holds no replay permission: each row writes its own on its
/// request.
fn connect_retry_client() -> Client {
    long_attempts(http::client().total_timeout(ATTEMPT_LIMIT))
        .retries(CONNECT_RETRIES)
        .backoff(CONNECT_BACKOFF)
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

/// The one label production records for every extension method.
const EXTENSION_LABEL: &str = "OTHER";

/// The method label a retry event carries: the token of a method the client
/// names, and one fixed label for every extension token.
fn method_label(method: &str) -> &str {
    match CLIENT_METHODS.contains(&method) {
        true => method,
        false => EXTENSION_LABEL,
    }
}

/// Require the retry event for `attempt` of `method`, as production recorded
/// it, with nothing of the request target in it.
fn assert_retry_event(capture: &TraceCapture, method: &str, attempt: u32, context: &str) {
    let events = capture.events();
    let event = &events[attempt as usize - 1];
    assert_field_value(event, "attempt", &attempt.to_string(), context);
    assert_field_value(event, "method", method_label(method), context);
    assert!(
        event.contains(RETRY_EVENT),
        "{context}: event {attempt} is not a retry event: {event}"
    );
    for secret in [CONNECT_PATH_SECRET, CONNECT_QUERY_SECRET] {
        assert!(
            !event.contains(secret),
            "{context}: retry event {attempt} repeats the request target: {event}"
        );
    }
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
    let result: Option<Result<ClientResponse, RuntimeError>> = settled(call).await;
    settle_wakeups().await;
    let recorded = capture.len();
    drop(capture);

    assert!(
        matches!(
            result,
            Some(Err(RuntimeError::Client(ClientError::Connect)))
        ),
        "{context}: returned {} instead of the connect failure",
        describe(&result)
    );
    assert_eq!(
        recorded, retries,
        "{context}: {recorded} retry events were recorded"
    );
}

/// One permitted row: every refused send but the last is followed by one retry
/// event carrying its attempt number, and the clock crosses each backoff only
/// after that event.
async fn assert_connect_failure_retries(draft: Draft) {
    let method = draft.method;
    let context = format!("{draft:?} to a refused port, permitted");
    let capture = capture_events(RETRY_EVENT);
    let call = spawn_refused(draft);

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

/// One row without permission: the first refused send is the answer, and no
/// retry event is ever recorded.
async fn assert_connect_failure_is_once(draft: Draft) {
    let context = format!("{draft:?} to a refused port, not permitted");
    let capture = capture_events(RETRY_EVENT);
    let call = spawn_refused(draft);
    assert_transport_failure_after(call, capture, 0, &context).await;
}

/// One refused-port row under the request's own permission.
///
/// A refused dial reached no peer. It is still sent again only when the
/// request's permission allows this method another send.
async fn assert_connect_failure_row(draft: Draft) {
    match draft.permission.permits(draft.method) {
        true => assert_connect_failure_retries(draft).await,
        false => assert_connect_failure_is_once(draft).await,
    }
}

/// 4.T4, revised for request-local permission: the default repeats a refused
/// dial for the idempotent methods, `Replayable` for every method, and `Never`
/// for none.
///
/// Runs in a private child, because the capture it reads is the process's one
/// global subscriber.
#[test]
fn connect_failure_retries_under_the_request_permission() {
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
                for permission in PERMISSIONS {
                    for method in METHODS {
                        assert_connect_failure_row(Draft::new(method, permission)).await;
                    }
                }
            });
        },
    );
}
