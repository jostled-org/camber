use super::BufferConfig;
use super::conn::accept_loop;
use super::handle::ConnCtx;
use super::router::{Router, ServerDispatch};
use super::server_lifecycle::{
    ServerContextSnapshot, ServerControl, ServerSupervisor, SupervisorJoin, poll_supervisor_join,
};
use crate::task::spawn_async;
use crate::{RuntimeError, net, runtime};
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Armed lifecycle owner for a background HTTP server.
///
/// [`shutdown`](Self::shutdown) requests graceful shutdown, while
/// [`cancel`](Self::cancel) requests forced cancellation. `Drop` records
/// `Abort` before releasing control, so discarding an unfinished owner forces
/// shutdown while the independently running supervisor continues to join its
/// work.
///
/// Awaiting this handle is equivalent to [`join`](Self::join) and returns one
/// flat `Result<(), RuntimeError>`. A successful join proves that every owned
/// accepted transport, connection permit, and registered WebSocket bridge has
/// completed. It does not prove termination of non-yielding async execution.
/// It is not proof that a non-cooperative callback has released its callback-held
/// `Request`, handler captures, or `WsConn`. It also does not prove runtime
/// teardown after the signal watcher is gone.
///
/// Under plain Tokio, only per-server methods trigger shutdown and standalone
/// timeout defaults apply. Under Camber, runtime shutdown and active signal
/// watcher notifications can also request graceful shutdown.
pub struct ServerHandle {
    control: Option<tokio::sync::watch::Sender<ServerControl>>,
    join: Option<SupervisorJoin>,
}

impl ServerHandle {
    /// Request graceful shutdown without consuming the owner.
    ///
    /// Admission stops, active HTTP work receives graceful protocol shutdown,
    /// and the aggregate grace deadline starts once. Call [`join`](Self::join)
    /// or await the handle to observe completion.
    pub fn shutdown(&self) {
        if let Some(control) = self.control.as_ref() {
            ServerControl::send_graceful(control);
        }
    }

    /// Transfer lifecycle authority into the concrete join future.
    ///
    /// This does not initiate shutdown. The returned future retains
    /// [`shutdown`](ServerHandleFuture::shutdown) and
    /// [`cancel`](ServerHandleFuture::cancel) authority.
    pub fn join(mut self) -> ServerHandleFuture {
        ServerHandleFuture::new(self.control.take(), self.join.take())
    }

    /// Request graceful shutdown and transfer authority into the join future.
    pub fn shutdown_and_join(self) -> ServerHandleFuture {
        self.shutdown();
        self.join()
    }

    /// Request forced cancellation of the background server.
    ///
    /// Cancellation is idempotent. The eventual result is
    /// `RuntimeError::Cancelled` unless timeout or another immutable result was
    /// already fixed. Awaiting still waits for cooperatively abortable owned
    /// transports to be joined.
    pub fn cancel(&self) {
        if let Some(control) = self.control.as_ref() {
            ServerControl::send_abort(control);
        }
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            ServerControl::send_abort(&control);
        }
    }
}

impl IntoFuture for ServerHandle {
    type Output = Result<(), RuntimeError>;
    type IntoFuture = ServerHandleFuture;

    fn into_future(self) -> Self::IntoFuture {
        self.join()
    }
}

/// Armed concrete future for controlling and joining a background HTTP server.
///
/// `ServerHandleFuture` has the same flat `Result<(), RuntimeError>` output
/// whether produced by [`ServerHandle::join`],
/// [`ServerHandle::shutdown_and_join`], or `ServerHandle::into_future`. Polling
/// a ready result disarms Drop before returning it; dropping a pending future
/// requests forced cancellation and leaves the supervisor running to own and
/// join its tasks.
///
/// Successful join proves completion of each owned accepted transport,
/// connection permit, and registered WebSocket bridge. It does not prove
/// termination of non-yielding async execution. It is not proof that a
/// non-cooperative callback has released its callback-held `Request`, handler
/// captures, or `WsConn`, nor does it prove runtime teardown after the signal
/// watcher is gone.
pub struct ServerHandleFuture {
    control: Option<tokio::sync::watch::Sender<ServerControl>>,
    join: Option<SupervisorJoin>,
}

impl ServerHandleFuture {
    fn new(
        control: Option<tokio::sync::watch::Sender<ServerControl>>,
        join: Option<SupervisorJoin>,
    ) -> Self {
        Self { control, join }
    }

    pub(super) fn from_join(join: SupervisorJoin) -> Self {
        Self::new(None, Some(join))
    }

    /// Request graceful shutdown while retaining this join future.
    pub fn shutdown(&self) {
        if let Some(control) = self.control.as_ref() {
            ServerControl::send_graceful(control);
        }
    }

    /// Request forced cancellation while retaining this join future.
    pub fn cancel(&self) {
        if let Some(control) = self.control.as_ref() {
            ServerControl::send_abort(control);
        }
    }
}

impl Future for ServerHandleFuture {
    type Output = Result<(), RuntimeError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let result = match self.join.as_mut() {
            Some(join) => poll_supervisor_join(join, cx),
            None => Poll::Ready(Err(RuntimeError::TaskPanicked(
                "server supervisor join authority unavailable".into(),
            ))),
        };
        match result {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.control.take();
                Poll::Ready(result)
            }
        }
    }
}

impl Drop for ServerHandleFuture {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            ServerControl::send_abort(&control);
        }
    }
}

/// Build a semaphore from an optional connection limit.
fn make_conn_limit(limit: Option<usize>) -> Option<Arc<tokio::sync::Semaphore>> {
    limit.map(|n| Arc::new(tokio::sync::Semaphore::new(n)))
}

/// Serve HTTP on a Tokio TCP listener without Camber's runtime.
///
/// Runs the hyper accept loop directly on the caller's Tokio runtime.
/// Designed for embedding Camber's router in another application
/// (e.g. Kingpin's dashboard alongside its DNS server).
///
/// Runs until the spawned task is cancelled or the listener is closed.
pub async fn serve_async(
    listener: tokio::net::TcpListener,
    router: Router,
) -> Result<(), RuntimeError> {
    let buffers = router.buffer_config();
    let dispatch = ServerDispatch::Single(router.freeze());
    serve_async_dispatch(listener, dispatch, None, buffers).await
}

/// Serve HTTPS on a Tokio TCP listener without Camber's runtime.
///
/// Same as `serve_async` but wraps each connection in TLS via tokio-rustls.
pub async fn serve_async_tls(
    listener: tokio::net::TcpListener,
    router: Router,
    tls_config: Arc<rustls::ServerConfig>,
) -> Result<(), RuntimeError> {
    let buffers = router.buffer_config();
    let dispatch = ServerDispatch::Single(router.freeze());
    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
    serve_async_dispatch(listener, dispatch, Some(acceptor), buffers).await
}

/// Serve HTTP with host-based routing on a Tokio TCP listener.
///
/// Dispatches by Host header.
/// Unmatched hosts fall through to the default router (if set).
pub async fn serve_async_hosts(
    listener: tokio::net::TcpListener,
    host_router: super::host_router::HostRouter,
) -> Result<(), RuntimeError> {
    let buffers = host_router.buffer_config();
    let dispatch = ServerDispatch::Host(host_router.freeze());
    serve_async_dispatch(listener, dispatch, None, buffers).await
}

/// Serve HTTPS with host-based routing on a Tokio TCP listener.
pub async fn serve_async_hosts_tls(
    listener: tokio::net::TcpListener,
    host_router: super::host_router::HostRouter,
    tls_config: Arc<rustls::ServerConfig>,
) -> Result<(), RuntimeError> {
    let buffers = host_router.buffer_config();
    let dispatch = ServerDispatch::Host(host_router.freeze());
    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
    serve_async_dispatch(listener, dispatch, Some(acceptor), buffers).await
}

/// Spawn an HTTP server as a background async task.
///
/// Returns a [`ServerHandle`] for lifecycle control — cancel to stop the server.
/// Participates in Camber's structured concurrency. Awaiting the handle returns
/// `Result<(), RuntimeError>` with flattened error semantics.
pub fn serve_background(listener: tokio::net::TcpListener, router: Router) -> ServerHandle {
    let buffers = router.buffer_config();
    let dispatch = ServerDispatch::Single(router.freeze());
    serve_background_dispatch(listener, dispatch, None, buffers)
}

/// Spawn an HTTPS server as a background async task.
pub fn serve_background_tls(
    listener: tokio::net::TcpListener,
    router: Router,
    tls_config: Arc<rustls::ServerConfig>,
) -> ServerHandle {
    let buffers = router.buffer_config();
    let dispatch = ServerDispatch::Single(router.freeze());
    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
    serve_background_dispatch(listener, dispatch, Some(acceptor), buffers)
}

/// Spawn an HTTP server with host-based routing as a background async task.
pub fn serve_background_hosts(
    listener: tokio::net::TcpListener,
    host_router: super::host_router::HostRouter,
) -> ServerHandle {
    let buffers = host_router.buffer_config();
    let dispatch = ServerDispatch::Host(host_router.freeze());
    serve_background_dispatch(listener, dispatch, None, buffers)
}

/// Spawn an HTTPS server with host-based routing as a background async task.
pub fn serve_background_hosts_tls(
    listener: tokio::net::TcpListener,
    host_router: super::host_router::HostRouter,
    tls_config: Arc<rustls::ServerConfig>,
) -> ServerHandle {
    let buffers = host_router.buffer_config();
    let dispatch = ServerDispatch::Host(host_router.freeze());
    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
    serve_background_dispatch(listener, dispatch, Some(acceptor), buffers)
}

fn serve_background_dispatch(
    listener: tokio::net::TcpListener,
    dispatch: ServerDispatch,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    buffers: BufferConfig,
) -> ServerHandle {
    if tokio::runtime::Handle::try_current().is_err() {
        let (control, receiver) = tokio::sync::watch::channel(ServerControl::Running);
        drop(receiver);
        return ServerHandle {
            control: Some(control),
            join: Some(SupervisorJoin::Ready(std::future::ready(Err(
                RuntimeError::InvalidArgument(
                    "serve_background requires an active Tokio runtime".into(),
                ),
            )))),
        };
    }
    let snapshot = ServerContextSnapshot::capture(buffers, tls_acceptor.is_some());
    let (supervisor, control) = ServerSupervisor::new(listener, dispatch, tls_acceptor, snapshot);
    let join = match runtime::has_runtime() {
        true => SupervisorJoin::Camber(spawn_async(supervisor.run()).into_future()),
        false => SupervisorJoin::Tokio(tokio::spawn(supervisor.run())),
    };
    ServerHandle {
        control: Some(control),
        join: Some(join),
    }
}

async fn serve_async_dispatch(
    listener: tokio::net::TcpListener,
    dispatch: ServerDispatch,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    buffers: BufferConfig,
) -> Result<(), RuntimeError> {
    let is_tls = tls_acceptor.is_some();
    let snapshot = ServerContextSnapshot::capture(buffers, is_tls);
    let (supervisor, control) = ServerSupervisor::new(listener, dispatch, tls_acceptor, snapshot);
    drop(control);
    supervisor.run().await
}

/// Bind an HTTP server and route requests until shutdown.
pub fn serve(addr: &str, router: Router) -> Result<(), RuntimeError> {
    match runtime::has_runtime() {
        true => {
            let listener = net::listen(addr)?;
            serve_listener(listener, router)
        }
        false => runtime::run(|| {
            let listener = net::listen(addr)?;
            serve_listener(listener, router)
        })?,
    }
}

/// Serve HTTP on an existing listener. Blocks until shutdown.
pub fn serve_listener(listener: net::Listener, router: Router) -> Result<(), RuntimeError> {
    let buffers = router.buffer_config();
    let dispatch = ServerDispatch::Single(router.freeze());
    serve_dispatch(listener, dispatch, buffers)
}

/// Serve HTTP with host-based routing on an existing listener. Blocks until shutdown.
pub fn serve_hosts(
    listener: net::Listener,
    host_router: super::host_router::HostRouter,
) -> Result<(), RuntimeError> {
    let buffers = host_router.buffer_config();
    let dispatch = ServerDispatch::Host(host_router.freeze());
    serve_dispatch(listener, dispatch, buffers)
}

fn serve_dispatch(
    listener: net::Listener,
    dispatch: ServerDispatch,
    buffers: BufferConfig,
) -> Result<(), RuntimeError> {
    let router = Arc::new(dispatch);
    let rt = runtime::current_runtime();
    let shutdown = rt.shutdown_signal();
    let keepalive_timeout = rt.config.keepalive_timeout;
    let conn_limit = make_conn_limit(rt.config.connection_limit);
    let tls_acceptor = rt
        .config
        .tls_config
        .as_ref()
        .map(|cfg| tokio_rustls::TlsAcceptor::from(Arc::clone(cfg)));
    let is_tls = tls_acceptor.is_some();
    let ctx = Arc::new(ConnCtx::from_runtime(&rt, buffers, is_tls));
    drop(rt);

    // Run the hyper accept loop in block_in_place so the calling thread
    // can participate in async work without blocking the Tokio runtime.
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            accept_loop(
                &listener,
                router,
                ctx,
                shutdown,
                keepalive_timeout,
                tls_acceptor,
                conn_limit,
            )
            .await
        })
    })?;

    listener.cleanup()
}
