use super::BufferConfig;
use super::ServerPolicy;
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

/// The largest connection limit the admission semaphore below can hold.
///
/// The ceiling belongs to the owner that enforces it, so the policy setter
/// that validates a limit and the semaphore that carries it read one value.
pub(super) const MAX_CONNECTION_LIMIT: usize = tokio::sync::Semaphore::MAX_PERMITS;

/// Build a semaphore from an optional connection limit.
///
/// Every listener-wide limit — synchronous and owned alike — is turned into a
/// semaphore here, so the two entry-point families cannot disagree about what
/// an absent limit means.
pub(super) fn make_conn_limit(limit: Option<usize>) -> Option<Arc<tokio::sync::Semaphore>> {
    limit.map(|n| Arc::new(tokio::sync::Semaphore::new(n)))
}

/// The routes one server serves, before they are frozen.
///
/// A builder holds its router by value until a terminal `serve*` call, so the
/// two router kinds travel through one type and freeze at the same instant.
enum ServerRoutes {
    Single(Router),
    Host(super::host_router::HostRouter),
}

impl ServerRoutes {
    fn buffer_config(&self) -> BufferConfig {
        match self {
            Self::Single(router) => router.buffer_config(),
            Self::Host(host_router) => host_router.buffer_config(),
        }
    }

    fn freeze(self) -> ServerDispatch {
        match self {
            Self::Single(router) => ServerDispatch::Single(router.freeze()),
            Self::Host(host_router) => ServerDispatch::Host(host_router.freeze()),
        }
    }
}

/// The canonical owned server: routes, policy, and transport, frozen at one
/// terminal call.
///
/// Every serving entry point in Camber — blocking, listener-bound, async, and
/// background; single-router and host-router; plain and TLS — assembles this
/// builder and hands it to one supervisor. There is no second accept loop and
/// no second configuration path.
///
/// Each terminal captures the current Camber runtime, or its absence, before it
/// returns. A returned [`ServerHandleFuture`] moved to another executor
/// therefore keeps the authority its constructor saw, and ambient context that
/// changes afterwards cannot reach the server.
///
/// ```rust,no_run
/// use camber::RuntimeError;
/// use camber::http::{self, Response, Router, ServerPolicy};
/// use std::time::Duration;
///
/// fn main() -> Result<(), RuntimeError> {
///     let mut router = Router::new();
///     router.get("/hello", |_req| async { Response::text(200, "hi") });
///
///     http::server(router)
///         .policy(
///             ServerPolicy::default()
///                 .header_timeout(Duration::from_secs(10))?
///                 .connection_limit(1024)?,
///         )
///         .serve("0.0.0.0:8080")
/// }
/// ```
pub struct ServerBuilder {
    routes: ServerRoutes,
    policy: ServerPolicy,
    tls: Option<Arc<rustls::ServerConfig>>,
}

/// Start building an owned server for one router.
#[must_use]
pub fn server(router: Router) -> ServerBuilder {
    ServerBuilder::new(ServerRoutes::Single(router))
}

/// Start building an owned server that dispatches by Host header.
#[must_use]
pub fn server_hosts(host_router: super::host_router::HostRouter) -> ServerBuilder {
    ServerBuilder::new(ServerRoutes::Host(host_router))
}

impl ServerBuilder {
    fn new(routes: ServerRoutes) -> Self {
        Self {
            routes,
            policy: ServerPolicy::default(),
            tls: None,
        }
    }

    /// Set the bounds this server serves under.
    ///
    /// Inside a Camber runtime the runtime's own policy contains this one: each
    /// dimension resolves to the narrower of the two, so a server can tighten
    /// its runtime's envelope and can never escape it. Under bare Tokio this
    /// policy is the sole authority.
    #[must_use]
    pub fn policy(mut self, policy: ServerPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Serve TLS with this configuration.
    ///
    /// A server started inside a runtime that configured TLS inherits that
    /// configuration; this overrides it for this server alone.
    #[must_use]
    pub fn tls(mut self, config: Arc<rustls::ServerConfig>) -> Self {
        self.tls = Some(config);
        self
    }

    /// Bind an address and serve until shutdown.
    ///
    /// # Errors
    ///
    /// Returns `RuntimeError::Io` if `addr` cannot be parsed or bound, and any
    /// error the supervisor propagates. When no runtime context is established
    /// this establishes one rather than refusing, so runtime startup and
    /// teardown failures from `runtime::run` propagate here too — that is the
    /// difference between this terminal and
    /// [`serve_listener`](Self::serve_listener).
    pub fn serve(self, addr: &str) -> Result<(), RuntimeError> {
        let bind_and_serve = move || {
            let listener = net::listen(addr)?;
            self.serve_listener(listener)
        };
        match runtime::has_runtime() {
            true => bind_and_serve(),
            false => runtime::run(bind_and_serve)?,
        }
    }

    /// Serve an already bound listener until shutdown. Blocks.
    ///
    /// # Errors
    ///
    /// Returns `RuntimeError::NoRuntime` if no runtime context is established.
    /// This terminal serves on the current runtime rather than creating one, so
    /// absence is refused before the listener is ever polled — use
    /// [`serve`](Self::serve) for the bind-and-run form. Returns
    /// `RuntimeError::Http` when the caller is already inside a current-thread
    /// runtime, which has no worker core to hand off for the blocking wait.
    /// Listener and connection failures propagate as `RuntimeError::Io`.
    pub fn serve_listener(self, listener: net::Listener) -> Result<(), RuntimeError> {
        let (supervisor, control) = self.freeze(listener);
        // The executor this server belongs to, named rather than resolved from
        // ambient context: `Handle::current()` panics with none established and
        // would silently run this supervisor on a foreign runtime when a
        // different one is ambient. A runtime with no executor cannot serve,
        // and that absence is the same typed refusal a missing context gets.
        let executor = runtime::runtime_context()?
            .tokio_handle
            .clone()
            .ok_or(RuntimeError::NoRuntime)?;
        refuse_unless_multi_thread()?;
        drop(control);
        // `block_in_place` so the calling thread hands its worker core back
        // while it waits on the supervisor it owns.
        tokio::task::block_in_place(|| executor.block_on(supervisor.run()))
    }

    /// Serve on a Tokio listener, returning the owned join future.
    ///
    /// The returned future is a concrete owner, not an `async fn`: routing is
    /// frozen and the serving context captured before it exists, so moving it
    /// to another executor cannot change either.
    ///
    /// # Errors
    ///
    /// Returns `RuntimeError::NoRuntime` when no Tokio executor is established
    /// to spawn connection work on.
    pub fn serve_async(
        self,
        listener: tokio::net::TcpListener,
    ) -> Result<ServerHandleFuture, RuntimeError> {
        refuse_without_tokio()?;
        let (supervisor, control) = self.freeze(net::Listener::from_tcp(listener));
        drop(control);
        Ok(ServerHandleFuture::from_join(SupervisorJoin::Owned(
            Box::pin(supervisor.run()),
        )))
    }

    /// Spawn the server as a background task and return its lifecycle owner.
    ///
    /// # Errors
    ///
    /// Returns `RuntimeError::NoRuntime` when no Tokio executor is established.
    /// The refusal is synchronous: there is no owner to hand back for a server
    /// that was never started.
    pub fn serve_background(
        self,
        listener: tokio::net::TcpListener,
    ) -> Result<ServerHandle, RuntimeError> {
        refuse_without_tokio()?;
        let (supervisor, control) = self.freeze(net::Listener::from_tcp(listener));
        let join = match supervisor.is_camber() {
            true => SupervisorJoin::Camber(spawn_async(supervisor.run()).into_future()),
            false => SupervisorJoin::Tokio(tokio::spawn(supervisor.run())),
        };
        Ok(ServerHandle(ServerHandleFuture::new(Some(control), join)))
    }

    /// Freeze routing and capture this server's context at one instant.
    ///
    /// TLS is resolved from the captured runtime rather than a second lookup,
    /// so the runtime that supplies the policy is the runtime that supplies the
    /// certificate.
    fn freeze(
        self,
        listener: net::Listener,
    ) -> (ServerSupervisor, tokio::sync::watch::Sender<ServerControl>) {
        let buffers = self.routes.buffer_config();
        let snapshot = ServerContextSnapshot::capture(buffers, self.policy);
        let tls_acceptor = self
            .tls
            .or_else(|| snapshot.runtime_tls())
            .map(tokio_rustls::TlsAcceptor::from);
        let snapshot = snapshot.with_tls(tls_acceptor.is_some());
        ServerSupervisor::new(listener, self.routes.freeze(), tls_acceptor, snapshot)
    }
}

/// Refuse an owned server with no executor to run on.
///
/// Both owned terminals ask before they freeze anything, so a caller with no
/// Tokio runtime is told immediately rather than being handed an owner whose
/// only possible answer is the same refusal.
fn refuse_without_tokio() -> Result<(), RuntimeError> {
    match tokio::runtime::Handle::try_current() {
        Ok(_) => Ok(()),
        Err(_) => Err(RuntimeError::NoRuntime),
    }
}

/// Serve HTTP on a Tokio TCP listener without Camber's runtime.
///
/// Shorthand for [`server(router).serve_async(listener)`](ServerBuilder::serve_async).
/// The returned owner is built before this call returns, so the runtime context
/// it serves under is the one current here.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` when no Tokio executor is established.
pub fn serve_async(
    listener: tokio::net::TcpListener,
    router: Router,
) -> Result<ServerHandleFuture, RuntimeError> {
    server(router).serve_async(listener)
}

/// Serve HTTPS on a Tokio TCP listener without Camber's runtime.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` when no Tokio executor is established.
pub fn serve_async_tls(
    listener: tokio::net::TcpListener,
    router: Router,
    tls_config: Arc<rustls::ServerConfig>,
) -> Result<ServerHandleFuture, RuntimeError> {
    server(router).tls(tls_config).serve_async(listener)
}

/// Serve HTTP with host-based routing on a Tokio TCP listener.
///
/// Unmatched hosts fall through to the default router, if one is set.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` when no Tokio executor is established.
pub fn serve_async_hosts(
    listener: tokio::net::TcpListener,
    host_router: super::host_router::HostRouter,
) -> Result<ServerHandleFuture, RuntimeError> {
    server_hosts(host_router).serve_async(listener)
}

/// Serve HTTPS with host-based routing on a Tokio TCP listener.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` when no Tokio executor is established.
pub fn serve_async_hosts_tls(
    listener: tokio::net::TcpListener,
    host_router: super::host_router::HostRouter,
    tls_config: Arc<rustls::ServerConfig>,
) -> Result<ServerHandleFuture, RuntimeError> {
    server_hosts(host_router)
        .tls(tls_config)
        .serve_async(listener)
}

/// Spawn an HTTP server as a background async task.
///
/// Returns a [`ServerHandle`] for lifecycle control. Participates in Camber's
/// structured concurrency, and awaiting the handle returns one flat
/// `Result<(), RuntimeError>`.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` when no Tokio executor is established.
pub fn serve_background(
    listener: tokio::net::TcpListener,
    router: Router,
) -> Result<ServerHandle, RuntimeError> {
    server(router).serve_background(listener)
}

/// Spawn an HTTPS server as a background async task.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` when no Tokio executor is established.
pub fn serve_background_tls(
    listener: tokio::net::TcpListener,
    router: Router,
    tls_config: Arc<rustls::ServerConfig>,
) -> Result<ServerHandle, RuntimeError> {
    server(router).tls(tls_config).serve_background(listener)
}

/// Spawn an HTTP server with host-based routing as a background async task.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` when no Tokio executor is established.
pub fn serve_background_hosts(
    listener: tokio::net::TcpListener,
    host_router: super::host_router::HostRouter,
) -> Result<ServerHandle, RuntimeError> {
    server_hosts(host_router).serve_background(listener)
}

/// Spawn an HTTPS server with host-based routing as a background async task.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` when no Tokio executor is established.
pub fn serve_background_hosts_tls(
    listener: tokio::net::TcpListener,
    host_router: super::host_router::HostRouter,
    tls_config: Arc<rustls::ServerConfig>,
) -> Result<ServerHandle, RuntimeError> {
    server_hosts(host_router)
        .tls(tls_config)
        .serve_background(listener)
}

/// Bind an HTTP server and route requests until shutdown.
///
/// Shorthand for [`server(router).serve(addr)`](ServerBuilder::serve).
///
/// # Errors
///
/// Returns `RuntimeError::Io` if `addr` cannot be parsed or bound, and any
/// error the supervisor propagates. When no runtime context is established this
/// establishes one, so runtime startup and teardown failures propagate here
/// too — that is the difference between this entry point and
/// [`serve_listener`].
pub fn serve(addr: &str, router: Router) -> Result<(), RuntimeError> {
    server(router).serve(addr)
}

/// Serve HTTP on an existing listener. Blocks until shutdown.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` if no runtime context is established, and
/// `RuntimeError::Http` when the caller is already inside a current-thread
/// runtime. Listener and connection failures propagate as `RuntimeError::Io`.
pub fn serve_listener(listener: net::Listener, router: Router) -> Result<(), RuntimeError> {
    server(router).serve_listener(listener)
}

/// Serve HTTP with host-based routing on an existing listener. Blocks until shutdown.
///
/// # Errors
///
/// Returns `RuntimeError::NoRuntime` if no runtime context is established, and
/// `RuntimeError::Http` when the caller is already inside a current-thread
/// runtime. Listener and connection failures propagate as `RuntimeError::Io`.
pub fn serve_hosts(
    listener: net::Listener,
    host_router: super::host_router::HostRouter,
) -> Result<(), RuntimeError> {
    server_hosts(host_router).serve_listener(listener)
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
