use super::BufferConfig;
use super::conn::{accept_loop, accept_tcp};
use super::handle::ConnCtx;
use super::router::{Router, ServerDispatch};
use crate::task::{AsyncJoinFuture, AsyncJoinHandle, spawn_async};
use crate::{RuntimeError, net, runtime};
use std::future::{Future, IntoFuture};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

/// Handle to a background server task with flattened error semantics.
///
/// Awaiting returns `Result<(), RuntimeError>` — the inner server error
/// and outer task error (panic, cancellation) are merged into a single Result.
pub struct ServerHandle {
    inner: AsyncJoinHandle<Result<(), RuntimeError>>,
}

impl ServerHandle {
    /// Request cancellation of the background server.
    pub fn cancel(&self) {
        self.inner.cancel();
    }
}

impl IntoFuture for ServerHandle {
    type Output = Result<(), RuntimeError>;
    type IntoFuture = ServerHandleFuture;

    fn into_future(self) -> Self::IntoFuture {
        ServerHandleFuture {
            inner: self.inner.into_future(),
        }
    }
}

/// Future for awaiting a [`ServerHandle`].
pub struct ServerHandleFuture {
    inner: AsyncJoinFuture<Result<(), RuntimeError>>,
}

impl Future for ServerHandleFuture {
    type Output = Result<(), RuntimeError>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match Pin::new(&mut self.inner).poll(cx) {
            Poll::Ready(Ok(inner_result)) => Poll::Ready(inner_result),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
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
    ServerHandle {
        inner: spawn_async(serve_async(listener, router)),
    }
}

/// Spawn an HTTPS server as a background async task.
pub fn serve_background_tls(
    listener: tokio::net::TcpListener,
    router: Router,
    tls_config: Arc<rustls::ServerConfig>,
) -> ServerHandle {
    ServerHandle {
        inner: spawn_async(serve_async_tls(listener, router, tls_config)),
    }
}

/// Spawn an HTTP server with host-based routing as a background async task.
pub fn serve_background_hosts(
    listener: tokio::net::TcpListener,
    host_router: super::host_router::HostRouter,
) -> ServerHandle {
    ServerHandle {
        inner: spawn_async(serve_async_hosts(listener, host_router)),
    }
}

/// Spawn an HTTPS server with host-based routing as a background async task.
pub fn serve_background_hosts_tls(
    listener: tokio::net::TcpListener,
    host_router: super::host_router::HostRouter,
    tls_config: Arc<rustls::ServerConfig>,
) -> ServerHandle {
    ServerHandle {
        inner: spawn_async(serve_async_hosts_tls(listener, host_router, tls_config)),
    }
}

async fn serve_async_dispatch(
    listener: tokio::net::TcpListener,
    dispatch: ServerDispatch,
    tls_acceptor: Option<tokio_rustls::TlsAcceptor>,
    buffers: BufferConfig,
) -> Result<(), RuntimeError> {
    let is_tls = tls_acceptor.is_some();
    let dispatch = Arc::new(dispatch);
    let (ctx, shutdown, keepalive, conn_limit) = match runtime::has_runtime() {
        true => {
            let rt = runtime::current_runtime();
            let shutdown = Arc::clone(&rt.shutdown_notify);
            let keepalive = rt.config.keepalive_timeout;
            let limit = make_conn_limit(rt.config.connection_limit);
            let ctx = Arc::new(ConnCtx::from_runtime(&rt, buffers, is_tls));
            drop(rt);
            (ctx, shutdown, keepalive, limit)
        }
        false => {
            let ctx = Arc::new(ConnCtx::without_runtime(buffers, is_tls));
            let shutdown = Arc::new(tokio::sync::Notify::new());
            let keepalive = std::time::Duration::from_secs(60);
            (ctx, shutdown, keepalive, None)
        }
    };
    accept_tcp(
        &listener,
        dispatch,
        ctx,
        shutdown,
        keepalive,
        tls_acceptor,
        conn_limit,
    )
    .await
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
    let shutdown_notify = Arc::clone(&rt.shutdown_notify);
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
                shutdown_notify,
                keepalive_timeout,
                tls_acceptor,
                conn_limit,
            )
            .await
        })
    })?;

    listener.cleanup()
}
