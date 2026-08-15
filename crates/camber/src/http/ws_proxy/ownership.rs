//! Who owns one upgraded connection, and when its `101` becomes real.
//!
//! The direct and proxied bridges differ in everything they do with a
//! transport and agree on everything about how they get one: an owned server
//! registers the bridge before the response reaches the wire, an unavailable
//! registrar refuses the upgrade, and neither bridge frames against a peer that
//! never saw the response. That sequence lives here, once. What the handshake
//! handed over lives beside it.

use super::super::body::HyperResponseBody;
use super::super::disconnect::DisconnectSignal;
use super::super::rejection::Rejected;
use super::super::server_lifecycle::{
    ConnectionLifecycle, ServerControl, UpgradeRegistrar, UpgradeRegistration,
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
pub(super) struct BridgeAttachment {
    control: tokio::sync::watch::Receiver<ServerControl>,
    dispatch: super::super::server_lifecycle::UpgradeDispatchGate,
    /// The Camber runtime this connection is served under, if it is served
    /// under one.
    ///
    /// Carried by value because the launch below crosses a bare `tokio::spawn`,
    /// which no task-local follows. It is the runtime's existing shared
    /// authority — the same `Arc` task admission reads — not a second one
    /// minted here.
    callback_runtime: Option<Arc<crate::runtime_state::RuntimeInner>>,
}

impl BridgeAttachment {
    /// The runtime authority a callback served under this attachment inherits.
    ///
    /// Read from the attachment rather than looked up where the callback runs,
    /// which is the whole of the contract's suppression rule: the capture
    /// happened on the connection task, so a server with no Camber runtime over
    /// it — bare-Tokio serving — carries `None` here, and its callback cannot
    /// pick up a blocking worker's leftover context by accident.
    pub(super) fn callback_runtime(&self) -> Option<Arc<crate::runtime_state::RuntimeInner>> {
        self.callback_runtime.clone()
    }

    /// Spread an attachment over the parts a bridge holds separately.
    ///
    /// Stated once, beside the struct, so a field added here reaches both
    /// bridges. Split at each bridge instead, a bridge that forgot the new
    /// field would still compile because the parts are independent values.
    fn split(
        attachment: Self,
    ) -> (
        tokio::sync::watch::Receiver<ServerControl>,
        super::super::server_lifecycle::UpgradeDispatchGate,
    ) {
        // Every field is named, including the one this spread does not carry:
        // the callback authority is read from the whole attachment before it is
        // spent, so a `..` here would also swallow the next field somebody adds.
        let Self {
            control,
            dispatch,
            callback_runtime: _,
        } = attachment;
        (control, dispatch)
    }
}

/// Choose who owns the bridge, then resolve the response lifetime to match.
///
/// The server registers the bridge and commits the `101` only once its registrar
/// has admitted it. An unavailable registrar refuses the upgrade instead of
/// launching work no supervisor owns. Every upgrade kind routes through here,
/// so a new one inherits the rule instead of restating it.
pub(super) async fn own_upgrade_bridge<F, Fut>(
    lifecycle: &ConnectionLifecycle,
    response: hyper::Response<HyperResponseBody>,
    handoff: &DisconnectSignal,
    build_bridge: F,
) -> Result<hyper::Response<HyperResponseBody>, Rejected>
where
    F: FnOnce(BridgeAttachment) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let registrar = match lifecycle.upgrade_registrar() {
        Some(registrar) => registrar,
        None => return Err(Rejected::upgrade_registration_unavailable()),
    };
    // Captured HERE, on the connection task, because that task is the last
    // owner of this runtime's context: the launch below is a bare
    // `tokio::spawn`, and no task-local crosses it.
    let attachment = BridgeAttachment {
        control: registrar.control(),
        dispatch: registrar.dispatch_gate(),
        callback_runtime: crate::runtime_state::try_current_runtime(),
    };
    let (gate, start) = tokio::sync::oneshot::channel();
    let handle = spawn_gated_bridge(start, build_bridge(attachment));
    complete_upgrade_registration(registrar, handle, gate, response, handoff).await
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

fn spawn_gated_bridge<F>(
    start: tokio::sync::oneshot::Receiver<()>,
    bridge: F,
) -> tokio::task::JoinHandle<()>
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tokio::spawn(async move {
        match start.await {
            Ok(()) => bridge.await,
            Err(_) => {}
        }
    })
}

/// Hand the bridge to the owned server's registrar, committing the `101` only
/// once the bridge is registered and owned.
///
/// A registrar-produced `503` or `500` is an ordinary HTTP response whose body
/// owns its own completion, so only the admitted arm resolves the handoff.
async fn complete_upgrade_registration(
    registrar: UpgradeRegistrar,
    handle: tokio::task::JoinHandle<()>,
    gate: tokio::sync::oneshot::Sender<()>,
    response: hyper::Response<HyperResponseBody>,
    handoff: &DisconnectSignal,
) -> Result<hyper::Response<HyperResponseBody>, Rejected> {
    match registrar.submit(handle).await {
        UpgradeRegistration::Admitted => release_admitted_bridge(gate, response, handoff),
        UpgradeRegistration::Rejected => Err(Rejected::upgrade_registration_refused()),
        UpgradeRegistration::Unavailable => Err(Rejected::upgrade_registration_unavailable()),
    }
}

/// Release the admitted bridge from its gate, then commit its `101`.
///
/// The gate's receiver lives inside the registered task, so a send failure has
/// one meaning: the supervisor aborted that task between admitting it and this
/// release. The bridge will never run, and a `101` committed for it would hand
/// the peer a transport nothing serves and resolve the response lifetime as
/// `Completed`. That race reports what it is — the upgrade could not be taken
/// up — through the same response the registrar's own unavailability produces.
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

/// Open a bridge: spread the attachment, take over the client transport, and
/// wait for the `101` to reach the wire.
///
/// The sequence, not the steps, is what a third bridge would get wrong — every
/// step below is already shared — so the sequence is written once. `None` is
/// both ways it can fail to open: an upgrade Hyper never completed, and a
/// dispatch the connection never committed. Neither leaves anything for the
/// caller to do, because both have already logged or shut the transport down.
pub(super) async fn open_bridge(
    on_upgrade: hyper::upgrade::OnUpgrade,
    attachment: BridgeAttachment,
    context: &str,
) -> Option<OpenBridge> {
    let (control, dispatch) = BridgeAttachment::split(attachment);
    let mut stream = upgrade_client_ws(on_upgrade, context).await?;
    match commit_dispatch(dispatch, &mut stream).await {
        ControlFlow::Break(()) => None,
        ControlFlow::Continue(()) => Some((control, stream)),
    }
}
