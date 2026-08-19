//! One aggregate shutdown deadline, one disposition per framework owner, and
//! one immutable account returned after every join or abandonment has happened.
//!
//! 18.T1 owns the deadline: the first graceful transition anywhere mints the
//! only expiry there is, and every nested owner — the server, an admitted
//! request, a registered upgrade, the root scope, a registered resource, the
//! executor — reads that same instant back rather than starting a fresh copy of
//! the configured grace when it happens to notice.
//!
//! 18.T2 owns the inventory: every framework-owned participant completes, is
//! cancelled and joined, or is named in the returned aggregate. There is no
//! fourth disposition, and a participant that reached none of the three is a
//! defect this row fails on.
//!
//! 18.T3 owns explicit cancellation: it mints nothing, it takes effect at once
//! instead of under a fresh grace, its cause outranks every other cause that was
//! ready, and the failures it displaced stay in the account.

use crate::common;
use crate::lifecycle_kinds;

use camber::http::{BodyAdmission, BodyAdmissionContext, Request, Response, Router};
use camber::runtime_test_support::{
    ParticipantDisposition, ParticipantSettlement, RuntimeController, ShutdownDeadlineReading,
    runtime_schedule,
};
use camber::{RuntimeError, runtime};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The aggregate grace every row here configures.
///
/// Long enough that no row reaches it by accident, and short enough that a row
/// which deliberately does still finishes inside the suite's own bound.
const AGGREGATE_GRACE: Duration = Duration::from_millis(1_500);

/// The bound a fixture's readiness probe and bounded joins run under.
const FIXTURE_BOUND: Duration = Duration::from_secs(5);

/// The route a held request is admitted on.
const HELD_ROUTE: &str = "/held";

/// The maximum the witnessing admission policy selects.
///
/// Comfortably above the stalled head's declared length, so nothing this row
/// asserts turns on a byte ceiling.
const ADMITTED_BODY_MAX: usize = 64 * 1024;

/// A router that answers a bodyless probe at once and reads a payload on the
/// same path.
///
/// The `POST` arm is what holds a connection open: its body owner keeps polling
/// a payload the peer never finishes, which is the admitted request that has to
/// observe the graceful transition and read the shared expiry.
fn answering_router() -> Router {
    admitting_router(None)
}

/// [`answering_router`] with a witness on the production body-admission
/// boundary.
///
/// A row that shuts a server down while one request is being read has to know
/// that request is already being read. Admission is where production decides
/// exactly that, so the witness is armed there rather than inferred from a
/// socket write the accept loop may not have reached yet.
fn admitting_router(admitted: Option<std::sync::mpsc::SyncSender<()>>) -> Router {
    let mut router = Router::new();
    router.get(HELD_ROUTE, |_req: &Request| async move {
        Response::text(200, "held").expect("a valid status")
    });
    router.post(HELD_ROUTE, |_req: &Request| async move {
        Response::text(200, "read").expect("a valid status")
    });
    match admitted {
        Some(admitted) => router.body_admission(move |_context: &BodyAdmissionContext<'_>| {
            // Dropped rather than reported: the row may already have taken its
            // one notification, and this is production's own thread.
            let _witnessed = admitted.try_send(());
            Ok(BodyAdmission::new(ADMITTED_BODY_MAX))
        }),
        None => router,
    }
}

/// Every owner name a reading or settlement was recorded against.
fn reading_owners(readings: &[ShutdownDeadlineReading]) -> Box<[&str]> {
    readings
        .iter()
        .map(ShutdownDeadlineReading::participant)
        .collect()
}

/// Every owner name a settlement was recorded against, with its disposition.
fn settlement_rows(settlements: &[ParticipantSettlement]) -> Box<[String]> {
    settlements
        .iter()
        .map(|settled| {
            format!(
                "{}|{}",
                settled.participant(),
                settled.disposition().label()
            )
        })
        .collect()
}

/// Wait until `owner` has read the shared expiry, failing with the owners that
/// did read it rather than with the absence alone.
fn wait_for_reading(controller: &RuntimeController, owner: &str) {
    let settled = common::poll_until(common::OBSERVATION_BOUND, || {
        reading_owners(&controller.shutdown_deadline_readings()).contains(&owner)
    });
    assert!(
        settled,
        "{owner} never read the shared expiry; owners: {:?}",
        reading_owners(&controller.shutdown_deadline_readings())
    );
}

/// Whether `owner` settled at least once as `disposition`.
fn settled_as(
    settlements: &[ParticipantSettlement],
    owner: &str,
    disposition: ParticipantDisposition,
) -> bool {
    settlements
        .iter()
        .any(|settled| settled.participant() == owner && settled.disposition() == disposition)
}

// ---------------------------------------------------------------------------
// 18.T1
// ---------------------------------------------------------------------------

/// 18.T1
#[test]
fn first_graceful_transition_mints_one_deadline_and_nested_owners_never_restart_it() {
    let controller = runtime_schedule();
    let log = Arc::new(common::CallbackLog::default());

    let outcome = staged_graceful_transitions(&controller, &log);
    assert!(
        outcome.is_ok(),
        "the deadline fixture's runtime did not tear down cleanly: {outcome:?}"
    );

    assert_eq!(
        controller.shutdown_deadline_mints(),
        1,
        "more than one aggregate shutdown deadline was minted"
    );
    let mint = controller
        .shutdown_deadline_mint()
        .expect("no graceful transition minted an aggregate deadline");
    assert_eq!(
        mint.grace(),
        AGGREGATE_GRACE,
        "the minted deadline was not the configured grace away from its transition"
    );

    let readings = controller.shutdown_deadline_readings();
    assert!(
        !readings.is_empty(),
        "no framework owner read the aggregate deadline"
    );
    for reading in &readings {
        assert_eq!(
            reading.expiry(),
            mint.expiry(),
            "{} read a different expiry from the one the first transition minted; owners: {:?}",
            reading.participant(),
            reading_owners(&readings)
        );
    }

    let owners = reading_owners(&readings);
    for expected in ["server", "connection", "root-scope", "executor"] {
        assert!(
            owners.contains(&expected),
            "{expected} never read the shared expiry; owners: {owners:?}"
        );
    }
    assert!(
        owners.iter().any(|owner| owner.starts_with("resource ")),
        "no registered resource read the shared expiry; owners: {owners:?}"
    );
}

/// Serve a runtime whose server, admitted request, background child, and
/// registered resource are all live, then take repeated graceful transitions at
/// distinct instants.
///
/// The instants are separated by production observations rather than by sleeps:
/// the second request is issued only once the admitted connection has been
/// observed reading the expiry the first transition minted, which is real
/// elapsed time and a real ordering edge.
fn staged_graceful_transitions(
    controller: &RuntimeController,
    log: &Arc<common::CallbackLog>,
) -> Result<(), RuntimeError> {
    runtime::builder()
        .with_test_schedule(controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .health_interval(common::TICK)
        .resource_budget(common::short_resource_budget())
        .resource(common::ScriptedResource::new("deadline-reader", log))
        .run(|| {
            let (admitted_tx, admitted) = std::sync::mpsc::sync_channel(1);
            let server =
                common::spawn_server_ready(admitting_router(Some(admitted_tx)), FIXTURE_BOUND)
                    .expect("the deadline fixture served");
            let child = camber::spawn_async(async {
                camber::runtime_test_support::wait_scope_closing().await;
            });
            let addr = server.local_addr();
            let mut peer = common::connect(addr).expect("the held peer connected");
            common::write_stalled_body(&mut peer, None, "POST", HELD_ROUTE)
                .expect("write a head whose payload never finishes");
            admitted
                .recv_timeout(FIXTURE_BOUND)
                .expect("the held request reached production body admission");

            // The first transition, and the only mint this row allows.
            let handle = server.into_handle();
            handle.shutdown();
            wait_for_reading(controller, "connection");
            // A later instant, and a transition that must find the expiry
            // already fixed rather than start a second one.
            runtime::request_shutdown();

            drop(common::read_http_response_bounded(&mut peer));
            drop(peer);
            drop(runtime::block_on(common::join_bounded(
                handle,
                FIXTURE_BOUND,
            )));
            runtime::block_on(common::join_bounded(child, FIXTURE_BOUND))
                .expect("the background child exited on scope closing");
        })
}

// ---------------------------------------------------------------------------
// 18.T2
// ---------------------------------------------------------------------------

/// What one row observed: how each participant settled, and what the runtime
/// returned.
struct RowOutcome {
    settlements: Box<[ParticipantSettlement]>,
    result: Result<(), RuntimeError>,
}

impl RowOutcome {
    /// The error this row's runtime returned, or a failure naming its success.
    fn failure(&self, row: &str) -> &RuntimeError {
        self.result
            .as_ref()
            .expect_err(&format!("{row}: the runtime reported a clean teardown"))
    }

    /// Assert `owner` settled as named, and that a named owner is one the
    /// returned aggregate actually holds.
    fn assert_settled(&self, row: &str, owner: &str, disposition: ParticipantDisposition) {
        assert!(
            settled_as(&self.settlements, owner, disposition),
            "{row}: {owner} never settled as {}; settlements: {:?}",
            disposition.label(),
            settlement_rows(&self.settlements)
        );
        if disposition == ParticipantDisposition::Named {
            self.assert_named_in_aggregate(row, owner);
        }
    }

    /// A participant the inventory calls named must appear in the account the
    /// caller reads back.
    fn assert_named_in_aggregate(&self, row: &str, owner: &str) {
        let identities = lifecycle_kinds::aggregate_identities(self.failure(row));
        assert!(
            identities
                .iter()
                .any(|identity| identity.starts_with(&normalized_owner(owner))),
            "{row}: {owner} settled as named but the returned aggregate does not hold it: \
             {identities:?}"
        );
    }
}

/// The settlement inventory renders a resource as `resource <name>` and the
/// aggregate vocabulary as `resource:<name>`; one owner, two spellings, and
/// this is where they are reconciled rather than at each assertion.
fn normalized_owner(owner: &str) -> String {
    match owner.split_once(' ') {
        Some(("resource", name)) => format!("resource:{name}"),
        _ => owner.to_owned(),
    }
}

/// 18.T2
#[test]
fn aggregate_shutdown_cancels_joins_or_names_every_framework_owner() {
    graceful_completion_row();
    cancellable_work_row();
    panicking_child_row();
    resource_deadline_row();
    lost_resource_worker_row();
    non_preemptible_callback_row();
}

/// A server, its connection, a background child, and a resource that all finish
/// their own work: every one of them completes, and nothing is returned.
fn graceful_completion_row() {
    let row = "graceful completion";
    let controller = runtime_schedule();
    let log = Arc::new(common::CallbackLog::default());
    let result = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .resource_budget(common::short_resource_budget())
        .resource(common::ScriptedResource::new("clean", &log))
        .run(|| {
            let server = common::spawn_server_ready(answering_router(), FIXTURE_BOUND)
                .expect("the completion fixture served");
            drop(common::send(
                server.local_addr(),
                "GET",
                HELD_ROUTE,
                &[],
                b"",
            ));
            server
                .shutdown_bounded(FIXTURE_BOUND)
                .expect("the completion fixture tore down");
            runtime::request_shutdown();
        });
    assert!(
        result.is_ok(),
        "{row}: a teardown whose owners all finished returned {result:?}"
    );

    let settlements = controller.participant_settlements();
    for owner in ["server", "connection", "root-scope", "resource clean"] {
        assert!(
            settled_as(&settlements, owner, ParticipantDisposition::Completed),
            "{row}: {owner} did not complete; settlements: {:?}",
            settlement_rows(&settlements)
        );
    }
}

/// A yielding child that ignores the close: cancelled and joined by the forced
/// stop, with the scope that could not drain named in the account.
fn cancellable_work_row() {
    let row = "cancellable work";
    let outcome = run_row(|| {
        drop(camber::spawn_async(async {
            std::future::pending::<()>().await;
        }));
    });
    outcome.assert_settled(row, "root-scope", ParticipantDisposition::Named);
    outcome.assert_settled(
        row,
        "background-task",
        ParticipantDisposition::CancelledAndJoined,
    );
    lifecycle_kinds::assert_scope_drain(outcome.failure(row), 1, row);
}

/// A Camber-owned child that unwinds: named, and its payload is the primary.
fn panicking_child_row() {
    let row = "panicking child";
    let panicked = Arc::new(AtomicBool::new(false));
    let entered = Arc::clone(&panicked);
    let outcome = run_row(move || {
        camber::schedule::every(Duration::from_millis(1), move || {
            entered.store(true, Ordering::SeqCst);
            panic!("aggregate row child panic");
        })
        .expect("the panicking child was admitted");
        common::wait_until("the scheduled child to unwind", || {
            panicked.load(Ordering::SeqCst)
        });
    });
    lifecycle_kinds::assert_background_panic(
        outcome.failure(row),
        "aggregate row child panic",
        row,
    );
}

/// A resource callback that outlives its phase deadline: named, and the
/// abandoned worker keeps the resource rather than being reported terminated.
fn resource_deadline_row() {
    let row = "resource deadline";
    let controller = runtime_schedule();
    let log = Arc::new(common::CallbackLog::default());
    let parked =
        common::ScriptedResource::new("parked", &log).shutdown(common::Behavior::ParkFrom(1));
    let gate = parked.gate();
    let witness = parked.drop_witness();

    let result = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .resource_budget(common::short_resource_budget())
        .resource(parked)
        .run(|| runtime::request_shutdown());

    let outcome = RowOutcome {
        settlements: controller.participant_settlements(),
        result,
    };
    outcome.assert_settled(row, "resource parked", ParticipantDisposition::Named);

    gate.open();
    common::wait_until("the released worker to let its resource go", || {
        witness.load(Ordering::Acquire)
    });
}

/// A resource whose worker never starts: named through the phase that met it,
/// with no callback ever entered.
fn lost_resource_worker_row() {
    let row = "lost resource worker";
    let controller = runtime_schedule();
    let log = Arc::new(common::CallbackLog::default());
    let result = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .resource_budget(common::short_resource_budget())
        .resource(common::ScriptedResource::new("lost", &log))
        .run(|| {
            // Refused only once the readiness pass is behind this run: a
            // refusal standing through it would refuse the run instead, and
            // teardown is what this row is about.
            controller.refuse_resource_worker("lost");
            runtime::request_shutdown();
        });

    let outcome = RowOutcome {
        settlements: controller.participant_settlements(),
        result,
    };
    outcome.assert_settled(row, "resource lost", ParticipantDisposition::Named);
    controller.admit_resource_worker("lost");
}

/// A blocking child abort cannot preempt: the scope names it, and the executor
/// that still owns it is named for the deadline it crossed.
fn non_preemptible_callback_row() {
    let row = "non-preemptible callback";
    let (park_tx, park_rx) = std::sync::mpsc::channel::<()>();
    let outcome = run_row(move || {
        drop(camber::spawn(move || {
            let _parked = park_rx.recv_timeout(FIXTURE_BOUND);
        }));
    });
    outcome.assert_settled(row, "root-scope", ParticipantDisposition::Named);
    outcome.assert_settled(row, "executor", ParticipantDisposition::Named);
    drop(park_tx);
}

/// Run one row's body inside a runtime with a controller attached, and hand
/// back what the runtime returned beside how each participant settled.
fn run_row(body: impl FnOnce()) -> RowOutcome {
    let controller = runtime_schedule();
    let result = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .run(body);
    RowOutcome {
        settlements: controller.participant_settlements(),
        result,
    }
}

// ---------------------------------------------------------------------------
// 18.T3
// ---------------------------------------------------------------------------

/// 18.T3
#[test]
fn explicit_cancel_skips_new_grace_and_returns_cancel_primary() {
    cancel_before_any_drain();
    cancel_during_a_drain();
}

/// A cancellation issued before any transition mints nothing at all, and the
/// server it ended settles as cancelled and joined.
fn cancel_before_any_drain() {
    let row = "cancel before drain";
    let controller = runtime_schedule();
    let minted = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .run(|| {
            let server = common::spawn_server_ready(answering_router(), FIXTURE_BOUND)
                .expect("the cancel fixture served");
            let handle = server.into_handle();
            handle.cancel();
            let joined = runtime::block_on(common::join_bounded(handle, FIXTURE_BOUND));
            assert!(
                matches!(joined, Err(RuntimeError::Cancelled)),
                "{row}: an explicitly cancelled server reported {joined:?}"
            );
            controller.shutdown_deadline_mints()
        })
        .expect("the cancel fixture's runtime tore down");
    assert_eq!(
        minted, 0,
        "{row}: an explicit cancellation minted a fresh aggregate deadline"
    );
    assert!(
        settled_as(
            &controller.participant_settlements(),
            "server",
            ParticipantDisposition::CancelledAndJoined
        ),
        "{row}: the cancelled server was not settled as cancelled and joined"
    );
}

/// A cancellation issued while a drain is already running mints nothing, ends
/// the drain at once rather than at the grace it had left, reports cancellation
/// over every other ready cause, and leaves the account's other entries intact.
fn cancel_during_a_drain() {
    let row = "cancel during drain";
    let controller = runtime_schedule();
    let log = Arc::new(common::CallbackLog::default());
    let failing =
        common::ScriptedResource::new("displaced", &log).shutdown(common::Behavior::FailFrom(1));

    let observed = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .resource_budget(common::short_resource_budget())
        .resource(failing)
        .run(|| cancel_a_running_drain(&controller, row));

    let outcome = observed.expect_err(&format!("{row}: the teardown reported no failure"));
    assert_eq!(
        controller.shutdown_deadline_mints(),
        1,
        "{row}: the cancellation minted a second aggregate deadline"
    );
    let identities = lifecycle_kinds::aggregate_identities(&outcome);
    assert!(
        identities
            .iter()
            .any(|identity| identity == "resource:displaced|resource:shutdown|resource"),
        "{row}: the cancellation dropped the failure it displaced: {identities:?}"
    );
}

/// Hold one admitted request open, start the drain, then cancel it and read
/// what the cancelled server reported and how long it took.
fn cancel_a_running_drain(controller: &RuntimeController, row: &str) {
    let (admitted_tx, admitted) = std::sync::mpsc::sync_channel(1);
    let server = common::spawn_server_ready(admitting_router(Some(admitted_tx)), FIXTURE_BOUND)
        .expect("the drain-cancel fixture served");
    let mut peer = common::connect(server.local_addr()).expect("the held peer connected");
    common::write_stalled_body(&mut peer, None, "POST", HELD_ROUTE)
        .expect("write a head whose payload never finishes");
    admitted
        .recv_timeout(FIXTURE_BOUND)
        .expect("the held request reached production body admission");

    let handle = server.into_handle();
    handle.shutdown();
    wait_for_reading(controller, "connection");

    let started = std::time::Instant::now();
    handle.cancel();
    let joined = runtime::block_on(common::join_bounded(handle, FIXTURE_BOUND));
    let elapsed = started.elapsed();

    assert!(
        matches!(joined, Err(RuntimeError::Cancelled)),
        "{row}: cancellation did not outrank every other ready cause: {joined:?}"
    );
    assert!(
        elapsed < AGGREGATE_GRACE,
        "{row}: the cancellation waited {elapsed:?}, so it was given a fresh grace"
    );
    drop(peer);
    runtime::request_shutdown();
}
