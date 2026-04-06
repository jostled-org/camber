use crate::RuntimeError;
use crate::net::{Listener, ListenerInner};
use crate::runtime;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;

fn require_tcp<'a>(
    listener: &'a Listener,
    fn_name: &'static str,
) -> Result<&'a tokio::net::TcpListener, RuntimeError> {
    match &listener.inner {
        ListenerInner::Tcp(tcp) => Ok(tcp),
        ListenerInner::Unix(_, _) => Err(RuntimeError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("{fn_name} requires a TCP listener, not Unix"),
        ))),
    }
}

/// Async TCP stream wrapping `tokio::net::TcpStream`.
pub struct TcpStream {
    inner: tokio::net::TcpStream,
}

impl tokio::io::AsyncRead for TcpStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl tokio::io::AsyncWrite for TcpStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        std::pin::Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::pin::Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

impl std::fmt::Debug for TcpStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpStream")
            .field("local_addr", &self.inner.local_addr().ok())
            .field("peer_addr", &self.inner.peer_addr().ok())
            .finish()
    }
}

impl TcpStream {
    pub(crate) fn from_tokio(inner: tokio::net::TcpStream) -> Self {
        Self { inner }
    }

    /// Connect to a remote TCP address.
    pub async fn connect(addr: &str) -> Result<Self, RuntimeError> {
        let inner = tokio::net::TcpStream::connect(addr).await?;
        Ok(Self { inner })
    }

    /// Read data into the buffer, returning the number of bytes read.
    pub async fn read(&mut self, dest: &mut [u8]) -> Result<usize, RuntimeError> {
        use tokio::io::AsyncReadExt;
        let bytes_read = self.inner.read(dest).await?;
        Ok(bytes_read)
    }

    /// Write all bytes from the buffer.
    pub async fn write_all(&mut self, buf: &[u8]) -> Result<(), RuntimeError> {
        use tokio::io::AsyncWriteExt;
        self.inner.write_all(buf).await?;
        Ok(())
    }

    /// Shut down the write half of the stream.
    pub async fn shutdown(&mut self) -> Result<(), RuntimeError> {
        use tokio::io::AsyncWriteExt;
        self.inner.shutdown().await?;
        Ok(())
    }

    /// Returns the remote address this stream is connected to.
    pub fn peer_addr(&self) -> Result<SocketAddr, RuntimeError> {
        let addr = self.inner.peer_addr()?;
        Ok(addr)
    }

    /// Returns the local address this stream is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, RuntimeError> {
        let addr = self.inner.local_addr()?;
        Ok(addr)
    }
}

/// Accept TCP connections on `addr` and dispatch each to `handler`.
///
/// Runs until the runtime's shutdown signal fires, then returns `Ok(())`.
pub async fn serve_tcp<F, Fut>(addr: &str, handler: F) -> Result<(), RuntimeError>
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send + 'static,
{
    let listener = crate::net::listen(addr)?;
    serve_tcp_listener(listener, handler).await
}

/// Accept TCP connections on an existing listener and dispatch each to `handler`.
///
/// Runs until the runtime's shutdown signal fires, then returns `Ok(())`.
pub async fn serve_tcp_listener<F, Fut>(listener: Listener, handler: F) -> Result<(), RuntimeError>
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send + 'static,
{
    let tcp = require_tcp(&listener, "serve_tcp")?;
    let shutdown_notify = runtime::shutdown_notify();
    let handler = Arc::new(handler);

    super::accept::accept_loop(tcp, &shutdown_notify, None, |(stream, _addr)| {
        let h = Arc::clone(&handler);
        async move { handle_connection(stream, h).await }
    })
    .await
}

async fn handle_connection<F, Fut>(stream: tokio::net::TcpStream, handler: Arc<F>)
where
    F: Fn(TcpStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send,
{
    let tcp_stream = TcpStream::from_tokio(stream);
    match handler(tcp_stream).await {
        Ok(()) => {}
        Err(e) if crate::error::is_benign_io_error(&e) => {}
        Err(e) => tracing::warn!("tcp connection error: {e}"),
    }
}

/// Accept TCP+TLS connections on `addr` and dispatch each to `handler`.
///
/// Performs TLS handshake on each accepted connection before passing
/// the `TlsStream` to the handler. Runs until the runtime's shutdown signal.
pub async fn serve_tcp_tls<F, Fut>(
    addr: &str,
    tls_config: Arc<rustls::ServerConfig>,
    handler: F,
) -> Result<(), RuntimeError>
where
    F: Fn(super::TlsStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send + 'static,
{
    let listener = crate::net::listen(addr)?;
    serve_tcp_tls_listener(listener, tls_config, handler).await
}

/// Accept TCP+TLS connections on an existing listener and dispatch each to `handler`.
///
/// Runs until the runtime's shutdown signal fires, then returns `Ok(())`.
pub async fn serve_tcp_tls_listener<F, Fut>(
    listener: Listener,
    tls_config: Arc<rustls::ServerConfig>,
    handler: F,
) -> Result<(), RuntimeError>
where
    F: Fn(super::TlsStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send + 'static,
{
    let tcp = require_tcp(&listener, "serve_tcp_tls")?;
    let shutdown_notify = runtime::shutdown_notify();
    let acceptor = tokio_rustls::TlsAcceptor::from(tls_config);
    let handler = Arc::new(handler);

    super::accept::accept_loop(tcp, &shutdown_notify, None, |(stream, _addr)| {
        let a = acceptor.clone();
        let h = Arc::clone(&handler);
        async move { handle_tls_connection(stream, a, h).await }
    })
    .await
}

async fn handle_tls_connection<F, Fut>(
    stream: tokio::net::TcpStream,
    acceptor: tokio_rustls::TlsAcceptor,
    handler: Arc<F>,
) where
    F: Fn(super::TlsStream) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send,
{
    let tls_stream = match super::accept::tls_handshake(stream, &acceptor).await {
        Some(s) => super::TlsStream::from_server(s),
        None => return,
    };
    match handler(tls_stream).await {
        Ok(()) => {}
        Err(e) if crate::error::is_benign_io_error(&e) => {}
        Err(e) => tracing::warn!("tls connection error: {e}"),
    }
}
