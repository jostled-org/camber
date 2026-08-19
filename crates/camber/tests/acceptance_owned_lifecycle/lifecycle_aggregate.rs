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

use camber::http::{
    BodyAdmission, BodyAdmissionContext, Request, Response, Router, StreamResponse, StreamSender,
};
use camber::runtime_test_support::{
    ParticipantDisposition, ParticipantSettlement, RuntimeController, ShutdownDeadlineReading,
    runtime_schedule,
};
use camber::{RuntimeError, runtime};
use std::io::Write;
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
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

/// The route a registered upgrade bridge is served on.
const BRIDGE_ROUTE: &str = "/bridge";

/// The route a streamed response is produced on.
const STREAM_ROUTE: &str = "/stream";

/// The maximum the witnessing admission policy selects.
///
/// Comfortably above the stalled head's declared length, so nothing this row
/// asserts turns on a byte ceiling.
const ADMITTED_BODY_MAX: usize = 64 * 1024;

/// Every owner that consults the shared expiry during a graceful teardown.
///
/// Read as an exact set rather than as a lower bound: an owner that stops
/// reading and an owner nobody declared both fail 18.T1, where a containment
/// check would only have caught the first. The registered resource is matched
/// by prefix because it carries its own name.
const REQUIRED_READERS: [&str; 4] = ["server", "connection", "root-scope", "executor"];

/// What one row's router registers beyond the two held routes.
///
/// Every row here serves the same held pair, and what varies between them is
/// which production boundary needs a witness. One options value keeps that
/// variation at the call rather than in a third and fourth copy of the router.
#[derive(Default)]
struct RouterParts {
    /// Where production body admission reports the held request.
    admitted: Option<SyncSender<()>>,
    /// Where the permits production takes are counted out and back.
    permits: Option<PermitWitness>,
    /// Where a streamed response ships the producer its handler was given.
    producers: Option<SyncSender<StreamSender>>,
    /// Whether a registered upgrade bridge is served.
    bridge: bool,
}

/// What one row observed about the permits production took and handed back.
///
/// Counted as a pool rather than as a single release, because the claim is that
/// every permit production took is gone — not that some particular request's
/// was, which a row would have to know the exact admission count to state.
#[derive(Clone, Default)]
struct PermitWitness {
    /// How many admitted permits have reached their release.
    released: Arc<AtomicUsize>,
    /// How many are still outstanding.
    outstanding: Arc<AtomicUsize>,
}

impl PermitWitness {
    /// The probe production is handed for one admitted request.
    fn probe(&self) -> common::PermitProbe {
        common::pooled_permit_probe(&self.released, &self.outstanding)
    }

    /// Assert production took at least one permit and handed every one back.
    fn assert_settled(&self, row: &str) {
        assert!(
            self.released.load(Ordering::SeqCst) > 0,
            "{row}: no admitted request ever carried a permit, so nothing was released"
        );
        common::assert_released(&self.outstanding, 0, &format!("{row}: outstanding permits"));
    }
}

/// A router that answers a bodyless probe at once and reads a payload on the
/// same path.
///
/// The `POST` arm is what holds a connection open: its body owner keeps polling
/// a payload the peer never finishes, which is the admitted request that has to
/// observe the graceful transition and read the shared expiry.
fn answering_router() -> Router {
    row_router(RouterParts::default())
}

/// [`answering_router`] plus whatever `parts` asks this row to serve.
///
/// A row that shuts a server down while one request is being read has to know
/// that request is already being read. Admission is where production decides
/// exactly that, so the `admitted` witness is armed there rather than inferred
/// from a socket write the accept loop may not have reached yet.
fn row_router(parts: RouterParts) -> Router {
    let mut router = Router::new();
    router.get(HELD_ROUTE, |_req: &Request| async move {
        Response::text(200, "held").expect("a valid status")
    });
    router.post(HELD_ROUTE, |_req: &Request| async move {
        Response::text(200, "read").expect("a valid status")
    });
    add_producer_route(&mut router, parts.producers);
    add_bridge_route(&mut router, parts.bridge);
    admitting_policy(router, parts.admitted, parts.permits)
}

/// Serve a streamed response whose producer leaves for the fixture that owns it.
///
/// The sender is shipped out rather than driven from inside the handler. A
/// cancellation releases the response owner production holds, and the producer's
/// own end of that channel is the only place the release is observable at all.
fn add_producer_route(router: &mut Router, producers: Option<SyncSender<StreamSender>>) {
    let Some(producers) = producers else {
        return;
    };
    router.get_stream(STREAM_ROUTE, move |_req: &Request| {
        let (response, sender) = StreamResponse::new(200);
        // Dropped rather than reported: this is production's own thread, and
        // the row takes exactly one producer.
        let _shipped = producers.try_send(sender);
        Box::pin(async move { response })
    });
}

/// Serve a WebSocket route whose bridge ends when its peer closes.
///
/// The bridge is what makes `LifecycleParticipant::Upgrade` reachable: a
/// registered bridge outlives the response head that created it and settles its
/// own connection, which is a different disposition from the connection's.
#[cfg(feature = "ws")]
fn add_bridge_route(router: &mut Router, bridge: bool) {
    if !bridge {
        return;
    }
    router.ws(
        BRIDGE_ROUTE,
        |_req: &Request, mut conn: camber::http::WsConn| {
            while conn.recv().is_some() {}
            Ok(())
        },
    );
}

/// A build with no `ws` feature registers no bridge, and reaches no upgrade.
#[cfg(not(feature = "ws"))]
fn add_bridge_route(_router: &mut Router, _bridge: bool) {}

/// Arm the production body-admission boundary with the witnesses this row asked
/// for, or leave the router without a policy at all.
fn admitting_policy(
    router: Router,
    admitted: Option<SyncSender<()>>,
    permits: Option<PermitWitness>,
) -> Router {
    if admitted.is_none() && permits.is_none() {
        return router;
    }
    router.body_admission(move |_context: &BodyAdmissionContext<'_>| {
        if let Some(admitted) = admitted.as_ref() {
            // Dropped rather than reported: the row may already have taken its
            // one notification, and this is production's own thread.
            let _witnessed = admitted.try_send(());
        }
        Ok(match permits.as_ref() {
            Some(permits) => BodyAdmission::with_permit(ADMITTED_BODY_MAX, permits.probe()),
            None => BodyAdmission::new(ADMITTED_BODY_MAX),
        })
    })
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

/// Wait until `owner` has settled, failing with the whole inventory rather than
/// with the absence alone.
fn wait_for_settlement(controller: &RuntimeController, owner: &str) {
    let settled = common::poll_until(common::OBSERVATION_BOUND, || {
        settled_at_all(&controller.participant_settlements(), owner)
    });
    assert!(
        settled,
        "{owner} never settled; settlements: {:?}",
        settlement_rows(&controller.participant_settlements())
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

/// Whether `owner` settled at all, whichever disposition it reached.
fn settled_at_all(settlements: &[ParticipantSettlement], owner: &str) -> bool {
    settlements
        .iter()
        .any(|settled| settled.participant() == owner)
}

/// Admit one request whose payload never finishes, and hand back the peer that
/// holds it open.
fn hold_one_request(addr: SocketAddr, admitted: &Receiver<()>) -> TcpStream {
    let mut peer = common::connect(addr).expect("the held peer connected");
    common::write_stalled_body(&mut peer, None, "POST", HELD_ROUTE)
        .expect("write a head whose payload never finishes");
    admitted
        .recv_timeout(FIXTURE_BOUND)
        .expect("the held request reached production body admission");
    peer
}

/// Ask for the streamed response and take the producer its handler shipped.
fn hold_one_stream(
    addr: SocketAddr,
    producers: &Receiver<StreamSender>,
) -> (TcpStream, StreamSender) {
    let mut peer = common::connect(addr).expect("the streaming peer connected");
    peer.write_all(format!("GET {STREAM_ROUTE} HTTP/1.1\r\nHost: localhost\r\n\r\n").as_bytes())
        .expect("write the streamed request head");
    peer.flush().expect("flush the streamed request head");
    let producer = producers
        .recv_timeout(FIXTURE_BOUND)
        .expect("the streamed response reached its handler and shipped its producer");
    (peer, producer)
}

/// Open a registered upgrade bridge and hand back the peer that holds it.
#[cfg(feature = "ws")]
fn hold_one_bridge(addr: SocketAddr) -> TcpStream {
    let mut peer = common::start_upgrade(addr, BRIDGE_ROUTE);
    let head = common::read_until_double_crlf(&mut peer);
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "the bridge fixture refused its own handshake: {head}"
    );
    peer
}

// ---------------------------------------------------------------------------
// 18.T1
// ---------------------------------------------------------------------------

/// 18.T1
#[test]
fn first_graceful_transition_mints_one_deadline_and_nested_owners_never_restart_it() {
    let controller = runtime_schedule();
    let log = Arc::new(common::CallbackLog::default());

    let second_at = staged_graceful_transitions(&controller, &log)
        .expect("the deadline fixture's runtime tore down cleanly");

    assert_eq!(
        controller.shutdown_deadline_mints(),
        1,
        "more than one aggregate shutdown deadline was minted"
    );
    let mint = controller
        .shutdown_deadline_mint()
        .expect("no graceful transition minted an aggregate deadline");
    assert!(
        mint.at() < second_at,
        "the deadline was minted at the second transition, not at the first"
    );
    assert!(
        mint.expiry() < second_at + AGGREGATE_GRACE,
        "the second transition was given a fresh grace of its own"
    );
    assert_eq!(
        mint.grace(),
        AGGREGATE_GRACE,
        "the minted deadline was not the configured grace away from its transition"
    );

    assert_every_owner_read(&controller, mint.expiry());
}

/// Every reading is the one minted expiry, and the owners that read are exactly
/// the ones this row declares.
fn assert_every_owner_read(controller: &RuntimeController, expiry: tokio::time::Instant) {
    let readings = controller.shutdown_deadline_readings();
    let owners = reading_owners(&readings);
    assert!(
        !readings.is_empty(),
        "no framework owner read the aggregate deadline"
    );
    for reading in &readings {
        assert_eq!(
            reading.expiry(),
            expiry,
            "{} read a different expiry from the one the first transition minted; owners: {owners:?}",
            reading.participant(),
        );
    }
    for expected in REQUIRED_READERS {
        assert!(
            owners.contains(&expected),
            "{expected} never read the shared expiry; owners: {owners:?}"
        );
    }
    assert!(
        owners.iter().any(|owner| owner.starts_with("resource ")),
        "no registered resource read the shared expiry; owners: {owners:?}"
    );
    let undeclared: Box<[&&str]> = owners
        .iter()
        .filter(|owner| !REQUIRED_READERS.contains(owner) && !owner.starts_with("resource "))
        .collect();
    assert!(
        undeclared.is_empty(),
        "an owner outside this row's declared inventory read the shared expiry: {undeclared:?}"
    );
}

/// Serve a runtime whose server, admitted request, registered bridge, background
/// child, and registered resource are all live, then take repeated graceful
/// transitions at distinct instants. Hands back the instant of the second one.
///
/// The instants are separated by production observations rather than by sleeps:
/// the second request is issued only once the admitted connection has been
/// observed reading the expiry the first transition minted, which is real
/// elapsed time and a real ordering edge.
///
/// The clock is the production one, running. Pausing Tokio's timer needs a
/// current-thread runtime and virtual time nothing advances on its own, and this
/// row is a real multi-thread runtime serving real listeners over real peers.
/// Nothing here is asserted against wall-clock arithmetic: the claims are that
/// exactly one mint happened, that it happened before the second transition, and
/// that every reading is that one instant.
fn staged_graceful_transitions(
    controller: &RuntimeController,
    log: &Arc<common::CallbackLog>,
) -> Result<tokio::time::Instant, RuntimeError> {
    runtime::builder()
        .with_test_schedule(controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .health_interval(common::TICK)
        .resource_budget(common::short_resource_budget())
        .resource(common::ScriptedResource::new("deadline-reader", log))
        .run(|| staged_transitions_body(controller))
}

/// The body of [`staged_graceful_transitions`], inside its runtime.
fn staged_transitions_body(controller: &RuntimeController) -> tokio::time::Instant {
    let (admitted_tx, admitted) = std::sync::mpsc::sync_channel(1);
    let server = common::spawn_server_ready(
        row_router(RouterParts {
            admitted: Some(admitted_tx),
            bridge: true,
            ..RouterParts::default()
        }),
        FIXTURE_BOUND,
    )
    .expect("the deadline fixture served");
    let child = camber::spawn_async(async {
        camber::runtime_test_support::wait_scope_closing().await;
    });
    let addr = server.local_addr();
    let mut peer = hold_one_request(addr, &admitted);
    let bridge = open_bridge_peer(addr);

    // The first transition, and the only mint this row allows.
    let handle = server.into_handle();
    handle.shutdown();
    wait_for_reading(controller, "connection");
    // A later instant, and a transition that must find the expiry already fixed
    // rather than start a second one.
    let second_at = tokio::time::Instant::now();
    runtime::request_shutdown();

    close_bridge_peer(bridge);
    drop(common::read_http_response_bounded(&mut peer));
    drop(peer);
    drop(runtime::block_on(common::join_bounded(
        handle,
        FIXTURE_BOUND,
    )));
    runtime::block_on(common::join_bounded(child, FIXTURE_BOUND))
        .expect("the background child exited on scope closing");
    second_at
}

/// The bridge peer this row holds open across both transitions.
#[cfg(feature = "ws")]
fn open_bridge_peer(addr: SocketAddr) -> Option<TcpStream> {
    Some(hold_one_bridge(addr))
}

/// A build with no `ws` feature holds no bridge open.
#[cfg(not(feature = "ws"))]
fn open_bridge_peer(_addr: SocketAddr) -> Option<TcpStream> {
    None
}

/// Let the bridge end its own connection, so the drain has nothing left to
/// force.
fn close_bridge_peer(bridge: Option<TcpStream>) {
    let Some(mut bridge) = bridge else {
        return;
    };
    common::write_ws_close_frame(&mut bridge);
    drop(bridge);
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
    registered_upgrade_row();
    cancellable_work_row();
    panicking_child_row();
    resource_deadline_row();
    lost_resource_worker_row();
    non_preemptible_callback_row();
    exporter_row();
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

/// A registered upgrade bridge that ends on its peer's close: it settles as the
/// upgrade it is rather than as the connection that carried its handshake.
#[cfg(feature = "ws")]
fn registered_upgrade_row() {
    let row = "registered upgrade";
    let controller = runtime_schedule();
    let result = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .run(|| bridge_through_close(&controller));
    assert!(
        result.is_ok(),
        "{row}: a teardown whose bridge finished returned {result:?}"
    );
    let settlements = controller.participant_settlements();
    assert!(
        settled_as(&settlements, "upgrade", ParticipantDisposition::Completed),
        "{row}: the registered bridge never settled as an upgrade; settlements: {:?}",
        settlement_rows(&settlements)
    );
}

/// Serve one bridge, close it from the peer, and wait for production to settle
/// it before the server is torn down.
#[cfg(feature = "ws")]
fn bridge_through_close(controller: &RuntimeController) {
    let server = common::spawn_server_ready(
        row_router(RouterParts {
            bridge: true,
            ..RouterParts::default()
        }),
        FIXTURE_BOUND,
    )
    .expect("the upgrade fixture served");
    let bridge = hold_one_bridge(server.local_addr());
    close_bridge_peer(Some(bridge));
    wait_for_settlement(controller, "upgrade");
    server
        .shutdown_bounded(FIXTURE_BOUND)
        .expect("the upgrade fixture tore down");
    runtime::request_shutdown();
}

/// A build with no `ws` feature has no registered bridge, so no teardown in it
/// may name an upgrade at all.
#[cfg(not(feature = "ws"))]
fn registered_upgrade_row() {
    let row = "registered upgrade";
    let controller = runtime_schedule();
    let result = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .run(|| {
            let server = common::spawn_server_ready(answering_router(), FIXTURE_BOUND)
                .expect("the upgrade fixture served");
            server
                .shutdown_bounded(FIXTURE_BOUND)
                .expect("the upgrade fixture tore down");
            runtime::request_shutdown();
        });
    assert!(result.is_ok(), "{row}: teardown returned {result:?}");
    let settlements = controller.participant_settlements();
    assert!(
        !settled_at_all(&settlements, "upgrade"),
        "{row}: a build with no bridge settled an upgrade anyway; settlements: {:?}",
        settlement_rows(&settlements)
    );
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

/// The exporter settles as its own participant wherever a build has one.
///
/// Its production settle site compiles only under `otel`, so the row states both
/// halves of that. With the feature, teardown visits and releases the exporter;
/// without it there is no exporter at all, and a settlement naming one would be
/// a claim about an owner this build never had.
fn exporter_row() {
    let row = "exporter";
    let controller = runtime_schedule();
    let result = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .run(runtime::request_shutdown);
    assert!(
        result.is_ok(),
        "{row}: a teardown with nothing to fail returned {result:?}"
    );
    assert_exporter_settlement(row, &controller.participant_settlements());
}

/// A build with an exporter releases it and records that it did.
#[cfg(feature = "otel")]
fn assert_exporter_settlement(row: &str, settlements: &[ParticipantSettlement]) {
    assert!(
        settled_as(settlements, "exporter", ParticipantDisposition::Completed),
        "{row}: teardown never visited and released the exporter; settlements: {:?}",
        settlement_rows(settlements)
    );
}

/// A build with no exporter settles none.
#[cfg(not(feature = "otel"))]
fn assert_exporter_settlement(row: &str, settlements: &[ParticipantSettlement]) {
    assert!(
        !settled_at_all(settlements, "exporter"),
        "{row}: a build with no exporter settled one anyway; settlements: {:?}",
        settlement_rows(settlements)
    );
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
/// over every other ready cause, releases the permits and producers the drain
/// still held, and leaves the account's other entries intact.
fn cancel_during_a_drain() {
    let row = "cancel during drain";
    let controller = runtime_schedule();
    let log = Arc::new(common::CallbackLog::default());
    let failing =
        common::ScriptedResource::new("displaced", &log).shutdown(common::Behavior::FailFrom(1));
    let permits = PermitWitness::default();

    let observed = runtime::builder()
        .with_test_schedule(&controller)
        .shutdown_timeout(AGGREGATE_GRACE)
        .resource_budget(common::short_resource_budget())
        .resource(failing)
        .run(|| cancel_a_running_drain(&controller, row, &permits));

    let outcome = observed.expect_err(&format!("{row}: the teardown reported no failure"));
    assert_eq!(
        controller.shutdown_deadline_mints(),
        1,
        "{row}: the cancellation minted a second aggregate deadline"
    );
    assert_displaced_entry_retained(row, &outcome);
    permits.assert_settled(row);
}

/// The failure the cancellation displaced is still in the account, it is the
/// entry the account names as primary, and no entry reports the cancellation
/// itself.
///
/// Production keeps a cancelled owner's cause on that owner's own flat result:
/// cancelling is a control action a caller asked for, not a lifecycle failure
/// the runtime has to report, and the aggregate names the owners no caller holds
/// a handle for. So the primary a caller reads back here is the resource failure
/// the cancellation would have hidden had the account kept only one winner.
fn assert_displaced_entry_retained(row: &str, outcome: &RuntimeError) {
    let displaced = "resource:displaced|resource:shutdown|resource";
    let identities = lifecycle_kinds::aggregate_identities(outcome);
    assert!(
        identities.iter().any(|identity| identity == displaced),
        "{row}: the cancellation dropped the failure it displaced: {identities:?}"
    );
    let primary = lifecycle_kinds::entry_identity(lifecycle_kinds::aggregate_primary(outcome));
    assert_eq!(
        primary, displaced,
        "{row}: the account named {primary} as the entry to act on; entries: {identities:?}"
    );
    assert!(
        !identities
            .iter()
            .any(|identity| identity.ends_with("|cancelled")),
        "{row}: a control action the caller asked for was reported as a lifecycle \
         failure: {identities:?}"
    );
}

/// Hold one admitted request and one streamed response open, start the drain,
/// then cancel it and read what the cancelled server reported, how long it took,
/// and what it let go of.
fn cancel_a_running_drain(controller: &RuntimeController, row: &str, permits: &PermitWitness) {
    let (admitted_tx, admitted) = std::sync::mpsc::sync_channel(1);
    let (producer_tx, producers) = std::sync::mpsc::sync_channel(1);
    let server = common::spawn_server_ready(
        row_router(RouterParts {
            admitted: Some(admitted_tx),
            permits: Some(permits.clone()),
            producers: Some(producer_tx),
            bridge: false,
        }),
        FIXTURE_BOUND,
    )
    .expect("the drain-cancel fixture served");
    let addr = server.local_addr();
    let held = hold_one_request(addr, &admitted);
    let (streaming, producer) = hold_one_stream(addr, &producers);

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
    assert_producer_released(row, &producer);
    drop(held);
    drop(streaming);
    runtime::request_shutdown();
}

/// The response owner production held for the streamed answer is gone.
///
/// A producer whose receiving end is still owned accepts a frame; one whose
/// owner the cancellation released cannot, and reports the closure by name.
fn assert_producer_released(row: &str, producer: &StreamSender) {
    let sent = runtime::block_on(producer.send("after the cancellation"));
    assert!(
        matches!(sent, Err(RuntimeError::ChannelClosed)),
        "{row}: the cancelled response still owned its producer's channel: {sent:?}"
    );
}
