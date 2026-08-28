//! Component proof for the causal server stop state.
//!
//! Each case has two halves. The table half drives the production state machine
//! itself: an owner submits the one fact it is authorized to submit, and the
//! commit that lands first decides what the server reports. The seam half runs a
//! real owned server and enters through the public `ServerHandle` and the
//! supervisor's own observation seam, so what the table claims about commit
//! order is also claimed about the wiring that carries it — a handle whose
//! command only latched a request, or a supervisor that never committed its own
//! deadline, fails there.
//!
//! Sociable, not daemon-live: the live half reads what the commit left and what
//! the flat join reported. The public journey — admission, drain, permit
//! reacquisition, address reuse — belongs to `acceptance_owned_lifecycle`.

use std::sync::Arc;
use std::time::Duration;

use camber::RuntimeError;
use camber::http::mock::{ServerStopEdge, ServerStopEvent, ServerStopProbe, server_stop_probe};

use crate::common::{
    HELD_ROUTE, held_server, join_bounded, request_on_new_peer, wait_until_paused_bounded,
};

/// The configured grace every probed server stops under.
///
/// Never reached: no case here waits on wall time, and the deadline event is
/// submitted rather than fired. It exists because a stop state mints one
/// aggregate expiry, and a grace of zero would make that mint meaningless.
const GRACE: Duration = Duration::from_secs(30);

/// How long a joined committer has to report its transition.
const JOIN_BOUND: Duration = Duration::from_secs(5);

/// The drain a live seam row configures when it does not want the deadline.
///
/// Long enough that no row reaches it while a peer is being released promptly,
/// so a result of `Timeout` from those rows is a failure rather than a race.
const WIDE_DRAIN: Duration = Duration::from_secs(20);

/// The drain the deadline rows configure.
///
/// The one place this file lets wall time in, because the claim there is that
/// an expiring deadline COMMITS — which is product behavior and not an ordering
/// device. Every ordering claim around it is carried by a commit edge.
const NARROW_DRAIN: Duration = Duration::from_millis(200);

/// Every phase a stop state can be committed into, and how to reach it.
///
/// Reaching is spelled as events rather than as a constructor, because the only
/// way production reaches a phase is by committing into it. A phase a case
/// could set directly would let the table pass over transitions no owner can
/// actually make.
#[derive(Clone, Copy)]
struct Reached {
    phase: &'static str,
    events: &'static [ServerStopEvent],
}

const PHASES: [Reached; 5] = [
    Reached {
        phase: "running",
        events: &[],
    },
    Reached {
        phase: "graceful",
        events: &[ServerStopEvent::Graceful],
    },
    Reached {
        phase: "cancelled",
        events: &[ServerStopEvent::Cancel],
    },
    Reached {
        phase: "deadline-expired",
        events: &[ServerStopEvent::Graceful, ServerStopEvent::DeadlineExpiry],
    },
    Reached {
        phase: "finished",
        events: &[ServerStopEvent::Graceful, ServerStopEvent::Settled],
    },
];

/// One cell of the documented event table.
struct Cell {
    from: &'static str,
    event: ServerStopEvent,
    phase: &'static str,
    changed: bool,
}

/// Build a probe already committed into `reached`.
fn probe_at(reached: Reached) -> ServerStopProbe {
    let probe = server_stop_probe(GRACE);
    for event in reached.events {
        probe.apply(*event);
    }
    assert_eq!(
        probe.observed().phase,
        reached.phase,
        "reaching {} did not commit that phase",
        reached.phase
    );
    probe
}

/// One table row before it is named: source phase, event, committed phase, and
/// whether the commit moved anything.
type Row = (&'static str, ServerStopEvent, &'static str, bool);

/// Running: a command or a fatal fact starts the stop; a deadline that was
/// never armed and a settlement over open admission decide nothing.
const FROM_RUNNING: [Row; 6] = [
    ("running", ServerStopEvent::Graceful, "graceful", true),
    ("running", ServerStopEvent::Cancel, "cancelled", true),
    ("running", ServerStopEvent::Abandon, "cancelled", true),
    ("running", ServerStopEvent::Fatal, "graceful", true),
    ("running", ServerStopEvent::DeadlineExpiry, "running", false),
    ("running", ServerStopEvent::Settled, "running", false),
];

/// Graceful: cancellation is the permitted escalation, the deadline is the
/// other, and repeating the graceful request changes nothing.
const FROM_GRACEFUL: [Row; 6] = [
    ("graceful", ServerStopEvent::Graceful, "graceful", false),
    ("graceful", ServerStopEvent::Cancel, "cancelled", true),
    ("graceful", ServerStopEvent::Abandon, "cancelled", true),
    ("graceful", ServerStopEvent::Fatal, "graceful", false),
    (
        "graceful",
        ServerStopEvent::DeadlineExpiry,
        "deadline-expired",
        true,
    ),
    ("graceful", ServerStopEvent::Settled, "finished", true),
];

/// Forced cancellation: nothing escalates it, and only settlement ends it.
const FROM_CANCELLED: [Row; 6] = [
    ("cancelled", ServerStopEvent::Graceful, "cancelled", false),
    ("cancelled", ServerStopEvent::Cancel, "cancelled", false),
    ("cancelled", ServerStopEvent::Abandon, "cancelled", false),
    ("cancelled", ServerStopEvent::Fatal, "cancelled", false),
    (
        "cancelled",
        ServerStopEvent::DeadlineExpiry,
        "cancelled",
        false,
    ),
    ("cancelled", ServerStopEvent::Settled, "finished", true),
];

/// Forced timeout: a cancellation arriving after it is a no-op, which is the
/// row a ranked selection got backwards.
const FROM_TIMED_OUT: [Row; 6] = [
    (
        "deadline-expired",
        ServerStopEvent::Graceful,
        "deadline-expired",
        false,
    ),
    (
        "deadline-expired",
        ServerStopEvent::Cancel,
        "deadline-expired",
        false,
    ),
    (
        "deadline-expired",
        ServerStopEvent::Abandon,
        "deadline-expired",
        false,
    ),
    (
        "deadline-expired",
        ServerStopEvent::Fatal,
        "deadline-expired",
        false,
    ),
    (
        "deadline-expired",
        ServerStopEvent::DeadlineExpiry,
        "deadline-expired",
        false,
    ),
    (
        "deadline-expired",
        ServerStopEvent::Settled,
        "finished",
        true,
    ),
];

/// Finished: the result is immutable, so every event is a no-op.
const FROM_FINISHED: [Row; 6] = [
    ("finished", ServerStopEvent::Graceful, "finished", false),
    ("finished", ServerStopEvent::Cancel, "finished", false),
    ("finished", ServerStopEvent::Abandon, "finished", false),
    ("finished", ServerStopEvent::Fatal, "finished", false),
    (
        "finished",
        ServerStopEvent::DeadlineExpiry,
        "finished",
        false,
    ),
    ("finished", ServerStopEvent::Settled, "finished", false),
];

/// The whole documented table, one row per (phase, event) pair.
fn table() -> Box<[Cell]> {
    FROM_RUNNING
        .into_iter()
        .chain(FROM_GRACEFUL)
        .chain(FROM_CANCELLED)
        .chain(FROM_TIMED_OUT)
        .chain(FROM_FINISHED)
        .map(|(from, event, phase, changed)| Cell {
            from,
            event,
            phase,
            changed,
        })
        .collect()
}

/// Apply one cell against a probe committed into its source phase, and check
/// everything that cell claims.
fn assert_cell(cell: &Cell) {
    let reached = PHASES
        .into_iter()
        .find(|reached| reached.phase == cell.from)
        .expect("every table row names a reachable phase");
    let probe = probe_at(reached);
    let before = probe.observed();
    let transition = probe.apply(cell.event);
    let after = probe.observed();

    assert_eq!(
        transition.phase, cell.phase,
        "{:?} from {} committed {} rather than {}",
        cell.event, cell.from, transition.phase, cell.phase
    );
    assert_eq!(
        transition.changed, cell.changed,
        "{:?} from {} reported changed={} rather than {}",
        cell.event, cell.from, transition.changed, cell.changed
    );
    assert_eq!(
        after.phase, cell.phase,
        "{:?} from {} left the observed phase at {}",
        cell.event, cell.from, after.phase
    );
    assert_eq!(
        after.applied,
        before.applied + 1,
        "{:?} from {} was not applied exactly once",
        cell.event,
        cell.from
    );
    assert_eq!(
        after.commits,
        before.commits + u64::from(cell.changed),
        "{:?} from {} disagreed with its own transition about committing",
        cell.event,
        cell.from
    );
}

/// Name the one member of the closed set a settlement handed over.
///
/// A settlement fixes one of four results or none at all, and every one of them
/// is a different account of how the server ended. Naming the value read is what
/// lets a row that changed say which member it produced.
fn settled_as(taken: &Option<Result<(), RuntimeError>>) -> &'static str {
    match taken {
        None => "nothing",
        Some(Ok(())) => "completed",
        Some(Err(RuntimeError::Http(_))) => "failed",
        Some(Err(RuntimeError::Cancelled)) => "cancelled",
        Some(Err(RuntimeError::Timeout)) => "deadline-expired",
        Some(Err(_)) => "a result outside the closed set",
    }
}

/// Assert a settlement handed over `expect`, and report what it handed over.
///
/// The whole `Option<Result<(), RuntimeError>>` is carried into the message: a
/// row that collapsed it to a bool would report a regression as `false` and
/// leave a reader unable to tell a missing result from the wrong one.
fn assert_result(taken: Option<Result<(), RuntimeError>>, expect: &str, context: &str) {
    assert_eq!(
        settled_as(&taken),
        expect,
        "{context}: the settlement handed over {taken:?}"
    );
}

/// The result a settlement fixes from each phase that can fix one, and the two
/// phases that cannot.
fn assert_settlement_results() {
    let clean = probe_at(PHASES[1]);
    assert_result(clean.take_result(), "completed", "a drained graceful stop");
    assert_result(
        clean.take_result(),
        "nothing",
        "the flat result leaves exactly once",
    );

    let failed = server_stop_probe(GRACE);
    failed.apply(ServerStopEvent::Fatal);
    assert_result(failed.take_result(), "failed", "a recorded fatal fact");
    assert_eq!(failed.observed().outcome, "failed");

    let cancelled = probe_at(PHASES[2]);
    assert_result(cancelled.take_result(), "cancelled", "a cancelled stop");

    let expired = probe_at(PHASES[3]);
    assert_result(expired.take_result(), "deadline-expired", "an expired stop");

    // A settlement over open admission fixes nothing, so there is no result to
    // hand over and the server is still running.
    let running = server_stop_probe(GRACE);
    assert_result(
        running.take_result(),
        "nothing",
        "a settlement over open admission",
    );
    assert_eq!(running.observed().phase, "running");
}

/// Four real tasks race the same lock with the same command, and every one of
/// them is joined.
///
/// Each must observe a committed phase rather than the running one it started
/// from, and only one of the four may have moved it.
async fn assert_joined_readers_agree() {
    let probe = Arc::new(server_stop_probe(GRACE));
    let readers: Box<[_]> = (0..4)
        .map(|_| {
            let probe = Arc::clone(&probe);
            tokio::spawn(async move {
                probe.apply(ServerStopEvent::Graceful);
                probe.observed().phase
            })
        })
        .collect();
    for reader in readers {
        let phase = join_bounded(reader, JOIN_BOUND)
            .await
            .expect("a stop-state reader task must join");
        assert_eq!(phase, "graceful");
    }
    assert_eq!(
        probe.observed().commits,
        1,
        "four graceful commits moved the phase more than once"
    );
}

/// Both public commands, issued while the supervisor cannot have been notified.
///
/// The supervisor is held at its own select boundary before the server has
/// taken a single event, so it has observed nothing and can observe nothing
/// while this runs. Every phase read below therefore belongs to the command
/// that returned, and to nothing downstream of it.
///
/// This is the half that fails when the seam is unwired: a `ServerHandle` whose
/// `shutdown` or `cancel` only published a notification leaves the phase at
/// `running` on the line after the command returned, and a supervisor that
/// never settled its state hands back the wrong flat result at the join.
async fn assert_public_commands_commit_before_the_supervisor_is_notified() {
    let server = held_server(4, WIDE_DRAIN);
    let stop = server.stop();
    stop.pause_once(ServerStopEdge::BeforeSupervisorSelect)
        .expect("arm the supervisor's select boundary");
    wait_until_paused_bounded(
        &server.controller,
        ServerStopEdge::BeforeSupervisorSelect,
        "the supervisor never reached its select boundary",
    )
    .await;

    let running = stop.observed();
    assert_eq!(running.phase, "running");
    assert_eq!(running.commits, 0, "an idle server has committed nothing");

    server.handle.shutdown();
    let graceful = stop.observed();
    assert_eq!(
        graceful.phase, "graceful",
        "shutdown() returned before its control fact was committed"
    );
    assert_eq!(graceful.commits, 1);
    assert!(
        graceful.aggregate_deadline.is_some(),
        "the graceful commit mints the one aggregate deadline before it returns"
    );

    server.handle.cancel();
    let cancelled = stop.observed();
    assert_eq!(
        cancelled.phase, "cancelled",
        "cancel() returned before its control fact was committed"
    );
    assert_eq!(cancelled.commits, 2);
    assert!(cancelled.cancel_commanded);
    assert_eq!(
        cancelled.outcome, "pending",
        "no command fixes the flat result before the children settle"
    );

    stop.release(ServerStopEdge::BeforeSupervisorSelect)
        .expect("release the supervisor's select boundary");
    let result = join_bounded(server.handle.join(), JOIN_BOUND).await;
    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "the supervisor reported {result:?} rather than the phase the command committed"
    );
    assert_eq!(stop.observed().outcome, "cancelled");
}

// 1.T1 — Invariant 1: a stop command commits its control fact before the public
// command returns or the supervisor is notified.
//
// The whole documented table, applied against the production state machine. The
// commit is what the table is about: every cell asserts the phase the event
// left AND whether the event moved it, so a state that merely latched a request
// and selected an outcome later cannot satisfy it.
//
// The last section then makes the same claim through the public seam, against a
// supervisor that is provably still parked, so the table is a claim about the
// server callers actually hold rather than about a state machine on its own.
#[camber::test]
async fn server_stop_state_applies_every_event_from_every_phase() {
    for cell in table() {
        assert_cell(&cell);
    }
    assert_settlement_results();
    assert_joined_readers_agree().await;
    assert_public_commands_commit_before_the_supervisor_is_notified().await;
}

// 1.T3 — Invariant 3: settlement returns the first committed fatal fact when
// present, while a later accepted cancellation before terminal commitment
// yields cancellation.
//
// Every row here is decided by the order the events COMMITTED, not by the order
// an observer would rank them. One probe row releases two commits with no
// ordering barrier at all and accepts only that exactly one of them moved the
// phase.
//
// The last two rows put the same claim on the production path: a real drain that
// cannot finish reaches the one aggregate deadline and settles on the expiry the
// supervisor committed, and the same expiry held at the supervisor's own commit
// edge loses to a public cancellation that committed while it waited.
#[camber::test]
async fn graceful_fatal_cancel_and_deadline_use_commit_order_not_poll_order() {
    assert_fatal_facts_follow_the_first();
    assert_escalations_follow_commit_order();
    assert_unordered_escalation_stays_in_its_closed_set().await;
    assert_expiring_deadline_commits_through_the_supervisor().await;
    assert_command_committed_first_beats_the_supervisors_own_expiry().await;
}

/// The one aggregate deadline, expiring against a real drain that cannot end.
///
/// A graceful stop is asked for while a handler is held, so the drain has work
/// outstanding when the expiry arrives. The supervisor is the only owner that
/// can submit that fact, and the flat result is what says it did: a supervisor
/// that let the deadline pass without committing it would settle the drain as
/// completed and hand back `Ok(())`.
async fn assert_expiring_deadline_commits_through_the_supervisor() {
    let server = held_server(4, NARROW_DRAIN);
    let stop = server.stop();
    let peer = request_on_new_peer(server.addr, HELD_ROUTE, "close").await;
    server
        .await_entry("the expiring drain's held request")
        .await;

    server.handle.shutdown();
    assert_eq!(stop.observed().phase, "graceful");

    let result = join_bounded(server.handle.join(), JOIN_BOUND).await;
    assert!(
        matches!(result, Err(RuntimeError::Timeout)),
        "an expired drain reported {result:?} rather than its committed timeout"
    );
    assert_eq!(stop.observed().outcome, "deadline-expired");
    drop(peer);
}

/// The same expiry, held at the supervisor's own commit edge while a public
/// cancellation commits ahead of it.
///
/// This is the row a ranked selection got backwards, put on the production
/// path. Both facts are genuinely in flight: the deadline has fired and its
/// owner is standing at the lock, and the cancellation arrives while it waits.
/// The order is the lock, not the executor — so the caller who asked is the one
/// the server answers, and the expiry behind that commit moves nothing.
async fn assert_command_committed_first_beats_the_supervisors_own_expiry() {
    let server = held_server(4, NARROW_DRAIN);
    let stop = server.stop();
    let peer = request_on_new_peer(server.addr, HELD_ROUTE, "close").await;
    server.await_entry("the raced drain's held request").await;

    // The deadline is the only fact this server's supervisor commits, so the
    // edge below can name no other one.
    stop.pause_once(ServerStopEdge::BeforeCommit)
        .expect("arm the supervisor's commit edge");
    server.handle.shutdown();
    wait_until_paused_bounded(
        &server.controller,
        ServerStopEdge::BeforeCommit,
        "the expiring deadline never reached the supervisor's commit edge",
    )
    .await;

    // Time passing is not a committed timeout. The expiry is at the lock and
    // has changed nothing yet.
    assert_eq!(
        stop.observed().phase,
        "graceful",
        "an expiry standing at the commit edge has already decided something"
    );

    server.handle.cancel();
    assert_eq!(stop.observed().phase, "cancelled");
    assert_eq!(stop.observed().commits, 2);

    stop.release(ServerStopEdge::BeforeCommit)
        .expect("release the supervisor's commit edge");
    let result = join_bounded(server.handle.join(), JOIN_BOUND).await;
    assert!(
        matches!(result, Err(RuntimeError::Cancelled)),
        "the expiry committed second reported {result:?} rather than losing"
    );
    let settled = stop.observed();
    assert_eq!(settled.outcome, "cancelled");
    assert_eq!(
        settled.commits, 3,
        "the graceful request, the cancellation, and the settlement are every \
         commit this server made — a fourth is the expiry moving the phase"
    );
    drop(peer);
}

/// The drain carries the first fatal fact it recorded, and later ones are
/// diagnostics rather than a better account of the end.
fn assert_fatal_facts_follow_the_first() {
    let probe = server_stop_probe(GRACE);
    probe.apply(ServerStopEvent::Graceful);
    probe.apply(ServerStopEvent::Fatal);
    assert!(probe.observed().fatal_recorded);
    assert_result(
        probe.take_result(),
        "failed",
        "a fatal fact recorded inside a graceful drain",
    );

    let probe = server_stop_probe(GRACE);
    probe.apply(ServerStopEvent::Fatal);
    probe.apply(ServerStopEvent::Fatal);
    assert_eq!(probe.observed().later_fatal_facts, 1);
    assert_eq!(probe.observed().phase, "graceful");
    assert_result(
        probe.take_result(),
        "failed",
        "a second fatal fact behind the first",
    );
}

/// Every ordered escalation row, decided by the order the events committed.
///
/// Wall time never enters: the deadline is a committed event here, so these
/// rows are about order and not about duration.
fn assert_escalations_follow_commit_order() {
    // A cancellation committed after the fatal fact but before settlement is
    // what the caller is told.
    let probe = server_stop_probe(GRACE);
    probe.apply(ServerStopEvent::Graceful);
    probe.apply(ServerStopEvent::Fatal);
    probe.apply(ServerStopEvent::Cancel);
    assert_result(
        probe.take_result(),
        "cancelled",
        "a cancellation committed between the fatal fact and settlement",
    );

    // The deadline committed first keeps the result, and the cancellation
    // behind it changes nothing.
    let probe = server_stop_probe(GRACE);
    probe.apply(ServerStopEvent::Graceful);
    probe.apply(ServerStopEvent::DeadlineExpiry);
    probe.apply(ServerStopEvent::Cancel);
    assert_result(
        probe.take_result(),
        "deadline-expired",
        "a cancellation behind a committed expiry",
    );

    // The same two events in the other order, and the answer follows the order
    // rather than a rank.
    let probe = server_stop_probe(GRACE);
    probe.apply(ServerStopEvent::Graceful);
    probe.apply(ServerStopEvent::Cancel);
    probe.apply(ServerStopEvent::DeadlineExpiry);
    assert_result(
        probe.take_result(),
        "cancelled",
        "an expiry behind a committed cancellation",
    );

    // An abandoned owner forces the same phase as a command and is not recorded
    // as one asking, so a forced wait can still tell them apart.
    let probe = server_stop_probe(GRACE);
    probe.apply(ServerStopEvent::Abandon);
    assert_eq!(probe.observed().phase, "cancelled");
    assert!(!probe.observed().cancel_commanded);
    probe.apply(ServerStopEvent::Cancel);
    assert!(probe.observed().cancel_commanded);

    // An event submitted after terminal settlement does not rewrite the result.
    let probe = server_stop_probe(GRACE);
    probe.apply(ServerStopEvent::Graceful);
    assert_result(
        probe.take_result(),
        "completed",
        "a settled graceful stop the cancellation below arrives after",
    );
    probe.apply(ServerStopEvent::Cancel);
    assert_eq!(probe.observed().phase, "finished");
    assert_eq!(probe.observed().outcome, "completed");
}

/// Two escalations released with no ordering barrier between them.
///
/// Either committed phase is valid; what is not valid is both of them counting,
/// the loser reporting that it moved anything, or a result outside the set.
async fn assert_unordered_escalation_stays_in_its_closed_set() {
    let probe = Arc::new(server_stop_probe(GRACE));
    probe.apply(ServerStopEvent::Graceful);
    let committers: Box<[_]> = [ServerStopEvent::Cancel, ServerStopEvent::DeadlineExpiry]
        .into_iter()
        .map(|event| {
            let probe = Arc::clone(&probe);
            tokio::spawn(async move { probe.apply(event).changed })
        })
        .collect();
    let mut moved = 0;
    for committer in committers {
        let changed = join_bounded(committer, JOIN_BOUND)
            .await
            .expect("a stop-state escalation task must join");
        moved += usize::from(changed);
    }
    assert_eq!(moved, 1, "exactly one escalation may move the phase");
    let settled = probe.take_result();
    let context = "the unordered escalation";
    match probe.observed().outcome {
        "cancelled" => assert_result(settled, "cancelled", context),
        "deadline-expired" => assert_result(settled, "deadline-expired", context),
        other => panic!("unordered escalation committed {other}, outside the closed set"),
    }
}
