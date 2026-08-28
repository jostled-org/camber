use super::body::GuardedBody;
use super::disconnect::ConnectionLiveness;
use super::handle::{ConnCtx, handle_request};
use super::router::ServerDispatch;
use super::server_lifecycle::{ConnectionLifecycle, ServerControl, wait_shutdown_control};
use crate::net::accept;
use std::sync::Arc;

/// The transport-independent state one connection serves requests with.
///
/// Both serve paths build the same per-request service from it; only the
/// shutdown authority they carry alongside differs.
pub(super) struct ConnectionState {
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    lifecycle: ConnectionLifecycle,
    header_timeout: std::time::Duration,
    remote_addr: Option<std::net::IpAddr>,
}

impl ConnectionState {
    pub(super) fn new(
        router: Arc<ServerDispatch>,
        ctx: Arc<ConnCtx>,
        lifecycle: ConnectionLifecycle,
        header_timeout: std::time::Duration,
        remote_addr: Option<std::net::IpAddr>,
    ) -> Self {
        Self {
            router,
            ctx,
            lifecycle,
            header_timeout,
            remote_addr,
        }
    }
}

/// Build the per-request service both serve paths hand to Hyper.
///
/// Each request gets its own signal and armed guard from the connection's
/// liveness, so no response-scoped state is shared between requests.
///
/// The state arrives by value: the connection that built it never reads it
/// again, so the router, the context, and the lifecycle move here rather than
/// being cloned out of a borrow. That matters most for the lifecycle, whose own
/// `Clone` bumps a permit, two watch receivers, two channel senders, and a
/// script handle.
///
/// The lifecycle is then shared through an `Arc` rather than cloned per
/// request, because `serve_request` only borrows it. Taking the value here is
/// also what makes the one snapshot correct — the owned path binds its upgrade
/// transport onto the lifecycle before it asks for a service, so this is the
/// first point at which the value no longer changes.
fn connection_service(
    state: ConnectionState,
    liveness: Arc<ConnectionLiveness>,
) -> impl hyper::service::Service<
    hyper::Request<hyper::body::Incoming>,
    Response = hyper::Response<GuardedBody>,
    Error = std::convert::Infallible,
    Future: Send + 'static,
> + use<> {
    let ConnectionState {
        router,
        ctx,
        lifecycle,
        remote_addr,
        ..
    } = state;
    let lifecycle = Arc::new(lifecycle);
    hyper::service::service_fn(move |request| {
        let router = Arc::clone(&router);
        let ctx = Arc::clone(&ctx);
        let lifecycle = Arc::clone(&lifecycle);
        let liveness = Arc::clone(&liveness);
        async move { serve_request(request, &router, &ctx, remote_addr, &lifecycle, liveness).await }
    })
}

/// Serve one connection an owned server accepted.
///
/// The shutdown authority arrives by value from the supervisor that subscribed
/// it, so every later reader — the handshake race, the connection driver, and
/// every response guard's cause table — takes a receiver derived from that one
/// subscription. No arm can invent a different answer to "is the server
/// shutting down", and no arm can find the answer missing.
pub(super) async fn serve_owned_connection(
    stream: crate::net::AcceptedStream,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    state: ConnectionState,
    control: tokio::sync::watch::Receiver<ServerControl>,
    #[cfg(feature = "ws")] retention: Arc<super::server_lifecycle::UpgradeRetention>,
) {
    // Per-connection liveness belongs to the accepted stream, never to the
    // per-server context every connection shares.
    let liveness = ConnectionLiveness::controlled(control.clone());
    // TLS is offered on TCP alone. A Unix peer is already confined by the
    // socket's filesystem permissions, and no entry point has ever wrapped one,
    // so a configured acceptor does not silently change what a Unix client
    // speaks.
    match (stream, tls_acceptor) {
        (crate::net::AcceptedStream::Tcp(stream), Some(acceptor)) => {
            serve_owned_tls(
                stream,
                acceptor,
                state,
                liveness,
                control,
                #[cfg(feature = "ws")]
                retention,
            )
            .await;
        }
        (crate::net::AcceptedStream::Tcp(stream), None) => {
            serve_owned_stream(
                stream,
                state,
                liveness,
                control,
                #[cfg(feature = "ws")]
                retention,
            )
            .await;
        }
        (crate::net::AcceptedStream::Unix(stream), _) => {
            serve_owned_stream(
                stream,
                state,
                liveness,
                control,
                #[cfg(feature = "ws")]
                retention,
            )
            .await;
        }
    }
}

async fn serve_owned_tls(
    stream: tokio::net::TcpStream,
    acceptor: tokio_rustls::TlsAcceptor,
    state: ConnectionState,
    liveness: Arc<ConnectionLiveness>,
    mut control: tokio::sync::watch::Receiver<ServerControl>,
    #[cfg(feature = "ws")] retention: Arc<super::server_lifecycle::UpgradeRetention>,
) {
    let handshake = accept::tls_handshake(stream, &acceptor);
    tokio::pin!(handshake);
    let tls_stream = tokio::select! {
        biased;
        _ = wait_connection_shutdown(&mut control) => return,
        stream = &mut handshake => match stream {
            Some(stream) => stream,
            None => return,
        },
    };
    serve_owned_stream(
        tls_stream,
        state,
        liveness,
        control,
        #[cfg(feature = "ws")]
        retention,
    )
    .await;
}

#[cfg(feature = "ws")]
const OWNED_TRANSPORT_BUFFER_SIZE: usize = 8 * 1024;

#[cfg(feature = "ws")]
struct TransportStream<S> {
    reader: Option<tokio::io::ReadHalf<S>>,
    writer: tokio::io::WriteHalf<S>,
    activation: Option<tokio::sync::oneshot::Sender<tokio::io::ReadHalf<S>>>,
    incoming: tokio::sync::mpsc::Receiver<Result<bytes::Bytes, std::io::Error>>,
    pending: Option<bytes::Bytes>,
    reader_abort: tokio::task::AbortHandle,
}

#[cfg(feature = "ws")]
impl<S> Drop for TransportStream<S> {
    fn drop(&mut self) {
        self.reader_abort.abort();
    }
}

#[cfg(feature = "ws")]
impl<S> tokio::io::AsyncRead for TransportStream<S>
where
    S: tokio::io::AsyncRead + Unpin,
{
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        if let Some(reader) = self.reader.as_mut() {
            let filled = buffer.filled().len();
            let result = std::pin::Pin::new(reader).poll_read(context, buffer);
            let received_bytes =
                matches!(result, std::task::Poll::Ready(Ok(()))) && buffer.filled().len() > filled;
            self.activate_after_read(received_bytes);
            return result;
        }
        if self.copy_pending(buffer) {
            return std::task::Poll::Ready(Ok(()));
        }
        match self.incoming.poll_recv(context) {
            std::task::Poll::Ready(Some(Ok(bytes))) => {
                self.pending = Some(bytes);
                self.copy_pending(buffer);
                std::task::Poll::Ready(Ok(()))
            }
            std::task::Poll::Ready(Some(Err(error))) => std::task::Poll::Ready(Err(error)),
            std::task::Poll::Ready(None) => std::task::Poll::Ready(Ok(())),
            std::task::Poll::Pending => std::task::Poll::Pending,
        }
    }
}

#[cfg(feature = "ws")]
impl<S> TransportStream<S> {
    fn activate_after_read(&mut self, received_bytes: bool) {
        match received_bytes {
            true => self.activate_reader(),
            false => {}
        }
    }

    /// Detach the read half to the reader task, once and for this connection.
    fn activate_reader(&mut self) {
        match (self.reader.take(), self.activation.take()) {
            (Some(reader), Some(activation)) => hand_off_reader(activation, reader),
            _ => {}
        }
    }

    fn copy_pending(&mut self, buffer: &mut tokio::io::ReadBuf<'_>) -> bool {
        let mut bytes = match self.pending.take() {
            Some(bytes) => bytes,
            None => return false,
        };
        let count = bytes.len().min(buffer.remaining());
        buffer.put_slice(&bytes[..count]);
        bytes::Buf::advance(&mut bytes, count);
        if !bytes.is_empty() {
            self.pending = Some(bytes);
        }
        true
    }
}

/// Hand the read half to the detached reader task.
///
/// A refused handoff drops the read half, and every later read on this
/// connection then reports EOF — indistinguishable from a real peer close, and
/// enough to resolve every remaining guard on it as `PeerDisconnect`. The
/// reader task outlives this stream by construction, so a refusal is only
/// reachable through an abort that has already ended the connection; it is
/// recorded rather than discarded because nothing else would name it.
#[cfg(feature = "ws")]
fn hand_off_reader<S>(
    activation: tokio::sync::oneshot::Sender<tokio::io::ReadHalf<S>>,
    reader: tokio::io::ReadHalf<S>,
) {
    match activation.send(reader) {
        Ok(()) => {}
        Err(_) => tracing::debug!("upgrade transport reader is gone; connection read half dropped"),
    }
}

#[cfg(feature = "ws")]
impl<S> tokio::io::AsyncWrite for TransportStream<S>
where
    S: tokio::io::AsyncWrite + Unpin,
{
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &[u8],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        std::pin::Pin::new(&mut self.writer).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.writer).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), std::io::Error>> {
        std::pin::Pin::new(&mut self.writer).poll_shutdown(context)
    }

    fn is_write_vectored(&self) -> bool {
        self.writer.is_write_vectored()
    }

    fn poll_write_vectored(
        mut self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffers: &[std::io::IoSlice<'_>],
    ) -> std::task::Poll<Result<usize, std::io::Error>> {
        std::pin::Pin::new(&mut self.writer).poll_write_vectored(context, buffers)
    }
}

#[cfg(feature = "ws")]
struct OwnedTransport {
    handle: Option<tokio::task::JoinHandle<()>>,
    peer_closed: Option<tokio::sync::oneshot::Receiver<()>>,
    barrier: tokio::sync::mpsc::Sender<tokio::sync::oneshot::Sender<()>>,
}

#[cfg(feature = "ws")]
impl OwnedTransport {
    fn new<S>(
        stream: S,
        script: Option<Arc<super::mock::LifecycleScript>>,
    ) -> (TransportStream<S>, Self)
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let (reader, writer) = tokio::io::split(stream);
        let (activation, activated) = tokio::sync::oneshot::channel();
        let (incoming_sender, incoming) = tokio::sync::mpsc::channel(1);
        let (peer_closed_sender, peer_closed) = tokio::sync::oneshot::channel();
        let (barrier, barriers) = tokio::sync::mpsc::channel(1);
        let handle = tokio::spawn(drive_owned_reader(
            activated,
            incoming_sender,
            peer_closed_sender,
            barriers,
            script,
        ));
        let reader_abort = handle.abort_handle();
        (
            TransportStream {
                reader: Some(reader),
                writer,
                activation: Some(activation),
                incoming,
                pending: None,
                reader_abort,
            },
            Self {
                handle: Some(handle),
                peer_closed: Some(peer_closed),
                barrier,
            },
        )
    }

    async fn peer_closed(&mut self) {
        wait_for_peer_close(&mut self.peer_closed).await;
    }

    /// Wait for the reader task to end, and say so when it did not end cleanly.
    ///
    /// Cancellation is the ordinary end here — `close` aborts the task — so it
    /// is silent. A panic is not: without this the connection would degrade to
    /// "the peer closed" with nothing recording that its reader died.
    async fn join(&mut self) {
        match self.handle.take() {
            None => {}
            Some(handle) => log_reader_join(handle.await),
        }
    }

    async fn close(&mut self) {
        if let Some(handle) = self.handle.as_ref() {
            handle.abort();
        }
        self.join().await;
    }

    async fn peer_remains_open(&mut self) -> bool {
        let (acknowledgement, acknowledged) = tokio::sync::oneshot::channel();
        if self.barrier.send(acknowledgement).await.is_err() {
            return false;
        }
        tokio::select! {
            biased;
            () = wait_for_peer_close(&mut self.peer_closed) => false,
            result = acknowledged => result.is_ok(),
        }
    }
}

/// Report a reader task that panicked, and stay quiet about one that was
/// aborted: an aborted read half is how every upgrade handoff ends.
#[cfg(feature = "ws")]
fn log_reader_join(outcome: Result<(), tokio::task::JoinError>) {
    match outcome {
        Err(error) if error.is_panic() => {
            tracing::warn!("upgrade transport reader panicked: {error}");
        }
        Ok(()) | Err(_) => {}
    }
}

/// Report a transport read error the stream never received.
///
/// A delivered error reaches `TransportStream::poll_read` and is logged from
/// there. A refused send is the only path that carries a real `io::Error` and
/// has nowhere left to put it, and `SendError` hands the value back at exactly
/// that point. Dropping it would leave the connection degrading to an
/// indistinguishable "the peer closed", the same way a refused reader handoff
/// would.
#[cfg(feature = "ws")]
fn report_unsent_read_error(
    outcome: Result<(), tokio::sync::mpsc::error::SendError<Result<bytes::Bytes, std::io::Error>>>,
) {
    match outcome {
        Err(tokio::sync::mpsc::error::SendError(Err(error))) => {
            tracing::debug!("upgrade transport reader is gone; connection read failed: {error}");
        }
        Ok(()) | Err(_) => {}
    }
}

#[cfg(feature = "ws")]
async fn wait_for_peer_close(peer_closed: &mut Option<tokio::sync::oneshot::Receiver<()>>) {
    match peer_closed.as_mut() {
        Some(receiver) => {
            let _ = receiver.await;
            *peer_closed = None;
        }
        None => std::future::pending().await,
    }
}

#[cfg(feature = "ws")]
impl Drop for OwnedTransport {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.as_ref() {
            handle.abort();
        }
    }
}

#[cfg(feature = "ws")]
async fn drive_owned_reader<S>(
    activation: tokio::sync::oneshot::Receiver<tokio::io::ReadHalf<S>>,
    incoming: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
    peer_closed: tokio::sync::oneshot::Sender<()>,
    mut barriers: tokio::sync::mpsc::Receiver<tokio::sync::oneshot::Sender<()>>,
    script: Option<Arc<super::mock::LifecycleScript>>,
) where
    S: tokio::io::AsyncRead + Unpin,
{
    use tokio::io::AsyncReadExt;

    let mut reader = match activation.await {
        Ok(reader) => reader,
        Err(_) => return,
    };
    // One reusable buffer, read into directly and split off per read. Every
    // read on this path costs a chunk otherwise: activation fires on the first
    // non-empty read, so from the first byte onward the whole connection is
    // read through here whether or not an upgrade is ever attempted.
    let mut buffer = bytes::BytesMut::with_capacity(OWNED_TRANSPORT_BUFFER_SIZE);
    loop {
        // `read_buf` appends, and the buffer is emptied by the split below, so
        // this asks for one full read's room rather than growing without bound.
        buffer.reserve(OWNED_TRANSPORT_BUFFER_SIZE);
        let result = tokio::select! {
            biased;
            result = reader.read_buf(&mut buffer) => result,
            barrier = barriers.recv() => match barrier {
                Some(barrier) => {
                    let _ = barrier.send(());
                    continue;
                }
                None => break,
            },
        };
        let count = match result {
            Ok(count) => count,
            Err(error) => {
                report_unsent_read_error(incoming.send(Err(error)).await);
                break;
            }
        };
        if count == 0 {
            break;
        }
        if incoming.send(Ok(buffer.split().freeze())).await.is_err() {
            break;
        }
    }
    let _ = peer_closed.send(());
    super::mock::LifecycleScript::pause_at_upgrade(
        script.as_deref(),
        super::mock::UpgradeOwnerEdge::PeerClosed,
    )
    .await;
}

async fn serve_owned_stream<S>(
    stream: S,
    state: ConnectionState,
    liveness: Arc<ConnectionLiveness>,
    control: tokio::sync::watch::Receiver<ServerControl>,
    #[cfg(feature = "ws")] retention: Arc<super::server_lifecycle::UpgradeRetention>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    // Composes with the owned path's existing transport wrapper by sitting
    // beneath it: both wrap the raw stream, never the built IO adapter.
    let stream = liveness.wrap(stream);
    #[cfg(feature = "ws")]
    let (stream, transport) = OwnedTransport::new(stream, state.lifecycle.script());
    let io = hyper_util::rt::TokioIo::new(stream);
    serve_owned_io(
        io,
        #[cfg(feature = "ws")]
        transport,
        state,
        liveness,
        control,
        #[cfg(feature = "ws")]
        retention,
    )
    .await;
}

async fn serve_owned_io<I>(
    io: hyper_util::rt::TokioIo<I>,
    #[cfg(feature = "ws")] mut transport: OwnedTransport,
    state: ConnectionState,
    liveness: Arc<ConnectionLiveness>,
    mut control: tokio::sync::watch::Receiver<ServerControl>,
    #[cfg(feature = "ws")] retention: Arc<super::server_lifecycle::UpgradeRetention>,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    #[cfg(feature = "ws")]
    let mut state = state;
    // Binding precedes the service build: the lifecycle the service takes over
    // has to carry the upgrade transport registration, and the build is what
    // moves the state out of this frame.
    #[cfg(feature = "ws")]
    let mut upgrade_transport = state.lifecycle.bind_upgrade_transport();
    let connection = build_connection(io, state, liveness);
    tokio::pin!(connection);

    #[cfg(feature = "ws")]
    let mut peer_closed = false;
    #[cfg(feature = "ws")]
    loop {
        let event = next_owned_connection_event(
            connection.as_mut(),
            &mut control,
            &mut upgrade_transport,
            &mut transport,
        )
        .await;
        // The event decides the flow; acting on it is a separate statement, so
        // an arm that ends the connection reads the same as one that does not.
        let flow = match event {
            OwnedConnectionEvent::Complete(result) => {
                finish_owned_connection(result, &mut upgrade_transport, &mut transport).await;
                ConnectionFlow::Finished
            }
            OwnedConnectionEvent::Shutdown(mode) => {
                shutdown_owned_connection(
                    mode,
                    connection.as_mut(),
                    &mut upgrade_transport,
                    &mut transport,
                )
                .await;
                ConnectionFlow::Finished
            }
            OwnedConnectionEvent::Handoff(Some(handoff)) => {
                serve_upgrade_handoff(
                    handoff,
                    peer_closed,
                    connection.as_mut(),
                    &mut control,
                    &mut upgrade_transport,
                    &mut transport,
                    &retention,
                )
                .await
            }
            OwnedConnectionEvent::Handoff(None) => ConnectionFlow::Serving,
            OwnedConnectionEvent::PeerClosed => {
                peer_closed = true;
                ConnectionFlow::Serving
            }
        };
        match flow {
            ConnectionFlow::Finished => return,
            ConnectionFlow::Serving => {}
        }
    }

    #[cfg(not(feature = "ws"))]
    drive_connection_until_shutdown(connection.as_mut(), wait_connection_shutdown(&mut control))
        .await;
}

/// What driving one Hyper connection to its end answers with.
type ConnectionResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

/// One built connection, in the two operations every serve path needs of it.
///
/// Hyper's connection type cannot be named here: it is parameterised by the
/// opaque closure type `connection_service` returns, and by the IO type of
/// whichever transport accepted the connection. Naming what the paths do with
/// it instead — poll it to completion, and start its graceful shutdown — is
/// what lets the build be written once for both of them.
trait HyperConnection: std::future::Future<Output = ConnectionResult> {
    /// Send the peer a GOAWAY and stop accepting new work on this connection.
    ///
    /// The connection still has to be polled after this; the supervisor's one
    /// shutdown deadline is what bounds that poll.
    fn begin_graceful_shutdown(self: std::pin::Pin<&mut Self>);
}

impl<I, S, B, E> HyperConnection
    for hyper_util::server::conn::auto::UpgradeableConnection<'static, I, S, E>
where
    S: hyper::service::Service<
            hyper::Request<hyper::body::Incoming>,
            Response = hyper::Response<B>,
        >,
    S::Future: 'static,
    S::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    B: hyper::body::Body + 'static,
    B::Error: Into<Box<dyn std::error::Error + Send + Sync>>,
    I: hyper::rt::Read + hyper::rt::Write + Unpin + Send + 'static,
    E: hyper_util::server::conn::auto::HttpServerConnExec<S::Future, B>,
{
    fn begin_graceful_shutdown(self: std::pin::Pin<&mut Self>) {
        self.graceful_shutdown();
    }
}

/// Build the connection every serve path drives.
///
/// `into_owned` is what makes one builder per connection affordable to hand
/// over: Hyper's connection borrows the builder it came from, and an owned
/// connection is the only shape that can leave this frame.
fn build_connection<I>(
    io: hyper_util::rt::TokioIo<I>,
    state: ConnectionState,
    liveness: Arc<ConnectionLiveness>,
) -> impl HyperConnection + Send + use<I>
where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let builder = connection_builder(state.header_timeout);
    let service = connection_service(state, liveness);
    builder
        .serve_connection_with_upgrades(io, service)
        .into_owned()
}

/// Why a connection is winding down.
///
/// `ServerControl::Running` has no variant: it is not a reason to end a
/// connection, and every arm that ends one would otherwise carry a branch for
/// a case that must never abandon a live connection silently.
enum ConnectionShutdown {
    /// Drain the in-flight work.
    Graceful,
    /// The server is aborting. Winding down runs the same sequence; what makes
    /// it stop sooner is the supervisor aborting this task.
    Abort,
}

/// Which half of a connection's life a result came back from.
///
/// The same reason the other two-case decisions here are enums: the phase is
/// known by the call that reports the result, and cannot be inferred from the
/// result itself — an error out of a drain and an error out of a live
/// connection are the same value and mean different things. Carried rather than
/// inferred, so no site can name the wrong half by transposing a bare literal.
enum ConnectionPhase {
    /// The connection was serving requests when it ended.
    Serving,
    /// The connection was draining after its graceful shutdown began.
    Draining,
}

/// Wait for this server to leave `Running`, as a reason to wind a connection
/// down.
///
/// `wait_shutdown_control` loops until the control value leaves `Running`, so
/// the third case cannot arrive. It is answered with a future that never
/// completes rather than with a shutdown, because every caller treats the
/// answer as terminal: a `Running` shutdown would drop an accepted, still
/// serving connection with no response written and nothing logged.
async fn wait_connection_shutdown(
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
) -> ConnectionShutdown {
    match wait_shutdown_control(control).await {
        ServerControl::Graceful => ConnectionShutdown::Graceful,
        ServerControl::Abort => ConnectionShutdown::Abort,
        ServerControl::Running => {
            tracing::error!("shutdown control reported a running server; connection kept serving");
            std::future::pending().await
        }
    }
}

/// Whether the connection loop keeps serving or is done.
#[cfg(feature = "ws")]
enum ConnectionFlow {
    Serving,
    Finished,
}

/// Take one offered bridge as this connection's child, and carry it to its end.
///
/// The whole of the connection-local handoff: the child is transferred before
/// the `101` is released, held for as long as it speaks, and joined here. This
/// connection cannot end — and so cannot give its permit back — until that join
/// returns. Every step can end the connection instead, which is why it answers
/// with a flow rather than by falling off the end: the caller owns the loop.
#[cfg(feature = "ws")]
async fn serve_upgrade_handoff<C>(
    handoff: super::server_lifecycle::UpgradeHandoff,
    peer_closed: bool,
    mut connection: std::pin::Pin<&mut C>,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    upgrade_transport: &mut super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
    retention: &super::server_lifecycle::UpgradeRetention,
) -> ConnectionFlow
where
    C: HyperConnection,
{
    // Held from the moment this connection takes the offer, and before the
    // answer that releases a `101`, because either disposition is work a forced
    // abort must not take away where it stands: a transferred child owes its
    // peer a close, and a refusal owes its peer the response the handler is
    // already producing. A refusal therefore keeps the hold — the connection has
    // an answer outstanding for the rest of its life — and only the transferred
    // path has a join that says when it is over.
    retention.hold();
    let upgrade = match upgrade_transport.accept(handoff, peer_closed).await {
        Some(upgrade) => upgrade,
        None => return ConnectionFlow::Serving,
    };
    let event = await_upgrade_commitment(connection.as_mut(), control, transport).await;
    finish_upgrade_handoff(event, &upgrade, connection, upgrade_transport, transport).await;
    upgrade.join().await;
    retention.release();
    transport.join().await;
    ConnectionFlow::Finished
}

#[cfg(feature = "ws")]
async fn finish_owned_connection(
    result: ConnectionResult,
    upgrade_transport: &mut super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
) {
    upgrade_transport.cancel();
    upgrade_transport.abort_pending().await;
    log_connection_result(result, ConnectionPhase::Serving);
    transport.close().await;
}

/// Drive one connection to its end under the shutdown that ended it.
///
/// The single shutdown table every serve path reads: nothing about it is
/// WebSocket-specific or transport-specific, so the upgrade-aware loop, the
/// plain owned tail, and the synchronous path all wind down the same way, and
/// differ only in what bounds the drain that follows.
async fn shutdown_hyper_connection<C>(
    mode: ConnectionShutdown,
    mut connection: std::pin::Pin<&mut C>,
) where
    C: HyperConnection,
{
    match mode {
        ConnectionShutdown::Graceful | ConnectionShutdown::Abort => {
            connection.as_mut().begin_graceful_shutdown();
            log_connection_result(connection.await, ConnectionPhase::Draining);
        }
    }
}

/// Drive a connection until it ends on its own or a shutdown winds it down.
///
/// The race the non-upgrade serve path runs. The shutdown reason travels in as
/// an argument so the race itself is written once. The upgrade-aware loop is
/// deliberately not a caller: it races two more events and acts on each with
/// its own prologue.
#[cfg(not(feature = "ws"))]
async fn drive_connection_until_shutdown<C, F>(mut connection: std::pin::Pin<&mut C>, shutdown: F)
where
    C: HyperConnection,
    F: std::future::Future<Output = ConnectionShutdown>,
{
    tokio::select! {
        biased;
        mode = shutdown => shutdown_hyper_connection(mode, connection.as_mut()).await,
        result = connection.as_mut() => log_connection_result(result, ConnectionPhase::Serving),
    }
}

#[cfg(feature = "ws")]
async fn shutdown_owned_connection<C>(
    mode: ConnectionShutdown,
    connection: std::pin::Pin<&mut C>,
    upgrade_transport: &mut super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
) where
    C: HyperConnection,
{
    upgrade_transport.cancel();
    upgrade_transport.abort_pending().await;
    shutdown_hyper_connection(mode, connection).await;
    transport.close().await;
}

/// Hand the upgraded transport over, if the peer that asked for it is still
/// there.
///
/// A peer that went away between the response head and this point never saw the
/// `101`, so the child is ended rather than given a transport with nothing on
/// the far end of it.
#[cfg(feature = "ws")]
async fn commit_open_transport(
    upgrade: &super::server_lifecycle::UpgradeOwner,
    upgrade_transport: &super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
) {
    match transport.peer_remains_open().await {
        true => upgrade_transport.commit(),
        false => {
            upgrade_transport.cancel();
            upgrade.cancel();
        }
    }
}

/// Why a transferred child is given up instead of handed the transport.
///
/// Split from the event so the cancel-then-end prologue every abandonment runs
/// is written once rather than restated per arm.
#[cfg(feature = "ws")]
enum AbandonedCommitment {
    /// The connection ended with an error, which is reported after the drop.
    Ended(ConnectionResult),
    /// The server is winding down, so the connection drains under it.
    Shutdown(ConnectionShutdown),
    /// The peer went away; the connection is polled to its end.
    PeerClosed,
}

#[cfg(feature = "ws")]
async fn finish_upgrade_handoff<C>(
    event: UpgradeCommitmentEvent,
    upgrade: &super::server_lifecycle::UpgradeOwner,
    connection: std::pin::Pin<&mut C>,
    upgrade_transport: &super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
) where
    C: HyperConnection,
{
    let abandoned = match event {
        // The one arm that hands the transport over, and so the one arm that
        // does not run the prologue below.
        UpgradeCommitmentEvent::Complete(result) if result.is_ok() => {
            commit_open_transport(upgrade, upgrade_transport, transport).await;
            log_connection_result(result, ConnectionPhase::Serving);
            return;
        }
        UpgradeCommitmentEvent::Complete(result) => AbandonedCommitment::Ended(result),
        UpgradeCommitmentEvent::Shutdown(mode) => AbandonedCommitment::Shutdown(mode),
        UpgradeCommitmentEvent::PeerClosed => AbandonedCommitment::PeerClosed,
    };
    upgrade_transport.cancel();
    upgrade.cancel();
    match abandoned {
        AbandonedCommitment::Ended(result) => {
            log_connection_result(result, ConnectionPhase::Serving)
        }
        AbandonedCommitment::Shutdown(mode) => {
            shutdown_hyper_connection(mode, connection).await;
        }
        AbandonedCommitment::PeerClosed => {
            log_connection_result(connection.await, ConnectionPhase::Serving)
        }
    }
}

#[cfg(feature = "ws")]
enum OwnedConnectionEvent {
    Complete(ConnectionResult),
    Shutdown(ConnectionShutdown),
    Handoff(Option<super::server_lifecycle::UpgradeHandoff>),
    PeerClosed,
}

#[cfg(feature = "ws")]
async fn next_owned_connection_event<C>(
    mut connection: std::pin::Pin<&mut C>,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    transport: &mut super::server_lifecycle::UpgradeTransportOwner,
    owned_transport: &mut OwnedTransport,
) -> OwnedConnectionEvent
where
    C: HyperConnection,
{
    tokio::select! {
        biased;
        () = owned_transport.peer_closed() => OwnedConnectionEvent::PeerClosed,
        result = connection.as_mut() => OwnedConnectionEvent::Complete(result),
        mode = wait_connection_shutdown(control) => OwnedConnectionEvent::Shutdown(mode),
        handoff = transport.next_handoff() => OwnedConnectionEvent::Handoff(handoff),
    }
}

#[cfg(feature = "ws")]
enum UpgradeCommitmentEvent {
    Complete(ConnectionResult),
    Shutdown(ConnectionShutdown),
    PeerClosed,
}

#[cfg(feature = "ws")]
async fn await_upgrade_commitment<C>(
    mut connection: std::pin::Pin<&mut C>,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    transport: &mut OwnedTransport,
) -> UpgradeCommitmentEvent
where
    C: HyperConnection,
{
    tokio::select! {
        biased;
        () = transport.peer_closed() => UpgradeCommitmentEvent::PeerClosed,
        result = connection.as_mut() => UpgradeCommitmentEvent::Complete(result),
        mode = wait_connection_shutdown(control) => UpgradeCommitmentEvent::Shutdown(mode),
    }
}

/// Serve one request under its own response-lifetime guard.
///
/// The guard is armed here, held across the handler, and moves into the
/// response body — the only holder that can still resolve the signal. Liveness
/// arrives by value because the guard becomes its holder for the rest of the
/// response; the service closure's per-request clone is the one it keeps.
///
/// The request method is read before the request is consumed, because a `HEAD`
/// gets a response Hyper never writes a body for, and this is the last place
/// that fact is still available to the body that has to know it.
async fn serve_request(
    request: hyper::Request<hyper::body::Incoming>,
    router: &ServerDispatch,
    ctx: &ConnCtx,
    remote_addr: Option<std::net::IpAddr>,
    lifecycle: &ConnectionLifecycle,
    liveness: Arc<ConnectionLiveness>,
) -> Result<hyper::Response<GuardedBody>, std::convert::Infallible> {
    let bodyless_request = request.method() == hyper::Method::HEAD;
    // The one place a request becomes a child of this connection. Recorded
    // passively at both ends, so the ownership record shows the request nested
    // under the connection identity rather than beside it.
    let owner = lifecycle.admit_request();
    // The request clock starts here, before dispatch reads the head. It is the
    // only clock every route class shares: one per class put three meanings of
    // "request duration" into one histogram, and one started after the body was
    // read left out the inbound time a slow or large upload is made of.
    //
    // The identity is minted from the raw head for the same reason. Every later
    // stage refines it, and an operation whose peer leaves before any exit
    // answers is still a request an operator can name.
    let account = super::completion::CompletionAccount::begin(
        ctx,
        lifecycle.script(),
        super::rejection::RequestIdentity::admitted(
            super::rejection::RequestId::generate(),
            request.method(),
            request.uri(),
            remote_addr,
            request.version(),
        ),
    );
    // The finalizer moves into the response guard, which is this operation's
    // one response lifetime. Whichever way that lifetime ends, its drop is the
    // moment the record is written.
    let (lifetime, guard) = liveness.begin_response(super::completion::OperationFinalizer::owning(
        Arc::clone(&account),
        lifecycle.stop(),
    ));
    let response = handle_request(
        request,
        router,
        ctx,
        remote_addr,
        lifecycle,
        lifetime,
        &account,
    )
    .await?;
    drop(owner);
    Ok(GuardedBody::attach(response, guard, bodyless_request))
}

fn connection_builder(
    header_timeout: std::time::Duration,
) -> hyper_util::server::conn::auto::Builder<hyper_util::rt::TokioExecutor> {
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http1()
        .keep_alive(true)
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Some(header_timeout));
    builder
}

fn log_connection_result(result: ConnectionResult, phase: ConnectionPhase) {
    match (result, phase) {
        (Ok(()), _) => {}
        (Err(ref error), _) if is_benign_hyper_error(&**error) => {}
        (Err(error), ConnectionPhase::Draining) => {
            tracing::warn!("connection error during shutdown: {error}");
        }
        (Err(error), ConnectionPhase::Serving) => tracing::warn!("connection error: {error}"),
    }
}

fn is_benign_hyper_error(err: &(dyn std::error::Error + 'static)) -> bool {
    let mut source: Option<&(dyn std::error::Error + 'static)> = Some(err);
    while let Some(e) = source {
        match e.downcast_ref::<std::io::Error>() {
            Some(io_err) => return crate::error::is_benign_io(io_err),
            None => source = e.source(),
        }
    }
    false
}
