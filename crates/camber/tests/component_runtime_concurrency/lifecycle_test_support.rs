//! Component proof for the ceiling the narrow test-support surface stops at.
//!
//! The surface walked here is every narrow controller this plan introduced for
//! a case to hold production with: the owner-local families under
//! `camber::http::mock`, and the root-scope settlement observation under
//! `camber::runtime_test_support`. Each names one owner's own edge vocabulary,
//! arms and releases only the edges it was given, and reads back only what that
//! owner published. None of them spans owners, submits a cause, or selects a
//! terminal — the pause protocol is the whole authority, and it is typed to the
//! family that owns it.
//!
//! `runtime_test_support::RuntimeController` is outside that surface on
//! purpose. It schedules the runtime's own root-scope and shutdown edges, it
//! predates this work, and no step here narrows it. The ceiling stated below is
//! the one over the narrow families, not a claim about that seam.
//!
//! Four parts. The refusal table walks every family that publishes an edge and
//! shows the trio answers for exactly the edge that was armed and refuses every
//! other, which is what "only its named local edge" means for a caller. The
//! families that hold nothing then state the same ceiling the other way: a
//! fault vocabulary that takes one arm, and read-only observations that answer
//! for their own owner or refuse. The surface table binds every operation these
//! controllers publish at its exact signature, so the vocabulary each one takes
//! is checked by the compiler rather than by prose. The live part finally puts
//! the gate itself on a real listener: a wait over an owner-local edge ends on
//! the held future's first look, not on the phase flip that reached the edge,
//! so a case released at the end of that wait is releasing onto a poll that has
//! not started.

use std::time::Duration;

use camber::RuntimeError;
use camber::http::mock::{
    BlockingWorkerController, BlockingWorkerEdge, ConnectionFault, ConnectionOwnerController,
    ConnectionOwnerEdge, MultipartOwnerController, MultipartOwnerEdge, RequestBodyOwnerController,
    ResponseCommitmentController, ResponseCommitmentEdge, ServerStopController, ServerStopEdge,
    ServerTaskController, ServerTaskFault, TransferOwnerController, TransferOwnerEdge,
    connection_owner, multipart_owner, request_body_owner, response_commitment, server_stop,
    server_task, static_file_worker, transfer_owner,
};
#[cfg(feature = "ws")]
use camber::http::mock::{
    UpgradeOwnerController, UpgradeOwnerEdge, WebSocketDirectionController, WebSocketDirectionEdge,
    WebSocketTerminalController, WebSocketTerminalEdge, retained_bridge, upgrade_owner,
};
use camber::runtime_test_support::{AdmittedScope, ScopeSettlementController, runtime_schedule};

use crate::common::{
    HELD_ROUTE, OwnerPoint, Owns, TempRoot, held_server, join_bounded, request_on_new_peer,
    reserve_registered, wait_until_paused_bounded,
};

/// The drain every live server in this file stops under.
///
/// Wide, because no claim here is about an expiring deadline: each row holds an
/// owner at an edge it armed and lets it go again, and a drain that could
/// expire underneath that would be a second, unnamed fact in the row.
const WIDE_DRAIN: Duration = Duration::from_secs(20);

/// The connection limit every live server here admits under.
///
/// More than the one peer any row opens, so nothing in this file ever waits on
/// a permit it did not mean to wait on.
const ADMISSION_LIMIT: usize = 4;

/// How long the released server has to stop and join.
///
/// Everything it owns has already been let go by the time the join is taken, so
/// an expired bound is an owner that never settled rather than a slow one.
const JOIN_BOUND: Duration = Duration::from_secs(10);

/// One published owner-local family, named by the two edges the table steps.
///
/// Two rather than one, because the claim is about what arming grants: an owner
/// holding `armed` still refuses `other`, so the trio answers per edge rather
/// than per owner.
struct Family<Point: OwnerPoint> {
    name: &'static str,
    armed: Point,
    other: Point,
}

/// Walk one family's refusal table against the owner that publishes it.
///
/// Four claims, and they are the whole of "pause/wait/release only its named
/// local edge": an unarmed edge cannot be released, an unarmed edge cannot be
/// waited on, an edge arms exactly once, and arming one edge grants nothing
/// over another. Written once and applied per family, so no family can quietly
/// answer a different question than the rest.
async fn assert_named_edge_ceiling<Point: OwnerPoint>(owner: &Point::Owner, family: Family<Point>) {
    let Family { name, armed, other } = family;

    let unreleased = armed.release_at(owner);
    assert!(
        matches!(unreleased, Err(RuntimeError::InvalidArgument(_))),
        "{name}: releasing {armed:?} before anything armed it answered {unreleased:?}"
    );

    let unwaited = armed.paused_at(owner).await;
    assert!(
        matches!(unwaited, Err(RuntimeError::InvalidArgument(_))),
        "{name}: waiting on the unarmed {armed:?} answered {unwaited:?} rather than \
         refusing"
    );

    armed
        .arm_at(owner)
        .unwrap_or_else(|error| panic!("{name}: arming {armed:?} was refused: {error}"));

    let rearmed = armed.arm_at(owner);
    assert!(
        matches!(rearmed, Err(RuntimeError::InvalidArgument(_))),
        "{name}: arming {armed:?} twice answered {rearmed:?}"
    );

    let sideways = other.release_at(owner);
    assert!(
        matches!(sideways, Err(RuntimeError::InvalidArgument(_))),
        "{name}: an owner holding {armed:?} released the unarmed {other:?} with \
         {sideways:?}"
    );
}

/// Every family whose owner is reached through one listener's own registration.
///
/// Each takes its own reserved address, because one scope answers to one script
/// and a second registration for it is refused. Nothing is served on them: the
/// refusal table is about what the controller grants, and a live owner would
/// only add a second thing that could reach an edge under the assertion.
async fn assert_listener_families() {
    let stop = reserve_registered(server_stop);
    assert_named_edge_ceiling(
        &Owns::<ServerStopController>::owner(&stop.controller()),
        Family {
            name: "server stop",
            armed: ServerStopEdge::BeforeCommit,
            other: ServerStopEdge::AfterCommit,
        },
    )
    .await;

    let connections = reserve_registered(connection_owner);
    assert_named_edge_ceiling(
        &Owns::<ConnectionOwnerController>::owner(&connections.controller()),
        Family {
            name: "connection owner",
            armed: ConnectionOwnerEdge::AfterAccept,
            other: ConnectionOwnerEdge::AfterPermit,
        },
    )
    .await;

    let commitment = reserve_registered(response_commitment);
    assert_named_edge_ceiling(
        &Owns::<ResponseCommitmentController>::owner(&commitment.controller()),
        Family {
            name: "response commitment",
            armed: ResponseCommitmentEdge::BeforeResponseCommit,
            other: ResponseCommitmentEdge::AfterResponseCommit,
        },
    )
    .await;

    let transfers = reserve_registered(transfer_owner);
    assert_named_edge_ceiling(
        &Owns::<TransferOwnerController>::owner(&transfers.controller()),
        Family {
            name: "transfer owner",
            armed: TransferOwnerEdge::BeforeSourcePoll,
            other: TransferOwnerEdge::BeforeTerminalCommit,
        },
    )
    .await;

    let sessions = reserve_registered(multipart_owner);
    assert_named_edge_ceiling(
        &Owns::<MultipartOwnerController>::owner(&sessions.controller()),
        Family {
            name: "multipart owner",
            armed: MultipartOwnerEdge::CommandAccepted,
            other: MultipartOwnerEdge::IngressAdvanced,
        },
    )
    .await;
}

/// Every family one upgraded connection retains, over the views that publish
/// them.
///
/// The bridge families have no single-family reservation of their own, because
/// no case reads a direction without the upgrade that retained it. The view
/// that names all four is what a bridge row already holds, so the table steps
/// its fields rather than asking for a narrower one that no caller wants.
#[cfg(feature = "ws")]
async fn assert_bridge_families() {
    let upgrades = reserve_registered(upgrade_owner);
    assert_named_edge_ceiling(
        &Owns::<UpgradeOwnerController>::owner(&upgrades.controller()),
        Family {
            name: "upgrade owner",
            armed: UpgradeOwnerEdge::AfterHandoffSubmitted,
            other: UpgradeOwnerEdge::BeforeTransferAcknowledge,
        },
    )
    .await;

    let bridge = reserve_registered(retained_bridge);
    assert_named_edge_ceiling(
        &Owns::<WebSocketDirectionController>::owner(&bridge.controller()),
        Family {
            name: "websocket direction",
            armed: WebSocketDirectionEdge::BeforeOutboundWrite,
            other: WebSocketDirectionEdge::InboundFrameArrived,
        },
    )
    .await;
    assert_named_edge_ceiling(
        &Owns::<WebSocketTerminalController>::owner(&bridge.controller()),
        Family {
            name: "websocket terminal",
            armed: WebSocketTerminalEdge::BeforeCommit,
            other: WebSocketTerminalEdge::AfterCommit,
        },
    )
    .await;
}

/// The offloaded-worker family, over the root its workers answer from.
///
/// A path scope rather than an address, because a blocking worker is reached
/// through the root it served rather than through a listener.
async fn assert_blocking_worker_family() {
    let root = TempRoot::new().expect("a root for the worker family");
    let workers = static_file_worker(root.path()).expect("watch a root's static-file workers");
    let worker = Owns::<BlockingWorkerController>::owner(&workers.worker);
    assert_named_edge_ceiling(
        &worker,
        Family {
            name: "blocking worker",
            armed: BlockingWorkerEdge::StaticFileWorkerEntered,
            other: BlockingWorkerEdge::StaticFileMetadataObserved,
        },
    )
    .await;
}

/// The two `http::mock` families that hold nothing, and what they publish
/// instead.
///
/// A fault vocabulary and a read-only observation. Neither has an edge, so
/// neither can be walked by the refusal table; what they claim here is the same
/// ceiling stated the other way — one script arms one fault, and the observer
/// reads counters it never wrote.
fn assert_unheld_families() {
    let tasks_port = reserve_registered(server_task);
    let tasks = Owns::<ServerTaskController>::owner(&tasks_port.controller());
    tasks
        .inject_once(ServerTaskFault::PanicNextOwnedTask)
        .expect("arm one server-task fault");
    let second = tasks.inject_once(ServerTaskFault::CancelNextOwnedTask);
    assert!(
        matches!(second, Err(RuntimeError::InvalidArgument(_))),
        "server task: a second fault over an unconsumed one answered {second:?}"
    );

    let connections_port = reserve_registered(connection_owner);
    let connections = Owns::<ConnectionOwnerController>::owner(&connections_port.controller());
    connections
        .inject_once(ConnectionFault::Accept(
            std::io::ErrorKind::ConnectionAborted,
        ))
        .expect("arm one admission fault");
    let crossed = connections.inject_once(ConnectionFault::Accept(std::io::ErrorKind::TimedOut));
    assert!(
        matches!(crossed, Err(RuntimeError::InvalidArgument(_))),
        "connection owner: a second admission fault answered {crossed:?}"
    );

    let bodies = reserve_registered(request_body_owner);
    let observed = Owns::<RequestBodyOwnerController>::owner(&bodies.controller()).observed();
    assert_eq!(
        (
            observed.frames_polled,
            observed.peak_retained_bytes,
            observed.permit_owners_dropped
        ),
        (0, 0, 0),
        "a listener nothing served published request-body work"
    );
}

/// The runtime's root-scope observation, and the reads it refuses.
///
/// The one narrow family this file reaches outside `http::mock`, and a third
/// that holds nothing: it publishes no edge, so naming a child is a claim on an
/// admission rather than a hold over one. What it does refuse is every read
/// taken through a controller no runtime ever attached. The observation answers
/// for the single runtime it was bound to or for none at all, so a case cannot
/// read one runtime's scope and report it as another's — which is the same
/// owner-local ceiling the edge families state through their trio.
fn assert_scope_settlement_family() {
    let unattached = runtime_schedule().scope_settlement();

    let drained = unattached.drained();
    assert!(
        matches!(drained, Err(RuntimeError::InvalidArgument(_))),
        "scope settlement: a controller no runtime attached read the scope as {drained:?}"
    );

    let reads: [(&str, fn(&AdmittedScope) -> Result<bool, RuntimeError>); 4] = [
        ("admitted", AdmittedScope::admitted),
        ("retained", AdmittedScope::retained),
        ("joined", AdmittedScope::joined),
        ("settled", AdmittedScope::settled),
    ];
    let positional = unattached.name_next_admission();
    let subsystem = unattached.name_subsystem("resource health");
    for (subject_name, subject) in [
        ("the next admission", &positional),
        ("a named subsystem", &subsystem),
    ] {
        for (fact, read) in reads {
            let answer = read(subject);
            assert!(
                matches!(answer, Err(RuntimeError::InvalidArgument(_))),
                "scope settlement: {subject_name} reported {fact} as {answer:?} with no \
                 runtime attached"
            );
        }
    }
}

/// The claim the deleted broad wait probe used to hold, over a live owner.
///
/// A checkpoint is reached and then looked at, and those are two moments rather
/// than one. `wait_until_paused` ends on the second: at the instant it returns,
/// the held owner has already taken a turn at its gate, so the release the case
/// records next lands on a poll that has not started. A wait that ended on the
/// phase flip would return with the count still at zero and put the case's next
/// two calls inside the production poll it meant to stand in front of.
///
/// Every published controller shares one gate, so this is stated once over the
/// owner that reaches an edge first and read through that owner's own count.
async fn assert_the_wait_ends_on_the_first_look() {
    let server = held_server(ADMISSION_LIMIT, WIDE_DRAIN);
    let connections = Owns::<ConnectionOwnerController>::owner(&server.controller);

    let uncounted = connections.polls(ConnectionOwnerEdge::AfterPermit);
    assert!(
        matches!(uncounted, Err(RuntimeError::InvalidArgument(_))),
        "an edge nothing armed reported {uncounted:?} rather than refusing to count"
    );

    connections
        .pause_once(ConnectionOwnerEdge::AfterAccept)
        .expect("arm the admission edge");
    let peer = request_on_new_peer(server.addr, HELD_ROUTE, "close").await;
    wait_until_paused_bounded(
        &server.controller,
        ConnectionOwnerEdge::AfterAccept,
        "the admitted connection never reached its accept edge",
    )
    .await;

    let looked = connections
        .polls(ConnectionOwnerEdge::AfterAccept)
        .expect("count the turns the held connection took");
    assert!(
        looked >= 1,
        "the wait ended on the phase flip rather than the first look: the held \
         owner had taken {looked} turns"
    );

    // The release is what proves the hold was real: the handler on the other
    // side of it has not entered, and only letting the edge go lets it.
    connections
        .release(ConnectionOwnerEdge::AfterAccept)
        .expect("release the admission edge");
    server
        .await_entry("the released connection's held request")
        .await;

    drop(peer);
    let stopped = join_bounded(server.handle.shutdown_and_join(), JOIN_BOUND).await;
    assert!(
        stopped.is_ok(),
        "the released server reported {stopped:?} rather than a clean stop"
    );
}

// 16.T1 — Invariant 21: private scheduling controllers prove only owner-local
// transitions, and every public lifecycle claim crosses a public API or a real
// transport boundary instead.
//
// The narrow surface below is the whole of what this plan left a case to hold
// production with. Each family's trio is typed to that family's own edge enum,
// so naming another owner's edge through it does not compile, and the refusal
// table shows that within one vocabulary the trio still answers only for the
// edge that was armed. The three families that hold nothing carry a fault
// vocabulary and two read-only observations instead: the fault refuses a second
// arm rather than widening, and the root-scope observation refuses every read
// taken through a controller no runtime attached.
//
// The surface table then binds every operation these controllers publish at its
// exact signature, so "no submit-cause, no generic poll-count, no stage-release,
// no terminal selection" is a compile-checked shape rather than a sentence.
//
// The last section is the claim the removed broad wait probe used to state, now
// stated over a real listener's own owner: the wait ends on the held future's
// first look. That gate is shared by every controller above, so the claim
// outlives the probe that used to hold it apart by hand.
#[camber::test]
async fn owner_local_controllers_pause_only_their_named_commit_edge() {
    assert_listener_families().await;
    #[cfg(feature = "ws")]
    assert_bridge_families().await;
    assert_blocking_worker_family().await;
    assert_unheld_families();
    assert_scope_settlement_family();
    assert_published_operations_name_only_their_owners_vocabulary();
    assert_the_wait_ends_on_the_first_look().await;
}

/// Every operation a narrow controller can act on production with.
///
/// A compile-checked enumeration rather than prose: each binding names one
/// operation at its exact signature, so the parameter every held operation
/// takes is that owner's own edge type and the only other vocabulary anywhere
/// in the set is an owner-local fault or an owner-local subject. The rest of
/// each controller's surface is a read-only observation, which acts on nothing.
/// There is no binding here for a cause, a rank, a terminal selection, or a
/// checkpoint that spans owners, because there is no such operation left on
/// this surface to bind.
///
/// Absence is owned by the independently compiled probes under
/// `tests/fixtures/removed_lifecycle_api` and by the removed-source scan; this
/// states the shape of what remains, which is what a reader of the surface
/// needs alongside them.
fn assert_published_operations_name_only_their_owners_vocabulary() {
    type Arm<Controller, Edge> = fn(&Controller, Edge) -> Result<(), RuntimeError>;
    type Count<Controller, Edge> = fn(&Controller, Edge) -> Result<usize, RuntimeError>;

    let _: Arm<ServerStopController, ServerStopEdge> = ServerStopController::pause_once;
    let _: Arm<ServerStopController, ServerStopEdge> = ServerStopController::release;

    let _: Arm<ConnectionOwnerController, ConnectionOwnerEdge> =
        ConnectionOwnerController::pause_once;
    let _: Arm<ConnectionOwnerController, ConnectionOwnerEdge> = ConnectionOwnerController::release;
    let _: Count<ConnectionOwnerController, ConnectionOwnerEdge> = ConnectionOwnerController::polls;
    let _: fn(&ConnectionOwnerController, ConnectionFault) -> Result<(), RuntimeError> =
        ConnectionOwnerController::inject_once;

    let _: Arm<ResponseCommitmentController, ResponseCommitmentEdge> =
        ResponseCommitmentController::pause_once;
    let _: Arm<ResponseCommitmentController, ResponseCommitmentEdge> =
        ResponseCommitmentController::release;
    let _: Arm<ResponseCommitmentController, ResponseCommitmentEdge> =
        ResponseCommitmentController::release_without_waking;
    let _: Count<ResponseCommitmentController, ResponseCommitmentEdge> =
        ResponseCommitmentController::polls;

    let _: Arm<TransferOwnerController, TransferOwnerEdge> = TransferOwnerController::pause_once;
    let _: Arm<TransferOwnerController, TransferOwnerEdge> = TransferOwnerController::release;
    let _: Arm<TransferOwnerController, TransferOwnerEdge> =
        TransferOwnerController::release_without_waking;
    let _: Count<TransferOwnerController, TransferOwnerEdge> = TransferOwnerController::polls;

    let _: Arm<MultipartOwnerController, MultipartOwnerEdge> = MultipartOwnerController::pause_once;
    let _: Arm<MultipartOwnerController, MultipartOwnerEdge> = MultipartOwnerController::release;
    let _: Count<MultipartOwnerController, MultipartOwnerEdge> = MultipartOwnerController::polls;

    let _: Arm<BlockingWorkerController, BlockingWorkerEdge> = BlockingWorkerController::pause_once;
    let _: Arm<BlockingWorkerController, BlockingWorkerEdge> = BlockingWorkerController::release;

    let _: fn(&ServerTaskController, ServerTaskFault) -> Result<(), RuntimeError> =
        ServerTaskController::inject_once;

    let _: fn(&ScopeSettlementController) -> AdmittedScope =
        ScopeSettlementController::name_next_admission;
    let _: fn(&ScopeSettlementController, &str) -> AdmittedScope =
        ScopeSettlementController::name_subsystem;
    let _: fn(&ScopeSettlementController) -> Result<bool, RuntimeError> =
        ScopeSettlementController::drained;
    let _: fn(&AdmittedScope) -> Result<bool, RuntimeError> = AdmittedScope::admitted;
    let _: fn(&AdmittedScope) -> Result<bool, RuntimeError> = AdmittedScope::retained;
    let _: fn(&AdmittedScope) -> Result<bool, RuntimeError> = AdmittedScope::joined;
    let _: fn(&AdmittedScope) -> Result<bool, RuntimeError> = AdmittedScope::settled;

    #[cfg(feature = "ws")]
    {
        let _: Arm<UpgradeOwnerController, UpgradeOwnerEdge> = UpgradeOwnerController::pause_once;
        let _: Arm<UpgradeOwnerController, UpgradeOwnerEdge> = UpgradeOwnerController::release;

        let _: Arm<WebSocketDirectionController, WebSocketDirectionEdge> =
            WebSocketDirectionController::pause_once;
        let _: Arm<WebSocketDirectionController, WebSocketDirectionEdge> =
            WebSocketDirectionController::release;
        let _: Arm<WebSocketDirectionController, WebSocketDirectionEdge> =
            WebSocketDirectionController::release_without_waking;
        let _: Count<WebSocketDirectionController, WebSocketDirectionEdge> =
            WebSocketDirectionController::polls;

        let _: Arm<WebSocketTerminalController, WebSocketTerminalEdge> =
            WebSocketTerminalController::pause_once;
        let _: Arm<WebSocketTerminalController, WebSocketTerminalEdge> =
            WebSocketTerminalController::release;
    }
}
