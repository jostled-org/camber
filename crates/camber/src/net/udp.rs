use crate::RuntimeError;
use crate::runtime;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// Async UDP socket wrapping `tokio::net::UdpSocket`.
#[derive(Debug)]
pub struct UdpSocket {
    inner: tokio::net::UdpSocket,
}

impl UdpSocket {
    /// Bind a UDP socket to the given address.
    pub async fn bind(addr: &str) -> Result<Self, RuntimeError> {
        let inner = tokio::net::UdpSocket::bind(addr).await?;
        Ok(Self { inner })
    }

    /// Connect to a remote address for use with `send`/`recv`.
    pub async fn connect(&self, addr: &str) -> Result<(), RuntimeError> {
        self.inner.connect(addr).await?;
        Ok(())
    }

    /// Send a datagram to the specified address.
    pub async fn send_to(&self, datagram: &[u8], target: &str) -> Result<usize, RuntimeError> {
        let bytes_sent = self.inner.send_to(datagram, target).await?;
        Ok(bytes_sent)
    }

    /// Receive a datagram, returning the number of bytes read and the sender address.
    pub async fn recv_from(
        &self,
        recv_buf: &mut [u8],
    ) -> Result<(usize, SocketAddr), RuntimeError> {
        let (bytes_read, addr) = self.inner.recv_from(recv_buf).await?;
        Ok((bytes_read, addr))
    }

    /// Send a datagram on a connected socket.
    pub async fn send(&self, datagram: &[u8]) -> Result<usize, RuntimeError> {
        let bytes_sent = self.inner.send(datagram).await?;
        Ok(bytes_sent)
    }

    /// Receive a datagram on a connected socket.
    pub async fn recv(&self, recv_buf: &mut [u8]) -> Result<usize, RuntimeError> {
        let bytes_read = self.inner.recv(recv_buf).await?;
        Ok(bytes_read)
    }

    /// Returns the local address this socket is bound to.
    pub fn local_addr(&self) -> Result<SocketAddr, RuntimeError> {
        let addr = self.inner.local_addr()?;
        Ok(addr)
    }
}

/// Bind a UDP socket to `addr` and run a recv loop dispatching datagrams to `handler`.
///
/// Runs until the runtime's shutdown signal fires, then returns `Ok(())`.
/// The handler runs inline — no per-datagram spawn. If concurrency is needed,
/// spawn inside the handler.
pub async fn serve_udp<F, Fut>(addr: &str, handler: F) -> Result<(), RuntimeError>
where
    F: Fn(Vec<u8>, SocketAddr, Arc<UdpSocket>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send,
{
    let socket = UdpSocket::bind(addr).await?;
    serve_udp_on(socket, handler).await
}

/// Run a recv loop on an existing UDP socket, dispatching datagrams to `handler`.
///
/// Runs until the runtime's shutdown signal fires, then returns `Ok(())`.
/// The handler runs inline — no per-datagram spawn.
pub async fn serve_udp_on<F, Fut>(socket: UdpSocket, handler: F) -> Result<(), RuntimeError>
where
    F: Fn(Vec<u8>, SocketAddr, Arc<UdpSocket>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send,
{
    let (shutdown, shutdown_notify) = runtime::shutdown_signal();
    let socket = Arc::new(socket);

    recv_loop(&socket, &shutdown, &shutdown_notify, &handler).await
}

async fn recv_loop<F, Fut>(
    socket: &Arc<UdpSocket>,
    shutdown: &std::sync::atomic::AtomicBool,
    shutdown_notify: &tokio::sync::Notify,
    handler: &F,
) -> Result<(), RuntimeError>
where
    F: Fn(Vec<u8>, SocketAddr, Arc<UdpSocket>) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), RuntimeError>> + Send,
{
    let mut buf = [0u8; 65535];
    loop {
        if shutdown.load(Ordering::Acquire) {
            return Ok(());
        }
        tokio::select! {
            result = socket.recv_from(&mut buf) => {
                let (n, addr) = result?;
                let datagram = buf[..n].to_vec();
                match handler(datagram, addr, Arc::clone(socket)).await {
                    Ok(()) => {}
                    Err(e) => tracing::warn!("udp handler error: {e}"),
                }
            }
            () = shutdown_notify.notified() => {
                return Ok(());
            }
        }
    }
}
