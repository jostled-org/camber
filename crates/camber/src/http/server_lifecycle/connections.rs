//! The owner tree one server registers, and what each connection contains.
//!
//! One shape, three levels. The server registers connection owners and nothing
//! else. A connection contains its requests and, when a handshake succeeds, one
//! upgrade child. Every child is reached through its parent, so there is no
//! second place a request, an upgrade, or a direction can be registered, and no
//! second authority that can cancel or settle one.
//!
//! The passive ownership record follows that shape exactly. Each mutation below
//! publishes the event it is, which is what lets a case say "this upgrade was a
//! child of that connection" rather than infer it from cleanup order.

use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll};

use futures_util::stream::{FuturesUnordered, StreamExt};

#[cfg(feature = "ws")]
use super::super::mock::UpgradeOwnerEdge;
use super::super::mock::{ConnectionOwnershipEvent, LifecycleScript};
use super::super::server_stop::ServerStopState;
#[cfg(feature = "ws")]
use super::super::server_stop::StopPhase;
use super::control::ServerControl;
use crate::RuntimeError;
#[cfg(feature = "ws")]
use crate::lifecycle::{AggregateShutdown, ShutdownOwner};
use crate::runtime_test_support::ParticipantDisposition;

pub(in crate::http) struct ConnectionPermit {
    permit: Option<tokio::sync::OwnedSemaphorePermit>,
}

impl ConnectionPermit {
    pub(super) fn new(permit: Option<tokio::sync::OwnedSemaphorePermit>) -> Arc<Self> {
        Arc::new(Self { permit })
    }
}

/// Return the connection's slot in the limit when the last owner of the shared
/// handle exits.
///
/// The permit is never read anywhere else — holding it IS the whole type — so
/// this release is also the only read the field has, and without it the field
/// is dead code.
impl Drop for ConnectionPermit {
    fn drop(&mut self) {
        drop(self.permit.take());
    }
}

/// What one connection carries into every request and upgrade it admits.
pub(in crate::http) struct ConnectionLifecycle {
    /// The opaque identity this connection's children are recorded under.
    ///
    /// Zero when no observer is registered, which is every production run. It
    /// names a parent in the passive ownership record and nothing else: no
    /// admission, cancellation, or release reads it.
    identity: u64,
    permit: Arc<ConnectionPermit>,
    control: Option<tokio::sync::watch::Receiver<ServerControl>>,
    /// The aggregate expiry this connection's admitted requests answer to, and
    /// the aggregate it settles its upgrade child against.
    ///
    /// Travels with `control` because the two are one authority: the control
    /// transition says a shutdown started, and this says when it ends. It is
    /// also the only aggregate held here — a second copy beside it could name a
    /// different one than this connection's own bounds were narrowed by.
    shutdown: super::control::ConnectionShutdownDeadline,
    /// The one causal stop state this connection's server committed into.
    ///
    /// Read, never written. It is the only answer to "was this server still
    /// admitting" that cannot be stale: a command commits its phase before the
    /// control watch is sent, so a connection that read the watch instead could
    /// admit an upgrade into a server that had already stopped.
    stop: Option<Arc<ServerStopState>>,
    #[cfg(feature = "ws")]
    handoff: Option<tokio::sync::mpsc::Sender<UpgradeHandoff>>,
    #[cfg(feature = "ws")]
    transport_state: Option<tokio::sync::watch::Receiver<UpgradeTransportState>>,
    script: Option<Arc<LifecycleScript>>,
}

/// Hand-written rather than derived, and it has to stay that way.
///
/// `permit` is held to be dropped, not to be read: the last lifecycle clone to
/// go away is what returns the connection's slot to the limit. Only the upgrade
/// path reads the field back, so in a build without `ws` this clone is its one
/// read and a derive would leave the field dead — which is the field's whole
/// job, stated as a warning.
impl Clone for ConnectionLifecycle {
    fn clone(&self) -> Self {
        Self {
            identity: self.identity,
            permit: Arc::clone(&self.permit),
            control: self.control.clone(),
            shutdown: self.shutdown.clone(),
            stop: self.stop.clone(),
            #[cfg(feature = "ws")]
            handoff: self.handoff.clone(),
            #[cfg(feature = "ws")]
            transport_state: self.transport_state.clone(),
            script: self.script.clone(),
        }
    }
}

/// Everything a supervisor hands one connection owner at admission.
///
/// A struct rather than six positional arguments, because every field is a
/// shared authority and transposing two of them would compile.
pub(super) struct ConnectionAdmission {
    pub(super) identity: u64,
    pub(super) permit: Arc<ConnectionPermit>,
    pub(super) control: tokio::sync::watch::Receiver<ServerControl>,
    pub(super) shutdown: super::control::ConnectionShutdownDeadline,
    pub(super) stop: Arc<ServerStopState>,
    pub(super) script: Option<Arc<LifecycleScript>>,
}

impl ConnectionLifecycle {
    pub(super) fn owned(admission: ConnectionAdmission) -> Self {
        Self {
            identity: admission.identity,
            permit: admission.permit,
            control: Some(admission.control),
            shutdown: admission.shutdown,
            stop: Some(admission.stop),
            #[cfg(feature = "ws")]
            handoff: None,
            #[cfg(feature = "ws")]
            transport_state: None,
            script: admission.script,
        }
    }

    #[cfg(feature = "ws")]
    pub(in crate::http) fn permit(&self) -> Arc<ConnectionPermit> {
        Arc::clone(&self.permit)
    }

    pub(in crate::http) fn script(&self) -> Option<Arc<LifecycleScript>> {
        self.script.clone()
    }

    /// The causal stop state this connection's children answer to.
    ///
    /// `None` is a connection with no supervisor over it. Read-only for every
    /// caller here: a child bounds itself against what the server has already
    /// committed, and it submits no event of its own.
    pub(in crate::http) fn stop(&self) -> Option<Arc<ServerStopState>> {
        self.stop.clone()
    }

    /// This connection's shutdown and forced-cancellation authority.
    ///
    /// `None` is a connection with no supervisor to hear from, which is what an
    /// operation carrying it reads as "neither terminal can be observed" rather
    /// than as "the server is running".
    pub(in crate::http) fn control(&self) -> Option<tokio::sync::watch::Receiver<ServerControl>> {
        self.control.clone()
    }

    /// The shutdown authority an admitted request on this connection answers to.
    pub(in crate::http) fn shutdown_deadline(&self) -> super::control::ConnectionShutdownDeadline {
        self.shutdown.clone()
    }

    /// Record one request as a child of this connection.
    pub(in crate::http) fn admit_request(&self) -> RequestOwner {
        RequestOwner::admitted(self.script.clone(), self.identity)
    }

    /// What this connection offers a handler that wants to upgrade.
    ///
    /// `None` is a connection with no upgrade transport bound, which is every
    /// path that does not serve upgrades; the handler answers it with the same
    /// refusal an unavailable owner produces.
    #[cfg(feature = "ws")]
    pub(in crate::http) fn upgrade_admission(&self) -> Option<UpgradeAdmission> {
        let handoff = self.handoff.as_ref()?.clone();
        let control = self.control.as_ref()?.clone();
        let transport_state = self.transport_state.as_ref()?.clone();
        // Minted here, where this connection decides to offer a place at all, so
        // the bridge can be given its identity as it is built rather than told
        // it back after the transfer it is meant to name.
        let owner = UpgradeIdentity {
            connection: self.identity,
            upgrade: LifecycleScript::mint_owner_identity(self.script.as_deref()),
        };
        Some(UpgradeAdmission {
            handoff,
            control,
            transport_state,
            owner,
            script: self.script.clone(),
        })
    }

    /// Take the upgrade transport this connection owns, and leave the handler
    /// side of it on the lifecycle.
    #[cfg(feature = "ws")]
    pub(in crate::http) fn bind_upgrade_transport(&mut self) -> UpgradeTransportOwner {
        let (handoff_sender, handoffs) = tokio::sync::mpsc::channel(1);
        let (state_sender, state_receiver) =
            tokio::sync::watch::channel(UpgradeTransportState::Pending);
        self.handoff = Some(handoff_sender);
        self.transport_state = Some(state_receiver);
        UpgradeTransportOwner {
            handoffs,
            state_sender,
            connection: self.identity,
            stop: self.stop.clone(),
            shutdown_aggregate: self.shutdown.aggregate(),
            script: self.script.clone(),
        }
    }
}

/// One request's place in its connection's owner tree, recorded passively.
///
/// Admission is the per-request service call this connection's driver makes;
/// settlement is that call returning the response head. Those are the only two
/// per-request boundaries the connection owns, so they are the only two this
/// owner can honestly name — the body that follows the head is owned by the
/// guard attached to it.
///
/// Nothing production does reads it back. It admits nothing, cancels nothing,
/// and releases nothing; dropping it publishes one event and returns.
pub(in crate::http) struct RequestOwner {
    script: Option<Arc<LifecycleScript>>,
    connection: u64,
    request: u64,
}

impl RequestOwner {
    fn admitted(script: Option<Arc<LifecycleScript>>, connection: u64) -> Self {
        let request = LifecycleScript::mint_owner_identity(script.as_deref());
        LifecycleScript::observe_ownership(
            script.as_deref(),
            ConnectionOwnershipEvent::ConnectionRequestAdmitted {
                connection,
                request,
            },
        );
        Self {
            script,
            connection,
            request,
        }
    }
}

impl Drop for RequestOwner {
    fn drop(&mut self) {
        LifecycleScript::observe_ownership(
            self.script.as_deref(),
            ConnectionOwnershipEvent::ConnectionRequestSettled {
                connection: self.connection,
                request: self.request,
            },
        );
    }
}

#[cfg(feature = "ws")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::http) enum UpgradeTransportState {
    Pending,
    Committed,
    Cancelled,
}

#[cfg(feature = "ws")]
pub(in crate::http) struct UpgradeDispatchGate {
    state: tokio::sync::watch::Receiver<UpgradeTransportState>,
}

#[cfg(feature = "ws")]
impl UpgradeDispatchGate {
    pub(in crate::http) async fn committed(mut self) -> bool {
        loop {
            match *self.state.borrow_and_update() {
                UpgradeTransportState::Committed => return true,
                UpgradeTransportState::Cancelled => return false,
                UpgradeTransportState::Pending => {}
            }
            if self.state.changed().await.is_err() {
                return false;
            }
        }
    }
}

/// How one connection answered a handler that offered it a bridge.
#[cfg(feature = "ws")]
pub(in crate::http) enum UpgradeRegistration {
    Admitted,
    Rejected,
    Unavailable,
}

/// Where one upgrade child sits in its connection's owner tree.
///
/// Minted where the child is built, on the connection task, and carried down
/// both sides of the handoff: the parent records the transfer under it, and the
/// bridge names it in everything it publishes about the callback it started.
/// One identity written by two independent paths is what lets a case say the
/// callback beneath an upgrade is *that* upgrade's rather than some upgrade's.
#[cfg(feature = "ws")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(in crate::http) struct UpgradeIdentity {
    /// The connection that offers this child a place.
    pub(in crate::http) connection: u64,
    /// The child itself.
    pub(in crate::http) upgrade: u64,
}

/// What a handler holds to offer its bridge to the connection that serves it.
///
/// The offer goes to the connection and nowhere else. There is no server-level
/// channel behind this: the connection that produced the response head is the
/// only owner that can take the bridge, so an upgrade cannot register beside
/// the connection it came from.
#[cfg(feature = "ws")]
pub(in crate::http) struct UpgradeAdmission {
    handoff: tokio::sync::mpsc::Sender<UpgradeHandoff>,
    control: tokio::sync::watch::Receiver<ServerControl>,
    transport_state: tokio::sync::watch::Receiver<UpgradeTransportState>,
    owner: UpgradeIdentity,
    script: Option<Arc<LifecycleScript>>,
}

#[cfg(feature = "ws")]
impl UpgradeAdmission {
    pub(in crate::http) fn control(&self) -> tokio::sync::watch::Receiver<ServerControl> {
        self.control.clone()
    }

    /// The place in the owner tree a bridge offered here will be taken as.
    pub(in crate::http) fn owner(&self) -> UpgradeIdentity {
        self.owner
    }

    pub(in crate::http) fn dispatch_gate(&self) -> UpgradeDispatchGate {
        UpgradeDispatchGate {
            state: self.transport_state.clone(),
        }
    }

    /// Offer one spawned bridge to this connection, and wait for its answer.
    ///
    /// A closed channel is a connection that has already stopped taking
    /// children, which is the same answer as an unavailable owner: the bridge is
    /// ended here rather than left running for a parent that will never join it.
    pub(in crate::http) async fn submit(
        self,
        handle: tokio::task::JoinHandle<()>,
    ) -> UpgradeRegistration {
        let (decision, decided) = tokio::sync::oneshot::channel();
        let offer = UpgradeHandoff {
            handle: Some(handle),
            owner: self.owner,
            decision: Some(decision),
        };
        match self.handoff.send(offer).await {
            Ok(()) => {}
            Err(error) => {
                error.0.refuse().await;
                return UpgradeRegistration::Unavailable;
            }
        }
        LifecycleScript::pause_at_upgrade(
            self.script.as_deref(),
            UpgradeOwnerEdge::AfterHandoffSubmitted,
        )
        .await;
        match decided.await {
            Ok(registration) => registration,
            Err(_) => UpgradeRegistration::Unavailable,
        }
    }
}

/// One bridge on its way from the handler that built it to the connection that
/// will own it.
///
/// It carries the task, the identity the bridge was built under, and the answer
/// channel, and nothing else. Dropping it unanswered ends the bridge, so a
/// handoff lost to a connection that ended first never leaves a task with no
/// parent.
#[cfg(feature = "ws")]
pub(in crate::http) struct UpgradeHandoff {
    handle: Option<tokio::task::JoinHandle<()>>,
    owner: UpgradeIdentity,
    decision: Option<tokio::sync::oneshot::Sender<UpgradeRegistration>>,
}

#[cfg(feature = "ws")]
impl UpgradeHandoff {
    /// End this bridge and tell the handler it was not taken up.
    async fn refuse(mut self) {
        self.answer(UpgradeRegistration::Unavailable);
        if let Some(handle) = self.handle.take() {
            handle.abort();
            let _ = handle.await;
        }
    }

    fn answer(&mut self, registration: UpgradeRegistration) {
        if let Some(decision) = self.decision.take() {
            let _ = decision.send(registration);
        }
    }
}

/// End a bridge whose offer nothing answered.
///
/// The gated bridge parks on its release channel until the handler commits the
/// `101`, so a dropped, unanswered handoff leaves a task waiting on a gate no
/// one will open. Aborting here is what makes that impossible.
#[cfg(feature = "ws")]
impl Drop for UpgradeHandoff {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.as_ref() {
            handle.abort();
        }
    }
}

/// The upgrade transport one connection owns, and the child it may take.
#[cfg(feature = "ws")]
pub(in crate::http) struct UpgradeTransportOwner {
    handoffs: tokio::sync::mpsc::Receiver<UpgradeHandoff>,
    state_sender: tokio::sync::watch::Sender<UpgradeTransportState>,
    connection: u64,
    stop: Option<Arc<ServerStopState>>,
    shutdown_aggregate: Option<Arc<AggregateShutdown>>,
    script: Option<Arc<LifecycleScript>>,
}

#[cfg(feature = "ws")]
impl UpgradeTransportOwner {
    pub(in crate::http) async fn next_handoff(&mut self) -> Option<UpgradeHandoff> {
        self.handoffs.recv().await
    }

    /// Take one offered bridge as this connection's child, or refuse it.
    ///
    /// The transfer is recorded BEFORE the handler is answered, so the record
    /// shows the child under its connection ahead of the `101` the answer
    /// releases. Refusal is the same act in reverse: nothing is recorded, and
    /// the bridge is ended where it stands.
    pub(in crate::http) async fn accept(
        &self,
        mut handoff: UpgradeHandoff,
        peer_closed: bool,
    ) -> Option<UpgradeOwner> {
        LifecycleScript::pause_at_upgrade(
            self.script.as_deref(),
            UpgradeOwnerEdge::BeforeTransferAcknowledge,
        )
        .await;
        // A refused offer keeps the task it carries, so dropping the offer is
        // what ends it. The answer is what lets the handler produce its own
        // `503` instead of waiting on one that never comes.
        let taken = (!peer_closed && self.admitting())
            .then(|| handoff.handle.take())
            .flatten();
        let handle = match taken {
            Some(handle) => handle,
            None => {
                handoff.answer(UpgradeRegistration::Rejected);
                return None;
            }
        };
        // The parent names itself and adopts only the child half of the identity
        // the bridge was built under. Taking both from the offer would make a
        // handoff that arrived at the wrong connection agree with itself, and
        // agreement between two independently written halves is the only thing
        // that makes the record's parentage checkable.
        let owner = UpgradeOwner::transferred(
            handle,
            UpgradeIdentity {
                connection: self.connection,
                upgrade: handoff.owner.upgrade,
            },
            self.shutdown_aggregate.clone(),
            self.script.clone(),
        );
        // Between the record and the answer, which is the one place a case can
        // read the transfer and still know the peer has seen nothing: the
        // handler is parked on the answer below, so no byte of the `101` it
        // produces can be on the wire yet.
        LifecycleScript::pause_at_upgrade(
            self.script.as_deref(),
            UpgradeOwnerEdge::AfterTransferRecorded,
        )
        .await;
        handoff.answer(UpgradeRegistration::Admitted);
        Some(owner)
    }

    /// Whether this connection's server is still admitting children.
    ///
    /// Read from the causal stop state rather than from the control watch: a
    /// command commits its phase before the watch is sent, so this is the only
    /// answer that a returned stop command has already fixed.
    fn admitting(&self) -> bool {
        match self.stop.as_deref() {
            Some(stop) => matches!(stop.phase(), StopPhase::Running),
            None => true,
        }
    }

    pub(in crate::http) fn commit(&self) {
        self.state_sender
            .send_replace(UpgradeTransportState::Committed);
    }

    pub(in crate::http) fn cancel(&self) {
        self.state_sender
            .send_replace(UpgradeTransportState::Cancelled);
    }

    /// Refuse every offer still on the channel, and take no more.
    pub(in crate::http) async fn abort_pending(&mut self) {
        self.handoffs.close();
        while let Some(handoff) = self.handoffs.recv().await {
            handoff.refuse().await;
        }
    }
}

/// One upgraded transport, held by the connection that produced it.
///
/// The whole of Invariant 7 on the upgrade side: the connection that
/// transferred this child is the only owner that can cancel or join it, and it
/// cannot settle — and so cannot give its permit back — until this owner has.
#[cfg(feature = "ws")]
pub(in crate::http) struct UpgradeOwner {
    handle: Option<tokio::task::JoinHandle<()>>,
    owner: UpgradeIdentity,
    shutdown: Option<Arc<AggregateShutdown>>,
    script: Option<Arc<LifecycleScript>>,
}

#[cfg(feature = "ws")]
impl UpgradeOwner {
    fn transferred(
        handle: tokio::task::JoinHandle<()>,
        owner: UpgradeIdentity,
        shutdown: Option<Arc<AggregateShutdown>>,
        script: Option<Arc<LifecycleScript>>,
    ) -> Self {
        LifecycleScript::observe_ownership(
            script.as_deref(),
            ConnectionOwnershipEvent::ConnectionUpgradeTransferred {
                connection: owner.connection,
                upgrade: owner.upgrade,
            },
        );
        Self {
            handle: Some(handle),
            owner,
            shutdown,
            script,
        }
    }

    /// End this child where it stands.
    ///
    /// The connection's answer to a `101` that never reached the wire: the
    /// bridge has nothing to speak to, so it is not given the settlement a
    /// committed bridge gets.
    pub(in crate::http) fn cancel(&self) {
        if let Some(handle) = self.handle.as_ref() {
            handle.abort();
        }
    }

    /// Wait for this child to end, and report how it ended.
    ///
    /// The join is the connection's own bound: a bridge that will not settle is
    /// ended by the server aborting the connection that holds it, which is the
    /// same forced deadline every other owner answers to.
    pub(in crate::http) async fn join(mut self) {
        let disposition = match self.handle.take() {
            None => ParticipantDisposition::Completed,
            Some(handle) => upgrade_disposition(handle.await),
        };
        self.settle(disposition);
    }

    /// Publish how this child ended, once, to both accounts that name it.
    ///
    /// The aggregate hears the disposition because the runtime has to account
    /// for every owner no caller holds a handle for; the ownership record hears
    /// the settlement because a case has to be able to say the child settled
    /// under this connection rather than beside it.
    fn settle(&mut self, disposition: ParticipantDisposition) {
        if let Some(shutdown) = self.shutdown.as_ref() {
            shutdown.settle(&ShutdownOwner::UPGRADE, disposition);
        }
        LifecycleScript::observe_ownership(
            self.script.as_deref(),
            ConnectionOwnershipEvent::ConnectionUpgradeSettled {
                connection: self.owner.connection,
                upgrade: self.owner.upgrade,
            },
        );
        self.handle = None;
    }
}

/// How one upgrade child's join reads.
///
/// A cancellation is the connection's own doing — nothing else holds this
/// handle — so it is named as one rather than reported as a fault. A panic is
/// the one thing the connection could not have asked for.
#[cfg(feature = "ws")]
fn upgrade_disposition(joined: Result<(), tokio::task::JoinError>) -> ParticipantDisposition {
    match joined {
        Ok(()) => ParticipantDisposition::Completed,
        Err(error) if error.is_cancelled() => ParticipantDisposition::CancelledAndJoined,
        Err(error) => {
            tracing::warn!(%error, "upgrade child panicked");
            ParticipantDisposition::Named
        }
    }
}

/// End a child whose owner was dropped without joining it.
///
/// Only reachable when the connection itself was taken away mid-settlement, and
/// the abort is the whole disposition then: an orphaned bridge has no parent
/// left to hand its transport back to.
#[cfg(feature = "ws")]
impl Drop for UpgradeOwner {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            handle.abort();
            self.settle(ParticipantDisposition::CancelledAndJoined);
        }
    }
}

/// Whether one connection is holding an upgrade child right now.
///
/// Written by the connection that transfers and settles the child, read by the
/// server that decides which owners a forced abort may end where they stand.
/// A parent publishing one fact about itself is not a sibling registry: nothing
/// else can write it, and the server reads it without being able to reach the
/// child at all.
pub(in crate::http) struct UpgradeRetention {
    retained: AtomicBool,
}

impl UpgradeRetention {
    pub(super) fn new() -> Arc<Self> {
        Arc::new(Self {
            retained: AtomicBool::new(false),
        })
    }

    #[cfg(feature = "ws")]
    pub(in crate::http) fn hold(&self) {
        self.retained.store(true, Ordering::Release);
    }

    #[cfg(feature = "ws")]
    pub(in crate::http) fn release(&self) {
        self.retained.store(false, Ordering::Release);
    }

    fn is_held(&self) -> bool {
        self.retained.load(Ordering::Acquire)
    }
}

/// One connection the server owns, and the only handle it has on it.
struct ConnectionOwner {
    handle: tokio::task::JoinHandle<()>,
    /// The opaque identity this owner was registered under.
    identity: u64,
    upgrade: Arc<UpgradeRetention>,
}

impl Future for ConnectionOwner {
    type Output = ConnectionCompletion;

    fn poll(mut self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let identity = self.identity;
        Pin::new(&mut self.handle)
            .poll(context)
            .map(|result| ConnectionCompletion { result, identity })
    }
}

/// Abort the connection this owner is letting go of.
///
/// This is the whole field-drop path: dropping [`OwnedConnections`] drops its
/// `FuturesUnordered`, which drops every owner through here, so the set needs no
/// `Drop` of its own. `abort_all` covers the explicit path, which is separate
/// only because it also records that the supervisor asked.
impl Drop for ConnectionOwner {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

pub(super) struct ConnectionCompletion {
    pub(super) result: Result<(), tokio::task::JoinError>,
    /// The opaque identity this owner was registered under.
    pub(super) identity: u64,
}

/// Every connection one server currently owns.
///
/// Connections only. A request, an upgrade, or a direction is reached through
/// the connection that contains it, so this set is the whole server registry
/// and its cardinality is the number of live connections.
pub(super) struct OwnedConnections {
    connections: FuturesUnordered<ConnectionOwner>,
    supervisor_aborted: bool,
}

impl OwnedConnections {
    pub(super) fn new() -> Self {
        Self {
            connections: FuturesUnordered::new(),
            supervisor_aborted: false,
        }
    }

    pub(super) fn insert(
        &mut self,
        handle: tokio::task::JoinHandle<()>,
        identity: u64,
        upgrade: Arc<UpgradeRetention>,
    ) {
        self.connections.push(ConnectionOwner {
            handle,
            identity,
            upgrade,
        });
    }

    pub(super) fn is_empty(&self) -> bool {
        self.connections.is_empty()
    }

    /// Whether the supervisor has told these owners to end.
    pub(super) fn supervisor_aborted(&self) -> bool {
        self.supervisor_aborted
    }

    pub(super) fn abort_all(&mut self) {
        self.supervisor_aborted = true;
        self.connections
            .iter()
            .for_each(|owner| owner.handle.abort());
    }

    /// Abort every connection that has no settlement of its own to run.
    ///
    /// A connection holding an upgrade child is left running. The server has
    /// already published the abort, and such a connection answers it by settling
    /// what it owns: it fixes the one cause every half of the upgraded transport
    /// reads, cancels the frames a successful send had admitted, joins its
    /// child, and only then gives the connection permit up. Aborting it here
    /// would take all four away from the application and leave the halves to
    /// infer why. A connection with no child has no answer of its own — it
    /// drains for as long as its peer keeps it — so it is not given the chance.
    ///
    /// What makes the difference is that a retained child keeps reading this
    /// abort while it settles. Every step of its settlement that waits on a peer
    /// is bounded by the same control watch this abort was published to, so a
    /// bridge already committed to a graceful close hears the escalation where
    /// it is parked rather than after it.
    pub(super) fn abort_without_retained_upgrade(&mut self) {
        self.supervisor_aborted = true;
        self.connections
            .iter()
            .filter(|owner| !owner.upgrade.is_held())
            .for_each(|owner| owner.handle.abort());
    }

    pub(super) async fn next(&mut self) -> Option<ConnectionCompletion> {
        self.connections.next().await
    }
}

/// How one accepted connection's completion settles.
///
/// A join the supervisor got back clean is complete; one it got back after
/// asking for the cancellation is cancelled and joined; anything else is named,
/// because the supervisor turns that failure into the terminal it reports.
///
/// The failure arrives as the value itself rather than as a second `bool`. Two
/// adjacent same-typed flags let a transposed call site compile and report a
/// panicked connection as a cancelled one, which is the hazard `ConnectionPhase`
/// was introduced to remove.
pub(super) const fn connection_disposition(
    failure: Option<&RuntimeError>,
    cancelled: bool,
) -> ParticipantDisposition {
    match (failure, cancelled) {
        (Some(_), _) => ParticipantDisposition::Named,
        (None, true) => ParticipantDisposition::CancelledAndJoined,
        (None, false) => ParticipantDisposition::Completed,
    }
}
