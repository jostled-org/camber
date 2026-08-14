//! The direct WebSocket bridge: two independently polled directions under one
//! coordinator.
//!
//! A WebSocket's two directions have separate backpressure, so they have
//! separate owners. The inbound pump owns the framed source and the receive
//! queue's producer; the outbound pump owns the framed sink and the send
//! queue's consumer; neither waits through the other's queue operation. Above
//! them, one coordinator owns the terminal cause, the server control watch, the
//! transport, and the connection permit — and it is the only thing here that
//! decides when the connection ends.

use super::super::Request;
use super::super::body::HyperResponseBody;
use super::super::mock::{LifecycleCheckpoint, LifecycleScript};
use super::super::router::WsHandler;
use super::super::server_lifecycle::{ConnectionLifecycle, ConnectionPermit, ServerControl};
use super::super::websocket::{
    TerminalState, WsCloseCause, WsConn, WsMessage, WsReceiver, WsSender,
};
use super::framing::{
    WsError, WsFrameMessage, close_transport, drain_until_close, flush_transport, next_control,
    next_frame, send_close, shutdown_client_transport, until_abort,
};
use super::handoff::{WsHandoff, WsHandoffOutcome, WsRefusal, prepare_ws_handoff};
use super::handshake::WsUpgrade;
use super::ownership::{BridgeAttachment, ClientWs, open_bridge, own_upgrade_bridge};
use futures_util::stream::{SplitSink, SplitStream};
use std::ops::ControlFlow;
use std::pin::Pin;
use std::sync::Arc;
use std::task::Poll;

/// Which of a connection's two directions one pump owns.
///
/// Named rather than a boolean because the two are not opposites of one
/// setting: each owns a different transport half, a different queue end, and a
/// different half of the disposition table.
#[derive(Clone, Copy, Eq, PartialEq)]
pub(in crate::http) enum WsDirection {
    Inbound,
    Outbound,
}

/// Validate the upgrade pair, spawn background work, return 101.
pub(in crate::http) async fn handle_ws_upgrade(
    ws_upgrade: WsUpgrade,
    handler: WsHandler,
    req: Request,
    buffer_size: usize,
    lifecycle: &ConnectionLifecycle,
) -> Result<hyper::Response<HyperResponseBody>, WsRefusal> {
    let prepared = match prepare_ws_handoff(ws_upgrade, &req, lifecycle) {
        WsHandoffOutcome::Ready(prepared) => prepared,
        WsHandoffOutcome::Refused(refusal) => return Err(refusal),
    };
    // Taken as an owned value before the request moves into the bridge: a
    // registration refusal past this point still has to say what negotiation
    // had selected.
    let selected: Option<Box<str>> = prepared.subprotocol.map(Box::from);
    let WsHandoff {
        on_upgrade,
        response,
        permit,
        handoff,
        ..
    } = prepared;
    // Present only on the owned path, which is the same path that produces a
    // registrar: a synchronous lifecycle carries neither.
    let script = lifecycle.script();
    own_upgrade_bridge(lifecycle, response, &handoff, move |attachment| {
        bridge_ws_handler(
            on_upgrade,
            handler,
            req,
            buffer_size,
            attachment,
            script,
            permit,
        )
    })
    .await
    .map_err(|rejected| WsRefusal {
        rejected,
        subprotocol: selected,
    })
}

/// The two bounded queues one direct connection's application side owns.
struct DirectQueues {
    outbound: tokio::sync::mpsc::Receiver<WsMessage>,
    inbound: tokio::sync::mpsc::Sender<WsMessage>,
}

/// Build both application queues at the configured capacity, and the facade the
/// callback is given over them.
///
/// One function for both, because `ws_buffer_size` is one capacity for one
/// connection: two construction sites are two chances for the directions to be
/// bounded differently than the configuration says.
async fn direct_queues(
    buffer_size: usize,
    terminal: &Arc<TerminalState>,
    script: Option<&LifecycleScript>,
) -> (DirectQueues, WsConn) {
    LifecycleScript::pause_at(
        script,
        LifecycleCheckpoint::WebSocketOutgoingBufferConfigured(buffer_size),
    )
    .await;
    let (outgoing_tx, outgoing_rx) = tokio::sync::mpsc::channel::<WsMessage>(buffer_size);
    LifecycleScript::pause_at(
        script,
        LifecycleCheckpoint::WebSocketIncomingBufferConfigured(buffer_size),
    )
    .await;
    let (incoming_tx, incoming_rx) = tokio::sync::mpsc::channel::<WsMessage>(buffer_size);
    LifecycleScript::observe_ws_capacities(script, buffer_size, buffer_size);
    let conn = WsConn::new(
        WsSender::new(outgoing_tx, Arc::clone(terminal)),
        WsReceiver::new(incoming_rx, Arc::clone(terminal)),
    );
    let queues = DirectQueues {
        outbound: outgoing_rx,
        inbound: incoming_tx,
    };
    (queues, conn)
}

async fn bridge_ws_handler(
    on_upgrade: hyper::upgrade::OnUpgrade,
    handler: WsHandler,
    req: Request,
    buffer_size: usize,
    attachment: Option<BridgeAttachment>,
    script: Option<Arc<LifecycleScript>>,
    permit: Arc<ConnectionPermit>,
) {
    // Read before the attachment is spent on opening the bridge: what a
    // callback may admit is decided by which server owns this connection, and
    // the attachment is what says.
    let authority = BridgeAttachment::callback_runtime(&attachment);
    let opened = open_bridge(on_upgrade, attachment, "WebSocket client upgrade failed").await;
    let (mut control, stream) = match opened {
        Some(opened) => opened,
        None => {
            release_permit(permit, script.as_deref());
            return;
        }
    };
    let terminal = Arc::new(TerminalState::default());
    let (queues, conn) = direct_queues(buffer_size, &terminal, script.as_deref()).await;
    drop(tokio::task::spawn_blocking(move || {
        run_ws_callback(&handler, &req, conn, authority);
    }));
    let transport =
        run_direct_bridge(&mut control, stream, queues, &terminal, script.as_deref()).await;
    match transport {
        Some(mut stream) => shutdown_client_transport(&mut stream).await,
        None => {}
    }
    release_permit(permit, script.as_deref());
}

/// Give this connection's slot back, then say that it is back.
///
/// Dropping the permit is what returns the slot, so the observation follows the
/// drop: a controller woken by the release and reading the free slot would
/// otherwise be racing the drop it was told about. Every way this bridge ends
/// leaves through here — including an upgrade that never opened — so the
/// release is published exactly once on each of them.
fn release_permit(permit: Arc<ConnectionPermit>, script: Option<&LifecycleScript>) {
    drop(permit);
    LifecycleScript::observe_ws_permit_released(script);
}

/// Run one blocking `Router::ws` callback under the runtime authority its
/// serving connection captured.
///
/// The guard is scoped to this call alone. A blocking worker is reused, so a
/// context left behind would hand the next unrelated body a runtime it was
/// never served under; the guard restores the thread's own context whether the
/// callback returns or unwinds. The callback is not admitted to that runtime's
/// root scope — it is given the authority to admit work, not made work itself,
/// which is what keeps server completion from claiming a callback has exited.
///
/// Detached by contract — no handle exists to carry a panic back — so the
/// structured record is the only report there is.
fn run_ws_callback(
    handler: &WsHandler,
    req: &Request,
    conn: WsConn,
    authority: Option<Arc<crate::runtime_state::RuntimeInner>>,
) {
    let _authority = crate::runtime_state::install_carried_runtime(authority);
    match crate::task::catch_panic(move || handler(req, conn)) {
        Ok(Ok(())) => {}
        Ok(Err(error)) => report_callback_error(&error),
        Err(error) => tracing::error!(%error, "WebSocket handler panicked"),
    }
}

/// Record one callback's returned error at the level that error deserves.
///
/// A handler written the documented way propagates the end of its own
/// connection: a send past the close answers with this connection's cause, and
/// a peer that went away answers through the transport. Both are what every
/// disconnect looks like, so both are ordinary events rather than faults — a
/// server holding many connections would otherwise warn once per client
/// leaving, and a real handler fault would read the same as all of them.
fn report_callback_error(error: &crate::error::RuntimeError) {
    match error {
        crate::error::RuntimeError::WebSocketClosed(cause) => {
            tracing::debug!(%cause, "WebSocket handler stopped on a closed connection");
        }
        _ if crate::error::is_benign_io_error(error) => {
            tracing::debug!(%error, "WebSocket handler stopped on a closed transport");
        }
        _ => tracing::warn!(%error, "WebSocket handler returned error"),
    }
}

/// Run one direct bridge to its terminal cause and hand back its transport.
///
/// `None` is two halves that could not be put back together, which leaves
/// nothing to shut down: dropping both closes the same socket the shutdown
/// would have.
async fn run_direct_bridge(
    control: &mut Option<tokio::sync::watch::Receiver<ServerControl>>,
    stream: ClientWs,
    queues: DirectQueues,
    terminal: &TerminalState,
    script: Option<&LifecycleScript>,
) -> Option<ClientWs> {
    use futures_util::StreamExt;
    let (sink, source) = stream.split();
    let mut bridge = DirectBridge {
        inbound: InboundPump {
            source,
            frames: Some(queues.inbound.clone()),
            script,
        },
        outbound: OutboundPump {
            sink,
            frames: queues.outbound,
            pending: None,
            script,
        },
        receive_owner: Some(queues.inbound),
        terminal,
        script,
    };
    bridge.run(control).await;
    bridge.into_transport()
}

/// The one owner of a direct connection's two pumps and its terminal answer.
struct DirectBridge<'a> {
    inbound: InboundPump<'a>,
    outbound: OutboundPump<'a>,
    /// A second producer for the receive queue, held only to be watched.
    ///
    /// The inbound pump can be parked on the transport for as long as the peer
    /// is quiet, and a receive owner that goes away in that time still ends the
    /// connection. This clone is what makes that event visible to the
    /// coordinator without asking the pump.
    ///
    /// Optional because the settlement releases it: this and the pump's own
    /// producer are the receive queue's only two, and letting both go is the
    /// whole of what wakes a receive owner parked on this connection.
    receive_owner: Option<tokio::sync::mpsc::Sender<WsMessage>>,
    terminal: &'a TerminalState,
    script: Option<&'a LifecycleScript>,
}

impl DirectBridge<'_> {
    /// Fix this connection's one cause, then apply what that cause decides.
    async fn run(&mut self, control: &mut Option<tokio::sync::watch::Receiver<ServerControl>>) {
        LifecycleScript::pause_at(
            self.script,
            LifecycleCheckpoint::WebSocketBeforeTerminalSelection,
        )
        .await;
        let selected = self.race(control).await;
        let cause = self.terminal.commit(selected);
        LifecycleScript::observe_ws_terminal(self.script, cause);
        LifecycleScript::pause_at(self.script, LifecycleCheckpoint::WebSocketTerminalSelected)
            .await;
        self.settle(cause, control).await;
    }

    /// The highest-precedence terminal event ready in one turn of this
    /// coordinator.
    ///
    /// Every source is polled in the same turn rather than raced arm by arm,
    /// because the precedence between two events that are both ready is fixed
    /// by the contract and not by which one a `select!` happened to look at
    /// first. A source that answers ends the race, so none of them is ever
    /// polled again after it has.
    async fn race(
        &mut self,
        control: &mut Option<tokio::sync::watch::Receiver<ServerControl>>,
    ) -> WsCloseCause {
        use std::future::Future;
        let Self {
            inbound,
            outbound,
            receive_owner,
            ..
        } = self;
        let stopped = next_control(control);
        let unreceived = receive_owner_left(receive_owner);
        let received = inbound.run();
        let written = outbound.run();
        tokio::pin!(stopped, unreceived, received, written);
        std::future::poll_fn(|cx| {
            let mut selected = None;
            consider(&mut selected, stopped.as_mut().poll(cx).map(control_cause));
            consider(&mut selected, received.as_mut().poll(cx));
            consider(
                &mut selected,
                unreceived.as_mut().poll(cx).map(receiver_gone),
            );
            consider(&mut selected, written.as_mut().poll(cx));
            settled(selected)
        })
        .await
    }

    /// Apply one cause's queue disposition and protocol close to both
    /// directions.
    ///
    /// Both directions' admission closes first, so every application half
    /// waiting on this connection is woken by a queue whose cause is already
    /// fixed rather than at the end of a teardown it has no part in. The
    /// outbound side settles next, so a drained frame reaches the peer ahead of
    /// the close it was admitted before.
    ///
    /// Both steps that write to or wait on the peer are bounded by the server's
    /// own control watch. A settlement is the answer to the cause this bridge
    /// committed, and while it runs the server may ask for something stronger;
    /// a settlement that could not hear that would hold the connection past the
    /// abort already published to it. The two published settlements sit outside
    /// that bound, because an interrupted step is still this bridge letting its
    /// direction go.
    async fn settle(
        &mut self,
        cause: WsCloseCause,
        control: &mut Option<tokio::sync::watch::Receiver<ServerControl>>,
    ) {
        self.outbound.close_admission();
        self.close_receive_queue();
        self.outbound
            .apply(outbound_disposition(cause), control)
            .await;
        LifecycleScript::observe_ws_pump_settled(self.script, self.outbound.direction());
        until_abort(control, self.owed_close(cause)).await;
        LifecycleScript::observe_ws_pump_settled(self.script, self.inbound.direction());
    }

    /// Release both producers for the receive queue, and wake whoever is
    /// waiting on it.
    ///
    /// Closing that queue is the only thing that reaches a receive owner parked
    /// in a blocking receive: it reads this connection's cause once and then
    /// waits on the queue, so a cause published after the wait is one it cannot
    /// see. Done where send admission closes, rather than after the peer-facing
    /// teardown, because a peer that never answers its close would otherwise
    /// hold an application thread on a connection that has already ended.
    ///
    /// Nothing admits to the queue from here on — the inbound pump's future is
    /// dropped the moment the coordinator answers — so this is the wake and
    /// never a lost frame. What is already queued stays queued: releasing a
    /// producer hands the messages over before the cause, and only a cause that
    /// discards them drops them.
    fn close_receive_queue(&mut self) {
        drop(self.receive_owner.take());
        drop(self.inbound.frames.take());
    }

    /// Give the peer the close this cause owes it.
    ///
    /// A cancelled server owes nothing and a failed transport can carry
    /// nothing. A graceful stop owes the full handshake; a peer that closed
    /// first is owed only the flush that pushes the answer already queued for
    /// it; and a connection whose application halves went away is owed a normal
    /// close it does not wait on.
    async fn owed_close(&mut self, cause: WsCloseCause) {
        match cause {
            WsCloseCause::ServerCancelled | WsCloseCause::PeerDisconnected => {}
            WsCloseCause::ServerShutdown => {
                send_close(&mut self.outbound.sink, None).await;
                drain_until_close(&mut self.inbound.source).await;
            }
            WsCloseCause::PeerClosed => flush_transport(&mut self.outbound.sink).await,
            WsCloseCause::ReceiverDropped | WsCloseCause::SendersDropped => {
                close_transport(&mut self.outbound.sink).await;
            }
        }
    }

    /// Put the two halves of this bridge's transport back together.
    ///
    /// Consuming, because reuniting spends both halves. Both producers for the
    /// receive queue are already gone by here — the settlement releases them
    /// where it closes send admission — so nothing a blocked application half
    /// waits on is left for this step to do.
    fn into_transport(self) -> Option<ClientWs> {
        self.outbound.sink.reunite(self.inbound.source).ok()
    }
}

/// Wait for this connection's receive owner to go away.
///
/// A producer already released answers nothing, because there is nothing left
/// to answer for: the coordinator releases both of them at settlement, and by
/// then this connection's cause is fixed and no race is running.
async fn receive_owner_left(owner: &Option<tokio::sync::mpsc::Sender<WsMessage>>) {
    match owner {
        Some(owner) => owner.closed().await,
        None => std::future::pending().await,
    }
}

/// The receive owner's departure, as a cause.
fn receiver_gone((): ()) -> WsCloseCause {
    WsCloseCause::ReceiverDropped
}

/// Whether a coordinator turn found any terminal event at all.
fn settled(selected: Option<WsCloseCause>) -> Poll<WsCloseCause> {
    match selected {
        Some(cause) => Poll::Ready(cause),
        None => Poll::Pending,
    }
}

/// Keep the higher-precedence of two terminal events ready in one turn.
fn consider(selected: &mut Option<WsCloseCause>, ready: Poll<WsCloseCause>) {
    match (ready, *selected) {
        (Poll::Pending, _) => {}
        (Poll::Ready(cause), Some(held)) if precedence(held) <= precedence(cause) => {}
        (Poll::Ready(cause), _) => *selected = Some(cause),
    }
}

/// Where one terminal event sits in this connection's fixed precedence.
///
/// Lower wins. The order is the contract's: what the server was asked to do
/// outranks what the peer did, what the peer did outranks what the application
/// stopped doing, and a receive owner that left outranks a last sender that
/// did — a connection with nothing reading it is over for a stronger reason
/// than one with nothing left to write.
fn precedence(cause: WsCloseCause) -> u8 {
    match cause {
        WsCloseCause::ServerCancelled => 0,
        WsCloseCause::ServerShutdown => 1,
        WsCloseCause::PeerClosed => 2,
        WsCloseCause::PeerDisconnected => 3,
        WsCloseCause::ReceiverDropped => 4,
        WsCloseCause::SendersDropped => 5,
    }
}

/// The cause one server control transition fixes.
///
/// `Running` reaching a bridge means the control sender is gone: the server it
/// belonged to is no longer there to ask for anything, which is the same answer
/// as being cancelled by one.
fn control_cause(mode: ServerControl) -> WsCloseCause {
    match mode {
        ServerControl::Graceful => WsCloseCause::ServerShutdown,
        ServerControl::Abort | ServerControl::Running => WsCloseCause::ServerCancelled,
    }
}

/// What one cause does with the frames a successful send already admitted.
enum OutboundDisposition {
    /// Write them, in the order they were admitted.
    Drain,
    /// Drop them without writing.
    Cancel,
}

/// Which of the two a cause chooses.
///
/// Only an orderly end drains. A graceful stop is the server keeping the
/// promise a successful send was given, and a last sender dropping is the
/// application saying it has produced everything it means to. Every other cause
/// is a connection that cannot carry those frames or a peer that will not read
/// them.
fn outbound_disposition(cause: WsCloseCause) -> OutboundDisposition {
    match cause {
        WsCloseCause::ServerShutdown | WsCloseCause::SendersDropped => OutboundDisposition::Drain,
        WsCloseCause::ServerCancelled
        | WsCloseCause::PeerClosed
        | WsCloseCause::PeerDisconnected
        | WsCloseCause::ReceiverDropped => OutboundDisposition::Cancel,
    }
}

/// The direction that owns the framed source and the receive queue's producer.
struct InboundPump<'a> {
    source: SplitStream<ClientWs>,
    /// This direction's producer for the receive queue.
    ///
    /// Optional because the coordinator releases it at settlement, together
    /// with its own second producer, to wake a parked receive owner.
    frames: Option<tokio::sync::mpsc::Sender<WsMessage>>,
    script: Option<&'a LifecycleScript>,
}

impl InboundPump<'_> {
    /// Which direction of its connection this pump owns.
    ///
    /// Named by the pump rather than by the coordinator that reads it, so a
    /// settlement can never be published against the other direction.
    const fn direction(&self) -> WsDirection {
        WsDirection::Inbound
    }

    /// Read peer frames until this direction has a reason to stop.
    async fn run(&mut self) -> WsCloseCause {
        loop {
            match self.step().await {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(cause) => return cause,
            }
        }
    }

    /// Read one peer frame and place it.
    ///
    /// The checkpoint sits on the item this direction took off the transport,
    /// ahead of what that item turns out to be. A message, a close, and a
    /// transport that failed are three answers to the same read, and a
    /// checkpoint that only held the first two would let a case stage one of
    /// them against a second terminal event and not the others.
    async fn step(&mut self) -> ControlFlow<WsCloseCause> {
        use futures_util::StreamExt;
        let arrived = self.source.next().await;
        LifecycleScript::pause_at(
            self.script,
            LifecycleCheckpoint::WebSocketInboundFrameArrived,
        )
        .await;
        let frame = match next_frame(arrived, "WebSocket client bridge closed") {
            ControlFlow::Continue(frame) => frame,
            ControlFlow::Break(()) => return ControlFlow::Break(WsCloseCause::PeerDisconnected),
        };
        match frame.is_close() {
            true => ControlFlow::Break(WsCloseCause::PeerClosed),
            false => self.admit(frame).await,
        }
    }

    /// Hand one application message to the receive owner's bounded queue.
    ///
    /// Control frames never get here as messages: the transport answers a ping
    /// itself and a pong is that answer coming back, so neither is anything the
    /// peer's application sent and neither may take a slot in the bounded
    /// receive queue.
    ///
    /// A frame the queue has not accepted yet is owed to nobody. This wait is
    /// dropped where the coordinator answers, and the message goes with it —
    /// the promise a receive owner holds starts at the queue, not at the wire.
    /// The outbound direction owes the opposite promise, and holds a `pending`
    /// frame to keep it.
    async fn admit(&self, frame: WsFrameMessage) -> ControlFlow<WsCloseCause> {
        let message = match super::super::websocket::from_frame(frame) {
            Some(message) => message,
            None => return ControlFlow::Continue(()),
        };
        let admitted = match &self.frames {
            Some(frames) => frames.send(message).await,
            None => return ControlFlow::Break(WsCloseCause::ReceiverDropped),
        };
        match admitted {
            Ok(()) => {}
            Err(_) => return ControlFlow::Break(WsCloseCause::ReceiverDropped),
        }
        LifecycleScript::count_ws_inbound_admitted(self.script);
        LifecycleScript::pause_at(
            self.script,
            LifecycleCheckpoint::WebSocketInboundFrameQueued,
        )
        .await;
        ControlFlow::Continue(())
    }
}

/// The direction that owns the framed sink and the send queue's consumer.
struct OutboundPump<'a> {
    sink: SplitSink<ClientWs, WsFrameMessage>,
    frames: tokio::sync::mpsc::Receiver<WsMessage>,
    /// One admitted message taken off the queue and not yet written.
    ///
    /// Held on the pump rather than inside its running future, because that
    /// future is dropped the moment the coordinator selects a cause. A frame on
    /// that future's stack would be lost, and what a drain row claims is
    /// exactly that an admitted frame is not.
    ///
    /// It stays here across the wait for sink readiness for the same reason,
    /// and leaves only into a sink that has already agreed to take it.
    pending: Option<WsMessage>,
    script: Option<&'a LifecycleScript>,
}

impl OutboundPump<'_> {
    /// Which direction of its connection this pump owns.
    const fn direction(&self) -> WsDirection {
        WsDirection::Outbound
    }

    /// Do what one terminal disposition decides for the frames still admitted.
    ///
    /// Only the drain is bounded by the server's control watch: writing an
    /// admitted frame is a wait on a peer that may never read it, while
    /// dropping one is this pump's alone to do and finishes either way.
    async fn apply(
        &mut self,
        disposition: OutboundDisposition,
        control: &mut Option<tokio::sync::watch::Receiver<ServerControl>>,
    ) {
        match disposition {
            OutboundDisposition::Drain => until_abort(control, self.drain()).await,
            OutboundDisposition::Cancel => self.cancel(),
        }
    }

    /// Write admitted messages until this direction has a reason to stop.
    async fn run(&mut self) -> WsCloseCause {
        loop {
            match self.frames.recv().await {
                Some(message) => self.pending = Some(message),
                None => return WsCloseCause::SendersDropped,
            }
            match self.write_pending().await {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(cause) => return cause,
            }
        }
    }

    /// Write whatever this pump is holding.
    ///
    /// Holding nothing is answered before the sink is asked for anything: a
    /// drain reads its next admitted message through this same answer, and a
    /// readiness wait taken with no frame to write would park it on a peer it
    /// has nothing to say to.
    async fn write_pending(&mut self) -> ControlFlow<WsCloseCause> {
        LifecycleScript::pause_at(
            self.script,
            LifecycleCheckpoint::WebSocketBeforeOutboundWrite,
        )
        .await;
        match self.pending.is_some() {
            true => write_outcome(self.write_held().await),
            false => ControlFlow::Continue(()),
        }
    }

    /// Wait until the sink will take a frame, then give it the held one.
    ///
    /// The frame stays in `pending` across that wait, which is this write's one
    /// cancellation point: a sink that is not ready parks here, and a
    /// coordinator that answers meanwhile drops this future — a frame already
    /// taken out of `pending` would go with it, and a successful send has
    /// already promised the application it would not. Handing the frame over is
    /// synchronous, so past the wait the sink owns it. A flush cut short leaves
    /// it there, where the close this settlement still owes the peer writes it.
    async fn write_held(&mut self) -> Result<(), WsError> {
        use futures_util::{Sink, SinkExt};
        std::future::poll_fn(|cx| Pin::new(&mut self.sink).poll_ready(cx)).await?;
        match self.pending.take() {
            Some(message) => {
                let frame = super::super::websocket::into_frame(message);
                Pin::new(&mut self.sink).start_send(frame)?;
            }
            None => {}
        }
        self.sink.flush().await
    }

    /// Refuse every further send on this connection, and wake the ones waiting.
    fn close_admission(&mut self) {
        self.frames.close();
    }

    /// Write everything already admitted, in the order it was admitted.
    async fn drain(&mut self) {
        loop {
            match self.write_pending().await {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(_) => return,
            }
            match self.frames.recv().await {
                Some(message) => self.pending = Some(message),
                None => return,
            }
        }
    }

    /// Drop every admitted frame this connection's cause cancels.
    fn cancel(&mut self) {
        let mut cancelled = usize::from(self.pending.take().is_some());
        while self.frames.try_recv().is_ok() {
            cancelled += 1;
        }
        LifecycleScript::count_ws_outbound_cancelled(self.script, cancelled);
    }
}

/// One write attempt's result, as this direction's control flow.
///
/// A transport that could not take a frame carries nothing further either, so
/// every way a write fails is the same answer: this peer is gone.
fn write_outcome(result: Result<(), WsError>) -> ControlFlow<WsCloseCause> {
    match result {
        Ok(()) => ControlFlow::Continue(()),
        Err(error) => {
            tracing::debug!(%error, "WebSocket client send failed");
            ControlFlow::Break(WsCloseCause::PeerDisconnected)
        }
    }
}
