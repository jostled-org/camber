use super::handle::{ConnCtx, handle_request};
use super::router::ServerDispatch;
use crate::net::accept;
use crate::{RuntimeError, net};
use std::sync::Arc;

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
    accept::accept_loop(
        listener,
        &shutdown_notify,
        conn_limit.as_ref(),
        |(stream, addr)| {
            let router = Arc::clone(&router);
            let ctx = Arc::clone(&ctx);
            let shutdown = Arc::clone(&shutdown_notify);
            let acceptor = tls_acceptor.clone();
            let remote_ip = addr.ip();
            async move {
                match acceptor {
                    Some(a) => {
                        serve_tls_connection(
                            stream,
                            a,
                            router,
                            ctx,
                            shutdown,
                            keepalive_timeout,
                            remote_ip,
                        )
                        .await;
                    }
                    None => {
                        serve_stream(
                            stream,
                            router,
                            ctx,
                            shutdown,
                            keepalive_timeout,
                            Some(remote_ip),
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
    accept::accept_loop(listener, &shutdown_notify, conn_limit.as_ref(), |stream| {
        let router = Arc::clone(&router);
        let ctx = Arc::clone(&ctx);
        let shutdown = Arc::clone(&shutdown_notify);
        async move {
            serve_stream(stream, router, ctx, shutdown, keepalive_timeout, None).await;
        }
    })
    .await
}

async fn serve_stream<S>(
    stream: S,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    keepalive_timeout: std::time::Duration,
    remote_addr: Option<std::net::IpAddr>,
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
    )
    .await;
}

async fn serve_tls_connection(
    stream: tokio::net::TcpStream,
    acceptor: tokio_rustls::TlsAcceptor,
    router: Arc<ServerDispatch>,
    ctx: Arc<ConnCtx>,
    shutdown_notify: Arc<tokio::sync::Notify>,
    keepalive_timeout: std::time::Duration,
    remote_ip: std::net::IpAddr,
) {
    let tls_stream = match accept::tls_handshake(stream, &acceptor).await {
        Some(s) => s,
        None => return,
    };
    serve_stream(
        tls_stream,
        router,
        ctx,
        shutdown_notify,
        keepalive_timeout,
        Some(remote_ip),
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
) where
    I: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let router = Arc::clone(&router);
        let ctx = Arc::clone(&ctx);
        async move { handle_request(req, &router, &ctx, remote_addr).await }
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
