//! Who owns one upgraded connection, and when its `101` becomes real.
//!
//! The direct and proxied bridges differ in everything they do with a
//! transport and agree on everything about how they get one: the connection
//! takes the bridge as its own child before the response reaches the wire, a
//! connection with no upgrade transport refuses the upgrade, and neither bridge
//! frames against a peer that never saw the response. That sequence lives here,
//! once. What the handshake handed over lives beside it.

use super::super::body::HyperResponseBody;
use super::super::disconnect::DisconnectSignal;
use super::super::rejection::Rejected;
use super::super::server_lifecycle::{
    ConnectionLifecycle, ServerControl, UpgradeAdmission, UpgradeRegistration,
};
use super::framing::shutdown_client_transport;
use std::ops::ControlFlow;
use std::sync::Arc;

/// The client-side WebSocket transport both bridges take over after the `101`.
///
/// Named here rather than beside framing, because taking it over is what this
/// file does: every one of these is produced by the upgrade below and handed to
/// exactly one bridge.
pub(super) type ClientWs =
    tokio_tungstenite::WebSocketStream<hyper_util::rt::TokioIo<hyper::upgrade::Upgraded>>;

/// What an owned server contributes to a bridge it is about to register.
///
/// Two parts, because the two bridges share only one of them: every bridge
/// opens through [`BridgeOpening`], and only a direct bridge runs the callback
/// [`CallbackContext`] is for.
pub(super) struct BridgeAttachment {
    opening: BridgeOpening,
    callback: CallbackContext,
}

/// What every bridge spends on opening: the control watch it stops on, and
/// the gate that says whether its `101` reached the wire.
///
/// A field added here reaches both bridges through [`open_bridge`], so neither
/// can forget it.
pub(super) struct BridgeOpening {
    control: tokio::sync::watch::Receiver<ServerControl>,
    dispatch: super::super::server_lifecycle::UpgradeDispatchGate,
}

/// What a direct bridge's inline callback inherits from its connection.
///
/// Moved out whole, before the opening is spent: every value is the
/// connection's existing shared handle, so the callback takes it rather than a
/// copy.
pub(super) struct CallbackContext {
    /// The Camber runtime this connection is served under, if it is served
    /// under one.
    ///
    /// Carried by value because the launch below crosses a bare `tokio::spawn`,
    /// which no task-local follows. It is the runtime's existing shared
    /// authority — the same `Arc` task admission reads — not a second one
    /// minted here. Captured on the connection task, which is the whole of the
    /// contract's suppression rule: a server with no Camber runtime over it —
    /// bare-Tokio serving — carries `None` here, and its callback cannot pick
    /// up another server's context from the thread that polls it.
    pub(super) runtime: Option<Arc<crate::runtime_state::RuntimeInner>>,
    /// The causal stop state this connection's server commits its control facts
    /// into.
    ///
    /// Carried for the same reason as the runtime authority above: the bridge
    /// runs past a bare `tokio::spawn`, and the phase an inline callback's
    /// settlement deadline is fixed from has to be the one the server committed, not
    /// the one a watch notification happened to have delivered.
    pub(super) stop: Option<Arc<super::super::server_stop::ServerStopState>>,
    /// The place in the owner tree this bridge will be taken as.
    ///
    /// Handed down rather than looked up, for the same reason as the two above:
    /// the connection minted it before it offered the bridge a place, so a
    /// bridge names the child its parent recorded rather than one it invented
    /// for itself after the fact.
    pub(super) owner: super::super::server_lifecycle::UpgradeIdentity,
}

impl BridgeAttachment {
    /// Give up the attachment for its two parts.
    pub(super) fn into_parts(self) -> (BridgeOpening, CallbackContext) {
        (self.opening, self.callback)
    }

    /// Give up the attachment for the opening alone.
    ///
    /// For a bridge that runs no application callback: the callback context is
    /// released here, unread.
    pub(super) fn into_opening(self) -> BridgeOpening {
        self.opening
    }
}

/// The gate one launched bridge waits behind until its connection admits it.
///
/// Handed to every launch rather than created by one, so the only sender is
/// the registration below: no launch can open its own gate, and a bridge whose
/// registration was refused never starts. The receiver is private, so only
/// [`spawn_gated_bridge`] can wait on it.
pub(in crate::http) struct BridgeStart(tokio::sync::oneshot::Receiver<()>);

/// One bridge task, spawned waiting behind its [`BridgeStart`].
///
/// The handle is private and [`spawn_gated_bridge`] is the only constructor,
/// so a launch cannot hand its connection a task that starts before admission.
#[must_use = "a dropped bridge detaches its task from the connection that must own it"]
pub(in crate::http) struct GatedBridge(tokio::task::JoinHandle<()>);

/// Give the bridge to its connection, then resolve the response lifetime to
/// match.
///
/// The connection takes the bridge as its own child and the `101` is committed
/// only once it has. A connection that cannot take one refuses the upgrade
/// instead of launching work no owner holds. Every upgrade kind routes through
/// here, so a new one inherits the rule instead of restating it.
///
/// `launch` returns a [`GatedBridge`], which only exists already waiting
/// behind `start`, so a bridge keeps its own future type all the way into its
/// task: nothing here erases the future it runs.
pub(super) async fn own_upgrade_bridge<L>(
    lifecycle: &ConnectionLifecycle,
    response: hyper::Response<HyperResponseBody>,
    handoff: &DisconnectSignal,
    launch: L,
) -> Result<hyper::Response<HyperResponseBody>, Rejected>
where
    L: FnOnce(BridgeAttachment, BridgeStart) -> GatedBridge,
{
    let admission = match lifecycle.upgrade_admission() {
        Some(admission) => admission,
        None => return Err(Rejected::upgrade_registration_unavailable()),
    };
    // Captured HERE, on the connection task, because that task is the last
    // owner of this runtime's context: the launch below is a bare
    // `tokio::spawn`, and no task-local crosses it.
    let attachment = BridgeAttachment {
        opening: BridgeOpening {
            control: admission.control(),
            dispatch: admission.dispatch_gate(),
        },
        callback: CallbackContext {
            runtime: crate::runtime_state::try_current_runtime(),
            stop: lifecycle.stop(),
            owner: admission.owner(),
        },
    };
    let (gate, start) = tokio::sync::oneshot::channel();
    let bridge = launch(attachment, BridgeStart(start));
    complete_upgrade_transfer(admission, bridge, gate, response, handoff).await
}

/// Resolve the response lifetime at a successful `101` handoff.
///
/// Past this point the transport belongs to the WebSocket close contract, so
/// this is Camber's last observation of the HTTP response. A `101` is excluded
/// from the body's generic empty-response completion precisely so this handoff
/// — not a rule about body length — owns the transition.
fn commit_upgrade(
    response: hyper::Response<HyperResponseBody>,
    handoff: &DisconnectSignal,
) -> hyper::Response<HyperResponseBody> {
    handoff.complete();
    response
}

/// Spawn one bridge that runs only once its registration opens `start`.
///
/// A launch must return a [`GatedBridge`], and this is its only constructor,
/// so the type guarantees no bridge kind starts before its connection admits
/// it. A gate dropped unopened ends the task without polling the bridge.
pub(super) fn spawn_gated_bridge<F>(start: BridgeStart, bridge: F) -> GatedBridge
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let BridgeStart(start) = start;
    GatedBridge(tokio::spawn(async move {
        match start.await {
            Ok(()) => bridge.await,
            Err(_) => {}
        }
    }))
}

/// Offer the bridge to the connection that serves this request, committing the
/// `101` only once that connection has taken it as a child.
///
/// A refusal-produced `503` or `500` is an ordinary HTTP response whose body
/// owns its own completion, so only the admitted arm resolves the handoff.
async fn complete_upgrade_transfer(
    admission: UpgradeAdmission,
    bridge: GatedBridge,
    gate: tokio::sync::oneshot::Sender<()>,
    response: hyper::Response<HyperResponseBody>,
    handoff: &DisconnectSignal,
) -> Result<hyper::Response<HyperResponseBody>, Rejected> {
    let GatedBridge(handle) = bridge;
    match admission.submit(handle).await {
        UpgradeRegistration::Admitted => release_admitted_bridge(gate, response, handoff),
        UpgradeRegistration::Rejected => Err(Rejected::upgrade_registration_refused()),
        UpgradeRegistration::Unavailable => Err(Rejected::upgrade_registration_unavailable()),
    }
}

/// Release the admitted bridge from its gate, then commit its `101`.
///
/// The gate's receiver lives inside the transferred task, so a send failure has
/// one meaning: the connection ended that task between taking it and this
/// release. The bridge will never run, and a `101` committed for it would hand
/// the peer a transport nothing serves and resolve the response lifetime as
/// `Completed`. That race reports what it is — the upgrade could not be taken
/// up — through the same response an unavailable owner produces.
fn release_admitted_bridge(
    gate: tokio::sync::oneshot::Sender<()>,
    response: hyper::Response<HyperResponseBody>,
    handoff: &DisconnectSignal,
) -> Result<hyper::Response<HyperResponseBody>, Rejected> {
    match gate.send(()) {
        Ok(()) => Ok(commit_upgrade(response, handoff)),
        Err(()) => Err(Rejected::upgrade_registration_unavailable()),
    }
}

/// Await the hyper upgrade, logging on failure.
async fn await_upgrade(
    on_upgrade: hyper::upgrade::OnUpgrade,
    context: &str,
) -> Option<hyper::upgrade::Upgraded> {
    match on_upgrade.await {
        Ok(u) => Some(u),
        Err(e) => {
            tracing::warn!(error = %e, "{context}");
            None
        }
    }
}

/// Take over the client transport as a server-role WebSocket stream.
///
/// Both bridges start here, so the handshake role and the framing
/// configuration are stated once rather than restated per bridge kind.
async fn upgrade_client_ws(
    on_upgrade: hyper::upgrade::OnUpgrade,
    context: &str,
) -> Option<ClientWs> {
    let upgraded = await_upgrade(on_upgrade, context).await?;
    Some(
        tokio_tungstenite::WebSocketStream::from_raw_socket(
            hyper_util::rt::TokioIo::new(upgraded),
            tokio_tungstenite::tungstenite::protocol::Role::Server,
            None,
        )
        .await,
    )
}

/// Wait for the connection to report whether the peer ever saw this `101`.
///
/// An uncommitted dispatch means the response never reached the wire, so the
/// transport is shut down rather than spoken WebSocket over. Both bridges gate
/// on this answer, so neither can start framing against a peer that is still
/// waiting on an HTTP response.
async fn commit_dispatch(
    gate: super::super::server_lifecycle::UpgradeDispatchGate,
    stream: &mut ClientWs,
) -> ControlFlow<()> {
    let committed = gate.committed().await;
    match committed {
        true => ControlFlow::Continue(()),
        false => {
            shutdown_client_transport(stream).await;
            ControlFlow::Break(())
        }
    }
}

/// What a bridge holds once it is open: the control watch it stops on, and the
/// client transport it frames over.
type OpenBridge = (tokio::sync::watch::Receiver<ServerControl>, ClientWs);

/// Open a bridge: spend its opening, take over the client transport, and wait
/// for the `101` to reach the wire.
///
/// The sequence, not the steps, is what a third bridge would get wrong — every
/// step below is already shared — so the sequence is written once. `None` is
/// both ways it can fail to open: an upgrade Hyper never completed, and a
/// dispatch the connection never committed. Neither leaves anything for the
/// caller to do, because both have already logged or shut the transport down.
pub(super) async fn open_bridge(
    on_upgrade: hyper::upgrade::OnUpgrade,
    opening: BridgeOpening,
    context: &str,
) -> Option<OpenBridge> {
    let BridgeOpening { control, dispatch } = opening;
    let mut stream = upgrade_client_ws(on_upgrade, context).await?;
    match commit_dispatch(dispatch, &mut stream).await {
        ControlFlow::Break(()) => None,
        ControlFlow::Continue(()) => Some((control, stream)),
    }
}
