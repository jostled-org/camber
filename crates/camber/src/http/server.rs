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
///
/// The owner is one stage of the join future's own lifetime, so it holds one
/// rather than restating its fields: control, `Drop`, and the flattened join
/// have a single definition, and `join` is the move that hands them over.
pub struct ServerHandle(ServerHandleFuture);

impl ServerHandle {
    /// Request graceful shutdown without consuming the owner.
    ///
    /// Admission stops, active HTTP work receives graceful protocol shutdown,
    /// and the aggregate grace deadline starts once. Call [`join`](Self::join)
    /// or await the handle to observe completion.
    pub fn shutdown(&self) {
        self.0.shutdown();
    }

    /// Transfer lifecycle authority into the concrete join future.
    ///
    /// This does not initiate shutdown. The returned future retains
    /// [`shutdown`](ServerHandleFuture::shutdown) and
    /// [`cancel`](ServerHandleFuture::cancel) authority.
    pub fn join(self) -> ServerHandleFuture {
        self.0
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
        self.0.cancel();
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
    join: SupervisorJoin,
}

impl ServerHandleFuture {
    /// The join authority is held by value: it exists for as long as this
    /// future does, so there is no absent case to invent a result for.
    fn new(
        control: Option<tokio::sync::watch::Sender<ServerControl>>,
        join: SupervisorJoin,
    ) -> Self {
        Self { control, join }
    }

    pub(super) fn from_join(join: SupervisorJoin) -> Self {
        Self::new(None, join)
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
        let result = poll_supervisor_join(&mut self.join, cx);
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
///
/// Every listener-wide limit — synchronous and owned alike — is turned into a
/// semaphore here, so the two entry-point families cannot disagree about what
/// an absent limit means.
pub(super) fn make_conn_limit(limit: Option<usize>) -> Option<Arc<tokio::sync::Semaphore>> {
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
        // The absent executor is the whole failure, and it has a variant: an
        // owner is still handed back so the caller's shutdown and join calls
        // behave, and awaiting it reports the missing runtime. There is no
        // supervisor to carry a request to, so the handle holds no control
        // authority — the absent case its `shutdown`, `cancel`, and `Drop`
        // already answer.
        let refusal = SupervisorJoin::Ready(std::future::ready(Err(RuntimeError::NoRuntime)));
        return ServerHandle(ServerHandleFuture::from_join(refusal));
    }
    let snapshot = ServerContextSnapshot::capture(buffers, tls_acceptor.is_some());
    let is_camber = snapshot.is_camber();
    let (supervisor, control) = ServerSupervisor::new(listener, dispatch, tls_acceptor, snapshot);
    let join = match is_camber {
        true => SupervisorJoin::Camber(spawn_async(supervisor.run()).into_future()),
        false => SupervisorJoin::Tokio(tokio::spawn(supervisor.run())),
    };
    ServerHandle(ServerHandleFuture::new(Some(control), join))
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
///
/// # Errors
///
/// Returns `RuntimeError::Io` if `addr` cannot be parsed or bound, and any
/// error the accept loop propagates. When no runtime context is established
/// this establishes one rather than refusing, so runtime startup and teardown
/// failures from `runtime::run` propagate here too — that is the difference
/// between this entry point and [`serve_listener`].
pub fn serve(addr: &str, router: Router) -> Result<(), RuntimeError> {
    let bind_and_serve = move || {
        let listener = net::listen(addr)?;
        serve_listener(listener, router)
    };
    match runtime::has_runtime() {
        true => bind_and_serve(),
        false => runtime::run(bind_and_serve)?,
    }
}

/// Serve HTTP on an existing listener. Blocks until shutdown.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` if no runtime context is established.
/// This entry point serves on the current runtime rather than creating one, so
/// absence is refused before the listener is ever polled — use [`serve`] for
/// the bind-and-run form, which establishes a runtime when there is none.
/// Returns `RuntimeError::Http` when the caller is already inside a
/// current-thread runtime, which has no worker core to hand off for the
/// blocking accept loop. Listener and connection failures propagate as
/// `RuntimeError::Io`.
pub fn serve_listener(listener: net::Listener, router: Router) -> Result<(), RuntimeError> {
    let buffers = router.buffer_config();
    let dispatch = ServerDispatch::Single(router.freeze());
    serve_dispatch(listener, dispatch, buffers)
}

/// Serve HTTP with host-based routing on an existing listener. Blocks until shutdown.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` if no runtime context is established.
/// This entry point serves on the current runtime rather than creating one, so
/// absence is refused before the listener is ever polled — use [`serve`] for
/// the bind-and-run form, which establishes a runtime when there is none.
/// Returns `RuntimeError::Http` when the caller is already inside a
/// current-thread runtime, which has no worker core to hand off for the
/// blocking accept loop. Listener and connection failures propagate as
/// `RuntimeError::Io`.
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
    let rt = runtime::runtime_context()?;
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
    // The executor this server belongs to, named rather than resolved from
    // ambient context: `Handle::current()` panics with none established and
    // would silently run this loop on a foreign runtime when a different one is
    // ambient. A runtime with no executor cannot serve, and that absence is the
    // same typed refusal a missing context gets.
    let executor = rt.tokio_handle.clone().ok_or(RuntimeError::NoRuntime)?;
    drop(rt);
    refuse_unless_multi_thread()?;

    // Run the hyper accept loop in block_in_place so the calling thread
    // can participate in async work without blocking the Tokio runtime.
    let served = tokio::task::block_in_place(|| {
        executor.block_on(accept_loop(
            &listener,
            router,
            ctx,
            shutdown,
            keepalive_timeout,
            tls_acceptor,
            conn_limit,
        ))
    });
    // Cleanup runs whether or not the accept loop failed — a listener that
    // ended badly is exactly the one whose socket file is still on disk.
    report_serve_outcome(served, listener.cleanup())
}

/// Refuse a runtime flavor this accept loop cannot be driven from.
///
/// `Handle::block_on` aborts when the calling thread is already inside a
/// runtime, and `block_in_place` is what leaves it first — but only a
/// multi-thread worker has a core to hand to a replacement thread. A
/// current-thread caller has no way out of its own poll, so it is told that in
/// the error its signature already promises rather than aborted. No ambient
/// handle at all is not a refusal: the thread has not entered a runtime, so
/// tokio runs the closure inline and the bridge is legal.
fn refuse_unless_multi_thread() -> Result<(), RuntimeError> {
    match tokio::runtime::Handle::try_current().map(|handle| handle.runtime_flavor()) {
        Ok(tokio::runtime::RuntimeFlavor::MultiThread) | Err(_) => Ok(()),
        Ok(_) => Err(RuntimeError::Http(
            "serving on an existing listener requires a multi-thread runtime".into(),
        )),
    }
}

/// Report both halves of a served listener's outcome.
///
/// Only one error can be returned, and the accept loop's is the one that says
/// why serving ended; a cleanup failure behind it is logged rather than
/// destroyed.
fn report_serve_outcome(
    served: Result<(), RuntimeError>,
    cleaned: Result<(), RuntimeError>,
) -> Result<(), RuntimeError> {
    match (served, cleaned) {
        (Ok(()), cleaned) => cleaned,
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(cleanup_error)) => {
            tracing::warn!(%cleanup_error, "listener cleanup failed after a serve error");
            Err(error)
        }
    }
}
