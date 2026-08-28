use std::fmt::Debug;
use std::future::{Future, IntoFuture};
use std::sync::Arc;
use std::time::Duration;

#[cfg(feature = "ws")]
use camber::http::WsCloseCause;
use camber::http::mock::{InboundTerminal, ResponseOrigin, completion_vocabulary};
use camber::http::{ByteBoundary, DeadlineBoundary};
use camber::{
    LifecycleFailureKind, LifecycleFailures, LifecycleParticipant, LifecyclePhase, ResourceFailure,
    ResourceFailureKind, ResourcePhase, RuntimeError,
};

use crate::lifecycle_kinds::{kind_name, participant_name};
#[cfg(feature = "ws")]
use crate::rejection_kinds::{KINDS, label_of};

fn assert_flat_future<F>()
where
    F: Future<Output = Result<(), RuntimeError>>,
{
}

fn assert_server_handle_into_future<T>()
where
    T: IntoFuture<Output = Result<(), RuntimeError>, IntoFuture = camber::http::ServerHandleFuture>,
{
}

fn assert_lifecycle_enum_traits<T>()
where
    T: Copy + Clone + Debug + Eq + PartialEq,
{
}

// 3.T2: the canonical builder and every fallible terminal
#[test]
fn additive_lifecycle_api_compiles_through_exact_public_paths() {
    // The canonical owned server path, named exactly as documentation presents
    // it. Every terminal is fallible, and the two owner-producing terminals
    // hand back the same owners the free functions do.
    let _: fn(camber::http::Router) -> camber::http::ServerBuilder = camber::http::server;
    let _: fn(camber::http::HostRouter) -> camber::http::ServerBuilder = camber::http::server_hosts;
    let _: fn(
        camber::http::ServerBuilder,
        camber::http::ServerPolicy,
    ) -> camber::http::ServerBuilder = camber::http::ServerBuilder::policy;
    let _: fn(camber::http::ServerBuilder, &str) -> Result<(), RuntimeError> =
        camber::http::ServerBuilder::serve;
    let _: fn(camber::http::ServerBuilder, camber::net::Listener) -> Result<(), RuntimeError> =
        camber::http::ServerBuilder::serve_listener;
    let _: fn(
        camber::http::ServerBuilder,
        tokio::net::TcpListener,
    ) -> Result<camber::http::ServerHandleFuture, RuntimeError> =
        camber::http::ServerBuilder::serve_async;
    let _: fn(
        camber::http::ServerBuilder,
        tokio::net::TcpListener,
    ) -> Result<camber::http::ServerHandle, RuntimeError> =
        camber::http::ServerBuilder::serve_background;

    let _: fn(&camber::http::ServerHandle) = camber::http::ServerHandle::shutdown;
    let _: fn(camber::http::ServerHandle) -> camber::http::ServerHandleFuture =
        camber::http::ServerHandle::join;
    let _: fn(camber::http::ServerHandle) -> camber::http::ServerHandleFuture =
        camber::http::ServerHandle::shutdown_and_join;
    let _: fn(&camber::http::ServerHandleFuture) = camber::http::ServerHandleFuture::shutdown;
    let _: fn(&camber::http::ServerHandleFuture) = camber::http::ServerHandleFuture::cancel;

    assert_flat_future::<camber::http::ServerHandleFuture>();
    assert_server_handle_into_future::<camber::http::ServerHandle>();

    assert_lifecycle_enum_traits::<camber::http::mock::ServerStopEdge>();
    assert_lifecycle_enum_traits::<camber::http::mock::ConnectionOwnerEdge>();
    #[cfg(feature = "ws")]
    assert_lifecycle_enum_traits::<camber::http::mock::UpgradeOwnerEdge>();
    assert_lifecycle_enum_traits::<camber::http::mock::ConnectionFault>();
    assert_lifecycle_enum_traits::<camber::http::mock::ServerTaskFault>();
    assert_lifecycle_enum_traits::<camber::http::mock::SupervisorJoinProbe>();
}

// 1.T5: Invariant 4 — Camber promises an exact winner between competing
// lifecycle facts only when a public action, owner commit, or protocol
// acknowledgement establishes their order.
//
// The public surface is unchanged and stays flat: no supervisor, no
// orchestration object, no cancellation token. What changed is what a returned
// command means, and this is where that promise is checked without a server:
// the command is the commit, so its transition is available the instant it
// returns, and an event with no order against a committed one wins nothing.
#[test]
fn server_stop_commands_keep_public_flat_handle_contract() {
    // Stop authority remains two nullary methods on an owner and its join
    // future, each returning unit, and the join stays one flat result.
    let _: fn(&camber::http::ServerHandle) = camber::http::ServerHandle::shutdown;
    let _: fn(&camber::http::ServerHandle) = camber::http::ServerHandle::cancel;
    let _: fn(&camber::http::ServerHandleFuture) = camber::http::ServerHandleFuture::shutdown;
    let _: fn(&camber::http::ServerHandleFuture) = camber::http::ServerHandleFuture::cancel;
    assert_flat_future::<camber::http::ServerHandleFuture>();
    assert_server_handle_into_future::<camber::http::ServerHandle>();

    // The stop vocabularies are closed and comparable, which is what lets a
    // case name an edge or an event exactly rather than by description.
    assert_lifecycle_enum_traits::<camber::http::mock::ServerStopEdge>();
    assert_lifecycle_enum_traits::<camber::http::mock::ServerStopEvent>();
    assert_lifecycle_enum_traits::<camber::http::mock::ServerStopTransition>();
    assert_lifecycle_enum_traits::<camber::http::mock::ServerStopObservation>();

    let probe = camber::http::mock::server_stop_probe(Duration::from_secs(5));

    // An accepted command IS the commit. There is no later selection step in
    // which the phase could still be decided.
    let cancelled = probe.apply(camber::http::mock::ServerStopEvent::Cancel);
    assert_eq!(cancelled.phase, "cancelled");
    assert!(cancelled.changed);
    assert_eq!(probe.observed().phase, "cancelled");
    assert!(probe.observed().cancel_commanded);

    // A competing fact with no order over that commit wins nothing, and says so
    // in its own transition rather than leaving a reader to rank the two.
    let expired = probe.apply(camber::http::mock::ServerStopEvent::DeadlineExpiry);
    assert_eq!(expired.phase, "cancelled");
    assert!(!expired.changed);

    // The flat result is fixed once, by settlement, and leaves once.
    assert!(matches!(
        probe.take_result(),
        Some(Err(RuntimeError::Cancelled))
    ));
    assert!(probe.take_result().is_none());
    assert_eq!(probe.observed().outcome, "cancelled");
}

// ---------------------------------------------------------------------------
// The published vocabulary this plan changed, and the pages that state it.
// ---------------------------------------------------------------------------

const HTTP_REFERENCE: &str = include_str!("../../../../docs/reference/http.md");
const RUNTIME_REFERENCE: &str = include_str!("../../../../docs/reference/runtime.md");
const ERROR_REFERENCE: &str = include_str!("../../../../docs/reference/error.md");

/// The two results an owned server's flat join commits to for a stop.
///
/// A third outcome exists — the fatal error that ended the server — but it is
/// whatever the failing subsystem returned rather than a name this vocabulary
/// owns, so it has no row. These two are the ones the stop state itself
/// commits: the caller's cancellation, and the one aggregate deadline expiring.
const SERVER_STOP_RESULTS: [(RuntimeError, &str); 2] = [
    (RuntimeError::Cancelled, "operation cancelled"),
    (RuntimeError::Timeout, "operation timed out"),
];

/// Every cause a direct WebSocket connection can end on, with its rendered
/// word.
///
/// Closed, and every member reaches an application through
/// `RuntimeError::WebSocketClosed` — which is what the application acts on,
/// because a channel result flattens six answers into one.
#[cfg(feature = "ws")]
const WEBSOCKET_CAUSES: [(WsCloseCause, &str); 6] = [
    (WsCloseCause::PeerClosed, "peer closed"),
    (WsCloseCause::PeerDisconnected, "peer disconnected"),
    (WsCloseCause::ServerShutdown, "server shutdown"),
    (WsCloseCause::ServerCancelled, "server cancelled"),
    (WsCloseCause::ReceiverDropped, "receiver dropped"),
    (WsCloseCause::SendersDropped, "senders dropped"),
];

/// Every owner a direct lifecycle aggregate may name, in its rendering order.
///
/// The order is reproducible output, not precedence. It is asserted because two
/// identical runs must render identically; nothing reads the first entry as the
/// one to act on.
fn aggregate_owners() -> [(LifecycleParticipant, &'static str, &'static str); 4] {
    [
        (LifecycleParticipant::RootScope, "root-scope", "root-scope"),
        (
            LifecycleParticipant::BackgroundTask,
            "background-task",
            "background-task",
        ),
        (
            LifecycleParticipant::Resource(Arc::from("cache")),
            "resource:cache",
            "resource cache",
        ),
        (LifecycleParticipant::Exporter, "exporter", "exporter"),
    ]
}

/// Every way a direct participant can fail, with the words it renders.
///
/// One row per closed variant. `kind_name` is the wildcard-free matcher the
/// suite already owns, so a variant added later fails to compile there rather
/// than being silently skipped here.
fn aggregate_failure_kinds() -> [(LifecycleFailureKind, &'static str, &'static str); 7] {
    [
        (
            LifecycleFailureKind::DeadlineExceeded(DeadlineBoundary::RequestTotal),
            "deadline",
            "deadline exceeded: request_total",
        ),
        (LifecycleFailureKind::Cancelled, "cancelled", "cancelled"),
        (
            LifecycleFailureKind::TaskPanicked(Arc::from("boom")),
            "panicked",
            "panicked: boom",
        ),
        (
            LifecycleFailureKind::ScopeDrainTimeout { outstanding: 2 },
            "scope-drain",
            "children outstanding: 2",
        ),
        (
            LifecycleFailureKind::Resource(ResourceFailure::new(
                Arc::from("cache"),
                ResourcePhase::Shutdown,
                ResourceFailureKind::DeadlineExceeded,
            )),
            "resource",
            "resource cache",
        ),
        (
            LifecycleFailureKind::JoinLost(Arc::from("worker")),
            "join-lost",
            "join lost: worker",
        ),
        (
            LifecycleFailureKind::Operation(Arc::new(RuntimeError::Cancelled)),
            "operation",
            "operation cancelled",
        ),
    ]
}

/// The bounded name one completion origin is recorded under.
///
/// The production spelling is crate-private, so this is the mirror a test reads
/// through — deliberately a `match` with no `_` arm, exactly as the rejection
/// taxonomy's mirror is. A producer added later fails here until it is named,
/// and [`assert_completion_origins_are_closed`] then requires production's own
/// published vocabulary to carry the same name.
const fn origin_name(origin: ResponseOrigin) -> &'static str {
    match origin {
        ResponseOrigin::Application => "application",
        ResponseOrigin::Middleware => "middleware",
        ResponseOrigin::Router => "router",
        ResponseOrigin::Framework => "framework",
        ResponseOrigin::Upstream => "upstream",
        ResponseOrigin::StaticFile => "static-file",
        ResponseOrigin::ServerSentEvents => "sse",
        #[cfg(feature = "grpc")]
        ResponseOrigin::Grpc => "grpc",
        #[cfg(feature = "ws")]
        ResponseOrigin::WebSocket => "websocket",
        ResponseOrigin::Internal => "internal",
        ResponseOrigin::Protocol => "protocol",
    }
}

/// Every producer a completion may name as the origin of its answer.
const ORIGINS: [ResponseOrigin; ORIGIN_ROWS] = [
    ResponseOrigin::Application,
    ResponseOrigin::Middleware,
    ResponseOrigin::Router,
    ResponseOrigin::Framework,
    ResponseOrigin::Upstream,
    ResponseOrigin::StaticFile,
    ResponseOrigin::ServerSentEvents,
    #[cfg(feature = "grpc")]
    ResponseOrigin::Grpc,
    #[cfg(feature = "ws")]
    ResponseOrigin::WebSocket,
    ResponseOrigin::Internal,
    ResponseOrigin::Protocol,
];

/// How many producers this build compiles in.
///
/// Derived the way production derives it, because the two protocol handoffs are
/// feature-gated and a fixed width would only compile for one feature set.
const ORIGIN_ROWS: usize = 9 + cfg!(feature = "grpc") as usize + cfg!(feature = "ws") as usize;

/// Every cause that can take an admitted operation's commitment before a head.
///
/// Unordered on purpose: these are competing facts and whichever commits first
/// owns the operation. There is no rank over the set, so the table is a list of
/// members rather than a sequence.
const PRECOMMIT_CAUSES: [InboundTerminal; 11] = [
    InboundTerminal::ShutdownDeadline,
    InboundTerminal::ForcedCancellation,
    InboundTerminal::RouteBodyLimit,
    InboundTerminal::TransferBytes,
    InboundTerminal::BodyIdle,
    InboundTerminal::TransferIdle,
    InboundTerminal::TransferTotal,
    InboundTerminal::RequestTotal,
    InboundTerminal::Disconnect,
    InboundTerminal::SourceFailure,
    InboundTerminal::ResponseHead,
];

/// Freeze the recorded log and hand back the aggregate a caller receives.
///
/// The log is the only constructor, so nothing asserted below reads a value a
/// caller could not be given.
fn frozen(log: camber::__private::LifecycleFailureLog) -> LifecycleFailures {
    match log.into_error().expect("a recorded log mints an aggregate") {
        RuntimeError::Lifecycle(failures) => failures,
        other => panic!("a recorded lifecycle log returned {other:?}"),
    }
}

/// Assert the aggregate names every direct owner, renders each one, and elects
/// none.
fn assert_direct_aggregate_names_every_owner() {
    let mut log = camber::__private::LifecycleFailureLog::new();
    for (participant, _, _) in aggregate_owners() {
        log.record(
            participant,
            LifecyclePhase::GracefulDrain,
            LifecycleFailureKind::Cancelled,
        );
    }
    let failures = frozen(log);

    let recorded: Box<[Box<str>]> = failures
        .iter()
        .map(|failure| participant_name(failure.participant()).into())
        .collect();
    let published: Box<[Box<str>]> = aggregate_owners()
        .into_iter()
        .map(|(_, name, _)| name.into())
        .collect();
    assert_eq!(
        recorded, published,
        "the aggregate no longer renders its owners in the published order"
    );
    assert_eq!(failures.len(), 4, "the aggregate dropped a recorded owner");

    // Every entry is rendered, led by the count. An account that rendered one
    // chosen entry would be electing an owner for the operator to act on.
    let line = failures.to_string();
    assert!(
        line.starts_with("[4 recorded]"),
        "the aggregate's operator line no longer leads with its count: {line}"
    );
    for (_, _, displayed) in aggregate_owners() {
        assert!(
            line.contains(displayed),
            "the aggregate's operator line dropped {displayed:?}: {line}"
        );
    }
}

/// Assert every owner renders the one name the vocabulary gives it.
fn assert_every_owner_renders_its_bounded_name() {
    for (participant, name, displayed) in aggregate_owners() {
        assert_eq!(
            participant.to_string(),
            displayed,
            "{name} no longer renders its published name"
        );
    }
}

/// Assert every failure kind renders the operator words it publishes.
fn assert_every_failure_kind_renders_its_words() {
    for (kind, name, rendered) in aggregate_failure_kinds() {
        assert_eq!(
            kind_name(&kind),
            name,
            "a published failure kind no longer matches its category"
        );
        let mut log = camber::__private::LifecycleFailureLog::new();
        log.record(
            LifecycleParticipant::RootScope,
            LifecyclePhase::Finalize,
            kind,
        );
        let line = frozen(log).to_string();
        assert!(
            line.contains(rendered),
            "a rendered {name} failure no longer states {rendered:?}: {line}"
        );
    }
}

/// Assert the closed WebSocket close vocabulary renders, and that a refusal
/// before `101` is not one of its members.
///
/// The two vocabularies do not overlap. A handshake that failed is a typed
/// framework rejection, and only a connection that existed can end on a cause,
/// so an application matching on a cause never has to consider a refusal.
#[cfg(feature = "ws")]
fn assert_websocket_causes_are_closed_and_rendered() {
    for (cause, word) in WEBSOCKET_CAUSES {
        assert_eq!(
            cause.to_string(),
            word,
            "a published close cause no longer renders its word"
        );
        assert_eq!(
            RuntimeError::WebSocketClosed(cause).to_string(),
            format!("websocket closed: {word}"),
            "a typed closure no longer renders the cause it carries"
        );
        // No wildcard: a seventh cause fails to compile here rather than
        // slipping past the table above.
        match cause {
            WsCloseCause::PeerClosed
            | WsCloseCause::PeerDisconnected
            | WsCloseCause::ServerShutdown
            | WsCloseCause::ServerCancelled
            | WsCloseCause::ReceiverDropped
            | WsCloseCause::SendersDropped => {}
        }
    }

    let refusal = label_of(camber::http::RejectionKind::WebSocketHandshake);
    assert!(
        !WEBSOCKET_CAUSES
            .iter()
            .any(|(_, word)| word.replace(' ', "_") == refusal),
        "the handshake refusal {refusal:?} is also spelled as a close cause"
    );
    assert!(
        KINDS.contains(&camber::http::RejectionKind::WebSocketHandshake),
        "the closed rejection taxonomy dropped the WebSocket handshake refusal"
    );
}

/// Assert every pre-commit cause is named and none of them carries a rank.
fn assert_precommit_causes_are_closed() {
    for cause in PRECOMMIT_CAUSES {
        // No wildcard: a cause added later has to be placed here before this
        // compiles, which is what keeps the table as wide as the vocabulary.
        match cause {
            InboundTerminal::ShutdownDeadline
            | InboundTerminal::ForcedCancellation
            | InboundTerminal::RouteBodyLimit
            | InboundTerminal::TransferBytes
            | InboundTerminal::BodyIdle
            | InboundTerminal::TransferIdle
            | InboundTerminal::TransferTotal
            | InboundTerminal::RequestTotal
            | InboundTerminal::Disconnect
            | InboundTerminal::SourceFailure
            | InboundTerminal::ResponseHead => {}
        }
    }
    for (index, cause) in PRECOMMIT_CAUSES.iter().enumerate() {
        assert!(
            !PRECOMMIT_CAUSES[..index].contains(cause),
            "the pre-commit cause table names {cause:?} twice"
        );
    }
}

/// Assert the completion origin vocabulary is closed and published as such.
///
/// Production's own vocabulary is scraped rather than transcribed, so a
/// producer it gains reaches this assertion instead of a hand-written list that
/// never heard of it.
fn assert_completion_origins_are_closed() {
    let published = completion_vocabulary().origins;
    for origin in ORIGINS {
        assert!(
            published.contains(&origin_name(origin)),
            "the published origin vocabulary is missing {:?}",
            origin_name(origin)
        );
    }
    // One absence name plus one name per producer, and no repeats: a completion
    // that could be counted under two names for one producer would split one
    // time series in two.
    assert_eq!(
        published.len(),
        ORIGINS.len() + 1,
        "the published origin vocabulary is a different width than the closed set"
    );
    assert_eq!(
        published
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        published.len(),
        "the published origin vocabulary repeats a name"
    );
}

/// Assert the references state the same contract the types carry.
///
/// The documentation is the operator's copy of these closed sets, so a page
/// that stopped stating one would leave the two disagreeing.
fn assert_the_references_state_the_same_contract() {
    const PUBLISHED: [(&str, &str); 9] = [
        (
            HTTP_REFERENCE,
            "status, origin, rejection, delivery, connection_end, boundary, and shutdown",
        ),
        (
            HTTP_REFERENCE,
            "CallbackDisposition::OutstandingAfterForcedGrace",
        ),
        (
            RUNTIME_REFERENCE,
            "root scope, background task, resource, and exporter",
        ),
        (RUNTIME_REFERENCE, "ShutdownOwner::EXPORTER"),
        (
            RUNTIME_REFERENCE,
            "never reached as an aggregate failure entry",
        ),
        (RUNTIME_REFERENCE, "connection owner"),
        (ERROR_REFERENCE, "RuntimeError::Cancelled"),
        (ERROR_REFERENCE, "callback disposition"),
        (ERROR_REFERENCE, "ShutdownOwner::EXPORTER"),
    ];
    for (document, statement) in PUBLISHED {
        assert!(
            document.contains(statement),
            "the published documentation no longer states {statement:?}"
        );
    }

    // The ranked vocabulary this plan removed is gone from the operator pages
    // as well as from the crate. A page that still named it would send readers
    // to a surface that no longer exists.
    // The removed type's name is assembled rather than written, because the
    // repository-wide absence scan reads this file too and a literal here would
    // be indistinguishable from the surface it exists to prove is gone.
    const RETIRED: [&str; 5] = [
        "LifecycleFailures::primary",
        "strongest terminal",
        "same-turn precedence",
        concat!("Completion", "Terminal"),
        "primary lifecycle failure",
    ];
    for document in [HTTP_REFERENCE, RUNTIME_REFERENCE, ERROR_REFERENCE] {
        for name in RETIRED {
            assert!(
                !document.contains(name),
                "a published reference still names the retired {name:?}"
            );
        }
    }
}

// 16.T4 — the published lifecycle and error vocabulary this plan changed,
// locked at its public shape and against the pages that describe it.
//
// Five tables, each exhaustive by a wildcard-free match: the results an owned
// server's flat join commits to, the owners and failure kinds a direct
// lifecycle aggregate may name, the WebSocket close causes and the refusal that
// is deliberately not one of them, the causes that can take an operation's
// commitment before a head, and the producers a completion may name as its
// origin. A variant added or removed later fails to compile here.
//
// `LifecycleParticipant::Exporter` is locked as settlement-only vocabulary. The
// published contract has to say the variant is reached through
// `ShutdownOwner::EXPORTER` settlement and never as an aggregate failure entry,
// because the trace provider's shutdown is unbounded and hands nothing back, so
// there is no outcome an entry could be recorded from.
//
// What this does not claim is which production path constructs any of these.
// The production-driven mapping authorities are 1.T2, 4.T1–4.T4, 5.T1–9.T1,
// 10.T1–10.T4, and 11.T1–11.T3, and the absence of the removed ranked surface
// belongs to the independently compiled probes under
// `tests/fixtures/removed_lifecycle_api`.
#[test]
fn published_runtime_lifecycle_and_error_contracts_are_causal() {
    for (result, rendered) in SERVER_STOP_RESULTS {
        assert_eq!(
            result.to_string(),
            rendered,
            "a published server stop result no longer renders its words"
        );
    }
    assert_eq!(
        RuntimeError::DeadlineExceeded(DeadlineBoundary::RequestTotal).to_string(),
        "deadline exceeded: request_total",
        "a crossed request deadline no longer names its bound"
    );
    assert_eq!(
        RuntimeError::LimitExceeded(ByteBoundary::RequestBody).to_string(),
        "byte limit exceeded: request_body",
        "a crossed byte maximum no longer names its bound"
    );

    assert_direct_aggregate_names_every_owner();
    assert_every_owner_renders_its_bounded_name();
    assert_every_failure_kind_renders_its_words();
    #[cfg(feature = "ws")]
    assert_websocket_causes_are_closed_and_rendered();
    assert_precommit_causes_are_closed();
    assert_completion_origins_are_closed();
    assert_the_references_state_the_same_contract();
}
