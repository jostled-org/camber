//! What a row needs to hold a real `Router::ws` callback in application code,
//! and to get a peer onto the bridge that started it.
//!
//! Both live here because both are the same claim seen from two ends. A row
//! about a callback still pending when its bridge settles needs a callback that
//! genuinely does not answer the endpoints that bridge closed, so only the
//! settlement deadline can cancel it, and a peer that genuinely completed the
//! upgrade handshake. A second copy of either is a second definition of what
//! "still in application code" and "upgraded" mean.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use camber::http::mock::{
    ConnectionOwnerController, ConnectionOwnershipEvent, ScopedRetainedCallback,
    ServerStopController, UpgradeOwnerController, WebSocketCallbackObservation,
    WebSocketTerminalController, retained_callback,
};
use camber::http::{Router, ServerHandle};
use tokio::net::TcpStream;

use super::http::serve_router_with_policy;
use super::ws_async::lifecycle_event;

/// The end of a callback gate a row holds. Dropping it is the release.
pub type CallbackRelease = tokio::sync::watch::Sender<()>;

/// The end of a callback gate a row's callbacks park on.
///
/// Cloneable because the router that carries it is built once and every
/// bridge it serves parks on the same gate: each callback future takes its own
/// clone, so parking holds no lock and suspends rather than blocking a thread.
#[derive(Clone)]
pub struct CallbackPark(tokio::sync::watch::Receiver<()>);

/// Suspend a callback in its own future until the row lets go of the gate's
/// release end.
///
/// The release is the sender dropping, never a value: a row that had to
/// remember to send one would leave the callback parked on every failure path,
/// and a parked callback holds the connection its row is finished with. Nothing
/// is ever sent, so the only change this can observe is that drop. A pending
/// park is cancellable: a bridge that drops the callback at its deadline drops
/// this wait with it.
pub async fn park_until_released(gate: &CallbackPark) {
    let mut parked = gate.0.clone();
    while parked.changed().await.is_ok() {}
}

/// The two ends of the gate a row's callback parks on.
///
/// The parked end is what the row's callback awaits through
/// [`park_until_released`]. The release end is what the row holds and
/// never sends on: dropping it is the only release there is, so a row that
/// unwinds past its last line still lets every parked callback return, and a
/// row whose claim is the moment of release drops it at that moment.
///
/// Deliberately not a Camber primitive. A callback parked on something Camber
/// has no part in is exactly the subject: closing its receive queue and its
/// send admission wakes nothing, which is what makes the row about the
/// settlement deadline rather than about a cooperative return.
pub fn callback_gate() -> (CallbackRelease, CallbackPark) {
    let (release, parked) = tokio::sync::watch::channel(());
    (release, CallbackPark(parked))
}

/// Hold a real peer write through a local terminal. Both Pending observations
/// come from the framed sink, not from assumed socket capacity.
pub async fn stall_callback_transport(
    sender: &camber::http::WsSender,
    receiver: camber::http::WsReceiver,
    controller: &ScopedRetainedCallback,
    context: &str,
) {
    let payload = bytes::Bytes::from(vec![7; 32 * 1024 * 1024]);
    lifecycle_event(context, sender.send_shared_binary(payload))
        .await
        .expect("admit stalled frame");
    lifecycle_event(context, controller.terminals.outbound_write_pending()).await;
    drop(receiver);
    lifecycle_event(context, controller.terminals.outbound_close_pending()).await;
}

/// Read until the peer leaves, discarding every message.
///
/// The body a callback runs once it has nothing left to say: it holds the
/// connection exactly as long as the peer does, and it suspends in every
/// receive rather than occupying a thread.
pub async fn drain_until_closed(connection: &mut camber::http::WsConn) {
    while connection.recv().await.is_some() {}
}

/// A WebSocket route that reads until its peer leaves, then returns.
///
/// The callback a row registers when the bridge is its subject and the callback
/// is not. Shaped as a route so a registration names it directly. The returned
/// future captures only the connection, never the borrowed request.
pub fn drain_ws(
    _: &camber::http::Request,
    mut connection: camber::http::WsConn,
) -> impl Future<Output = Result<(), camber::RuntimeError>> + Send + use<> {
    async move {
        drain_until_closed(&mut connection).await;
        Ok(())
    }
}

/// Open a peer and write the handshake on `path`, each step bounded.
///
/// The start of every upgrade a row drives, and the whole of one whose answer
/// is the row's own subject: a row that holds the registration short of its
/// acknowledgement reads a refusal here instead of a `101`.
pub async fn start_ws_upgrade(addr: SocketAddr, path: &str, context: &str) -> TcpStream {
    let mut peer = lifecycle_event(context, TcpStream::connect(addr))
        .await
        .unwrap_or_else(|error| panic!("{context}: connecting the upgrade peer failed: {error}"));
    write_ws_upgrade(&mut peer, path, context).await;
    peer
}

/// Open a peer and take it through the handshake on `path` to its `101`.
pub async fn upgraded_ws_peer(addr: SocketAddr, path: &str, context: &str) -> TcpStream {
    let mut peer = start_ws_upgrade(addr, path, context).await;
    expect_switching_protocols(&mut peer, context).await;
    peer
}

/// Open a peer whose close will be a TCP reset, and take it through the
/// handshake on `path` to its `101`.
///
/// Tokio-side, because the zero linger that produces the reset is only
/// reachable through a Tokio socket. Every step is bounded: an unlikely stall
/// in an unbounded write is a hung binary rather than a failed row.
pub async fn abortive_upgraded_ws_peer(addr: SocketAddr, path: &str, context: &str) -> TcpStream {
    let mut peer = lifecycle_event(context, super::http::abortive_tcp_socket().connect(addr))
        .await
        .unwrap_or_else(|error| panic!("{context}: connecting the abortive peer failed: {error}"));
    write_ws_upgrade(&mut peer, path, context).await;
    expect_switching_protocols(&mut peer, context).await;
    peer
}

/// Write the upgrade request for `path`, bounded.
async fn write_ws_upgrade(peer: &mut TcpStream, path: &str, context: &str) {
    lifecycle_event(
        context,
        tokio::io::AsyncWriteExt::write_all(peer, super::ws::ws_upgrade_request(path).as_bytes()),
    )
    .await
    .unwrap_or_else(|error| panic!("{context}: writing the handshake failed: {error}"));
}

/// Read the handshake's answer and require the `101`.
async fn expect_switching_protocols(peer: &mut TcpStream, context: &str) {
    let head = super::ws_async::read_async_http_head(peer, context).await;
    assert!(
        head.starts_with("HTTP/1.1 101"),
        "{context}: the upgrade was refused: {head}"
    );
}

/// Send the peer's close frame, which is a bridge's local terminal.
pub async fn close_ws_peer(peer: &mut TcpStream, context: &str) {
    super::ws_async::write_async_ws_frame(peer, super::ws_async::CLOSE, &[], context).await;
}

/// Every record this listener's bridges have published about their callbacks.
pub fn published_callbacks(
    controller: &ScopedRetainedCallback,
) -> Box<[WebSocketCallbackObservation]> {
    controller.upgrades.callbacks()
}

/// How many of `records` carry a settled disposition.
///
/// [`settled_callbacks`] without the copy, for a predicate polled until the
/// count it needs arrives.
pub fn settled_count(records: &[WebSocketCallbackObservation]) -> usize {
    records
        .iter()
        .filter(|record| record.disposition.is_some())
        .count()
}

/// The records among `records` that carry a settled disposition.
///
/// A bridge publishes one record when it fixes its join deadline and another
/// each time a transition narrows it; only the last carries a disposition. So
/// "how many callbacks have settled" is this count and never the record count.
pub fn settled_callbacks(
    records: &[WebSocketCallbackObservation],
) -> Box<[WebSocketCallbackObservation]> {
    records
        .iter()
        .copied()
        .filter(|record| record.disposition.is_some())
        .collect()
}

/// The one upgrade this listener's single connection took as its child.
pub fn transferred_child(controller: &ScopedRetainedCallback, context: &str) -> (u64, u64) {
    let transferred = super::http::transferred_upgrades(&controller.connections.observed());
    assert_eq!(
        transferred.len(),
        1,
        "{context}: exactly one upgrade transfer was expected: {transferred:?}"
    );
    transferred[0]
}

/// Assert every published record names `owner` as the upgrade above it.
///
/// The parent half of the callback-ownership claim, read from two writers
/// rather than inferred from one: the connection records the transfer, the
/// bridge records the callback, and a callback beneath a different upgrade — or
/// beneath none — disagrees here. Both the component row and the daemon-live
/// acceptance row make this claim, so the sentence that states it has one home
/// and cannot drift into two spellings of the same failure.
pub fn assert_callbacks_own(
    decisions: &[WebSocketCallbackObservation],
    owner: (u64, u64),
    context: &str,
) {
    assert!(
        !decisions.is_empty(),
        "{context}: nothing was published about the callback"
    );
    for decision in decisions {
        assert_eq!(
            (decision.connection, decision.upgrade),
            owner,
            "{context}: the callback names an upgrade its connection never transferred: {decision:?}"
        );
    }
}

/// Assert the fields a cancelled callback's WARN event owes: the disposition,
/// the transition behind it, and the cause its bridge committed.
///
/// The component row and the daemon-live row read the same event, so the
/// fields it must carry have one statement.
pub fn assert_cancelled_fields(
    event: &str,
    cause: impl std::fmt::Display,
    shutdown: &str,
    context: &str,
) {
    super::trace_capture::assert_field_value(event, "disposition", "cancelled", context);
    super::trace_capture::assert_field_value(event, "shutdown", shutdown, context);
    assert!(
        event.contains(&format!("cause={cause}")),
        "{context}: the event does not name the committed cause: {event}"
    );
}

/// What a listener had published at the instant one callback's captures were
/// dropped.
#[derive(Clone, Copy, Debug)]
pub struct AtDrop {
    /// How many upgrades had settled under their connections.
    pub upgrades_settled: usize,
    /// Whether a bridge on this listener had given its permit back.
    pub permit_released: bool,
    /// How many callback dispositions had been published.
    pub dispositions: usize,
    /// The stop phase the server had committed.
    pub phase: &'static str,
}

/// The owners a drop witness reads, cloned out of the row's listener scope.
pub struct Observers {
    stop: ServerStopController,
    connections: ConnectionOwnerController,
    upgrades: UpgradeOwnerController,
    terminals: WebSocketTerminalController,
}

impl Observers {
    /// Clone the owners a witness reads out of `controller`.
    pub fn of(controller: &ScopedRetainedCallback) -> Self {
        Self {
            stop: controller.stop.clone(),
            connections: controller.connections.clone(),
            upgrades: controller.upgrades.clone(),
            terminals: controller.terminals.clone(),
        }
    }

    /// Everything this listener has published that a dropped callback must
    /// precede.
    fn snapshot(&self) -> AtDrop {
        AtDrop {
            upgrades_settled: self
                .connections
                .observed()
                .events
                .iter()
                .filter(|event| {
                    matches!(
                        event,
                        ConnectionOwnershipEvent::ConnectionUpgradeSettled { .. }
                    )
                })
                .count(),
            permit_released: self.terminals.observed().permit_released,
            dispositions: settled_count(&self.upgrades.callbacks()),
            phase: self.stop.observed().phase,
        }
    }
}

/// Every drop witness one listener's callbacks captured, and how many times its
/// factory was entered.
///
/// The component rows and the daemon-live rows read the same destruction, so
/// what a dropped capture records has one definition.
#[derive(Clone)]
pub struct DropWitnesses {
    observers: Arc<Observers>,
    entered: Arc<AtomicUsize>,
    dropped: Arc<Mutex<Vec<AtDrop>>>,
}

impl DropWitnesses {
    pub fn new(observers: Observers) -> Self {
        Self {
            observers: Arc::new(observers),
            entered: Arc::new(AtomicUsize::new(0)),
            dropped: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The witness one factory entry hands its future.
    pub fn enter(&self) -> DropWitness {
        self.entered.fetch_add(1, Ordering::AcqRel);
        DropWitness(self.clone())
    }

    /// How many times the factory was entered.
    pub fn entered(&self) -> usize {
        self.entered.load(Ordering::Acquire)
    }

    /// What each witness saw when it went, in the order they went.
    pub fn dropped(&self) -> Box<[AtDrop]> {
        self.dropped
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .copied()
            .collect()
    }

    /// How many witnesses have gone, without copying what they saw.
    ///
    /// For a predicate polled until a drop arrives.
    pub fn dropped_count(&self) -> usize {
        self.dropped
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .len()
    }
}

/// One callback's capture: recording on drop is its only behavior.
///
/// A flag set before the callback's last statement would say the body reached
/// it. The drop says the captures themselves are gone, which is the claim:
/// returning, unwinding, and being cancelled all reach it, and nothing else does.
pub struct DropWitness(DropWitnesses);

impl Drop for DropWitness {
    fn drop(&mut self) {
        let at = self.0.observers.snapshot();
        self.0
            .dropped
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .push(at);
    }
}

/// A loopback listener with a retained-callback observer registered on its
/// address, not yet serving.
///
/// Split from serving because a row builds its router from the observer: what
/// its callbacks capture reads the controller this registers.
pub struct RetainedCallbackListener {
    listener: tokio::net::TcpListener,
    /// The address the observer is registered on.
    pub addr: SocketAddr,
    /// The observer over every owner this listener's server publishes.
    pub controller: ScopedRetainedCallback,
}

impl RetainedCallbackListener {
    /// Bind an ephemeral loopback port and register the observer on it.
    pub async fn bind() -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind the callback fixture");
        let addr = listener.local_addr().expect("read the fixture's address");
        let controller = retained_callback(addr).expect("register the callback observer");
        Self {
            listener,
            addr,
            controller,
        }
    }

    /// Serve `router` in the background under a drain bound of `drain`.
    pub fn serve(
        self,
        router: Router,
        drain: Duration,
    ) -> (SocketAddr, ScopedRetainedCallback, ServerHandle) {
        let policy = camber::http::ServerPolicy::default()
            .shutdown_timeout(drain)
            .expect("a positive drain bound");
        let handle = serve_router_with_policy(self.listener, router, policy);
        (self.addr, self.controller, handle)
    }
}
