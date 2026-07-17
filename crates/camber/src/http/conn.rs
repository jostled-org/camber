use super::handle::{ConnCtx, handle_request};
use super::router::ServerDispatch;
use super::server_lifecycle::{ConnectionLifecycle, ServerControl};
use crate::net::accept;
use crate::{RuntimeError, net};
use std::sync::Arc;

struct SyncConnectionState {
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    keepalive_timeout: std::time::Duration,
    remote_ip: std::net::IpAddr,
    lifecycle: ConnectionLifecycle,
}

pub(super) async fn accept_loop(
    listener: &net::Listener,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown_notify: Arc<tokio::sync::Notify>,
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
                shutdown_notify,
                keepalive_timeout,
                tls_acceptor,
                conn_limit,
            )
            .await
        }
        net::ListenerInner::Unix(unix, _) => {
            accept_unix(
                unix,
                router,
                ctx,
                shutdown_notify,
                keepalive_timeout,
                conn_limit,
            )
            .await
        }
    }
}

pub(super) async fn accept_tcp(
    listener: &tokio::net::TcpListener,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown_notify: Arc<tokio::sync::Notify>,
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
        &shutdown_notify,
        conn_limit.as_ref(),
        script.as_ref(),
        |(stream, addr), permit| {
            let router = Arc::clone(&router);
            let ctx = Arc::clone(&ctx);
            let shutdown = Arc::clone(&shutdown_notify);
            let acceptor = tls_acceptor.clone();
            let remote_ip = addr.ip();
            async move {
                match acceptor {
                    Some(a) => {
                        let state = SyncConnectionState {
                            router,
                            ctx,
                            shutdown_notify: shutdown,
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
    shutdown_notify: Arc<tokio::sync::Notify>,
    keepalive_timeout: std::time::Duration,
    conn_limit: Option<Arc<tokio::sync::Semaphore>>,
) -> Result<(), RuntimeError> {
    accept::accept_loop_with_permit(
        listener,
        &shutdown_notify,
        conn_limit.as_ref(),
        None,
        |stream, permit| {
            let router = Arc::clone(&router);
            let ctx = Arc::clone(&ctx);
            let shutdown = Arc::clone(&shutdown_notify);
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
    shutdown_notify: Arc<tokio::sync::Notify>,
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
        shutdown_notify,
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
        state.shutdown_notify,
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
    shutdown_notify: Arc<tokio::sync::Notify>,
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
        result = &mut conn => {
            match result {
                Ok(()) => {}
                Err(ref e) if is_benign_hyper_error(e.as_ref()) => {}
                Err(e) => tracing::warn!("connection error: {e}"),
            }
        }
        () = shutdown_notify.notified() => {
            // Signal HTTP/2 GOAWAY and let in-flight streams finish.
            conn.as_mut().graceful_shutdown();
            match tokio::time::timeout(std::time::Duration::from_secs(15), conn).await {
                Ok(Ok(())) => {}
                Ok(Err(ref e)) if is_benign_hyper_error(e.as_ref()) => {}
                Ok(Err(e)) => tracing::warn!("connection error during shutdown: {e}"),
                Err(_) => tracing::debug!("connection timed out during graceful shutdown"),
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
    let io = hyper_util::rt::TokioIo::new(stream);
    serve_owned_io(
        io,
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
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    lifecycle: ConnectionLifecycle,
    keepalive_timeout: std::time::Duration,
    remote_addr: Option<std::net::IpAddr>,
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
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
