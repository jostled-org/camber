//! Daemon-live proof that a server owns connections, and a connection owns
//! everything under it.
//!
//! Every row runs a real background server over real TCP peers. The barriers
//! are public or protocol: a command has returned, a `101` has been read, a peer
//! has been answered, or a permit has been reacquired by work that could only
//! run on it. Nothing here reads a private schedule to decide what the server
//! should report, and no sleep is used as ordering evidence.

#![cfg(feature = "ws")]

use std::net::SocketAddr;
use std::time::Duration;

use camber::http::mock::{
    ConnectionOwnershipEvent, ConnectionOwnershipObservation, ScopedOwnerTree, owner_tree,
};
use camber::http::{Request, Response, Router, WsConn};
use camber::runtime;

use crate::common::{
    assert_address_reused, await_live, registered_connections, upgraded_ws_peer,
    wait_until_paused_within,
};

/// How long a live observation has to arrive before the row fails.
const LIVE_BOUND: Duration = Duration::from_secs(10);

/// The route every row's plain request is answered on.
const ANSWER_ROUTE: &str = "/answer";

/// The route every row's upgrade is offered on.
const SOCKET_ROUTE: &str = "/ws";

/// A router with one answered route and one bridge that lives until its peer
/// closes.
fn owner_tree_router() -> Router {
    let mut router = Router::new();
    router.get(ANSWER_ROUTE, |_request: &Request| async {
        Response::text(200, "answered")
    });
    router.ws(SOCKET_ROUTE, |_request: &Request, mut conn: WsConn| {
        while conn.recv().is_some() {}
        Ok(())
    });
    router
}

/// Serve `router` on a fresh address under `limit` concurrent connections.
///
/// The listener is bound here rather than by the server builder so the row owns
/// the address it later proves is reusable.
fn owned_server(limit: usize) -> (SocketAddr, ScopedOwnerTree, camber::http::ServerHandle) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind the ownership fixture");
    listener
        .set_nonblocking(true)
        .expect("the ownership fixture's listener takes a Tokio reactor");
    let listener = tokio::net::TcpListener::from_std(listener)
        .expect("adopt the ownership fixture's listener");
    let addr = listener.local_addr().expect("read the fixture's address");
    let controller = owner_tree(addr).expect("register the ownership observer");
    let policy = camber::http::ServerPolicy::default()
        .connection_limit(limit)
        .expect("a positive connection limit")
        .shutdown_timeout(Duration::from_secs(20))
        .expect("a positive drain bound");
    let handle = camber::http::server(owner_tree_router())
        .policy(policy)
        .serve_background(listener)
        .expect("the owned server requires a Tokio runtime");
    (addr, controller, handle)
}

/// Open a peer, take one answer on it, and keep the connection open.
///
/// The plain-request counterpart of [`upgraded_ws_peer`]: the request child has
/// settled, and the connection that served it is still live because its peer
/// has not gone away. Only the head is read, so nothing here can be mistaken for
/// the peer having closed.
async fn holding_request_peer(addr: SocketAddr, context: &str) -> tokio::net::TcpStream {
    let mut peer = crate::common::request_on_new_peer(addr, ANSWER_ROUTE, "keep-alive").await;
    let head = crate::common::read_async_http_head(&mut peer, context).await;
    assert!(
        head.starts_with("HTTP/1.1 200"),
        "{context}: the request was refused: {head}"
    );
    peer
}

/// Fail if the record names any upgrade at server scope.
///
/// Checked by shape rather than by identity: no sibling upgrade event may
/// appear at all, whatever it names.
fn assert_no_server_scope_upgrade(observed: &ConnectionOwnershipObservation, context: &str) {
    for event in observed.events.iter() {
        assert!(
            !matches!(
                event,
                ConnectionOwnershipEvent::ServerUpgradeRegistered { .. }
                    | ConnectionOwnershipEvent::ServerUpgradeSettled { .. }
            ),
            "{context}: the server registry named an upgrade of its own: {event:?}"
        );
    }
}

/// Fail if any child names a parent the server never registered.
fn assert_every_child_nests(observed: &ConnectionOwnershipObservation, context: &str) {
    let registered = registered_connections(observed);
    for event in observed.events.iter() {
        let parent = match event {
            ConnectionOwnershipEvent::ConnectionRequestAdmitted { connection, .. }
            | ConnectionOwnershipEvent::ConnectionRequestSettled { connection, .. }
            | ConnectionOwnershipEvent::ConnectionUpgradeTransferred { connection, .. }
            | ConnectionOwnershipEvent::ConnectionUpgradeSettled { connection, .. } => *connection,
            _ => continue,
        };
        assert!(
            registered.contains(&parent),
            "{context}: {event:?} names a parent the server never registered"
        );
    }
}

/// The upgrade identity transferred to `connection`, if one was.
fn transferred_upgrade(observed: &ConnectionOwnershipObservation, connection: u64) -> Option<u64> {
    observed.events.iter().find_map(|event| match event {
        ConnectionOwnershipEvent::ConnectionUpgradeTransferred {
            connection: parent,
            upgrade,
        } if *parent == connection => Some(*upgrade),
        _ => None,
    })
}

/// Wait until the record holds `event`, under [`LIVE_BOUND`].
///
/// [`await_live`] named against one event, so a settlement that never arrives
/// says which one it was.
async fn await_event(controller: &ScopedOwnerTree, event: ConnectionOwnershipEvent, context: &str) {
    await_live(
        || controller.connections.observed().contains(event),
        LIVE_BOUND,
        &format!("{context}: {event:?} was never recorded"),
    )
    .await;
}

/// Where `event` sits in the record.
///
/// The record is written in the order production published it, so comparing two
/// positions is an ordering claim: waiting for two events one after the other
/// only says both arrived, and would pass just as well on a parent that settled
/// first.
fn position(
    observed: &ConnectionOwnershipObservation,
    event: ConnectionOwnershipEvent,
    context: &str,
) -> usize {
    observed
        .events
        .iter()
        .position(|recorded| *recorded == event)
        .unwrap_or_else(|| {
            panic!(
                "{context}: {event:?} is not in the record: {:?}",
                observed.events
            )
        })
}

/// Require `child` to have been published before `parent`.
fn assert_settled_before(
    observed: &ConnectionOwnershipObservation,
    child: ConnectionOwnershipEvent,
    parent: ConnectionOwnershipEvent,
    context: &str,
) {
    let child_position = position(observed, child, context);
    let parent_position = position(observed, parent, context);
    assert!(
        child_position < parent_position,
        "{context}: {parent:?} was published before its child's {child:?}: {:?}",
        observed.events
    );
}

/// Wait until one connection beyond `baseline` has registered, and name it.
///
/// The registry only ever grows, so the identity is read back after the wait
/// rather than carried out of it: the entry that satisfied the wait is still at
/// `baseline` when the wait returns.
async fn await_new_connection(controller: &ScopedOwnerTree, baseline: usize, context: &str) -> u64 {
    await_live(
        || registered_connections(&controller.connections.observed()).len() > baseline,
        LIVE_BOUND,
        &format!("{context}: no connection owner beyond the first {baseline} registered"),
    )
    .await;
    registered_connections(&controller.connections.observed())[baseline]
}

/// A peer queued behind a permit that a live connection is still holding.
///
/// Holding this is the negative half of Invariant 7: the peer is connected and
/// has asked for the answered route, and the only reason it has no owner is that
/// the permit is not free. Its answer, read later through [`Self::answered`], is
/// the reacquisition proof — nothing but a returned permit can produce it.
struct QueuedPeer {
    peer: tokio::net::TcpStream,
}

impl QueuedPeer {
    /// Queue one peer behind the limit and prove it is parked there.
    ///
    /// Two independent readings, because either alone can be argued with: the
    /// server suspended in its own permit wait, and the record still names only
    /// the `expected` owners it named before this peer connected. A permit
    /// released while a child was live would fail the first — production never
    /// reaches the wait — and a served peer would fail the second.
    async fn park(
        controller: &ScopedOwnerTree,
        addr: SocketAddr,
        expected: usize,
        context: &str,
    ) -> Self {
        controller
            .connections
            .pause_once(camber::http::mock::ConnectionOwnerEdge::PermitWaitPending)
            .expect("arm the connection limit's own wait");
        let peer = crate::common::request_on_new_peer(addr, ANSWER_ROUTE, "close").await;
        wait_until_paused_within(
            controller,
            camber::http::mock::ConnectionOwnerEdge::PermitWaitPending,
            LIVE_BOUND,
            &format!("{context}: a peer was admitted while a live child still held the permit"),
        )
        .await;
        let registered = registered_connections(&controller.connections.observed());
        assert_eq!(
            registered.len(),
            expected,
            "{context}: the queued peer took an owner while the permit was held: {registered:?}"
        );
        controller
            .connections
            .release(camber::http::mock::ConnectionOwnerEdge::PermitWaitPending)
            .expect("release the connection limit's own wait");
        Self { peer }
    }

    /// Read the answer this peer could only get on a returned permit.
    async fn answered(mut self, context: &str) {
        let answer = crate::common::read_peer_to_eof(&mut self.peer, context).await;
        assert!(
            answer.starts_with("HTTP/1.1 200"),
            "{context}: the queued peer was never served on the returned permit: {answer}"
        );
    }
}

/// Fail if this peer has already been given part of an answer.
///
/// A one-shot read, so it is a guard rather than a clock: it never waits, and
/// the only way it fails is that bytes really are on the peer's socket. Read
/// against the record assertion beside it — that one establishes the transfer,
/// and this one says the peer whose acknowledgement it precedes has heard
/// nothing yet.
fn assert_peer_unanswered(peer: &tokio::net::TcpStream, context: &str) {
    let mut answer = [0_u8; 1];
    match peer.try_read(&mut answer) {
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
        Ok(read) => {
            panic!("{context}: the peer was answered before the transfer was read: {read} byte(s)")
        }
        Err(error) => panic!("{context}: reading the held peer failed: {error}"),
    }
}

// 2.T2 — Invariant 6: the server task registry contains connection owners, not
// sibling request, upgrade, direction, or callback tasks.
//
// Two overlapping connections, one plain and one upgraded, so the registry is
// asked about both kinds at once. Every barrier is public or protocol: the
// answer has been read, the `101` has been read, the peers are released, the
// join has returned, and the address is bound again.
//
// The limit is two, which is what makes the registry's reaping observable as
// more than a record: with both owners live the limit is spent, so the peer
// queued behind it is served only if reaping gave both permits back.
#[camber::test]
async fn server_registry_reaps_connections_without_sibling_upgrade_tasks() {
    let context = "the overlapped registry";
    let (addr, controller, handle) = owned_server(2);

    let upgraded = upgraded_ws_peer(addr, SOCKET_ROUTE, context).await;
    let answered = holding_request_peer(addr, context).await;

    let observed = controller.connections.observed();
    assert_no_server_scope_upgrade(&observed, context);
    assert_every_child_nests(&observed, context);
    let connections = registered_connections(&observed);
    assert_eq!(
        connections.len(),
        2,
        "{context}: two peers registered {} connection owners: {:?}",
        connections.len(),
        observed.events
    );
    let upgraded_parent = connections
        .iter()
        .copied()
        .find(|connection| transferred_upgrade(&observed, *connection).is_some())
        .expect("one of the two connections transferred an upgrade");

    // Both owners are live, so the limit is spent. This peer has nowhere to go
    // until one of them is reaped.
    let queued = QueuedPeer::park(&controller, addr, 2, context).await;

    // Releasing both peers settles both owners, and each settles as the
    // connection it is.
    drop(upgraded);
    drop(answered);
    for connection in connections.iter().copied() {
        await_event(
            &controller,
            ConnectionOwnershipEvent::ServerConnectionSettled { connection },
            context,
        )
        .await;
    }
    // Reaping gave the slot back: this answer could not have been produced on
    // any other permit.
    queued.answered(context).await;
    let upgrade = transferred_upgrade(&controller.connections.observed(), upgraded_parent)
        .expect("the transfer this row already read stayed in the record");
    await_event(
        &controller,
        ConnectionOwnershipEvent::ConnectionUpgradeSettled {
            connection: upgraded_parent,
            upgrade,
        },
        context,
    )
    .await;
    assert_no_server_scope_upgrade(&controller.connections.observed(), context);

    handle.shutdown();
    let result = tokio::time::timeout(LIVE_BOUND, handle)
        .await
        .unwrap_or_else(|_| panic!("{context}: the server never joined"));
    assert!(result.is_ok(), "{context}: the server ended {result:?}");
    assert_address_reused(addr, context).await;
}

/// The request half of 2.T3, answering with the owners registered so far.
///
/// A peer that took its answer and stayed is a connection whose request child
/// has settled and whose own settlement has not happened. The permit must still
/// be spent on it: the child settling is not what returns the slot.
async fn request_child_holds_the_permit(controller: &ScopedOwnerTree, addr: SocketAddr) -> usize {
    let context = "the held request peer";
    let peer = holding_request_peer(addr, context).await;
    let connection = await_new_connection(controller, 0, context).await;

    let observed = controller.connections.observed();
    let request = observed
        .events
        .iter()
        .find_map(|event| match event {
            ConnectionOwnershipEvent::ConnectionRequestAdmitted {
                connection: parent,
                request,
            } if *parent == connection => Some(*request),
            _ => None,
        })
        .expect("the answered request was admitted under its connection");
    let request_settled = ConnectionOwnershipEvent::ConnectionRequestSettled {
        connection,
        request,
    };
    assert!(
        observed.contains(request_settled),
        "{context}: the answer was read before its request child settled: {:?}",
        observed.events
    );
    let connection_settled = ConnectionOwnershipEvent::ServerConnectionSettled { connection };
    assert!(
        !observed.contains(connection_settled),
        "{context}: a connection whose peer is still live settled anyway"
    );

    let queued = QueuedPeer::park(controller, addr, 1, context).await;

    // The peer's own close is the barrier: the connection ends, and only then is
    // the permit free.
    drop(peer);
    await_event(controller, connection_settled, context).await;
    let observed = controller.connections.observed();
    assert_settled_before(&observed, request_settled, connection_settled, context);
    queued.answered(context).await;

    registered_connections(&controller.connections.observed()).len()
}

/// The upgrade half of 2.T3, run after `baseline` owners have registered.
async fn upgrade_child_holds_the_permit(
    controller: &ScopedOwnerTree,
    addr: SocketAddr,
    baseline: usize,
) {
    let context = "the held upgrade peer";
    let upgraded = upgraded_ws_peer(addr, SOCKET_ROUTE, context).await;
    let connection = await_new_connection(controller, baseline, context).await;

    let observed = controller.connections.observed();
    let upgrade = transferred_upgrade(&observed, connection)
        .expect("the live upgrade was transferred to its connection");
    let upgrade_settled = ConnectionOwnershipEvent::ConnectionUpgradeSettled {
        connection,
        upgrade,
    };
    let connection_settled = ConnectionOwnershipEvent::ServerConnectionSettled { connection };

    // The child is live, so neither it nor its parent has settled. Read from the
    // record rather than inferred from a clock.
    assert!(
        !observed.contains(upgrade_settled),
        "{context}: a live bridge settled its upgrade owner"
    );
    assert!(
        !observed.contains(connection_settled),
        "{context}: a connection holding a live child settled anyway"
    );

    let queued = QueuedPeer::park(controller, addr, baseline + 1, context).await;

    // The peer's own close is the barrier: the child ends, the parent settles
    // after it, and only then is the permit free.
    drop(upgraded);
    await_event(controller, upgrade_settled, context).await;
    await_event(controller, connection_settled, context).await;
    let observed = controller.connections.observed();
    assert_settled_before(&observed, upgrade_settled, connection_settled, context);
    queued.answered(context).await;
}

// 2.T3 — Invariant 7 substrate: a connection cannot settle or release its permit
// before its request and upgrade children settle.
//
// The limit is one, so the permit is the whole claim, and each kind of child is
// held independently on the same server. In both halves a peer is queued behind
// the limit while the child is live: it can only be served once the child has
// settled and its parent has given the slot back, so an early release fails the
// row where it happens rather than at the end of it.
//
// Both halves run on one server because one runtime mints one graceful expiry —
// a second `shutdown()` here would drain under what the first left.
#[camber::test]
async fn connection_permit_releases_only_after_request_and_upgrade_owner_settle() {
    let context = "the single-permit upgrade";
    let (addr, controller, handle) = owned_server(1);

    let served = request_child_holds_the_permit(&controller, addr).await;
    upgrade_child_holds_the_permit(&controller, addr, served).await;

    handle.shutdown();
    let result = tokio::time::timeout(LIVE_BOUND, handle)
        .await
        .unwrap_or_else(|_| panic!("{context}: the server never joined"));
    assert!(result.is_ok(), "{context}: the server ended {result:?}");
    assert_address_reused(addr, context).await;
}

/// Hold one handshaking peer at its connection's transfer edge, and read the
/// parentage the record already holds there.
///
/// Answers with the peer, its connection, and the upgrade transferred to it. The
/// hold is released before returning, so the caller reads the `101` that the
/// answer beyond this edge releases.
async fn read_transfer_before_acknowledgement(
    controller: &ScopedOwnerTree,
    addr: SocketAddr,
    context: &str,
) -> (tokio::net::TcpStream, u64, u64) {
    controller
        .upgrades
        .pause_once(camber::http::mock::UpgradeOwnerEdge::AfterTransferRecorded)
        .expect("arm the connection's transfer edge");
    let mut peer = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect the held upgrade peer");
    tokio::io::AsyncWriteExt::write_all(
        &mut peer,
        crate::common::ws_upgrade_request(SOCKET_ROUTE).as_bytes(),
    )
    .await
    .expect("write the held handshake");
    wait_until_paused_within(
        controller,
        camber::http::mock::UpgradeOwnerEdge::AfterTransferRecorded,
        LIVE_BOUND,
        &format!("{context}: the offer never reached its connection"),
    )
    .await;

    // Held before the answer that releases the `101`, so everything read here is
    // established ahead of the acknowledgement: the transfer names this
    // connection as the parent, and no upgrade is registered beside it.
    assert_peer_unanswered(&peer, context);
    let held = controller.connections.observed();
    assert_no_server_scope_upgrade(&held, context);
    let connections = registered_connections(&held);
    assert_eq!(connections.len(), 1, "{context}: one peer, one connection");
    let connection = connections[0];
    let upgrade = transferred_upgrade(&held, connection).unwrap_or_else(|| {
        panic!(
            "{context}: the connection was answered without recording the transfer first: {:?}",
            held.events
        )
    });

    controller
        .upgrades
        .release(camber::http::mock::UpgradeOwnerEdge::AfterTransferRecorded)
        .expect("release the connection's transfer edge");
    (peer, connection, upgrade)
}

// 2.T4 — Invariant 7 handoff: the transfer is recorded before the protocol
// acknowledgement, and the child settles under the same connection.
//
// The peer is held between the transfer and the answer that releases its `101`,
// which is the only point where reading the record proves an order: the handler
// is parked on that answer, so the transfer this row reads is established ahead
// of any byte the peer could have seen. No server-scope upgrade event may exist
// at that point or at any later one.
#[camber::test]
async fn upgrade_handoff_remains_a_connection_child_before_101() {
    let context = "the pre-acknowledgement transfer";
    let (addr, controller, handle) = owned_server(1);

    let (mut peer, connection, upgrade) =
        read_transfer_before_acknowledgement(&controller, addr, context).await;

    let head = crate::common::read_async_http_head(&mut peer, context).await;
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "{context}: the held upgrade was refused: {head}"
    );

    let observed = controller.connections.observed();
    assert_no_server_scope_upgrade(&observed, context);
    assert_eq!(
        transferred_upgrade(&observed, connection),
        Some(upgrade),
        "{context}: the acknowledged upgrade is not the child the transfer recorded"
    );

    // Kept live across the acknowledgement, then released: settlement is under
    // the same parent, after the child, and the permit only comes back then.
    let queued = QueuedPeer::park(&controller, addr, 1, context).await;
    drop(peer);
    let upgrade_settled = ConnectionOwnershipEvent::ConnectionUpgradeSettled {
        connection,
        upgrade,
    };
    let connection_settled = ConnectionOwnershipEvent::ServerConnectionSettled { connection };
    await_event(&controller, upgrade_settled, context).await;
    await_event(&controller, connection_settled, context).await;
    assert_settled_before(
        &controller.connections.observed(),
        upgrade_settled,
        connection_settled,
        context,
    );
    queued.answered(context).await;

    handle.shutdown();
    let result = tokio::time::timeout(LIVE_BOUND, handle)
        .await
        .unwrap_or_else(|_| panic!("{context}: the server never joined"));
    assert!(result.is_ok(), "{context}: the server ended {result:?}");
    assert_address_reused(addr, context).await;
    runtime::request_shutdown();
}
