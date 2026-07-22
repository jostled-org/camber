use super::handle::{ConnCtx, handle_request};
use super::router::ServerDispatch;
use super::server_lifecycle::{ConnectionLifecycle, ServerControl};
use crate::net::accept;
use crate::{RuntimeError, net};
use std::sync::Arc;

struct SyncConnectionState {
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown: crate::runtime_state::ShutdownSignal,
    keepalive_timeout: std::time::Duration,
    remote_ip: std::net::IpAddr,
    lifecycle: ConnectionLifecycle,
}

pub(super) async fn accept_loop(
    listener: &net::Listener,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown: crate::runtime_state::ShutdownSignal,
    keepalive_timeout: std::time::Duration,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    conn_limit: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<(), RuntimeError> {
    match &listener.inner {
        net::ListenerInner::Tcp(tcp) => {
            accept_tcp(
                tcp,
                router,
                ctx,
                shutdown,
                keepalive_timeout,
                tls_acceptor,
                conn_limit,
            )
            .await
        }
        net::ListenerInner::Unix(unix, _) => {
            accept_unix(unix, router, ctx, shutdown, keepalive_timeout, conn_limit).await
        }
    }
}

pub(super) async fn accept_tcp(
    listener: &tokio::net::TcpListener,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown: crate::runtime_state::ShutdownSignal,
    keepalive_timeout: std::time::Duration,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    conn_limit: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<(), RuntimeError> {
    let script = listener
        .local_addr()
        .ok()
        .and_then(super::mock::lifecycle_script);
    accept::accept_loop_with_permit(
        listener,
        &shutdown,
        conn_limit.as_ref(),
        script.as_ref(),
        |(stream, addr), permit| {
            let router = Arc::clone(&router);
            let ctx = Arc::clone(&ctx);
            let shutdown = shutdown.clone();
            let acceptor = tls_acceptor.clone();
            let remote_ip = addr.ip();
            async move {
                match acceptor {
                    Some(a) => {
                        let state = SyncConnectionState {
                            router,
                            ctx,
                            shutdown,
                            keepalive_timeout,
                            remote_ip,
                            lifecycle: ConnectionLifecycle::synchronous(permit),
                        };
                        serve_tls_connection(stream, a, state).await;
                    }
                    None => {
                        serve_stream(
                            stream,
                            router,
                            ctx,
                            shutdown,
                            keepalive_timeout,
                            Some(remote_ip),
                            ConnectionLifecycle::synchronous(permit),
                        )
                        .await;
                    }
                }
            }
        },
    )
    .await
}

async fn accept_unix(
    listener: &tokio::net::UnixListener,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown: crate::runtime_state::ShutdownSignal,
    keepalive_timeout: std::time::Duration,
    conn_limit: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<(), RuntimeError> {
    accept::accept_loop_with_permit(
        listener,
        &shutdown,
        conn_limit.as_ref(),
        None,
        |stream, permit| {
            let router = Arc::clone(&router);
            let ctx = Arc::clone(&ctx);
            let shutdown = shutdown.clone();
            async move {
                serve_stream(
                    stream,
                    router,
                    ctx,
                    shutdown,
                    keepalive_timeout,
                    None,
                    ConnectionLifecycle::synchronous(permit),
                )
                .await;
            }
        },
    )
    .await
}

async fn serve_stream<S>(
    stream: S,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown: crate::runtime_state::ShutdownSignal,
    keepalive_timeout: std::time::Duration,
    remote_addr: Option<std::net::IpAddr>,
    lifecycle: ConnectionLifecycle,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let io = hyper_util::rt::TokioIo::new(stream);
    serve_io(
        io,
        router,
        ctx,
        shutdown,
        keepalive_timeout,
        remote_addr,
        lifecycle,
    )
    .await;
}

async fn serve_tls_connection(
    stream: tokio::net::TcpStream,
    acceptor: tokio_rustls::TlsAcceptor,
    state: SyncConnectionState,
) {
    let tls_stream = match accept::tls_handshake(stream, &acceptor).await {
        Some(s) => s,
        None => return,
    };
    serve_stream(
        tls_stream,
        state.router,
        state.ctx,
        state.shutdown,
        state.keepalive_timeout,
        Some(state.remote_ip),
        state.lifecycle,
    )
    .await;
}

async fn serve_io<I>(
    io: hyper_util::rt::TokioIo<I>,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown: crate::runtime_state::ShutdownSignal,
    keepalive_timeout: std::time::Duration,
    remote_addr: Option<std::net::IpAddr>,
    lifecycle: ConnectionLifecycle,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let router = Arc::clone(&router);
        let ctx = Arc::clone(&ctx);
        let lifecycle = lifecycle.clone();
        async move { handle_request(req, &router, &ctx, remote_addr, &lifecycle).await }
    });

    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http1()
        .keep_alive(true)
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Some(keepalive_timeout));
    let conn = builder.serve_connection_with_upgrades(io, service);

    tokio::pin!(conn);
    tokio::select! {
        biased;
        () = shutdown.wait() => {
            // Signal HTTP/2 GOAWAY and let in-flight streams finish.
            conn.as_mut().graceful_shutdown();
            match tokio::time::timeout(std::time::Duration::from_secs(15), conn).await {
                Ok(Ok(())) => {}
                Ok(Err(ref e)) if is_benign_hyper_error(e.as_ref()) => {}
                Ok(Err(e)) => tracing::warn!("connection error during shutdown: {e}"),
                Err(_) => tracing::debug!("connection timed out during graceful shutdown"),
            }
        }
        result = &mut conn => {
            match result {
                Ok(()) => {}
                Err(ref e) if is_benign_hyper_error(e.as_ref()) => {}
                Err(e) => tracing::warn!("connection error: {e}"),
            }
        }
    }
}

pub(super) async fn serve_owned_connection(
    stream: tokio::net::TcpStream,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    lifecycle: ConnectionLifecycle,
    keepalive_timeout: std::time::Duration,
    remote_addr: std::net::IpAddr,
) {
    match tls_acceptor {
        Some(acceptor) => {
            serve_owned_tls(
                stream,
                acceptor,
                router,
                ctx,
                lifecycle,
                keepalive_timeout,
                remote_addr,
            )
            .await;
        }
        None => {
            serve_owned_stream(
                stream,
                router,
                ctx,
                lifecycle,
                keepalive_timeout,
                remote_addr,
            )
            .await;
        }
    }
}

async fn serve_owned_tls(
    stream: tokio::net::TcpStream,
    acceptor: tokio_rustls::TlsAcceptor,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    lifecycle: ConnectionLifecycle,
    keepalive_timeout: std::time::Duration,
    remote_addr: std::net::IpAddr,
) {
    let mut control = match lifecycle.control() {
        Some(control) => control,
        None => return,
    };
    let handshake = accept::tls_handshake(stream, &acceptor);
    tokio::pin!(handshake);
    let tls_stream = tokio::select! {
        biased;
        _ = wait_for_shutdown(&mut control) => return,
        stream = &mut handshake => match stream {
            Some(stream) => stream,
            None => return,
        },
    };
    serve_owned_stream(
        tls_stream,
        router,
        ctx,
        lifecycle,
        keepalive_timeout,
        remote_addr,
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

    fn activate_reader(&mut self) {
        if let (Some(reader), Some(activation)) = (self.reader.take(), self.activation.take()) {
            let _ = activation.send(reader);
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

    async fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let _ = handle.await;
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
    let mut buffer = [0; OWNED_TRANSPORT_BUFFER_SIZE];
    loop {
        let result = tokio::select! {
            biased;
            result = reader.read(&mut buffer) => result,
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
                let _ = incoming.send(Err(error)).await;
                break;
            }
        };
        if count == 0 {
            break;
        }
        let bytes = bytes::Bytes::copy_from_slice(&buffer[..count]);
        if incoming.send(Ok(bytes)).await.is_err() {
            break;
        }
    }
    let _ = peer_closed.send(());
    if let Some(script) = script {
        script
            .pause(super::mock::LifecycleCheckpoint::UpgradePeerClosed)
            .await;
    }
}

async fn serve_owned_stream<S>(
    stream: S,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    lifecycle: ConnectionLifecycle,
    keepalive_timeout: std::time::Duration,
    remote_addr: std::net::IpAddr,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    #[cfg(feature = "ws")]
    let (stream, transport) = OwnedTransport::new(stream, lifecycle.script());
    let io = hyper_util::rt::TokioIo::new(stream);
    serve_owned_io(
        io,
        #[cfg(feature = "ws")]
        transport,
        router,
        ctx,
        lifecycle,
        keepalive_timeout,
        Some(remote_addr),
    )
    .await;
}

async fn serve_owned_io<I>(
    io: hyper_util::rt::TokioIo<I>,
    #[cfg(feature = "ws")] mut transport: OwnedTransport,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    lifecycle: ConnectionLifecycle,
    keepalive_timeout: std::time::Duration,
    remote_addr: Option<std::net::IpAddr>,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    #[cfg(feature = "ws")]
    let mut lifecycle = lifecycle;
    #[cfg(feature = "ws")]
    let mut upgrade_transport = lifecycle.bind_upgrade_transport();
    let service_lifecycle = lifecycle.clone();
    let service = hyper::service::service_fn(move |request| {
        let router = Arc::clone(&router);
        let ctx = Arc::clone(&ctx);
        let lifecycle = service_lifecycle.clone();
        async move { handle_request(request, &router, &ctx, remote_addr, &lifecycle).await }
    });
    let builder = connection_builder(keepalive_timeout);
    let connection = builder.serve_connection_with_upgrades(io, service);
    tokio::pin!(connection);
    let mut control = match lifecycle.control() {
        Some(control) => control,
        None => return,
    };

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
        match event {
            OwnedConnectionEvent::Complete(result) => {
                finish_owned_connection(result, &mut upgrade_transport, &mut transport).await;
                return;
            }
            OwnedConnectionEvent::Shutdown(mode) => {
                shutdown_owned_connection(
                    mode,
                    connection.as_mut(),
                    &mut upgrade_transport,
                    &mut transport,
                    |mut connection| connection.as_mut().graceful_shutdown(),
                )
                .await;
                return;
            }
            OwnedConnectionEvent::Registration(Some(mut registration)) => {
                let cancellation = registration.prepare();
                cancel_closed_registration(peer_closed, cancellation.as_ref());
                let registration = registration.register();
                tokio::pin!(registration);
                let event = await_upgrade_registration(
                    connection.as_mut(),
                    &mut control,
                    registration.as_mut(),
                    &mut transport,
                )
                .await;
                let outcome = finish_interrupted_registration(
                    event,
                    connection.as_mut(),
                    registration.as_mut(),
                    cancellation.as_ref(),
                    &mut upgrade_transport,
                    &mut transport,
                    |mut connection| connection.as_mut().graceful_shutdown(),
                )
                .await;
                let Some(outcome) = outcome else {
                    return;
                };
                let outcome = retain_open_upgrade(
                    outcome,
                    connection.as_mut(),
                    &mut upgrade_transport,
                    &mut transport,
                )
                .await;
                let Some(outcome) = outcome else {
                    return;
                };
                let Some(commitment) = outcome.complete() else {
                    continue;
                };
                let event =
                    await_upgrade_commitment(connection.as_mut(), &mut control, &mut transport)
                        .await;
                finish_upgrade_commitment(
                    event,
                    commitment,
                    connection.as_mut(),
                    &upgrade_transport,
                    &mut transport,
                    |mut connection| connection.as_mut().graceful_shutdown(),
                )
                .await;
                transport.join().await;
                return;
            }
            OwnedConnectionEvent::Registration(None) => {}
            OwnedConnectionEvent::PeerClosed => peer_closed = true,
        }
    }

    #[cfg(not(feature = "ws"))]
    tokio::select! {
        biased;
        mode = wait_for_shutdown(&mut control) => match mode {
            ServerControl::Graceful | ServerControl::Abort => {
                connection.as_mut().graceful_shutdown();
                log_connection_result(connection.await, true);
            }
            ServerControl::Running => {}
        },
        result = &mut connection => log_connection_result(result, false),
    }
}

#[cfg(feature = "ws")]
trait LoggableConnectionError: AsRef<dyn std::error::Error + Send + Sync> + std::fmt::Display {}

#[cfg(feature = "ws")]
impl<T> LoggableConnectionError for T where
    T: AsRef<dyn std::error::Error + Send + Sync> + std::fmt::Display
{
}

#[cfg(feature = "ws")]
async fn finish_owned_connection<E: LoggableConnectionError>(
    result: Result<(), E>,
    upgrade_transport: &mut super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
) {
    upgrade_transport.cancel();
    upgrade_transport.abort_pending().await;
    log_connection_result(result, false);
    transport.close().await;
}

#[cfg(feature = "ws")]
async fn shutdown_hyper_connection<C, E, F>(
    mode: ServerControl,
    mut connection: std::pin::Pin<&mut C>,
    begin_shutdown: F,
) where
    C: std::future::Future<Output = Result<(), E>>,
    E: LoggableConnectionError,
    F: FnOnce(std::pin::Pin<&mut C>),
{
    match mode {
        ServerControl::Graceful | ServerControl::Abort => {
            begin_shutdown(connection.as_mut());
            log_connection_result(connection.await, true);
        }
        ServerControl::Running => {}
    }
}

#[cfg(feature = "ws")]
async fn shutdown_owned_connection<C, E, F>(
    mode: ServerControl,
    connection: std::pin::Pin<&mut C>,
    upgrade_transport: &mut super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
    begin_shutdown: F,
) where
    C: std::future::Future<Output = Result<(), E>>,
    E: LoggableConnectionError,
    F: FnOnce(std::pin::Pin<&mut C>),
{
    upgrade_transport.cancel();
    upgrade_transport.abort_pending().await;
    shutdown_hyper_connection(mode, connection, begin_shutdown).await;
    transport.close().await;
}

#[cfg(feature = "ws")]
fn cancel_prepared_upgrade(cancellation: Option<&super::server_lifecycle::UpgradeCancellation>) {
    match cancellation {
        Some(cancellation) => cancellation.cancel(),
        None => {}
    }
}

#[cfg(feature = "ws")]
fn cancel_closed_registration(
    peer_closed: bool,
    cancellation: Option<&super::server_lifecycle::UpgradeCancellation>,
) {
    match (peer_closed, cancellation) {
        (true, Some(cancellation)) => cancellation.cancel(),
        _ => {}
    }
}

#[cfg(feature = "ws")]
async fn settle_interrupted_registration<R>(
    registration: std::pin::Pin<&mut R>,
    cancellation: Option<&super::server_lifecycle::UpgradeCancellation>,
    upgrade_transport: &mut super::server_lifecycle::UpgradeTransportOwner,
) where
    R: std::future::Future<Output = super::server_lifecycle::TransportRegistrationOutcome>,
{
    upgrade_transport.cancel();
    cancel_prepared_upgrade(cancellation);
    let outcome = registration.await;
    drop(outcome.complete());
    upgrade_transport.abort_pending().await;
}

#[cfg(feature = "ws")]
async fn finish_interrupted_registration<C, E, R, F>(
    event: UpgradeRegistrationEvent<E>,
    connection: std::pin::Pin<&mut C>,
    registration: std::pin::Pin<&mut R>,
    cancellation: Option<&super::server_lifecycle::UpgradeCancellation>,
    upgrade_transport: &mut super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
    begin_shutdown: F,
) -> Option<super::server_lifecycle::TransportRegistrationOutcome>
where
    C: std::future::Future<Output = Result<(), E>>,
    E: LoggableConnectionError,
    R: std::future::Future<Output = super::server_lifecycle::TransportRegistrationOutcome>,
    F: FnOnce(std::pin::Pin<&mut C>),
{
    match event {
        UpgradeRegistrationEvent::Registered(outcome) => Some(outcome),
        UpgradeRegistrationEvent::Complete(result) => {
            settle_interrupted_registration(registration, cancellation, upgrade_transport).await;
            log_connection_result(result, false);
            transport.close().await;
            None
        }
        UpgradeRegistrationEvent::Shutdown(mode) => {
            settle_interrupted_registration(registration, cancellation, upgrade_transport).await;
            shutdown_hyper_connection(mode, connection, begin_shutdown).await;
            transport.close().await;
            None
        }
        UpgradeRegistrationEvent::PeerClosed => {
            settle_interrupted_registration(registration, cancellation, upgrade_transport).await;
            log_connection_result(connection.await, false);
            transport.close().await;
            None
        }
    }
}

#[cfg(feature = "ws")]
async fn cancel_admitted_upgrade<C, E>(
    outcome: super::server_lifecycle::TransportRegistrationOutcome,
    connection: std::pin::Pin<&mut C>,
    upgrade_transport: &mut super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
) where
    C: std::future::Future<Output = Result<(), E>>,
    E: LoggableConnectionError,
{
    upgrade_transport.cancel();
    outcome.cancel();
    upgrade_transport.abort_pending().await;
    log_connection_result(connection.await, false);
    transport.close().await;
}

#[cfg(feature = "ws")]
async fn retain_open_upgrade<C, E>(
    outcome: super::server_lifecycle::TransportRegistrationOutcome,
    connection: std::pin::Pin<&mut C>,
    upgrade_transport: &mut super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
) -> Option<super::server_lifecycle::TransportRegistrationOutcome>
where
    C: std::future::Future<Output = Result<(), E>>,
    E: LoggableConnectionError,
{
    let peer_open = match outcome.admitted() {
        true => transport.peer_remains_open().await,
        false => true,
    };
    match peer_open {
        true => Some(outcome),
        false => {
            cancel_admitted_upgrade(outcome, connection, upgrade_transport, transport).await;
            None
        }
    }
}

#[cfg(feature = "ws")]
async fn commit_open_transport(
    commitment: super::server_lifecycle::UpgradeCommitment,
    upgrade_transport: &super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
) {
    match transport.peer_remains_open().await {
        true => {
            commitment.commit();
            upgrade_transport.commit();
        }
        false => {
            upgrade_transport.cancel();
            drop(commitment);
        }
    }
}

#[cfg(feature = "ws")]
async fn finish_upgrade_commitment<C, E, F>(
    event: UpgradeCommitmentEvent<E>,
    commitment: super::server_lifecycle::UpgradeCommitment,
    connection: std::pin::Pin<&mut C>,
    upgrade_transport: &super::server_lifecycle::UpgradeTransportOwner,
    transport: &mut OwnedTransport,
    begin_shutdown: F,
) where
    C: std::future::Future<Output = Result<(), E>>,
    E: LoggableConnectionError,
    F: FnOnce(std::pin::Pin<&mut C>),
{
    match event {
        UpgradeCommitmentEvent::Complete(result) if result.is_ok() => {
            commit_open_transport(commitment, upgrade_transport, transport).await;
            log_connection_result(result, false);
        }
        UpgradeCommitmentEvent::Complete(result) => {
            upgrade_transport.cancel();
            drop(commitment);
            log_connection_result(result, false);
        }
        UpgradeCommitmentEvent::Shutdown(mode) => {
            upgrade_transport.cancel();
            drop(commitment);
            shutdown_hyper_connection(mode, connection, begin_shutdown).await;
        }
        UpgradeCommitmentEvent::PeerClosed => {
            upgrade_transport.cancel();
            drop(commitment);
            log_connection_result(connection.await, false);
        }
    }
}

#[cfg(feature = "ws")]
enum OwnedConnectionEvent<E> {
    Complete(Result<(), E>),
    Shutdown(ServerControl),
    Registration(Option<super::server_lifecycle::TransportRegistration>),
    PeerClosed,
}

#[cfg(feature = "ws")]
async fn next_owned_connection_event<C, E>(
    mut connection: std::pin::Pin<&mut C>,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    transport: &mut super::server_lifecycle::UpgradeTransportOwner,
    owned_transport: &mut OwnedTransport,
) -> OwnedConnectionEvent<E>
where
    C: std::future::Future<Output = Result<(), E>>,
{
    tokio::select! {
        biased;
        () = owned_transport.peer_closed() => OwnedConnectionEvent::PeerClosed,
        result = connection.as_mut() => OwnedConnectionEvent::Complete(result),
        mode = wait_for_shutdown(control) => OwnedConnectionEvent::Shutdown(mode),
        registration = transport.next_registration() => {
            OwnedConnectionEvent::Registration(registration)
        }
    }
}

#[cfg(feature = "ws")]
enum UpgradeRegistrationEvent<E> {
    Complete(Result<(), E>),
    Shutdown(ServerControl),
    Registered(super::server_lifecycle::TransportRegistrationOutcome),
    PeerClosed,
}

#[cfg(feature = "ws")]
async fn await_upgrade_registration<C, E, R>(
    mut connection: std::pin::Pin<&mut C>,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    registration: std::pin::Pin<&mut R>,
    transport: &mut OwnedTransport,
) -> UpgradeRegistrationEvent<E>
where
    C: std::future::Future<Output = Result<(), E>>,
    R: std::future::Future<Output = super::server_lifecycle::TransportRegistrationOutcome>,
{
    tokio::select! {
        biased;
        () = transport.peer_closed() => UpgradeRegistrationEvent::PeerClosed,
        result = connection.as_mut() => UpgradeRegistrationEvent::Complete(result),
        mode = wait_for_shutdown(control) => UpgradeRegistrationEvent::Shutdown(mode),
        outcome = registration => UpgradeRegistrationEvent::Registered(outcome),
    }
}

#[cfg(feature = "ws")]
enum UpgradeCommitmentEvent<E> {
    Complete(Result<(), E>),
    Shutdown(ServerControl),
    PeerClosed,
}

#[cfg(feature = "ws")]
async fn await_upgrade_commitment<C, E>(
    mut connection: std::pin::Pin<&mut C>,
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
    transport: &mut OwnedTransport,
) -> UpgradeCommitmentEvent<E>
where
    C: std::future::Future<Output = Result<(), E>>,
{
    tokio::select! {
        biased;
        () = transport.peer_closed() => UpgradeCommitmentEvent::PeerClosed,
        result = connection.as_mut() => UpgradeCommitmentEvent::Complete(result),
        mode = wait_for_shutdown(control) => UpgradeCommitmentEvent::Shutdown(mode),
    }
}

fn connection_builder(
    keepalive_timeout: std::time::Duration,
) -> hyper_util::server::conn::auto::Builder<hyper_util::rt::TokioExecutor> {
    let mut builder =
        hyper_util::server::conn::auto::Builder::new(hyper_util::rt::TokioExecutor::new());
    builder
        .http1()
        .keep_alive(true)
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Some(keepalive_timeout));
    builder
}

async fn wait_for_shutdown(
    control: &mut tokio::sync::watch::Receiver<ServerControl>,
) -> ServerControl {
    loop {
        let current = *control.borrow_and_update();
        if current != ServerControl::Running {
            return current;
        }
        match control.changed().await {
            Ok(()) => {}
            Err(_) => return current,
        }
    }
}

fn log_connection_result<E>(result: Result<(), E>, draining: bool)
where
    E: AsRef<dyn std::error::Error + Send + Sync> + std::fmt::Display,
{
    match (result, draining) {
        (Ok(()), _) => {}
        (Err(ref error), _) if is_benign_hyper_error(error.as_ref()) => {}
        (Err(error), true) => tracing::warn!("connection error during shutdown: {error}"),
        (Err(error), false) => tracing::warn!("connection error: {error}"),
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
